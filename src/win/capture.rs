//! Screen/window capture through Windows Graphics Capture.

use std::error::Error;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
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
use crate::i18n::tr;

type BoxError = Box<dyn Error + Send + Sync>;
type Size = (u32, u32);

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

#[cfg(test)]
mod source_order_tests {
    use super::*;

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

/// Applications with a visible window (one entry per executable), excluding obs-lite.
pub fn list_audio_apps() -> Vec<AudioApp> {
    let own_pid = std::process::id();
    let mut apps: Vec<AudioApp> = Vec::new();
    for w in Window::enumerate().unwrap_or_default() {
        let (Ok(title), Ok(pid), Ok(exe)) = (w.title(), w.process_id(), w.process_name()) else { continue };
        if title.trim().is_empty() || pid == own_pid || cloaked(&w) || apps.iter().any(|a| a.exe.eq_ignore_ascii_case(&exe)) {
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

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Flags;
    type Error = BoxError;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { flags: ctx.flags, last_preview: None, region: None })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, _control: InternalCaptureControl) -> Result<(), Self::Error> {
        let shared = self.flags.shared.clone();
        shared.frames.fetch_add(1, Ordering::Relaxed);
        // The screenshot copy and the preview are extras: failing them must not end a recording.
        let _ = keep_last_frame(&shared, frame);

        if let Some(rec) = &self.flags.record {
            let mut encoder = shared.encoder.lock().unwrap();
            if encoder.is_none() {
                let full = (frame.width(), frame.height());
                let input = match rec.region.and_then(|r| r.fit(full)) {
                    Some(r) => {
                        self.region = Some((r.x, r.y));
                        (r.width, r.height)
                    }
                    None => (full.0 & !1, full.1 & !1),
                };
                let output = rec.resolution.apply(input);
                let fps = self.flags.fps;
                let created = Encoder::new(
                    frame.device(),
                    EncoderSettings {
                        input,
                        output,
                        fps,
                        bitrate: rec.bitrate(output.0, output.1, fps),
                        audio: rec.audio,
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
                enc.push_frame(frame, self.region);
            }
        }

        if let Some(interval) = self.flags.preview_interval
            && !shared.preview_hidden.load(Ordering::Relaxed)
            && self.last_preview.is_none_or(|t| t.elapsed() >= interval)
        {
            self.last_preview = Some(Instant::now());
            let _ = self.update_preview(frame);
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
    /// Scales the frame down to the size the preview is drawn at.
    fn update_preview(&mut self, frame: &mut Frame) -> Result<(), BoxError> {
        let mut buffer = frame.buffer()?;
        let (w, h, pitch) = (buffer.width() as usize, buffer.height() as usize, buffer.row_pitch() as usize);
        if w == 0 || h == 0 {
            return Ok(());
        }
        let shared = &self.flags.shared;
        let pw = shared.preview_width.load(Ordering::Relaxed).clamp(64.min(w), w);
        let ph = (h * pw / w).max(1);
        let rgba = downscale(buffer.as_raw_buffer(), (w, h), pitch, (pw, ph));
        let mut preview = shared.preview.lock().unwrap();
        let seq = preview.as_ref().map_or(0, |p| p.seq + 1);
        *preview = Some(PreviewFrame { size: [pw, ph], rgba, seq });
        drop(preview);
        if let Some(ctx) = &shared.repaint {
            crate::capture::repaint_for_picture(ctx);
        }
        Ok(())
    }
}

pub struct Session {
    control: Option<CaptureControl<Handler, BoxError>>,
    pub shared: Arc<Shared>,
}

impl Session {
    pub fn start(opts: SessionOptions, repaint: Option<eframe::egui::Context>) -> Result<Self, String> {
        let shared = Arc::new(Shared {
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
        let control = match start_with_border(&opts, &shared, DrawBorderSettings::WithoutBorder) {
            Ok(c) => c,
            // Borderless capture needs Windows 11; fall back to the default border.
            Err(_) => start_with_border(&opts, &shared, DrawBorderSettings::Default)?,
        };
        Ok(Self { control: Some(control), shared })
    }

    /// The capture thread exited (source closed or handler error).
    pub fn is_finished(&self) -> bool {
        self.control.as_ref().is_none_or(CaptureControl::is_finished)
    }

    /// Size of the source, once a frame has arrived.
    pub fn source_size(&self) -> Option<(u32, u32)> {
        self.shared.last_frame.lock().unwrap().as_ref().map(|l| l.0.size)
    }

    /// The newest captured frame as RGBA pixels: (width, height, pixels).
    pub fn snapshot(&self) -> Result<(u32, u32, Vec<u8>), String> {
        let last = self.shared.last_frame.lock().unwrap();
        let Some(Com(last)) = last.as_ref() else {
            return Err(tr!("todavía no hay imagen de la fuente", "there is no image from the source yet").into());
        };
        read_back(last).map_err(|e| e.to_string())
    }

    /// Stops capturing and hands back the encoder (if recording) so it can be finalized.
    /// The error is whatever made the capture thread exit, if anything.
    pub fn stop(mut self) -> (Option<Encoder>, Option<String>) {
        let mut error = None;
        if let Some(control) = self.control.take() {
            let finished = control.is_finished();
            let result = if finished { control.wait() } else { control.stop() };
            if let Err(e) = result {
                error = Some(e.to_string());
            }
        }
        (self.shared.encoder.lock().unwrap().take(), error)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            let _ = control.stop();
        }
    }
}

fn keep_last_frame(shared: &Shared, frame: &Frame) -> windows::core::Result<()> {
    let source = frame.as_raw_texture();
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { source.GetDesc(&mut desc) };
    let mut last = shared.last_frame.lock().unwrap();
    if last.as_ref().is_none_or(|l| l.0.size != (desc.Width, desc.Height)) {
        let size = (desc.Width, desc.Height);
        desc.Usage = D3D11_USAGE_DEFAULT;
        (desc.BindFlags, desc.CPUAccessFlags, desc.MiscFlags) = (0, 0, 0);
        let mut texture = None;
        unsafe {
            frame.device().CreateTexture2D(&desc, None, Some(&mut texture))?;
            // The UI thread reads the copy back through the same immediate context.
            let _ = frame.device_context().cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
        }
        let texture = texture.expect("CreateTexture2D returned no texture");
        let (device, context) = (frame.device().clone(), frame.device_context().clone());
        *last = Some(Com(LastFrame { device, context, texture, size }));
    }
    let last = &last.as_ref().expect("set above").0;
    unsafe { last.context.CopyResource(&last.texture, source) };
    Ok(())
}

fn read_back(last: &LastFrame) -> windows::core::Result<(u32, u32, Vec<u8>)> {
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
    let flags = Flags {
        shared: shared.clone(),
        record: opts.record.clone(),
        fps: opts.fps,
        preview_interval: (opts.preview_fps > 0).then(|| Duration::from_secs_f64(1.0 / opts.preview_fps as f64)),
    };
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
