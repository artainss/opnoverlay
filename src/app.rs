//! UI and recording lifecycle.
//!
//! The layout follows OBS Studio (preview on top; Sources, Audio Mixer and Controls
//! docks below; status bar; Settings dialog) and uses its Yami palette and Spanish
//! wording, so OBS users find everything where they expect it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Button, Color32, ComboBox, CornerRadius, FontId, Layout, Margin, Rect, RichText, Sense,
    Slider, Stroke, TextureHandle, TextureOptions, Vec2, pos2, vec2,
};

use crate::audio::{self, Device, Input, InputKind, METER_FLOOR, Mixer};
use crate::camera::{self, Camera, CameraConfig, CameraDevice, CameraFrame, CameraSize, Corner};
use crate::capture::{self, RecordConfig, Session, SessionOptions, Source, SourceId};
use crate::platform::encoder::{Encoder, PendingFile};
use crate::config::{Config, Hotkeys, HudCorner, Quality, Region, ReplayStorage, Resolution, ScreenshotFormat};
use crate::hotkeys::{self, Binding, GlobalHotkeys};
use crate::i18n::{self, Language, tr, tr3};
use crate::tray::{Tray, TrayCommand, TrayState};
use crate::update::Release;

const PREVIEW_FPS: u32 = 5;
const DOCK_HEIGHT: f32 = 236.0;

// OBS "Yami" theme palette.
const GREY1: Color32 = Color32::from_rgb(0x63, 0x75, 0x76);
const GREY3: Color32 = Color32::from_rgb(0x35, 0x46, 0x49);
const GREY4: Color32 = Color32::from_rgb(0x2B, 0x39, 0x3C);
const GREY6: Color32 = Color32::from_rgb(0x25, 0x30, 0x33);
const GREY7: Color32 = Color32::from_rgb(0x1C, 0x25, 0x29);
const GREY8: Color32 = Color32::from_rgb(0x15, 0x1A, 0x1F);
const PRIMARY: Color32 = Color32::from_rgb(0x30, 0x5E, 0x56);
const PRIMARY_LIGHT: Color32 = Color32::from_rgb(0xA6, 0xDD, 0xD0);
const TEXT_MUTED: Color32 = Color32::from_rgb(0xB4, 0xC2, 0xC0);
const RED: Color32 = Color32::from_rgb(0xE3, 0x3B, 0x57);
const YELLOW: Color32 = Color32::from_rgb(0xE5, 0xAF, 0x24);
const GREEN: Color32 = Color32::from_rgb(0x37, 0xD2, 0x47);
const RED_DIM: Color32 = Color32::from_rgb(0x7D, 0x12, 0x24);
const YELLOW_DIM: Color32 = Color32::from_rgb(0x6E, 0x52, 0x0D);
const GREEN_DIM: Color32 = Color32::from_rgb(0x17, 0x64, 0x1E);

#[derive(Clone, Copy)]
enum Action {
    ToggleMenu,
    ToggleRecording,
    TogglePause,
    ToggleReplay,
    SaveReplay,
    Screenshot,
    ToggleMicMute,
}

enum RecState {
    Idle,
    /// Capturing and encoding, for a recording, the replay buffer or both.
    Active { session: Session, mixer: Option<Mixer>, recording: Option<Recording>, replay: bool },
    /// The last output was stopped and the recording is being written out.
    Saving(Receiver<Result<PathBuf, String>>),
}

/// A recording in progress (the session may also be running for the replay buffer).
struct Recording;

struct Status {
    text: String,
    file: Option<PathBuf>,
    error: bool,
}

struct OpeningAnimation {
    last: Instant,
    progress: f32,
    from: egui::Pos2,
    to: egui::Pos2,
    rest: egui::Pos2,
    duration: f32,
}

impl OpeningAnimation {
    fn advance(&mut self, now: Instant) -> f32 {
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;
        self.progress = (self.progress + elapsed.as_secs_f32() / self.duration).min(1.0);
        self.progress
    }

    fn position(&self) -> egui::Pos2 {
        let eased = self.progress * self.progress * (3.0 - 2.0 * self.progress);
        self.from.lerp(self.to, eased)
    }
}

struct ImageViewer {
    path: PathBuf,
    loading: Option<Receiver<Result<egui::ColorImage, String>>>,
    texture: Option<TextureHandle>,
    error: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum SettingsPage {
    General,
    Output,
    Audio,
    Video,
    Hotkeys,
}

/// The Settings dialog edits a copy; tr!("Aceptar", "OK") applies it, tr!("Cancelar", "Cancel") discards it.
struct SettingsDialog {
    draft: Config,
    page: SettingsPage,
    replay_duration: String,
    selected_source: Option<SourceId>,
    /// The hotkey row waiting for a key combination.
    capturing: Option<usize>,
}

#[derive(Default)]
struct StoragePanel {
    folder: Option<PathBuf>,
    report: Option<crate::storage::StorageReport>,
    loading: Option<Receiver<crate::storage::StorageReport>>,
    failed: bool,
}

/// Process CPU usage as a share of all cores, like the OBS status bar.
struct CpuMeter {
    last: Option<(Instant, u64)>,
    percent: f64,
}

#[derive(Default)]
struct SystemMeter {
    last: Option<(Instant, (u64, u64))>,
    cpu_percent: Option<f64>,
    memory: Option<(u64, u64)>,
    #[cfg(windows)]
    gpu_receiver: Option<Receiver<crate::platform::nvml::GpuStats>>,
    #[cfg(windows)]
    gpu: Option<crate::platform::nvml::GpuStats>,
    #[cfg(windows)]
    fps_monitor: Option<crate::platform::fps::FpsMonitor>,
    #[cfg(windows)]
    fps: Option<(Instant, Option<f64>)>,
}

pub struct App {
    config: Config,
    sources: Vec<Source>,
    selected: Option<SourceId>,
    system_devices: Vec<Device>,
    mic_devices: Vec<Device>,
    preview: Option<Session>,
    rec: RecState,
    /// A screenshot being written to disk.
    screenshots: Vec<Receiver<Result<PathBuf, String>>>,
    system: Option<Input>,
    mic: Option<Input>,
    /// Application Audio Capture, when its application is running.
    app_audio: Option<Input>,
    audio_apps: Vec<capture::AudioApp>,
    cameras: Vec<CameraDevice>,
    camera: Option<Camera>,
    camera_texture: Option<TextureHandle>,
    camera_seq: Option<u64>,
    meters: [f32; 3],
    /// For audio inputs to wake the UI when they start to make sound.
    ctx: egui::Context,
    /// When the window was last drawn, to tell when it is not shown (see `preview_hidden`).
    last_drawn: Instant,
    texture: Option<TextureHandle>,
    texture_seq: Option<u64>,
    /// Choosing a region on the preview, and the drag in progress (start, end).
    picking_region: bool,
    region_drag: Option<(egui::Pos2, egui::Pos2)>,
    status: Option<Status>,
    notification: Option<(Instant, String, bool, Arc<OnceLock<Instant>>)>,
    settings: Option<SettingsDialog>,
    storage_panel: StoragePanel,
    cpu: CpuMeter,
    system_meter: SystemMeter,
    hud_meter: Arc<Mutex<SystemMeter>>,
    actions: Arc<Mutex<Vec<Action>>>,
    hotkeys: Option<GlobalHotkeys>,
    /// The action of each bound hotkey, by index, shared with the hotkey handler.
    bound_actions: Arc<Mutex<Vec<Action>>>,
    /// The main window, for excluding it from capture.
    native_window: Option<isize>,
    tray: Option<Tray>,
    tray_commands: Arc<Mutex<Vec<TrayCommand>>>,
    /// Minimized to the tray.
    hidden: bool,
    opening: Option<OpeningAnimation>,
    closing: Option<OpeningAnimation>,
    #[cfg(windows)]
    focus_armed: bool,
    exit_requested: bool,
    advanced_view: bool,
    last_mic_error: Option<String>,
    last_system_error: Option<String>,
    last_app_error: Option<String>,
    pending_delete: Option<PathBuf>,
    pending_rename: Option<(PathBuf, String)>,
    image_viewer: Option<ImageViewer>,
    #[cfg(windows)]
    player: Option<(PathBuf, crate::platform::player::Player, f32)>,
    #[cfg(windows)]
    player_error: Option<String>,
    #[cfg(windows)]
    pending_trim: Option<(PathBuf, String, String)>,
    #[cfg(windows)]
    trim_frames: Vec<TextureHandle>,
    #[cfg(windows)]
    trim_loading: Option<Receiver<Result<Vec<egui::ColorImage>, String>>>,
    #[cfg(windows)]
    trim_requested: bool,
    #[cfg(windows)]
    trim_error: Option<String>,
    gallery_deletions: Vec<Receiver<(PathBuf, Result<(), String>)>>,
    gallery_thumbnails: HashMap<PathBuf, Option<TextureHandle>>,
    gallery_loading: HashMap<PathBuf, Receiver<Option<egui::ColorImage>>>,
    disk_check: Option<Receiver<Vec<(&'static str, Option<u64>)>>>,
    last_disk_check: Option<Instant>,
    low_disk_warned: bool,
    #[cfg(windows)]
    desktop_available: bool,
    #[cfg(windows)]
    desktop_checked: Instant,
    #[cfg(windows)]
    desktop_resume: Option<(bool, bool, bool)>,
    /// A newer release, once the start-up check has answered.
    update: Option<Release>,
    update_check: Option<Receiver<Option<Release>>>,
    /// The dialog telling of `update` is open.
    update_dialog: bool,
    /// Files being written in the background (a stopped recording, a replay).
    jobs: Vec<JoinHandle<Status>>,
    /// Output files named this run, which may not exist yet.
    named: HashSet<PathBuf>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_theme(&cc.egui_ctx);
        let mut config = Config::load();
        i18n::set(config.language.unwrap_or_else(Language::system));
        let startup_error = match crate::platform::sys::startup_enabled() {
            Ok(enabled) => { config.auto_start_windows = enabled; None }
            Err(error) => Some(error),
        };
        if cfg!(windows) && !config.setup_complete && startup_error.is_none() {
            config.auto_start_windows = true;
        }
        let actions = Arc::new(Mutex::new(Vec::new()));
        let bound_actions: Arc<Mutex<Vec<Action>>> = Arc::default();
        let on_press = {
            let (actions, bound, ctx) = (actions.clone(), bound_actions.clone(), cc.egui_ctx.clone());
            move |index: usize| {
                if let Some(action) = bound.lock().unwrap().get(index).copied() {
                    actions.lock().unwrap().push(action);
                    ctx.request_repaint();
                }
            }
        };
        let (hotkeys, hotkey_error) = match GlobalHotkeys::new(on_press) {
            Ok(hotkeys) => (Some(hotkeys), None),
            Err(e) => (None, Some(e)),
        };

        let mut app = Self {
            system: None,
            mic: None,
            app_audio: None,
            audio_apps: Vec::new(),
            cameras: Vec::new(),
            camera: None,
            camera_texture: None,
            camera_seq: None,
            config,
            sources: Vec::new(),
            selected: None,
            system_devices: Vec::new(),
            mic_devices: Vec::new(),
            preview: None,
            rec: RecState::Idle,
            screenshots: Vec::new(),
            meters: [0.0; 3],
            ctx: cc.egui_ctx.clone(),
            last_drawn: Instant::now(),
            texture: None,
            texture_seq: None,
            picking_region: false,
            region_drag: None,
            status: None,
            notification: None,
            settings: None,
            storage_panel: StoragePanel::default(),
            cpu: CpuMeter { last: None, percent: 0.0 },
            system_meter: SystemMeter::default(),
            hud_meter: Arc::new(Mutex::new(SystemMeter::default())),
            actions,
            hotkeys,
            bound_actions,
            native_window: crate::platform::sys::window_handle(cc),
            tray: None,
            tray_commands: Arc::new(Mutex::new(Vec::new())),
            hidden: false,
            opening: None,
            closing: None,
            #[cfg(windows)]
            focus_armed: false,
            exit_requested: false,
            advanced_view: false,
            last_mic_error: None,
            last_system_error: None,
            last_app_error: None,
            pending_delete: None,
            pending_rename: None,
            image_viewer: None,
            #[cfg(windows)]
            player: None,
            #[cfg(windows)]
            player_error: None,
            #[cfg(windows)]
            pending_trim: None,
            #[cfg(windows)]
            trim_frames: Vec::new(),
            #[cfg(windows)]
            trim_loading: None,
            #[cfg(windows)]
            trim_requested: false,
            #[cfg(windows)]
            trim_error: None,
            gallery_deletions: Vec::new(),
            gallery_thumbnails: HashMap::new(),
            gallery_loading: HashMap::new(),
            disk_check: None,
            last_disk_check: None,
            low_disk_warned: false,
            #[cfg(windows)]
            desktop_available: crate::platform::sys::interactive_desktop_available(),
            #[cfg(windows)]
            desktop_checked: Instant::now(),
            #[cfg(windows)]
            desktop_resume: None,
            update: None,
            update_check: None,
            update_dialog: false,
            jobs: Vec::new(),
            named: HashSet::new(),
        };
        let commands = app.tray_commands.clone();
        #[cfg(all(windows, feature = "visual-test"))]
        if let Some(window) = app.native_window && std::env::var_os("OPNOVERLAY_TEST_DESKTOP").is_some() {
            crate::platform::sys::move_to_test_desktop(window).expect("cannot move test window to its virtual desktop");
        }
        match Tray::new(&cc.egui_ctx, crate::ICON_RGBA, 64, move |c| commands.lock().unwrap().push(c)) {
            Ok(tray) => app.tray = Some(tray),
            Err(e) => app.set_status(tr!("Icono de la bandeja no disponible: {}", "Tray icon unavailable: {}", e), None, true),
        }
        if let Some(window) = app.native_window {
            crate::platform::sys::square_window_corners(window);
        }
        app.apply_capture_visibility();
        app.refresh_devices();
        // The application first: desktop audio leaves it out.
        app.restart_app_audio();
        app.restart_input(InputKind::System);
        app.restart_input(InputKind::Microphone);
        app.refresh_sources();
        app.restart_preview(&cc.egui_ctx);
        app.restart_camera(&cc.egui_ctx);
        let menu_hotkey_ready = app.bind_hotkeys();
        if let Some(e) = hotkey_error {
            app.set_status(tr3!("Atajos globales no disponibles: {}", "Global hotkeys unavailable: {}", "Глобальные горячие клавиши недоступны: {}", e), None, true);
        }
        // Another obs-lite may still be writing that recording.
        if crate::config::first_instance() {
            app.recover_last_recording();
        }
        if app.config.setup_complete && app.config.auto_start_replay {
            app.start_replay(&cc.egui_ctx);
        }
        #[cfg(windows)]
        if !app.desktop_available && app.config.setup_complete && app.config.auto_start_replay && !app.is_replaying() {
            app.desktop_resume = Some((false, true, false));
        }
        if let Some(error) = startup_error {
            app.set_status(tr3!("No se pudo comprobar el inicio automático: {}", "Could not check startup: {}", "Не удалось проверить автозапуск: {}", error), None, true);
        }
        if app.tray.is_some() || menu_hotkey_ready {
            app.finish_hide_window(&cc.egui_ctx);
        }
        cc.egui_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        app
    }

    /// A recording obs-lite was writing when it stopped (a crash, a power cut) is still a
    /// fragmented MP4: make it a regular one. A finished recording is left as it is.
    fn recover_last_recording(&mut self) {
        if let Some(path) = self.config.last_recording.take() {
            if !self.config.pending_recoveries.contains(&path) { self.config.pending_recoveries.push(path); }
        }
        let paths = self.config.pending_recoveries.clone();
        for path in paths {
            match crate::mp4::repair(&path) {
                Ok(repaired) => {
                    self.config.pending_recoveries.retain(|item| item != &path);
                    if repaired {
                        let shown = crate::platform::sys::display_path(&path);
                        self.set_status(tr3!("Se recuperó la grabación interrumpida '{}'", "Recovered the interrupted recording '{}'", "Прерванная запись восстановлена: {}", shown.display()), Some(path), false);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.config.pending_recoveries.retain(|item| item != &path),
                Err(error) => {
                    let shown = crate::platform::sys::display_path(&path);
                    self.set_status(tr3!("No se pudo recuperar '{}': {}", "Could not recover '{}': {}", "Не удалось восстановить запись '{}': {}. Повторим при следующем запуске.", shown.display(), error), None, true);
                }
            }
        }
    }

    fn is_recording(&self) -> bool {
        matches!(self.rec, RecState::Active { recording: Some(_), .. })
    }

    fn is_replaying(&self) -> bool {
        matches!(self.rec, RecState::Active { replay: true, .. })
    }

    fn is_session_active(&self) -> bool {
        matches!(self.rec, RecState::Active { .. })
    }

    /// The capture session of a recording or the replay buffer.
    fn active_session(&self) -> Option<&Session> {
        match &self.rec {
            RecState::Active { session, .. } => Some(session),
            _ => None,
        }
    }

    fn is_paused(&self) -> bool {
        self.is_recording() && self.active_session().is_some_and(|s| s.shared.outputs.lock().unwrap().file_paused())
    }

    /// Recording and the encoder clock is running (pausing before that would be lost).
    fn is_live(&self) -> bool {
        self.is_recording() && self.active_session().is_some_and(|s| s.shared.encoder_ready.load(Ordering::Acquire))
    }

    fn refresh_sources(&mut self) {
        self.sources = capture::list_sources();
        let still_there = self.selected.is_some_and(|id| self.sources.iter().any(|s| s.id == id));
        if !still_there {
            let wanted = self.config.source_label.as_deref();
            self.selected = self
                .sources
                .iter()
                .find(|s| Some(s.label.as_str()) == wanted)
                .or_else(|| self.sources.first())
                .map(|s| s.id);
        }
    }

    fn refresh_devices(&mut self) {
        self.system_devices = audio::list_devices(InputKind::System);
        self.mic_devices = audio::list_devices(InputKind::Microphone);
        // A saved device that disappeared falls back to the Windows default.
        let keep = |id: &mut Option<String>, devices: &[Device]| {
            if id.as_ref().is_some_and(|id| !devices.iter().any(|d| &d.id == id)) {
                *id = None;
            }
        };
        keep(&mut self.config.system_device, &self.system_devices);
        keep(&mut self.config.mic_device, &self.mic_devices);
    }

    fn selected_source(&self) -> Option<&Source> {
        self.selected.and_then(|id| self.sources.iter().find(|s| s.id == id))
    }

    fn select_source(&mut self, ctx: &egui::Context, id: SourceId) {
        self.selected = Some(id);
        // A region belongs to the source it was drawn on.
        self.config.region = None;
        self.config.source_label = self.selected_source().map(|s| s.label.clone());
        self.restart_preview(ctx);
    }

    fn set_status(&mut self, text: impl Into<String>, file: Option<PathBuf>, error: bool) {
        let text = text.into();
        if !error && let Some(path) = file.as_ref() {
            self.remember_file(path);
            self.gallery_thumbnails.remove(path);
            self.gallery_loading.remove(path);
        }
        if error || file.is_some() {
            let message = if file.is_some() { text.split(':').next().unwrap_or(&text).to_owned() } else { text.clone() };
            self.notify(message, error);
        }
        self.status = Some(Status { text, file, error });
    }

    fn notify(&mut self, message: impl Into<String>, error: bool) {
        if self.config.notifications {
            self.notification = Some((Instant::now(), message.into(), error, Arc::new(OnceLock::new())));
            self.ctx.request_repaint();
        }
    }

    fn remember_file(&mut self, path: &Path) {
        if add_gallery_file(&mut self.config.gallery_files, path) {
            self.config.save();
        }
    }

    fn channel(&self, kind: InputKind) -> (bool, Option<String>, f32) {
        let c = &self.config;
        match kind {
            InputKind::System | InputKind::SystemExcept(_) => {
                (c.system_audio, c.system_device.clone(), channel_gain(c.system_volume, c.system_muted))
            }
            InputKind::Microphone => (c.mic, c.mic_device.clone(), channel_gain(c.mic_volume, c.mic_muted)),
            InputKind::Application(_) => (c.app_audio.is_some(), None, channel_gain(c.app_volume, c.app_muted)),
        }
    }

    /// Captures the configured application's audio, if that application is running now.
    fn start_app_audio(&mut self) {
        self.app_audio = None;
        let Some(exe) = &self.config.app_audio else { return };
        let Some(app) = capture::list_audio_apps().into_iter().find(|a| a.exe.eq_ignore_ascii_case(exe)) else { return };
        let (_, _, gain) = self.channel(InputKind::Application(app.pid));
        self.app_audio = Some(Input::start(InputKind::Application(app.pid), None, gain, Some(self.ctx.clone())));
    }

    /// Restarts the application's capture and, when that changes what desktop audio has to
    /// leave out, desktop audio too.
    fn restart_app_audio(&mut self) {
        self.start_app_audio();
        let wanted = system_kind(self.app_audio.as_ref().map(|a| a.kind), self.config.system_device.as_deref());
        if self.system.as_ref().is_some_and(|s| s.kind != wanted) {
            self.restart_input(InputKind::System);
        }
    }

    fn restart_input(&mut self, kind: InputKind) {
        let kind = match kind {
            InputKind::System | InputKind::SystemExcept(_) => {
                system_kind(self.app_audio.as_ref().map(|a| a.kind), self.config.system_device.as_deref())
            }
            other => other,
        };
        let (enabled, device, gain) = self.channel(kind);
        let slot = match kind {
            InputKind::System | InputKind::SystemExcept(_) => &mut self.system,
            InputKind::Microphone => &mut self.mic,
            InputKind::Application(_) => &mut self.app_audio,
        };
        *slot = None; // release the device before reopening it
        *slot = enabled.then(|| Input::start(kind, device, gain, Some(self.ctx.clone())));
        if kind == InputKind::Microphone
            && let Some(mic) = &self.mic
        {
            mic.shared.set_denoise(self.config.mic_denoise);
        }
        self.sync_mixer_inputs();
    }

    fn sync_mixer_inputs(&self) {
        if let RecState::Active { mixer: Some(mixer), .. } = &self.rec {
            let inputs = [&self.system, &self.mic, &self.app_audio]
                .into_iter().flatten().map(|input| input.shared.clone()).collect();
            mixer.set_inputs(inputs);
        }
    }

    fn toggle_desktop_audio(&mut self) {
        if !self.config.system_audio {
            self.config.system_audio = true;
            self.config.system_muted = false;
            self.restart_input(InputKind::System);
        } else {
            self.config.system_muted = !self.config.system_muted;
            if let Some(system) = &self.system {
                system.shared.set_gain(channel_gain(self.config.system_volume, self.config.system_muted));
            } else if !self.config.system_muted {
                self.restart_input(InputKind::System);
            }
        }
    }

    fn restart_camera(&mut self, ctx: &egui::Context) {
        (self.camera, self.camera_texture, self.camera_seq) = (None, None, None);
        if let Some(c) = &self.config.camera {
            self.camera = Some(Camera::start(c.device.clone(), Some(ctx.clone())));
        }
    }

    /// The camera overlay, when a camera is configured and running.
    fn overlay(&self) -> Option<(Corner, CameraSize)> {
        self.camera.as_ref().and(self.config.camera.as_ref()).map(|c| (c.corner, c.size))
    }

    fn restart_preview(&mut self, ctx: &egui::Context) {
        self.preview = None;
        self.texture_seq = None;
        if self.active_session().is_some() {
            return;
        }
        let Some(source) = self.selected else { return };
        let opts = SessionOptions {
            source,
            fps: PREVIEW_FPS,
            cursor: self.config.cursor,
            preview_fps: PREVIEW_FPS,
            record: None,
        };
        match Session::start(opts, Some(ctx.clone())) {
            Ok(s) => self.preview = Some(s),
            Err(e) => self.source_lost(tr3!("No se pudo capturar la fuente: {}", "Could not capture the source: {}", "Не удалось захватить изображение: {}", e)),
        }
    }

    /// The selected source cannot be captured (e.g. its window was closed): stop showing
    /// it as the source and drop its last frame from the preview.
    fn source_lost(&mut self, message: String) {
        self.preview = None;
        self.selected = None;
        self.texture = None;
        self.texture_seq = None;
        self.set_status(message, None, true);
    }

    fn handle(&mut self, ctx: &egui::Context, action: Action) {
        if !self.config.setup_complete && !matches!(action, Action::ToggleMenu) {
            let message = match self.config.language.unwrap_or_else(Language::system) {
                Language::Russian => "Сначала завершите начальную настройку.",
                Language::English => "Complete initial setup first.",
                Language::Spanish => "Completa la configuración inicial primero.",
            };
            self.set_status(message, None, true);
            self.show_window(ctx);
            return;
        }
        match action {
            Action::ToggleMenu => {
                if self.hidden || (self.opening.is_none() && ctx.input(|i| i.viewport().minimized == Some(true))) {
                    self.show_window(ctx);
                } else {
                    self.hide_window(ctx);
                }
            }
            Action::ToggleRecording => match self.rec {
                RecState::Saving(_) => {}
                RecState::Active { recording: Some(_), .. } => self.stop_recording(ctx),
                _ => self.start_recording(ctx),
            },
            Action::ToggleReplay => match self.rec {
                RecState::Saving(_) => {}
                RecState::Active { replay: true, .. } => self.stop_replay(ctx),
                _ => self.start_replay(ctx),
            },
            Action::SaveReplay => self.save_replay(ctx),
            Action::TogglePause => {
                if self.is_live()
                    && let Some(session) = self.active_session()
                {
                    let mut outputs = session.shared.outputs.lock().unwrap();
                    let paused = outputs.file_paused();
                    outputs.set_file_paused(!paused);
                }
            }
            Action::Screenshot => self.take_screenshot(ctx),
            Action::ToggleMicMute => {
                self.config.mic_muted = !self.config.mic_muted;
                if let Some(mic) = &self.mic {
                    mic.shared.set_gain(channel_gain(self.config.mic_volume, self.config.mic_muted));
                }
            }
        }
    }

    fn start_recording(&mut self, ctx: &egui::Context) {
        let stem = match video_name(&self.config.video_name_template, chrono::Local::now().naive_local()) {
            Ok(name) => name,
            Err(error) => return self.set_status(tr3!("Plantilla de nombre de vídeo no válida: {}", "Invalid video name template: {}", "Неверный шаблон имени видео: {}", error), None, true),
        };
        if let Some(problem) = Self::output_dir_problem(&self.config.output_dir) {
            return self.set_status(problem, None, true);
        }
        if let Err(e) = std::fs::create_dir_all(&self.config.output_dir) {
            return self.set_status(tr!("No se puede usar la ruta de grabación: {}", "Cannot use the recording path: {}", e), None, true);
        }
        if !self.start_session(ctx) {
            return;
        }
        let path = match reserve_recording_name(&self.config.output_dir, &stem, &mut self.named) {
            Ok(result) => result,
            Err(error) => {
                if !self.is_replaying() { self.stop_session(ctx, None); }
                return self.set_status(tr3!("No se pudo crear el archivo de grabación: {}", "Could not create the recording file: {}", "Не удалось создать файл записи: {}", error), None, true);
            }
        };
        let RecState::Active { session, recording, .. } = &mut self.rec else { return };
        session.shared.outputs.lock().unwrap().start_file(path.clone());
        *recording = Some(Recording);
        self.status = None;
        self.last_disk_check = None;
        self.low_disk_warned = false;
        // Saved now, so the next start can repair the file if obs-lite stops while writing it.
        self.config.last_recording = Some(path);
        self.config.save();
        self.notify(if i18n::russian() { "Запись начата" } else { tr!("Grabación iniciada", "Recording started") }, false);
    }

    fn start_replay(&mut self, ctx: &egui::Context) {
        if !self.start_session(ctx) {
            return;
        }
        let RecState::Active { session, replay, .. } = &mut self.rec else { return };
        let result = {
            let mut outputs = session.shared.outputs.lock().unwrap();
            match self.config.replay_storage {
                ReplayStorage::Memory => { outputs.set_replay(Some(self.config.replay_seconds)); Ok(()) }
                ReplayStorage::Disk => outputs.set_replay_on_disk(self.config.replay_seconds),
            }
        };
        if let Err(error) = result {
            if !self.is_recording() {
                self.stop_session(ctx, None);
            }
            self.set_status(tr3!("No se pudo activar la repetición en disco: {}", "Could not enable disk replay: {}", "Не удалось включить откаты на диске: {}", error), None, true);
            return;
        }
        *replay = true;
        self.last_disk_check = None;
    }

    /// Why files cannot go to the recordings folder, if they cannot.
    fn output_dir_problem(path: &Path) -> Option<String> {
        crate::platform::sys::volatile(path).then(|| {
            tr!(
                "La ruta de grabación no está compartida con obs-lite: lo que se guardara ahí se perdería al cerrarlo. Elige la carpeta con Examinar.",
                "The recording path is not shared with obs-lite: anything saved there would be lost when it closes. Choose the folder with Browse."
            )
            .to_owned()
        })
    }

    /// Saves the replay buffer as a new file, in the background.
    fn save_replay(&mut self, ctx: &egui::Context) {
        let Some(session) = self.active_session().filter(|_| self.is_replaying()) else {
            return self.set_status(tr!("Inicia el búfer de repetición para poder guardar repeticiones.", "Start the replay buffer to save replays."), None, true);
        };
        if let Some(problem) = Self::output_dir_problem(&self.config.output_dir) {
            return self.set_status(problem, None, true);
        }
        let replay = session.shared.outputs.lock().unwrap().replay();
        let replay = match replay {
            Ok(replay) => replay,
            Err(e) => return self.set_status(tr3!("No se pudo guardar la repetición: {}", "Could not save the replay: {}", "Не удалось сохранить откат: {}", e), None, true),
        };
        let path = new_output(&self.config.output_dir, Some("Replay"), "mp4", &mut self.named);
        self.spawn_job(ctx, move || match std::fs::create_dir_all(path.parent().unwrap_or(&path)).map_err(|e| e.to_string()).and_then(|()| replay.save_unique(path)) {
            Ok(path) => saved(tr3!("Repetición guardada: {}", "Replay saved: {}", "Откат сохранён: {}", crate::platform::sys::display_path(&path).display()), path),
            Err(e) => failed(tr3!("No se pudo guardar la repetición: {}", "Could not save the replay: {}", "Не удалось сохранить откат: {}", e)),
        });
    }

    /// Runs `job` on another thread; its status is shown when it ends, and exit waits for it.
    fn spawn_job(&mut self, ctx: &egui::Context, job: impl FnOnce() -> Status + Send + 'static) {
        let ctx = ctx.clone();
        self.jobs.push(thread::spawn(move || {
            let status = job();
            ctx.request_repaint();
            status
        }));
    }

    /// Starts capturing and encoding for a recording or the replay buffer, unless already
    /// doing so. Returns whether the session is running.
    fn start_session(&mut self, ctx: &egui::Context) -> bool {
        match self.rec {
            RecState::Active { .. } => return true,
            RecState::Saving(_) => return false,
            RecState::Idle => {}
        }
        let Some(source) = self.selected else {
            self.set_status(tr!("Añade una fuente con el botón + del panel Fuentes.", "Add a source with the + button in the Sources dock."), None, true);
            return false;
        };
        // One capture session at a time: the recording session also feeds the preview.
        self.preview = None;
        // A menu left open (the session started from a hotkey) would still change the sources.
        egui::Popup::close_all(ctx);

        // The application may have started after obs-lite, and a device that failed (unplugged)
        // may be back.
        if self.app_audio.is_none() || input_failed(&self.app_audio) {
            self.restart_app_audio();
        }
        if input_failed(&self.system) {
            self.restart_input(InputKind::System);
        }
        if input_failed(&self.mic) {
            self.restart_input(InputKind::Microphone);
        }
        let inputs: Vec<_> =
            [&self.system, &self.mic, &self.app_audio].into_iter().flatten().map(|i| i.shared.clone()).collect();
        let opts = SessionOptions {
            source,
            fps: self.config.fps,
            cursor: self.config.cursor,
            preview_fps: PREVIEW_FPS,
            record: Some(RecordConfig {
                quality: self.config.quality,
                custom_bitrate_mbps: self.config.custom_bitrate_mbps,
                resolution: self.config.resolution,
                audio: true,
                region: self.config.region,
                overlay: self.overlay(),
            }),
        };
        let session = match Session::start(opts, Some(ctx.clone())) {
            Ok(s) => s,
            Err(e) => {
                self.restart_preview(ctx);
                self.set_status(tr3!("No se pudo iniciar la grabación: {}", "Could not start recording: {}", "Не удалось начать запись: {}", e), None, true);
                return false;
            }
        };
        let mixer = Some({
            let shared = session.shared.clone();
            Mixer::start(inputs, shared.encoder_ready.clone(), shared.clock.clone(), move |pcm| {
                if let Some(enc) = shared.encoder.lock().unwrap().as_ref() {
                    enc.push_audio(pcm);
                }
            })
        });
        if let Some(camera) = &self.camera {
            let shared = session.shared.clone();
            *camera.shared.sink.lock().unwrap() = Some(Box::new(move |frame: &CameraFrame| {
                // Scale outside the encoder lock: audio and screen frames wait on it, and audio
                // arriving late made the encoder emit black frames.
                let size = (frame.width, frame.height);
                let Some(rect) = shared.encoder.lock().unwrap().as_ref().and_then(|e| e.overlay_rect(size)) else { return };
                let pixels = camera::resize(&frame.bgra, size, (rect.2, rect.3));
                if let Some(enc) = shared.encoder.lock().unwrap().as_ref() {
                    enc.push_overlay(rect, &pixels);
                }
            }));
        }
        self.rec = RecState::Active { session, mixer, recording: None, replay: false };
        true
    }

    fn stop_recording(&mut self, ctx: &egui::Context) {
        if !self.is_recording() { return; }
        self.notify(if i18n::russian() { "Запись остановлена · сохранение" } else { tr!("Grabación detenida · guardando", "Recording stopped · saving") }, false);
        let RecState::Active { session, recording, replay, .. } = &mut self.rec else { return };
        if !*replay {
            return self.stop_session(ctx, None);
        }
        *recording = None;
        // The replay buffer keeps the session going: only the file ends.
        let file = session.shared.outputs.lock().unwrap().take_file();
        self.set_status(tr!("Guardando grabación…", "Saving recording…"), None, false);
        self.spawn_job(ctx, move || match file.map(PendingFile::finish) {
            Some(Ok(path)) => saved(tr3!("Grabación guardada: {}", "Recording saved: {}", "Запись сохранена: {}", crate::platform::sys::display_path(&path).display()), path),
            Some(Err(e)) => failed(tr!("Error al guardar la grabación: {}", "Error saving the recording: {}", e)),
            None => failed(tr!("no se capturó ningún fotograma", "no frames were captured").into()),
        });
    }

    fn stop_replay(&mut self, ctx: &egui::Context) {
        let RecState::Active { session, recording, replay, .. } = &mut self.rec else { return };
        *replay = false;
        if recording.is_none() {
            return self.stop_session(ctx, None);
        }
        session.shared.outputs.lock().unwrap().set_replay(None);
    }

    /// Ends capture and encoding, writing out the recording if there is one.
    fn stop_session(&mut self, ctx: &egui::Context, reason: Option<String>) {
        let RecState::Active { session, mixer, recording, .. } = std::mem::replace(&mut self.rec, RecState::Idle) else {
            return;
        };
        drop(mixer);
        if let Some(camera) = &self.camera {
            *camera.shared.sink.lock().unwrap() = None;
        }
        let (encoder, stop_error) = session.stop();
        if recording.is_none() {
            // Only the replay buffer: nothing to write; let the encoder wind down elsewhere.
            if let Some(encoder) = encoder {
                thread::spawn(move || encoder.finish().map(PendingFile::finish));
            }
            self.restart_preview(ctx);
            if let Some(reason) = reason {
                self.set_status(reason, None, true);
            }
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx2 = ctx.clone();
        // Finalizing the MP4 can take a moment; keep the UI responsive.
        thread::spawn(move || {
            let result = match encoder.map(Encoder::finish) {
                Some(Some(file)) => file.finish(),
                _ => Err(stop_error.unwrap_or_else(|| tr!("no se capturó ningún fotograma", "no frames were captured").into())),
            };
            let _ = tx.send(result);
            ctx2.request_repaint();
        });
        self.rec = RecState::Saving(rx);
        self.restart_preview(ctx);
        self.set_status(reason.unwrap_or_else(|| tr!("Guardando grabación…", "Saving recording…").into()), None, false);
    }

    fn show_window(&mut self, ctx: &egui::Context) {
        let width = if self.advanced_view { 740.0 } else { 420.0 };
        let (from, to) = self.closing.take().map(|closing| (closing.position(), closing.rest)).unwrap_or((pos2(-width, 0.0), pos2(0.0, 0.0)));
        self.hidden = false;
        #[cfg(windows)]
        { self.focus_armed = false; }
        self.opening = Some(OpeningAnimation { last: Instant::now(), progress: 0.0, from, to, rest: to, duration: 0.14 });
        #[cfg(windows)]
        if let Some(window) = self.native_window {
            crate::platform::sys::prepare_slide_in(window);
        }
        self.set_taskbar_button(false);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(from));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn hide_window(&mut self, ctx: &egui::Context) {
        if self.hidden { return; }
        if self.settings.take().is_some() { self.bind_hotkeys(); }
        let rect = ctx.input(|i| i.viewport().outer_rect);
        let opening = self.opening.take();
        let from = opening.as_ref().map(OpeningAnimation::position).or_else(|| rect.map(|r| r.min)).unwrap_or(pos2(0.0, 0.0));
        let rest = opening.as_ref().map(|opening| opening.to).unwrap_or(from);
        let width = rect.map(|r| r.width()).filter(|width| *width > 1.0).unwrap_or(if self.advanced_view { 740.0 } else { 420.0 });
        self.closing = Some(OpeningAnimation { last: Instant::now(), progress: 0.0, from, to: pos2(-width, from.y), rest, duration: 0.12 });
        self.hidden = true;
        #[cfg(windows)]
        { self.focus_armed = false; }
        ctx.request_repaint();
    }

    fn finish_hide_window(&mut self, ctx: &egui::Context) {
        self.image_viewer = None;
        #[cfg(windows)]
        { self.player = None; self.player_error = None; self.pending_trim = None; self.trim_frames.clear(); self.trim_loading = None; self.trim_requested = false; self.trim_error = None; }
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        self.set_taskbar_button(false);
        self.hidden = true;
        self.opening = None;
        self.closing = None;
        #[cfg(windows)]
        { self.focus_armed = false; }
        ctx.request_repaint_after(Duration::from_millis(20));
    }

    fn advance_window_animation(&mut self, ctx: &egui::Context, now: Instant) {
        if let Some(opening) = &mut self.opening {
            let progress = opening.advance(now);
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(opening.position()));
            if progress < 1.0 { ctx.request_repaint_after(Duration::from_millis(16)); }
            else { self.opening = None; }
        }
        if let Some(closing) = &mut self.closing {
            let progress = closing.advance(now);
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(closing.position()));
            if progress < 1.0 { ctx.request_repaint_after(Duration::from_millis(16)); }
            else { self.finish_hide_window(ctx); }
        }
    }

    #[cfg(windows)]
    fn handle_focus_change(&mut self, ctx: &egui::Context, focused: Option<bool>, external_foreground: bool) {
        if self.hidden { return; }
        if focused == Some(true) { self.focus_armed = true; }
        if self.focus_armed && self.opening.is_none() && focused == Some(false) && external_foreground {
            self.hide_window(ctx);
        }
    }

    fn escape_navigation(&mut self, ctx: &egui::Context) {
        if self.image_viewer.take().is_some() { return; }
        #[cfg(windows)]
        if self.player.take().is_some() { return; }
        if self.advanced_view {
            self.advanced_view = false;
            return;
        }
        self.hide_window(ctx);
    }

    fn quit(&mut self, ctx: &egui::Context) {
        self.exit_requested = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn update_gallery_thumbnails(&mut self, ctx: &egui::Context, files: &[PathBuf]) {
        self.gallery_thumbnails.retain(|path, _| files.contains(path));
        for (path, receiver) in std::mem::take(&mut self.gallery_loading) {
            match receiver.try_recv() {
                Ok(image) => {
                    let texture = image.map(|image| ctx.load_texture(path.to_string_lossy(), image, TextureOptions::LINEAR));
                    if files.contains(&path) { self.gallery_thumbnails.insert(path, texture); }
                }
                Err(mpsc::TryRecvError::Empty) => { self.gallery_loading.insert(path, receiver); }
                Err(mpsc::TryRecvError::Disconnected) => { self.gallery_thumbnails.insert(path, None); }
            }
        }
        for path in files {
            if self.gallery_loading.len() >= 2 { break; }
            if self.gallery_thumbnails.contains_key(path) || self.gallery_loading.contains_key(path) { continue; }
            let (tx, rx) = mpsc::channel();
            let path = path.clone();
            let key = path.clone();
            let ctx = ctx.clone();
            thread::spawn(move || {
                #[cfg(windows)]
                let image = crate::platform::thumbnail::load(&path);
                #[cfg(not(windows))]
                let image = None;
                let _ = tx.send(image);
                ctx.request_repaint();
            });
            self.gallery_loading.insert(key, rx);
        }
    }

    fn gallery_card(&mut self, ui: &mut egui::Ui, path: &Path, side: f32) {
        let language = self.config.language.unwrap_or_else(Language::system);
        let text = |ru, en, es| match language { Language::Russian => ru, Language::English => en, Language::Spanish => es };
        let (rect, response) = ui.allocate_exact_size(vec2(side, side), Sense::click());
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, response.enabled(), format!("{}: {file_name}", text("Открыть файл", "Open file", "Abrir archivo"))));
        let video = path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"));
        let replay = video && path.file_stem().is_some_and(|stem| stem.to_string_lossy().to_lowercase().contains(" — replay"));
        let label = if replay { text("ОТКАТ", "REPLAY", "REPETICIÓN") } else if video { text("ВИДЕО", "VIDEO", "VÍDEO") } else { text("СНИМОК", "IMAGE", "IMAGEN") };
        let painter = ui.painter();
        painter.rect_filled(rect, 5.0, GREY6);
        if let Some(Some(texture)) = self.gallery_thumbnails.get(path) {
            painter.image(texture.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            let font = FontId::proportional(10.0);
            let galley = painter.layout_no_wrap(label.to_owned(), font.clone(), Color32::WHITE);
            let badge = Rect::from_min_size(rect.min + vec2(5.0, 5.0), vec2(galley.size().x + 12.0, 18.0));
            painter.rect_filled(badge, 4.0, Color32::from_black_alpha(200));
            painter.text(badge.center(), Align2::CENTER_CENTER, label, font, Color32::WHITE);
        } else {
            painter.text(rect.center(), Align2::CENTER_CENTER, label, FontId::proportional(11.0), TEXT_MUTED);
        }
        let mut action_clicked = false;
        let focus_id = ui.make_persistent_id(("gallery-actions-focus", path));
        let action_focused = ui.data(|data| data.get_temp::<bool>(focus_id).unwrap_or(false));
        let mut keep_actions = response.has_focus();
        let highlighted = ui.rect_contains_pointer(rect) || response.has_focus() || action_focused;
        let hover = ui.ctx().animate_bool_with_time(ui.make_persistent_id(("gallery-hover", path)), highlighted, 0.16);
        if hover > 0.0 {
            ui.painter().rect_filled(rect, 5.0, Color32::from_white_alpha((hover * 14.0).round() as u8));
        }
        if highlighted || hover > 0.0 {
            let mut overlay = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            overlay.set_opacity(hover);
            if !highlighted { overlay.disable(); }
            let button_side = 26.0;
            let delete_rect = Rect::from_min_size(pos2(rect.right() - button_side - 5.0, rect.bottom() - button_side - 5.0), vec2(button_side, button_side));
            let delete = overlay.put(delete_rect, Button::new("").fill(GREY6)).on_hover_text(text("Удалить в корзину", "Move to recycle bin", "Mover a la papelera"));
            delete.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, delete.enabled(), format!("{}: {file_name}", text("Удалить в корзину", "Move to recycle bin", "Mover a la papelera"))));
            keep_actions |= delete.has_focus();
            let center = delete_rect.center();
            let stroke = Stroke::new(1.3, Color32::WHITE);
            let painter = overlay.painter();
            painter.line_segment([center + vec2(-5.0, -4.0), center + vec2(5.0, -4.0)], stroke);
            painter.line_segment([center + vec2(-3.5, -2.5), center + vec2(-2.5, 5.0)], stroke);
            painter.line_segment([center + vec2(3.5, -2.5), center + vec2(2.5, 5.0)], stroke);
            painter.line_segment([center + vec2(-2.5, 5.0), center + vec2(2.5, 5.0)], stroke);
            if delete.clicked() {
                self.pending_delete = Some(path.to_owned());
                action_clicked = true;
            }
            #[cfg(windows)]
            if video {
                let trim_rect = delete_rect.translate(vec2(-button_side - 4.0, 0.0));
                let trim = overlay.put(trim_rect, Button::new("").fill(GREY6)).on_hover_text(text("Обрезать видео", "Trim video", "Recortar vídeo"));
                trim.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, trim.enabled(), format!("{}: {file_name}", text("Обрезать видео", "Trim video", "Recortar vídeo"))));
                keep_actions |= trim.has_focus();
                let center = trim_rect.center();
                let painter = overlay.painter();
                painter.circle_stroke(center + vec2(-3.5, -3.5), 2.0, stroke);
                painter.circle_stroke(center + vec2(-3.5, 3.5), 2.0, stroke);
                painter.line_segment([center + vec2(-1.5, -2.0), center + vec2(5.0, 4.0)], stroke);
                painter.line_segment([center + vec2(-1.5, 2.0), center + vec2(5.0, -4.0)], stroke);
                if trim.clicked() {
                    self.pending_trim = Some((path.to_owned(), "00:00.000".into(), "auto".into()));
                    action_clicked = true;
                }
            }
        }
        ui.data_mut(|data| data.insert_temp(focus_id, keep_actions));
        if response.clicked() && !action_clicked {
            #[cfg(windows)]
            if video {
                match self.native_window.ok_or_else(|| tr3!("La ventana del panel no está disponible.", "Overlay window is unavailable.", "Окно оверлея недоступно.").to_owned()).and_then(|parent| crate::platform::player::Player::open(parent, path)) {
                    Ok(player) => { self.player_error = None; self.player = Some((path.to_owned(), player, 1.0)); }
                    Err(error) => self.set_status(tr3!("No se pudo abrir el vídeo: {}", "Could not open the video: {}", "Не удалось открыть видео: {}", error), None, true),
                }
            } else {
                self.open_image(path, ui.ctx());
            }
            #[cfg(not(windows))]
            match crate::platform::sys::open_file(path) {
                Ok(()) => self.hide_window(ui.ctx()),
                Err(error) => self.set_status(error, None, true),
            }
        }
        response.context_menu(|ui| {
            #[cfg(windows)]
            if video && ui.button(text("Обрезать видео", "Trim video", "Recortar vídeo")).clicked() {
                self.pending_trim = Some((path.to_owned(), "00:00.000".into(), "auto".into()));
                ui.close();
            }
            #[cfg(windows)]
            if video && crate::platform::edit::has_original(path) && ui.button(text("Вернуть оригинал", "Restore original", "Restaurar original")).clicked() {
                self.restore_original(path.to_owned(), ui.ctx());
                ui.close();
            }
            if ui.button(text("Показать в проводнике", "Show in folder", "Mostrar en carpeta")).clicked() {
                crate::platform::sys::show_in_folder(path);
                self.hide_window(ui.ctx());
                ui.close();
            }
            if ui.button(text("Переименовать", "Rename", "Renombrar")).clicked() {
                self.pending_rename = Some((path.to_owned(), path.file_stem().unwrap_or_default().to_string_lossy().into_owned()));
                ui.close();
            }
            if ui.button(text("Удалить в корзину", "Move to recycle bin", "Mover a la papelera")).clicked() {
                self.pending_delete = Some(path.to_owned());
                ui.close();
            }
        });
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        ui.add_sized([side, 20.0], egui::Label::new(RichText::new(name.as_ref()).size(12.0)).truncate()).on_hover_text(name);
    }

    fn open_image(&mut self, path: &Path, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let source = path.to_owned();
        let repaint = ctx.clone();
        thread::spawn(move || {
            let result = load_gallery_image(&source);
            let _ = tx.send(result);
            repaint.request_repaint();
        });
        self.image_viewer = Some(ImageViewer { path: path.to_owned(), loading: Some(rx), texture: None, error: None });
    }

    fn image_view(&mut self, ui: &mut egui::Ui) {
        let language = self.config.language.unwrap_or_else(Language::system);
        let text = |ru, en, es| match language { Language::Russian => ru, Language::English => en, Language::Spanish => es };
        let Some(viewer) = &mut self.image_viewer else { return };
        if let Some(rx) = &viewer.loading {
            match rx.try_recv() {
                Ok(Ok(image)) => { viewer.texture = Some(ui.ctx().load_texture("gallery-image", image, TextureOptions::LINEAR)); viewer.loading = None; }
                Ok(Err(error)) => { viewer.error = Some(error); viewer.loading = None; }
                Err(mpsc::TryRecvError::Disconnected) => { viewer.error = Some(tr3!("No se pudo cargar la imagen.", "Could not load the image.", "Не удалось загрузить изображение.").into()); viewer.loading = None; }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let mut close = false;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(GREY8).inner_margin(18)).show(ui, |ui| {
            close = viewer_header(ui, &viewer.path, text("СКРИНШОТ", "SCREENSHOT", "CAPTURA"), language);
            ui.add_space(12.0);
            if let Some(texture) = &viewer.texture {
                let available = ui.available_size();
                let size = texture.size_vec2();
                let scale = (available.x / size.x).min(available.y / size.y).max(0.0);
                ui.centered_and_justified(|ui| { ui.add(egui::Image::new(texture).fit_to_exact_size(size * scale)); });
            } else if let Some(error) = &viewer.error {
                ui.colored_label(RED, tr3!("No se pudo abrir la captura: {}", "Could not open the screenshot: {}", "Не удалось открыть скриншот: {}", error));
            } else {
                ui.label(text("Загрузка скриншота…", "Loading screenshot…", "Cargando captura…"));
            }
        });
        if close { self.image_viewer = None; }
    }

    #[cfg(windows)]
    fn player_view(&mut self, ui: &mut egui::Ui) {
        if self.pending_trim.is_some() {
            self.trim_view(ui);
            return;
        }
        let language = self.config.language.unwrap_or_else(Language::system);
        let text = |ru, en, es| match language { Language::Russian => ru, Language::English => en, Language::Spanish => es };
        let current_error = self.player_error.clone();
        let Some((path, player, volume)) = &mut self.player else { return };
        let mut close = false;
        let mut show_folder = false;
        let mut trim_end = None;
        let mut restore = false;
        let mut error = None;
        let mut clear_error = false;
        let file_path = path.clone();
        let can_trim = self.config.gallery_files.contains(path);
        let can_restore = can_trim && crate::platform::edit::has_original(path);
        let file_size = std::fs::metadata(path.as_path()).ok().map(|metadata| metadata.len());
        let file_size = file_size.map_or_else(|| text("Размер неизвестен", "Unknown size", "Tamaño desconocido").to_owned(), |size| format!("{:.1} {}", size as f64 / 1_048_576.0, text("МБ", "MB", "MB")));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(GREY8).inner_margin(18)).show(ui, |ui| {
            let content_height = ui.available_height() - if can_restore { 30.0 } else { 0.0 };
            let column_width = ui.available_width().min(((content_height - 340.0).max(100.0) * 16.0 / 9.0).max(320.0));
            let gutter = ((ui.available_width() - column_width) / 2.0).max(0.0);
            ui.horizontal(|ui| {
                ui.add_space(gutter);
                ui.vertical(|ui| {
                    ui.set_width(column_width);
                    close = viewer_header(ui, path, text("ВИДЕО", "VIDEO", "VÍDEO"), language);
                    ui.add_space(12.0);
                    let width = ui.available_width();
                    let height = (width * 9.0 / 16.0).min((content_height - 337.0).max(100.0));
                    let spare = (content_height - height - 337.0).max(0.0);
                    ui.add_space(spare * 0.45);
                    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
                    ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
                    let scale = ui.ctx().pixels_per_point();
                    player.set_bounds(((rect.left() * scale).round() as i32, (rect.top() * scale).round() as i32, (rect.width() * scale).round() as i32, (rect.height() * scale).round() as i32));
                    ui.add_space(12.0);
                    let duration = player.duration().unwrap_or(0.0);
                    let mut position = player.position().unwrap_or(0.0).clamp(0.0, duration.max(0.0));
                    egui::Frame::new().fill(GREY7).corner_radius(8).inner_margin(14).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().slider_rail_height = 4.0;
                        ui.visuals_mut().handle_shape = egui::style::HandleShape::Circle;
                        ui.visuals_mut().selection.bg_fill = PRIMARY_LIGHT;
                        ui.visuals_mut().widgets.inactive.bg_fill = GREY4;
                        ui.visuals_mut().widgets.inactive.fg_stroke.color = PRIMARY_LIGHT;
                        ui.visuals_mut().widgets.hovered.fg_stroke.color = Color32::WHITE;
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(text("ВОСПРОИЗВЕДЕНИЕ", "PLAYBACK", "REPRODUCCIÓN")).size(11.0).strong().color(TEXT_MUTED));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.label(RichText::new(format!("{} / {}", player_time(position), player_time(duration))).size(14.0).color(TEXT_MUTED));
                            });
                        });
                        ui.add_space(4.0);
                        ui.spacing_mut().slider_width = (ui.available_width() - 16.0).max(40.0);
                        if ui.add_enabled(duration > 0.0, egui::Slider::new(&mut position, 0.0..=duration.max(0.01)).show_value(false).trailing_fill(true)).accessible_name(text("Положение видео", "Video position", "Posición del vídeo")).changed()
                            && let Err(problem) = player.seek(position) { error = Some(problem); }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            let label = if player.playing() { text("Пауза", "Pause", "Pausa") } else { text("Воспроизвести", "Play", "Reproducir") };
                            let toggle = ui.add_sized([140.0, 38.0], Button::new(RichText::new(label).size(14.0).strong()).fill(PRIMARY)).on_hover_text(text("Пробел — пауза / воспроизведение", "Space — pause / play", "Espacio — pausa / reproducir"));
                            let keyboard_toggle = ui.input(|input| input.key_pressed(egui::Key::Space)) && !ui.ctx().egui_wants_keyboard_input();
                            if (toggle.clicked() || keyboard_toggle) && let Err(problem) = player.toggle() { error = Some(problem); }
                            ui.add_space(10.0);
                            let volume_label = ui.label(RichText::new(text("Звук", "Volume", "Volumen")).size(14.0));
                            ui.spacing_mut().slider_width = (ui.available_width() - 55.0).clamp(30.0, 180.0);
                            if ui.add(egui::Slider::new(volume, 0.0..=1.0).show_value(false).trailing_fill(true)).labelled_by(volume_label.id).changed()
                                && let Err(problem) = player.set_volume(*volume) { error = Some(problem); }
                            ui.label(RichText::new(format!("{:.0}%", *volume * 100.0)).size(13.0).color(TEXT_MUTED));
                        });
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label(text("Скорость", "Speed", "Velocidad"));
                            let current = player.rate().unwrap_or(1.0);
                            for rate in [0.5_f32, 0.75, 1.0, 1.5, 2.0] {
                                let selected = (current - rate).abs() < 0.01;
                                if ui.selectable_label(selected, RichText::new(format!("{rate}×")).size(13.0).color(if selected { GREY8 } else { PRIMARY_LIGHT })).clicked() {
                                    match player.set_rate(rate) { Ok(()) => clear_error = true, Err(problem) => error = Some(problem) }
                                }
                            }
                        });
                    });
                    if let Some(error) = &current_error { ui.colored_label(RED, error); }
                    ui.add_space(16.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(text("ФАЙЛ", "FILE", "ARCHIVO")).size(12.0).strong().color(TEXT_MUTED));
                        ui.label(RichText::new(format!("MP4  ·  {file_size}")).size(13.0).color(TEXT_MUTED));
                    });
                    if can_restore && ui.button(text("Вернуть оригинал", "Restore original", "Restaurar original")).clicked() { restore = true; }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add_sized([160.0, 38.0], Button::new(RichText::new(text("Показать в папке", "Show in folder", "Mostrar en carpeta")).size(14.0))).clicked() { show_folder = true; }
                        if ui.add_enabled(can_trim, Button::new(RichText::new(text("Обрезать видео", "Trim video", "Recortar vídeo")).size(14.0)).min_size(vec2(150.0, 38.0))).clicked() {
                            trim_end = Some(if duration > 0.0 { trim_time((duration * 1000.0).ceil().clamp(1.0, 86_400_000.0) as u64) } else { "auto".into() });
                        }
                    });
                });
            });
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        });
        if restore {
            self.player = None;
            self.restore_original(file_path.clone(), ui.ctx());
        } else if let Some(end) = trim_end {
            self.pending_trim = Some((file_path.clone(), "00:00.000".into(), end));
            self.trim_frames.clear();
            self.trim_loading = None;
            self.trim_requested = false;
            self.trim_error = None;
        } else if show_folder {
            crate::platform::sys::show_in_folder(&file_path);
            self.hide_window(ui.ctx());
        } else if close {
            self.player = None;
        }
        if clear_error { self.player_error = None; }
        if let Some(error) = error { self.player_error = Some(tr3!("Error del reproductor: {}", "Player error: {}", "Ошибка проигрывателя: {}", error)); }
    }

    #[cfg(windows)]
    fn trim_view(&mut self, ui: &mut egui::Ui) {
        let language = self.config.language.unwrap_or_else(Language::system);
        let text = |ru, en, es| match language { Language::Russian => ru, Language::English => en, Language::Spanish => es };
        let current_error = self.trim_error.clone();
        let Some((path, player, volume)) = self.player.as_mut() else { return };
        let Some((_, start_text, end_text)) = self.pending_trim.as_mut() else { return };
        let current_path = path.clone();
        let can_restore = crate::platform::edit::has_original(path);
        let duration = player.duration().unwrap_or(0.0);
        if duration > 0.0 && !self.trim_requested {
            if end_text == "auto" { *end_text = trim_time((duration * 1000.0).ceil().clamp(1.0, 86_400_000.0) as u64); }
            let (tx, rx) = mpsc::channel();
            let file = path.clone();
            let ctx = ui.ctx().clone();
            thread::spawn(move || {
                let result = crate::platform::thumbnail::video_frames(&file, duration, 10);
                let _ = tx.send(result);
                ctx.request_repaint();
            });
            self.trim_loading = Some(rx);
            self.trim_requested = true;
        }
        if let Some(receiver) = &self.trim_loading {
            match receiver.try_recv() {
                Ok(Ok(images)) => {
                    self.trim_frames = images.into_iter().enumerate().map(|(i, image)| ui.ctx().load_texture(format!("trim-frame-{i}"), image, egui::TextureOptions::LINEAR)).collect();
                    self.trim_loading = None;
                }
                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => { self.trim_loading = None; }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let mut close = false;
        let mut save = false;
        let mut restore = false;
        let mut error = None;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(GREY8).inner_margin(18)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new(text("Обрезка видео", "Trim video", "Recortar vídeo")).size(22.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(text("Закрыть", "Close", "Cerrar")).clicked() { close = true; }
                });
            });
            ui.label(path.file_name().unwrap_or_default().to_string_lossy());
            ui.add_space(10.0);
            let available = ui.available_size();
            let height = (available.x * 9.0 / 16.0).min((available.y - 235.0).max(80.0));
            let (preview, _) = ui.allocate_exact_size(vec2(available.x, height), Sense::hover());
            ui.painter().rect_filled(preview, 4.0, Color32::BLACK);
            let scale = ui.ctx().pixels_per_point();
            player.set_bounds(((preview.left() * scale).round() as i32, (preview.top() * scale).round() as i32, (preview.width() * scale).round() as i32, (preview.height() * scale).round() as i32));
            ui.add_space(8.0);
            let mut position = player.position().unwrap_or(0.0).clamp(0.0, duration.max(0.0));
            ui.horizontal(|ui| {
                let label = if player.playing() { text("Пауза", "Pause", "Pausa") } else { text("Воспроизвести", "Play", "Reproducir") };
                if ui.add_sized([110.0, 36.0], Button::new(label)).clicked() && let Err(problem) = player.toggle() { error = Some(problem); }
                ui.label(format!("{} / {}", player_time(position), player_time(duration)));
                ui.spacing_mut().slider_width = (ui.available_width() - 145.0).max(40.0);
                if ui.add_enabled(duration > 0.0, egui::Slider::new(&mut position, 0.0..=duration.max(0.01)).show_value(false)).accessible_name(text("Положение видео", "Video position", "Posición del vídeo")).changed()
                    && let Err(problem) = player.seek(position) { error = Some(problem); }
                ui.spacing_mut().slider_width = 90.0;
                if ui.add(egui::Slider::new(volume, 0.0..=1.0).show_value(false).fixed_decimals(2)).accessible_name(text("Звук", "Volume", "Volumen")).changed()
                    && let Err(problem) = player.set_volume(*volume) { error = Some(problem); }
            });
            ui.add_space(8.0);
            let max_ms = (duration * 1000.0).ceil().clamp(1.0, 86_400_000.0) as u64;
            let mut start = parse_trim_time(start_text).unwrap_or(0).min(max_ms.saturating_sub(1));
            let mut end = parse_trim_time(end_text).unwrap_or(max_ms).clamp(start + 1, max_ms);
            ui.horizontal(|ui| {
                ui.label(player_time(0.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| { ui.label(player_time(duration)); });
            });
            let (timeline, _) = ui.allocate_exact_size(vec2(ui.available_width(), 58.0), Sense::hover());
            ui.painter().rect_filled(timeline, 4.0, GREY7);
            if !self.trim_frames.is_empty() {
                let frame_width = timeline.width() / self.trim_frames.len() as f32;
                for (index, texture) in self.trim_frames.iter().enumerate() {
                    let rect = Rect::from_min_max(pos2(timeline.left() + index as f32 * frame_width, timeline.top()), pos2(timeline.left() + (index + 1) as f32 * frame_width, timeline.bottom()));
                    ui.painter().image(texture.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                }
            } else {
                let label = if self.trim_requested && self.trim_loading.is_none() { text("Не удалось загрузить кадры", "Could not load frames", "No se pudieron cargar los fotogramas") } else { text("Загрузка кадров…", "Loading frames…", "Cargando fotogramas…") };
                ui.painter().text(timeline.center(), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(13.0), TEXT_MUTED);
            }
            let x_for = |ms: u64| timeline.left() + timeline.width() * (ms as f64 / max_ms as f64) as f32;
            let start_x = x_for(start);
            let end_x = x_for(end);
            ui.painter().rect_filled(Rect::from_min_max(timeline.min, pos2(start_x, timeline.bottom())), 0.0, Color32::from_black_alpha(170));
            ui.painter().rect_filled(Rect::from_min_max(pos2(end_x, timeline.top()), timeline.max), 0.0, Color32::from_black_alpha(170));
            ui.painter().rect_stroke(Rect::from_min_max(pos2(start_x, timeline.top()), pos2(end_x, timeline.bottom())), 3.0, Stroke::new(2.0, PRIMARY_LIGHT), egui::StrokeKind::Inside);
            for (id, x, is_start) in [("trim-start", start_x, true), ("trim-end", end_x, false)] {
                let handle = Rect::from_center_size(pos2(x, timeline.center().y), vec2(20.0, timeline.height()));
                ui.painter().rect_filled(handle, 3.0, PRIMARY_LIGHT);
                for offset in [-3.0, 3.0] {
                    ui.painter().line_segment([pos2(x + offset, timeline.center().y - 7.0), pos2(x + offset, timeline.center().y + 7.0)], Stroke::new(1.5, GREY8));
                }
                let response = ui.interact(handle, ui.scope_id().with(id), Sense::drag());
                if response.dragged() && let Some(pointer) = response.interact_pointer_pos() {
                    let value = ((((pointer.x - timeline.left()) / timeline.width()).clamp(0.0, 1.0) as f64) * max_ms as f64).round() as u64;
                    if is_start { start = value.min(end.saturating_sub(1)); *start_text = trim_time(start); }
                    else { end = value.clamp(start + 1, max_ms); *end_text = trim_time(end); }
                    if let Err(problem) = player.seek((if is_start { start } else { end }) as f64 / 1000.0) { error = Some(problem); }
                }
            }
            let play_x = timeline.left() + timeline.width() * (position / duration.max(0.01)).clamp(0.0, 1.0) as f32;
            ui.painter().line_segment([pos2(play_x, timeline.top()), pos2(play_x, timeline.bottom())], Stroke::new(2.0, Color32::WHITE));
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                let start_label = ui.label(text("Начало", "Start", "Inicio"));
                if ui.add(egui::TextEdit::singleline(start_text).desired_width(100.0)).labelled_by(start_label.id).changed()
                    && let Some(ms) = parse_trim_time(start_text) && ms as f64 <= duration * 1000.0
                    && let Err(problem) = player.seek(ms as f64 / 1000.0) { error = Some(problem); }
                let end_label = ui.label(text("Конец", "End", "Fin"));
                if ui.add(egui::TextEdit::singleline(end_text).desired_width(100.0)).labelled_by(end_label.id).changed()
                    && let Some(ms) = parse_trim_time(end_text) && ms as f64 <= duration * 1000.0
                    && let Err(problem) = player.seek(ms as f64 / 1000.0) { error = Some(problem); }
                ui.label(format!("{} {}", text("Длительность", "Duration", "Duración"), trim_time(end - start)));
            });
            ui.label(RichText::new(text("Время — минуты:секунды.миллисекунды. Оригинал для восстановления хранится в скрытой папке .opnoverlay-originals и занимает место.", "Time: minutes:seconds.milliseconds. The original is kept in the hidden .opnoverlay-originals folder and uses disk space.", "Tiempo: minutos:segundos.milisegundos. El original se guarda en la carpeta oculta .opnoverlay-originals y ocupa espacio.")).size(12.0).color(TEXT_MUTED));
            if let Some(error) = &current_error { ui.colored_label(RED, error); }
            ui.horizontal_wrapped(|ui| {
                if ui.add_sized([120.0, 36.0], Button::new(text("Отмена", "Cancel", "Cancelar"))).clicked() { close = true; }
                if ui.add_enabled(duration > 0.0, Button::new(text("Сохранить", "Save", "Guardar")).min_size(vec2(160.0, 36.0)).fill(PRIMARY)).clicked() { save = true; }
                if can_restore && ui.add_sized([180.0, 36.0], Button::new(text("Вернуть оригинал", "Restore original", "Restaurar original"))).clicked() { restore = true; }
            });
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        });
        if restore {
            self.pending_trim = None;
            self.player = None;
            self.trim_loading = None;
            self.trim_frames.clear();
            self.trim_requested = false;
            self.trim_error = None;
            self.restore_original(current_path, ui.ctx());
        } else if save { self.save_trim(ui.ctx()); }
        if close || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.pending_trim = None;
            self.player = None;
            self.trim_loading = None;
            self.trim_frames.clear();
            self.trim_requested = false;
            self.trim_error = None;
        }
        if let Some(error) = error { self.trim_error = Some(tr3!("Error del reproductor: {}", "Player error: {}", "Ошибка проигрывателя: {}", error)); }
    }

    fn overlay_home(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let language = self.config.language.unwrap_or_else(Language::system);
        let text = |ru, en, es| match language { Language::Russian => ru, Language::English => en, Language::Spanish => es };
        self.system_meter.update();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(GREY8).inner_margin(18))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("opnoverlay").size(21.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button(text("Закрыть  Esc", "Close  Esc", "Cerrar  Esc")).clicked() { self.hide_window(&ctx); }
                        if ui.button(text("Настройки", "Settings", "Ajustes")).clicked() { self.open_settings(); }
                    });
                });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(12.0);
                let scroll_size = vec2(ui.available_width(), (ui.available_height() - 76.0).max(1.0));
                ui.allocate_ui_with_layout(scroll_size, Layout::top_down(Align::Min), |ui| {
                    ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        let width = ui.available_width();
                        let replay_active = self.is_replaying();
                        let saving = matches!(self.rec, RecState::Saving(_));
                        egui::Frame::new()
                            .fill(GREY7)
                            .corner_radius(6)
                            .inner_margin(12)
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    let state = if replay_active { text("Откаты включены", "Replay enabled", "Repetición activada") } else { text("Откаты выключены", "Replay disabled", "Repetición desactivada") };
                                    ui.label(RichText::new(state).size(14.0).strong().color(if replay_active { PRIMARY_LIGHT } else { TEXT_MUTED }));
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        let seconds = self.config.replay_seconds;
                                        ui.label(RichText::new(format!("{:02}:{:02}", seconds / 60, seconds % 60)).size(13.0).color(TEXT_MUTED));
                                    });
                                });
                                ui.add_space(8.0);
                                let replay_width = ui.available_width();
                                let save_replay = format!("{}   {}", text("Сохранить откат", "Save replay", "Guardar repetición"), hotkey_hint(&self.config.hotkeys.save_replay));
                                if ui.add_enabled_ui(!saving, |ui| ui.add_sized([replay_width, 42.0], Button::new(RichText::new(save_replay).size(14.0)))).inner.clicked() {
                                    self.handle(&ctx, Action::SaveReplay);
                                }
                                ui.add_space(4.0);
                                let replay_toggle = if replay_active { text("Выключить откаты", "Disable replay", "Desactivar repetición") } else { text("Включить откаты", "Enable replay", "Activar repetición") };
                                if ui.add_enabled_ui(!saving, |ui| ui.add_sized([replay_width, 34.0], Button::new(RichText::new(replay_toggle).size(14.0)).fill(GREY6))).inner.clicked() {
                                    self.handle(&ctx, Action::ToggleReplay);
                                }
                            });
                        ui.add_space(14.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(text("ЗАПИСЬ И СНИМОК", "RECORDING AND SCREENSHOT", "GRABACIÓN Y CAPTURA")).size(12.0).strong().color(TEXT_MUTED));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let (label, color) = if saving {
                                    (text("Сохранение…", "Saving…", "Guardando…"), TEXT_MUTED)
                                } else if self.is_paused() {
                                    (text("Запись на паузе", "Recording paused", "Grabación en pausa"), YELLOW)
                                } else if self.is_recording() && !self.is_live() {
                                    (text("Подготовка записи…", "Preparing recording…", "Preparando grabación…"), TEXT_MUTED)
                                } else if self.is_recording() {
                                    (text("Запись идёт", "Recording", "Grabando"), Color32::from_rgb(255, 140, 145))
                                } else {
                                    (text("Запись выключена", "Recording off", "Grabación desactivada"), TEXT_MUTED)
                                };
                                ui.label(RichText::new(label).size(13.0).color(color));
                            });
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            let half = ((ui.available_width() - ui.spacing().item_spacing.x) / 2.0).max(1.0);
                            let record = if saving { text("Сохранение записи…", "Saving recording…", "Guardando grabación…").to_owned() } else { format!("{}\n{}", if self.is_recording() { text("Остановить запись", "Stop recording", "Detener grabación") } else { text("Начать запись", "Start recording", "Iniciar grabación") }, hotkey_hint(&self.config.hotkeys.record)) };
                            let record_button = if self.is_recording() { Button::new(RichText::new(record).size(14.0)).fill(RED_DIM) } else { Button::new(RichText::new(record).size(14.0)) };
                            if ui.add_enabled_ui(!saving, |ui| ui.add_sized([half, 54.0], record_button)).inner.clicked() {
                                self.handle(&ctx, Action::ToggleRecording);
                            }
                            let screenshot = format!("{}\n{}", text("Скриншот", "Screenshot", "Captura"), hotkey_hint(&self.config.hotkeys.screenshot));
                            if ui.add_sized([half, 54.0], Button::new(RichText::new(screenshot).size(14.0))).clicked() {
                                self.handle(&ctx, Action::Screenshot);
                            }
                        });
                        ui.add_space(12.0);
                        ui.label(RichText::new(text("ЗВУК", "AUDIO", "AUDIO")).size(12.0).strong().color(TEXT_MUTED));
                        ui.add_space(5.0);
                        let mic = if self.config.mic && !self.config.mic_muted { text("Микрофон: включён", "Microphone: on", "Micrófono: activado") } else { text("Микрофон: выключен", "Microphone: off", "Micrófono: desactivado") };
                        if ui.add_sized([width, 36.0], Button::new(RichText::new(mic).size(14.0))).clicked() {
                            if !self.config.mic {
                                self.config.mic = true;
                                self.config.mic_muted = false;
                                self.restart_input(InputKind::Microphone);
                            } else {
                                self.handle(&ctx, Action::ToggleMicMute);
                            }
                            self.config.save();
                        }
                        ui.add_space(4.0);
                        let desktop = if self.config.system_audio && !self.config.system_muted { text("Звук рабочего стола: включён", "Desktop audio: on", "Audio del escritorio: activado") } else { text("Звук рабочего стола: выключен", "Desktop audio: off", "Audio del escritorio: desactivado") };
                        if ui.add_sized([width, 36.0], Button::new(RichText::new(desktop).size(14.0))).clicked() {
                            self.toggle_desktop_audio();
                            self.config.save();
                        }
                        ui.add_space(4.0);
                        if ui.add_sized([width, 36.0], Button::new(RichText::new(text("Экран и звук · подробнее", "Screen and audio · more", "Pantalla y sonido · más")).size(14.0))).clicked() {
                            self.advanced_view = true;
                        }
                        ui.add_space(8.0);
                        let (status_rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
                        if let Some(status) = &self.status {
                            let mut shown = status.text.clone();
                            if !status.error && let Some(path) = &status.file && let Some(name) = path.file_name() {
                                let display = crate::platform::sys::display_path(path).to_string_lossy().into_owned();
                                shown = shown.replace(&display, &name.to_string_lossy());
                            }
                            ui.put(status_rect, egui::Label::new(RichText::new(shown).color(if status.error { RED } else { TEXT_MUTED })).truncate()).on_hover_ui(|ui| {
                                ui.set_max_width(340.0);
                                ui.add(egui::Label::new(&status.text).wrap());
                            });
                        }
                        ui.add_space(18.0);
                        ui.separator();
                        ui.add_space(10.0);
                        ui.label(RichText::new(text("ГАЛЕРЕЯ", "GALLERY", "GALERÍA")).size(12.0).strong().color(TEXT_MUTED));
                        ui.label(RichText::new(text("Последние файлы", "Recent files", "Archivos recientes")).size(15.0));
                        ui.add_space(6.0);
                        let files: Vec<_> = self.config.gallery_files.iter().rev().filter(|path| path.is_file()).take(20).cloned().collect();
                        if files.is_empty() {
                            ui.label(RichText::new(text("Сохранённые откаты, записи и скриншоты появятся здесь.", "Saved replays, recordings and screenshots appear here.", "Aquí aparecerán las repeticiones, grabaciones y capturas guardadas.")).color(TEXT_MUTED));
                        }
                        self.update_gallery_thumbnails(&ctx, &files);
                        let side = ((ui.available_width() - 16.0) / 3.0).clamp(72.0, 150.0);
                        egui::Grid::new("gallery-cards").num_columns(3).spacing([8.0, 10.0]).show(ui, |ui| {
                            for (index, path) in files.iter().enumerate() {
                                ui.vertical(|ui| self.gallery_card(ui, path, side));
                                if index % 3 == 2 { ui.end_row(); }
                            }
                        });
                    });
                });
                ui.separator();
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    let hud_text = if self.config.hud_enabled { text("Скрыть показатели", "Hide metrics", "Ocultar métricas") } else { text("Показать показатели", "Show metrics", "Mostrar métricas") };
                    if ui.add_sized([160.0, 32.0], Button::new(RichText::new(hud_text).size(13.0))).clicked() {
                        self.config.hud_enabled = !self.config.hud_enabled;
                        self.config.save();
                    }
                    if ui.add_sized([145.0, 32.0], Button::new(RichText::new(text("Выход из приложения", "Exit application", "Salir de la aplicación")).size(13.0))).clicked() { self.quit(&ctx); }
                });
                if self.system_meter.cpu_percent.is_some() || self.system_meter.memory.is_some() {
                    let cpu = self.system_meter.cpu_percent.map_or_else(|| "—".to_owned(), |v| format!("{v:.0}%"));
                    let memory = self.system_meter.memory.map_or_else(|| "—".to_owned(), |(used, total)| format!("{:.1} / {:.1} {}", used as f64 / 1_073_741_824.0, total as f64 / 1_073_741_824.0, text("ГиБ", "GiB", "GiB")));
                    ui.label(RichText::new(format!("{}: {cpu}   {}: {memory}", text("Процессор", "CPU", "CPU"), text("ОЗУ", "RAM", "RAM"))).size(11.0).color(TEXT_MUTED));
                }
            });
    }
    fn set_taskbar_button(&self, visible: bool) {
        if let Some(window) = self.native_window {
            crate::platform::sys::set_taskbar_button(window, visible);
        }
    }

    fn show_performance_hud(&self, ctx: &egui::Context) {
        if !self.config.hud_enabled || !self.config.setup_complete {
            #[cfg(windows)]
            if let Ok(mut meter) = self.hud_meter.lock() { meter.gpu_receiver = None; meter.gpu = None; meter.fps_monitor = None; meter.fps = None; }
            return;
        }
        let count = [cfg!(windows) && self.config.hud_fps, self.config.hud_cpu, self.config.hud_memory, cfg!(windows) && self.config.hud_gpu, cfg!(windows) && self.config.hud_gpu_temperature, cfg!(windows) && self.config.hud_video_memory].into_iter().filter(|enabled| *enabled).count();
        let height = (16.0 + count as f32 * 20.0).max(36.0);
        let monitor = ctx.input(|i| i.viewport().monitor_size).unwrap_or(vec2(1920.0, 1080.0));
        let position = hud_position(self.config.hud_corner, monitor, height);
        let builder = egui::ViewportBuilder::default()
            .with_title("opnoverlay metrics")
            .with_position(position)
            .with_inner_size([190.0, height])
            .with_decorations(false)
            .with_resizable(false)
            .with_taskbar(false)
            .with_active(false)
            .with_always_on_top()
            .with_transparent(true)
            .with_mouse_passthrough(true);
        let meter = self.hud_meter.clone();
        let language = self.config.language.unwrap_or_else(Language::system);
        let (show_cpu, show_memory, hide_from_capture) = (self.config.hud_cpu, self.config.hud_memory, self.config.hide_from_capture);
        #[cfg(windows)]
        let (show_gpu, show_gpu_temperature, show_video_memory) = (self.config.hud_gpu, self.config.hud_gpu_temperature, self.config.hud_video_memory);
        #[cfg(windows)]
        let show_fps = self.config.hud_fps;
        ctx.show_viewport_deferred(egui::ViewportId::from_hash_of("performance-hud"), builder, move |ui, _| {
            crate::platform::sys::prepare_hud_window("opnoverlay metrics", hide_from_capture);
            let mut meter = meter.lock().unwrap();
            meter.update();
            #[cfg(windows)]
            meter.update_gpu(show_gpu || show_gpu_temperature || show_video_memory);
            #[cfg(windows)]
            meter.update_fps(show_fps);
            egui::CentralPanel::default().frame(egui::Frame::new().fill(Color32::from_black_alpha(190)).inner_margin(8)).show(ui, |ui| {
                #[cfg(windows)]
                if show_fps {
                    let value = meter.fps.filter(|(at, _)| at.elapsed() < Duration::from_secs(4)).and_then(|(_, value)| value).map_or_else(|| "—".to_owned(), |value| format!("{value:.0}"));
                    ui.label(RichText::new(format!("FPS  {value}")).color(Color32::WHITE).size(14.0));
                }
                if show_cpu {
                    let value = meter.cpu_percent.map_or_else(|| "—".to_owned(), |value| format!("{value:.0}%"));
                    ui.label(RichText::new(format!("CPU  {value}")).color(Color32::WHITE).size(14.0));
                }
                if show_memory {
                    let unit = if language == Language::Russian { "ГиБ" } else { "GiB" };
                    let name = if language == Language::Russian { "ОЗУ" } else { "RAM" };
                    let value = meter.memory.map_or_else(|| "—".to_owned(), |(used, total)| format!("{:.1} / {:.1} {unit}", used as f64 / 1_073_741_824.0, total as f64 / 1_073_741_824.0));
                    ui.label(RichText::new(format!("{name}  {value}")).color(Color32::WHITE).size(14.0));
                }
                #[cfg(windows)]
                if show_gpu {
                    let value = meter.gpu.and_then(|gpu| gpu.utilization).map_or_else(|| "—".to_owned(), |value| format!("{value}%"));
                    ui.label(RichText::new(format!("GPU NVIDIA  {value}")).color(Color32::WHITE).size(14.0));
                }
                #[cfg(windows)]
                if show_gpu_temperature {
                    let value = meter.gpu.and_then(|gpu| gpu.temperature).map_or_else(|| "—".to_owned(), |value| format!("{value} °C"));
                    let name = if language == Language::Russian { "GPU температура" } else { "GPU temp" };
                    ui.label(RichText::new(format!("{name}  {value}")).color(Color32::WHITE).size(14.0));
                }
                #[cfg(windows)]
                if show_video_memory {
                    let unit = if language == Language::Russian { "ГиБ" } else { "GiB" };
                    let name = if language == Language::Russian { "Видеопамять" } else { "VRAM" };
                    let value = meter.gpu.and_then(|gpu| gpu.memory).map_or_else(|| "—".to_owned(), |(used, total)| format!("{:.1} / {:.1} {unit}", used as f64 / 1_073_741_824.0, total as f64 / 1_073_741_824.0));
                    ui.label(RichText::new(format!("{name}  {value}")).color(Color32::WHITE).size(14.0));
                }
            });
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        });
    }

    fn show_notification(&mut self, ctx: &egui::Context) {
        let Some((created, message, error, first_frame)) = &self.notification else { return };
        let lifetime = Duration::from_secs(if *error { 7 } else { 4 });
        let elapsed = first_frame.get().map(Instant::elapsed).unwrap_or_default();
        let Some(remaining) = lifetime.checked_sub(elapsed).filter(|_| first_frame.get().is_some() || created.elapsed() < lifetime) else {
            self.notification = None;
            return;
        };
        let first_frame = first_frame.clone();
        let message = message.clone();
        let error = *error;
        let width = 280.0;
        let mut text_layout = egui::text::LayoutJob::simple(message.clone(), FontId::proportional(14.0), Color32::WHITE, width - 76.0);
        text_layout.wrap.max_rows = 4;
        let text = ctx.fonts_mut(|fonts| fonts.layout_job(text_layout));
        let details = text.elided && self.status.as_ref().is_some_and(|status| status.text == message);
        let details_hint = tr3!("Más información en el menú {}", "Details in menu {}", "Подробнее в меню {}", hotkey_hint(&self.config.hotkeys.menu));
        let height = 28.0 + (text.size().y + if details { 18.0 } else { 0.0 }).max(34.0);
        let exclude = self.config.hide_notifications_from_capture;
        let monitor_width = ctx.input(|input| input.viewport().monitor_size.map_or(1920.0, |size| size.x));
        let builder = egui::ViewportBuilder::default()
            .with_title("opnoverlay notification")
            .with_position([(monitor_width - width - 16.0).max(0.0), 16.0])
            .with_inner_size([width, height])
            .with_decorations(false)
            .with_resizable(false)
            .with_taskbar(false)
            .with_active(false)
            .with_always_on_top()
            .with_transparent(true)
            .with_mouse_passthrough(true);
        ctx.show_viewport_deferred(egui::ViewportId::from_hash_of("notification"), builder, move |ui, _| {
            crate::platform::sys::prepare_hud_window("opnoverlay notification", exclude);
            let elapsed = first_frame.get_or_init(Instant::now).elapsed();
            let remaining = lifetime.saturating_sub(elapsed);
            let opacity = (elapsed.as_secs_f32() / 0.20).min(remaining.as_secs_f32() / 0.25).clamp(0.0, 1.0);
            ui.set_opacity(opacity);
            let next = if elapsed < Duration::from_millis(200) || remaining < Duration::from_millis(250) {
                Duration::from_millis(16)
            } else {
                remaining.saturating_sub(Duration::from_millis(250))
            };
            ui.ctx().request_repaint_after(next);
            egui::CentralPanel::default().frame(egui::Frame::new()
                .fill(Color32::from_rgba_unmultiplied(21, 26, 31, 250))
                .corner_radius(10)
                .inner_margin(Margin::symmetric(12, 14)))
                .show(ui, |ui| {
                    ui.horizontal_centered(|ui| {
                        let (icon, _) = ui.allocate_exact_size(vec2(34.0, 34.0), Sense::hover());
                        let painter = ui.painter();
                        let accent = if error { RED } else { PRIMARY_LIGHT };
                        painter.circle_filled(icon.center(), 16.0, if error { RED_DIM } else { PRIMARY });
                        if error {
                            painter.line_segment([icon.center() + vec2(0.0, -7.0), icon.center() + vec2(0.0, 2.0)], Stroke::new(2.0, accent));
                            painter.circle_filled(icon.center() + vec2(0.0, 7.0), 1.2, accent);
                        } else if message.contains("Скриншот") || message.contains("Screenshot") || message.contains("Captura") {
                            let center = icon.center();
                            painter.rect_stroke(Rect::from_center_size(center, vec2(19.0, 14.0)), 3.0, Stroke::new(1.6, accent), egui::StrokeKind::Inside);
                            painter.circle_stroke(center, 3.8, Stroke::new(1.6, accent));
                            painter.line_segment([center + vec2(-6.0, -8.0), center + vec2(-2.0, -8.0)], Stroke::new(1.6, accent));
                        } else if message.contains("начата") || message.contains("started") || message.contains("iniciada") {
                            painter.circle_filled(icon.center(), 5.0, accent);
                        } else if message.contains("остановлена") || message.contains("stopped") || message.contains("detenida") {
                            painter.rect_filled(Rect::from_center_size(icon.center(), vec2(10.0, 10.0)), 2.0, accent);
                        } else {
                            painter.line_segment([icon.center() + vec2(-7.0, 0.0), icon.center() + vec2(-2.0, 5.0)], Stroke::new(2.0, accent));
                            painter.line_segment([icon.center() + vec2(-2.0, 5.0), icon.center() + vec2(8.0, -6.0)], Stroke::new(2.0, accent));
                        }
                        ui.add_space(8.0);
                        if details {
                            ui.vertical(|ui| {
                                ui.label(text.clone());
                                ui.label(RichText::new(&details_hint).size(11.0).color(TEXT_MUTED));
                            });
                        } else {
                            ui.label(text.clone());
                        }
                    });
                });
        });
        let next = if elapsed < Duration::from_millis(200) || remaining < Duration::from_millis(250) {
            Duration::from_millis(16)
        } else {
            remaining.saturating_sub(Duration::from_millis(250))
        };
        ctx.request_repaint_after(next);
    }

    fn delete_pending_file(&mut self, ctx: &egui::Context) {
        let Some(path) = self.pending_delete.take() else { return };
        if self.config.gallery_files.contains(&path) {
            let (tx, rx) = mpsc::channel();
            let ctx = ctx.clone();
            thread::spawn(move || {
                #[cfg(windows)]
                let result = crate::platform::edit::recycle(&path);
                #[cfg(not(windows))]
                let result = crate::platform::sys::recycle_file(&path);
                let _ = tx.send((path, result));
                ctx.request_repaint();
            });
            self.gallery_deletions.push(rx);
        }
    }

    fn rename_dialog(&mut self, ctx: &egui::Context) {
        let language = self.config.language.unwrap_or_else(Language::system);
        let text = |ru, en, es| match language { Language::Russian => ru, Language::English => en, Language::Spanish => es };
        let error = self.status.as_ref().filter(|status| status.error).map(|status| status.text.clone());
        let Some((path, name)) = self.pending_rename.as_mut() else { return };
        let path = path.clone();
        let mut confirmed = false;
        let mut cancelled = false;
        let modal = egui::Modal::new(egui::Id::unique("rename-gallery-file")).show(ctx, |ui| {
            ui.heading(text("Переименовать файл", "Rename file", "Renombrar archivo"));
            ui.label(text("Новое имя без расширения", "New name without extension", "Nuevo nombre sin extensión"));
            ui.text_edit_singleline(name);
            if let Some(error) = &error { ui.colored_label(RED, error); }
            ui.horizontal(|ui| {
                if ui.button(text("Отмена", "Cancel", "Cancelar")).clicked() { cancelled = true; }
                if ui.button(text("Сохранить", "Save", "Guardar")).clicked() { confirmed = true; }
            });
        });
        if confirmed {
            let name = self.pending_rename.as_ref().map(|(_, name)| name.clone()).unwrap_or_default();
            let result = if self.config.gallery_files.contains(&path) {
                gallery_rename_target(&path, &name).and_then(|target| {
                    if target != path {
                        #[cfg(windows)]
                        crate::platform::edit::rename(&path, &target)?;
                        #[cfg(not(windows))]
                        crate::platform::sys::rename_file(&path, &target)?;
                    }
                    Ok(target)
                })
            } else {
                Err(text("Файла нет в галерее.", "The file is not in the gallery.", "El archivo no está en la galería.").into())
            };
            match result {
                Ok(target) => {
                    for item in &mut self.config.gallery_files {
                        if *item == path { *item = target.clone(); }
                    }
                    self.config.save();
                    self.pending_rename = None;
                    self.set_status(text("Файл переименован.", "File renamed.", "Archivo renombrado."), None, false);
                }
                Err(error) => self.set_status(tr3!("No se pudo renombrar el archivo: {}", "Could not rename file: {}", "Не удалось переименовать файл: {}", error), None, true),
            }
        } else if cancelled || modal.should_close() {
            self.pending_rename = None;
        }
    }

    #[cfg(windows)]
    fn save_trim(&mut self, ctx: &egui::Context) {
        let Some((path, start, end)) = self.pending_trim.as_ref() else { return };
        let path = path.clone();
        let duration = self.player.as_ref().and_then(|(_, player, _)| player.duration());
        let range = parse_trim_time(start).zip(parse_trim_time(end)).filter(|(start, end)| end > start && duration.is_none_or(|duration| *end <= (duration * 1000.0).ceil() as u64));
        let Some((start, end)) = range else { self.trim_error = Some(tr3!("Introduce un inicio y un fin válidos.", "Enter a valid start and end.", "Укажите верные начало и конец обрезки.").into()); return };
        if !self.config.gallery_files.contains(&path) || !path.is_file() {
            self.trim_error = Some(tr3!("Archivo no encontrado en la galería.", "File not found in the gallery.", "Файл не найден в галерее.").into()); return;
        }
        self.pending_trim = None;
        self.player = None;
        self.trim_loading = None;
        self.trim_frames.clear();
        self.trim_requested = false;
        self.trim_error = None;
        self.set_status(tr3!("Guardando archivo recortado…", "Saving trimmed file…", "Сохраняется обрезанный файл…"), None, false);
        self.spawn_job(ctx, move || match crate::platform::edit::trim_current(&path, start as i64 * 10_000, end as i64 * 10_000) {
            Ok((actual_start, actual_end)) => saved(tr3!("Recorte guardado: de {:.3} a {:.3} s.", "Trim saved: {:.3} to {:.3} s.", "Обрезка сохранена: с {:.3} до {:.3} с.", actual_start as f64 / 10_000_000.0, actual_end as f64 / 10_000_000.0), path),
            Err(error) => failed(tr3!("No se pudo recortar el vídeo: {}", "Could not trim video: {}", "Не удалось обрезать видео: {}", error)),
        });
    }

    #[cfg(windows)]
    fn restore_original(&mut self, path: PathBuf, ctx: &egui::Context) {
        if !self.config.gallery_files.contains(&path) { return; }
        self.set_status(tr3!("Restaurando el original…", "Restoring original…", "Восстанавливается оригинал…"), None, false);
        self.spawn_job(ctx, move || match crate::platform::edit::restore(&path) {
            Ok(()) => saved(tr3!("Original restaurado.", "Original restored.", "Оригинал восстановлен.").into(), path),
            Err(error) => failed(tr3!("No se pudo restaurar el original: {}", "Could not restore the original: {}", "Не удалось восстановить оригинал: {}", error)),
        });
    }

    /// Saves the newest frame of the current source as a PNG in the recordings folder.
    fn take_screenshot(&mut self, ctx: &egui::Context) {
        let Some(session) = self.active_session().or(self.preview.as_ref()) else {
            return self.set_status(tr!("Añade una fuente con el botón + del panel Fuentes.", "Add a source with the + button in the Sources dock."), None, true);
        };
        if let Some(problem) = Self::output_dir_problem(&self.config.output_dir) {
            return self.set_status(problem, None, true);
        }
        let (mut width, mut height, mut rgba) = match session.snapshot() {
            Ok(image) => image,
            Err(e) => return self.set_status(tr3!("No se pudo hacer la captura: {}", "Could not take the screenshot: {}", "Не удалось сделать скриншот: {}", e), None, true),
        };
        if let Some(region) = self.config.region.and_then(|r| r.fit((width, height))) {
            rgba = capture::crop_rgba(&rgba, width, region);
            (width, height) = (region.width, region.height);
        }
        if let (Some((corner, size)), Some(camera)) = (self.overlay(), self.camera.as_ref())
            && let Some(frame) = camera.shared.latest.lock().unwrap().clone()
        {
            paste_camera(&mut rgba, width, height, &frame, corner, size);
        }
        let format = self.config.screenshot_format;
        let quality = self.config.jpeg_quality;
        let path = new_output(&self.config.output_dir, Some("Screenshot"), format.extension(), &mut self.named);
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        // PNG encoding of a large frame takes a moment; keep the UI responsive.
        thread::spawn(move || {
            let result = std::fs::create_dir_all(path.parent().unwrap_or(&path))
                .map_err(|e| e.to_string())
                .and_then(|()| capture::write_screenshot_unique(&path, format, width, height, &rgba, quality));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.screenshots.push(rx);
    }

    #[cfg(windows)]
    fn check_desktop(&mut self, ctx: &egui::Context) {
        if self.exit_requested || self.desktop_checked.elapsed() < Duration::from_secs(1) { return; }
        self.desktop_checked = Instant::now();
        let available = crate::platform::sys::interactive_desktop_available();
        let restored = available && !self.desktop_available;
        self.desktop_available = available;
        if restored { self.restore_capture_after_desktop_change(ctx); }
    }

    #[cfg(windows)]
    fn restore_capture_after_desktop_change(&mut self, ctx: &egui::Context) {
        let recording = self.is_recording();
        let replay = self.is_replaying();
        let paused = self.is_paused();
        if self.is_session_active() {
            if recording { self.desktop_resume = Some((recording, replay, paused)); }
            self.stop_session(ctx, None);
            self.refresh_sources();
            if !recording && replay {
                self.start_replay(ctx);
                if self.is_replaying() { self.notify(tr3!("Captura restablecida; el búfer se llenará de nuevo", "Capture restored; the replay buffer will refill", "Захват экрана восстановлен; буфер отката заполнится заново"), false); }
            }
            if recording { self.notify(tr3!("Restableciendo captura; la grabación continuará en otro archivo", "Restoring capture; recording will continue in a new file", "Восстанавливаем захват; запись продолжится в новом файле"), true); }
        } else if !matches!(self.rec, RecState::Saving(_)) {
            self.refresh_sources();
            self.restart_preview(ctx);
            if let Some((recording, replay, paused)) = self.desktop_resume.take() {
                if replay { self.start_replay(ctx); }
                if recording { self.start_recording(ctx); }
                if paused && self.is_recording() {
                        if let Some(session) = self.active_session() { session.shared.outputs.lock().unwrap().set_file_paused(true); }
                    }
            }
        }
    }

    /// Background bookkeeping; runs even while the window is minimized.
    fn poll(&mut self, ctx: &egui::Context) {
        #[cfg(windows)]
        self.check_desktop(ctx);
        let replay_error = self.active_session().and_then(|session| session.shared.outputs.lock().unwrap().replay_error());
        if let Some(error) = replay_error {
            self.stop_replay(ctx);
            self.set_status(tr3!("Repetición desactivada: {}", "Replay disabled: {}", "Откаты выключены: {}", error), None, true);
        }
        if let Some(rx) = self.disk_check.take() {
            match rx.try_recv() {
                Ok(results) => {
                    let low = results.into_iter().find_map(|(label, bytes)| bytes.filter(|bytes| *bytes < 1024 * 1024 * 1024).map(|bytes| (label, bytes)));
                    if let Some((label, bytes)) = low {
                        if !self.low_disk_warned {
                            self.low_disk_warned = true;
                            self.set_status(tr3!("Poco espacio para {}: quedan unos {} MB. Libera espacio para evitar errores al guardar.", "Low disk space for {}: about {} MB left. Free up space to prevent save errors.", "Мало места для {}: осталось примерно {} МБ. Освободите место, иначе файл может не сохраниться.", label, bytes / 1_000_000), None, true);
                        }
                    } else { self.low_disk_warned = false; }
                }
                Err(mpsc::TryRecvError::Empty) => self.disk_check = Some(rx),
                _ => {}
            }
        }
        let replaying = self.is_replaying();
        let disk_replay = replaying && self.config.replay_storage == ReplayStorage::Disk;
        if (self.is_recording() || replaying) && self.disk_check.is_none()
            && self.last_disk_check.is_none_or(|last| last.elapsed() >= Duration::from_secs(30))
        {
            let folder = self.config.output_dir.clone();
            let recording = self.is_recording();
            let (tx, rx) = mpsc::channel();
            let wake = ctx.clone();
            self.last_disk_check = Some(Instant::now());
            self.disk_check = Some(rx);
            thread::spawn(move || {
                let mut results = Vec::new();
                let label = if recording { tr3!("la grabación", "recording", "записи") } else { tr3!("guardar repeticiones", "saving replays", "сохранения откатов") };
                results.push((label, crate::storage::disk_space(&folder).1));
                if disk_replay { results.push((tr3!("el búfer temporal", "the replay buffer", "временного буфера отката"), crate::platform::sys::free_space(&std::env::temp_dir()))); }
                let _ = tx.send(results);
                wake.request_repaint();
            });
        }
        for rx in std::mem::take(&mut self.gallery_deletions) {
            match rx.try_recv() {
                Ok((path, Ok(()))) => {
                    self.config.gallery_files.retain(|item| item != &path);
                    self.config.save();
                    self.set_status(tr3!("Archivo movido a la papelera.", "File moved to the recycle bin.", "Файл перемещён в корзину."), None, false);
                }
                Ok((path, Err(error))) => {
                    if !path.is_file() { self.config.gallery_files.retain(|item| item != &path); self.config.save(); }
                    self.set_status(error, None, true);
                }
                Err(mpsc::TryRecvError::Empty) => self.gallery_deletions.push(rx),
                Err(mpsc::TryRecvError::Disconnected) => self.set_status(tr3!("No se pudo mover el archivo a la papelera.", "Could not move file to the recycle bin.", "Не удалось переместить файл в корзину."), None, true),
            }
        }
        let mic_error = self.mic.as_ref().and_then(|mic| mic.shared.error());
        if mic_error != self.last_mic_error {
            self.last_mic_error = mic_error.clone();
            if let Some(error) = mic_error {
                self.set_status(tr3!("Micrófono no disponible: {}. La grabación continuará sin él.", "Microphone unavailable: {}. Recording will continue without it.", "Микрофон недоступен: {}. Запись продолжится без него.", error), None, true);
            }
        }
        let system_error = self.system.as_ref().and_then(|system| system.shared.error());
        if system_error != self.last_system_error {
            self.last_system_error = system_error.clone();
            if let Some(error) = system_error {
                self.set_status(tr3!("Audio del escritorio no disponible: {}. La grabación continuará sin él.", "Desktop audio unavailable: {}. Recording will continue without it.", "Звук рабочего стола недоступен: {}. Запись продолжится без него.", error), None, true);
            }
        }
        let app_error = self.app_audio.as_ref().and_then(|input| input.shared.error());
        self.report_app_audio_error(app_error);
        let actions: Vec<Action> = self.actions.lock().unwrap().drain(..).collect();
        for action in actions {
            self.handle(ctx, action);
        }
        if let Some(e) = self.hotkeys.as_ref().and_then(GlobalHotkeys::take_error) {
            self.set_status(tr!("Atajos globales no disponibles: {}", "Global hotkeys unavailable: {}", e), None, true);
        }
        if let Some(rx) = &self.update_check
            && let Ok(release) = rx.try_recv()
        {
            self.update_dialog = release.as_ref().is_some_and(|r| self.config.skipped_version.as_ref() != Some(&r.version));
            (self.update, self.update_check) = (release, None);
        }
        let commands: Vec<TrayCommand> = self.tray_commands.lock().unwrap().drain(..).collect();
        for command in commands {
            match command {
                // Settings applies a copy of the config taken when it opened, which a session started
                // now would not be in; stopping is fine.
                TrayCommand::ToggleRecording | TrayCommand::ToggleReplay if self.settings.is_some() && !self.is_session_active() => {
                    self.set_status(tr!("Cierra los ajustes para empezar.", "Close Settings to start."), None, true);
                }
                TrayCommand::ToggleRecording => self.handle(ctx, Action::ToggleRecording),
                TrayCommand::TogglePause => self.handle(ctx, Action::TogglePause),
                TrayCommand::ToggleReplay => self.handle(ctx, Action::ToggleReplay),
                TrayCommand::SaveReplay => self.handle(ctx, Action::SaveReplay),
                TrayCommand::Screenshot => self.handle(ctx, Action::Screenshot),
                TrayCommand::Show => self.show_window(ctx),
                TrayCommand::Exit => self.quit(ctx),
            }
        }
        // Minimize to tray: the window stays minimized (a hidden window would stop eframe from
        // handling hotkeys and tray clicks) and only loses its taskbar button.
        let minimized = ctx.input(|i| i.viewport().minimized);
        if self.config.minimize_to_tray && !self.hidden && self.opening.is_none() && minimized == Some(true) {
            self.set_taskbar_button(false);
            self.hidden = true;
        }
        let state = TrayState {
            recording: self.is_recording(),
            paused: self.is_paused(),
            replay: self.is_replaying(),
            english: i18n::english(),
            russian: i18n::russian(),
        };
        if let Some(tray) = &mut self.tray {
            tray.update(state);
        }

        let saved_recording = match &self.rec {
            RecState::Saving(rx) => match rx.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err("обработчик сохранения завершился".into())),
            },
            _ => None,
        };
        if let Some(result) = saved_recording {
            self.rec = RecState::Idle;
            // Before the status: if the source is gone, its error must not hide where the file went.
            if self.preview.is_none() {
                self.restart_preview(ctx);
            }
            match result {
                Ok(path) => self.set_status(tr3!("Grabación guardada: {}", "Recording saved: {}", "Запись сохранена: {}", crate::platform::sys::display_path(&path).display()), Some(path), false),
                Err(e) => self.set_status(tr!("Error al guardar la grabación: {}", "Error saving the recording: {}", e), None, true),
            }
            #[cfg(windows)]
            if self.desktop_available && !self.exit_requested {
                if let Some((recording, replay, paused)) = self.desktop_resume.take() {
                    self.refresh_sources();
                    if replay { self.start_replay(ctx); }
                    if recording { self.start_recording(ctx); }
                    if paused && self.is_recording() {
                        if let Some(session) = self.active_session() { session.shared.outputs.lock().unwrap().set_file_paused(true); }
                    }
                }
            }
        }

        for rx in std::mem::take(&mut self.screenshots) {
            match rx.try_recv() {
                Ok(Ok(path)) => self.set_status(tr3!("Captura guardada: {}", "Screenshot saved: {}", "Скриншот сохранён: {}", crate::platform::sys::display_path(&path).display()), Some(path), false),
                Ok(Err(e)) => self.set_status(tr3!("No se pudo guardar la captura: {}", "Could not save the screenshot: {}", "Не удалось сохранить скриншот: {}", e), None, true),
                Err(mpsc::TryRecvError::Empty) => self.screenshots.push(rx),
                Err(mpsc::TryRecvError::Disconnected) => self.set_status(tr3!("No se pudo guardar la captura: finalizó el proceso.", "Could not save screenshot: worker exited.", "Не удалось сохранить скриншот: обработчик завершился."), None, true),
            }
        }

        let (done, running) = std::mem::take(&mut self.jobs).into_iter().partition(JoinHandle::is_finished);
        self.jobs = running;
        for job in done {
            if let Ok(status) = job.join() {
                self.set_status(status.text, status.file, status.error);
            }
        }

        // A recording that cannot be written (its folder, a full disk, a failed encoder) is stopped
        // now, not when the user stops it, so the error shows right away.
        if let RecState::Active { session, recording: Some(_), .. } = &self.rec
            && session.shared.outputs.lock().unwrap().failed()
        {
            return self.stop_recording(ctx);
        }
        if self.active_session().is_some_and(Session::is_finished) {
            let reason = if self.is_recording() {
                tr!("La fuente dejó de estar disponible; guardando…", "The source is no longer available; saving…")
            } else {
                tr!("La fuente dejó de estar disponible; búfer de repetición detenido.", "The source is no longer available; replay buffer stopped.")
            };
            return self.stop_session(ctx, Some(reason.into()));
        }
        if self.preview.as_ref().is_some_and(Session::is_finished) {
            self.source_lost(tr!("La fuente ya no está disponible. Añade otra con el botón +.", "The source is no longer available. Add another one with the + button.").into());
        }
        self.cpu.update();
    }

    fn report_app_audio_error(&mut self, error: Option<String>) {
        if error != self.last_app_error {
            self.last_app_error = error.clone();
            if let Some(error) = error {
                self.set_status(tr3!("No se pudo capturar el audio de la aplicación: {}. Puede faltar en la grabación.", "Could not capture application audio: {}. It may be missing from the recording.", "Не удалось захватить звук выбранного приложения: {}. Его звук может отсутствовать в записи.", error), None, true);
            }
        }
    }

    fn update_camera_texture(&mut self, ctx: &egui::Context) {
        let Some(camera) = &self.camera else { return };
        let seq = camera.shared.seq.load(Ordering::Relaxed);
        if self.camera_seq == Some(seq) {
            return;
        }
        let Some(frame) = camera.shared.latest.lock().unwrap().clone() else { return };
        self.camera_seq = Some(seq);
        let rgba: Vec<u8> = frame.bgra.as_chunks::<4>().0.iter().flat_map(|&[b, g, r, a]| [r, g, b, a]).collect();
        let image = egui::ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &rgba);
        match &mut self.camera_texture {
            Some(t) => t.set(image, TextureOptions::LINEAR),
            None => self.camera_texture = Some(ctx.load_texture("camera", image, TextureOptions::LINEAR)),
        }
    }

    fn update_texture(&mut self, ctx: &egui::Context) {
        let Some(session) = self.active_session().or(self.preview.as_ref()) else { return };
        let (seq, image) = {
            let guard = session.shared.preview.lock().unwrap();
            let Some(frame) = guard.as_ref() else { return };
            if self.texture_seq == Some(frame.seq) && self.texture.is_some() {
                return;
            }
            (frame.seq, egui::ColorImage::from_rgba_unmultiplied(frame.size, &frame.rgba))
        };
        self.texture_seq = Some(seq);
        match &mut self.texture {
            Some(t) => t.set(image, TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("preview", image, TextureOptions::LINEAR)),
        }
    }

    fn open_settings(&mut self) {
        self.refresh_devices();
        self.refresh_sources();
        self.storage_panel = StoragePanel::default();
        let seconds = self.config.replay_seconds;
        self.settings = Some(SettingsDialog { draft: self.config.clone(), page: SettingsPage::General, capturing: None, replay_duration: format!("{:02}:{:02}", seconds / 60, seconds % 60), selected_source: self.selected });
        // Registered combinations never reach the window, so they could not be typed again.
        self.unbind_hotkeys();
    }

    fn apply_settings(&mut self, mut new: Config) {
        let startup_error = if new.auto_start_windows != self.config.auto_start_windows {
            match crate::platform::sys::set_startup(new.auto_start_windows) {
                Ok(()) => None,
                Err(error) => { new.auto_start_windows = self.config.auto_start_windows; Some(error) }
            }
        } else { None };
        new.gallery_files.clone_from(&self.config.gallery_files);
        new.output_dir = typed_output_dir(new.output_dir, &self.config.output_dir);
        new.replay_dir.clone_from(&new.output_dir);
        new.screenshot_dir.clone_from(&new.output_dir);
        let old = std::mem::replace(&mut self.config, new);
        let c = &self.config;
        let system_changed = (old.system_audio, &old.system_device) != (c.system_audio, &c.system_device);
        let mic_changed = (old.mic, &old.mic_device) != (c.mic, &c.mic_device);
        if old.language != c.language {
            i18n::set(c.language.unwrap_or_else(Language::system));
            // Source names are translated ("Pantalla 1" / "Display 1").
            self.refresh_sources();
            self.config.source_label = self.selected_source().map(|s| s.label.clone());
        }
        // OK also tries again a device that failed (unplugged, then plugged back in).
        if system_changed || input_failed(&self.system) {
            self.restart_input(InputKind::System);
        }
        if mic_changed || input_failed(&self.mic) {
            self.restart_input(InputKind::Microphone);
        }
        if let Some(mic) = &self.mic {
            mic.shared.set_denoise(self.config.mic_denoise);
        }
        self.apply_capture_visibility();
        self.config.save();
        if let Some(error) = startup_error {
            self.set_status(tr3!("No se pudo cambiar el inicio automático: {}", "Could not change startup: {}", "Не удалось изменить автозапуск: {}", error), None, true);
        }
    }

    /// Registers the configured hotkeys, reporting the ones another application already uses.
    fn bind_hotkeys(&mut self) -> bool {
        let Some(hotkeys) = &mut self.hotkeys else { return false };
        let Hotkeys { menu, record, pause, screenshot, mute_mic, replay, save_replay } = &self.config.hotkeys;
        let wanted = [
            (menu, Action::ToggleMenu, "menu", tr!("Abrir / cerrar menú", "Open / close menu")),
            (record, Action::ToggleRecording, "record", tr!("Iniciar / Detener Grabación", "Start / Stop Recording")),
            (pause, Action::TogglePause, "pause", tr!("Pausar / Reanudar Grabación", "Pause / Resume Recording")),
            (screenshot, Action::Screenshot, "screenshot", tr!("Hacer captura de pantalla", "Take Screenshot")),
            (mute_mic, Action::ToggleMicMute, "mute-mic", tr!("Silenciar / Activar Mic/Aux", "Mute / Unmute Mic/Aux")),
            (replay, Action::ToggleReplay, "replay", tr!("Iniciar / Detener búfer de repetición", "Start / Stop Replay Buffer")),
            (save_replay, Action::SaveReplay, "save-replay", tr!("Guardar repetición", "Save Replay")),
        ];
        let (bindings, actions): (Vec<Binding>, Vec<Action>) = wanted
            .into_iter()
            .filter_map(|(text, action, id, description)| {
                Some((Binding { id, description: description.to_owned(), text: text.clone()? }, action))
            })
            .unzip();
        *self.bound_actions.lock().unwrap() = actions;
        let failed = hotkeys.bind(&bindings);
        let menu_ready = menu.as_ref().is_some_and(|key| !failed.contains(key));
        if !failed.is_empty() {
            let list = failed.join(", ");
            let text = tr3!("Atajos no disponibles (¿los usa otra aplicación?): {}", "Hotkeys unavailable (used by another application?): {}", "Горячие клавиши недоступны, возможно, их использует другая программа: {}.", list);
            let text = if !menu_ready && self.tray.is_some() { tr3!("{} Abre los ajustes desde la bandeja.", "{} Open settings from the tray icon.", "{} Откройте настройки через значок в трее.", text) } else { text };
            self.set_status(text, None, true);
        }
        menu_ready
    }

    fn unbind_hotkeys(&mut self) {
        if let Some(hotkeys) = &mut self.hotkeys {
            hotkeys.unbind();
        }
    }

    fn apply_capture_visibility(&self) {
        if let Some(window) = self.native_window {
            crate::platform::sys::exclude_from_capture(window, self.config.hide_from_capture);
        }
    }

    // ------------------------------------------------------------------ main window

    fn preview_area(&mut self, ui: &mut egui::Ui) {
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let source = self.active_session().or(self.preview.as_ref()).and_then(Session::source_size);
        let mut screenshot = false;
        response.context_menu(|ui| {
            if ui.button(tr!("Hacer captura de pantalla", "Take Screenshot")).clicked() {
                screenshot = true;
                ui.close();
            }
        });
        if screenshot {
            self.take_screenshot(ui.ctx());
        }
        let painter = ui.painter_at(rect);
        let aspect = self.texture.as_ref().map_or(16.0 / 9.0, |t| t.size_vec2().x / t.size_vec2().y);
        let size = if rect.width() / rect.height() > aspect {
            vec2(rect.height() * aspect, rect.height())
        } else {
            vec2(rect.width(), rect.width() / aspect)
        };
        let canvas = Rect::from_center_size(rect.center(), size);
        // Ask for a preview as sharp as it is drawn on screen (but never larger than the source).
        if let Some(session) = self.active_session().or(self.preview.as_ref()) {
            let pixels = (canvas.width() * ui.ctx().pixels_per_point()).round() as usize;
            session.shared.preview_width.store(pixels, Ordering::Relaxed);
        }
        painter.rect_filled(canvas, 0.0, Color32::BLACK);
        match &self.texture {
            Some(texture) => {
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                painter.image(texture.id(), canvas, uv, Color32::WHITE);
            }
            None => {
                let msg = if self.selected.is_none() { tr!("Añade una fuente con el botón +", "Add a source with the + button") } else { "" };
                painter.text(canvas.center(), Align2::CENTER_CENTER, msg, FontId::proportional(15.0), TEXT_MUTED);
            }
        }
        let Some(source) = source.filter(|_| self.texture.is_some()) else { return };
        let area = self.config.region.and_then(|r| r.fit(source));
        if let (Some((corner, size)), Some(texture)) = (self.overlay(), &self.camera_texture)
            && !self.picking_region
        {
            // The overlay sits in the recorded area: the region, or the whole source.
            let area = area.unwrap_or(Region { x: 0, y: 0, width: source.0, height: source.1 });
            let (x, y, w, h) = camera::overlay_rect((area.width, area.height), (texture.size()[0] as u32, texture.size()[1] as u32), corner, size);
            let rect = region_rect(Region { x: area.x + x, y: area.y + y, width: w, height: h }, canvas, source);
            painter.image(texture.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }
        if self.picking_region {
            self.pick_region(ui, &response, &painter, canvas, source);
        } else if let Some(region) = self.config.region.and_then(|r| r.fit(source)) {
            outline_region(&painter, canvas, region_rect(region, canvas, source));
        }
    }

    /// Drag on the preview to choose the region to record; Escape cancels.
    fn pick_region(&mut self, ui: &egui::Ui, response: &egui::Response, painter: &egui::Painter, canvas: Rect, source: (u32, u32)) {
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            (self.picking_region, self.region_drag) = (false, None);
            return;
        }
        let pointer = response.interact_pointer_pos();
        if response.drag_started()
            && let Some(p) = pointer
        {
            self.region_drag = Some((p, p));
        }
        if let (Some(drag), Some(p)) = (&mut self.region_drag, pointer) {
            drag.1 = p;
        }
        match self.region_drag {
            Some((start, end)) => {
                let selection = Rect::from_two_pos(start, end).intersect(canvas);
                outline_region(painter, canvas, selection);
                if response.drag_stopped() {
                    self.config.region = region_from_rect(selection, canvas, source);
                    (self.picking_region, self.region_drag) = (false, None);
                }
            }
            None => {
                painter.rect_filled(canvas, 0.0, Color32::from_black_alpha(90));
                let hint = tr!("Arrastra para elegir la región que se grabará · Esc para cancelar", "Drag to choose the region to record · Esc to cancel");
                painter.text(canvas.center(), Align2::CENTER_CENTER, hint, FontId::proportional(15.0), Color32::WHITE);
            }
        }
    }

    fn docks(&mut self, ui: &mut egui::Ui) {
        let gap = 4.0;
        let width = ui.available_width() - gap * 2.0;
        let height = ui.available_height();
        if width < 580.0 {
            egui::ScrollArea::vertical().id_salt("compact-docks").auto_shrink([false, false]).show(ui, |ui| {
                let width = ui.available_width();
                dock(ui, tr3!("Fuentes", "Capture", "Что записывать"), vec2(width, 160.0), |ui| self.sources_dock(ui));
                dock(ui, tr3!("Mezclador de audio", "Audio levels", "Громкость звука"), vec2(width, 200.0), |ui| self.mixer_dock(ui));
                dock(ui, tr3!("Controles", "Controls", "Управление"), vec2(width, 190.0), |ui| self.controls_dock(ui));
            });
            return;
        }
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            dock(ui, tr3!("Fuentes", "Capture", "Что записывать"), vec2(width * 0.30, height), |ui| self.sources_dock(ui));
            dock(ui, tr3!("Mezclador de audio", "Audio levels", "Громкость звука"), vec2(width * 0.44, height), |ui| self.mixer_dock(ui));
            dock(ui, tr3!("Controles", "Controls", "Управление"), vec2(width * 0.26, height), |ui| self.controls_dock(ui));
        });
    }

    fn sources_dock(&mut self, ui: &mut egui::Ui) {
        let recording = matches!(self.rec, RecState::Active { .. });
        let ctx = ui.ctx().clone();
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_enabled_ui(!recording, |ui| {
                ui.horizontal(|ui| {
                    let mut camera_choice = None;
                    #[cfg_attr(not(windows), allow(unused_mut))]
                    let mut app_choice: Option<String> = None;
                    let add = ui.menu_button(RichText::new(tr3!("+ Añadir", "+ Add", "+ Добавить")).strong(), |ui| {
                        let mut chosen = None;
                        // The portal's dialog lets the user pick the screen or window.
                        #[cfg(target_os = "linux")]
                        if ui.button(tr!("Captura de pantalla o ventana…", "Screen or Window Capture…")).clicked() {
                            capture::forget_choice();
                            chosen = Some(SourceId::Portal);
                        }
                        #[cfg(windows)]
                        ui.menu_button(tr!("Captura de pantalla", "Display Capture"), |ui| {
                            for s in self.sources.iter().filter(|s| matches!(s.id, SourceId::Monitor(_))) {
                                if ui.button(&s.label).clicked() {
                                    chosen = Some(s.id);
                                }
                            }
                        });
                        #[cfg(windows)]
                        ui.menu_button(tr!("Captura de ventana", "Window Capture"), |ui| {
                            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                                for s in self.sources.iter().filter(|s| matches!(s.id, SourceId::Window(_))) {
                                    if ui.button(&s.label).clicked() {
                                        chosen = Some(s.id);
                                    }
                                }
                            });
                        });
                        #[cfg(windows)]
                        ui.menu_button(tr!("Captura de audio de aplicación", "Application Audio Capture"), |ui| {
                            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                                for app in &self.audio_apps {
                                    if ui.button(format!("{}  ·  {}", app.exe, app.title)).clicked() {
                                        app_choice = Some(app.exe.clone());
                                    }
                                }
                            });
                        });
                        ui.menu_button(tr!("Dispositivo de captura de vídeo", "Video Capture Device"), |ui| {
                            if self.cameras.is_empty() {
                                ui.label(RichText::new(tr!("No se ha encontrado ninguna cámara", "No camera found")).color(TEXT_MUTED));
                            }
                            for device in &self.cameras {
                                if ui.button(&device.name).clicked() {
                                    camera_choice = Some(device.clone());
                                }
                            }
                        });
                        chosen
                    });
                    // Windows come and go; refresh the list each time the menu opens.
                    if add.response.clicked() {
                        self.refresh_sources();
                        self.cameras = camera::list_devices();
                        self.audio_apps = capture::list_audio_apps();
                    }
                    if let Some(Some(id)) = add.inner {
                        self.select_source(&ctx, id);
                        ui.close();
                    }
                    add.response.on_hover_text(tr!("Añadir fuente", "Add source"));
                    if let Some(exe) = app_choice {
                        self.config.app_audio = Some(exe);
                        self.restart_app_audio();
                        if audio_overlap(&self.config) {
                            self.set_status(tr3!("El audio de la aplicación puede grabarse dos veces o seguir grabándose al desactivarlo. Selecciona el dispositivo predeterminado en ajustes de audio.", "Application audio may be recorded twice or remain in the recording when disabled. Select the default device in audio settings.", "Звук приложения может записываться дважды или остаться в записи после отключения: для рабочего стола выбрано отдельное устройство. Выберите устройство по умолчанию в настройках звука."), None, true);
                        }
                        ui.close();
                    }
                    if let Some(device) = camera_choice {
                        let (corner, size) = self.config.camera.as_ref().map_or((Corner::BottomRight, CameraSize::Medium), |c| (c.corner, c.size));
                        self.config.camera = Some(CameraConfig { device: device.id, name: device.name, corner, size });
                        self.restart_camera(&ctx);
                        ui.close();
                    }
                    ui.menu_button(tr3!("Propiedades", "Options", "Настройки"), |ui| {
                        if ui.checkbox(&mut self.config.cursor, tr!("Capturar el cursor", "Capture Cursor")).changed() {
                            self.restart_preview(&ctx);
                        }
                        ui.separator();
                        if ui.add_enabled(self.selected.is_some(), Button::new(tr!("Seleccionar región…", "Select Region…"))).clicked() {
                            self.picking_region = true;
                            ui.close();
                        }
                        let whole = Button::new(tr!("Grabar toda la fuente", "Record Whole Source"));
                        if ui.add_enabled(self.config.region.is_some(), whole).clicked() {
                            self.config.region = None;
                            ui.close();
                        }
                        if self.config.app_audio.is_some() {
                            ui.separator();
                            if ui.button(tr!("Quitar audio de aplicación", "Remove Application Audio")).clicked() {
                                self.config.app_audio = None;
                                self.restart_app_audio();
                                ui.close();
                            }
                        }
                        let mut remove_camera = false;
                        if let Some(cam) = &mut self.config.camera {
                            ui.separator();
                            ui.label(RichText::new(tr!("Cámara", "Camera")).strong());
                            for corner in Corner::ALL {
                                ui.radio_value(&mut cam.corner, corner, corner.label());
                            }
                            ui.separator();
                            for size in CameraSize::ALL {
                                ui.radio_value(&mut cam.size, size, size.label());
                            }
                            ui.separator();
                            remove_camera = ui.button(tr!("Quitar cámara", "Remove Camera")).clicked();
                        }
                        if remove_camera {
                            self.config.camera = None;
                            self.restart_camera(&ctx);
                            ui.close();
                        }
                    });
                });
            });
            ui.add_space(2.0);
            if recording {
                ui.label(RichText::new(tr3!("Detén la grabación y la repetición para cambiar la fuente.", "Stop recording and replay to change the source.", "Чтобы сменить источник, отключите запись и откаты.")).size(11.0).color(TEXT_MUTED));
                ui.add_space(4.0);
            }
            ui.with_layout(Layout::top_down(Align::Min), |ui| match self.selected_source() {
                Some(source) => {
                    let kind = match source.id {
                        SourceId::Monitor(_) => tr!("Captura de pantalla", "Display Capture"),
                        SourceId::Window(_) => tr!("Captura de ventana", "Window Capture"),
                        #[cfg(target_os = "linux")]
                        SourceId::Portal => tr!("Captura de pantalla o ventana", "Screen or Window Capture"),
                    };
                    // Before the label, which is often too long to show in full.
                    let region = self.config.region.map_or(String::new(), |r| {
                        format!(" ({} {}×{})", tr!("región", "region"), r.width, r.height)
                    });
                    list_row(ui, &format!("{kind}{region}  ·  {}", source.label));
                    if let Some(exe) = &self.config.app_audio {
                        ui.add_space(2.0);
                        let state = if self.app_audio.is_some() { String::new() } else { tr!(" (no se está ejecutando)", " (not running)").to_owned() };
                        list_row(ui, &format!("{}  ·  {exe}{state}", tr!("Captura de audio de aplicación", "Application Audio Capture")));
                    }
                    if let Some(cam) = &self.config.camera {
                        ui.add_space(2.0);
                        let error = self.camera.as_ref().and_then(|c| c.shared.error.lock().unwrap().clone());
                        let text = format!("{}  ·  {}", tr!("Dispositivo de captura de vídeo", "Video Capture Device"), cam.name);
                        let row = list_row(ui, &text);
                        if let Some(e) = error {
                            row.on_hover_text(tr!("Error de la cámara: {}", "Camera error: {}", e));
                        }
                    }
                }
                None => {
                    ui.label(RichText::new(tr!("No hay ninguna fuente. Pulsa + para añadir una.", "No sources. Press + to add one.")).color(TEXT_MUTED));
                }
            });
        });
    }

    fn mixer_dock(&mut self, ui: &mut egui::Ui) {
        // Three channels do not always fit the dock's height.
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.mixer_channels(ui));
    }

    fn mixer_channels(&mut self, ui: &mut egui::Ui) {
        let app_label = self.config.app_audio.clone().unwrap_or_default();
        let mut any = false;
        for i in 0..3 {
            let c = &mut self.config;
            let (label, volume, muted, input) = match i {
                0 => (tr!("Audio del escritorio", "Desktop Audio").to_owned(), &mut c.system_volume, &mut c.system_muted, &self.system),
                1 => (tr3!("Micrófono", "Microphone", "Микрофон").to_owned(), &mut c.mic_volume, &mut c.mic_muted, &self.mic),
                _ => (app_label.clone(), &mut c.app_volume, &mut c.app_muted, &self.app_audio),
            };
            // Disabled devices are not shown, as in OBS.
            let Some(input) = input else {
                self.meters[i] = 0.0;
                continue;
            };
            any = true;
            let peak = input.shared.peak() * channel_gain(*volume, *muted);
            // Fast attack, slow release.
            self.meters[i] = if peak > self.meters[i] { peak } else { self.meters[i] * 0.85 };
            let db = 20.0 * self.meters[i].max(1e-6).log10();

            ui.horizontal(|ui| {
                ui.label(&label);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let text = if db <= -60.0 { "-inf dB".to_owned() } else { format!("{db:.1} dB") };
                    ui.label(RichText::new(text).monospace().small().color(TEXT_MUTED));
                    if let Some(e) = input.shared.error() {
                        ui.label(RichText::new("Error").small().color(RED)).on_hover_text(e);
                    }
                });
            });
            meter(ui, db);
            ui.horizontal(|ui| {
                ui.spacing_mut().slider_width = (ui.available_width() - 40.0).max(60.0);
                let changed = ui.add(Slider::new(volume, 0.0..=1.0).show_value(false)).accessible_name(format!("{}: {label}", tr3!("Volumen", "Volume", "Громкость"))).changed();
                let tip = if *muted { tr!("Quitar silencio", "Unmute") } else { tr!("Silenciar", "Mute") };
                let toggled = mute_button(ui, *muted, &format!("{tip}: {label}")).on_hover_text(tip).clicked();
                if toggled {
                    *muted = !*muted;
                }
                if changed || toggled {
                    input.shared.set_gain(channel_gain(*volume, *muted));
                }
            });
            ui.add_space(8.0);
        }
        let limiting = matches!(&self.rec, RecState::Active { mixer: Some(mixer), .. } if mixer.limiting());
        if limiting {
            ui.label(RichText::new(tr!("Mezcla al límite: baja algún volumen", "Mix at its limit: lower a volume")).small().color(RED)).on_hover_text(tr!(
                "Los canales juntos superan el máximo. Para que no se distorsione, la grabación baja el volumen en esos momentos.",
                "The channels add up to more than the maximum. To keep it from distorting, the recording lowers the volume at those moments."
            ));
        }
        if !any {
            ui.label(RichText::new(tr3!("No hay fuentes de audio activas.\nActiva el micrófono o el audio del escritorio en la pantalla principal.", "No audio sources active.\nEnable desktop audio or microphone on the main screen.", "Звук выключен.\nВключите микрофон или звук рабочего стола на главном экране.")).color(TEXT_MUTED));
        }
    }

    fn controls_dock(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let full = ui.available_width();
        let height = 30.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            match &self.rec {
                RecState::Idle | RecState::Active { recording: None, .. } => {
                    if ui.add_sized([full, height], Button::new(tr!("Iniciar Grabación", "Start Recording"))).on_hover_text(hotkey_hint(&self.config.hotkeys.record)).clicked() {
                        self.handle(&ctx, Action::ToggleRecording);
                    }
                }
                RecState::Active { .. } => {
                    let stop = Button::new(RichText::new(tr!("Detener Grabación", "Stop Recording")).color(Color32::WHITE)).fill(PRIMARY);
                    if ui.add_sized([full - height - 4.0, height], stop).on_hover_text(hotkey_hint(&self.config.hotkeys.record)).clicked() {
                        self.handle(&ctx, Action::ToggleRecording);
                    }
                    let paused = self.is_paused();
                    let tip = if paused { tr!("Reanudar Grabación", "Resume Recording") } else { tr!("Pausar Grabación", "Pause Recording") };
                    if icon_button(ui, if paused { Icon::Play } else { Icon::Pause }, height, self.is_live(), paused, tip)
                        .on_hover_text(format!("{tip} {}", hotkey_hint(&self.config.hotkeys.pause)))
                        .clicked()
                    {
                        self.handle(&ctx, Action::TogglePause);
                    }
                }
                RecState::Saving(_) => {
                    ui.add_enabled(false, Button::new(tr!("Guardando…", "Saving…")).min_size(vec2(full, height)));
                }
            }
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            let hint = hotkey_hint(&self.config.hotkeys.replay);
            let saving = matches!(self.rec, RecState::Saving(_));
            if self.is_replaying() {
                let stop = Button::new(RichText::new(tr!("Detener búfer de repetición", "Stop Replay Buffer")).color(Color32::WHITE)).fill(PRIMARY);
                if ui.add_sized([full - height - 4.0, height], stop).on_hover_text(hint).clicked() {
                    self.handle(&ctx, Action::ToggleReplay);
                }
                let tip = format!("{} {}", tr!("Guardar repetición", "Save Replay"), hotkey_hint(&self.config.hotkeys.save_replay));
                if icon_button(ui, Icon::Save, height, true, false, &tip).on_hover_text(tip).clicked() {
                    self.handle(&ctx, Action::SaveReplay);
                }
            } else {
                let start = Button::new(tr!("Iniciar búfer de repetición", "Start Replay Buffer"));
                let tip = tr!("Guarda en memoria los últimos {} s, para guardarlos cuando quieras. {}", "Keeps the last {} s in memory, to save whenever you want. {}", self.config.replay_seconds, hint);
                if ui.add_enabled_ui(!saving, |ui| ui.add_sized([full, height], start)).inner.on_hover_text(tip).clicked() {
                    self.handle(&ctx, Action::ToggleReplay);
                }
            }
        });
        ui.add_space(2.0);
        if ui.add_sized([full, height], Button::new(tr!("Ajustes", "Settings"))).clicked() {
            self.open_settings();
        }
        ui.add_space(2.0);
        if ui.add_sized([full, height], Button::new(tr!("Salir", "Exit"))).clicked() {
            self.quit(&ctx);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        if let Some(status) = &self.status {
            let color = if status.error { RED } else { TEXT_MUTED };
            let label = ui.add(
                egui::Label::new(RichText::new(&status.text).small().color(color)).truncate().sense(Sense::click()),
            );
            let label = label.on_hover_text(&status.text);
            if let Some(file) = &status.file && label.clicked() {
                crate::platform::sys::show_in_folder(file);
            }
        }
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                self.recording_indicator(ui);
                ui.label(RichText::new(format!("CPU: {:.1}%", self.cpu.percent)).small().color(TEXT_MUTED));
                if let Some(release) = &self.update {
                    let text = tr!("Nueva versión {} disponible", "New version {} available", release.version);
                    let link = ui.add(egui::Label::new(RichText::new(text).small().color(PRIMARY_LIGHT).underline()).sense(Sense::click()));
                    if link.on_hover_text(&release.url).clicked() {
                        open_url(&release.url);
                    }
                }
            });
        });
    }

    fn recording_indicator(&self, ui: &mut egui::Ui) {
        if self.is_replaying() {
            ui.label(RichText::new(tr!("Búfer de repetición activo", "Replay buffer active")).small().color(PRIMARY_LIGHT));
        }
        let (Some(session), RecState::Active { recording: Some(_), .. }) = (self.active_session(), &self.rec) else {
            status_icon(ui, Icon::Record, GREY3, "00:00:00");
            return;
        };
        let shared = &session.shared;
        if !shared.encoder_ready.load(Ordering::Acquire) {
            // Hardware encoder start-up takes 0.2–1 s (more when cold); nothing is recorded yet.
            ui.label(RichText::new(tr!("Preparando…", "Starting…")).small().color(TEXT_MUTED));
            ui.spinner();
            return;
        }
        let (recorded, paused) = {
            let outputs = shared.outputs.lock().unwrap();
            (outputs.recorded(), outputs.file_paused())
        };
        let secs = recorded.as_secs();
        let time = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
        if paused {
            status_icon(ui, Icon::Pause, YELLOW, &time);
        } else {
            status_icon(ui, Icon::Record, RED, &time);
        }
        if let Some((encoded, lagged)) = shared.encoder.lock().unwrap().as_ref().map(|e| e.stats())
            && lagged > 0
        {
            let pct = lagged as f64 * 100.0 / (encoded + lagged).max(1) as f64;
            ui.label(RichText::new(tr!("Fotogramas perdidos {} ({:.1}%)", "Dropped frames {} ({:.1}%)", lagged, pct)).small().color(YELLOW))
                .on_hover_text(tr!("El equipo no da abasto: prueba a bajar la resolución o los FPS en Ajustes › Vídeo", "This computer can't keep up: try a lower resolution or FPS in Settings › Video"));
        }
    }

    // ------------------------------------------------------------------ settings dialog

    fn settings_dialog(&mut self, ctx: &egui::Context) {
        let recording = self.is_recording() || matches!(self.rec, RecState::Saving(_));
        let replaying = self.is_replaying();
        let source_size = self.active_session().or(self.preview.as_ref()).and_then(Session::source_size);
        let Some(dialog) = &mut self.settings else { return };
        let language = dialog.draft.language.unwrap_or_else(Language::system);
        let text = |ru, en, es| match language { Language::Russian => ru, Language::English => en, Language::Spanish => es };
        // Read before the dialog runs: Escape there ends the wait for a hotkey, and must not then
        // also close the dialog.
        let capturing = dialog.capturing.is_some();
        let mut decision = None;
        let response = egui::Modal::new(egui::Id::unique("settings"))
            .frame(egui::Frame::new().fill(GREY7).stroke(Stroke::new(1.0, GREY4)).corner_radius(6).inner_margin(12))
            .show(ctx, |ui| {
                let width = (ctx.content_rect().width() - 48.0).clamp(250.0, 370.0);
                let body = (ctx.content_rect().height() - 190.0).clamp(180.0, 560.0);
                ui.set_width(width);
                ui.style_mut().text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
                ui.style_mut().text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
                ui.style_mut().text_styles.insert(egui::TextStyle::Small, FontId::proportional(12.0));
                ui.spacing_mut().interact_size.y = 30.0;
                ui.spacing_mut().scroll = egui::style::ScrollStyle::solid();
                ui.heading(text("Настройки", "Settings", "Ajustes"));
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    for (page, name) in [
                        (SettingsPage::General, text("Общие", "General", "General")),
                        (SettingsPage::Output, text("Запись", "Recording", "Grabación")),
                        (SettingsPage::Audio, text("Звук", "Audio", "Audio")),
                        (SettingsPage::Video, text("Видео", "Video", "Vídeo")),
                        (SettingsPage::Hotkeys, text("Клавиши", "Hotkeys", "Atajos")),
                    ] {
                        if ui.selectable_label(dialog.page == page, name).clicked() {
                            (dialog.page, dialog.capturing) = (page, None);
                        }
                    }
                });
                ui.separator();
                let animation_id = egui::Id::new("settings-page-transition");
                let now = ctx.input(|input| input.time);
                let progress = ctx.data_mut(|data| {
                    let page = dialog.page as u8;
                    let (previous, started) = data.get_temp::<(u8, f64)>(animation_id).unwrap_or((page, now - 1.0));
                    let started = if previous == page { started } else { now };
                    data.insert_temp(animation_id, (page, started));
                    ((now - started) / 0.14).clamp(0.0, 1.0) as f32
                });
                if progress < 1.0 { ctx.request_repaint_after(Duration::from_millis(16)); }
                if recording {
                    ui.colored_label(YELLOW, text("Остановите запись, чтобы изменить настройки.", "Stop recording to change settings.", "Detén la grabación para cambiar los ajustes."));
                } else if replaying {
                    ui.colored_label(YELLOW, text("После сохранения буфер отката начнётся заново.", "Saving restarts the replay buffer.", "Al guardar se reinicia el búfer de repetición."));
                }
                ui.add_enabled_ui(!recording, |ui| {
                    ui.set_opacity(progress);
                    egui::ScrollArea::vertical().id_salt(("settings-page", dialog.page as u8)).min_scrolled_height(body).max_height(body).auto_shrink([false, false]).show(ui, |ui| {
                        ui.set_max_width(width - 16.0);
                        if dialog.page == SettingsPage::Output {
                            storage_panel(ui, &mut self.storage_panel, &dialog.draft.output_dir, &self.config.gallery_files, language);
                            ui.add_space(12.0);
                        }
                        compact_settings_page(ui, dialog, &self.system_devices, &self.mic_devices, &self.sources, source_size);
                    });
                });
                ui.add_space(10.0);
                let replay_duration = parse_replay_duration(&dialog.replay_duration);
                if replay_duration.is_none() {
                    ui.colored_label(RED, text("Длительность отката: укажите время от 00:01 до 1440:00.", "Replay duration must be between 00:01 and 1440:00.", "La duración debe estar entre 00:01 y 1440:00."));
                }
                let name_error = video_name(&dialog.draft.video_name_template, chrono::Local::now().naive_local()).err();
                if let Some(error) = &name_error {
                    ui.colored_label(RED, format!("{}: {error}", text("Шаблон имени", "Name template", "Plantilla de nombre")));
                }
                let path_error = invalid_output_path(&dialog.draft);
                if let Some(label) = path_error {
                    let label = match label {
                        "Папка файлов" => text("Папка файлов", "Media folder", "Carpeta de archivos"),
                        _ => label,
                    };
                    ui.colored_label(RED, format!("{label}: {}", text("укажите полный путь к папке.", "enter an absolute folder path.", "introduce la ruta completa de la carpeta.")));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(Button::new(tr!("Cancelar", "Cancel")).min_size(vec2(90.0, 26.0))).clicked() {
                        decision = Some(false);
                    }
                    let ok = Button::new(RichText::new(tr!("Aceptar", "OK")).color(Color32::WHITE)).fill(PRIMARY);
                    if ui.add_enabled(!recording && name_error.is_none() && path_error.is_none() && replay_duration.is_some(), ok.min_size(vec2(90.0, 26.0))).clicked() {
                        decision = Some(true);
                    }
                });
            });
        if response.should_close() && decision.is_none() && !capturing {
            decision = Some(false);
        }
        match decision {
            Some(true) => {
                let dialog = self.settings.take().expect("dialog is open");
                let mut draft = dialog.draft;
                draft.replay_seconds = parse_replay_duration(&dialog.replay_duration).expect("valid replay duration");
                let source_change = dialog.selected_source.filter(|id| Some(*id) != self.selected && self.sources.iter().any(|source| source.id == *id));
                let changed = source_change.is_some() || serde_json::to_value(&draft).ok() != serde_json::to_value(&self.config).ok();
                let restart_replay = changed && self.is_replaying();
                if restart_replay {
                    self.stop_replay(ctx);
                }
                self.apply_settings(draft);
                if let Some(source) = source_change {
                    self.select_source(ctx, source);
                    self.config.save();
                }
                self.bind_hotkeys();
                if restart_replay {
                    self.start_replay(ctx);
                }
            }
            Some(false) => {
                self.settings = None;
                self.bind_hotkeys();
            }
            None => {}
        }
    }

    fn setup_dialog(&mut self, ctx: &egui::Context) {
        let language = self.config.language.unwrap_or_else(Language::system);
        let text = |ru, en, es| match language { Language::Russian => ru, Language::English => en, Language::Spanish => es };
        if self.config.setup_complete { return; }
        let mut finish = false;
        egui::Modal::new(egui::Id::unique("first-run-setup"))
            .frame(egui::Frame::new().fill(GREY7).stroke(Stroke::new(1.0, GREY4)).corner_radius(6).inner_margin(18))
            .show(ctx, |ui| {
                ui.set_width(355.0);
                ui.heading(text("Начальная настройка opnoverlay", "Set up opnoverlay", "Configurar opnoverlay"));
                ui.add_space(8.0);
                ui.label(text("Выберите, где хранить последние минуты записи. Способ можно изменить в настройках.", "Choose where to keep recent recording data. You can change this in settings.", "Elige dónde guardar los últimos minutos de grabación. Puedes cambiarlo en ajustes."));
                ui.add_space(8.0);
                ui.radio_value(&mut self.config.replay_storage, ReplayStorage::Memory, text("Оперативная память — меньше обращений к диску", "Memory — fewer disk writes", "Memoria RAM — menos escrituras en disco"));
                ui.label(text("Подходит, если памяти достаточно. Заполненный буфер занимает её до выключения откатов.", "Suitable when enough RAM is available. The buffer uses memory until replay is disabled.", "Adecuado si hay suficiente RAM. El búfer usa memoria hasta desactivar la repetición."));
                ui.add_space(6.0);
                ui.radio_value(&mut self.config.replay_storage, ReplayStorage::Disk, text("Временные файлы — меньше расход оперативной памяти", "Temporary files — less RAM usage", "Archivos temporales — menos uso de RAM"));
                ui.label(format!("{} {}", text("Подходит для длинных откатов. Временная папка:", "Suitable for long replays. Temporary folder:", "Adecuado para repeticiones largas. Carpeta temporal:"), std::env::temp_dir().display()));
                ui.add_space(8.0);
                ui.label(text("По умолчанию: 45 секунд в качестве «Максимум деталей» при 1080p и 60 FPS — около 136 МБ для видео и звука. Приложению требуется дополнительная память; фактический размер зависит от записи.", "Default: 45 seconds at maximum detail, 1080p and 60 FPS use around 136 MB for video and audio. Additional memory is required; actual size varies.", "Por defecto: 45 segundos con máximo detalle, 1080p y 60 FPS usan unos 136 MB de vídeo y audio. Se necesita memoria adicional; el tamaño real varía."));
                ui.add_space(8.0);
                ui.checkbox(&mut self.config.auto_start_replay, text("Включать откаты при запуске приложения", "Enable replay when the app starts", "Activar repetición al iniciar la aplicación"));
                if cfg!(windows) {
                    ui.checkbox(&mut self.config.auto_start_windows, text("Запускать opnoverlay при входе в Windows", "Start opnoverlay when signing in to Windows", "Iniciar opnoverlay al entrar en Windows"));
                }
                ui.add_space(10.0);
                if ui.button(text("Сохранить и продолжить", "Save and continue", "Guardar y continuar")).clicked() { finish = true; }
            });
        if finish {
            if cfg!(windows) && let Err(error) = crate::platform::sys::set_startup(self.config.auto_start_windows) {
                self.config.auto_start_windows = crate::platform::sys::startup_enabled().unwrap_or(false);
                self.set_status(tr3!("No se pudo cambiar el inicio automático de Windows: {}", "Could not change Windows startup: {}", "Не удалось изменить автозапуск Windows: {}", error), None, true);
            }
            self.config.setup_complete = true;
            self.config.save();
            if self.config.auto_start_replay { self.start_replay(ctx); }
        }
    }

    /// Waits for the settings dialog to close: OK there would save its older copy of the config.
    fn update_dialog(&mut self, ctx: &egui::Context) {
        let Some(release) = self.update.as_ref().filter(|_| self.update_dialog && self.settings.is_none()) else { return };
        let mut skip = false;
        let response = egui::Modal::new(egui::Id::unique("update"))
            .frame(egui::Frame::new().fill(GREY7).stroke(Stroke::new(1.0, GREY4)).corner_radius(6).inner_margin(16))
            .show(ctx, |ui| {
                ui.heading(tr!("Nueva versión disponible", "New version available"));
                ui.add_space(8.0);
                ui.label(tr!(
                    "Ya está disponible obs-lite {}. Tienes la {}.",
                    "obs-lite {} is available. You have {}.",
                    release.version,
                    env!("CARGO_PKG_VERSION")
                ));
                ui.add_space(14.0);
                // The buttons, the widest row, set the dialog's width (it differs by language).
                ui.horizontal(|ui| {
                    if ui.add(Button::new(tr!("No recordar esta versión", "Don't remind me of this version")).min_size(vec2(0.0, 26.0))).clicked() {
                        skip = true;
                        ui.close();
                    }
                    if ui.add(Button::new(tr!("Cerrar", "Close")).min_size(vec2(90.0, 26.0))).clicked() {
                        ui.close();
                    }
                    let download = Button::new(RichText::new(tr!("Descargar", "Download")).color(Color32::WHITE)).fill(PRIMARY);
                    if ui.add(download.min_size(vec2(90.0, 26.0))).on_hover_text(&release.url).clicked() {
                        open_url(&release.url);
                        ui.close();
                    }
                });
            });
        if skip {
            self.config.skipped_version = Some(release.version.clone());
            self.config.save();
        }
        if response.should_close() {
            self.update_dialog = false;
        }
    }
}

/// Opens a web page in the default browser.
fn open_url(url: &str) {
    let _ = std::process::Command::new("explorer").arg(url).spawn();
}

fn parse_replay_duration(value: &str) -> Option<u32> {
    let (minutes, seconds) = value.split_once(':')?;
    if minutes.is_empty() || seconds.len() != 2 || !minutes.bytes().all(|b| b.is_ascii_digit()) || !seconds.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let minutes = minutes.parse::<u32>().ok()?;
    let seconds = seconds.parse::<u32>().ok()?;
    if seconds >= 60 { return None; }
    let total = minutes.checked_mul(60)?.checked_add(seconds)?;
    (1..=86_400).contains(&total).then_some(total)
}

#[cfg(windows)]
fn parse_trim_time(value: &str) -> Option<u64> {
    let (minutes, rest) = value.split_once(':')?;
    let (seconds, fraction) = rest.split_once('.').unwrap_or((rest, ""));
    if minutes.is_empty() || seconds.len() != 2 || !minutes.bytes().all(|b| b.is_ascii_digit()) || !seconds.bytes().all(|b| b.is_ascii_digit()) || fraction.len() > 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) || (rest.contains('.') && fraction.is_empty()) { return None; }
    let minutes = minutes.parse::<u64>().ok()?;
    let seconds = seconds.parse::<u64>().ok()?;
    if seconds >= 60 { return None; }
    let millis = if fraction.is_empty() { 0 } else { fraction.parse::<u64>().ok()? * 10u64.pow(3 - fraction.len() as u32) };
    let total = minutes.checked_mul(60_000)?.checked_add(seconds * 1000)?.checked_add(millis)?;
    (total <= 86_400_000).then_some(total)
}

#[cfg(windows)]
fn trim_time(ms: u64) -> String {
    format!("{:02}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

fn invalid_output_path(config: &Config) -> Option<&'static str> {
    (!config.output_dir.is_absolute()).then_some("Папка файлов")
}

fn viewer_header(ui: &mut egui::Ui, path: &Path, kind: &str, language: Language) -> bool {
    ui.horizontal(|ui| {
        let back = ui.add_sized([125.0, 40.0], Button::new(match language { Language::Russian => "В галерею", Language::English => "Gallery", Language::Spanish => "Galería" })).clicked();
        ui.add_space(8.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(kind).size(11.0).strong().color(TEXT_MUTED));
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let label = ui.add(egui::Label::new(RichText::new(name.as_ref()).size(15.0).strong()).truncate().sense(egui::Sense::focusable_noninteractive()));
            if label.has_focus() {
                ui.painter().rect_stroke(label.rect.expand(2.0), 2.0, Stroke::new(1.0, PRIMARY_LIGHT), egui::StrokeKind::Inside);
                label.show_tooltip_text(name.as_ref());
            }
            label.on_hover_text(name);
        });
        back
    }).inner
}

fn storage_size(bytes: u64, language: Language) -> String {
    let (value, unit) = if bytes >= 1_000_000_000 { (bytes as f64 / 1_000_000_000.0, if language == Language::Russian { "ГБ" } else { "GB" }) }
        else if bytes >= 1_000_000 { (bytes as f64 / 1_000_000.0, if language == Language::Russian { "МБ" } else { "MB" }) }
        else if bytes >= 1_000 { (bytes as f64 / 1_000.0, if language == Language::Russian { "КБ" } else { "KB" }) }
        else { (bytes as f64, if language == Language::Russian { "Б" } else { "B" }) };
    if bytes < 1_000 { format!("{value:.0} {unit}") } else { format!("{value:.1} {unit}") }
}

fn storage_panel(ui: &mut egui::Ui, state: &mut StoragePanel, folder: &Path, files: &[PathBuf], language: Language) {
    let text = |ru, en, es| match language { Language::Russian => ru, Language::English => en, Language::Spanish => es };
    if let Some(receiver) = state.loading.take() {
        match receiver.try_recv() {
            Ok(report) => state.report = Some(report),
            Err(mpsc::TryRecvError::Empty) => state.loading = Some(receiver),
            Err(mpsc::TryRecvError::Disconnected) => { state.report = None; state.failed = true; },
        }
    }
    let mut refresh = state.folder.is_none() && state.loading.is_none() && !state.failed;
    egui::Frame::new().fill(GREY6).corner_radius(6).inner_margin(10).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(text("Место и файлы", "Storage and files", "Espacio y archivos")).strong());
        ui.add_space(4.0);
        let current = state.folder.as_deref() == Some(folder);
        if let Some(report) = &state.report {
            if !current {
                ui.label(RichText::new(text("Папка изменена — обновите расчёт.", "Folder changed — refresh the estimate.", "La carpeta ha cambiado: actualiza el cálculo.")).color(YELLOW));
            } else {
                let free = report.free.map(|bytes| storage_size(bytes, language)).unwrap_or_else(|| text("недоступно", "unavailable", "no disponible").to_owned());
                ui.label(RichText::new(format!("{}: {free}", text("Свободно на диске", "Free on disk", "Libre en disco"))).strong());
                if !report.folder_exists && report.free.is_some() {
                    ui.add(egui::Label::new(RichText::new(text("Папка ещё не создана; показано место на диске родительской папки.", "Folder does not exist yet; space is shown for its parent disk.", "La carpeta aún no existe; se muestra el espacio del disco superior.")).small().color(TEXT_MUTED)).wrap());
                }
                ui.label(format!("{}: {}", text("Известные файлы", "Known files", "Archivos conocidos"), storage_size(report.total(), language)));
                ui.scope(|ui| {
                    ui.spacing_mut().interact_size.y = 18.0;
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for (name, bytes) in [
                        (text("Видео", "Videos", "Vídeos"), report.videos),
                        (text("Откаты", "Replays", "Repeticiones"), report.replays),
                        (text("Снимки", "Screenshots", "Capturas"), report.screenshots),
                        (text("Оригиналы обрезки", "Trim originals", "Originales recortados"), report.originals),
                    ] {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(name).small().color(TEXT_MUTED));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| { ui.label(RichText::new(storage_size(bytes, language)).small().color(TEXT_MUTED)); });
                        });
                    }
                });
                if report.other > 0 { ui.label(format!("{}: {}", text("Другие известные файлы", "Other known files", "Otros archivos conocidos"), storage_size(report.other, language))); }
                if report.missing > 0 || report.inaccessible > 0 {
                    ui.add(egui::Label::new(RichText::new(text("Не все файлы доступны; итог может быть неполным.", "Some files are unavailable; the total may be incomplete.", "Algunos archivos no están disponibles; el total puede estar incompleto.")).small().color(YELLOW)).wrap());
                }
                ui.add(egui::Label::new(RichText::new(text("Учтены файлы галереи и связанные оригиналы, в том числе в других папках. Тип определяется по имени; временный буфер и неизвестные файлы не учтены.", "Includes gallery files and related originals, including in other folders. Type is based on the name. Unknown files and the temporary replay buffer are excluded.", "Incluye archivos de la galería y originales relacionados en otras carpetas. El tipo se determina por el nombre. No incluye archivos desconocidos ni el búfer temporal.")).small().color(TEXT_MUTED)).wrap());
            }
        } else if state.loading.is_some() || refresh {
            ui.label(text("Считаем…", "Calculating…", "Calculando…"));
        } else {
            ui.label(text("Не удалось получить данные.", "Could not get storage information.", "No se pudo obtener información del espacio."));
        }
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(state.loading.is_none(), Button::new(text("Обновить", "Refresh", "Actualizar"))).clicked() { refresh = true; }
            let can_open = current && state.report.as_ref().is_some_and(|report| report.folder_exists);
            if ui.add_enabled(can_open, Button::new(text("Открыть папку", "Open folder", "Abrir carpeta"))).clicked() {
                crate::platform::sys::open_folder(folder);
            }
        });
    });
    if refresh && state.loading.is_none() {
        let folder = folder.to_owned();
        let files = files.to_owned();
        let (sender, receiver) = mpsc::channel();
        let ctx = ui.ctx().clone();
        state.folder = Some(folder.clone());
        state.report = None;
        state.failed = false;
        state.loading = Some(receiver);
        thread::spawn(move || {
            let _ = sender.send(crate::storage::measure(&folder, &files));
            ctx.request_repaint();
        });
    }
}

fn compact_settings_page(ui: &mut egui::Ui, dialog: &mut SettingsDialog, system: &[Device], mic: &[Device], sources: &[Source], source_size: Option<(u32, u32)>) {
    let SettingsDialog { draft: c, page, capturing, replay_duration, selected_source } = dialog;
    let width = ui.available_width().min(330.0);
    let language = c.language.unwrap_or_else(Language::system);
    let text = |ru, en, es| match language { Language::Russian => ru, Language::English => en, Language::Spanish => es };
    match page {
        SettingsPage::General => {
            group_title(ui, text("Общие", "General", "General"));
            let language_label = ui.label(text("Язык", "Language", "Idioma"));
            let options = [None, Some(Language::Russian), Some(Language::English), Some(Language::Spanish)];
            combo(ui, "language", &mut c.language, &options, |language| language.map_or(text("Автоматически", "Automatic", "Automático"), Language::name).to_owned(), width).labelled_by(language_label.id);
            ui.checkbox(&mut c.minimize_to_tray, text("Сворачивать в системный трей", "Minimize to system tray", "Minimizar a la bandeja"));
            ui.checkbox(&mut c.notifications, text("Показывать уведомления", "Show notifications", "Mostrar notificaciones"));
            if cfg!(windows) {
                ui.checkbox(&mut c.auto_start_windows, text("Запускать вместе с Windows", "Start with Windows", "Iniciar con Windows"));
                ui.checkbox(&mut c.hide_from_capture, text("Скрывать оверлей в записи", "Hide overlay in recordings", "Ocultar el panel en la grabación"));
                ui.checkbox(&mut c.hide_notifications_from_capture, text("Скрывать уведомления в записи", "Hide notifications in recordings", "Ocultar notificaciones en la grabación"));
            }
            ui.add_space(8.0);
            group_title(ui, text("Показатели", "Metrics", "Métricas"));
            ui.horizontal_wrapped(|ui| {
                if cfg!(windows) { ui.checkbox(&mut c.hud_fps, "FPS"); }
                ui.checkbox(&mut c.hud_cpu, "CPU");
                ui.checkbox(&mut c.hud_memory, text("ОЗУ", "RAM", "RAM"));
                if cfg!(windows) {
                    ui.checkbox(&mut c.hud_gpu, "GPU");
                    ui.checkbox(&mut c.hud_gpu_temperature, text("Температура", "Temperature", "Temperatura"));
                    ui.checkbox(&mut c.hud_video_memory, text("Видеопамять", "Video memory", "Memoria de vídeo"));
                }
            });
            let corner_label = ui.label(text("Положение показателей", "Metrics position", "Posición de métricas"));
            combo(ui, "hud-corner", &mut c.hud_corner, &HudCorner::ALL, |corner| match corner {
                HudCorner::TopLeft => text("Сверху слева", "Top left", "Arriba a la izquierda"),
                HudCorner::TopRight => text("Сверху справа", "Top right", "Arriba a la derecha"),
                HudCorner::BottomLeft => text("Снизу слева", "Bottom left", "Abajo a la izquierda"),
                HudCorner::BottomRight => text("Снизу справа", "Bottom right", "Abajo a la derecha"),
            }.to_owned(), width).labelled_by(corner_label.id);
        }
        SettingsPage::Output => {
            group_title(ui, text("Откат", "Replay", "Repetición"));
            let duration_label = ui.label(text("Длительность отката · минуты:секунды", "Replay duration · minutes:seconds", "Duración · minutos:segundos"));
            ui.add(egui::TextEdit::singleline(replay_duration).desired_width(100.0).char_limit(8)).labelled_by(duration_label.id);
            ui.label(RichText::new(text("Например: 00:45 или 01:30", "Example: 00:45 or 01:30", "Ejemplo: 00:45 o 01:30")).small().color(TEXT_MUTED));
            ui.add(egui::Label::new(RichText::new(text("Файл может быть на несколько секунд длиннее для корректного воспроизведения.", "The file may be a few seconds longer to play correctly.", "El archivo puede durar unos segundos más para reproducirse correctamente.")).small().color(TEXT_MUTED)).wrap());
            ui.checkbox(&mut c.auto_start_replay, text("Включать откаты при запуске", "Enable replay at startup", "Activar repetición al iniciar"));
            let storage_label = ui.label(text("Где хранить буфер", "Replay buffer storage", "Almacenamiento del búfer"));
            ComboBox::from_id_salt("replay-storage").width(width).selected_text(match c.replay_storage {
                ReplayStorage::Memory => text("Оперативная память", "Memory", "Memoria RAM"),
                ReplayStorage::Disk => text("Временные файлы на диске", "Temporary disk files", "Archivos temporales en disco"),
            }).show_ui(ui, |ui| {
                ui.selectable_value(&mut c.replay_storage, ReplayStorage::Memory, text("Оперативная память", "Memory", "Memoria RAM"));
                ui.selectable_value(&mut c.replay_storage, ReplayStorage::Disk, text("Временные файлы на диске", "Temporary disk files", "Archivos temporales en disco"));
            }).response.labelled_by(storage_label.id);
            if let (Some(source), Some(seconds)) = (source_size, parse_replay_duration(replay_duration)) {
                let input = c.region.and_then(|region| region.fit(source)).map_or(source, |region| (region.width, region.height));
                let (w, h) = c.resolution.apply(input);
                let estimate = (u64::from(c.quality.selected_bitrate(c.custom_bitrate_mbps, w, h, c.fps)) + 192_000) * u64::from(seconds) / 8_000_000;
                ui.add(egui::Label::new(match language {
                    Language::Russian => format!("Примерно {estimate} МБ; фактический размер зависит от записи."),
                    Language::English => format!("About {estimate} MB; actual size depends on the recording."),
                    Language::Spanish => format!("Unos {estimate} MB; el tamaño real depende de la grabación."),
                }).wrap());
            }
            ui.add_space(10.0);
            group_title(ui, text("Файлы и качество", "Files and quality", "Archivos y calidad"));
            for (label, folder) in [(text("Общая папка файлов", "Media folder", "Carpeta de archivos"), &mut c.output_dir)] {
                let folder_label = ui.label(label);
                let shown = crate::platform::sys::display_path(folder);
                let mut path = shown.display().to_string();
                let editable = shown == *folder;
                if ui.add(egui::TextEdit::singleline(&mut path).desired_width(width).interactive(editable)).labelled_by(folder_label.id).changed() { *folder = PathBuf::from(path); }
                let choose_folder = ui.button(text("Выбрать папку…", "Choose folder…", "Elegir carpeta…"));
                choose_folder.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, choose_folder.enabled(), format!("{}: {label}", text("Выбрать папку", "Choose folder", "Elegir carpeta"))));
                if choose_folder.clicked() && let Some(dir) = rfd::FileDialog::new().set_directory(&*folder).pick_folder() { *folder = dir; }
            }
            ui.label(RichText::new(text("Откаты и скриншоты: дата и время в начале имени. Сортируйте по имени, чтобы новые файлы были в конце.", "Replays and screenshots start with the date and time. Sort by name to see the newest files at the end.", "Las repeticiones y capturas empiezan por fecha y hora. Ordena por nombre para ver los archivos más recientes al final.")).small().color(TEXT_MUTED));
            let video_name_label = ui.label(text("Имя обычной записи", "Recording name", "Nombre de la grabación"));
            ui.add(egui::TextEdit::singleline(&mut c.video_name_template).desired_width(width)).labelled_by(video_name_label.id);
            ui.label(RichText::new("[ГГГГ-ММ-ДД]  [ЧЧ-ММ]  [ЧЧ-ММ-СС]").small().color(TEXT_MUTED));
            let quality_label = ui.label(text("Качество видео", "Video quality", "Calidad de vídeo"));
            combo(ui, "quality", &mut c.quality, &Quality::ALL, |quality| match quality {
                Quality::Low => text("Экономия места", "Save space", "Ahorrar espacio"),
                Quality::Medium => text("Баланс", "Balanced", "Equilibrado"),
                Quality::High => text("Максимум деталей", "Maximum detail", "Máximo detalle"),
                Quality::Custom => text("Свои настройки", "Custom", "Personalizado"),
            }.into(), width).labelled_by(quality_label.id);
            if c.quality == Quality::Custom { ui.add(egui::Slider::new(&mut c.custom_bitrate_mbps, 1..=80).suffix(text(" Мбит/с", " Mbps", " Mbps"))).accessible_name(text("Битрейт видео", "Video bitrate", "Tasa de bits de vídeo")); }
            if let Some(source) = source_size {
                let input = c.region.and_then(|region| region.fit(source)).map_or(source, |region| (region.width, region.height));
                let (w, h) = c.resolution.apply(input);
                let per_minute = (c.quality.selected_bitrate(c.custom_bitrate_mbps, w, h, c.fps) as f64 + 192_000.0) * 60.0 / 8_000_000.0;
                ui.label(match language {
                    Language::Russian => format!("Оценка размера: ≈ {per_minute:.0} МБ/мин · {:.1} ГБ/час", per_minute * 60.0 / 1000.0),
                    Language::English => format!("Estimated size: ≈ {per_minute:.0} MB/min · {:.1} GB/hour", per_minute * 60.0 / 1000.0),
                    Language::Spanish => format!("Tamaño estimado: ≈ {per_minute:.0} MB/min · {:.1} GB/hora", per_minute * 60.0 / 1000.0),
                });
                ui.add(egui::Label::new(RichText::new(text("Приблизительно для текущего качества, разрешения и FPS. Фактический размер может отличаться.", "Approximate for the current quality, resolution and FPS. Actual size may vary.", "Estimación según calidad, resolución y FPS. El tamaño real puede variar.")).small().color(TEXT_MUTED)).wrap());
            } else {
                ui.add(egui::Label::new(RichText::new(text("Оценка размера появится, когда разрешение источника будет доступно.", "The size estimate appears when the source resolution is available.", "La estimación aparece cuando se conoce la resolución de la fuente.")).small().color(TEXT_MUTED)).wrap());
            }
            let screenshot_label = ui.label(text("Формат скриншота", "Screenshot format", "Formato de captura"));
            combo(ui, "screenshot-format", &mut c.screenshot_format, &ScreenshotFormat::ALL, |format| format.label().into(), width).labelled_by(screenshot_label.id);
            if c.screenshot_format == ScreenshotFormat::Jpeg { ui.add(egui::Slider::new(&mut c.jpeg_quality, 1..=100).text(text("Качество JPEG", "JPEG quality", "Calidad JPEG"))); }
            ui.label(RichText::new(text("Видео: MP4 · H.264", "Video: MP4 · H.264", "Vídeo: MP4 · H.264")).small().color(TEXT_MUTED));
        }
        SettingsPage::Audio => {
            group_title(ui, text("Звук", "Audio", "Audio"));
            let system_label = ui.label(text("Звук рабочего стола", "Desktop audio", "Audio del escritorio"));
            device_combo(ui, "sysdev", &mut c.system_audio, &mut c.system_device, system).labelled_by(system_label.id);
            if audio_overlap(c) { ui.colored_label(YELLOW, text("Звук выбранного приложения может записаться дважды.", "The selected application may be recorded twice.", "La aplicación seleccionada podría grabarse dos veces.")); }
            let mic_label = ui.label(text("Микрофон", "Microphone", "Micrófono"));
            device_combo(ui, "micdev", &mut c.mic, &mut c.mic_device, mic).labelled_by(mic_label.id);
            ui.checkbox(&mut c.mic_denoise, text("Подавление шума микрофона", "Microphone noise suppression", "Reducción de ruido del micrófono"));
        }
        SettingsPage::Video => {
            group_title(ui, text("Видео", "Video", "Vídeo"));
            let source_label = ui.label(text("Экран или окно", "Screen or window", "Pantalla o ventana"));
            let selected = sources.iter().find(|source| Some(source.id) == *selected_source).map_or(text("Не выбран", "Not selected", "No seleccionado"), |source| source.label.as_str());
            ComboBox::from_id_salt("capture-source").width(width).truncate().selected_text(selected).show_ui(ui, |ui| {
                for source in sources {
                    let selected = *selected_source == Some(source.id);
                    let option = ui.add_sized([(width - 16.0).max(1.0), 0.0], Button::selectable(selected, (source.label.as_str(), egui::Atom::grow())).truncate());
                    if option.has_focus() { option.show_tooltip_text(&source.label); }
                    if option.on_hover_text(&source.label).clicked() {
                        *selected_source = Some(source.id);
                        ui.close();
                    }
                }
            }).response.labelled_by(source_label.id);
            ui.label(text("Выберите экран, чтобы записывать переключение между игрой и рабочим столом.", "Choose a screen to record switching between the game and desktop.", "Elige una pantalla para grabar el cambio entre el juego y el escritorio."));
            let resolution_label = ui.label(text("Разрешение", "Resolution", "Resolución"));
            combo(ui, "res", &mut c.resolution, &Resolution::ALL, |resolution| resolution.label().into(), width).labelled_by(resolution_label.id);
            let fps_label = ui.label(text("Частота кадров", "Frame rate", "Frecuencia de fotogramas"));
            ui.add(egui::DragValue::new(&mut c.fps).range(1..=240).suffix(" FPS")).labelled_by(fps_label.id);
            if c.fps > 60 { ui.label(text("Высокая частота увеличивает нагрузку и размер записи.", "Higher frame rates increase load and file size.", "Una frecuencia alta aumenta la carga y el tamaño del archivo.")); }
        }
        SettingsPage::Hotkeys => {
            group_title(ui, text("Горячие клавиши", "Hotkeys", "Atajos de teclado"));
            let Hotkeys { menu, record, pause, screenshot, mute_mic, replay, save_replay } = &mut c.hotkeys;
            let rows = [(text("Открыть или закрыть меню", "Open or close menu", "Abrir o cerrar el menú"), menu), (text("Начать или остановить запись", "Start or stop recording", "Iniciar o detener grabación"), record), (text("Пауза записи", "Pause recording", "Pausar grabación"), pause), (text("Включить или выключить откаты", "Enable or disable replay", "Activar o desactivar repetición"), replay), (text("Сохранить откат", "Save replay", "Guardar repetición"), save_replay), (text("Скриншот", "Screenshot", "Captura de pantalla"), screenshot), (text("Микрофон", "Microphone", "Micrófono"), mute_mic)];
            let mut slots = Vec::new();
            let mut set = None;
            for (i, (label, slot)) in rows.into_iter().enumerate() {
                ui.label(label);
                if hotkey_field(ui, i, slot, capturing, label) { set = Some(i); }
                slots.push(slot);
            }
            if let Some(row) = set { take_from_others(&mut slots, row); }
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        if self.notification.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
        }
        self.advance_window_animation(ctx, Instant::now());
        #[cfg(windows)]
        {
            let focused = ctx.input(|i| i.viewport().focused);
            let external_foreground = focused == Some(false) && crate::platform::sys::foreground_is_external();
            self.handle_focus_change(ctx, focused, external_foreground);
        }
        #[cfg(windows)]
        if self.hidden && self.closing.is_none() && let Some(window) = self.native_window {
            crate::platform::sys::move_minimized_offscreen(window);
        }
        if self.config.hud_enabled && self.config.setup_complete {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaintWhileHidden);
        }
        let (monitor, current) = ctx.input(|i| (i.viewport().monitor_size, i.viewport().inner_rect.map(|rect| rect.size())));
        if let Some(monitor) = monitor {
            #[cfg(windows)]
            let width: f32 = if self.player.is_some() { 1100.0 } else if self.advanced_view { 740.0 } else { 420.0 };
            #[cfg(not(windows))]
            let width: f32 = if self.advanced_view { 740.0 } else { 420.0 };
            #[cfg(windows)]
            let desired = if self.player.is_some() {
                let width = width.min(monitor.x);
                vec2(width, (width * 9.0 / 16.0 + 340.0).min(monitor.y))
            } else if let Some(viewer) = &self.image_viewer {
                image_viewport_size(monitor, viewer.texture.as_ref().map(TextureHandle::size_vec2))
            } else if self.advanced_view {
                let width = width.min(monitor.x);
                vec2(width, (width * 9.0 / 16.0 + DOCK_HEIGHT + 100.0).min(monitor.y))
            } else { vec2(width.min(monitor.x), monitor.y) };
            #[cfg(not(windows))]
            let desired = if let Some(viewer) = &self.image_viewer {
                image_viewport_size(monitor, viewer.texture.as_ref().map(TextureHandle::size_vec2))
            } else if self.advanced_view {
                let width = width.min(monitor.x);
                vec2(width, (width * 9.0 / 16.0 + DOCK_HEIGHT + 100.0).min(monitor.y))
            } else { vec2(width.min(monitor.x), monitor.y) };
            if current.is_some_and(|size| (size.x - desired.x).abs() > 2.0 || (size.y - desired.y).abs() > 2.0) {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(desired));
                #[cfg(windows)]
                let position = if self.player.is_some() || self.image_viewer.is_some() { pos2((monitor.x - desired.x) / 2.0, (monitor.y - desired.y) / 2.0) } else { pos2(0.0, 0.0) };
                #[cfg(not(windows))]
                let position = if self.image_viewer.is_some() { pos2((monitor.x - desired.x) / 2.0, (monitor.y - desired.y) / 2.0) } else { pos2(0.0, 0.0) };
                #[cfg(windows)]
                let media_open = self.player.is_some() || self.image_viewer.is_some();
                #[cfg(not(windows))]
                let media_open = self.image_viewer.is_some();
                if media_open && self.closing.is_none() { self.opening = None; }
                if self.opening.is_none() && self.closing.is_none() { ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(position)); }
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.exit_requested && self.tray.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(ctx);
        }
        let hidden = preview_hidden(ctx.input(|i| i.viewport().visible()), self.last_drawn.elapsed());
        if let Some(session) = self.active_session().or(self.preview.as_ref()) {
            session.shared.preview_hidden.store(hidden, Ordering::Relaxed);
        }
        // The meters fall only while drawn: hidden, they would keep the window waking 15 times a
        // second. They start again from silence when it shows.
        if hidden {
            self.meters = [0.0; 3];
        }
        ctx.request_repaint_after(repaint_interval(hidden, self.active_session().is_some(), &self.meters));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.last_drawn = Instant::now();
        let ctx = ui.ctx().clone();
        self.show_notification(&ctx);
        self.show_performance_hud(&ctx);
        #[cfg(windows)]
        let trim_open = self.pending_trim.is_some();
        #[cfg(not(windows))]
        let trim_open = false;
        if self.settings.is_none() && self.pending_delete.is_none() && self.pending_rename.is_none() && !trim_open && !self.picking_region && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.escape_navigation(&ctx);
        }
        #[cfg(windows)]
        if self.player.is_none() && let Some((path, _, end)) = &self.pending_trim && end == "auto" {
            let path = path.clone();
            match self.native_window.ok_or_else(|| "Окно оверлея недоступно.".to_owned()).and_then(|parent| crate::platform::player::Player::open(parent, &path)) {
                Ok(player) => {
                    self.trim_frames.clear();
                    self.trim_loading = None;
                    self.trim_requested = false;
                    self.trim_error = None;
                    self.player = Some((path, player, 1.0));
                }
                Err(error) => {
                    self.pending_trim = None;
                    self.set_status(tr3!("No se pudo abrir el vídeo: {}", "Could not open the video: {}", "Не удалось открыть видео: {}", error), None, true);
                }
            }
        }
        #[cfg(windows)]
        let showing_player = self.player.is_some();
        #[cfg(not(windows))]
        let showing_player = false;
        #[cfg(windows)]
        if showing_player { self.player_view(ui); }
        let showing_image = self.image_viewer.is_some();
        if showing_image { self.image_view(ui); }
        if showing_player || showing_image {
        } else if self.advanced_view {
            self.update_texture(&ctx);
            self.update_camera_texture(&ctx);
            let language = self.config.language.unwrap_or_else(Language::system);
            let text = |ru, en, es| match language { Language::Russian => ru, Language::English => en, Language::Spanish => es };
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(GREY8).inner_margin(Margin::same(12)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button(text("Назад", "Back", "Volver")).clicked() { self.advanced_view = false; }
                        ui.heading(text("Экран и звук", "Screen and audio", "Pantalla y sonido"));
                    });
                    ui.label(RichText::new(text("Выберите, что записывать, и настройте громкость звука.", "Choose what to record and adjust audio levels.", "Elige qué grabar y ajusta el volumen.")).size(12.0).color(TEXT_MUTED));
                    ui.add_space(6.0);
                    let preview_height = (ui.available_height() - DOCK_HEIGHT - 75.0).clamp(100.0, (ui.available_width() * 9.0 / 16.0).max(100.0));
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), preview_height), Layout::top_down(Align::Min), |ui| self.preview_area(ui));
                    ui.add_space(10.0);
                    ui.allocate_ui_with_layout(vec2(ui.available_width(), DOCK_HEIGHT), Layout::top_down(Align::Min), |ui| self.docks(ui));
                    ui.add_space(6.0);
                    self.status_bar(ui);
                });
        } else {
            self.overlay_home(ui);
        }
        self.settings_dialog(&ctx);
        self.update_dialog(&ctx);
        self.delete_pending_file(&ctx);
        self.rename_dialog(&ctx);
        self.setup_dialog(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(path) = finish_on_exit(std::mem::replace(&mut self.rec, RecState::Idle)) {
            add_gallery_file(&mut self.config.gallery_files, &path);
        }
        for receiver in self.screenshots.drain(..) {
            if let Ok(Ok(path)) = receiver.recv() {
                add_gallery_file(&mut self.config.gallery_files, &path);
            }
        }
        for receiver in self.gallery_deletions.drain(..) {
            if let Ok((path, Ok(()))) = receiver.recv_timeout(Duration::from_secs(10)) {
                self.config.gallery_files.retain(|item| item != &path);
            }
        }
        for job in self.jobs.drain(..) {
            if let Ok(status) = job.join()
                && !status.error
                && let Some(path) = status.file
            {
                add_gallery_file(&mut self.config.gallery_files, &path);
            }
        }
        self.config.save();
    }
}

fn load_gallery_image(path: &Path) -> Result<egui::ColorImage, String> {
    let reader = image::ImageReader::open(path).map_err(|error| error.to_string())?;
    let image = reader.with_guessed_format().map_err(|error| error.to_string())?.decode().map_err(|error| error.to_string())?;
    let image = if image.width() > 4096 || image.height() > 4096 { image.resize(4096, 4096, image::imageops::FilterType::Triangle) } else { image };
    let rgba = image.into_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

#[cfg(windows)]
fn image_viewport_size(monitor: Vec2, image: Option<Vec2>) -> Vec2 {
    let max_width = (monitor.x - 64.0).max(360.0).min(monitor.x).min(1000.0);
    let max_height = (monitor.y - 64.0).max(500.0).min(monitor.y);
    let aspect = image.filter(|size| size.x > 0.0 && size.y > 0.0).map_or(16.0 / 9.0, |size| size.x / size.y);
    let image_width = (max_width - 36.0).min((max_height - 100.0).max(1.0) * aspect);
    let width = (image_width + 36.0).max(360.0).min(max_width);
    let height = ((width - 36.0).max(1.0) / aspect + 100.0).max(500.0).min(max_height);
    vec2(width, height)
}

fn player_time(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    if seconds >= 3600 { format!("{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60) }
    else { format!("{}:{:02}", seconds / 60, seconds % 60) }
}

fn gallery_rename_target(path: &Path, name: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() || !path.is_file() {
        return Err(tr3!("No se encontró el archivo original.", "Source file not found.", "Исходный файл не найден.").into());
    }
    validate_file_stem(name)?;
    let extension = path.extension().ok_or(tr3!("El archivo no tiene extensión.", "File has no extension.", "У файла нет расширения."))?;
    let mut filename = std::ffi::OsString::from(name);
    filename.push(".");
    filename.push(extension);
    Ok(path.with_file_name(filename))
}

fn validate_file_stem(name: &str) -> Result<(), String> {
    if name.is_empty() || name.ends_with([' ', '.']) || name.chars().any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)) {
        return Err(tr3!("El nombre está vacío o contiene caracteres no válidos.", "The name is empty or contains invalid characters.", "Имя пустое или содержит недопустимые символы.").into());
    }
    if name.encode_utf16().count() > 200 { return Err(tr3!("El nombre es demasiado largo.", "The name is too long.", "Имя слишком длинное.").into()); }
    let device = name.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&device.as_str())
        || ["COM", "LPT"].iter().any(|prefix| device.strip_prefix(prefix).is_some_and(|suffix| matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")));
    if reserved { return Err(tr3!("Este nombre está reservado por Windows.", "This name is reserved by Windows.", "Это имя зарезервировано Windows.").into()); }
    Ok(())
}

fn video_name(template: &str, time: chrono::NaiveDateTime) -> Result<String, String> {
    let name = template
        .replace("[ГГГГ-ММ-ДД]", &time.format("%Y-%m-%d").to_string())
        .replace("[ЧЧ-ММ-СС]", &time.format("%H-%M-%S").to_string())
        .replace("[ЧЧ-ММ]", &time.format("%H-%M").to_string());
    if name.contains(['[', ']']) { return Err(tr3!("Marcador de fecha u hora desconocido.", "Unknown date or time placeholder.", "Неизвестное обозначение даты или времени.").into()); }
    validate_file_stem(&name)?;
    Ok(name)
}

fn add_gallery_file(files: &mut Vec<PathBuf>, path: &Path) -> bool {
    if !path.is_file() || files.iter().any(|item| item == path) {
        return false;
    }
    files.push(path.to_owned());
    true
}

fn saved(text: String, file: PathBuf) -> Status {
    Status { text, file: Some(file), error: false }
}

fn failed(text: String) -> Status {
    Status { text, file: None, error: true }
}

/// Never leaves the MP4 half-written when the app closes mid-recording or mid-save.
fn finish_on_exit(rec: RecState) -> Option<PathBuf> {
    match rec {
        RecState::Active { session, mixer, .. } => {
            drop(mixer);
            if let (Some(enc), _) = session.stop()
                && let Some(file) = enc.finish()
            {
                return file.finish().ok();
            }
            None
        }
        RecState::Saving(rx) => {
            rx.recv().ok().and_then(Result::ok)
        }
        RecState::Idle => None,
    }
}

impl CpuMeter {
    fn update(&mut self) {
        let now = Instant::now();
        if self.last.is_some_and(|(at, _)| now.duration_since(at) < Duration::from_secs(1)) {
            return;
        }
        let Some(cpu) = crate::platform::sys::process_cpu_time() else { return };
        if let Some((at, before)) = self.last {
            let cores = thread::available_parallelism().map_or(1, |n| n.get()) as f64;
            let wall = now.duration_since(at).as_secs_f64() * 1e7; // 100 ns units
            self.percent = (cpu.saturating_sub(before)) as f64 / wall / cores * 100.0;
        }
        self.last = Some((now, cpu));
    }
}

fn cpu_load(previous: (u64, u64), current: (u64, u64)) -> Option<f64> {
    let idle = current.0.checked_sub(previous.0)?;
    let total = current.1.checked_sub(previous.1)?;
    if total == 0 || idle > total { return None; }
    Some((total - idle) as f64 * 100.0 / total as f64)
}

fn hud_position(corner: HudCorner, monitor: Vec2, height: f32) -> egui::Pos2 {
    let left = 20.0f32.min((monitor.x - 190.0).max(0.0));
    let right = (monitor.x - 210.0).max(0.0);
    let top = 20.0f32.min((monitor.y - height).max(0.0));
    let bottom = (monitor.y - height - 20.0).max(0.0);
    match corner {
        HudCorner::TopLeft => pos2(left, top),
        HudCorner::TopRight => pos2(right, top),
        HudCorner::BottomLeft => pos2(left, bottom),
        HudCorner::BottomRight => pos2(right, bottom),
    }
}

impl SystemMeter {
    fn update(&mut self) {
        let now = Instant::now();
        if self.last.is_some_and(|(at, _)| now.duration_since(at) < Duration::from_secs(1)) { return; }
        self.memory = crate::platform::sys::physical_memory();
        let Some(times) = crate::platform::sys::system_cpu_times() else { self.cpu_percent = None; return };
        self.cpu_percent = self.last.and_then(|(_, before)| cpu_load(before, times));
        self.last = Some((now, times));
    }

    #[cfg(windows)]
    fn update_gpu(&mut self, enabled: bool) {
        if !enabled {
            self.gpu_receiver = None;
            self.gpu = None;
            return;
        }
        if self.gpu_receiver.is_none() {
            let (tx, rx) = mpsc::sync_channel(1);
            self.gpu_receiver = Some(rx);
            thread::spawn(move || {
                let Some(gpu) = crate::platform::nvml::Nvidia::new() else {
                    let _ = tx.try_send(crate::platform::nvml::GpuStats::default());
                    return;
                };
                loop {
                    match tx.try_send(gpu.sample()) {
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    }
                    thread::sleep(Duration::from_secs(2));
                }
            });
        }
        if let Some(receiver) = &self.gpu_receiver {
            while let Ok(stats) = receiver.try_recv() { self.gpu = Some(stats); }
        }
    }

    #[cfg(windows)]
    fn update_fps(&mut self, enabled: bool) {
        if !enabled {
            self.fps_monitor = None;
            self.fps = None;
            return;
        }
        let monitor = self.fps_monitor.get_or_insert_with(crate::platform::fps::FpsMonitor::start);
        if let Some(sample) = monitor.latest() { self.fps = Some(sample); }
    }
}

/// Whether the window is not shown, so the capture need not make preview pictures. Windows says
/// when the window is minimized (`visible`); Wayland never does, but there the compositor stops
/// asking for frames, so the window is not drawn. A shown window draws at least once a second
/// (`repaint_interval`).
fn preview_hidden(visible: Option<bool>, since_drawn: Duration) -> bool {
    visible == Some(false) || since_drawn > Duration::from_secs(3)
}

/// How soon to draw again. Meters move at 15 fps while there is sound, and the timer while
/// recording; a silent, idle window only needs the CPU figure, once a second (an input that
/// starts to make sound wakes it at once, and the preview and camera wake it for new pictures).
fn repaint_interval(hidden: bool, active: bool, meters: &[f32]) -> Duration {
    if !hidden && (active || meters.iter().any(|m| *m > METER_FLOOR)) { Duration::from_millis(66) } else { Duration::from_secs(1) }
}

fn dated_media_stem(label: Option<&str>, time: chrono::NaiveDateTime) -> String {
    let time = time.format("%Y-%m-%d %H-%M-%S%.3f");
    label.map_or_else(|| time.to_string(), |label| format!("{time} — {label}"))
}

fn new_output(dir: &Path, prefix: Option<&str>, ext: &str, named: &mut HashSet<PathBuf>) -> PathBuf {
    let stem = dated_media_stem(prefix, chrono::Local::now().naive_local());
    unused_name(dir, &stem, ext, named)
}

/// `dir/stem.ext`, or `dir/stem (2).ext` and so on if that is taken.
fn unused_name(dir: &Path, stem: &str, ext: &str, named: &mut HashSet<PathBuf>) -> PathBuf {
    let path = (1..)
        .map(|n| dir.join(if n == 1 { format!("{stem}.{ext}") } else { format!("{stem} ({n}).{ext}") }))
        .find(|p| !named.contains(p) && !p.exists())
        .expect("an unused name");
    named.insert(path.clone());
    path
}

pub(crate) fn reserve_recording_name(dir: &Path, stem: &str, named: &mut HashSet<PathBuf>) -> std::io::Result<PathBuf> {
    for _ in 0..10_000 {
        let path = unused_name(dir, stem, "mp4", named);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(&path) {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "Не удалось подобрать свободное имя записи"))
}

#[cfg(test)]
#[test]
fn simultaneous_recording_reservations_keep_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("запись.mp4");
    std::fs::write(&existing, b"existing").unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = (0..2).map(|_| {
        let path = dir.path().to_path_buf();
        let barrier = barrier.clone();
        thread::spawn(move || {
            barrier.wait();
            reserve_recording_name(&path, "запись", &mut HashSet::new()).unwrap()
        })
    }).collect();
    let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
    assert_ne!(results[0], results[1]);
    assert_eq!(std::fs::read(existing).unwrap(), b"existing");
    assert!(results.iter().all(|path| path.is_file()));
}

/// A typed recording path that is empty or relative would put recordings wherever obs-lite was
/// started from: the previous folder is kept instead.
fn typed_output_dir(typed: PathBuf, previous: &Path) -> PathBuf {
    if typed.is_absolute() { typed } else { previous.to_owned() }
}

/// Slider position (0..1) to linear gain; squared for a more natural loudness curve.
fn channel_gain(volume: f32, muted: bool) -> f32 {
    if muted { 0.0 } else { volume * volume }
}

/// Desktop audio leaves out the application captured on its own (`app`), which it would
/// otherwise repeat. Leaving a process out is not tied to an output device, so with a chosen
/// one (`system_device`) desktop audio stays that device's whole output.
fn system_kind(app: Option<InputKind>, system_device: Option<&str>) -> InputKind {
    match (app, system_device) {
        (Some(InputKind::Application(pid)), None) => InputKind::SystemExcept(pid),
        _ => InputKind::System,
    }
}

/// Where `region` of a `source`-sized image falls on the preview `canvas`.
fn region_rect(region: Region, canvas: Rect, source: (u32, u32)) -> Rect {
    let scale = vec2(canvas.width() / source.0 as f32, canvas.height() / source.1 as f32);
    let min = canvas.min + vec2(region.x as f32, region.y as f32) * scale;
    Rect::from_min_size(min, vec2(region.width as f32, region.height as f32) * scale)
}

/// The source region under `selection` on the preview `canvas`.
fn region_from_rect(selection: Rect, canvas: Rect, source: (u32, u32)) -> Option<Region> {
    let scale = vec2(source.0 as f32 / canvas.width(), source.1 as f32 / canvas.height());
    let min = (selection.min - canvas.min) * scale;
    let size = selection.size() * scale;
    let px = |v: f32| v.round().max(0.0) as u32;
    Region { x: px(min.x), y: px(min.y), width: px(size.x), height: px(size.y) }.fit(source)
}

/// Dims the preview outside `inner` and outlines it.
fn outline_region(painter: &egui::Painter, canvas: Rect, inner: Rect) {
    let shade = Color32::from_black_alpha(140);
    for part in [
        Rect::from_min_max(canvas.min, pos2(canvas.max.x, inner.min.y)),
        Rect::from_min_max(pos2(canvas.min.x, inner.max.y), canvas.max),
        Rect::from_min_max(pos2(canvas.min.x, inner.min.y), pos2(inner.min.x, inner.max.y)),
        Rect::from_min_max(pos2(inner.max.x, inner.min.y), pos2(canvas.max.x, inner.max.y)),
    ] {
        if part.is_positive() {
            painter.rect_filled(part, 0.0, shade);
        }
    }
    painter.rect_stroke(inner, 0.0, Stroke::new(1.5, PRIMARY_LIGHT), egui::StrokeKind::Inside);
}

/// Draws the webcam onto an RGBA screenshot where the recording would show it.
fn paste_camera(rgba: &mut [u8], width: u32, height: u32, frame: &CameraFrame, corner: Corner, size: CameraSize) {
    let (x, y, w, h) = camera::overlay_rect((width, height), (frame.width, frame.height), corner, size);
    let scaled = camera::resize(&frame.bgra, (frame.width, frame.height), (w, h));
    for (row, src) in scaled.chunks(w as usize * 4).enumerate() {
        let start = ((y as usize + row) * width as usize + x as usize) * 4;
        for (dst, &[b, g, r, a]) in rgba[start..start + src.len()].as_chunks_mut::<4>().0.iter_mut().zip(src.as_chunks::<4>().0) {
            *dst = [r, g, b, a];
        }
    }
}

/// An OBS-style dock: titled panel with a border.
fn dock(ui: &mut egui::Ui, title: &str, size: Vec2, content: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(size, Layout::top_down(Align::Min), |ui| {
        egui::Frame::new().fill(GREY6).stroke(Stroke::new(1.0, GREY4)).corner_radius(4).show(ui, |ui| {
            ui.set_min_size(size - vec2(2.0, 2.0));
            ui.set_max_width(size.x - 2.0);
            egui::Frame::new().fill(GREY7).inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(title).strong());
            });
            egui::Frame::new().inner_margin(Margin::same(8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_height(ui.available_height());
                content(ui);
            });
        });
    });
}

/// A selected list item, as in the OBS Sources list.
fn list_row(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
    ui.painter().rect_filled(rect, 3.0, PRIMARY);
    let clip = rect.shrink2(vec2(8.0, 0.0));
    let response = ui.put(clip, egui::Label::new(RichText::new(text).size(13.5).color(Color32::WHITE)).truncate().sense(Sense::focusable_noninteractive()));
    if response.has_focus() {
        ui.painter().rect_stroke(rect.expand(1.0), 3.0, Stroke::new(1.0, PRIMARY_LIGHT), egui::StrokeKind::Inside);
        response.show_tooltip_text(text);
    }
    response.on_hover_text(text)
}

fn group_title(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).strong().size(14.0));
    ui.add_space(4.0);
}

/// OBS-style volume meter: dim colour zones, lit up to the current level, with a dB scale.
fn meter(ui: &mut egui::Ui, db: f32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
    let bar = Rect::from_min_size(rect.min, vec2(width, 7.0));
    let painter = ui.painter();
    let x_of = |db: f32| bar.left() + bar.width() * ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let level = x_of(db);
    for (from, to, dim, lit) in [(-60.0, -20.0, GREEN_DIM, GREEN), (-20.0, -9.0, YELLOW_DIM, YELLOW), (-9.0, 0.0, RED_DIM, RED)] {
        let zone = Rect::from_x_y_ranges(x_of(from)..=x_of(to), bar.y_range());
        painter.rect_filled(zone, 0.0, dim);
        if level > zone.left() {
            painter.rect_filled(Rect::from_x_y_ranges(zone.left()..=level.min(zone.right()), bar.y_range()), 0.0, lit);
        }
    }
    for tick in [-60, -50, -40, -30, -20, -10, 0] {
        let x = x_of(tick as f32);
        painter.line_segment([pos2(x, bar.bottom() + 1.0), pos2(x, bar.bottom() + 3.0)], Stroke::new(1.0, GREY1));
        let align = match tick {
            -60 => Align2::LEFT_TOP,
            0 => Align2::RIGHT_TOP,
            _ => Align2::CENTER_TOP,
        };
        painter.text(pos2(x, bar.bottom() + 3.0), align, tick.to_string(), FontId::proportional(9.0), TEXT_MUTED);
    }
}

#[derive(Clone, Copy)]
enum Icon {
    Record,
    Pause,
    Play,
    Save,
}

/// Draws an icon centred on `c`; drawn with shapes because the bundled fonts lack these glyphs.
fn paint_icon(painter: &egui::Painter, icon: Icon, c: egui::Pos2, s: f32, color: Color32) {
    match icon {
        Icon::Record => {
            painter.circle_filled(c, s * 0.8, color);
        }
        Icon::Pause => {
            let bar = vec2(s * 0.55, s * 1.8);
            painter.rect_filled(Rect::from_center_size(c - vec2(s * 0.45, 0.0), bar), 1.0, color);
            painter.rect_filled(Rect::from_center_size(c + vec2(s * 0.45, 0.0), bar), 1.0, color);
        }
        Icon::Play => {
            let pts = vec![c + vec2(-s * 0.7, -s), c + vec2(-s * 0.7, s), c + vec2(s, 0.0)];
            painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
        }
        Icon::Save => {
            // A downward arrow onto a tray.
            let stroke = Stroke::new(1.6, color);
            painter.line_segment([c + vec2(0.0, -s), c + vec2(0.0, s * 0.35)], stroke);
            let head = vec![c + vec2(-s * 0.55, -s * 0.05), c + vec2(s * 0.55, -s * 0.05), c + vec2(0.0, s * 0.55)];
            painter.add(egui::Shape::convex_polygon(head, color, Stroke::NONE));
            painter.line_segment([c + vec2(-s, s), c + vec2(s, s)], stroke);
        }
    }
}

/// Square button with a drawn icon (the pause button next to tr!("Detener Grabación", "Stop Recording")).
fn icon_button(ui: &mut egui::Ui, icon: Icon, size: f32, enabled: bool, checked: bool, label: &str) -> egui::Response {
    let button = Button::new("").min_size(vec2(size, size)).fill(if checked { PRIMARY } else { GREY4 });
    let response = ui.add_enabled(enabled, button);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, response.enabled(), label));
    let color = if enabled { Color32::WHITE } else { TEXT_MUTED };
    paint_icon(ui.painter(), icon, response.rect.center(), 6.0, color);
    response
}

/// Speaker button that turns red with a cross when muted.
fn mute_button(ui: &mut egui::Ui, muted: bool, label: &str) -> egui::Response {
    let response = ui.add(Button::new("").min_size(vec2(28.0, 22.0)).fill(if muted { RED_DIM } else { GREY4 }));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, response.enabled(), label));
    let c = response.rect.center() - vec2(3.0, 0.0);
    let color = if muted { RED } else { Color32::WHITE };
    let painter = ui.painter();
    painter.rect_filled(Rect::from_min_max(c + vec2(-6.0, -3.0), c + vec2(-3.0, 3.0)), 0.0, color);
    let cone = vec![c + vec2(-3.0, -3.0), c + vec2(2.0, -7.0), c + vec2(2.0, 7.0), c + vec2(-3.0, 3.0)];
    painter.add(egui::Shape::convex_polygon(cone, color, Stroke::NONE));
    let stroke = Stroke::new(1.5, color);
    if muted {
        painter.line_segment([c + vec2(5.0, -3.0), c + vec2(11.0, 3.0)], stroke);
        painter.line_segment([c + vec2(11.0, -3.0), c + vec2(5.0, 3.0)], stroke);
    } else {
        painter.line_segment([c + vec2(5.0, -3.0), c + vec2(7.0, 0.0)], stroke);
        painter.line_segment([c + vec2(7.0, 0.0), c + vec2(5.0, 3.0)], stroke);
        painter.line_segment([c + vec2(8.0, -6.0), c + vec2(11.0, 0.0)], stroke);
        painter.line_segment([c + vec2(11.0, 0.0), c + vec2(8.0, 6.0)], stroke);
    }
    response
}

fn status_icon(ui: &mut egui::Ui, icon: Icon, color: Color32, text: &str) {
    ui.label(RichText::new(text).small().monospace().color(if color == GREY3 { TEXT_MUTED } else { Color32::WHITE }));
    let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
    paint_icon(ui.painter(), icon, rect.center(), 5.0, color);
}

fn combo<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut T,
    options: &[T],
    label: impl Fn(T) -> String,
    width: f32,
) -> egui::Response {
    ComboBox::from_id_salt(id).width(width).selected_text(label(*value)).show_ui(ui, |ui| {
        for &option in options {
            ui.selectable_value(value, option, label(option));
        }
    }).response
}

/// OBS-style device picker: "(Por defecto)", "Deshabilitado" or a specific device.
fn device_combo(ui: &mut egui::Ui, id: &str, enabled: &mut bool, device: &mut Option<String>, devices: &[Device]) -> egui::Response {
    let default = tr!("(Por defecto)", "Default");
    let disabled = tr!("Deshabilitado", "Disabled");
    let current = if !*enabled {
        disabled.to_owned()
    } else {
        device
            .as_ref()
            .and_then(|id| devices.iter().find(|d| &d.id == id))
            .map_or(default.to_owned(), |d| d.name.clone())
    };
    let width = ui.available_width().min(330.0);
    ComboBox::from_id_salt(id).width(width).truncate().selected_text(current).show_ui(ui, |ui| {
        if ui.selectable_label(*enabled && device.is_none(), default).clicked() {
            (*enabled, *device) = (true, None);
            ui.close();
        }
        if ui.selectable_label(!*enabled, disabled).clicked() {
            *enabled = false;
            ui.close();
        }
        for d in devices {
            let selected = *enabled && device.as_deref() == Some(d.id.as_str());
            let option = ui.add_sized([(width - 16.0).max(1.0), 0.0], Button::selectable(selected, (d.name.as_str(), egui::Atom::grow())).truncate());
            if option.has_focus() { option.show_tooltip_text(&d.name); }
            if option.on_hover_text(&d.name).clicked() {
                (*enabled, *device) = (true, Some(d.id.clone()));
                ui.close();
            }
        }
    }).response
}

fn apply_theme(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = GREY7;
    v.window_fill = GREY7;
    v.window_stroke = Stroke::new(1.0, GREY4);
    v.extreme_bg_color = GREY8;
    v.faint_bg_color = GREY6;
    v.selection.bg_fill = PRIMARY;
    v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    v.hyperlink_color = PRIMARY_LIGHT;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, GREY4);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    for (w, fill) in [
        (&mut v.widgets.inactive, GREY4),
        (&mut v.widgets.hovered, GREY3),
        (&mut v.widgets.active, GREY7),
        (&mut v.widgets.open, GREY3),
    ] {
        w.bg_fill = fill;
        w.weak_bg_fill = fill;
        w.corner_radius = CornerRadius::same(4);
        w.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    }
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, GREY1);
    v.widgets.active.bg_stroke = Stroke::new(1.0, PRIMARY);
    ctx.set_visuals(v);
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = vec2(6.0, 4.0);
        style.spacing.button_padding = vec2(10.0, 4.0);
        style.spacing.interact_size.y = 24.0;
    });
    ctx.set_theme(egui::ThemePreference::Dark);
}

/// "(Ctrl+Alt+R)" for tooltips, or nothing when unassigned.
fn hotkey_hint(hotkey: &Option<String>) -> String {
    hotkey.as_ref().map_or(String::new(), |h| format!("({h})"))
}

/// OBS-style hotkey field: click it, then press the combination; Escape cancels, × clears.
/// One hotkey's button; true when a new combination was just set.
fn hotkey_field(ui: &mut egui::Ui, row: usize, slot: &mut Option<String>, capturing: &mut Option<usize>, label: &str) -> bool {
    let mut set = false;
    ui.horizontal(|ui| {
        let waiting = *capturing == Some(row);
        let text = if waiting {
            tr!("Pulsa una combinación…", "Press a combination…").to_owned()
        } else {
            slot.clone().unwrap_or_else(|| tr!("(ninguno)", "(none)").to_owned())
        };
        let button = Button::new(RichText::new(&text).monospace()).min_size(vec2(220.0, 24.0));
        let button = ui.add(button.fill(if waiting { PRIMARY } else { GREY4 }));
        button.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, button.enabled(), format!("{label}: {text}")));
        if button.clicked() {
            *capturing = Some(row);
        }
        let clear = ui.add_enabled(slot.is_some(), Button::new("×")).on_hover_text(tr!("Quitar", "Clear"));
        clear.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, clear.enabled(), format!("{}: {label}", tr3!("Quitar", "Clear", "Очистить"))));
        if clear.clicked() {
            *slot = None;
        }
        if waiting {
            // Ctrl, Alt and Shift arrive as key presses of their own before the actual key.
            let pressed = ui.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Key { key: egui::Key::Escape, pressed: true, .. } => Some(None),
                    egui::Event::Key { key, pressed: true, modifiers, .. } => hotkeys::format(*modifiers, *key).map(Some),
                    _ => None,
                })
            });
            if let Some(text) = pressed {
                if text.is_some() {
                    *slot = text;
                    set = true;
                }
                *capturing = None;
            }
        }
    });
    set
}

/// A combination set for one action is taken from any other that had it: the system would
/// refuse the second registration, and blame another application.
fn take_from_others(slots: &mut [&mut Option<String>], row: usize) {
    let taken = slots[row].clone();
    for (i, slot) in slots.iter_mut().enumerate() {
        if i != row && **slot == taken {
            **slot = None;
        }
    }
}

/// An audio input that stopped with an error (its device was unplugged, say).
fn input_failed(input: &Option<Input>) -> bool {
    input.as_ref().is_some_and(|i| i.shared.error().is_some())
}

fn audio_overlap(config: &Config) -> bool {
    config.system_audio && config.system_device.is_some() && config.app_audio.is_some()
}

#[cfg(test)]
mod tests {
    #[test]
    fn long_source_label_is_elided_visually_and_keeps_full_accessible_name() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let name = "Display capture · A very long monitor name 1920x1080";
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0))), ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| { egui::CentralPanel::default().show(ui, |ui| { list_row(ui, name); }); });
        output.textures_delta.clear();
        let tree = output.platform_output.accesskit_update.expect("accessibility tree");
        assert!(tree.nodes.iter().any(|(_, node)| node.role() == egui::Role::Label && node.value() == Some(name)));
        let mut shapes: Vec<_> = output.shapes.into_iter().map(|shape| shape.shape).collect();
        let mut elided = false;
        while let Some(shape) = shapes.pop() {
            match shape {
                egui::Shape::Vec(inner) => shapes.extend(inner),
                egui::Shape::Text(text) if text.galley.elided => elided = true,
                _ => {}
            }
        }
        assert!(elided, "long source name must show truncation");
    }

    #[cfg(windows)]
    #[test]
    fn overlay_hides_on_external_focus_only_after_opening_and_receiving_focus() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.hidden = false;
        app.handle_focus_change(&ctx, Some(false), true);
        assert!(!app.hidden);
        app.handle_focus_change(&ctx, Some(true), false);
        assert!(app.focus_armed);
        app.handle_focus_change(&ctx, Some(false), false);
        assert!(!app.hidden);
        app.opening = Some(OpeningAnimation { last: Instant::now(), progress: 0.5, from: pos2(-420.0, 0.0), to: pos2(0.0, 0.0), rest: pos2(0.0, 0.0), duration: 0.14 });
        app.handle_focus_change(&ctx, Some(false), true);
        assert!(!app.hidden);
        app.opening = None;
        app.handle_focus_change(&ctx, Some(false), true);
        assert!(app.hidden);
        assert!(!app.focus_armed);
    }

    #[cfg(windows)]
    #[test]
    fn losing_focus_closes_settings_and_can_reopen_overlay() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.setup_complete = true;
        app.settings = Some(SettingsDialog { draft: app.config.clone(), page: SettingsPage::General, capturing: None, replay_duration: "00:45".into(), selected_source: None });
        app.focus_armed = true;
        app.handle_focus_change(&ctx, Some(false), true);
        assert!(app.hidden && app.settings.is_none());
        app.handle(&ctx, Action::ToggleMenu);
        assert!(!app.hidden);
    }

    #[test]
    fn home_exposes_main_actions_to_accessibility_tree() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = test_app();
        app.config.language = Some(Language::English);
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 1080.0))), ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| app.overlay_home(ui));
        output.textures_delta.clear();
        let tree = output.platform_output.accesskit_update.expect("accessibility tree");
        let labels: Vec<_> = tree.nodes.iter().filter_map(|(_, node)| node.label().map(|label| (node.role(), label))).collect();
        for name in ["Settings", "Save replay", "Enable replay"] {
            assert!(labels.iter().any(|(role, label)| *role == egui::Role::Button && label.contains(name)), "missing button {name}: {labels:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn gallery_exposes_named_card_and_actions_to_accessibility_tree() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut app = test_app();
        app.config.language = Some(Language::English);
        let path = Path::new("sample.mp4");
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 400.0))),
            events: vec![egui::Event::PointerMoved(pos2(30.0, 30.0))],
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| { egui::CentralPanel::default().show(ui, |ui| app.gallery_card(ui, path, 120.0)); });
        output.textures_delta.clear();
        let tree = output.platform_output.accesskit_update.expect("accessibility tree");
        let labels: Vec<_> = tree.nodes.iter().filter_map(|(_, node)| node.label().map(|label| (node.role(), label))).collect();
        for name in ["Open file: sample.mp4", "Move to recycle bin: sample.mp4", "Trim video: sample.mp4"] {
            assert!(labels.iter().any(|(role, label)| *role == egui::Role::Button && *label == name), "missing button {name}: {labels:?}");
        }
    }

    #[test]
    fn icon_actions_expose_accessible_names_and_enabled_state() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 400.0))), ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                icon_button(ui, Icon::Pause, 30.0, false, false, "Pause recording");
                mute_button(ui, false, "Mute: Desktop Audio");
            });
        });
        output.textures_delta.clear();
        let tree = output.platform_output.accesskit_update.expect("accessibility tree");
        let pause = tree.nodes.iter().find(|(_, node)| node.label() == Some("Pause recording")).map(|(_, node)| node).expect("pause button");
        assert_eq!(pause.role(), egui::Role::Button);
        assert!(pause.is_disabled());
        assert!(tree.nodes.iter().any(|(_, node)| node.role() == egui::Role::Button && node.label() == Some("Mute: Desktop Audio")));
    }

    #[test]
    fn general_settings_combo_boxes_reference_visible_labels() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut dialog = SettingsDialog { draft: Config::default(), page: SettingsPage::General, capturing: None, replay_duration: "00:45".into(), selected_source: None };
        dialog.draft.language = Some(Language::English);
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 1080.0))), ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| { egui::CentralPanel::default().show(ui, |ui| compact_settings_page(ui, &mut dialog, &[], &[], &[], None)); });
        output.textures_delta.clear();
        let tree = output.platform_output.accesskit_update.expect("accessibility tree");
        for expected in ["Language", "Metrics position"] {
            let label = tree.nodes.iter().find(|(_, node)| node.role() == egui::Role::Label && (node.value() == Some(expected) || node.label() == Some(expected))).map(|(id, _)| *id).expect(expected);
            assert!(tree.nodes.iter().any(|(_, node)| node.role() == egui::Role::ComboBox && node.labelled_by().contains(&label)), "combo box is not linked to {expected}");
        }
    }

    #[test]
    fn settings_controls_have_accessible_names() {
        for page in [SettingsPage::General, SettingsPage::Output, SettingsPage::Audio, SettingsPage::Video, SettingsPage::Hotkeys] {
            let ctx = egui::Context::default();
            ctx.enable_accesskit();
            let mut dialog = SettingsDialog { draft: Config::default(), page, capturing: None, replay_duration: "00:45".into(), selected_source: None };
            dialog.draft.language = Some(Language::English);
            dialog.draft.quality = Quality::Custom;
            dialog.draft.screenshot_format = ScreenshotFormat::Jpeg;
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 1400.0))), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| { egui::CentralPanel::default().show(ui, |ui| compact_settings_page(ui, &mut dialog, &[], &[], &[], None)); });
            output.textures_delta.clear();
            let tree = output.platform_output.accesskit_update.expect("accessibility tree");
            for (_, node) in &tree.nodes {
                if matches!(node.role(), egui::Role::Button | egui::Role::CheckBox | egui::Role::ComboBox | egui::Role::Slider | egui::Role::TextInput | egui::Role::SpinButton) {
                    assert!(node.label().is_some_and(|label| !label.is_empty()) || !node.labelled_by().is_empty(), "unnamed {:?}", node.role());
                }
            }
        }
    }

    #[test]
    fn gallery_image_opens_png_and_jpeg_and_reports_missing_file() {
        let folder = tempfile::tempdir().unwrap();
        let pixels = [255u8, 0, 0, 255].repeat(16);
        let png = folder.path().join("image.png");
        let jpeg = folder.path().join("image.jpg");
        crate::capture::write_png(&png, 4, 4, &pixels).unwrap();
        crate::capture::write_jpeg(&jpeg, 4, 4, &pixels, 90).unwrap();
        assert_eq!(super::load_gallery_image(&png).unwrap().size, [4, 4]);
        assert_eq!(super::load_gallery_image(&jpeg).unwrap().size, [4, 4]);
        assert!(super::load_gallery_image(&folder.path().join("missing.png")).is_err());
    }

    #[test]
    fn escape_returns_from_image_and_advanced_view_before_hiding_overlay() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.hidden = false;
        app.image_viewer = Some(ImageViewer { path: PathBuf::from("image.png"), loading: None, texture: None, error: None });
        app.escape_navigation(&ctx);
        assert!(app.image_viewer.is_none());
        assert!(!app.hidden);
        app.advanced_view = true;
        app.escape_navigation(&ctx);
        assert!(!app.advanced_view);
        assert!(!app.hidden);
        app.escape_navigation(&ctx);
        assert!(app.hidden);
    }

    #[cfg(windows)]
    #[test]
    fn gallery_click_opens_screenshot_in_overlay_and_hide_closes_it() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("image.png");
        crate::capture::write_png(&path, 4, 4, &[255u8; 64]).unwrap();
        let ctx = egui::Context::default();
        let mut app = test_app();
        let pos = pos2(30.0, 30.0);
        for events in [vec![], vec![egui::Event::PointerMoved(pos)], vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }], vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]] {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 500.0))), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| { egui::CentralPanel::default().show(ui, |ui| app.gallery_card(ui, &path, 120.0)); });
            output.textures_delta.clear();
        }
        assert_eq!(app.image_viewer.as_ref().map(|viewer| &viewer.path), Some(&path));
        assert!(!app.hidden);
        let deadline = Instant::now() + Duration::from_secs(3);
        while app.image_viewer.as_ref().is_some_and(|viewer| viewer.texture.is_none()) && Instant::now() < deadline {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(740.0, 600.0))), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| app.image_view(ui));
            output.textures_delta.clear();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(app.image_viewer.as_ref().is_some_and(|viewer| viewer.texture.is_some() && viewer.error.is_none()));
        let pos = pos2(50.0, 30.0);
        for events in [vec![egui::Event::PointerMoved(pos)], vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }], vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]] {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(740.0, 600.0))), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| app.image_view(ui));
            output.textures_delta.clear();
        }
        assert!(app.image_viewer.is_none());
        app.open_image(&path, &ctx);
        app.hide_window(&ctx);
        assert!(app.image_viewer.is_some());
        app.advance_window_animation(&ctx, Instant::now() + Duration::from_millis(200));
        assert!(app.image_viewer.is_none());
    }

    #[test]
    fn viewer_header_aligns_file_name_with_kind() {
        let ctx = egui::Context::default();
        let mut shapes = Vec::new();
        for _ in 0..2 {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 200.0))), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| { viewer_header(ui, Path::new("sample.png"), "СКРИНШОТ", Language::Russian); });
            });
            output.textures_delta.clear();
            shapes = output.shapes;
        }
        let mut positions = [None, None];
        let mut pending: Vec<_> = shapes.into_iter().map(|shape| shape.shape).collect();
        while let Some(shape) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == "СКРИНШОТ" => positions[0] = Some(text.visual_bounding_rect().left()),
                egui::Shape::Text(text) if text.galley.text() == "sample.png" => positions[1] = Some(text.visual_bounding_rect().left()),
                _ => {}
            }
        }
        let [Some(kind), Some(name)] = positions else { panic!("viewer labels not painted") };
        assert!((kind - name).abs() < 3.0, "header labels are misaligned: {kind} and {name}");
    }

    #[test]
    fn image_viewer_uses_a_centered_size_that_fits_the_monitor() {
        let monitor = vec2(1920.0, 1080.0);
        let wide = image_viewport_size(monitor, Some(vec2(1920.0, 1080.0)));
        let portrait = image_viewport_size(monitor, Some(vec2(720.0, 1280.0)));
        assert!(wide.x < monitor.x && wide.y < monitor.y && wide.x > wide.y);
        assert!(portrait.x < portrait.y && portrait.y < monitor.y);
        let small = image_viewport_size(vec2(640.0, 600.0), Some(vec2(1920.0, 1080.0)));
        assert!(small.x <= 640.0 && small.y <= 600.0 && small.x >= 360.0 && small.y >= 500.0);
    }

    #[test]
    fn viewer_header_truncates_long_names_inside_narrow_panel() {
        let ctx = egui::Context::default();
        let name = format!("{}.png", "very-long-file-name-".repeat(10));
        let mut shapes = Vec::new();
        for _ in 0..2 {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 200.0))), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| { viewer_header(ui, Path::new(&name), "СКРИНШОТ", Language::Russian); });
            });
            output.textures_delta.clear();
            shapes = output.shapes;
        }
        let mut pending: Vec<_> = shapes.into_iter().map(|shape| shape.shape).collect();
        let mut found = false;
        while let Some(shape) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == name => {
                    assert!(text.galley.elided, "long file name was not truncated");
                    assert!(text.visual_bounding_rect().right() <= 360.0, "file name extends outside panel");
                    found = true;
                }
                _ => {}
            }
        }
        assert!(found, "file name was not rendered");
    }

    #[test]
    fn viewer_header_shows_full_name_on_keyboard_focus() {
        let ctx = egui::Context::default();
        let name = format!("{}.png", "long-file-name-".repeat(10));
        let pass = |events| { let mut output = ctx.run_ui(egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 600.0))),
            events,
            ..Default::default()
        }, |ui| { egui::CentralPanel::default().show(ui, |ui| { viewer_header(ui, Path::new(&name), "СКРИНШОТ", Language::Russian); }); }); output.textures_delta.clear(); output };
        let key = |pressed| egui::Event::Key { key: egui::Key::Tab, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE };
        pass(vec![]);
        for _ in 0..2 {
            pass(vec![key(true)]);
            pass(vec![key(false)]);
        }
        let focused = ctx.memory(|memory| memory.focused()).and_then(|id| ctx.read_response(id)).expect("file name has no keyboard focus");
        assert!(focused.rect.left() > 125.0, "keyboard focus did not reach the file name");
        let output = pass(vec![]);
        let mut pending: Vec<_> = output.shapes.into_iter().map(|shape| shape.shape).collect();
        let mut tooltip_visible = false;
        while let Some(shape) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == name && !text.galley.elided => tooltip_visible = true,
                _ => {}
            }
        }
        assert!(tooltip_visible, "full file name tooltip was not displayed on focus");
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn player_view_fits_and_controls_real_media() {
        use windows::core::w;
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
        use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WINDOW_EX_STYLE, WS_OVERLAPPED};
        let path = PathBuf::from(std::env::var_os("OPNOVERLAY_CHECK_FILE").expect("test video required"));
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap(); }
        let parent = unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"), w!("player test"), WS_OVERLAPPED, 0, 0, 64, 64, None, None, None, None).unwrap() };
        let player = crate::platform::player::Player::open(parent.0 as isize, &path).unwrap();
        player.set_volume(0.0).unwrap();
        let mut app = test_app();
        app.config.gallery_files.push(path.clone());
        app.player = Some((path, player, 0.0));
        let ctx = egui::Context::default();
        let pass = |app: &mut App, size, events| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), size)), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| app.player_view(ui));
            output.textures_delta.clear();
            output.shapes
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut message = MSG::default();
        while Instant::now() < deadline && !app.player.as_ref().is_some_and(|(_, player, _)| player.playing() && player.duration().is_some_and(|duration| duration > 5.0)) {
            unsafe { while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() { let _ = TranslateMessage(&message); DispatchMessageW(&message); } }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(app.player.as_ref().is_some_and(|(_, player, _)| player.playing() && player.duration().is_some_and(|duration| duration > 5.0)));
        let player = &app.player.as_ref().unwrap().1;
        player.set_rate(0.5).expect("slower playback rate rejected");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && (player.rate().unwrap_or(1.0) - 0.5).abs() > 0.01 {
            unsafe { while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() { let _ = TranslateMessage(&message); DispatchMessageW(&message); } }
            thread::sleep(Duration::from_millis(10));
        }
        assert!((player.rate().unwrap() - 0.5).abs() < 0.01);
        let before = player.position().unwrap();
        let measure = Instant::now();
        while measure.elapsed() < Duration::from_secs(2) {
            unsafe { while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() { let _ = TranslateMessage(&message); DispatchMessageW(&message); } }
            thread::sleep(Duration::from_millis(10));
        }
        let advanced = player.position().unwrap() - before;
        assert!((0.4..1.6).contains(&advanced), "half-speed playback advanced {advanced:.2} seconds in two seconds");
        for rate in [0.75_f32, 1.5, 2.0, 1.0] {
            player.set_rate(rate).expect("playback rate rejected");
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline && (player.rate().unwrap_or(0.0) - rate).abs() > 0.01 {
                unsafe { while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() { let _ = TranslateMessage(&message); DispatchMessageW(&message); } }
                thread::sleep(Duration::from_millis(10));
            }
            assert!((player.rate().unwrap() - rate).abs() < 0.01, "playback rate did not change to {rate}");
        }
        ctx.enable_accesskit();
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(740.0, 600.0))), ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| app.player_view(ui));
        output.textures_delta.clear();
        let tree = output.platform_output.accesskit_update.expect("accessibility tree");
        let position = tree.nodes.iter().find(|(_, node)| node.role() == egui::Role::Slider && matches!(node.label(), Some("Положение видео" | "Video position" | "Posición del vídeo")));
        assert!(position.is_some(), "video position slider needs an accessible name");
        let volume = tree.nodes.iter().find(|(_, node)| node.role() == egui::Role::Label && matches!(node.value(), Some("Звук" | "Volume" | "Volumen"))).map(|(id, _)| *id).expect("volume label");
        assert!(tree.nodes.iter().any(|(_, node)| node.role() == egui::Role::Slider && node.labelled_by().contains(&volume)), "volume slider needs an accessible label");
        for size in [vec2(1920.0, 1080.0), vec2(740.0, 600.0), vec2(360.0, 600.0)] {
            let _ = pass(&mut app, size, vec![]);
            let mut pending: Vec<_> = pass(&mut app, size, vec![]).into_iter().map(|shape| shape.shape).collect();
            let mut labels = HashSet::new();
            let mut largest_black = Vec2::ZERO;
            while let Some(shape) = pending.pop() {
                match shape {
                    egui::Shape::Vec(inner) => pending.extend(inner),
                    egui::Shape::Rect(rect) if rect.fill == Color32::BLACK && rect.rect.width() > largest_black.x => largest_black = rect.rect.size(),
                    egui::Shape::Text(text) if ["Пауза", "Показать в папке", "Обрезать видео"].contains(&text.galley.text()) => {
                        let bounds = text.visual_bounding_rect();
                        assert!(bounds.left() >= 0.0 && bounds.right() <= size.x && bounds.top() >= 0.0 && bounds.bottom() <= size.y, "{}: {bounds:?} outside {size:?}", text.galley.text());
                        labels.insert(text.galley.text().to_owned());
                    }
                    _ => {}
                }
            }
            assert_eq!(labels.len(), 3, "missing controls at {size:?}: {labels:?}");
            if size.x > 1000.0 { assert!(largest_black.y > 600.0, "video area too small: {largest_black:?}"); }
        }
        let size = vec2(1920.0, 1080.0);
        let _ = pass(&mut app, size, vec![]);
        let mut pending: Vec<_> = pass(&mut app, size, vec![]).into_iter().map(|shape| shape.shape).collect();
        let mut pause = None;
        while let Some(shape) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == "Пауза" => pause = Some(text.visual_bounding_rect().center()),
                _ => {}
            }
        }
        let pause = pause.expect("pause button missing");
        let _ = pass(&mut app, size, vec![egui::Event::PointerMoved(pause), egui::Event::PointerButton { pos: pause, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
        let _ = pass(&mut app, size, vec![egui::Event::PointerButton { pos: pause, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && app.player.as_ref().unwrap().1.playing() {
            unsafe { while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() { let _ = TranslateMessage(&message); DispatchMessageW(&message); } }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!app.player.as_ref().unwrap().1.playing(), "pause did not take effect");
        let space = |pressed| egui::Event::Key { key: egui::Key::Space, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE };
        let _ = pass(&mut app, size, vec![space(true)]);
        let _ = pass(&mut app, size, vec![space(false)]);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !app.player.as_ref().unwrap().1.playing() {
            unsafe { while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() { let _ = TranslateMessage(&message); DispatchMessageW(&message); } }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(app.player.as_ref().unwrap().1.playing(), "space did not resume playback");
        let _ = pass(&mut app, size, vec![space(true)]);
        let _ = pass(&mut app, size, vec![space(false)]);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && app.player.as_ref().unwrap().1.playing() {
            unsafe { while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() { let _ = TranslateMessage(&message); DispatchMessageW(&message); } }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(!app.player.as_ref().unwrap().1.playing(), "space did not pause playback");
        let mut pending: Vec<_> = pass(&mut app, size, vec![]).into_iter().map(|shape| shape.shape).collect();
        let mut seek_track = None;
        let mut volume_track = None;
        while let Some(shape) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner),
                egui::Shape::Rect(rect) if rect.rect.width() > 800.0 && rect.rect.height() < 20.0 => seek_track = Some(rect.rect),
                egui::Shape::Rect(rect) if (150.0..=220.0).contains(&rect.rect.width()) && rect.rect.height() < 20.0 => volume_track = Some(rect.rect),
                _ => {}
            }
        }
        let seek = seek_track.expect("seek slider missing").center();
        let _ = pass(&mut app, size, vec![egui::Event::PointerMoved(seek), egui::Event::PointerButton { pos: seek, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
        let _ = pass(&mut app, size, vec![egui::Event::PointerButton { pos: seek, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && app.player.as_ref().unwrap().1.position().unwrap_or(0.0) < 10.0 {
            unsafe { while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() { let _ = TranslateMessage(&message); DispatchMessageW(&message); } }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(app.player.as_ref().unwrap().1.position().unwrap_or(0.0) >= 10.0, "seek did not move the real player");
        let volume = volume_track.expect("volume slider missing").center();
        let _ = pass(&mut app, size, vec![egui::Event::PointerMoved(volume), egui::Event::PointerButton { pos: volume, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
        let _ = pass(&mut app, size, vec![egui::Event::PointerButton { pos: volume, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        assert!((0.3..0.7).contains(&app.player.as_ref().unwrap().2), "volume slider did not update");
        assert!(!app.status.as_ref().is_some_and(|status| status.error), "volume change reported an error");
        let mut pending: Vec<_> = pass(&mut app, size, vec![]).into_iter().map(|shape| shape.shape).collect();
        let mut trim = None;
        while let Some(shape) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == "Обрезать видео" => trim = Some(text.visual_bounding_rect().center()),
                _ => {}
            }
        }
        let trim = trim.expect("trim button missing");
        let _ = pass(&mut app, size, vec![egui::Event::PointerMoved(trim), egui::Event::PointerButton { pos: trim, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
        let _ = pass(&mut app, size, vec![egui::Event::PointerButton { pos: trim, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        assert!(app.player.is_some(), "player must remain open while trimming");
        assert!(app.pending_trim.as_ref().is_some_and(|(_, start, end)| start == "00:00.000" && parse_trim_time(end).is_some_and(|ms| (5_000..=86_400_000).contains(&ms))));
        let editor_size = vec2(1100.0, 920.0);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && app.trim_frames.is_empty() {
            let _ = pass(&mut app, editor_size, vec![]);
            unsafe { while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() { let _ = TranslateMessage(&message); DispatchMessageW(&message); } }
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(app.trim_frames.len(), 10, "timeline frames did not load");
        let mut shapes: Vec<_> = pass(&mut app, editor_size, vec![]).into_iter().map(|shape| shape.shape).collect();
        let mut handles = Vec::new();
        while let Some(shape) = shapes.pop() {
            match shape {
                egui::Shape::Vec(inner) => shapes.extend(inner),
                egui::Shape::Rect(rect) if rect.fill == PRIMARY_LIGHT && (rect.rect.width() - 20.0).abs() < 0.1 && (rect.rect.height() - 58.0).abs() < 0.1 => handles.push(rect.rect),
                _ => {}
            }
        }
        handles.sort_by(|a, b| a.left().total_cmp(&b.left()));
        assert_eq!(handles.len(), 2, "timeline handles missing");
        let start_handle = handles[0].center();
        let moved = start_handle + vec2(150.0, 0.0);
        let _ = pass(&mut app, editor_size, vec![egui::Event::PointerMoved(start_handle), egui::Event::PointerButton { pos: start_handle, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
        let _ = pass(&mut app, editor_size, vec![egui::Event::PointerMoved(moved)]);
        let _ = pass(&mut app, editor_size, vec![egui::Event::PointerButton { pos: moved, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        assert!(app.pending_trim.as_ref().and_then(|(_, start, _)| parse_trim_time(start)).is_some_and(|start| start > 0), "dragging the trim handle did not change the start");
        app.pending_trim = None;
        let path = app.config.gallery_files[0].clone();
        let player = crate::platform::player::Player::open(parent.0 as isize, &path).unwrap();
        player.set_volume(0.0).unwrap();
        app.player = Some((path, player, 0.0));
        app.hidden = false;
        app.escape_navigation(&ctx);
        assert!(app.player.is_none());
        assert!(!app.hidden);
        unsafe { let _ = DestroyWindow(parent); CoUninitialize(); }
    }
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn application_audio_failure_is_reported_once_per_error() {
        let mut app = test_app();
        app.report_app_audio_error(Some("capture failed".into()));
        assert!(app.status.as_ref().is_some_and(|status| status.error && status.text.contains("capture failed")));
        app.status = None;
        app.report_app_audio_error(Some("capture failed".into()));
        assert!(app.status.is_none());
        app.report_app_audio_error(None);
        app.report_app_audio_error(Some("capture failed".into()));
        assert!(app.status.as_ref().is_some_and(|status| status.error));
    }

    #[test]
    fn warns_only_when_explicit_output_and_app_audio_can_overlap() {
        let mut config = Config::default();
        config.app_audio = Some("player.exe".into());
        config.system_audio = true;
        assert!(!audio_overlap(&config));
        config.system_device = Some("device".into());
        assert!(audio_overlap(&config));
        config.system_audio = false;
        assert!(!audio_overlap(&config));
    }

    /// The interface language is global; tests that depend on it take this lock.
    static LANGUAGE: Mutex<()> = Mutex::new(());

    #[test]
    fn storage_sizes_show_small_files_without_rounding_to_zero() {
        assert_eq!(storage_size(0, Language::Russian), "0 Б");
        assert_eq!(storage_size(999, Language::Russian), "999 Б");
        assert_eq!(storage_size(21_100, Language::Russian), "21.1 КБ");
        assert_eq!(storage_size(1_250_000, Language::English), "1.2 MB");
        assert_eq!(storage_size(1_500_000_000, Language::Russian), "1.5 ГБ");
    }

    #[test]
    fn storage_panel_stays_inside_narrow_views_in_all_languages() {
        let dir = tempfile::tempdir().unwrap();
        for language in [Language::Russian, Language::English, Language::Spanish] {
            let ctx = egui::Context::default();
            let mut state = StoragePanel {
                folder: Some(dir.path().to_owned()),
                report: Some(crate::storage::StorageReport { free: Some(999_000_000_000), folder_exists: true, videos: 125_000_000, screenshots: 2_000, ..Default::default() }),
                ..Default::default()
            };
            for _ in 0..2 {
                let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(320.0, 600.0))), ..Default::default() }, |ui| {
                    storage_panel(ui, &mut state, dir.path(), &[], language);
                });
                output.textures_delta.clear();
                let mut shapes: Vec<_> = output.shapes.into_iter().map(|shape| shape.shape).collect();
                while let Some(shape) = shapes.pop() {
                    match shape {
                        egui::Shape::Vec(inner) => shapes.extend(inner),
                        egui::Shape::Text(text) => {
                            let bounds = text.visual_bounding_rect();
                            assert!(bounds.left() >= 0.0 && bounds.right() <= 320.0, "{language:?}: {} outside screen: {bounds:?}", text.galley.text());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    #[test]
    fn storage_panel_refreshes_on_request_without_scanning_each_frame() {
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("test — Video.mp4");
        std::fs::write(&video, [1; 10]).unwrap();
        let files = [video.clone()];
        let ctx = egui::Context::default();
        let mut state = StoragePanel::default();
        let draw = |state: &mut StoragePanel, events| {
            let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 900.0))), events, ..Default::default() }, |ui| {
                storage_panel(ui, state, dir.path(), &files, Language::Russian);
            });
            output.textures_delta.clear();
            output.shapes
        };
        let _ = draw(&mut state, vec![]);
        let deadline = Instant::now() + Duration::from_secs(3);
        while state.report.is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
            let _ = draw(&mut state, vec![]);
        }
        assert_eq!(state.report.as_ref().unwrap().videos, 10);
        std::fs::write(&video, [1; 25]).unwrap();
        let mut shapes: Vec<_> = draw(&mut state, vec![]).into_iter().map(|shape| shape.shape).collect();
        assert_eq!(state.report.as_ref().unwrap().videos, 10);
        let mut refresh = None;
        while let Some(shape) = shapes.pop() {
            match shape {
                egui::Shape::Vec(inner) => shapes.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == "Обновить" => refresh = Some(text.visual_bounding_rect().center()),
                _ => {}
            }
        }
        let refresh = refresh.expect("refresh button missing");
        let pointer = |pressed| egui::Event::PointerButton { pos: refresh, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let _ = draw(&mut state, vec![egui::Event::PointerMoved(refresh), pointer(true)]);
        let _ = draw(&mut state, vec![pointer(false)]);
        let deadline = Instant::now() + Duration::from_secs(3);
        while state.loading.is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
            let _ = draw(&mut state, vec![]);
        }
        assert_eq!(state.report.as_ref().unwrap().videos, 25);
        assert!(state.loading.is_none());
    }

    fn test_app() -> App {
        App {
            config: Config { setup_complete: true, ..Config::default() },
            sources: Vec::new(),
            selected: None,
            system_devices: Vec::new(),
            mic_devices: Vec::new(),
            preview: None,
            rec: RecState::Idle,
            screenshots: Vec::new(),
            system: None,
            mic: None,
            app_audio: None,
            audio_apps: Vec::new(),
            cameras: Vec::new(),
            camera: None,
            camera_texture: None,
            camera_seq: None,
            meters: [0.0; 3],
            ctx: egui::Context::default(),
            last_drawn: Instant::now(),
            texture: None,
            texture_seq: None,
            picking_region: false,
            region_drag: None,
            status: None,
            notification: None,
            settings: None,
            storage_panel: StoragePanel::default(),
            cpu: CpuMeter { last: None, percent: 0.0 },
            system_meter: SystemMeter::default(),
            hud_meter: Arc::new(Mutex::new(SystemMeter::default())),
            actions: Arc::new(Mutex::new(Vec::new())),
            hotkeys: None,
            bound_actions: Arc::default(),
            native_window: None,
            tray: None,
            tray_commands: Arc::new(Mutex::new(Vec::new())),
            hidden: false,
            opening: None,
            closing: None,
            #[cfg(windows)]
            focus_armed: false,
            exit_requested: false,
            advanced_view: false,
            last_mic_error: None,
            last_system_error: None,
            last_app_error: None,
            pending_delete: None,
            pending_rename: None,
            image_viewer: None,
            #[cfg(windows)]
            player: None,
            #[cfg(windows)]
            player_error: None,
            #[cfg(windows)]
            pending_trim: None,
            #[cfg(windows)]
            trim_frames: Vec::new(),
            #[cfg(windows)]
            trim_loading: None,
            #[cfg(windows)]
            trim_requested: false,
            #[cfg(windows)]
            trim_error: None,
            gallery_deletions: Vec::new(),
            gallery_thumbnails: HashMap::new(),
            gallery_loading: HashMap::new(),
            disk_check: None,
            last_disk_check: None,
            low_disk_warned: false,
            #[cfg(windows)]
            desktop_available: crate::platform::sys::interactive_desktop_available(),
            #[cfg(windows)]
            desktop_checked: Instant::now(),
            #[cfg(windows)]
            desktop_resume: None,
            update: None,
            update_check: None,
            update_dialog: false,
            jobs: Vec::new(),
            named: HashSet::new(),
        }
    }

    #[test]
    fn menu_hotkey_minimizes_and_restores_window() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.handle(ui.ctx(), Action::ToggleMenu);
            assert!(app.closing.is_some());
            app.advance_window_animation(ui.ctx(), Instant::now() + Duration::from_millis(200));
        });
        output.textures_delta.clear();
        let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(commands.iter().any(|c| matches!(c, egui::ViewportCommand::Minimized(true))));
        let mut input = egui::RawInput::default();
        input.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().minimized = Some(true);
        let mut output = ctx.run_ui(input, |ui| app.handle(ui.ctx(), Action::ToggleMenu));
        output.textures_delta.clear();
        let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(commands.iter().any(|c| matches!(c, egui::ViewportCommand::Minimized(false))));
        assert!(commands.iter().any(|c| matches!(c, egui::ViewportCommand::Focus)));
    }

    #[test]
    fn rapid_menu_toggle_closes_while_viewport_still_reports_minimized() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.hidden = true;
        let mut input = egui::RawInput::default();
        input.viewports.get_mut(&egui::ViewportId::ROOT).unwrap().minimized = Some(true);
        let mut output = ctx.run_ui(input, |ui| {
            app.handle(ui.ctx(), Action::ToggleMenu);
            assert!(!app.hidden && app.opening.is_some());
            app.handle(ui.ctx(), Action::ToggleMenu);
            assert!(app.closing.is_some());
            app.advance_window_animation(ui.ctx(), Instant::now() + Duration::from_millis(200));
        });
        output.textures_delta.clear();
        assert!(app.hidden && app.opening.is_none());
        let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(commands.iter().any(|command| matches!(command, egui::ViewportCommand::Minimized(false))));
        assert!(commands.iter().any(|command| matches!(command, egui::ViewportCommand::Minimized(true))));
    }

    #[test]
    fn reopening_during_close_reverses_from_current_position() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.hide_window(ui.ctx());
            let start = app.closing.as_ref().unwrap().last;
            app.advance_window_animation(ui.ctx(), start + Duration::from_millis(60));
            let at_reversal = app.closing.as_ref().unwrap().position();
            assert!(at_reversal.x < 0.0 && at_reversal.x > -420.0);
            app.handle(ui.ctx(), Action::ToggleMenu);
            let opening = app.opening.as_ref().unwrap();
            assert_eq!(opening.from, at_reversal);
            assert_eq!(opening.to, pos2(0.0, 0.0));
            assert!(!app.hidden && app.closing.is_none());
            app.advance_window_animation(ui.ctx(), Instant::now() + Duration::from_millis(200));
            assert!(app.opening.is_none());
        });
        output.textures_delta.clear();
        let commands = &output.viewport_output[&egui::ViewportId::ROOT].commands;
        assert!(!commands.iter().any(|command| matches!(command, egui::ViewportCommand::Minimized(true))));
    }

    #[cfg(windows)]
    #[test]
    fn gallery_hover_actions_target_the_selected_video() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let path = PathBuf::from(r"Z:\opnoverlay-test-missing.mp4");
        let pass = |app: &mut App, events| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 500.0))), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| app.gallery_card(ui, &path, 120.0));
            });
            output.textures_delta.clear();
            output.shapes
        };
        let _ = pass(&mut app, vec![]);
        let mut shapes: Vec<egui::Shape> = pass(&mut app, vec![]).into_iter().map(|shape| shape.shape).collect();
        let mut center = None;
        while let Some(shape) = shapes.pop() {
            match shape {
                egui::Shape::Vec(inner) => shapes.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == "ВИДЕО" => center = Some(text.visual_bounding_rect().center()),
                _ => {}
            }
        }
        let center = center.expect("video card was not rendered");
        let click = |app: &mut App, pos| {
            let _ = pass(app, vec![egui::Event::PointerMoved(pos)]);
            let _ = pass(app, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
            let _ = pass(app, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        };
        click(&mut app, center + vec2(6.0, 40.0));
        assert_eq!(app.pending_trim.as_ref().map(|item| &item.0), Some(&path));
        assert!(!app.hidden);
        click(&mut app, center + vec2(40.0, 40.0));
        assert_eq!(app.pending_delete.as_ref(), Some(&path));
        assert!(!app.hidden);
    }

    #[test]
    fn gallery_actions_remain_available_when_keyboard_focus_moves_to_them() {
        gallery_keyboard_action("keyboard-test.png", 2, false);
        #[cfg(windows)]
        gallery_keyboard_action("keyboard-test.mp4", 3, true);
    }

    fn gallery_keyboard_action(name: &str, tab_count: usize, trim: bool) {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let path = PathBuf::from(name);
        let pass = |app: &mut App, events| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 500.0))), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| app.gallery_card(ui, &path, 120.0));
            });
            output.textures_delta.clear();
        };
        let key = |key, pressed| egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE };
        pass(&mut app, vec![]);
        for _ in 0..tab_count {
            pass(&mut app, vec![key(egui::Key::Tab, true)]);
            pass(&mut app, vec![key(egui::Key::Tab, false)]);
        }
        pass(&mut app, vec![]);
        pass(&mut app, vec![key(egui::Key::Space, true)]);
        pass(&mut app, vec![key(egui::Key::Space, false)]);
        if trim {
            #[cfg(windows)]
            assert_eq!(app.pending_trim.as_ref().map(|item| item.0.as_path()), Some(path.as_path()));
        } else {
            assert_eq!(app.pending_delete.as_deref(), Some(path.as_path()));
        }
    }

    #[test]
    fn gallery_keeps_only_existing_unique_files() {
        let dir = std::env::temp_dir().join(format!("opnoverlay-gallery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.mp4");
        let missing = dir.join("missing.mp4");
        std::fs::write(&file, b"test").unwrap();
        let mut files = Vec::new();
        assert!(!add_gallery_file(&mut files, &missing));
        assert!(add_gallery_file(&mut files, &file));
        assert!(!add_gallery_file(&mut files, &file));
        assert_eq!(files, vec![file.clone()]);
        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn gallery_rename_rejects_unsafe_names_and_preserves_existing_files() {
        let dir = std::env::temp_dir().join(format!("opnoverlay-rename-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("old.mp4");
        let existing = dir.join("taken.mp4");
        std::fs::write(&source, b"old").unwrap();
        std::fs::write(&existing, b"keep").unwrap();
        for name in ["", "..", "bad/name", "trailing.", "NUL", "COM¹", "LPT3.txt"] {
            assert!(gallery_rename_target(&source, name).is_err(), "{name}");
        }
        let target = gallery_rename_target(&source, "новое имя").unwrap();
        assert_eq!(target, dir.join("новое имя.mp4"));
        assert!(crate::platform::sys::rename_file(&source, &existing).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"old");
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep");
        crate::platform::sys::rename_file(&source, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"old");
        std::fs::remove_file(target).unwrap();
        std::fs::remove_file(existing).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn multiple_screenshot_results_are_processed_without_losing_pending_ones() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();
        app.screenshots.extend([first_rx, second_rx]);
        first_tx.send(Ok(PathBuf::from("first.png"))).unwrap();
        app.poll(&ctx);
        assert_eq!(app.screenshots.len(), 1);
        assert_eq!(app.status.as_ref().unwrap().file.as_deref(), Some(Path::new("first.png")));
        second_tx.send(Ok(PathBuf::from("second.jpg"))).unwrap();
        app.poll(&ctx);
        assert!(app.screenshots.is_empty());
        assert_eq!(app.status.as_ref().unwrap().file.as_deref(), Some(Path::new("second.jpg")));
    }

    #[test]
    fn notifications_follow_setting_and_expire() {
        let mut app = test_app();
        app.config.notifications = true;
        app.set_status("Откат сохранён: C:\\video.mp4", Some(PathBuf::from("video.mp4")), false);
        assert_eq!(app.notification.as_ref().unwrap().1, "Откат сохранён");
        app.notification.as_mut().unwrap().0 = Instant::now() - Duration::from_secs(5);
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show_notification(ui.ctx()));
        output.textures_delta.clear();
        assert!(app.notification.is_none());
        app.config.notifications = false;
        app.set_status("Ошибка сохранения", None, true);
        assert!(app.notification.is_none());
    }

    #[test]
    fn saved_notifications_do_not_include_file_paths() {
        let mut app = test_app();
        app.config.notifications = true;
        for (message, expected) in [
            ("Откат сохранён: C:\\video.mp4", "Откат сохранён"),
            ("Replay saved: C:\\video.mp4", "Replay saved"),
            ("Repetición guardada: C:\\video.mp4", "Repetición guardada"),
        ] {
            app.set_status(message, Some(PathBuf::from("video.mp4")), false);
            assert_eq!(app.notification.as_ref().unwrap().1, expected);
        }
    }

    #[test]
    fn common_notifications_share_the_same_size_and_keep_their_message() {
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(false);
        let mut app = test_app();
        for message in [
            "Скриншот сохранён", "Откат сохранён", "Запись начата", "Запись остановлена · сохранение", "Запись сохранена",
            "Screenshot saved", "Replay saved", "Recording started", "Recording stopped · saving", "Recording saved",
            "Captura guardada", "Repetición guardada", "Grabación iniciada", "Grabación detenida · guardando", "Grabación guardada",
        ] {
            let started = Instant::now() - Duration::from_millis(250);
            app.notification = Some((started, message.into(), false, Arc::new(OnceLock::from(started))));
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show_notification(ui.ctx()));
            let viewport = output.viewport_output.get(&egui::ViewportId::from_hash_of("notification")).unwrap();
            assert_eq!(viewport.builder.inner_size, Some(vec2(280.0, 62.0)), "inconsistent notification size for {message}");
            let callback = viewport.viewport_ui_cb.clone().unwrap();
            output.textures_delta.clear();
            let mut painted = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(280.0, 62.0))), ..Default::default() }, |ui| callback(ui));
            painted.textures_delta.clear();
            let mut shapes: Vec<_> = painted.shapes.into_iter().map(|shape| shape.shape).collect();
            let mut found = false;
            while let Some(shape) = shapes.pop() {
                match shape {
                    egui::Shape::Vec(inner) => shapes.extend(inner),
                    egui::Shape::Text(text) if text.galley.text() == message => {
                        assert!(!text.galley.elided && text.visual_bounding_rect().bottom() <= 62.0);
                        found = true;
                    }
                    _ => {}
                }
            }
            assert!(found, "notification message missing: {message}");
        }
    }

    #[test]
    fn notification_viewport_repaints_the_fade() {
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(false);
        let mut app = test_app();
        app.notification = Some((Instant::now() - Duration::from_millis(500), "Скриншот сохранён".into(), false, Arc::new(OnceLock::new())));
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show_notification(ui.ctx()));
        let id = egui::ViewportId::from_hash_of("notification");
        let callback = output.viewport_output.get(&id).unwrap().viewport_ui_cb.clone().unwrap();
        output.textures_delta.clear();
        let paint = || {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| callback(ui));
            out.textures_delta.clear();
            let mut shapes: Vec<_> = out.shapes.into_iter().map(|shape| shape.shape).collect();
            let mut alpha = 0;
            while let Some(shape) = shapes.pop() {
                match shape {
                    egui::Shape::Vec(inner) => shapes.extend(inner),
                    egui::Shape::Rect(rect) => alpha = alpha.max(rect.fill.a()),
                    _ => {}
                }
            }
            alpha
        };
        let initial = paint();
        thread::sleep(Duration::from_millis(250));
        let full = paint();
        assert!(full > initial, "notification opacity stayed at {initial}, then {full}");
    }

    #[test]
    fn long_notification_fits_its_viewport_and_shows_it_was_truncated() {
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(false);
        let mut app = test_app();
        let message = "Не удалось сохранить запись: ".to_owned() + &"очень длинное сообщение об ошибке ".repeat(12);
        app.config.notifications = true;
        app.set_status(message.clone(), None, true);
        let _ = app.notification.as_ref().unwrap().3.set(Instant::now() - Duration::from_millis(250));
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show_notification(ui.ctx()));
        let viewport = output.viewport_output.get(&egui::ViewportId::from_hash_of("notification")).unwrap();
        let size = viewport.builder.inner_size.unwrap();
        let callback = viewport.viewport_ui_cb.clone().unwrap();
        output.textures_delta.clear();
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), size)), ..Default::default() };
        let mut painted = ctx.run_ui(input, |ui| callback(ui));
        painted.textures_delta.clear();
        let mut shapes: Vec<_> = painted.shapes.into_iter().map(|shape| shape.shape).collect();
        let mut found = false;
        let mut hint_found = false;
        while let Some(shape) = shapes.pop() {
            match shape {
                egui::Shape::Vec(inner) => shapes.extend(inner),
                egui::Shape::Text(text) if text.galley.text().contains("Не удалось сохранить запись") => {
                    assert!(text.galley.elided, "long notification should show truncation");
                    assert!(text.visual_bounding_rect().bottom() <= size.y, "notification text exceeds {size:?}");
                    found = true;
                }
                egui::Shape::Text(text) if ["Подробнее в меню", "Details in menu", "Más información en el menú"].iter().any(|hint| text.galley.text().contains(hint)) => {
                    assert!(text.visual_bounding_rect().bottom() <= size.y, "notification hint exceeds {size:?}");
                    hint_found = true;
                }
                _ => {}
            }
        }
        assert!(found && hint_found);
    }

    #[test]
    fn notification_viewport_fades_out_before_disappearing() {
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(false);
        let mut app = test_app();
        let started = Instant::now() - Duration::from_millis(3800);
        app.notification = Some((started, "Скриншот сохранён".into(), false, Arc::new(OnceLock::from(started))));
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.show_notification(ui.ctx()));
        let id = egui::ViewportId::from_hash_of("notification");
        let callback = output.viewport_output.get(&id).unwrap().viewport_ui_cb.clone().unwrap();
        output.textures_delta.clear();
        let paint = || {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| callback(ui));
            out.textures_delta.clear();
            let mut shapes: Vec<_> = out.shapes.into_iter().map(|shape| shape.shape).collect();
            let mut alpha = 0;
            while let Some(shape) = shapes.pop() {
                match shape {
                    egui::Shape::Vec(inner) => shapes.extend(inner),
                    egui::Shape::Rect(rect) => alpha = alpha.max(rect.fill.a()),
                    _ => {}
                }
            }
            alpha
        };
        let initial = paint();
        thread::sleep(Duration::from_millis(100));
        let later = paint();
        assert!(initial > later, "notification opacity stayed at {initial}, then {later}");
    }

    #[test]
    fn interrupted_recording_save_reports_error_and_allows_another_recording() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        app.rec = RecState::Saving(rx);
        app.poll(&ctx);
        assert!(matches!(app.rec, RecState::Saving(_)));
        drop(tx);
        app.poll(&ctx);
        assert!(matches!(app.rec, RecState::Idle));
        assert!(app.status.as_ref().is_some_and(|status| status.error && status.text.contains("обработчик сохранения")));
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn restoring_desktop_restarts_live_replay_capture() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.replay_storage = ReplayStorage::Memory;
        app.refresh_sources();
        assert!(app.selected.is_some());
        app.start_replay(&ctx);
        assert!(app.is_replaying());
        let old = app.active_session().unwrap().shared.clone();
        let deadline = Instant::now() + Duration::from_secs(8);
        while old.frames.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(old.frames.load(Ordering::Relaxed) > 0);
        app.desktop_available = false;
        app.desktop_checked = Instant::now() - Duration::from_secs(2);
        app.poll(&ctx);
        assert!(app.desktop_available);
        assert!(app.is_replaying());
        let new = app.active_session().unwrap().shared.clone();
        assert!(!Arc::ptr_eq(&old, &new));
        let deadline = Instant::now() + Duration::from_secs(8);
        while new.frames.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(new.frames.load(Ordering::Relaxed) > 0);
        app.stop_session(&ctx, None);
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn restoring_desktop_continues_recording_in_a_new_file() {
        assert!(std::env::var_os("OPNOVERLAY_CONFIG_DIR").is_some());
        let folder = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.output_dir = folder.path().to_owned();
        app.config.replay_dir = folder.path().to_owned();
        app.config.replay_storage = ReplayStorage::Memory;
        app.refresh_sources();
        app.start_replay(&ctx);
        app.start_recording(&ctx);
        assert!(app.is_recording() && app.is_replaying());
        let deadline = Instant::now() + Duration::from_secs(8);
        while !app.is_live() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(50)); }
        assert!(app.is_live());
        std::thread::sleep(Duration::from_millis(500));
        app.restore_capture_after_desktop_change(&ctx);
        let deadline = Instant::now() + Duration::from_secs(12);
        while !app.is_recording() && Instant::now() < deadline {
            app.poll(&ctx);
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(app.is_recording() && app.is_replaying());
        assert_eq!(app.config.gallery_files.len(), 1);
        let first = app.config.gallery_files[0].clone();
        assert!(first.is_file());
        let deadline = Instant::now() + Duration::from_secs(8);
        while !app.is_live() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(50)); }
        assert!(app.is_live());
        std::thread::sleep(Duration::from_millis(500));
        app.stop_session(&ctx, None);
        let deadline = Instant::now() + Duration::from_secs(12);
        while !matches!(app.rec, RecState::Idle) && Instant::now() < deadline {
            app.poll(&ctx);
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(matches!(app.rec, RecState::Idle));
        assert_eq!(app.config.gallery_files.len(), 2);
        assert_ne!(first, app.config.gallery_files[1]);
        assert!(app.config.gallery_files[1].is_file());
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn restoring_desktop_keeps_recording_paused() {
        assert!(std::env::var_os("OPNOVERLAY_CONFIG_DIR").is_some());
        let folder = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.output_dir = folder.path().to_owned();
        app.refresh_sources();
        app.start_recording(&ctx);
        let deadline = Instant::now() + Duration::from_secs(8);
        while !app.is_live() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(50)); }
        assert!(app.is_live());
        std::thread::sleep(Duration::from_millis(500));
        app.handle(&ctx, Action::TogglePause);
        assert!(app.is_paused());
        let home_contains = |app: &mut App, label: &str| {
            let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 900.0))), ..Default::default() }, |ui| app.overlay_home(ui));
            output.textures_delta.clear();
            let mut shapes: Vec<_> = output.shapes.into_iter().map(|shape| shape.shape).collect();
            while let Some(shape) = shapes.pop() {
                match shape {
                    egui::Shape::Vec(inner) => shapes.extend(inner),
                    egui::Shape::Text(text) if text.galley.text() == label => return true,
                    _ => {}
                }
            }
            false
        };
        assert!(home_contains(&mut app, "Запись на паузе"));
        app.restore_capture_after_desktop_change(&ctx);
        let deadline = Instant::now() + Duration::from_secs(12);
        while !app.is_recording() && Instant::now() < deadline {
            app.poll(&ctx);
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(app.is_recording());
        assert!(app.is_paused());
        let deadline = Instant::now() + Duration::from_secs(8);
        while !app.is_live() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(50)); }
        assert!(app.is_live());
        app.handle(&ctx, Action::TogglePause);
        assert!(!app.is_paused());
        assert!(home_contains(&mut app, "Запись идёт"));
        std::thread::sleep(Duration::from_millis(500));
        app.stop_session(&ctx, None);
        let deadline = Instant::now() + Duration::from_secs(12);
        while !matches!(app.rec, RecState::Idle) && Instant::now() < deadline {
            app.poll(&ctx);
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(matches!(app.rec, RecState::Idle));
        assert_eq!(app.config.gallery_files.len(), 2);
        assert!(app.config.gallery_files.iter().all(|path| path.is_file()));
    }

    #[test]
    fn overlay_home_renders_without_capture_session() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 700.0))),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| app.overlay_home(ui));
        assert!(!output.shapes.is_empty());
        output.textures_delta.clear();
    }

    #[test]
    fn gallery_filename_does_not_move_on_hover() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let path = PathBuf::from("video-test.mp4");
        let mut positions = Vec::new();
        for point in [pos2(400.0, 400.0), pos2(60.0, 60.0), pos2(400.0, 400.0)] {
            let mut found = None;
            for _ in 0..3 {
                let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(500.0, 500.0))), events: vec![egui::Event::PointerMoved(point)], ..Default::default() };
                let mut output = ctx.run_ui(input, |ui| { egui::CentralPanel::default().show(ui, |ui| app.gallery_card(ui, &path, 120.0)); });
                output.textures_delta.clear();
                let mut shapes: Vec<_> = output.shapes.into_iter().map(|shape| shape.shape).collect();
                while let Some(shape) = shapes.pop() {
                    match shape {
                        egui::Shape::Vec(inner) => shapes.extend(inner),
                        egui::Shape::Text(text) if text.galley.text() == "video-test.mp4" => found = Some(text.visual_bounding_rect().top()),
                        _ => {}
                    }
                }
            }
            positions.push(found.expect("gallery filename missing"));
        }
        assert!(positions.iter().all(|y| (*y - positions[0]).abs() < 0.1), "filename moved: {positions:?}");
    }

    #[test]
    fn gallery_position_does_not_change_with_status() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.language = Some(Language::Russian);
        let mut positions = Vec::new();
        for status in [None, Some("Скриншот сохранён"), Some("Очень длинное сообщение об ошибке, которое не должно переноситься на следующую строку и сдвигать галерею при появлении") ] {
            app.status = status.map(|text| Status { text: text.into(), file: None, error: true });
            let mut gallery = None;
            for _ in 0..3 {
                let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 1200.0))), ..Default::default() };
                let mut output = ctx.run_ui(input, |ui| app.overlay_home(ui));
                output.textures_delta.clear();
                let mut shapes: Vec<_> = output.shapes.into_iter().map(|shape| shape.shape).collect();
                while let Some(shape) = shapes.pop() {
                    match shape {
                        egui::Shape::Vec(inner) => shapes.extend(inner),
                        egui::Shape::Text(text) if text.galley.text() == "ГАЛЕРЕЯ" => gallery = Some(text.visual_bounding_rect().top()),
                        _ => {}
                    }
                }
            }
            positions.push(gallery.expect("gallery label missing"));
        }
        assert_eq!(positions.len(), 3);
        assert!(positions.iter().all(|y| (*y - positions[0]).abs() < 0.1), "gallery moved: {positions:?}");
    }

    #[test]
    fn opening_animation_uses_elapsed_time_and_finishes() {
        let now = Instant::now();
        let mut animation = OpeningAnimation { last: now, progress: 0.0, from: pos2(-420.0, 0.0), to: pos2(0.0, 0.0), rest: pos2(0.0, 0.0), duration: 0.14 };
        let first = animation.advance(now + Duration::from_millis(100));
        assert!((first - 100.0 / 140.0).abs() < 0.001, "a delayed frame must not prolong the animation");
        let mut previous = first;
        for step in 1..=20 {
            let progress = animation.advance(now + Duration::from_millis(100 + step * 16));
            assert!(progress >= previous && progress <= 1.0);
            previous = progress;
        }
        assert_eq!(previous, 1.0);
    }

    #[test]
    fn home_actions_follow_selected_language() {
        for (language, labels) in [
            (Language::Russian, ["Настройки", "Откаты выключены", "Начать запись"]),
            (Language::English, ["Settings", "Replay disabled", "Start recording"]),
            (Language::Spanish, ["Ajustes", "Repetición desactivada", "Iniciar grabación"]),
        ] {
            let ctx = egui::Context::default();
            let mut app = test_app();
            app.config.language = Some(language);
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 900.0))), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| app.overlay_home(ui));
            output.textures_delta.clear();
            let mut pending: Vec<_> = output.shapes.into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
            let mut found = [false; 3];
            while let Some((shape, clip)) = pending.pop() {
                match shape {
                    egui::Shape::Vec(inner) => pending.extend(inner.into_iter().map(|shape| (shape, clip))),
                    egui::Shape::Text(text) => for (index, label) in labels.iter().enumerate() {
                        if text.galley.text().contains(label) && clip.contains_rect(text.visual_bounding_rect()) { found[index] = true; }
                    },
                    _ => {}
                }
            }
            assert!(found.into_iter().all(|value| value), "missing translated action for {language:?}: {found:?}");
        }
    }

    #[test]
    fn translated_primary_actions_fit_narrow_home() {
        for (language, labels) in [
            (Language::Russian, ["Настройки", "Сохранить откат", "Начать запись", "Скриншот"]),
            (Language::English, ["Settings", "Save replay", "Start recording", "Screenshot"]),
            (Language::Spanish, ["Ajustes", "Guardar repetición", "Iniciar grabación", "Captura"]),
        ] {
            let ctx = egui::Context::default();
            let mut app = test_app();
            app.config.language = Some(language);
            let mut found = [false; 4];
            for _ in 0..3 {
                let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 900.0))), ..Default::default() };
                let mut output = ctx.run_ui(input, |ui| app.overlay_home(ui));
                output.textures_delta.clear();
                let mut pending: Vec<_> = output.shapes.into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
                while let Some((shape, clip)) = pending.pop() {
                    match shape {
                        egui::Shape::Vec(inner) => pending.extend(inner.into_iter().map(|shape| (shape, clip))),
                        egui::Shape::Text(text) => for (index, label) in labels.iter().enumerate() {
                            if text.galley.text().contains(label) && clip.contains_rect(text.visual_bounding_rect()) && text.visual_bounding_rect().max.x <= 360.0 { found[index] = true; }
                        },
                        _ => {}
                    }
                }
            }
            assert!(found.into_iter().all(|value| value), "clipped or missing action for {language:?}: {found:?}");
        }
    }

    #[test]
    fn viewer_return_label_follows_selected_language() {
        for (language, label) in [(Language::Russian, "В галерею"), (Language::English, "Gallery"), (Language::Spanish, "Galería")] {
            let ctx = egui::Context::default();
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(740.0, 600.0))), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| { egui::CentralPanel::default().show(ui, |ui| { viewer_header(ui, Path::new("sample.mp4"), "VIDEO", language); }); });
            output.textures_delta.clear();
            let mut pending: Vec<_> = output.shapes.into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
            let mut found = false;
            while let Some((shape, clip)) = pending.pop() {
                match shape {
                    egui::Shape::Vec(inner) => pending.extend(inner.into_iter().map(|shape| (shape, clip))),
                    egui::Shape::Text(text) if text.galley.text() == label && clip.contains_rect(text.visual_bounding_rect()) => found = true,
                    _ => {}
                }
            }
            assert!(found, "missing return control for {language:?}");
        }
    }

    #[test]
    fn deleting_gallery_file_starts_without_confirmation() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let dir = tempfile::tempdir().unwrap();
        for name in ["test.mp4", "test.png", "test.jpg"] {
            let path = dir.path().join(name);
            app.config.gallery_files.push(path.clone());
            app.pending_delete = Some(path.clone());
            app.delete_pending_file(&ctx);
            assert!(app.pending_delete.is_none());
            let receiver = app.gallery_deletions.pop().expect("delete was not started");
            let (target, result) = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(target, path);
            assert!(result.is_err());
            assert!(app.config.gallery_files.contains(&path));
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn deleting_gallery_file_moves_it_to_recycle_bin() {
        let path = PathBuf::from(std::env::var_os("OPNOVERLAY_RECYCLE_TEST_FILE").expect("test file required"));
        assert!(path.is_file());
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.gallery_files.push(path.clone());
        app.pending_delete = Some(path.clone());
        app.delete_pending_file(&ctx);
        assert!(app.pending_delete.is_none());
        let (target, result) = app.gallery_deletions.pop().unwrap().recv_timeout(Duration::from_secs(15)).unwrap();
        assert_eq!(target, path);
        result.unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn home_opens_sources_and_escape_returns_home() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let run = |app: &mut App, events| {
            let mut output = ctx.run_ui(egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 900.0))),
                events,
                ..Default::default()
            }, |ui| app.overlay_home(ui));
            output.textures_delta.clear();
            output.shapes
        };
        let mut shapes: Vec<_> = run(&mut app, vec![]).into_iter().map(|shape| shape.shape).collect();
        let mut target = None;
        while let Some(shape) = shapes.pop() {
            match shape {
                egui::Shape::Vec(inner) => shapes.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == "Экран и звук · подробнее" => target = Some(text.visual_bounding_rect().center()),
                _ => {}
            }
        }
        let pos = target.expect("sources and mixer button is visible");
        run(&mut app, vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
        run(&mut app, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        assert!(app.advanced_view);
        app.escape_navigation(&ctx);
        assert!(!app.advanced_view && !app.hidden);
    }

    #[test]
    fn narrow_sources_panel_scrolls_to_controls() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let pass = |app: &mut App, events| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 240.0))), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| { egui::CentralPanel::default().show(ui, |ui| app.docks(ui)); });
            output.textures_delta.clear();
            output.shapes
        };
        for _ in 0..3 { let _ = pass(&mut app, vec![]); }
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, -2000.0), phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Unknown, modifiers: egui::Modifiers::NONE };
        let _ = pass(&mut app, vec![egui::Event::PointerMoved(pos2(320.0, 200.0)), wheel]);
        for _ in 0..30 { let _ = pass(&mut app, vec![]); }
        let mut pending: Vec<_> = pass(&mut app, vec![]).into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
        let mut found = false;
        while let Some((shape, clip)) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner.into_iter().map(|shape| (shape, clip))),
                egui::Shape::Text(text) if ["Выход", "Exit", "Salir"].contains(&text.galley.text()) && clip.contains_rect(text.visual_bounding_rect()) => found = true,
                _ => {}
            }
        }
        assert!(found, "controls must be reachable by scrolling the narrow panel");
    }

    #[test]
    fn overlay_reports_recording_save_instead_of_offering_a_new_recording() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let (_sender, receiver) = mpsc::channel();
        app.rec = RecState::Saving(receiver);
        let mut output = ctx.run_ui(egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 900.0))),
            ..Default::default()
        }, |ui| app.overlay_home(ui));
        output.textures_delta.clear();
        let mut shapes: Vec<_> = output.shapes.into_iter().map(|shape| shape.shape).collect();
        let mut saving_label = false;
        let mut saving_action = false;
        while let Some(shape) = shapes.pop() {
            match shape {
                egui::Shape::Vec(inner) => shapes.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == "Сохранение…" => saving_label = true,
                egui::Shape::Text(text) if text.galley.text() == "Сохранение записи…" => saving_action = true,
                egui::Shape::Text(text) if text.galley.text().contains("Начать запись") => panic!("new recording offered while saving"),
                _ => {}
            }
        }
        assert!(saving_label && saving_action);
    }

    #[test]
    fn overlay_footer_stays_visible_with_a_full_gallery() {
        let folder = tempfile::tempdir().unwrap();
        let mut app = test_app();
        for index in 0..20 {
            let path = folder.path().join(format!("image-{index}.png"));
            std::fs::write(&path, b"test").unwrap();
            app.config.gallery_files.push(path);
        }
        for size in [vec2(360.0, 600.0), vec2(420.0, 900.0)] {
            let ctx = egui::Context::default();
            let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), size)), ..Default::default() }, |ui| app.overlay_home(ui));
            output.textures_delta.clear();
            let mut found = HashSet::new();
            let mut pending: Vec<_> = output.shapes.into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
            while let Some((shape, clip)) = pending.pop() {
                match shape {
                    egui::Shape::Vec(inner) => pending.extend(inner.into_iter().map(|shape| (shape, clip))),
                    egui::Shape::Text(text) if ["Показать показатели", "Выход из приложения"].contains(&text.galley.text()) => {
                        let rect = text.visual_bounding_rect();
                        assert!(clip.contains_rect(rect) && rect.bottom() <= size.y && rect.right() <= size.x, "footer clipped: {rect:?} in {clip:?}");
                        found.insert(text.galley.text().to_owned());
                    }
                    _ => {}
                }
            }
            assert_eq!(found.len(), 2, "footer missing at {size:?}: {found:?}");
        }
    }

    #[test]
    fn overlay_keyboard_navigation_reaches_actions_before_footer() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        let size = vec2(420.0, 900.0);
        let pass = |app: &mut App, events| {
            let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), size)), events, ..Default::default() }, |ui| app.overlay_home(ui));
            out.textures_delta.clear();
        };
        let key = |pressed| egui::Event::Key { key: egui::Key::Tab, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE };
        pass(&mut app, vec![]);
        let mut visited = Vec::new();
        for _ in 0..11 {
            pass(&mut app, vec![key(true)]);
            pass(&mut app, vec![key(false)]);
            if let Some(rect) = ctx.memory(|memory| memory.focused()).and_then(|id| ctx.read_response(id)).map(|response| response.rect) {
                visited.push(rect.center());
            }
        }
        assert_eq!(visited.len(), 11, "not all controls received focus: {visited:?}");
        assert!(visited[..2].iter().all(|position| position.y < size.y * 0.1), "header focus order: {visited:?}");
        assert!(visited[2..9].iter().all(|position| position.y > size.y * 0.1 && position.y < size.y * 0.7), "primary focus order: {visited:?}");
        assert!(visited[9..].iter().all(|position| position.y > size.y * 0.8), "footer focus order: {visited:?}");
    }

    #[test]
    fn gallery_scroll_reaches_old_files_while_footer_stays_visible() {
        let folder = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut app = test_app();
        for index in 0..20 {
            let path = folder.path().join(format!("f{index:02}.png"));
            std::fs::write(&path, b"test").unwrap();
            app.config.gallery_files.push(path);
        }
        let pass = |app: &mut App, events| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 600.0))), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| app.overlay_home(ui));
            output.textures_delta.clear();
            output.shapes
        };
        let _ = pass(&mut app, vec![]);
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, -3000.0), phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Unknown, modifiers: egui::Modifiers::NONE };
        let _ = pass(&mut app, vec![egui::Event::PointerMoved(pos2(200.0, 350.0)), wheel]);
        for _ in 0..30 { let _ = pass(&mut app, vec![]); }
        let shapes = pass(&mut app, vec![]);
        let mut found_file = false;
        let mut found_footer = false;
        let mut pending: Vec<_> = shapes.into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
        while let Some((shape, clip)) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner.into_iter().map(|shape| (shape, clip))),
                egui::Shape::Text(text) if clip.contains_rect(text.visual_bounding_rect()) => {
                    if text.galley.text() == "f00.png" { found_file = true; }
                    if text.galley.text() == "Выход из приложения" { found_footer = true; }
                }
                _ => {}
            }
        }
        assert!(found_file && found_footer, "scroll did not reveal the oldest file with the footer visible: {found_file} {found_footer}");
    }

    #[test]
    fn overlay_primary_controls_fit_narrow_panels() {
        for width in [360.0, 420.0] {
            let ctx = egui::Context::default();
            let mut app = test_app();
            app.config.hotkeys.save_replay = Some("Ctrl+Alt+Shift+F20".into());
            app.config.hotkeys.record = Some("Ctrl+Alt+Shift+F21".into());
            app.config.hotkeys.screenshot = Some("Ctrl+Alt+Shift+F22".into());
            let mut shapes = Vec::new();
            for _ in 0..3 {
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(width, 900.0))),
                    ..Default::default()
                };
                let mut output = ctx.run_ui(input, |ui| app.overlay_home(ui));
                output.textures_delta.clear();
                shapes = output.shapes;
            }
            let labels = ["Откаты выключены", "Сохранить откат", "Начать запись", "Скриншот", "Настройки", "Закрыть"];
            let mut found = [false; 6];
            let mut pending: Vec<egui::Shape> = shapes.into_iter().map(|shape| shape.shape).collect();
            while let Some(shape) = pending.pop() {
                match shape {
                    egui::Shape::Vec(inner) => pending.extend(inner),
                    egui::Shape::Text(text) => {
                        for (index, label) in labels.iter().enumerate() {
                            if text.galley.text().contains(label) {
                                let rect = text.visual_bounding_rect();
                                assert!(rect.min.x >= 0.0 && rect.max.x <= width, "{label}: {rect:?}");
                                if *label == "Начать запись" { assert!(rect.max.x <= width / 2.0, "{label}: {rect:?}"); }
                                if *label == "Скриншот" { assert!(rect.min.x >= width / 2.0, "{label}: {rect:?}"); }
                                found[index] = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            assert!(found.into_iter().all(|value| value), "missing primary controls: {found:?}");
        }
    }

    #[test]
    fn overlay_primary_buttons_dispatch_actions() {
        let folder = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.output_dir = folder.path().to_owned();
        let pass = |app: &mut App, events| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| app.overlay_home(ui));
            output.textures_delta.clear();
            output.shapes
        };
        for label in ["Сохранить откат", "Начать запись", "Скриншот"] {
            let _ = pass(&mut app, vec![]);
            let mut pending: Vec<egui::Shape> = pass(&mut app, vec![]).into_iter().map(|shape| shape.shape).collect();
            let mut target = None;
            while let Some(shape) = pending.pop() {
                match shape {
                    egui::Shape::Vec(inner) => pending.extend(inner),
                    egui::Shape::Text(text) if text.galley.text().contains(label) => {
                        target = Some(text.visual_bounding_rect().center());
                        break;
                    }
                    _ => {}
                }
            }
            let pos = target.expect(label);
            app.status = None;
            let _ = pass(&mut app, vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
            let _ = pass(&mut app, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
            assert!(app.status.as_ref().is_some_and(|status| status.error), "{label} did not report the missing source");
        }
    }

    #[test]
    fn first_run_choices_and_continue_fit_in_overlay() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.setup_complete = false;
        let mut shapes = Vec::new();
        for _ in 0..3 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 900.0))),
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| app.setup_dialog(ui.ctx()));
            output.textures_delta.clear();
            shapes = output.shapes;
        }
        let labels = ["Начальная настройка opnoverlay", "Оперативная память — меньше обращений к диску", "Временные файлы — меньше расход оперативной памяти", "Сохранить и продолжить"];
        let mut found = [false; 4];
        let mut pending: Vec<egui::Shape> = shapes.into_iter().map(|shape| shape.shape).collect();
        while let Some(shape) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner),
                egui::Shape::Text(text) => {
                    if let Some(index) = labels.iter().position(|label| *label == text.galley.text()) {
                        let rect = text.visual_bounding_rect();
                        assert!(rect.min.x >= 0.0 && rect.max.x <= 420.0 && rect.min.y >= 0.0 && rect.max.y <= 900.0, "{}: {rect:?}", labels[index]);
                        found[index] = true;
                    }
                }
                _ => {}
            }
        }
        assert!(found.into_iter().all(|value| value), "missing setup controls: {found:?}");
    }

    #[test]
    fn first_run_storage_and_replay_start_choices_change() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.setup_complete = false;
        let pass = |app: &mut App, events| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| app.setup_dialog(ui.ctx()));
            output.textures_delta.clear();
            output.shapes
        };
        let find = |shapes: Vec<egui::epaint::ClippedShape>, label: &str| {
            let mut pending: Vec<egui::Shape> = shapes.into_iter().map(|shape| shape.shape).collect();
            while let Some(shape) = pending.pop() {
                match shape {
                    egui::Shape::Vec(inner) => pending.extend(inner),
                    egui::Shape::Text(text) if text.galley.text() == label => return text.visual_bounding_rect().center(),
                    _ => {}
                }
            }
            panic!("missing control {label}");
        };
        let click = |app: &mut App, label| {
            let _ = pass(app, vec![]);
            let pos = find(pass(app, vec![]), label);
            let _ = pass(app, vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
            let _ = pass(app, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        };
        click(&mut app, "Временные файлы — меньше расход оперативной памяти");
        assert_eq!(app.config.replay_storage, ReplayStorage::Disk);
        click(&mut app, "Оперативная память — меньше обращений к диску");
        assert_eq!(app.config.replay_storage, ReplayStorage::Memory);
        assert!(app.config.auto_start_replay);
        click(&mut app, "Включать откаты при запуске приложения");
        assert!(!app.config.auto_start_replay);
        click(&mut app, "Включать откаты при запуске приложения");
        assert!(app.config.auto_start_replay);
        assert!(!app.config.setup_complete);
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn first_run_continue_persists_choices_without_changing_startup() {
        assert_eq!(crate::platform::sys::startup_enabled(), Ok(false));
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.setup_complete = false;
        app.config.auto_start_replay = false;
        app.config.auto_start_windows = false;
        let pass = |app: &mut App, events| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| app.setup_dialog(ui.ctx()));
            output.textures_delta.clear();
            output.shapes
        };
        let find = |shapes: Vec<egui::epaint::ClippedShape>, label: &str| {
            let mut pending: Vec<egui::Shape> = shapes.into_iter().map(|shape| shape.shape).collect();
            while let Some(shape) = pending.pop() {
                match shape {
                    egui::Shape::Vec(inner) => pending.extend(inner),
                    egui::Shape::Text(text) if text.galley.text() == label => return text.visual_bounding_rect().center(),
                    _ => {}
                }
            }
            panic!("missing control {label}");
        };
        let click = |app: &mut App, label| {
            let _ = pass(app, vec![]);
            let pos = find(pass(app, vec![]), label);
            let _ = pass(app, vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
            let _ = pass(app, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        };
        click(&mut app, "Временные файлы — меньше расход оперативной памяти");
        click(&mut app, "Сохранить и продолжить");
        assert!(app.config.setup_complete);
        let saved = Config::load();
        assert!(saved.setup_complete);
        assert_eq!(saved.replay_storage, ReplayStorage::Disk);
        assert!(!saved.auto_start_replay);
        assert!(!saved.auto_start_windows);
        assert_eq!(crate::platform::sys::startup_enabled(), Ok(false));
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn settings_save_capture_preferences_and_paths() {
        let _language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Language::Russian);
        let root = PathBuf::from(std::env::var_os("OPNOVERLAY_CONFIG_DIR").expect("isolated config directory required"));
        assert!(root.is_absolute());
        let ctx = egui::Context::default();
        let mut app = test_app();
        let mut draft = app.config.clone();
        draft.fps = 24;
        draft.quality = Quality::Medium;
        draft.resolution = Resolution::P720;
        draft.output_dir = root.join("recordings");
        draft.replay_dir = root.join("replays");
        draft.screenshot_dir = root.join("screenshots");
        draft.hotkeys.record = Some("Ctrl+Alt+F24".into());
        app.settings = Some(SettingsDialog { draft, page: SettingsPage::Output, capturing: None, replay_duration: "00:17".into(), selected_source: None });
        let pass = |app: &mut App, events| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| app.settings_dialog(ui.ctx()));
            output.textures_delta.clear();
            output.shapes
        };
        let _ = pass(&mut app, vec![]);
        let mut pending: Vec<egui::Shape> = pass(&mut app, vec![]).into_iter().map(|shape| shape.shape).collect();
        let mut save_pos = None;
        while let Some(shape) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == "Сохранить" => save_pos = Some(text.visual_bounding_rect().center()),
                _ => {}
            }
        }
        let pos = save_pos.expect("save button not rendered");
        let _ = pass(&mut app, vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
        let _ = pass(&mut app, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        assert!(app.settings.is_none());
        let saved = Config::load();
        assert_eq!(saved.fps, 24);
        assert_eq!(saved.quality, Quality::Medium);
        assert_eq!(saved.resolution, Resolution::P720);
        assert_eq!(saved.replay_seconds, 17);
        assert_eq!(saved.output_dir, root.join("recordings"));
        assert_eq!(saved.replay_dir, root.join("recordings"));
        assert_eq!(saved.screenshot_dir, root.join("recordings"));
        assert_eq!(saved.hotkeys.record.as_deref(), Some("Ctrl+Alt+F24"));
        crate::i18n::set(crate::i18n::Language::Spanish);
    }

    #[test]
    fn overlay_audio_buttons_enable_and_mute_sources() {
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.setup_complete = true;
        app.config.system_audio = false;
        app.config.mic = false;
        let pass = |app: &mut App, events| {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 1080.0))),
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| app.overlay_home(ui));
            output.textures_delta.clear();
            output.shapes
        };
        let find = |shapes: Vec<egui::epaint::ClippedShape>, label: &str| {
            let mut pending: Vec<egui::Shape> = shapes.into_iter().map(|shape| shape.shape).collect();
            while let Some(shape) = pending.pop() {
                match shape {
                    egui::Shape::Vec(inner) => pending.extend(inner),
                    egui::Shape::Text(text) if text.galley.text() == label => return text.visual_bounding_rect().center(),
                    _ => {}
                }
            }
            panic!("missing control {label}");
        };
        let click = |app: &mut App, label| {
            let _ = pass(app, vec![]);
            let pos = find(pass(app, vec![]), label);
            let _ = pass(app, vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
            let _ = pass(app, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        };
        click(&mut app, "Звук рабочего стола: выключен");
        assert!(app.config.system_audio && !app.config.system_muted && app.system.is_some());
        click(&mut app, "Звук рабочего стола: включён");
        assert!(app.config.system_muted);
        click(&mut app, "Звук рабочего стола: выключен");
        assert!(!app.config.system_muted);
        click(&mut app, "Микрофон: выключен");
        assert!(app.config.mic && !app.config.mic_muted && app.mic.is_some());
        click(&mut app, "Микрофон: включён");
        assert!(app.config.mic_muted);
        click(&mut app, "Микрофон: выключен");
        assert!(!app.config.mic_muted);
    }

    #[test]
    fn system_cpu_load_uses_idle_and_total_time_deltas() {
        assert_eq!(cpu_load((100, 1000), (150, 1200)), Some(75.0));
        assert_eq!(cpu_load((100, 1000), (300, 1200)), Some(0.0));
        assert_eq!(cpu_load((100, 1000), (100, 1000)), None);
        assert_eq!(cpu_load((100, 1000), (50, 1200)), None);
        assert_eq!(cpu_load((100, 1000), (400, 1200)), None);
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn gpu_polling_can_start_and_stop_without_an_overlay_window() {
        let mut meter = SystemMeter::default();
        meter.update_gpu(true);
        thread::sleep(Duration::from_millis(4_500));
        let receiver = meter.gpu_receiver.as_ref().unwrap();
        let sample = receiver.recv_timeout(Duration::from_secs(5)).expect("GPU polling did not answer");
        assert!(sample.utilization.is_some() || sample.temperature.is_some() || sample.memory.is_some());
        assert!(matches!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty)));
        meter.update_gpu(false);
        assert!(meter.gpu_receiver.is_none() && meter.gpu.is_none());
    }

    #[test]
    fn performance_hud_uses_the_selected_screen_corner() {
        let screen = vec2(1920.0, 1080.0);
        assert_eq!(hud_position(HudCorner::TopLeft, screen, 58.0), pos2(20.0, 20.0));
        assert_eq!(hud_position(HudCorner::TopRight, screen, 58.0), pos2(1710.0, 20.0));
        assert_eq!(hud_position(HudCorner::BottomLeft, screen, 58.0), pos2(20.0, 1002.0));
        assert_eq!(hud_position(HudCorner::BottomRight, screen, 58.0), pos2(1710.0, 1002.0));
        assert_eq!(hud_position(HudCorner::BottomRight, vec2(100.0, 40.0), 58.0), pos2(0.0, 0.0));
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn trimming_updates_video_and_restores_the_original() {
        let source = PathBuf::from(std::env::var_os("OPNOVERLAY_CHECK_FILE").unwrap());
        let folder = tempfile::tempdir().unwrap();
        let video = folder.path().join("video.mp4");
        std::fs::copy(source, &video).unwrap();
        let original = std::fs::read(&video).unwrap();
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.gallery_files.push(video.clone());
        app.pending_trim = Some((video.clone(), "00:01.123".into(), "00:03.456".into()));
        app.save_trim(&ctx);
        assert!(app.pending_trim.is_none());
        assert_eq!(app.jobs.len(), 1);
        let result = app.jobs.pop().unwrap().join().unwrap();
        assert!(!result.error, "{}", result.text);
        assert!(result.text.contains("1.1") && result.text.contains("3.4"), "unexpected range: {}", result.text);
        let output = result.file.unwrap();
        assert_eq!(output, video);
        assert!(output.is_file());
        assert_ne!(std::fs::read(&video).unwrap(), original);
        assert!(crate::platform::edit::has_original(&video));
        crate::platform::edit::restore(&video).unwrap();
        assert_eq!(std::fs::read(&video).unwrap(), original);
    }

    #[cfg(windows)]
    #[test]
    #[ignore]
    fn restore_button_restores_a_trimmed_video_in_the_gallery() {
        use windows::core::w;
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
        use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPED};
        let source = PathBuf::from(std::env::var_os("OPNOVERLAY_CHECK_FILE").unwrap());
        let folder = tempfile::tempdir().unwrap();
        let video = folder.path().join("video.mp4");
        std::fs::copy(source, &video).unwrap();
        let original = std::fs::read(&video).unwrap();
        let source_for_trim = video.clone();
        thread::spawn(move || crate::platform::edit::trim_current(&source_for_trim, 0, 60_000_000)).join().unwrap().unwrap();
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap(); }
        let parent = unsafe { CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"), w!("player test"), WS_OVERLAPPED, 0, 0, 64, 64, None, None, None, None).unwrap() };
        let player = crate::platform::player::Player::open(parent.0 as isize, &video).unwrap();
        player.set_volume(0.0).unwrap();
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.config.language = Some(Language::Russian);
        app.config.gallery_files.push(video.clone());
        app.player = Some((video.clone(), player, 0.0));
        let pass = |app: &mut App, events| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(740.0, 600.0))), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| app.player_view(ui));
            output.textures_delta.clear();
            output.shapes
        };
        let _ = pass(&mut app, vec![]);
        let mut shapes: Vec<_> = pass(&mut app, vec![]).into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
        let mut button = None;
        while let Some((shape, clip)) = shapes.pop() {
            match shape {
                egui::Shape::Vec(inner) => shapes.extend(inner.into_iter().map(|shape| (shape, clip))),
                egui::Shape::Text(text) if text.galley.text() == "Вернуть оригинал" => {
                    assert!(clip.contains_rect(text.visual_bounding_rect()));
                    assert!(text.visual_bounding_rect().bottom() <= 600.0);
                    button = Some(text.visual_bounding_rect().center());
                }
                _ => {}
            }
        }
        let pos = button.expect("restore button missing");
        let _ = pass(&mut app, vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
        let _ = pass(&mut app, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        assert_eq!(app.jobs.len(), 1);
        let result = app.jobs.pop().unwrap().join().unwrap();
        assert!(!result.error, "{}", result.text);
        assert_eq!(std::fs::read(&video).unwrap(), original);
        let source_for_trim = video.clone();
        thread::spawn(move || crate::platform::edit::trim_current(&source_for_trim, 0, 60_000_000)).join().unwrap().unwrap();
        let player = crate::platform::player::Player::open(parent.0 as isize, &video).unwrap();
        player.set_volume(0.0).unwrap();
        app.player = Some((video.clone(), player, 0.0));
        app.pending_trim = Some((video.clone(), "00:00.000".into(), "auto".into()));
        let _ = pass(&mut app, vec![]);
        let mut shapes: Vec<_> = pass(&mut app, vec![]).into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
        let mut editor_button = None;
        while let Some((shape, clip)) = shapes.pop() {
            match shape {
                egui::Shape::Vec(inner) => shapes.extend(inner.into_iter().map(|shape| (shape, clip))),
                egui::Shape::Text(text) if text.galley.text() == "Вернуть оригинал" => {
                    assert!(clip.contains_rect(text.visual_bounding_rect()));
                    assert!(text.visual_bounding_rect().bottom() <= 600.0);
                    editor_button = Some(text.visual_bounding_rect().center());
                }
                _ => {}
            }
        }
        let pos = editor_button.expect("restore button missing from trim editor");
        let _ = pass(&mut app, vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE }]);
        let _ = pass(&mut app, vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]);
        assert_eq!(app.jobs.len(), 1);
        let result = app.jobs.pop().unwrap().join().unwrap();
        assert!(!result.error, "{}", result.text);
        assert_eq!(std::fs::read(&video).unwrap(), original);
        unsafe { DestroyWindow(parent).unwrap(); CoUninitialize(); }
    }

    #[test]
    fn a_selection_on_the_preview_maps_to_source_pixels() {
        // A 200×100 preview of a 1920×960 source.
        let canvas = Rect::from_min_size(pos2(100.0, 50.0), vec2(200.0, 100.0));
        let selection = Rect::from_min_max(pos2(150.0, 75.0), pos2(250.0, 125.0));
        let region = region_from_rect(selection, canvas, (1920, 960)).unwrap();
        assert_eq!(region, Region { x: 480, y: 240, width: 960, height: 480 });
        assert_eq!(region_rect(region, canvas, (1920, 960)), selection);
        // A click without a drag selects nothing.
        let click = Rect::from_min_max(pos2(150.0, 75.0), pos2(150.5, 75.5));
        assert_eq!(region_from_rect(click, canvas, (1920, 960)), None);
    }

    #[test]
    fn exit_waits_for_the_recording_being_saved() {
        let (tx, rx) = mpsc::channel();
        let saved = Arc::new(AtomicBool::new(false));
        let saver = thread::spawn({
            let saved = saved.clone();
            move || {
                thread::sleep(Duration::from_millis(100));
                saved.store(true, Ordering::SeqCst);
                let _ = tx.send(Ok(PathBuf::from("rec.mp4")));
            }
        });
        let path = finish_on_exit(RecState::Saving(rx));
        assert!(saved.load(Ordering::SeqCst), "exited before the recording was finalized");
        assert_eq!(path, Some(PathBuf::from("rec.mp4")));
        saver.join().unwrap();
    }

    #[test]
    #[ignore]
    fn exit_preserves_a_recording_that_takes_over_ten_seconds_to_save() {
        let (tx, rx) = mpsc::channel();
        let saver = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10_200));
            tx.send(Ok(PathBuf::from("slow-recording.mp4"))).unwrap();
        });
        assert_eq!(finish_on_exit(RecState::Saving(rx)), Some(PathBuf::from("slow-recording.mp4")));
        saver.join().unwrap();
    }

    #[test]
    fn saved_message_survives_a_source_that_is_gone() {
        let _language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
        let ctx = egui::Context::default();
        let mut app = test_app();
        // The recorded window was closed: its handle no longer exists.
        app.selected = Some(SourceId::Window(0));
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(PathBuf::from("rec.mp4"))).unwrap();
        app.rec = RecState::Saving(rx);
        app.poll(&ctx);
        let status = app.status.as_ref().expect("status");
        assert!(status.text.starts_with("Grabación guardada"), "status: {}", status.text);
        assert_eq!(status.file.as_deref(), Some(std::path::Path::new("rec.mp4")));
        assert!(app.selected.is_none(), "the closed window is still shown as the source");
    }

    #[test]
    fn a_new_release_opens_the_dialog_unless_skipped() {
        let ctx = egui::Context::default();
        let release = Release { version: "9.0.0".into(), url: "https://example.com".into() };
        for (skipped, dialog) in [(None, true), (Some("8.0.0"), true), (Some("9.0.0"), false)] {
            let mut app = test_app();
            app.config.skipped_version = skipped.map(str::to_owned);
            let (tx, rx) = mpsc::channel();
            tx.send(Some(release.clone())).unwrap();
            app.update_check = Some(rx);
            app.poll(&ctx);
            assert_eq!(app.update_dialog, dialog, "skipped version: {skipped:?}");
            assert_eq!(app.update.as_ref(), Some(&release), "the status bar link is always shown");
        }
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        tx.send(None).unwrap();
        app.update_check = Some(rx);
        app.poll(&ctx);
        assert!(!app.update_dialog, "no newer release");
    }

    #[test]
    fn update_dialog_buttons_stay_inside_it_in_both_languages() {
        let _language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
        for language in [Language::English, Language::Spanish] {
            i18n::set(language);
            let mut app = test_app();
            app.update = Some(Release { version: "10.10.10".into(), url: "https://example.com".into() });
            app.update_dialog = true;
            let ctx = egui::Context::default();
            let input = || egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(1280.0, 720.0))),
                ..Default::default()
            };
            // The first passes size the dialog.
            let mut shapes = Vec::new();
            for _ in 0..3 {
                let mut output = ctx.run_ui(input(), |ui| app.update_dialog(ui.ctx()));
                output.textures_delta.clear(); // no renderer
                shapes = output.shapes;
            }
            let mut flat = Vec::new();
            let mut pending: Vec<egui::Shape> = shapes.into_iter().map(|c| c.shape).collect();
            while let Some(shape) = pending.pop() {
                match shape {
                    egui::Shape::Vec(inner) => pending.extend(inner),
                    shape => flat.push(shape),
                }
            }
            let title = tr!("Nueva versión disponible", "New version available");
            let title_pos = flat
                .iter()
                .find_map(|s| match s {
                    egui::Shape::Text(t) if t.galley.text() == title => Some(t.pos),
                    _ => None,
                })
                .expect("dialog title drawn");
            // All but the backdrop and the frame, the shapes around the title.
            let left = flat
                .iter()
                .map(egui::Shape::visual_bounding_rect)
                .filter(|r| r.is_positive() && !r.contains(title_pos))
                .map(|r| r.min.x)
                .fold(f32::INFINITY, f32::min);
            assert!(left >= title_pos.x - 0.5, "{language:?}: something starts at x = {left}, left of the title at {}", title_pos.x);
        }
        i18n::set(Language::Spanish);
    }

    #[test]
    fn a_recording_cut_short_is_repaired_at_start_up() {
        let _language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("obs-lite-recover-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cut.mp4");
        // Writing stopped inside the last fragment's data.
        let (mut file, _, _) = crate::mp4::tests::fragmented();
        file.truncate(file.len() - 42);
        std::fs::write(&path, file).unwrap();

        let mut app = test_app();
        app.config.last_recording = Some(path.clone());
        app.recover_last_recording();
        let status = app.status.as_ref().expect("a status");
        assert!(status.text.starts_with("Se recuperó la grabación interrumpida"), "status: {}", status.text);
        assert_eq!(status.file.as_deref(), Some(path.as_path()));
        assert!(app.config.last_recording.is_none(), "repaired once");
        assert!(!crate::mp4::repair(&path).unwrap(), "now a regular MP4");

        // A recording that finished (so is regular), or that is gone: nothing to say.
        for last in [path.clone(), dir.join("gone.mp4")] {
            let mut app = test_app();
            app.config.last_recording = Some(last);
            app.recover_last_recording();
            assert!(app.status.is_none());
            assert!(app.config.last_recording.is_none());
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn temporarily_locked_recording_remains_queued_for_recovery() {
        use std::os::windows::fs::OpenOptionsExt;

        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("interrupted.mp4");
        let (mut file, _, _) = crate::mp4::tests::fragmented();
        file.truncate(file.len() - 42);
        std::fs::write(&path, file).unwrap();
        let locked = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
        let mut app = test_app();
        app.config.last_recording = Some(path.clone());
        app.recover_last_recording();
        assert!(app.config.last_recording.is_none());
        assert_eq!(app.config.pending_recoveries, vec![path.clone()]);
        assert!(app.status.as_ref().is_some_and(|status| status.error));
        app.config = serde_json::from_str(&serde_json::to_string(&app.config).unwrap()).unwrap();
        app.config.last_recording = Some(folder.path().join("new-recording.mp4"));
        drop(locked);
        app.recover_last_recording();
        assert!(app.config.pending_recoveries.is_empty());
        assert!(!crate::mp4::repair(&path).unwrap());
        assert!(app.config.gallery_files.contains(&path));
    }

    /// One pass of the settings dialog with `events` as input.
    fn settings_pass(ctx: &egui::Context, app: &mut App, events: Vec<egui::Event>) {
        let mut output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| app.settings_dialog(ui.ctx()));
        output.textures_delta.clear();
    }

    #[test]
    fn escape_while_waiting_for_a_hotkey_keeps_the_dialog_open() {
        let _language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
        let mut app = test_app();
        app.settings = Some(SettingsDialog { draft: Config::default(), page: SettingsPage::Hotkeys, capturing: Some(0), replay_duration: "01:00".into(), selected_source: None });
        let escape = egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        let ctx = egui::Context::default();
        settings_pass(&ctx, &mut app, Vec::new());
        settings_pass(&ctx, &mut app, vec![escape.clone()]);
        let dialog = app.settings.as_ref().expect("still open");
        assert!(dialog.capturing.is_none(), "the wait for a hotkey ended");
        // Another Escape, with no wait, closes it.
        settings_pass(&ctx, &mut app, vec![escape]);
        assert!(app.settings.is_none());
    }

    #[test]
    fn a_hotkey_set_for_one_action_is_taken_from_another() {
        // F9 was just set for the first action, and the second had it.
        let (mut a, mut b, mut c) = (Some("F9".to_owned()), Some("F9".to_owned()), None);
        take_from_others(&mut [&mut a, &mut b, &mut c], 0);
        assert_eq!((a.as_deref(), b, c), (Some("F9"), None, None));
    }

    #[test]
    fn replay_duration_accepts_minutes_and_seconds_within_limits() {
        assert_eq!(parse_replay_duration("01:00"), Some(60));
        assert_eq!(parse_replay_duration("00:45"), Some(45));
        assert_eq!(parse_replay_duration("00:01"), Some(1));
        assert_eq!(parse_replay_duration("1440:00"), Some(86_400));
        for value in ["00:00", "1440:01", "01:60", "1:2", "-1:00", "1:00:00", "999999999999:00", ""] {
            assert_eq!(parse_replay_duration(value), None, "{value}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn trim_times_accept_zero_and_reject_invalid_values() {
        assert_eq!(parse_trim_time("00:00"), Some(0));
        assert_eq!(parse_trim_time("0:00"), Some(0));
        assert_eq!(parse_trim_time("01:05"), Some(65_000));
        assert_eq!(parse_trim_time("01:05.125"), Some(65_125));
        assert_eq!(parse_trim_time("00:01.2"), Some(1_200));
        assert_eq!(parse_trim_time("00:00.001"), Some(1));
        assert_eq!(trim_time(65_125), "01:05.125");
        assert_eq!(parse_trim_time("00:01.1234"), None);
        assert_eq!(parse_trim_time("00:01."), None);
        assert_eq!(parse_trim_time("1:60"), None);
        assert_eq!(parse_trim_time("abc:00"), None);
        assert_eq!(parse_trim_time("00:0"), None);
    }

    #[test]
    fn first_run_waits_for_setup_before_starting_recording() {
        let mut app = test_app();
        app.config.setup_complete = false;
        let ctx = egui::Context::default();
        app.handle(&ctx, Action::ToggleRecording);
        assert!(matches!(app.rec, RecState::Idle));
        assert!(app.status.as_ref().unwrap().text.contains("настройку"));
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.setup_dialog(ui.ctx()));
        output.textures_delta.clear();
        assert!(!app.config.setup_complete);
    }

    #[test]
    fn settings_navigation_and_actions_fit_narrow_viewports() {
        let _language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Language::Russian);
        for size in [vec2(420.0, 900.0), vec2(360.0, 600.0)] {
            for page in [SettingsPage::General, SettingsPage::Output, SettingsPage::Audio, SettingsPage::Video, SettingsPage::Hotkeys] {
                let ctx = egui::Context::default();
                let mut app = test_app();
                app.settings = Some(SettingsDialog { draft: app.config.clone(), page, capturing: None, replay_duration: "00:45".into(), selected_source: None });
                let pass = |app: &mut App| {
                    let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), size)), ..Default::default() };
                    let mut out = ctx.run_ui(input, |ui| app.settings_dialog(ui.ctx()));
                    out.textures_delta.clear();
                    out.shapes
                };
                let _ = pass(&mut app);
                let mut shapes: Vec<_> = pass(&mut app).into_iter().map(|shape| shape.shape).collect();
                let mut found = Vec::new();
                while let Some(shape) = shapes.pop() {
                    match shape {
                        egui::Shape::Vec(inner) => shapes.extend(inner),
                        egui::Shape::Text(text) if ["Общие", "Запись", "Звук", "Видео", "Клавиши", "Сохранить", "Отмена"].contains(&text.galley.text()) => {
                            let rect = text.visual_bounding_rect();
                            assert!(rect.left() >= 0.0 && rect.right() <= size.x && rect.top() >= 0.0 && rect.bottom() <= size.y, "control {} outside {:?}: {:?}", text.galley.text(), size, rect);
                            found.push(text.galley.text().to_owned());
                        }
                        _ => {}
                    }
                }
                for label in ["Общие", "Запись", "Звук", "Видео", "Клавиши", "Сохранить", "Отмена"] {
                    assert!(found.iter().any(|item| item == label), "missing {label} at {size:?}");
                }
            }
        }
        crate::i18n::set(crate::i18n::Language::Spanish);
    }

    #[test]
    fn settings_window_stays_in_place_when_switching_pages() {
        let _language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Language::Russian);
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.settings = Some(SettingsDialog { draft: app.config.clone(), page: SettingsPage::General, capturing: None, replay_duration: "00:45".into(), selected_source: None });
        let pass = |app: &mut App, events| {
            let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(420.0, 1080.0))), events, ..Default::default() }, |ui| app.settings_dialog(ui.ctx()));
            output.textures_delta.clear();
            output.shapes
        };
        pass(&mut app, vec![]);
        let shapes = pass(&mut app, vec![]);
        let general = egui::AreaState::load(&ctx, egui::Id::unique("settings")).unwrap().rect();
        let mut pending: Vec<_> = shapes.into_iter().map(|shape| shape.shape).collect();
        let mut output_tab = None;
        while let Some(shape) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == "Запись" => output_tab = Some(text.visual_bounding_rect().center()),
                _ => {}
            }
        }
        let output_tab = output_tab.expect("recording tab missing");
        let button = |pressed| egui::Event::PointerButton { pos: output_tab, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        pass(&mut app, vec![egui::Event::PointerMoved(output_tab), button(true)]);
        pass(&mut app, vec![button(false)]);
        for _ in 0..4 { pass(&mut app, vec![]); }
        assert!(app.settings.as_ref().is_some_and(|settings| settings.page == SettingsPage::Output));
        let audio = egui::AreaState::load(&ctx, egui::Id::unique("settings")).unwrap().rect();
        assert!((audio.height() - general.height()).abs() < 2.0, "window resized: general={general:?}, audio={audio:?}");
        assert!((audio.center().y - general.center().y).abs() < 2.0, "window moved: general={general:?}, audio={audio:?}");
        crate::i18n::set(crate::i18n::Language::Spanish);
    }

    #[test]
    fn narrow_settings_scroll_to_last_option_without_hiding_save() {
        let _language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Language::Russian);
        let ctx = egui::Context::default();
        let mut app = test_app();
        app.settings = Some(SettingsDialog { draft: app.config.clone(), page: SettingsPage::Output, capturing: None, replay_duration: "00:45".into(), selected_source: None });
        let pass = |app: &mut App, events| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 600.0))), events, ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| app.settings_dialog(ui.ctx()));
            output.textures_delta.clear();
            output.shapes
        };
        let _ = pass(&mut app, vec![]);
        let _ = pass(&mut app, vec![]);
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, -3000.0), phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Unknown, modifiers: egui::Modifiers::NONE };
        let _ = pass(&mut app, vec![egui::Event::PointerMoved(pos2(180.0, 300.0)), wheel]);
        for _ in 0..30 { let _ = pass(&mut app, vec![]); }
        let mut pending: Vec<_> = pass(&mut app, vec![]).into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
        let mut found = HashSet::new();
        while let Some((shape, clip)) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner.into_iter().map(|shape| (shape, clip))),
                egui::Shape::Text(text) if ["Формат скриншота", "Сохранить", "Отмена"].contains(&text.galley.text()) => {
                    let rect = text.visual_bounding_rect();
                    if clip.contains_rect(rect) && rect.left() >= 0.0 && rect.right() <= 360.0 && rect.bottom() <= 600.0 {
                        found.insert(text.galley.text().to_owned());
                    }
                }
                _ => {}
            }
        }
        assert_eq!(found.len(), 3, "settings controls missing or clipped: {found:?}");
        crate::i18n::set(crate::i18n::Language::Spanish);
    }

    #[test]
    fn each_narrow_settings_page_reaches_its_last_option_and_save() {
        let _language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Language::Russian);
        for (page, last) in [
            (SettingsPage::General, "Положение показателей"),
            (SettingsPage::Output, "Видео: MP4 · H.264"),
            (SettingsPage::Audio, "Подавление шума микрофона"),
            (SettingsPage::Video, "Частота кадров"),
            (SettingsPage::Hotkeys, "Микрофон"),
        ] {
            let ctx = egui::Context::default();
            let mut app = test_app();
            app.settings = Some(SettingsDialog { draft: app.config.clone(), page, capturing: None, replay_duration: "00:45".into(), selected_source: None });
            let pass = |app: &mut App, events| {
                let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 600.0))), events, ..Default::default() }, |ui| app.settings_dialog(ui.ctx()));
                output.textures_delta.clear();
                output.shapes
            };
            pass(&mut app, vec![]);
            let initial = pass(&mut app, vec![]);
            let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, -3000.0), phase: egui::TouchPhase::Move, source: egui::MouseWheelSource::Unknown, modifiers: egui::Modifiers::NONE };
            pass(&mut app, vec![egui::Event::PointerMoved(pos2(180.0, 300.0)), wheel]);
            for _ in 0..30 { pass(&mut app, vec![]); }
            let final_shapes = pass(&mut app, vec![]);
            for shapes in [&initial, &final_shapes] {
                let mut pending: Vec<_> = shapes.iter().map(|shape| (shape.shape.clone(), shape.clip_rect)).collect();
                while let Some((shape, clip)) = pending.pop() {
                    match shape {
                        egui::Shape::Vec(inner) => pending.extend(inner.into_iter().map(|shape| (shape, clip))),
                        egui::Shape::Text(text) if !text.galley.elided && !text.galley.text().contains(":\\") => {
                            let rect = text.visual_bounding_rect();
                            if rect.center().y >= clip.top() && rect.center().y <= clip.bottom() && rect.intersects(clip) {
                                assert!(rect.right() <= clip.right() + 1.0 && rect.left() >= clip.left() - 1.0, "settings text clipped horizontally: {} at {rect:?}, clip {clip:?}", text.galley.text());
                            }
                        }
                        _ => {}
                    }
                }
            }
            let mut pending: Vec<_> = final_shapes.into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
            let mut found = HashSet::new();
            while let Some((shape, clip)) = pending.pop() {
                match shape {
                    egui::Shape::Vec(inner) => pending.extend(inner.into_iter().map(|shape| (shape, clip))),
                    egui::Shape::Text(text) if text.galley.text() == last || text.galley.text() == "Сохранить" => {
                        let rect = text.visual_bounding_rect();
                        if clip.contains_rect(rect) && rect.left() >= 0.0 && rect.right() <= 360.0 && rect.bottom() <= 600.0 {
                            found.insert(text.galley.text().to_owned());
                        }
                    }
                    _ => {}
                }
            }
            assert!(found.contains(last) && found.contains("Сохранить"), "missing or clipped {last}: {found:?}");
        }
        crate::i18n::set(crate::i18n::Language::Spanish);
    }

    #[test]
    fn long_device_options_fit_the_picker_and_remain_selectable() {
        let _language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
        crate::i18n::set(crate::i18n::Language::English);
        let ctx = egui::Context::default();
        let devices = vec![Device { id: "test-device".into(), name: "Very long audio device name ".repeat(12) }];
        let mut enabled = false;
        let mut device = None;
        let pass = |enabled: &mut bool, device: &mut Option<String>, events| {
            let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 600.0))), events, ..Default::default() }, |ui| {
                egui::CentralPanel::default().show(ui, |ui| { device_combo(ui, "test-device-picker", enabled, device, &devices); });
            });
            output.textures_delta.clear();
            output.shapes
        };
        pass(&mut enabled, &mut device, vec![]);
        pass(&mut enabled, &mut device, vec![]);
        let click = |position, pressed| egui::Event::PointerButton { pos: position, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let button = pos2(100.0, 16.0);
        pass(&mut enabled, &mut device, vec![egui::Event::PointerMoved(button), click(button, true)]);
        let mut shapes = pass(&mut enabled, &mut device, vec![click(button, false)]);
        for _ in 0..3 { shapes = pass(&mut enabled, &mut device, vec![]); }
        let mut pending: Vec<_> = shapes.into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
        let mut option = None;
        while let Some((shape, clip)) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner.into_iter().map(|shape| (shape, clip))),
                egui::Shape::Text(text) if text.galley.text() == devices[0].name => {
                    let rect = text.visual_bounding_rect();
                    assert!(text.galley.elided && rect.right() <= 360.0 && clip.contains_rect(rect), "long option extends beyond the popup: {rect:?}, clip {clip:?}");
                    option = Some(rect.center());
                }
                _ => {}
            }
        }
        let option = option.expect("device option was not rendered");
        pass(&mut enabled, &mut device, vec![egui::Event::PointerMoved(option), click(option, true)]);
        pass(&mut enabled, &mut device, vec![click(option, false)]);
        assert!(enabled && device.as_deref() == Some("test-device"));
        crate::i18n::set(crate::i18n::Language::Spanish);
    }

    #[test]
    fn long_capture_source_options_fit_and_remain_selectable() {
        let ctx = egui::Context::default();
        let sources = vec![Source { id: SourceId::Window(123), label: "Long window title ".repeat(20) }];
        let mut dialog = SettingsDialog { draft: Config::default(), page: SettingsPage::Video, capturing: None, replay_duration: "00:45".into(), selected_source: None };
        let pass = |dialog: &mut SettingsDialog, events| {
            let mut output = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(360.0, 600.0))), events, ..Default::default() }, |ui| {
                egui::CentralPanel::default().show(ui, |ui| compact_settings_page(ui, dialog, &[], &[], &sources, None));
            });
            output.textures_delta.clear();
            output.shapes
        };
        pass(&mut dialog, vec![]);
        let shapes = pass(&mut dialog, vec![]);
        let mut pending: Vec<_> = shapes.into_iter().map(|shape| shape.shape).collect();
        let mut button = None;
        while let Some(shape) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner),
                egui::Shape::Text(text) if text.galley.text() == "Не выбран" => button = Some(text.visual_bounding_rect().center()),
                _ => {}
            }
        }
        let button = button.expect("source picker not rendered");
        let click = |position, pressed| egui::Event::PointerButton { pos: position, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        pass(&mut dialog, vec![egui::Event::PointerMoved(button), click(button, true)]);
        pass(&mut dialog, vec![click(button, false)]);
        let mut shapes = Vec::new();
        for _ in 0..3 { shapes = pass(&mut dialog, vec![]); }
        let mut pending: Vec<_> = shapes.into_iter().map(|shape| (shape.shape, shape.clip_rect)).collect();
        let mut option = None;
        while let Some((shape, clip)) = pending.pop() {
            match shape {
                egui::Shape::Vec(inner) => pending.extend(inner.into_iter().map(|shape| (shape, clip))),
                egui::Shape::Text(text) if text.galley.text() == sources[0].label => {
                    let rect = text.visual_bounding_rect();
                    assert!(text.galley.elided && rect.right() <= 360.0 && clip.contains_rect(rect), "long source extends beyond popup: {rect:?}, clip {clip:?}");
                    option = Some(rect.center());
                }
                _ => {}
            }
        }
        let option = option.expect("source option not rendered");
        pass(&mut dialog, vec![egui::Event::PointerMoved(option), click(option, true)]);
        pass(&mut dialog, vec![click(option, false)]);
        assert_eq!(dialog.selected_source, Some(SourceId::Window(123)));
    }

    #[test]
    fn settings_reject_a_non_absolute_common_output_path() {
        let mut config = Config::default();
        let absolute = std::env::temp_dir();
        config.output_dir = absolute.clone();
        config.replay_dir = absolute.clone();
        config.screenshot_dir = absolute.clone();
        assert_eq!(invalid_output_path(&config), None);
        config.output_dir = PathBuf::from("relative");
        assert_eq!(invalid_output_path(&config), Some("Папка файлов"));
    }

    #[test]
    fn dated_media_names_sort_oldest_to_newest_and_identify_kind() {
        let earlier = chrono::NaiveDate::from_ymd_opt(2026, 11, 12).unwrap().and_hms_milli_opt(10, 20, 0, 5).unwrap();
        let later = earlier + chrono::Duration::milliseconds(1);
        let first = dated_media_stem(Some("Replay"), earlier);
        let second = dated_media_stem(Some("Replay"), later);
        let screenshot = dated_media_stem(Some("Screenshot"), later);
        assert_eq!(first, "2026-11-12 10-20-00.005 — Replay");
        assert!(first < second);
        assert!(screenshot.ends_with(" — Screenshot"));
    }

    #[test]
    fn low_disk_space_is_reported_without_stopping_the_app() {
        let mut app = test_app();
        let (tx, rx) = mpsc::channel();
        tx.send(vec![("записи", Some(500_000_000))]).unwrap();
        app.disk_check = Some(rx);
        app.poll(&egui::Context::default());
        let status = app.status.as_ref().unwrap();
        assert!(status.error);
        assert!(status.text.contains("500"));
        assert!(app.disk_check.is_none());
        assert!(crate::platform::sys::free_space(&std::env::temp_dir()).is_some());
    }

    #[test]
    fn the_tray_does_not_start_a_session_while_settings_is_open() {
        let _language = LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
        let mut app = test_app();
        app.settings = Some(SettingsDialog { draft: Config::default(), page: SettingsPage::General, capturing: None, replay_duration: "01:00".into(), selected_source: None });
        app.tray_commands.lock().unwrap().push(TrayCommand::ToggleRecording);
        app.poll(&egui::Context::default());
        let status = app.status.as_ref().expect("a status");
        assert!(status.text.contains("ajustes"), "status: {}", status.text);
        assert!(matches!(app.rec, RecState::Idle));
    }

    #[test]
    fn outputs_named_in_the_same_second_get_different_names() {
        let dir = std::env::temp_dir().join(format!("obs-lite-names-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let name = |n: &str| dir.join(n);
        let mut named = HashSet::new();
        assert_eq!(unused_name(&dir, "Replay 1", "mp4", &mut named), name("Replay 1.mp4"));
        // The first is still being written in the background: its name is taken anyway.
        assert_eq!(unused_name(&dir, "Replay 1", "mp4", &mut named), name("Replay 1 (2).mp4"));
        // Files from another run.
        std::fs::write(name("Replay 1.mp4"), b"").unwrap();
        std::fs::write(name("Replay 1 (2).mp4"), b"").unwrap();
        assert_eq!(unused_name(&dir, "Replay 1", "mp4", &mut HashSet::new()), name("Replay 1 (3).mp4"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn video_template_formats_time_and_rejects_invalid_names() {
        let time = chrono::NaiveDate::from_ymd_opt(2026, 11, 1).unwrap().and_hms_opt(9, 5, 7).unwrap();
        assert_eq!(video_name("Видео [ГГГГ-ММ-ДД] [ЧЧ-ММ]", time).unwrap(), "Видео 2026-11-01 09-05");
        assert_eq!(video_name("[ЧЧ-ММ-СС]", time).unwrap(), "09-05-07");
        for bad in ["", "../файл", "NUL", "видео [НЕИЗВЕСТНО]", "видео:"] {
            assert!(video_name(bad, time).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_typed_recording_path_must_be_absolute() {
        let previous = Config::default().output_dir;
        assert_eq!(typed_output_dir(PathBuf::new(), &previous), previous);
        assert_eq!(typed_output_dir(PathBuf::from("Videos"), &previous), previous);
        let typed = std::env::temp_dir().join("recordings");
        assert_eq!(typed_output_dir(typed.clone(), &previous), typed);
    }

    #[test]
    fn mute_silences_regardless_of_volume() {
        assert_eq!(channel_gain(1.0, true), 0.0);
        assert_eq!(channel_gain(0.5, true), 0.0);
    }

    #[test]
    fn no_preview_pictures_for_a_window_that_is_not_shown() {
        let just = Duration::from_millis(200);
        assert!(!preview_hidden(Some(true), just));
        assert!(!preview_hidden(None, just), "Wayland: drawn recently, so shown");
        assert!(preview_hidden(Some(false), just), "minimized");
        assert!(preview_hidden(None, Duration::from_secs(4)), "Wayland: no frames asked for");
        assert!(!preview_hidden(None, Duration::from_millis(1500)), "an idle window draws once a second");
    }

    #[test]
    fn a_silent_idle_window_repaints_rarely() {
        let fast = Duration::from_millis(66);
        assert_eq!(repaint_interval(false, false, &[0.0, 0.0, 0.0]), Duration::from_secs(1));
        assert_eq!(repaint_interval(false, false, &[0.0, 0.2, 0.0]), fast, "a meter is moving");
        assert_eq!(repaint_interval(false, false, &[0.002, 0.0, 0.0]), fast, "a meter is still falling");
        assert_eq!(repaint_interval(false, true, &[0.0; 3]), fast, "the recording timer runs");
        assert_eq!(repaint_interval(true, true, &[0.5; 3]), Duration::from_secs(1));
    }

    #[test]
    fn desktop_audio_leaves_out_the_application_captured_on_its_own() {
        assert_eq!(system_kind(Some(InputKind::Application(42)), None), InputKind::SystemExcept(42));
        assert_eq!(system_kind(None, None), InputKind::System, "no application captured");
        assert_eq!(system_kind(Some(InputKind::Application(42)), Some("speakers")), InputKind::System, "a chosen output device");
    }

    #[test]
    fn volume_uses_a_squared_curve() {
        assert_eq!(channel_gain(1.0, false), 1.0);
        assert_eq!(channel_gain(0.5, false), 0.25);
        assert_eq!(channel_gain(0.0, false), 0.0);
    }
}
