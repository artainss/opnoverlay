//! Global hotkeys through RegisterHotKey (the global-hotkey crate).

use std::sync::{Arc, Mutex};

use eframe::egui::Key;
use global_hotkey::hotkey::{self, Code, HotKey};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

use crate::hotkeys::Binding;

/// Whether `key` can be part of a global hotkey.
pub fn supports(key: Key) -> bool {
    code(key).is_some()
}

/// Parses "Ctrl+Alt+R" into a hotkey that can be registered.
fn parse(text: &str) -> Option<HotKey> {
    let mut parts: Vec<&str> = text.split('+').collect();
    let key = Key::from_name(parts.pop()?)?;
    let mut mods = hotkey::Modifiers::empty();
    for part in parts {
        mods |= match part {
            "Ctrl" => hotkey::Modifiers::CONTROL,
            "Alt" => hotkey::Modifiers::ALT,
            "Shift" => hotkey::Modifiers::SHIFT,
            _ => return None,
        };
    }
    Some(HotKey::new((!mods.is_empty()).then_some(mods), code(key)?))
}

fn code(key: Key) -> Option<Code> {
    // Named differently in egui and in keyboard-types.
    let renamed = match key {
        Key::ArrowUp => Some(Code::ArrowUp),
        Key::ArrowDown => Some(Code::ArrowDown),
        Key::ArrowLeft => Some(Code::ArrowLeft),
        Key::ArrowRight => Some(Code::ArrowRight),
        Key::Equals => Some(Code::Equal),
        Key::OpenBracket => Some(Code::BracketLeft),
        Key::CloseBracket => Some(Code::BracketRight),
        Key::Backtick => Some(Code::Backquote),
        _ => None,
    };
    if renamed.is_some() {
        return renamed;
    }
    let name = key.name();
    let code = match name.as_bytes() {
        [c] if c.is_ascii_uppercase() => format!("Key{name}"),
        [c] if c.is_ascii_digit() => format!("Digit{name}"),
        _ => name.to_owned(),
    };
    code.parse().ok()
}

pub struct GlobalHotkeys {
    manager: GlobalHotKeyManager,
    /// Registered hotkeys and the index of their binding, shared with the event handler.
    bound: Arc<Mutex<Vec<(HotKey, usize)>>>,
}

impl GlobalHotkeys {
    /// Starts listening; `on_press` gets the index (in the last `bind`) of a pressed hotkey.
    pub fn new(on_press: impl Fn(usize) + Send + Sync + 'static) -> Result<Self, String> {
        let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
        let bound: Arc<Mutex<Vec<(HotKey, usize)>>> = Arc::default();
        let handler_bound = bound.clone();
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.state != HotKeyState::Pressed {
                return;
            }
            let index = handler_bound.lock().unwrap().iter().find(|(hotkey, _)| hotkey.id() == event.id).map(|(_, i)| *i);
            if let Some(index) = index {
                on_press(index);
            }
        }));
        Ok(Self { manager, bound })
    }

    /// Replaces the registered hotkeys; returns the ones that could not be registered.
    pub fn bind(&mut self, bindings: &[Binding]) -> Vec<String> {
        self.unbind();
        let mut bound = Vec::new();
        let mut failed = Vec::new();
        for (index, binding) in bindings.iter().enumerate() {
            match parse(&binding.text) {
                Some(hotkey) if self.manager.register(hotkey).is_ok() => bound.push((hotkey, index)),
                _ => failed.push(binding.text.clone()),
            }
        }
        *self.bound.lock().unwrap() = bound;
        failed
    }

    /// Registration happens in `bind`, which reports failures itself.
    pub fn take_error(&self) -> Option<String> {
        None
    }

    /// Releases every hotkey, so the combinations reach the focused window again.
    pub fn unbind(&mut self) {
        let bound = std::mem::take(&mut *self.bound.lock().unwrap());
        let hotkeys: Vec<HotKey> = bound.iter().map(|(hotkey, _)| *hotkey).collect();
        let _ = self.manager.unregister_all(&hotkeys);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn conflicting_shortcut_is_reported_and_can_be_rebound() {
        let mut first = GlobalHotkeys::new(|_| {}).unwrap();
        let mut second = GlobalHotkeys::new(|_| {}).unwrap();
        let binding = || Binding { id: "menu", description: "test".into(), text: "Ctrl+Alt+F24".into() };
        assert!(first.bind(&[binding()]).is_empty());
        assert_eq!(second.bind(&[binding()]), vec!["Ctrl+Alt+F24"]);
        first.unbind();
        assert!(second.bind(&[binding()]).is_empty());
    }

    #[test]
    fn text_round_trips_to_a_registrable_hotkey() {
        let expected = HotKey::new(Some(hotkey::Modifiers::CONTROL | hotkey::Modifiers::ALT), Code::KeyR);
        assert_eq!(parse("Ctrl+Alt+R"), Some(expected));
        assert_eq!(parse("Ctrl+Shift+5"), Some(HotKey::new(Some(hotkey::Modifiers::CONTROL | hotkey::Modifiers::SHIFT), Code::Digit5)));
        assert_eq!(parse("F9"), Some(HotKey::new(None, Code::F9)));
        for (name, code) in [("Alt+F1", Code::F1), ("Alt+F9", Code::F9), ("Alt+F10", Code::F10)] {
            assert_eq!(parse(name), Some(HotKey::new(Some(hotkey::Modifiers::ALT), code)));
        }
        // Keys egui and keyboard-types name differently.
        assert_eq!(parse("Ctrl+Alt+Up"), Some(HotKey::new(Some(hotkey::Modifiers::CONTROL | hotkey::Modifiers::ALT), Code::ArrowUp)));
        for key in [Key::ArrowDown, Key::ArrowLeft, Key::ArrowRight, Key::Equals, Key::OpenBracket, Key::CloseBracket, Key::Backtick] {
            assert!(supports(key), "{key:?}");
        }
    }

    #[test]
    fn invalid_text_is_rejected() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("Ctrl+Alt+"), None);
        assert_eq!(parse("Hyper+R"), None);
    }
}
