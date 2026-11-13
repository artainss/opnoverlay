//! Global hotkeys through the GlobalShortcuts portal. On Wayland only the desktop sees key
//! presses, so it owns the shortcuts: obs-lite proposes its combinations and the desktop may
//! ask the user to confirm or change them.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ashpd::desktop::{CreateSessionOptions, Session};
use ashpd::desktop::global_shortcuts::{GlobalShortcuts as Portal, NewShortcut};
use eframe::egui::Key;
use futures_util::StreamExt;

use crate::hotkeys::Binding;

/// Whether `key` can be part of a global hotkey.
pub fn supports(key: Key) -> bool {
    keysym(key).is_some()
}

/// The key's name in the shortcuts specification (XKB keysym names).
fn keysym(key: Key) -> Option<String> {
    let name = key.name();
    match name.as_bytes() {
        [c] if c.is_ascii_alphabetic() => return Some(name.to_ascii_lowercase()),
        [c] if c.is_ascii_digit() => return Some(name.to_owned()),
        _ => {}
    }
    if name.len() > 1 && name.starts_with('F') && name[1..].parse::<u8>().is_ok() {
        return Some(name.to_owned());
    }
    let named = match key {
        Key::Space => "space",
        Key::Enter => "Return",
        Key::Tab => "Tab",
        Key::Escape => "Escape",
        Key::Backspace => "BackSpace",
        Key::Insert => "Insert",
        Key::Delete => "Delete",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "Page_Up",
        Key::PageDown => "Page_Down",
        Key::ArrowUp => "Up",
        Key::ArrowDown => "Down",
        Key::ArrowLeft => "Left",
        Key::ArrowRight => "Right",
        _ => return None,
    };
    Some(named.to_owned())
}

/// "Ctrl+Alt+R" as the portal expects it: "CTRL+ALT+r".
fn trigger(text: &str) -> Option<String> {
    let mut parts: Vec<&str> = text.split('+').collect();
    let key = keysym(Key::from_name(parts.pop()?)?)?;
    let mut out = String::new();
    for part in parts {
        out.push_str(match part {
            "Ctrl" => "CTRL+",
            "Alt" => "ALT+",
            "Shift" => "SHIFT+",
            _ => return None,
        });
    }
    out.push_str(&key);
    Some(out)
}

struct State {
    /// Bumped by each `bind`; listeners of older bindings stop.
    generation: AtomicU64,
    enabled: AtomicBool,
    ids: Mutex<Vec<&'static str>>,
    on_press: Box<dyn Fn(usize) + Send + Sync>,
    /// The portal session of the newest binding; it is closed when the next one starts, so
    /// only one session ever reports activations.
    session: Mutex<Option<Arc<Session<Portal>>>>,
    /// Why the newest binding failed, for the UI to show once.
    error: Mutex<Option<String>>,
}

pub struct GlobalHotkeys {
    state: Arc<State>,
}

impl GlobalHotkeys {
    /// Checks that the desktop offers the portal; `on_press` gets the index (in the last
    /// `bind`) of an activated shortcut.
    pub fn new(on_press: impl Fn(usize) + Send + Sync + 'static) -> Result<Self, String> {
        pollster::block_on(Portal::new()).map_err(|e| e.to_string())?;
        Ok(Self {
            state: Arc::new(State {
                generation: AtomicU64::new(0),
                enabled: AtomicBool::new(false),
                ids: Mutex::new(Vec::new()),
                on_press: Box::new(on_press),
                session: Mutex::new(None),
                error: Mutex::new(None),
            }),
        })
    }

    /// Proposes the shortcuts to the desktop; returns those that cannot be expressed. The
    /// desktop's answer (and any dialog it shows) comes later, off the UI thread.
    pub fn bind(&mut self, bindings: &[Binding]) -> Vec<String> {
        let generation = self.state.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let mut failed = Vec::new();
        let mut shortcuts = Vec::new();
        for binding in bindings {
            match trigger(&binding.text) {
                Some(trigger) => {
                    shortcuts.push(NewShortcut::new(binding.id, binding.description.clone()).preferred_trigger(trigger.as_str()));
                }
                None => failed.push(binding.text.clone()),
            }
        }
        *self.state.ids.lock().unwrap() = bindings.iter().map(|b| b.id).collect();
        self.state.enabled.store(true, Ordering::Release);
        if shortcuts.is_empty() {
            // None left: the desktop must stop taking the old combinations from other apps.
            if let Some(previous) = self.state.session.lock().unwrap().take() {
                thread::spawn(move || pollster::block_on(previous.close()));
            }
        } else {
            let state = self.state.clone();
            thread::spawn(move || {
                // A newer binding closes this one's session, which may fail it: not worth telling.
                if let Err(e) = pollster::block_on(listen(&state, generation, shortcuts))
                    && state.generation.load(Ordering::Acquire) == generation
                {
                    *state.error.lock().unwrap() = Some(e.to_string());
                }
            });
        }
        failed
    }

    /// Stops acting on the shortcuts (the desktop keeps them until the next `bind`).
    pub fn unbind(&mut self) {
        self.state.enabled.store(false, Ordering::Release);
    }

    /// Why the desktop refused the shortcuts, if it did since the last call.
    pub fn take_error(&self) -> Option<String> {
        self.state.error.lock().unwrap().take()
    }
}

async fn listen(state: &State, generation: u64, shortcuts: Vec<NewShortcut>) -> ashpd::Result<()> {
    let previous = state.session.lock().unwrap().take();
    if let Some(previous) = previous {
        let _ = previous.close().await;
    }
    let portal = Portal::new().await?;
    let session = Arc::new(portal.create_session(CreateSessionOptions::default()).await?);
    *state.session.lock().unwrap() = Some(session.clone());
    let mut activated = portal.receive_activated().await?;
    portal.bind_shortcuts(&session, &shortcuts, None, Default::default()).await?.response()?;
    while let Some(event) = activated.next().await {
        // A newer binding replaced this one (and closed its session).
        if state.generation.load(Ordering::Acquire) != generation {
            break;
        }
        if !state.enabled.load(Ordering::Acquire) {
            continue;
        }
        let index = state.ids.lock().unwrap().iter().position(|id| *id == event.shortcut_id());
        if let Some(index) = index {
            (state.on_press)(index);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkeys_become_portal_triggers() {
        assert_eq!(trigger("Ctrl+Alt+R").as_deref(), Some("CTRL+ALT+r"));
        assert_eq!(trigger("Ctrl+Shift+5").as_deref(), Some("CTRL+SHIFT+5"));
        assert_eq!(trigger("F9").as_deref(), Some("F9"));
        assert_eq!(trigger("Alt+PageUp").as_deref(), Some("ALT+Page_Up"));
        assert_eq!(trigger("Hyper+R"), None);
        assert_eq!(trigger(""), None);
    }
}
