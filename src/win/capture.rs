//! Screen/window capture through Windows Graphics Capture.

use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::dxgi_duplication_api::{DxgiDuplicationApi, DxgiDuplicationFormat, Error as DuplicationError};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::core::Interface;
use windows_capture::window::Window;

use super::encoder::{Com, Encoder, EncoderSettings, Outputs};
use crate::media::Clock;
use crate::capture::{
    AudioApp, PREVIEW_DEFAULT_WIDTH, PreviewFrame, RecordConfig, SessionOptions, Source, SourceId, bgra_to_rgba,
    downscale,
};
use crate::i18n::{tr, tr3};

type BoxError = Box<dyn Error + Send + Sync>;
type Size = (u32, u32);
type Snapshot = (u32, u32, Vec<u8>);

/// Lists capturable monitors and top-level windows (excluding our own).
pub fn list_sources() -> Vec<Source> {
    let mut out = Vec::new();
    for (i, m) in Monitor::enumerate().unwrap_or_default().into_iter().enumerate() {
        let (w, h) = (m.width().unwrap_or(0), m.height().unwrap_or(0));
        let name = m.device_string().unwrap_or_default();
        out.push(Source {
            id: SourceId::Monitor(m.as_raw_hmonitor() as isize),
            label: if crate::i18n::russian() { format!("Экран {} ({}×{}) {}", i + 1, w, h, name) } else { tr!("Pantalla {} ({}×{}) {}", "Display {} ({}×{}) {}", i + 1, w, h, name) }.trim_end().to_string(),
        });
    }
    if let Ok(primary) = Monitor::primary() {
        prioritize_primary(&mut out, primary.as_raw_hmonitor() as isize);
    }
    let own_pid = std::process::id();
    for w in Window::enumerate().unwrap_or_default() {
        let Ok(title) = w.title() else { continue };
        if title.trim().is_empty() || w.process_id().ok() == Some(own_pid) || cloaked(&w) {
            continue;
        }
        let process = w.process_name().unwrap_or_default();
        out.push(Source { id: SourceId::Window(w.as_raw_hwnd() as isize), label: format!("{title} — {process}") });
    }
    out
}

fn prioritize_primary(sources: &mut [Source], primary: isize) {
    sources.sort_by_key(|source| source.id != SourceId::Monitor(primary));
}

pub fn display_key(id: SourceId) -> Option<String> {
    use windows::Win32::Graphics::Gdi::{DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICEW, EnumDisplayDevicesW};
    use windows::Win32::UI::WindowsAndMessaging::EDD_GET_DEVICE_INTERFACE_NAME;
    use windows::core::PCWSTR;
    let SourceId::Monitor(handle) = id else { return None };
    let name = Monitor::from_raw_hmonitor(handle as *mut _).device_name().ok()?;
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut index = 0;
    loop {
        let mut device = DISPLAY_DEVICEW { cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
        if !unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), index, &mut device, EDD_GET_DEVICE_INTERFACE_NAME).as_bool() } { return Some(name); }
        if device.StateFlags.contains(DISPLAY_DEVICE_ACTIVE) && device.DeviceID[0] != 0 {
            let end = device.DeviceID.iter().position(|c| *c == 0).unwrap_or(device.DeviceID.len());
            return Some(String::from_utf16_lossy(&device.DeviceID[..end]));
        }
        index += 1;
    }
}

pub fn desktop_resolution(id: SourceId) -> Option<Size> {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_REGISTRY_SETTINGS, EnumDisplaySettingsW};
    use windows::core::PCWSTR;
    let SourceId::Monitor(handle) = id else { return None };
    let name = Monitor::from_raw_hmonitor(handle as *mut _).device_name().ok()?;
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let mut mode = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    let found = unsafe { EnumDisplaySettingsW(PCWSTR(wide.as_ptr()), ENUM_REGISTRY_SETTINGS, &mut mode).as_bool() };
    (found && mode.dmPelsWidth >= 2 && mode.dmPelsHeight >= 2).then_some((mode.dmPelsWidth & !1, mode.dmPelsHeight & !1))
}

/// Applications with a visible window (one entry per executable), excluding obs-lite.
pub fn list_audio_apps() -> Vec<AudioApp> {
    audio_apps(false)
}

pub fn list_audio_instances() -> Vec<AudioApp> {
    audio_apps(true)
}

fn audio_apps(instances: bool) -> Vec<AudioApp> {
    let own_pid = std::process::id();
    let mut apps: Vec<AudioApp> = Vec::new();
    for w in Window::enumerate().unwrap_or_default() {
        let (Ok(title), Ok(pid), Ok(exe)) = (w.title(), w.process_id(), w.process_name()) else { continue };
        if title.trim().is_empty() || pid == own_pid || cloaked(&w)
            || apps.iter().any(|a| if instances { a.pid == pid } else { a.exe.eq_ignore_ascii_case(&exe) })
        {
            continue;
        }
        apps.push(AudioApp { pid, exe, title });
    }
    apps.sort_by_key(|a| a.exe.to_lowercase());
    apps
}

/// A window the desktop hides while it is still "visible": a suspended Store app, one on another
/// virtual desktop. Capturing it gives no picture.
fn cloaked(window: &Window) -> bool {
    let mut cloaked = 0u32;
    let hwnd = HWND(window.as_raw_hwnd());
    let read = unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), size_of::<u32>() as u32) };
    read.is_ok() && cloaked != 0
}

/// GPU copy of the newest frame. Windows Graphics Capture only delivers frames when
/// something changes, so a screenshot reads this instead of waiting for the next one.
pub struct LastFrame {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    size: Size,
}

/// State shared between the capture thread, the audio mixer and the UI.
pub struct Shared {
    restart_required: AtomicBool,
    pub preview: Mutex<Option<PreviewFrame>>,
    /// Width in screen pixels the preview is drawn at; frames are scaled down to it, not further.
    pub preview_width: AtomicUsize,
    /// Set while the window is not shown (minimized, or covered where the system says so):
    /// nothing would draw the preview, so no preview pictures are made.
    pub preview_hidden: AtomicBool,
    pub frames: AtomicU64,
    /// Drives video and audio timestamps; pausing it pauses the recording.
    pub clock: Arc<Clock>,
    /// Set once the encoder is pulling samples and the clock has started.
    pub encoder_ready: Arc<AtomicBool>,
    pub encoder: Mutex<Option<Encoder>>,
    /// The recording file and replay buffer; they can be turned on before the encoder exists.
    pub outputs: Arc<Mutex<Outputs>>,
    /// Capture size and encoded size, known once the encoder exists.
    pub sizes: Mutex<Option<(Size, Size)>>,
    last_frame: Mutex<Option<Com<LastFrame>>>,
    repaint: Option<eframe::egui::Context>,
}

struct Flags {
    shared: Arc<Shared>,
    record: Option<RecordConfig>,
    fps: u32,
    preview_interval: Option<Duration>,
}

struct Handler {
    flags: Flags,
    last_preview: Option<Instant>,
    /// Top-left corner of the recorded region, if there is one; fixed when the encoder is created.
    region: Option<(u32, u32)>,
}

#[derive(Clone, Copy)]
struct Surface<'a> {
    texture: &'a ID3D11Texture2D,
    device: &'a ID3D11Device,
    context: &'a ID3D11DeviceContext,
    size: Size,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = BoxError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self::from_flags(ctx.flags))
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, _control: InternalCaptureControl) -> Result<(), Self::Error> {
        self.handle_surface(Surface {
            texture: frame.as_raw_texture(), device: frame.device(), context: frame.device_context(),
            size: (frame.width(), frame.height()),
        })?;
        if self.preview_due() && let Ok(mut buffer) = frame.buffer() {
            let (w, h, pitch) = (buffer.width() as usize, buffer.height() as usize, buffer.row_pitch() as usize);
            self.update_preview(buffer.as_raw_buffer(), w, h, pitch);
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        if let Some(ctx) = &self.flags.shared.repaint {
            ctx.request_repaint();
        }
        Ok(())
    }
}

impl Handler {
    fn from_flags(flags: Flags) -> Self {
        Self { flags, last_preview: None, region: None }
    }

    fn handle_surface(&mut self, surface: Surface<'_>) -> Result<(), BoxError> {
        let shared = self.flags.shared.clone();
        shared.frames.fetch_add(1, Ordering::Relaxed);
        // The screenshot copy and the preview are extras: failing them must not end a recording.
        let _ = keep_last_frame(&shared, surface);

        if let Some(rec) = &self.flags.record {
            let mut encoder = shared.encoder.lock().unwrap();
            if encoder.is_none() {
                let full = surface.size;
                let input = match rec.region.and_then(|r| r.fit(full)) {
                    Some(r) => {
                        self.region = Some((r.x, r.y));
                        (r.width, r.height)
                    }
                    None => rec.display_canvas.unwrap_or((full.0 & !1, full.1 & !1)),
                };
                let output = rec.resolution.apply(input);
                let fps = self.flags.fps;
                let created = Encoder::new(
                    surface.device,
                    EncoderSettings {
                        input,
                        output,
                        stretch_display: rec.display_canvas.is_some() && self.region.is_none(),
                        fps,
                        bitrate: rec.bitrate(output.0, output.1, fps),
                        audio_tracks: if rec.audio { if rec.separate_audio_tracks { 3 } else { 1 } } else { 0 },
                        overlay: rec.overlay,
                    },
                    shared.clock.clone(),
                    shared.encoder_ready.clone(),
                    shared.outputs.clone(),
                )?;
                *encoder = Some(created);
                *shared.sizes.lock().unwrap() = Some((input, output));
            }
            if let Some(enc) = encoder.as_ref() {
                enc.push_frame(surface.texture, surface.size, self.region);
            }
        }

        Ok(())
    }

    fn preview_due(&mut self) -> bool {
        let due = self.flags.preview_interval.is_some_and(|interval| self.last_preview.is_none_or(|t| t.elapsed() >= interval))
            && !self.flags.shared.preview_hidden.load(Ordering::Relaxed);
        if due { self.last_preview = Some(Instant::now()); }
        due
    }

    fn update_preview(&mut self, bgra: &[u8], w: usize, h: usize, pitch: usize) {
        if w == 0 || h == 0 {
            return;
        }
        let shared = &self.flags.shared;
        let pw = shared.preview_width.load(Ordering::Relaxed).clamp(64.min(w), w);
        let ph = (h * pw / w).max(1);
        let rgba = downscale(bgra, (w, h), pitch, (pw, ph));
        let mut preview = shared.preview.lock().unwrap();
        let seq = preview.as_ref().map_or(0, |p| p.seq + 1);
        *preview = Some(PreviewFrame { size: [pw, ph], rgba, seq });
        drop(preview);
        if let Some(ctx) = &shared.repaint {
            crate::capture::repaint_for_picture(ctx);
        }
    }
}

pub struct Session {
    control: Option<CaptureWorker>,
    pub shared: Arc<Shared>,
}

enum CaptureWorker {
    GraphicsCapture(CaptureControl<Handler, BoxError>),
    Duplication { display: isize, stop: Arc<AtomicBool>, thread: JoinHandle<Result<(), String>> },
}

impl CaptureWorker {
    fn finished(&self) -> bool {
        match self {
            Self::GraphicsCapture(control) => control.is_finished(),
            Self::Duplication { thread, .. } => thread.is_finished(),
        }
    }

    fn stop(self) -> Result<(), String> {
        match self {
            Self::GraphicsCapture(control) => {
                let result = if control.is_finished() { control.wait() } else { control.stop() };
                result.map_err(|error| error.to_string())
            }
            Self::Duplication { stop, thread, .. } => {
                stop.store(true, Ordering::Relaxed);
                thread.join().unwrap_or_else(|_| Err("DXGI capture worker stopped unexpectedly".into()))
            }
        }
    }
}

impl Session {
    pub fn start(opts: SessionOptions, repaint: Option<eframe::egui::Context>) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            restart_required: AtomicBool::new(false),
            preview: Mutex::new(None),
            preview_width: AtomicUsize::new(PREVIEW_DEFAULT_WIDTH),
            preview_hidden: AtomicBool::new(false),
            frames: AtomicU64::new(0),
            clock: Arc::new(Clock::new()),
            encoder_ready: Arc::new(AtomicBool::new(false)),
            encoder: Mutex::new(None),
            outputs: Arc::default(),
            sizes: Mutex::new(None),
            last_frame: Mutex::new(None),
            repaint,
        });
        let standard = || {
            let border = if GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
                DrawBorderSettings::WithoutBorder
            } else {
                DrawBorderSettings::Default
            };
            start_with_border(&opts, &shared, border).map(CaptureWorker::GraphicsCapture)
        };
        let control = match opts.source {
            SourceId::Monitor(display) if opts.desktop_duplication && !opts.cursor && native_display_mode(display) => {
                start_duplication(display, &opts, &shared).or_else(|dxgi| {
                    standard().map_err(|wgc| format!("DXGI: {dxgi} · Windows Graphics Capture: {wgc}"))
                })?
            }
            _ => standard()?,
        };
        Ok(Self { control: Some(control), shared })
    }

    /// The capture thread exited (source closed or handler error).
    pub fn is_finished(&self) -> bool {
        self.control.as_ref().is_none_or(CaptureWorker::finished)
    }

    pub fn restart_required(&self) -> bool {
        self.shared.restart_required.load(Ordering::Relaxed) && self.shared.frames.load(Ordering::Relaxed) > 0
    }

    pub fn backend_name(&self) -> &'static str {
        if self.uses_duplication() { "DXGI Desktop Duplication" } else { "Windows Graphics Capture" }
    }

    pub fn uses_duplication(&self) -> bool {
        matches!(&self.control, Some(CaptureWorker::Duplication { .. }))
    }

    pub fn duplication_mode_changed(&self) -> bool {
        matches!(&self.control, Some(CaptureWorker::Duplication { display, .. }) if !native_display_mode(*display))
    }

    /// Size of the source, once a frame has arrived.
    pub fn source_size(&self) -> Option<(u32, u32)> {
        self.shared.last_frame.lock().unwrap().as_ref().map(|l| l.0.size)
    }

    /// The newest captured frame as RGBA pixels: (width, height, pixels).
    pub fn snapshot(&self) -> Result<Snapshot, String> {
        self.shared.try_snapshot().unwrap_or_else(|| Err(snapshot_unavailable()))
    }

    /// Stops capturing and hands back the encoder (if recording) so it can be finalized.
    /// The error is whatever made the capture thread exit, if anything.
    pub fn stop(mut self) -> (Option<Encoder>, Option<String>) {
        let error = self.control.take().and_then(|control| control.stop().err());
        (self.shared.encoder.lock().unwrap().take(), error)
    }
}

impl Shared {
    fn try_snapshot(&self) -> Option<Result<Snapshot, String>> {
        let frame = self.last_frame.lock().unwrap();
        frame.as_ref().map(|frame| read_back(&frame.0).map_err(|error| error.to_string()))
    }

    pub fn await_snapshot(&self) -> Result<Snapshot, String> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = self.try_snapshot() { return result; }
            if Instant::now() >= deadline { return Err(snapshot_unavailable()); }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn snapshot_unavailable() -> String {
    tr3!("La fuente no proporcionó ninguna imagen.", "The source did not provide an image.", "Источник не предоставил изображение.").into()
}

pub fn snapshot_once(source: SourceId, cursor: bool, desktop_duplication: bool) -> Result<Snapshot, String> {
    let session = Session::start(SessionOptions { source, fps: 30, cursor, desktop_duplication, preview_fps: 0, record: None }, None)?;
    session.shared.await_snapshot()
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
    }
}

fn keep_last_frame(shared: &Shared, surface: Surface<'_>) -> windows::core::Result<()> {
    let source = surface.texture;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { source.GetDesc(&mut desc) };
    let mut last = shared.last_frame.lock().unwrap();
    if last.as_ref().is_none_or(|l| l.0.size != (desc.Width, desc.Height)) {
        let size = (desc.Width, desc.Height);
        desc.Usage = D3D11_USAGE_DEFAULT;
        (desc.BindFlags, desc.CPUAccessFlags, desc.MiscFlags) = (0, 0, 0);
        let mut texture = None;
        unsafe {
            surface.device.CreateTexture2D(&desc, None, Some(&mut texture))?;
            // The UI thread reads the copy back through the same immediate context.
            let _ = surface.context.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
        }
        let texture = texture.expect("CreateTexture2D returned no texture");
        let (device, context) = (surface.device.clone(), surface.context.clone());
        *last = Some(Com(LastFrame { device, context, texture, size }));
    }
    let last = &last.as_ref().expect("set above").0;
    unsafe { last.context.CopyResource(&last.texture, source) };
    Ok(())
}

fn read_back(last: &LastFrame) -> windows::core::Result<Snapshot> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { last.texture.GetDesc(&mut desc) };
    desc.Usage = D3D11_USAGE_STAGING;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    (desc.BindFlags, desc.MiscFlags) = (0, 0);
    let mut staging = None;
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        last.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.expect("CreateTexture2D returned no texture");
        last.context.CopyResource(&staging, &last.texture);
        last.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let (w, h, pitch) = (desc.Width as usize, desc.Height as usize, mapped.RowPitch as usize);
        let bgra = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * h);
        let rgba = bgra_to_rgba(bgra, w, h, pitch);
        last.context.Unmap(&staging, 0);
        Ok((desc.Width, desc.Height, rgba))
    }
}

fn start_with_border(
    opts: &SessionOptions,
    shared: &Arc<Shared>,
    border: DrawBorderSettings,
) -> Result<CaptureControl<Handler, BoxError>, String> {
    let flags = capture_flags(opts, shared);
    let cursor = if opts.cursor { CursorCaptureSettings::WithCursor } else { CursorCaptureSettings::WithoutCursor };
    // Asking for fewer frames saves work, but only Windows 11 24H2 and later can: elsewhere the
    // capture would not start at all.
    let interval = if GraphicsCaptureApi::is_minimum_update_interval_supported().unwrap_or(false) {
        MinimumUpdateIntervalSettings::Custom(Duration::from_secs_f64(1.0 / opts.fps.max(1) as f64))
    } else {
        MinimumUpdateIntervalSettings::Default
    };
    let result = match opts.source {
        SourceId::Monitor(h) => Handler::start_free_threaded(Settings::new(
            Monitor::from_raw_hmonitor(h as *mut _),
            cursor,
            border,
            SecondaryWindowSettings::Default,
            interval,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            flags,
        )),
        SourceId::Window(h) => Handler::start_free_threaded(Settings::new(
            Window::from_raw_hwnd(h as *mut _),
            cursor,
            border,
            SecondaryWindowSettings::Default,
            interval,
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            flags,
        )),
    };
    result.map_err(|e| e.to_string())
}

fn capture_flags(opts: &SessionOptions, shared: &Arc<Shared>) -> Flags {
    Flags {
        shared: shared.clone(),
        record: opts.record.clone(),
        fps: opts.fps,
        preview_interval: (opts.preview_fps > 0).then(|| Duration::from_secs_f64(1.0 / opts.preview_fps as f64)),
    }
}

fn native_display_mode(display: isize) -> bool {
    let Some(bounds) = super::sys::display_bounds(display) else { return false };
    let current = ((bounds.right - bounds.left) as u32, (bounds.bottom - bounds.top) as u32);
    desktop_resolution(SourceId::Monitor(display)) == Some(current)
}

fn start_duplication(display: isize, opts: &SessionOptions, shared: &Arc<Shared>) -> Result<CaptureWorker, String> {
    let flags = capture_flags(opts, shared);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let (started, ready) = mpsc::sync_channel(1);
    let thread = thread::Builder::new().name("opnoverlay-dxgi".into()).spawn(move || {
        let monitor = Monitor::from_raw_hmonitor(display as *mut _);
        let mut capture = DxgiDuplicationApi::new_options(monitor, &[DxgiDuplicationFormat::Bgra8])
            .map_err(|error| error.to_string())
            .and_then(|capture| {
                if capture.duplication_desc().Rotation == DXGI_MODE_ROTATION_IDENTITY { Ok(capture) }
                else { Err(tr3!("DXGI no admite esta rotación de pantalla.", "DXGI capture does not support this display rotation.", "Захват DXGI не поддерживает этот поворот экрана.").into()) }
            });
        match &mut capture {
            Ok(capture) => {
                if started.send(Ok(())).is_err() { return Ok(()) }
                let repaint = flags.shared.repaint.clone();
                let result = run_duplication(capture, flags, &stop_worker);
                if let Some(ctx) = repaint { ctx.request_repaint(); }
                result
            }
            Err(error) => { let _ = started.send(Err(error.clone())); Err(error.clone()) }
        }
    }).map_err(|error| error.to_string())?;
    match ready.recv() {
        Ok(Ok(())) => Ok(CaptureWorker::Duplication { display, stop, thread }),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        Err(error) => {
            let _ = thread.join();
            Err(error.to_string())
        }
    }
}

fn run_duplication(capture: &mut DxgiDuplicationApi, flags: Flags, stop: &AtomicBool) -> Result<(), String> {
    let interval = Duration::from_secs_f64(1.0 / f64::from(flags.fps.max(1)));
    let mut handler = Handler::from_flags(flags);
    let mut next_frame = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if now < next_frame {
            thread::sleep((next_frame - now).min(Duration::from_millis(8)));
            continue;
        }
        let mut frame = match capture.acquire_next_frame(0) {
            Ok(frame) => frame,
            Err(DuplicationError::Timeout) => {
                thread::sleep(Duration::from_millis(4));
                continue;
            }
            Err(DuplicationError::AccessLost) => {
                handler.flags.shared.restart_required.store(true, Ordering::Relaxed);
                return Err(tr3!("Se interrumpió la captura de pantalla DXGI.", "DXGI lost access to the display.", "DXGI потерял доступ к экрану.").into());
            }
            Err(error) => return Err(format!("DXGI: {error}")),
        };
        next_frame = (next_frame + interval).max(now);
        handler.handle_surface(Surface {
            texture: frame.texture(), device: frame.device(), context: frame.device_context(),
            size: (frame.width(), frame.height()),
        }).map_err(|error| error.to_string())?;
        if handler.preview_due() && let Ok(mut buffer) = frame.buffer() {
            let (w, h, pitch) = (buffer.width() as usize, buffer.height() as usize, buffer.row_pitch() as usize);
            handler.update_preview(buffer.as_raw_buffer(), w, h, pitch);
        }
    }
    Ok(())
}

#[cfg(test)]
mod source_order_tests {
    use super::*;

    #[test]
    #[ignore]
    fn desktop_duplication_supplies_a_real_monitor_frame() {
        use windows_capture::dxgi_duplication_api::{DxgiDuplicationApi, DxgiDuplicationFormat, Error};

        let monitor = Monitor::primary().unwrap();
        let expected = (monitor.width().unwrap(), monitor.height().unwrap());
        let mut duplication = DxgiDuplicationApi::new_options(monitor, &[DxgiDuplicationFormat::Bgra8]).unwrap();
        let rotation = duplication.duplication_desc().Rotation;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match duplication.acquire_next_frame(250) {
                Ok(mut frame) => {
                    let size = (frame.width(), frame.height());
                    assert_eq!(size, expected);
                    assert_eq!(frame.format(), DxgiDuplicationFormat::Bgra8);
                    let cursor_separate = frame.frame_info().PointerPosition.Visible.as_bool();
                    let mut pixels = frame.buffer().unwrap();
                    let pitch = pixels.row_pitch() as usize;
                    let sample = downscale(pixels.as_raw_buffer(), (size.0 as usize, size.1 as usize), pitch, (24, 16));
                    let values: std::collections::HashSet<_> = sample.as_chunks::<4>().0.iter().map(|pixel| pixel[0]).collect();
                    assert!(values.len() > 3, "desktop duplication yielded a uniform image");
                    println!("DXGI output {size:?}, {rotation:?}, separate cursor: {cursor_separate}");
                    break;
                }
                Err(Error::Timeout) if Instant::now() < deadline => {}
                Err(error) => panic!("desktop duplication could not produce a frame: {error}"),
            }
        }
    }

    #[test]
    #[ignore]
    fn capture_real_game_window_receives_frames() {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let expected = std::env::var("OPNOVERLAY_TEST_GAME_TITLE").unwrap();
        let source = list_sources().into_iter().find(|source| matches!(source.id, SourceId::Window(_)) && source.label.contains(&expected)).expect("game window not listed");
        let session = Session::start(SessionOptions { source: source.id, fps: 30, cursor: true, desktop_duplication: false, preview_fps: 0, record: Some(RecordConfig { quality: crate::config::Quality::Medium, custom_bitrate_mbps: 12, resolution: crate::config::Resolution::Native, display_canvas: None, audio: false, separate_audio_tracks: false, region: None, overlay: None }) }, None).expect("game capture failed to start");
        let out = std::path::PathBuf::from(std::env::var_os("OPNOVERLAY_TEST_GAME_FILE").expect("test recording path missing"));
        session.shared.outputs.lock().unwrap().start_file(out);
        let deadline = Instant::now() + Duration::from_secs(8);
        while session.shared.frames.load(Ordering::Relaxed) < 30 && Instant::now() < deadline && !session.is_finished() {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_secs(3));
        let (width, height, rgba) = session.snapshot().expect("screenshot failed");
        let lit = rgba.as_chunks::<4>().0.iter().filter(|pixel| pixel[0] > 24 || pixel[1] > 24 || pixel[2] > 24).count();
        println!("screenshot {width}x{height}; non-black pixels: {lit}");
        assert!(lit > (width as usize * height as usize) / 100, "screenshot is black");
        let frames = session.shared.frames.load(Ordering::Relaxed);
        let (encoder, error) = session.stop();
        assert!(frames >= 10, "game frames: {frames}, capture error: {error:?}");
        encoder.expect("encoder missing").finish().expect("recording missing").finish().expect("cannot finalize recording");

    }

    #[test]
    #[ignore]
    fn replay_real_game_window() {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let expected = std::env::var("OPNOVERLAY_TEST_GAME_TITLE").unwrap();
        let source = list_sources().into_iter().find(|source| matches!(source.id, SourceId::Window(_)) && source.label.contains(&expected)).expect("game window not listed");
        let session = Session::start(SessionOptions { source: source.id, fps: 30, cursor: true, desktop_duplication: false, preview_fps: 0, record: Some(RecordConfig { quality: crate::config::Quality::Medium, custom_bitrate_mbps: 12, resolution: crate::config::Resolution::Native, display_canvas: None, audio: false, separate_audio_tracks: false, region: None, overlay: None }) }, None).expect("game capture failed");
        session.shared.outputs.lock().unwrap().set_replay(Some(45));
        let deadline = Instant::now() + Duration::from_secs(55);
        while session.shared.frames.load(Ordering::Relaxed) < 30 && Instant::now() < deadline && !session.is_finished() {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(session.shared.frames.load(Ordering::Relaxed) >= 30, "no game frames");
        std::thread::sleep(Duration::from_secs(48));
        let replay = session.shared.outputs.lock().unwrap().replay().expect("replay buffer unavailable");
        let output = std::path::PathBuf::from(std::env::var_os("OPNOVERLAY_TEST_GAME_FILE").expect("replay output missing"));
        let path = replay.save_unique(output).expect("failed to save replay");
        println!("replay saved: {}", path.display());
        let (_, error) = session.stop();
        assert!(error.is_none(), "capture failed: {error:?}");
    }

    #[test]
    fn primary_display_is_first_without_changing_labels() {
        let mut sources = vec![
            Source { id: SourceId::Monitor(10), label: "second".into() },
            Source { id: SourceId::Monitor(20), label: "primary".into() },
        ];
        prioritize_primary(&mut sources, 20);
        assert_eq!(sources[0].id, SourceId::Monitor(20));
        assert_eq!(sources[0].label, "primary");
        assert_eq!(sources[1].id, SourceId::Monitor(10));
    }
}
