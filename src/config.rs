//! Persistent user settings stored as JSON in %APPDATA%\screen-overlay\settings.json.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::camera::CameraConfig;
use crate::i18n::{Language, tr};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ReplayStorage {
    Memory,
    Disk,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize, Default)]
pub enum HudCorner {
    TopLeft,
    #[default]
    TopRight,
    BottomLeft,
    BottomRight,
}

impl HudCorner {
    pub const ALL: [Self; 4] = [Self::TopLeft, Self::TopRight, Self::BottomLeft, Self::BottomRight];

}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Quality {
    Low,
    Medium,
    High,
    Custom,
}

impl Quality {
    pub const ALL: [Self; 4] = [Self::Low, Self::Medium, Self::High, Self::Custom];

    /// Bitrate scaled from a 1080p60 reference by pixel rate.
    pub fn bitrate(self, width: u32, height: u32, fps: u32) -> u32 {
        let reference = match self {
            Self::Low => 6_000_000.0,
            Self::Medium => 12_000_000.0,
            Self::High => 24_000_000.0,
            Self::Custom => 12_000_000.0,
        };
        let pixel_rate = width as f64 * height as f64 * fps as f64 / (1920.0 * 1080.0 * 60.0);
        (reference * pixel_rate).clamp(1_500_000.0, 80_000_000.0) as u32
    }

    pub fn selected_bitrate(self, custom_mbps: u32, width: u32, height: u32, fps: u32) -> u32 {
        if self == Self::Custom { custom_mbps.clamp(1, 80) * 1_000_000 } else { self.bitrate(width, height, fps) }
    }
}

/// Output height; the capture is only ever scaled down.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Resolution {
    Native,
    P1440,
    P1080,
    P720,
}

impl Resolution {
    pub const ALL: [Self; 4] = [Self::Native, Self::P1440, Self::P1080, Self::P720];

    pub fn label(self) -> &'static str {
        match self {
            Self::Native => tr!("Nativa", "Native"),
            Self::P1440 => "1440p",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
        }
    }

    /// Output size for a capture of `(w, h)`, keeping the aspect ratio and even dimensions.
    pub fn apply(self, (w, h): (u32, u32)) -> (u32, u32) {
        let target = match self {
            Self::Native => return (w & !1, h & !1),
            Self::P1440 => 1440,
            Self::P1080 => 1080,
            Self::P720 => 720,
        };
        if h <= target {
            return (w & !1, h & !1);
        }
        let scaled_w = (w as u64 * target as u64 / h as u64) as u32;
        (scaled_w.max(2) & !1, target)
    }
}

/// Part of the source to record, in source pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    /// Smaller regions are almost certainly a mis-click.
    const MIN: u32 = 16;

    /// The region clipped to a source of `(w, h)`, with the even dimensions H.264 needs;
    /// `None` if little or nothing of it is left.
    pub fn fit(self, (w, h): (u32, u32)) -> Option<Self> {
        if self.x >= w || self.y >= h {
            return None;
        }
        let width = self.width.min(w - self.x) & !1;
        let height = self.height.min(h - self.y) & !1;
        (width >= Self::MIN && height >= Self::MIN).then_some(Self { width, height, ..self })
    }
}

/// Global hotkeys as text ("Ctrl+Alt+R"); `None` means unassigned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub menu: Option<String>,
    pub record: Option<String>,
    pub pause: Option<String>,
    pub screenshot: Option<String>,
    pub mute_mic: Option<String>,
    pub replay: Option<String>,
    pub save_replay: Option<String>,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            menu: Some("Alt+Z".into()),
            record: Some("Alt+F9".into()),
            pause: Some("Ctrl+Alt+P".into()),
            screenshot: Some("Alt+F1".into()),
            mute_mic: None,
            replay: None,
            save_replay: Some("Alt+F10".into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ScreenshotFormat {
    #[default]
    Png,
    Jpeg,
}

impl ScreenshotFormat {
    pub const ALL: [Self; 2] = [Self::Png, Self::Jpeg];

    pub fn extension(self) -> &'static str {
        match self { Self::Png => "png", Self::Jpeg => "jpg" }
    }

    pub fn label(self) -> &'static str {
        match self { Self::Png => "PNG", Self::Jpeg => "JPEG" }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// `None` follows the Windows display language.
    pub language: Option<Language>,
    /// Keep the obs-lite window out of every capture, including its own preview.
    pub hide_from_capture: bool,
    /// Minimizing hides the window; the tray icon brings it back.
    pub minimize_to_tray: bool,
    pub notifications: bool,
    pub hide_notifications_from_capture: bool,
    /// Ask GitHub for a newer release at start-up.
    pub check_updates: bool,
    /// Release whose start-up dialog the user asked not to see again.
    pub skipped_version: Option<String>,
    pub output_dir: PathBuf,
    pub replay_dir: PathBuf,
    pub screenshot_dir: PathBuf,
    pub screenshot_format: ScreenshotFormat,
    pub jpeg_quality: u8,
    pub video_name_template: String,
    pub gallery_files: Vec<PathBuf>,
    pub hud_enabled: bool,
    pub hud_cpu: bool,
    pub hud_fps: bool,
    pub hud_memory: bool,
    pub hud_gpu: bool,
    pub hud_gpu_temperature: bool,
    pub hud_video_memory: bool,
    pub hud_corner: HudCorner,
    pub fps: u32,
    pub resolution: Resolution,
    pub quality: Quality,
    pub custom_bitrate_mbps: u32,
    pub cursor: bool,
    pub system_audio: bool,
    pub system_volume: f32,
    pub system_muted: bool,
    pub mic: bool,
    pub mic_volume: f32,
    pub mic_muted: bool,
    /// RNNoise noise suppression on the microphone.
    pub mic_denoise: bool,
    /// Executable whose audio is captured on its own (Application Audio Capture).
    pub app_audio: Option<String>,
    pub app_volume: f32,
    pub app_muted: bool,
    /// WASAPI endpoint IDs; `None` follows the Windows default device.
    pub system_device: Option<String>,
    pub mic_device: Option<String>,
    pub source_label: Option<String>,
    /// Record only this part of the source.
    pub region: Option<Region>,
    /// Webcam overlaid on the recording.
    pub camera: Option<CameraConfig>,
    pub hotkeys: Hotkeys,
    pub auto_start_replay: bool,
    pub auto_start_windows: bool,
    pub replay_storage: ReplayStorage,
    pub setup_complete: bool,
    /// Seconds the replay buffer keeps.
    pub replay_seconds: u32,
    /// The last recording started, repaired at start-up in case obs-lite stopped while writing it.
    pub last_recording: Option<PathBuf>,
    pub pending_recoveries: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: None,
            hide_from_capture: true,
            minimize_to_tray: false,
            notifications: true,
            hide_notifications_from_capture: true,
            check_updates: false,
            skipped_version: None,
            output_dir: dirs::video_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join("opnoverlay"),
            replay_dir: dirs::video_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join("opnoverlay"),
            screenshot_dir: dirs::video_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from(".")).join("opnoverlay"),
            screenshot_format: ScreenshotFormat::Png,
            jpeg_quality: 85,
            video_name_template: "[ГГГГ-ММ-ДД] [ЧЧ-ММ-СС] — Video".into(),
            gallery_files: Vec::new(),
            hud_enabled: false,
            hud_cpu: true,
            hud_fps: false,
            hud_memory: true,
            hud_gpu: true,
            hud_gpu_temperature: true,
            hud_video_memory: true,
            hud_corner: HudCorner::default(),
            fps: 60,
            resolution: Resolution::Native,
            quality: Quality::High,
            custom_bitrate_mbps: 12,
            cursor: true,
            system_audio: true,
            system_volume: 1.0,
            system_muted: false,
            mic: false,
            mic_volume: 1.0,
            mic_muted: false,
            mic_denoise: false,
            app_audio: None,
            app_volume: 1.0,
            app_muted: false,
            system_device: None,
            mic_device: None,
            source_label: None,
            region: None,
            camera: None,
            hotkeys: Hotkeys::default(),
            auto_start_replay: true,
            auto_start_windows: false,
            replay_storage: ReplayStorage::Memory,
            setup_complete: false,
            replay_seconds: 45,
            last_recording: None,
            pending_recoveries: Vec::new(),
        }
    }
}

#[cfg(test)]
thread_local! {
    static TEST_CONFIG_ROOT: Option<tempfile::TempDir> = tempfile::tempdir().ok();
}

pub(crate) fn path() -> Option<PathBuf> {
    #[cfg(test)]
    let root = TEST_CONFIG_ROOT.with(|root| root.as_ref().map(|root| root.path().to_owned()))?;
    #[cfg(not(test))]
    let root = std::env::var_os("OPNOVERLAY_CONFIG_DIR").map(PathBuf::from).filter(|path| path.is_absolute()).or_else(dirs::config_dir)?;
    Some(root.join("screen-overlay").join("settings.json"))
}

impl Config {
    pub fn load() -> Self {
        path().and_then(|p| fs::read_to_string(p).ok()).and_then(|s| Self::parse(&s)).unwrap_or_default()
    }

    /// Editors such as Windows PowerShell save UTF-8 with a BOM, which serde_json rejects;
    /// without this the app would start with defaults and overwrite the file on exit.
    fn parse(json: &str) -> Option<Self> {
        let mut config: Self = serde_json::from_str(json.trim_start_matches('\u{feff}')).ok()?;
        if matches!(config.video_name_template.as_str(), "video [ГГГГ-ММ-ДД] [ЧЧ-ММ-СС]" | "Видео [ГГГГ-ММ-ДД] [ЧЧ-ММ-СС]" | "[ГГГГ-ММ-ДД] [ЧЧ-ММ-СС] — Видео") {
            config.video_name_template = "[ГГГГ-ММ-ДД] [ЧЧ-ММ-СС] — Video".into();
        }
        let default_dir = Self::default().output_dir;
        if config.output_dir == default_dir.join("Записи") && (config.replay_dir != config.output_dir || config.screenshot_dir != config.output_dir) {
            config.output_dir = default_dir;
        }
        config.replay_dir.clone_from(&config.output_dir);
        config.screenshot_dir.clone_from(&config.output_dir);
        config.fps = config.fps.clamp(1, 240);
        Some(config)
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = write_replacing(&p, json.as_bytes());
        }
    }
}

/// Writes `path` through a new file next to it, so a crash or a full disk midway leaves the old
/// contents rather than a cut-short file (which would load as the defaults).
fn write_replacing(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let new = path.with_extension("new");
    let written = File::create(&new).and_then(|mut f| f.write_all(data).and_then(|()| f.sync_all()));
    match written.and_then(|()| fs::rename(&new, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&new);
            Err(e)
        }
    }
}

/// Whether no other obs-lite runs with these settings: the first holds a lock on a file next to
/// them until it exits. Only that one repairs a recording cut short, which another may still be
/// writing.
pub fn first_instance() -> bool {
    static LOCK: OnceLock<Option<File>> = OnceLock::new();
    let lock = LOCK.get_or_init(|| path().and_then(|p| try_lock(&p.with_file_name("instance.lock"))));
    lock.is_some()
}

/// The file at `path`, locked for as long as it is open; `None` if another process holds it.
fn try_lock(path: &Path) -> Option<File> {
    fs::create_dir_all(path.parent()?).ok()?;
    let file = File::options().create(true).truncate(false).write(true).open(path).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn installed_settings_parse_and_preserve_fields() {
        let path = std::env::var_os("OPNOVERLAY_TEST_SETTINGS").expect("test settings path required");
        let json = std::fs::read_to_string(path).expect("test settings readable");
        let original: serde_json::Value = serde_json::from_str(json.trim_start_matches('\u{feff}')).expect("valid settings JSON");
        let config = Config::parse(&json).expect("installed settings parse with current code");
        let serialized = serde_json::to_value(config).unwrap();
        let mut changed = Vec::new();
        for (key, value) in original.as_object().unwrap() {
            if key == "fps" {
                if let Some(fps) = value.as_u64() { assert_eq!(serialized[key].as_u64(), Some(fps.clamp(1, 240))); }
                continue;
            }
            if key == "video_name_template" && matches!(value.as_str(), Some("Видео [ГГГГ-ММ-ДД] [ЧЧ-ММ-СС]" | "video [ГГГГ-ММ-ДД] [ЧЧ-ММ-СС]" | "[ГГГГ-ММ-ДД] [ЧЧ-ММ-СС] — Видео")) {
                assert_eq!(serialized[key].as_str(), Some("[ГГГГ-ММ-ДД] [ЧЧ-ММ-СС] — Video"));
                continue;
            }
            if matches!(key.as_str(), "output_dir" | "replay_dir" | "screenshot_dir") {
                continue;
            }
            if serialized.get(key) != Some(value) { changed.push(key.as_str()); }
        }
        assert!(changed.is_empty(), "settings changed or unrecognized: {changed:?}");
    }

    #[test]
    fn defaults_match_capture_preferences() {
        let config = Config::default();
        assert_eq!(config.video_name_template, "[ГГГГ-ММ-ДД] [ЧЧ-ММ-СС] — Video");
        assert_eq!(config.replay_seconds, 45);
        assert!(config.auto_start_replay);
        assert_eq!(config.fps, 60);
        assert_eq!(config.quality, Quality::High);
        assert!(!config.check_updates);
        assert_eq!(config.hotkeys.record.as_deref(), Some("Alt+F9"));
        assert_eq!(config.hotkeys.menu.as_deref(), Some("Alt+Z"));
        assert_eq!(config.hotkeys.screenshot.as_deref(), Some("Alt+F1"));
        assert_eq!(config.hotkeys.save_replay.as_deref(), Some("Alt+F10"));
        assert_eq!(config.output_dir, config.replay_dir);
        assert_eq!(config.output_dir, config.screenshot_dir);
        assert!(path().unwrap().ends_with(Path::new("screen-overlay").join("settings.json")));
    }

    #[test]
    fn tests_use_a_temporary_config_instead_of_the_user_profile() {
        let config = path().unwrap();
        assert!(config.is_absolute());
        if let Some(user_root) = dirs::config_dir() {
            assert!(!config.starts_with(user_root));
        }
        assert!(config.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn migrates_only_the_previous_default_video_name() {
        let mut config = Config::default();
        config.video_name_template = "Видео [ГГГГ-ММ-ДД] [ЧЧ-ММ-СС]".into();
        let parsed = Config::parse(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed.video_name_template, "[ГГГГ-ММ-ДД] [ЧЧ-ММ-СС] — Video");
        config.video_name_template = "[ГГГГ-ММ-ДД] [ЧЧ-ММ-СС] — Видео".into();
        let parsed = Config::parse(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed.video_name_template, "[ГГГГ-ММ-ДД] [ЧЧ-ММ-СС] — Video");
        config.video_name_template = "Мои записи [ЧЧ-ММ]".into();
        let parsed = Config::parse(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed.video_name_template, config.video_name_template);
    }

    #[test]
    fn old_folders_migrate_to_the_common_folder_without_changing_custom_output() {
        let mut config = Config::default();
        let common = config.output_dir.clone();
        config.output_dir = common.join("Записи");
        config.replay_dir = common.join("Откаты");
        config.screenshot_dir = common.join("Скриншоты");
        let parsed = Config::parse(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed.output_dir, common);
        assert_eq!(parsed.replay_dir, common);
        assert_eq!(parsed.screenshot_dir, common);
        config.output_dir = std::env::temp_dir().join("custom-folder");
        let parsed = Config::parse(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed.output_dir, config.output_dir);
        assert_eq!(parsed.replay_dir, config.output_dir);
        assert_eq!(parsed.screenshot_dir, config.output_dir);
    }

    #[test]
    fn only_one_process_holds_the_instance_lock() {
        let path = std::env::temp_dir().join(format!("obs-lite-lock-{}", std::process::id()));
        let held = try_lock(&path).expect("free at first");
        // A second open file is what a second process would have.
        assert!(try_lock(&path).is_none());
        drop(held);
        assert!(try_lock(&path).is_some(), "released when the holder exits");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn settings_are_replaced_whole() {
        let dir = std::env::temp_dir().join(format!("obs-lite-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        write_replacing(&path, b"old").unwrap();
        write_replacing(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1, "no temporary file left");
        // The new contents cannot be written (a folder is in the way): the old ones stay.
        fs::create_dir(path.with_extension("new")).unwrap();
        assert!(write_replacing(&path, b"lost").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bitrate_scales_with_pixel_rate() {
        assert_eq!(Quality::Medium.bitrate(1920, 1080, 60), 12_000_000);
        assert_eq!(Quality::Medium.bitrate(1920, 1080, 30), 6_000_000);
        assert_eq!(Quality::High.bitrate(3840, 2160, 60), 80_000_000); // clamped
        assert_eq!(Quality::Low.bitrate(640, 360, 30), 1_500_000); // clamped
        assert_eq!(Quality::Custom.selected_bitrate(8, 1920, 1080, 60), 8_000_000);
        assert_eq!(Quality::Custom.selected_bitrate(100, 1920, 1080, 60), 80_000_000);
        assert_eq!(Quality::Custom.selected_bitrate(0, 1920, 1080, 60), 1_000_000);
    }

    #[test]
    fn resolution_scales_down_only_and_keeps_aspect() {
        assert_eq!(Resolution::P1080.apply((3840, 2160)), (1920, 1080));
        assert_eq!(Resolution::P720.apply((2560, 1440)), (1280, 720));
        assert_eq!(Resolution::P1080.apply((1280, 720)), (1280, 720));
        assert_eq!(Resolution::P720.apply((3440, 1440)), (1720, 720));
        assert_eq!(Resolution::Native.apply((1001, 777)), (1000, 776));
    }

    #[test]
    fn region_is_clipped_to_the_source_with_even_sides() {
        let r = Region { x: 100, y: 50, width: 301, height: 2000 };
        assert_eq!(r.fit((1920, 1080)), Some(Region { x: 100, y: 50, width: 300, height: 1030 }));
        assert_eq!(r.fit((300, 1080)), Some(Region { x: 100, y: 50, width: 200, height: 1030 }));
        assert_eq!(r.fit((100, 1080)), None); // starts outside a source that shrank
        assert_eq!(Region { x: 0, y: 0, width: 10, height: 500 }.fit((1920, 1080)), None);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let c: Config = serde_json::from_str(r#"{"fps": 30}"#).unwrap();
        assert_eq!(c.fps, 30);
        assert_eq!(c.quality, Quality::High);
        assert_eq!(c.replay_dir, Config::default().replay_dir);
        assert_eq!(c.screenshot_dir, Config::default().screenshot_dir);
    }

    #[test]
    fn settings_saved_with_a_utf8_bom_still_load() {
        let c = Config::parse("\u{feff}{\"fps\": 30}").expect("settings with a BOM are rejected");
        assert_eq!(c.fps, 30);
    }

    #[test]
    fn loaded_fps_is_kept_in_supported_range() {
        assert_eq!(Config::parse("{\"fps\": 0}").unwrap().fps, 1);
        assert_eq!(Config::parse("{\"fps\": 1000}").unwrap().fps, 240);
        assert_eq!(Config::parse("{\"fps\": 144}").unwrap().fps, 144);
    }
}
