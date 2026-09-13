//! The overlay window.
//!
//! One of these exists only while the overlay is on screen. It is the root
//! window of the windowing stack, so closing it ends that stack's event loop
//! and hands control back to the waiting loop that started it.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};

use crossbeam_channel::{Receiver, Sender};
use egui::{Color32, ViewportCommand};
use resonance_core::messages::{Snapshot, UiCommand};

use crate::overlay::{self, OverlayState};
use crate::theme;
use crate::{ExitFlags, UiSignal, Waker};

/// Upper bound placed on the font atlas texture's width and height.
///
/// The overlay draws one font family at a handful of sizes, which a row-based
/// glyph packer fits comfortably within this many pixels. Left uncapped, the
/// atlas is sized against the GPU's actual texture limit, which on modern
/// hardware is far larger than this widget will ever need.
const MAX_FONT_ATLAS_SIDE: usize = 2048;

/// Name the embedded font is registered under.
const FONT_NAME: &str = "inter-regular";

/// Embeds the overlay's own font instead of using egui's bundled set.
///
/// The bundled set ships two Latin faces and two emoji faces so that a wide
/// range of apps have something reasonable out of the box; this overlay
/// draws a small, fixed set of English strings and its icons are painter
/// shapes, not glyphs (see `overlay.rs`), so none of that range is needed.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::empty();

    fonts.font_data.insert(
        FONT_NAME.to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/Inter-Regular.ttf"
        ))),
    );

    fonts
        .families
        .insert(egui::FontFamily::Proportional, vec![FONT_NAME.to_owned()]);
    fonts
        .families
        .insert(egui::FontFamily::Monospace, vec![FONT_NAME.to_owned()]);

    ctx.set_fonts(fonts);
}

pub(crate) struct ResonanceApp {
    state: Arc<Mutex<OverlayState>>,
    /// Newest snapshot handed over by the bridge thread, if one arrived since
    /// the last tick. Also carries whatever arrived while the overlay was
    /// closed, so a freshly opened overlay is up to date immediately.
    latest_snapshot: Arc<Mutex<Option<Snapshot>>>,
    /// Shared with the loop that waits while the overlay is closed. Whichever
    /// of the two is listening receives a signal; they are never both
    /// listening at once.
    signal_rx: Receiver<UiSignal>,
    ui_cmd_tx: Sender<UiCommand>,
    waker: Waker,
    exit_flags: ExitFlags,
}

impl ResonanceApp {
    pub(crate) fn new(
        state: Arc<Mutex<OverlayState>>,
        latest_snapshot: Arc<Mutex<Option<Snapshot>>>,
        signal_rx: Receiver<UiSignal>,
        ui_cmd_tx: Sender<UiCommand>,
        waker: Waker,
        exit_flags: ExitFlags,
    ) -> Self {
        Self {
            state,
            latest_snapshot,
            signal_rx,
            ui_cmd_tx,
            waker,
            exit_flags,
        }
    }

    /// Publishes this window's drawing context so the tray icon, the keyboard
    /// shortcut and the snapshot bridge can ask it to repaint.
    pub(crate) fn attach(self, cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        theme::apply_style(&cc.egui_ctx);
        *lock(&self.waker) = Some(cc.egui_ctx.clone());
        self
    }

    fn state(&self) -> MutexGuard<'_, OverlayState> {
        lock(&self.state)
    }

    fn close(ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    fn quit(&mut self, ctx: &egui::Context) {
        if self.ui_cmd_tx.send(UiCommand::Quit).is_err() {
            tracing::warn!("the backend had already stopped when quit was requested");
        }
        self.exit_flags.quit.store(true, Ordering::Relaxed);
        Self::close(ctx);
    }

    fn apply_pending_snapshot(&mut self) {
        let Some(snapshot) = lock(&self.latest_snapshot).take() else {
            return;
        };
        self.state().set_snapshot(snapshot);
    }

    fn handle_signals(&mut self, ctx: &egui::Context) {
        while let Ok(signal) = self.signal_rx.try_recv() {
            match signal {
                // The overlay is already open, so toggling means dismiss it.
                UiSignal::ToggleOverlay => Self::close(ctx),
                UiSignal::Quit => self.quit(ctx),
            }
        }
    }
}

impl eframe::App for ResonanceApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The windowing backend reports the GPU's real texture size limit
        // (commonly 16384px on a modern card) on the very first frame, and
        // the font atlas is sized against whatever limit is in effect. This
        // overlay only ever needs one small font at one size, so a limit that
        // large just lets the atlas grow far past what it will ever use. The
        // one-shot input value this overrides is re-applied by egui every
        // frame from the previous frame's state once the backend stops
        // supplying its own, so setting it here is enough to make it stick.
        ctx.input_mut(|i| i.max_texture_side = MAX_FONT_ATLAS_SIDE);

        self.apply_pending_snapshot();
        self.handle_signals(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        {
            let mut state = self.state();
            overlay::draw(ui, &mut state);
        }

        let (close_requested, restart_requested) = {
            let mut state = self.state();
            (state.take_close_request(), state.take_restart_request())
        };

        if restart_requested {
            // The window has to be rebuilt for the change to reach the surface
            // the system gave us, so ask for a new one on the way out.
            self.exit_flags.restart.store(true, Ordering::Relaxed);
        }

        if close_requested || restart_requested {
            Self::close(ui.ctx());
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // In opaque mode every pixel of the surface is painted, so the surface
        // is cleared to the same solid colour the panel is filled with, and the
        // two cannot disagree at the edges. Otherwise it is cleared to fully
        // transparent, which is what lets the desktop show through.
        if lock(&self.state).opaque_mode() {
            theme::SURFACE.to_normalized_gamma_f32()
        } else {
            Color32::TRANSPARENT.to_normalized_gamma_f32()
        }
    }
}

impl Drop for ResonanceApp {
    fn drop(&mut self) {
        // The context is about to become invalid; stop handing it out.
        *lock(&self.waker) = None;
    }
}

/// Takes the lock, recovering the value if a previous holder panicked.
///
/// The guarded values are plain presentation state with no invariant that a
/// panic could leave half-applied, so continuing with them is preferable to
/// bringing down the whole interface.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
