//! Overlay widget and notification-area icon for Resonance.
//!
//! This crate is self-contained: it is handed a command sender and a snapshot
//! receiver and drives the whole interface from those two channels. It holds no
//! reference to the audio backend and knows nothing about how snapshots are
//! produced.
//!
//! # Shape of the interface
//!
//! The tray icon and the keyboard shortcut each own a small thread with its own
//! message pump, and they are the only things that exist at rest. The windowing
//! and rendering stack is *not* started until the user actually asks for the
//! overlay: creating a window brings up a real OpenGL context and the graphics
//! driver behind it, which costs tens of megabytes that a background utility
//! has no business holding while nothing is on screen.
//!
//! So [`run`] sleeps on a channel. Each time the user toggles the overlay it
//! starts the windowing stack, which blocks until the overlay is dismissed, and
//! then tears it right back down and goes to sleep again.
//!
//! [`run`] blocks until the user quits.

mod app;
mod hotkey;
mod icon;
mod overlay;
mod theme;
mod tray;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender};
use resonance_core::messages::{HotkeyConfig, Snapshot, UiCommand};

use app::{lock, ResonanceApp};
use hotkey::HotkeyControl;
use overlay::{overlay_viewport, OverlayState};

/// Something that asks the interface to change, raised from the tray icon or
/// the keyboard shortcut rather than from the interface itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiSignal {
    ToggleOverlay,
    Quit,
}

/// A way to nudge the overlay, when there is one.
///
/// The drawing context only exists while the overlay window does, so this holds
/// a context between the moment the overlay opens and the moment it closes, and
/// nothing the rest of the time.
pub(crate) type Waker = Arc<Mutex<Option<egui::Context>>>;

/// Asks the overlay to repaint. Does nothing when the overlay is closed, which
/// is exactly what should happen: there is no window to refresh, and the state
/// to display is picked up when one is next opened.
pub(crate) fn wake(waker: &Waker) {
    if let Some(ctx) = lock(waker).as_ref() {
        ctx.request_repaint();
    }
}

/// Shared flags the overlay sets on its way out, telling the loop below why it
/// closed.
#[derive(Clone, Default)]
pub(crate) struct ExitFlags {
    /// The user quit the application from inside the overlay.
    pub(crate) quit: Arc<AtomicBool>,
    /// The overlay needs to come straight back with a rebuilt window, because
    /// a setting changed that is fixed when the window is created.
    pub(crate) restart: Arc<AtomicBool>,
}

impl ExitFlags {
    fn take(flag: &AtomicBool) -> bool {
        flag.swap(false, Ordering::Relaxed)
    }
}

/// Runs the interface until the user quits.
///
/// Sets up the notification-area icon and the system-wide keyboard shortcut,
/// then waits. The overlay window and everything behind it are created the
/// first time the user toggles it and destroyed again each time it is
/// dismissed.
///
/// `ui_cmd_tx` carries everything the user does to the backend. `snapshot_rx`
/// delivers the state to display; only the newest snapshot is ever rendered,
/// and older ones are discarded without being drawn.
///
/// `initial_hotkey` is the shortcut to register at startup, which the backend
/// reads from the saved settings. The user can record a different one from the
/// settings panel; that change is registered here and sent back over
/// `ui_cmd_tx` for the backend to store.
pub fn run(
    ui_cmd_tx: Sender<UiCommand>,
    snapshot_rx: Receiver<Snapshot>,
    initial_hotkey: HotkeyConfig,
) -> eframe::Result {
    let (signal_tx, signal_rx) = crossbeam_channel::unbounded::<UiSignal>();
    let (bridge_shutdown_tx, bridge_shutdown_rx) = crossbeam_channel::bounded::<()>(0);

    let waker: Waker = Arc::new(Mutex::new(None));
    let latest_snapshot: Arc<Mutex<Option<Snapshot>>> = Arc::new(Mutex::new(None));
    let exit_flags = ExitFlags::default();

    spawn_snapshot_bridge(
        snapshot_rx,
        bridge_shutdown_rx,
        Arc::clone(&latest_snapshot),
        waker.clone(),
    );

    // Held for its side effect: the icon disappears when it is dropped, at the
    // end of this function.
    let _tray = match tray::spawn(signal_tx.clone(), waker.clone()) {
        Ok(tray) => Some(tray),
        Err(err) => {
            tracing::error!(error = %err, "could not create the tray icon");
            None
        }
    };

    // The shortcut is shared with the settings panel, which re-registers it
    // when the user records a new combination. It is released once both this
    // function and the panel have let go of it.
    let hotkey = {
        let signal_tx = signal_tx.clone();
        let waker = waker.clone();
        HotkeyControl::new(initial_hotkey, move || {
            if signal_tx.send(UiSignal::ToggleOverlay).is_ok() {
                wake(&waker);
            }
        })
    };

    if !hotkey.apply(initial_hotkey) {
        tracing::info!("the overlay shortcut is unavailable; use the tray icon instead");
    }

    let state = Arc::new(Mutex::new(OverlayState::new(
        ui_cmd_tx.clone(),
        Arc::clone(&hotkey),
    )));

    let result = main_loop(
        &signal_rx,
        &ui_cmd_tx,
        &state,
        &latest_snapshot,
        &waker,
        &exit_flags,
    );

    // Stops the bridge thread before the channels it holds go away.
    drop(bridge_shutdown_tx);

    result
}

/// Sleeps until something asks for the overlay, shows it, and sleeps again.
fn main_loop(
    signal_rx: &Receiver<UiSignal>,
    ui_cmd_tx: &Sender<UiCommand>,
    state: &Arc<Mutex<OverlayState>>,
    latest_snapshot: &Arc<Mutex<Option<Snapshot>>>,
    waker: &Waker,
    exit_flags: &ExitFlags,
) -> eframe::Result {
    loop {
        match signal_rx.recv() {
            // Every sender is gone, so nothing can ask for the overlay again.
            Err(_) => return Ok(()),
            Ok(UiSignal::Quit) => {
                if ui_cmd_tx.send(UiCommand::Quit).is_err() {
                    tracing::warn!("the backend had already stopped when quit was requested");
                }
                return Ok(());
            }
            Ok(UiSignal::ToggleOverlay) => {
                show_overlay_until_closed(
                    signal_rx,
                    ui_cmd_tx,
                    state,
                    latest_snapshot,
                    waker,
                    exit_flags,
                )?;

                if ExitFlags::take(&exit_flags.quit) {
                    return Ok(());
                }
            }
        }
    }
}

/// Shows the overlay, blocking until it is dismissed.
///
/// Settings that are fixed when a window is created — whether its surface has
/// per-pixel transparency, in particular — cannot be changed on a live window.
/// Changing one asks for a restart instead, and this loop immediately builds a
/// fresh window rather than making the user reopen the overlay by hand.
fn show_overlay_until_closed(
    signal_rx: &Receiver<UiSignal>,
    ui_cmd_tx: &Sender<UiCommand>,
    state: &Arc<Mutex<OverlayState>>,
    latest_snapshot: &Arc<Mutex<Option<Snapshot>>>,
    waker: &Waker,
    exit_flags: &ExitFlags,
) -> eframe::Result {
    loop {
        let options = eframe::NativeOptions {
            viewport: overlay_viewport(&lock(state)),
            // The overlay is drawn from flat, opaque shapes; multisampling
            // would only cost memory and bandwidth, and on a transparent
            // surface it can leave partially covered pixels along the edges.
            multisampling: 0,
            depth_buffer: 0,
            stencil_buffer: 0,
            renderer: eframe::Renderer::Glow,
            // Hand control back here when the overlay closes, instead of
            // ending the process, so the tray icon outlives the window.
            run_and_return: true,
            ..Default::default()
        };

        let app = ResonanceApp::new(
            Arc::clone(state),
            Arc::clone(latest_snapshot),
            signal_rx.clone(),
            ui_cmd_tx.clone(),
            waker.clone(),
            exit_flags.clone(),
        );

        eframe::run_native(
            "Resonance",
            options,
            Box::new(move |cc| Ok(Box::new(app.attach(cc)))),
        )?;

        // The context belonged to the window that just went away.
        *lock(waker) = None;

        if !ExitFlags::take(&exit_flags.restart) {
            return Ok(());
        }
    }
}

/// Moves snapshots off the channel and keeps only the newest one.
///
/// Snapshots are produced whenever the audio state changes, which can be far
/// faster than the interface draws — a slider drag alone produces a burst. The
/// channel is therefore drained to its newest entry and the older ones are
/// dropped undrawn.
///
/// Draining happens on its own thread rather than during a draw because for
/// most of the time there is no draw: the overlay is closed and nothing is
/// consuming the channel, which would otherwise grow without bound. A repaint
/// is requested only when an overlay is actually open, which is what keeps the
/// process from waking up for state nobody is looking at.
fn spawn_snapshot_bridge(
    snapshot_rx: Receiver<Snapshot>,
    shutdown_rx: Receiver<()>,
    latest_snapshot: Arc<Mutex<Option<Snapshot>>>,
    waker: Waker,
) {
    let spawned = std::thread::Builder::new()
        .name("resonance-ui-bridge".into())
        .spawn(move || {
            loop {
                let snapshot = crossbeam_channel::select! {
                    recv(snapshot_rx) -> msg => match msg {
                        Ok(snapshot) => snapshot,
                        Err(_) => break,
                    },
                    // The sending half lives in `run`; it is dropped on
                    // shutdown, and the resulting disconnect is the signal to
                    // stop.
                    recv(shutdown_rx) -> _ => break,
                };

                // Keep only the newest: anything still queued has already been
                // superseded, so it is dropped without ever being drawn.
                let mut newest = snapshot;
                while let Ok(later) = snapshot_rx.try_recv() {
                    newest = later;
                }

                *lock(&latest_snapshot) = Some(newest);
                wake(&waker);
            }
        });

    if spawned.is_err() {
        tracing::error!("could not start the snapshot bridge; the overlay will not update");
    }
}
