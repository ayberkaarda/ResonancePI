//! System-wide keyboard shortcut that toggles the overlay.
//!
//! `RegisterHotKey` delivers `WM_HOTKEY` to the message queue of the thread
//! that registered it, so the registration and the message pump have to live on
//! the same thread. That thread is dedicated to this task and does nothing
//! else; it is woken only when the shortcut is pressed, so it costs no CPU
//! while idle.
//!
//! Which combination is registered is a user setting, so this module also owns
//! the two translations that setting needs: from an `egui` key press to the
//! virtual-key code the shortcut is stored as, and from a stored shortcut back
//! to a line of text the settings panel can show.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use resonance_core::messages::HotkeyConfig;
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN, VIRTUAL_KEY, VK_0, VK_A, VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_F1,
    VK_HOME, VK_INSERT, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, PostThreadMessageW, MSG, WM_HOTKEY, WM_QUIT,
};

use crate::app::lock;

/// Identifies our registration within this thread's hotkey table. Any value is
/// fine as long as it is unique per thread; each listener owns a thread and
/// registers exactly one shortcut on it.
const HOTKEY_ID: i32 = 1;

// ------------------------------------------------------------ key naming

/// `egui` keys for the letters, in the order their virtual-key codes run.
const LETTER_KEYS: [egui::Key; 26] = {
    use egui::Key as K;
    [
        K::A,
        K::B,
        K::C,
        K::D,
        K::E,
        K::F,
        K::G,
        K::H,
        K::I,
        K::J,
        K::K,
        K::L,
        K::M,
        K::N,
        K::O,
        K::P,
        K::Q,
        K::R,
        K::S,
        K::T,
        K::U,
        K::V,
        K::W,
        K::X,
        K::Y,
        K::Z,
    ]
};

/// `egui` keys for the digits, in the order their virtual-key codes run.
const DIGIT_KEYS: [egui::Key; 10] = {
    use egui::Key as K;
    [
        K::Num0,
        K::Num1,
        K::Num2,
        K::Num3,
        K::Num4,
        K::Num5,
        K::Num6,
        K::Num7,
        K::Num8,
        K::Num9,
    ]
};

/// `egui` keys for the function keys, in the order their virtual-key codes
/// run. Windows numbers them up to F24, but no common keyboard carries more
/// than twelve.
const FUNCTION_KEYS: [egui::Key; 12] = {
    use egui::Key as K;
    [
        K::F1,
        K::F2,
        K::F3,
        K::F4,
        K::F5,
        K::F6,
        K::F7,
        K::F8,
        K::F9,
        K::F10,
        K::F11,
        K::F12,
    ]
};

/// Keys written as a word rather than as the character they produce.
///
/// One table serves both directions — recording a shortcut and writing one
/// out — so the two can never disagree about which code a name belongs to.
const NAMED_KEYS: [(egui::Key, VIRTUAL_KEY, &str); 14] = {
    use egui::Key as K;
    [
        (K::Space, VK_SPACE, "Space"),
        (K::Tab, VK_TAB, "Tab"),
        (K::Enter, VK_RETURN, "Enter"),
        (K::Backspace, VK_BACK, "Backspace"),
        (K::Insert, VK_INSERT, "Insert"),
        (K::Delete, VK_DELETE, "Delete"),
        (K::Home, VK_HOME, "Home"),
        (K::End, VK_END, "End"),
        (K::PageUp, VK_PRIOR, "Page Up"),
        (K::PageDown, VK_NEXT, "Page Down"),
        (K::ArrowLeft, VK_LEFT, "Left"),
        (K::ArrowRight, VK_RIGHT, "Right"),
        (K::ArrowUp, VK_UP, "Up"),
        (K::ArrowDown, VK_DOWN, "Down"),
    ]
};

/// The virtual-key code a key press should be stored as, or `None` for a key
/// this product does not accept in a shortcut.
///
/// Modifier keys never reach this function: `egui` has no key variants for
/// them, so holding one only changes the modifiers carried by the next real
/// key press, which is exactly the shape a shortcut is recorded in.
pub(crate) fn egui_key_to_vk(key: egui::Key) -> Option<u32> {
    if let Some(index) = LETTER_KEYS.iter().position(|candidate| *candidate == key) {
        return Some(u32::from(VK_A.0) + index as u32);
    }
    if let Some(index) = DIGIT_KEYS.iter().position(|candidate| *candidate == key) {
        return Some(u32::from(VK_0.0) + index as u32);
    }
    if let Some(index) = FUNCTION_KEYS.iter().position(|candidate| *candidate == key) {
        return Some(u32::from(VK_F1.0) + index as u32);
    }

    NAMED_KEYS
        .iter()
        .find(|(candidate, _, _)| *candidate == key)
        .map(|(_, vk, _)| u32::from(vk.0))
}

/// Names one virtual-key code.
///
/// A code outside the set the recorder accepts is shown as its number rather
/// than dropped, so a shortcut saved by a later version is still described
/// honestly instead of appearing blank.
fn key_name(vk: u32) -> String {
    let letters = u32::from(VK_A.0);
    if let Some(offset) = vk.checked_sub(letters).filter(|o| *o < 26) {
        return char::from(b'A' + offset as u8).to_string();
    }

    let digits = u32::from(VK_0.0);
    if let Some(offset) = vk.checked_sub(digits).filter(|o| *o < 10) {
        return char::from(b'0' + offset as u8).to_string();
    }

    let functions = u32::from(VK_F1.0);
    if let Some(offset) = vk
        .checked_sub(functions)
        .filter(|o| (*o as usize) < FUNCTION_KEYS.len())
    {
        return format!("F{}", offset + 1);
    }

    if let Some((_, _, name)) = NAMED_KEYS
        .iter()
        .find(|(_, code, _)| u32::from(code.0) == vk)
    {
        return (*name).to_owned();
    }

    format!("Key 0x{vk:02X}")
}

/// Writes a shortcut the way it is printed on a keyboard, e.g. `Ctrl+Alt+V`.
pub(crate) fn describe(config: HotkeyConfig) -> String {
    let mut text = String::new();

    for (held, name) in [
        (config.ctrl, "Ctrl"),
        (config.alt, "Alt"),
        (config.shift, "Shift"),
        (config.win, "Win"),
    ] {
        if held {
            text.push_str(name);
            text.push('+');
        }
    }

    text.push_str(&key_name(config.key));
    text
}

/// Whether the combination carries at least one modifier.
///
/// A shortcut without one takes that key away from every other application on
/// the desktop, so the recorder refuses to store it.
pub(crate) fn has_modifier(config: HotkeyConfig) -> bool {
    config.ctrl || config.alt || config.shift || config.win
}

/// The modifier flags `RegisterHotKey` expects.
///
/// `MOD_NOREPEAT` is always included: holding the combination down should
/// toggle the overlay once, not once per key repeat.
fn modifier_flags(config: HotkeyConfig) -> HOT_KEY_MODIFIERS {
    [
        (config.ctrl, MOD_CONTROL),
        (config.alt, MOD_ALT),
        (config.shift, MOD_SHIFT),
        (config.win, MOD_WIN),
    ]
    .into_iter()
    .filter(|(held, _)| *held)
    .fold(MOD_NOREPEAT, |flags, (_, flag)| flags | flag)
}

// -------------------------------------------------------------- listener

/// A running hotkey listener. Dropping the handle stops the thread and
/// releases the shortcut back to the system.
struct HotkeyListener {
    thread_id: u32,
    handle: Option<JoinHandle<()>>,
}

/// Registers `config` and calls `on_press` on the listener thread each time it
/// fires.
///
/// Returns `None` if the shortcut could not be registered, which usually means
/// another application already owns it. That is not fatal: the tray menu
/// remains a working way to open the overlay.
fn spawn<F>(config: HotkeyConfig, on_press: F) -> Option<HotkeyListener>
where
    F: Fn() + Send + 'static,
{
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Option<u32>>(1);
    let flags = modifier_flags(config);
    let key = config.key;

    let handle = std::thread::Builder::new()
        .name("resonance-hotkey".into())
        .spawn(move || {
            // SAFETY: GetCurrentThreadId reads the calling thread's own id from
            // the thread environment block and cannot fail or touch memory we
            // own.
            let thread_id = unsafe { GetCurrentThreadId() };

            // SAFETY: A null owner window registers the shortcut against this
            // thread's message queue, which is exactly where the loop below
            // pumps messages from. The modifier and virtual key values are
            // plain integers, so no pointer outlives this call.
            let registered = unsafe { RegisterHotKey(None, HOTKEY_ID, flags, key) };

            if let Err(err) = registered {
                tracing::warn!(error = %err, "could not register the overlay shortcut");
                let _ = ready_tx.send(None);
                return;
            }

            if ready_tx.send(Some(thread_id)).is_err() {
                // Nobody is waiting for us any more; undo the registration and
                // leave before entering the pump.
                // SAFETY: The registration above succeeded on this same thread,
                // so this id is valid to release here.
                let _ = unsafe { UnregisterHotKey(None, HOTKEY_ID) };
                return;
            }

            let mut msg = MSG::default();
            loop {
                // SAFETY: `msg` is a live, correctly aligned MSG for the
                // duration of the call. A null window filter asks for messages
                // posted to this thread, which is where WM_HOTKEY arrives.
                let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };

                // Zero means WM_QUIT was received; -1 means the queue broke.
                // Either way this thread is done.
                if result.0 <= 0 {
                    break;
                }

                if msg.message == WM_HOTKEY && msg.wParam.0 == HOTKEY_ID as usize {
                    on_press();
                }
            }

            // SAFETY: Releasing the shortcut on the same thread that registered
            // it, before the thread exits.
            let _ = unsafe { UnregisterHotKey(None, HOTKEY_ID) };
        })
        .ok()?;

    match ready_rx.recv() {
        Ok(Some(thread_id)) => Some(HotkeyListener {
            thread_id,
            handle: Some(handle),
        }),
        _ => {
            let _ = handle.join();
            None
        }
    }
}

impl Drop for HotkeyListener {
    fn drop(&mut self) {
        // SAFETY: Posting WM_QUIT to the listener thread's queue. The id was
        // captured on that thread and the thread is still alive because we are
        // about to join it, so the id cannot have been recycled yet.
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };

        if posted.is_err() {
            tracing::warn!("could not signal the hotkey thread to stop");
        }

        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// --------------------------------------------------------------- control

/// What is registered right now, if anything.
struct Registration {
    listener: Option<HotkeyListener>,
    config: HotkeyConfig,
}

/// Owns the overlay's shortcut and the one action it triggers, so the
/// combination can be changed while the program runs.
///
/// The settings panel and the loop that waits for the overlay both hold one of
/// these; the registration is released when the last of them goes away.
pub(crate) struct HotkeyControl {
    /// Kept rather than rebuilt per registration, because a new listener has to
    /// be handed the same action the old one had.
    on_press: Arc<dyn Fn() + Send + Sync + 'static>,
    registration: Mutex<Registration>,
}

impl HotkeyControl {
    /// Creates a control that holds no registration yet. Call
    /// [`HotkeyControl::apply`] to register a combination.
    pub(crate) fn new<F>(config: HotkeyConfig, on_press: F) -> Arc<Self>
    where
        F: Fn() + Send + Sync + 'static,
    {
        Arc::new(Self {
            on_press: Arc::new(on_press),
            registration: Mutex::new(Registration {
                listener: None,
                config,
            }),
        })
    }

    /// The combination currently in effect.
    pub(crate) fn config(&self) -> HotkeyConfig {
        lock(&self.registration).config
    }

    /// Whether the system actually accepted that combination.
    pub(crate) fn is_registered(&self) -> bool {
        lock(&self.registration).listener.is_some()
    }

    /// Registers `config`, replacing whatever was registered before. Returns
    /// whether the new combination is now in effect.
    ///
    /// The new registration is taken out *before* the old one is released, so
    /// a combination the system refuses — one another application already
    /// owns, most often — leaves the previous shortcut working instead of
    /// leaving the user with no shortcut at all. The two registrations overlap
    /// for the moment in between, which is harmless: they are different
    /// combinations, and each listener numbers its own within its own thread.
    pub(crate) fn apply(&self, config: HotkeyConfig) -> bool {
        let mut registration = lock(&self.registration);

        // Re-registering the combination already held would ask the system for
        // a shortcut we own ourselves, which it refuses.
        if registration.listener.is_some() && registration.config == config {
            return true;
        }

        let on_press = Arc::clone(&self.on_press);
        let Some(listener) = spawn(config, move || on_press()) else {
            return false;
        };

        let previous = registration.listener.replace(listener);
        registration.config = config;

        // Releases the combination that was held until now, and joins its
        // thread.
        drop(previous);

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(key: u32) -> HotkeyConfig {
        HotkeyConfig {
            ctrl: true,
            alt: true,
            shift: false,
            win: false,
            key,
        }
    }

    #[test]
    fn letters_map_to_their_ascii_codes() {
        assert_eq!(egui_key_to_vk(egui::Key::A), Some(0x41));
        assert_eq!(egui_key_to_vk(egui::Key::V), Some(0x56));
        assert_eq!(egui_key_to_vk(egui::Key::Z), Some(0x5A));
    }

    #[test]
    fn digits_map_to_their_ascii_codes() {
        assert_eq!(egui_key_to_vk(egui::Key::Num0), Some(0x30));
        assert_eq!(egui_key_to_vk(egui::Key::Num9), Some(0x39));
    }

    #[test]
    fn function_keys_run_from_the_f1_code() {
        assert_eq!(egui_key_to_vk(egui::Key::F1), Some(0x70));
        assert_eq!(egui_key_to_vk(egui::Key::F12), Some(0x7B));
    }

    #[test]
    fn named_keys_map_to_their_own_codes() {
        assert_eq!(egui_key_to_vk(egui::Key::Space), Some(0x20));
        assert_eq!(egui_key_to_vk(egui::Key::ArrowUp), Some(0x26));
    }

    #[test]
    fn a_key_outside_the_accepted_set_is_refused() {
        assert_eq!(egui_key_to_vk(egui::Key::Escape), None);
        assert_eq!(egui_key_to_vk(egui::Key::Backtick), None);
    }

    #[test]
    fn every_accepted_key_survives_the_round_trip() {
        let keys = LETTER_KEYS
            .iter()
            .chain(DIGIT_KEYS.iter())
            .chain(FUNCTION_KEYS.iter())
            .copied()
            .chain(NAMED_KEYS.iter().map(|(key, _, _)| *key));

        for key in keys {
            let vk = egui_key_to_vk(key).expect("an accepted key has a code");
            assert_ne!(
                key_name(vk),
                format!("Key 0x{vk:02X}"),
                "{key:?} has a code but no name"
            );
        }
    }

    #[test]
    fn a_shortcut_reads_the_way_it_is_printed() {
        assert_eq!(describe(HotkeyConfig::default()), "Ctrl+Alt+V");
        assert_eq!(
            describe(HotkeyConfig {
                ctrl: false,
                alt: false,
                shift: true,
                win: true,
                key: 0x70,
            }),
            "Shift+Win+F1"
        );
    }

    #[test]
    fn an_unknown_key_is_shown_as_its_code() {
        assert_eq!(describe(config(0xFF)), "Ctrl+Alt+Key 0xFF");
    }

    #[test]
    fn a_combination_without_a_modifier_is_rejected() {
        assert!(has_modifier(HotkeyConfig::default()));
        assert!(!has_modifier(HotkeyConfig {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            key: 0x56,
        }));
    }

    #[test]
    fn modifier_flags_always_include_no_repeat() {
        let flags = modifier_flags(HotkeyConfig::default());
        assert!(flags.contains(MOD_NOREPEAT));
        assert!(flags.contains(MOD_CONTROL));
        assert!(flags.contains(MOD_ALT));
        assert!(!flags.contains(MOD_SHIFT));
        assert!(!flags.contains(MOD_WIN));
    }

    #[test]
    fn a_fresh_control_holds_no_registration() {
        let control = HotkeyControl::new(HotkeyConfig::default(), || {});

        assert!(!control.is_registered());
        assert_eq!(control.config(), HotkeyConfig::default());
    }
}
