//! Notification-area icon, owned by a dedicated message-pump thread.
//!
//! The icon has to outlive the overlay window: it is how the user brings the
//! overlay back after closing it, so it cannot depend on the overlay's event
//! loop being alive. The Windows implementation behind it does not pump
//! messages on its own — it relies on the caller running a loop on the thread
//! that created the icon — so this module owns that thread and that loop.
//!
//! The thread sleeps inside `GetMessageW` and is woken only by an actual user
//! interaction, so it costs nothing while idle.

use std::thread::JoinHandle;

use crossbeam_channel::Sender;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_QUIT,
};

use crate::icon::tray_icon_rgba;
use crate::{wake, UiSignal, Waker};

const TOGGLE_ITEM_ID: &str = "resonance.toggle-overlay";
const QUIT_ITEM_ID: &str = "resonance.quit";

/// A running tray icon. Dropping it stops the thread, which removes the icon
/// from the notification area.
pub(crate) struct Tray {
    thread_id: u32,
    handle: Option<JoinHandle<()>>,
}

/// Creates the icon on its own thread and starts pumping messages for it.
///
/// The icon and its menu are built inside the thread because they belong to
/// the thread whose message queue serves them; they are never moved across a
/// thread boundary.
pub(crate) fn spawn(signal_tx: Sender<UiSignal>, waker: Waker) -> Result<Tray, String> {
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<u32, String>>(1);

    let handle = std::thread::Builder::new()
        .name("resonance-tray".into())
        .spawn(move || {
            // SAFETY: GetCurrentThreadId reads the calling thread's own id and
            // cannot fail or touch memory we own.
            let thread_id = unsafe { GetCurrentThreadId() };

            // Held for the lifetime of this thread; dropped when the pump ends,
            // which is what removes the icon.
            let _tray = match build_icon() {
                Ok(tray) => {
                    if ready_tx.send(Ok(thread_id)).is_err() {
                        return;
                    }
                    tray
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };

            install_handlers(signal_tx, waker);
            pump_messages();
        })
        .map_err(|e| e.to_string())?;

    match ready_rx.recv() {
        Ok(Ok(thread_id)) => Ok(Tray {
            thread_id,
            handle: Some(handle),
        }),
        Ok(Err(err)) => {
            let _ = handle.join();
            Err(err)
        }
        Err(_) => {
            let _ = handle.join();
            Err("the tray thread stopped before it was ready".to_owned())
        }
    }
}

fn build_icon() -> Result<tray_icon::TrayIcon, String> {
    let (rgba, width, height) = tray_icon_rgba();
    let icon = Icon::from_rgba(rgba, width, height).map_err(|e| e.to_string())?;

    let toggle = MenuItem::with_id(TOGGLE_ITEM_ID, "Toggle overlay", true, None);
    let quit = MenuItem::with_id(QUIT_ITEM_ID, "Quit", true, None);
    let separator = PredefinedMenuItem::separator();

    let menu = Menu::with_items(&[&toggle, &separator, &quit]).map_err(|e| e.to_string())?;

    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip("Resonance")
        .build()
        .map_err(|e| e.to_string())
}

/// Standard message pump. Runs until `WM_QUIT` arrives, which is what the
/// handle's drop posts.
fn pump_messages() {
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a live, correctly aligned MSG for the duration of
        // the call. A null window filter asks for every message belonging to
        // this thread, which is where the icon's messages are delivered.
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };

        // Zero means WM_QUIT was received; -1 means the queue broke. Either
        // way this thread is done.
        if result.0 <= 0 {
            break;
        }

        // SAFETY: `msg` was just filled in by a successful GetMessageW call and
        // stays valid and owned by this thread across both calls.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Installs the process-wide menu and click callbacks.
///
/// These fire on this thread, from inside the pump above, so they do as little
/// as possible: hand the signal over and wake the overlay if one is open.
fn install_handlers(signal_tx: Sender<UiSignal>, waker: Waker) {
    let menu_tx = signal_tx.clone();
    let menu_waker = waker.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let signal = match event.id.0.as_str() {
            TOGGLE_ITEM_ID => UiSignal::ToggleOverlay,
            QUIT_ITEM_ID => UiSignal::Quit,
            other => {
                tracing::trace!(id = other, "ignoring unknown tray menu item");
                return;
            }
        };

        send(&menu_tx, &menu_waker, signal);
    }));

    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        // Only a completed left click toggles; the right button belongs to the
        // context menu, and reacting on button-down would fire twice.
        if let TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } = event
        {
            send(&signal_tx, &waker, UiSignal::ToggleOverlay);
        }
    }));
}

fn send(signal_tx: &Sender<UiSignal>, waker: &Waker, signal: UiSignal) {
    if signal_tx.send(signal).is_err() {
        tracing::warn!("the interface is no longer listening for tray signals");
        return;
    }

    // Nudges the overlay if one is open. When it is closed there is nothing to
    // repaint: the signal is already queued and the thread waiting on that
    // queue will pick it up.
    wake(waker);
}

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: Posting WM_QUIT to the tray thread's queue. The id was
        // captured on that thread and the thread is still alive because we are
        // about to join it, so the id cannot have been recycled yet.
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };

        if posted.is_err() {
            tracing::warn!("could not signal the tray thread to stop");
        }

        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
