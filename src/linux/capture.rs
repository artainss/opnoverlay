//! Screen/window capture: the ScreenCast portal gives a PipeWire stream, which GStreamer turns
//! into BGRx frames for the preview, screenshots and the encoder. A recording VA-API can encode
//! takes the frames as DMA-BUFs instead, which stay on the GPU (`gpu`).

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use super::encoder::{Encoder, EncoderSettings, FrameRef, Outputs, swap_red_blue};
use super::{gpu, portal};
use crate::capture::{
    AudioApp, PREVIEW_DEFAULT_WIDTH, PreviewFrame, RecordConfig, SessionOptions, Source, SourceId, bgra_to_rgba, downscale,
};
use crate::i18n::tr;
use crate::media::Clock;

type Size = (u32, u32);

/// How long a capture taking frames on the GPU waits for the first before taking them in memory.
const NO_GPU_FRAMES: Duration = Duration::from_secs(3);

/// The portal lets the user choose the screen or window, so there is a single source.
pub fn list_sources() -> Vec<Source> {
    vec![Source { id: SourceId::Portal, label: tr!("Pantalla o ventana", "Screen or window").to_owned() }]
}

/// Capturing one application's audio is not available on Linux.
pub fn list_audio_apps() -> Vec<AudioApp> {
    Vec::new()
}

/// Forgets the screen or window chosen in the portal, so the next session asks again.
pub fn forget_choice() {
    portal::forget();
}

/// State shared between the capture thread, the audio mixer and the UI (as on Windows).
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
    /// Set once the encoder is running and the clock has started.
    pub encoder_ready: Arc<AtomicBool>,
    pub encoder: Mutex<Option<Encoder>>,
    /// The recording file and replay buffer; they can be turned on before the encoder exists.
    pub outputs: Arc<Mutex<Outputs>>,
    /// Capture size and encoded size, known once the encoder exists.
    pub sizes: Mutex<Option<(Size, Size)>>,
    /// The newest frame, for screenshots: PipeWire only sends frames when something changes.
    last_frame: Mutex<Option<gst::Sample>>,
    finished: AtomicBool,
    error: Mutex<Option<String>>,
    repaint: Option<eframe::egui::Context>,
}

impl Shared {
    fn new(repaint: Option<eframe::egui::Context>) -> Self {
        Self {
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
            finished: AtomicBool::new(false),
            error: Mutex::new(None),
            repaint,
        }
    }
}

/// What the capture pipeline does with each frame.
struct Handler {
    shared: Arc<Shared>,
    record: Option<RecordConfig>,
    fps: u32,
    preview_interval: Option<Duration>,
    last_preview: Option<Instant>,
    /// Why handling a frame failed (the encoder could not be made, say), which ends the stream.
    failed: Option<String>,
    /// The GPU could not encode the frames it was sent, so they are taken in memory.
    gpu_failed: bool,
    /// Scales frames on the GPU down for the preview, while they come so.
    scaler: Option<gpu::Scaler>,
}

impl Handler {
    fn new(shared: Arc<Shared>, record: Option<RecordConfig>, fps: u32, preview_fps: u32) -> Self {
        Self {
            shared,
            record,
            fps,
            preview_interval: (preview_fps > 0).then(|| Duration::from_secs_f64(1.0 / preview_fps as f64)),
            last_preview: None,
            failed: None,
            gpu_failed: false,
            scaler: None,
        }
    }

    fn on_frame(&mut self, sample: &gst::Sample) -> Result<(), String> {
        let shared = self.shared.clone();
        shared.frames.fetch_add(1, Ordering::Relaxed);
        *shared.last_frame.lock().unwrap() = Some(sample.clone());
        let (Some(buffer), Some(size)) = (sample.buffer(), frame_size(sample)) else { return Ok(()) };
        if gpu::on_gpu(sample) {
            let mut encoder = shared.encoder.lock().unwrap();
            if encoder.is_none() {
                *encoder = Some(self.new_encoder(sample, size)?);
            }
            let Some(enc) = encoder.as_ref() else { return Ok(()) };
            enc.push_gpu_frame(sample);
            if let Some(to) = self.preview_due(size)
                && let Some(scaler) = &mut self.scaler
                && let Some(source) = enc.newest_source()
            {
                scaler.push(&source, to);
            }
            return Ok(());
        }
        let map = buffer.map_readable().map_err(|e| e.to_string())?;
        let frame = FrameRef { data: map.as_slice(), size, stride: map.size() / size.1.max(1) as usize };

        if self.record.is_some() {
            let mut encoder = shared.encoder.lock().unwrap();
            if encoder.is_none() {
                *encoder = Some(self.new_encoder(sample, size)?);
            }
            if let Some(enc) = encoder.as_ref() {
                enc.push_frame(&frame);
            }
        }

        if let Some((pw, ph)) = self.preview_due(size) {
            let (w, h) = (size.0 as usize, size.1 as usize);
            let mut rgba = downscale(frame.data, (w, h), frame.stride, (pw as usize, ph as usize));
            // `downscale` takes blue first; swapping the small result is cheaper than the frame.
            if red_first(sample) {
                swap_red_blue(&mut rgba);
            }
            show_preview(&shared, (pw, ph), rgba);
        }
        Ok(())
    }

    /// The encoder for frames like `sample` (`size`), which starts the recording's clock.
    fn new_encoder(&mut self, sample: &gst::Sample, size: Size) -> Result<Encoder, String> {
        let rec = self.record.as_ref().ok_or("not recording")?;
        let (origin, input) = match rec.region.and_then(|r| r.fit(size)) {
            Some(r) => ((r.x, r.y), (r.width, r.height)),
            None => ((0, 0), (size.0 & !1, size.1 & !1)),
        };
        let output = rec.resolution.apply(input);
        let settings = EncoderSettings {
            origin,
            input,
            output,
            fps: self.fps,
            bitrate: rec.bitrate(output.0, output.1, self.fps),
            audio: rec.audio,
            overlay: rec.overlay,
            red_first: red_first(sample),
            gpu: gpu::on_gpu(sample).then(|| sample.clone()),
        };
        let shared = &self.shared;
        let encoder = Encoder::new(settings, shared.clock.clone(), shared.encoder_ready.clone(), shared.outputs.clone())?;
        *shared.sizes.lock().unwrap() = Some((input, output));
        Ok(encoder)
    }

    /// The size to make a preview picture of a `size` frame at, if one is due.
    fn preview_due(&mut self, size: Size) -> Option<Size> {
        let interval = self.preview_interval?;
        if self.shared.preview_hidden.load(Ordering::Relaxed) || self.last_preview.is_some_and(|t| t.elapsed() < interval) {
            return None;
        }
        self.last_preview = Some(Instant::now());
        let (w, h) = (size.0 as usize, size.1 as usize);
        let pw = self.shared.preview_width.load(Ordering::Relaxed).clamp(64.min(w), w);
        Some((pw as u32, (h * pw / w).max(1) as u32))
    }

    /// Whether frames can come as DMA-BUFs, and in which formats: when recording, on a GPU that
    /// has encoded such frames, if it has had any.
    fn gpu_formats(&self) -> Option<String> {
        if self.record.is_none() || self.gpu_failed {
            return None;
        }
        if self.shared.encoder.lock().unwrap().as_ref().is_some_and(|e| !e.on_gpu()) {
            return None;
        }
        gpu::formats()
    }
}

/// Shows an RGBA preview picture of `size`.
fn show_preview(shared: &Shared, (w, h): Size, rgba: Vec<u8>) {
    let mut preview = shared.preview.lock().unwrap();
    let seq = preview.as_ref().map_or(0, |p| p.seq + 1);
    *preview = Some(PreviewFrame { size: [w as usize, h as usize], rgba, seq });
    drop(preview);
    if let Some(ctx) = &shared.repaint {
        crate::capture::repaint_for_picture(ctx);
    }
}

/// Whether the desktop sent red first (RGBx or RGBA) rather than blue (BGRx or BGRA).
fn red_first(sample: &gst::Sample) -> bool {
    let format = sample.caps().and_then(|c| c.structure(0)).and_then(|s| s.get::<&str>("format").ok());
    format.is_some_and(|f| f.starts_with("RGB"))
}

fn frame_size(sample: &gst::Sample) -> Option<Size> {
    let s = sample.caps()?.structure(0)?;
    Some((s.get::<i32>("width").ok()? as u32, s.get::<i32>("height").ok()? as u32))
}

pub struct Session {
    pub shared: Arc<Shared>,
    /// Asks the portal and then runs the pipeline; the portal may show a dialog, so this
    /// does not run on the UI thread.
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Session {
    pub fn start(opts: SessionOptions, repaint: Option<eframe::egui::Context>) -> Result<Self, String> {
        // Only the portal's source exists here (a Windows source cannot be captured).
        if opts.source != SourceId::Portal {
            return Err(tr!("esta fuente no está disponible", "this source is not available").to_owned());
        }
        gst::init().map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared::new(repaint));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::Builder::new()
            .name("capture".into())
            .spawn({
                let (shared, stop) = (shared.clone(), stop.clone());
                move || {
                    if let Err(e) = run(opts, &shared, &stop) {
                        *shared.error.lock().unwrap() = Some(e);
                    }
                    shared.finished.store(true, Ordering::Release);
                    if let Some(ctx) = &shared.repaint {
                        ctx.request_repaint();
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { shared, thread: Some(thread), stop })
    }

    /// The capture ended: the source went away, sharing was stopped, or the portal failed.
    pub fn is_finished(&self) -> bool {
        self.shared.finished.load(Ordering::Acquire)
    }

    /// Size of the source, once a frame has arrived.
    pub fn source_size(&self) -> Option<(u32, u32)> {
        self.shared.last_frame.lock().unwrap().as_ref().and_then(frame_size)
    }

    /// The newest captured frame as RGBA pixels: (width, height, pixels).
    pub fn snapshot(&self) -> Result<(u32, u32, Vec<u8>), String> {
        let last = self.shared.last_frame.lock().unwrap();
        let none = || tr!("todavía no hay imagen de la fuente", "there is no image from the source yet").to_owned();
        let sample = last.as_ref().ok_or_else(none)?;
        let (w, h) = frame_size(sample).ok_or_else(none)?;
        if gpu::on_gpu(sample) {
            drop(last);
            let source = self.shared.encoder.lock().unwrap().as_ref().and_then(Encoder::newest_source).ok_or_else(none)?;
            return gpu::download(&source);
        }
        let map = sample.buffer().ok_or_else(none)?.map_readable().map_err(|e| e.to_string())?;
        let mut rgba = bgra_to_rgba(map.as_slice(), w as usize, h as usize, map.size() / h.max(1) as usize);
        if red_first(sample) {
            swap_red_blue(&mut rgba);
        }
        Ok((w, h, rgba))
    }

    /// Stops capturing and hands back the encoder (if recording) so it can be finalized.
    /// The error is whatever made the capture end, if anything.
    pub fn stop(mut self) -> (Option<Encoder>, Option<String>) {
        self.halt();
        let error = self.shared.error.lock().unwrap().take();
        (self.shared.encoder.lock().unwrap().take(), error)
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.halt();
    }
}

/// Portal, then pipeline, until asked to stop or the stream ends. A portal that crashes ends the
/// stream without closing the session (COSMIC's did, every few seconds to minutes): then a new
/// session is started, from the remembered choice so without the dialog, and capture goes on.
fn run(opts: SessionOptions, shared: &Arc<Shared>, stop: &AtomicBool) -> Result<(), String> {
    let handler = Arc::new(Mutex::new(Handler::new(shared.clone(), opts.record, opts.fps, opts.preview_fps)));
    loop {
        let mut cast = portal::start(opts.cursor)?;
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let frames = shared.frames.load(Ordering::Relaxed);
        let result = stream(&cast, &handler, stop);
        // Our own failure would only happen again with a new session.
        if stop.load(Ordering::Acquire) || handler.lock().unwrap().failed.is_some() {
            return result;
        }
        // The desktop's `Closed` can come just after the end of the stream. A session that sent
        // no frames is not started again, so a source that cannot work does not loop.
        thread::sleep(Duration::from_millis(500));
        if stop.load(Ordering::Acquire) || cast.was_closed() || shared.frames.load(Ordering::Relaxed) == frames {
            return result;
        }
    }
}

/// How PipeWire hands frames over.
enum Frames<'a> {
    /// As DMA-BUFs in one of these formats, on the GPU.
    Gpu(&'a str),
    /// In memory, in a format used as it is.
    AsIs,
    /// In memory, converted.
    Converted,
}

/// Runs the pipeline for one portal session until asked to stop or the stream ends.
fn stream(cast: &portal::Cast, handler: &Arc<Mutex<Handler>>, stop: &AtomicBool) -> Result<(), String> {
    // Frames on the GPU spare copying and converting them. When the desktop cannot send them, the
    // GPU cannot encode them, or none come, they come in memory, before anything is recorded.
    let formats = handler.lock().unwrap().gpu_formats();
    if let Some(formats) = formats {
        match stream_as(cast, handler, stop, Frames::Gpu(&formats)) {
            Err(_) if !stop.load(Ordering::Acquire) && handler.lock().unwrap().shared.encoder.lock().unwrap().is_none() => {}
            result => return result,
        }
    }
    // BGRx, BGRA, RGBx and RGBA are used as they are (the canvas keeps the desktop's order), so the
    // only conversion is the encoder's. A desktop that can send none of them gets the picture
    // converted.
    let frames = handler.lock().unwrap().shared.frames.load(Ordering::Relaxed);
    match stream_as(cast, handler, stop, Frames::AsIs) {
        Err(_) if !stop.load(Ordering::Acquire)
            && handler.lock().unwrap().failed.is_none()
            && handler.lock().unwrap().shared.frames.load(Ordering::Relaxed) == frames =>
        {
            stream_as(cast, handler, stop, Frames::Converted)
        }
        result => result,
    }
}

/// `stream`, taking frames as `how` says.
fn stream_as(cast: &portal::Cast, handler: &Arc<Mutex<Handler>>, stop: &AtomicBool, how: Frames) -> Result<(), String> {
    // A desktop may send frames at the display's refresh rate (144 Hz here, on COSMIC) even for
    // a still picture: no more than the session uses are converted or copied.
    let fps = handler.lock().unwrap().fps.max(1);
    let (before, after) = match how {
        Frames::Gpu(formats) => (format!("! video/x-raw(memory:DMABuf),format=DMA_DRM,drm-format={formats} "), ""),
        Frames::AsIs => ("! video/x-raw,format=(string){BGRx,BGRA,RGBx,RGBA} ".to_owned(), ""),
        Frames::Converted => (String::new(), "! videoconvert ! video/x-raw,format=BGRx "),
    };
    let on_gpu = matches!(how, Frames::Gpu(_));
    let pipeline = gst::parse::launch(&format!(
        "pipewiresrc fd={fd} path={node} do-timestamp=true keepalive-time=1000 {before}! videorate max-rate={fps} \
         {after}! appsink name=sink max-buffers=2 drop=true sync=false",
        fd = cast.fd.as_raw_fd(),
        node = cast.node,
    ))
    .map_err(|e| e.to_string())?
    .downcast::<gst::Pipeline>()
    .map_err(|_| "not a pipeline".to_owned())?;
    let sink = pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or("no sink")?;
    {
        let mut handler = handler.lock().unwrap();
        handler.scaler = None;
        if on_gpu && handler.preview_interval.is_some() {
            let shared = handler.shared.clone();
            // Without one there is no preview; the recording goes on.
            handler.scaler = gpu::Scaler::new(move |size, rgba| show_preview(&shared, size, rgba)).ok();
        }
    }
    let frames = handler.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let mut handler = frames.lock().unwrap();
                if let Err(e) = handler.on_frame(&sample) {
                    // Frames on the GPU fail only when it cannot encode them: they come in memory instead.
                    if gpu::on_gpu(&sample) {
                        handler.gpu_failed = true;
                    } else {
                        handler.failed = Some(e);
                    }
                    return Err(gst::FlowError::Error);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let result = watch(&pipeline, handler, stop, on_gpu);
    let _ = pipeline.set_state(gst::State::Null);
    handler.lock().unwrap().scaler = None;
    result
}

/// Plays a capture pipeline until asked to stop or the stream ends. One taking frames on the GPU
/// (`on_gpu`) ends if none comes in its first seconds, before recording.
fn watch(pipeline: &gst::Pipeline, handler: &Arc<Mutex<Handler>>, stop: &AtomicBool, on_gpu: bool) -> Result<(), String> {
    super::encoder::play(pipeline)?;
    let bus = pipeline.bus().ok_or("no bus")?;
    let started = Instant::now();
    let shared = handler.lock().unwrap().shared.clone();
    let frames = shared.frames.load(Ordering::Relaxed);
    let mut result = Ok(());
    while !stop.load(Ordering::Acquire) {
        if on_gpu
            && started.elapsed() > NO_GPU_FRAMES
            && shared.frames.load(Ordering::Relaxed) == frames
            && shared.encoder.lock().unwrap().is_none()
        {
            return Err(tr!("no llegan imágenes a la GPU", "no frames reach the GPU").to_owned());
        }
        let Some(message) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(100), &[gst::MessageType::Eos, gst::MessageType::Error]) else {
            continue;
        };
        if let gst::MessageView::Error(e) = message.view() {
            result = Err(handler.lock().unwrap().failed.clone().unwrap_or_else(|| e.error().to_string()));
        }
        // End of stream: the window was closed, sharing was stopped from the desktop, or the
        // portal went away.
        break;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{CameraSize, Corner};
    use crate::config::{Quality, Region, Resolution};

    #[test]
    fn a_recording_takes_frames_on_the_gpu() {
        gst::init().unwrap();
        let recording = |region, overlay| RecordConfig { quality: Quality::Medium, custom_bitrate_mbps: 12, resolution: Resolution::Native, audio: false, region, overlay };
        let handler = |record| Handler::new(Arc::new(Shared::new(None)), record, 30, 5);
        // Where VA-API can record them (not in CI), in as many formats as it reads, with a region
        // and the webcam too.
        let region = Region { x: 0, y: 0, width: 640, height: 360 };
        let webcam = Some((Corner::BottomRight, CameraSize::Small));
        for (region, overlay) in [(None, None), (Some(region), None), (None, webcam), (Some(region), webcam)] {
            assert_eq!(handler(Some(recording(region, overlay))).gpu_formats(), gpu::formats());
        }
        // The preview alone is small, and a GPU that failed is not asked again.
        assert_eq!(handler(None).gpu_formats(), None);
        let mut failed = handler(Some(recording(None, None)));
        failed.gpu_failed = true;
        assert_eq!(failed.gpu_formats(), None);
    }
}
