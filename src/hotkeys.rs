//! Global hotkeys as the text shown in Settings and stored in settings.json ("Ctrl+Alt+R").
//! Registering them is platform code (`platform::hotkeys`).

use eframe::egui::{Key, Modifiers};

pub use crate::platform::hotkeys::GlobalHotkeys;

/// A hotkey to register: a stable id (for desktops that store shortcuts), what it does, and
/// the combination as text.
pub struct Binding {
    // Only desktops that keep the shortcuts themselves (the Linux portal) use these two.
    #[cfg_attr(windows, allow(dead_code))]
    pub id: &'static str,
    #[cfg_attr(windows, allow(dead_code))]
    pub description: String,
    pub text: String,
}

/// The text for a combination pressed in Settings, or `None` if it cannot be a global hotkey.
/// Without Ctrl or Alt, only F-keys are allowed: a global "R" or "Shift+R" would stop that
/// key from typing in every other application.
pub fn format(mods: Modifiers, key: Key) -> Option<String> {
    use Key::{AltLeft, AltRight, ControlLeft, ControlRight, ShiftLeft, ShiftRight, SuperLeft, SuperRight};
    if matches!(key, ShiftLeft | ShiftRight | ControlLeft | ControlRight | AltLeft | AltRight | SuperLeft | SuperRight) {
        return None; // still waiting for the actual key
    }
    if !crate::platform::hotkeys::supports(key) {
        return None;
    }
    let function_key = key.name().len() > 1 && key.name().starts_with('F') && key.name()[1..].parse::<u8>().is_ok();
    if !(mods.ctrl || mods.alt || function_key) {
        return None;
    }
    let mut text = String::new();
    for (on, name) in [(mods.ctrl, "Ctrl+"), (mods.alt, "Alt+"), (mods.shift, "Shift+")] {
        if on {
            text.push_str(name);
        }
    }
    text.push_str(key.name());
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL_ALT: Modifiers = Modifiers { alt: true, ctrl: true, shift: false, mac_cmd: false, command: true };

    #[test]
    fn a_pressed_combination_becomes_text() {
        assert_eq!(format(CTRL_ALT, Key::R).as_deref(), Some("Ctrl+Alt+R"));
        assert_eq!(format(Modifiers::CTRL | Modifiers::SHIFT, Key::Num5).as_deref(), Some("Ctrl+Shift+5"));
    }

    #[test]
    fn keys_that_would_break_typing_are_rejected() {
        assert_eq!(format(Modifiers::NONE, Key::R), None);
        assert_eq!(format(Modifiers::SHIFT, Key::R), None);
        assert_eq!(format(Modifiers::NONE, Key::F9).as_deref(), Some("F9"));
    }

    #[test]
    fn a_modifier_alone_is_not_a_hotkey() {
        // egui reports the modifier keys themselves as key presses too.
        assert_eq!(format(CTRL_ALT, Key::ControlLeft), None);
        assert_eq!(format(CTRL_ALT, Key::AltRight), None);
        assert_eq!(format(Modifiers::SHIFT, Key::ShiftLeft), None);
    }
}
