//! Linux implementation: the ScreenCast and Camera portals and PipeWire for capture,
//! GStreamer for audio, encoding and MP4, and the GlobalShortcuts portal for hotkeys.

pub mod audio;
pub mod camera;
pub mod capture;
pub mod encoder;
mod gpu;
pub mod hotkeys;
mod portal;
pub mod sys;

use crate::i18n::tr;

/// The app id the desktop knows obs-lite by (the same as the Flatpak's).
pub const APP_ID: &str = "io.github._25Dark25.ObsLite";

/// Error for what the Linux version cannot do yet.
fn not_yet() -> String {
    tr!("todavía no disponible en Linux", "not available on Linux yet").to_owned()
}
