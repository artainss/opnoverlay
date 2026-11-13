//! H.264 + AAC encoding with GStreamer, producing constant-frame-rate packets for the shared
//! outputs (`media`).
//!
//! PipeWire may deliver frames only when the picture changes, so each captured frame is drawn
//! once into a picture in memory; like on Windows, a producer thread hands the newest picture
//! (with the webcam on top) to the encoder every 1/fps of recording time. The best available H.264 encoder is used:
//! VA-API (Intel/AMD), NVENC, then x264 or OpenH264 in software. Frames on the GPU (DMA-BUFs) are
//! instead cropped and converted there once each, by VA-API, with the webcam put on top there, and
//! the newest surface is what is handed over.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;

use crate::audio::{CHANNELS, SAMPLE_RATE};
use crate::camera::{self, CameraSize, Corner, PixelRect};
use crate::i18n::tr;
use crate::media::{self, Clock, Muxer, Packet};

pub type Outputs = media::Outputs<Mp4>;
pub type PendingFile = media::PendingFile<Mp4>;

/// Queued frames the encoder may fall behind by before frames are dropped (and counted).
const QUEUED_FRAMES: u64 = 8;
/// Pictures drawn from captured frames: the queued ones, those being encoded and the newest.
const MAX_PICTURES: usize = QUEUED_FRAMES as usize + 4;

/// A picture shared with GStreamer, which reads it in place.
struct Shared(Arc<Vec<u8>>);

impl AsRef<[u8]> for Shared {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}
/// Packet times are in 100 ns units.
const NS_PER_TICK: u64 = 100;
/// The smallest picture VA-API's compositor takes, in pixels a side.
const MIN_COMPOSITED: u32 = 16;
/// Whole frames kept a second when they come on the GPU, for screenshots and the preview: twice
/// as many as it draws, so each finds a fresh one.
const SOURCE_COPIES: u32 = 10;

pub struct EncoderSettings {
    /// Top-left corner of the recorded part of each frame (a region), and its size.
    pub origin: (u32, u32),
    pub input: (u32, u32),
    pub output: (u32, u32),
    pub fps: u32,
    pub bitrate: u32,
    pub audio: bool,
    /// Where the webcam goes, if there is one.
    pub overlay: Option<(Corner, CameraSize)>,
    /// The pictures hold red first (RGBx, as some desktops send them) instead of blue (BGRx).
    pub red_first: bool,
    /// The first frame, if frames come on the GPU (DMA-BUFs, see `push_gpu_frame`).
    pub gpu: Option<gst::Sample>,
}

/// The newest picture: drawn in memory, or a VA-API surface converted from a frame on the GPU.
#[derive(Clone)]
enum Picture {
    Memory(Arc<Vec<u8>>),
    Surface(gst::Buffer),
}

/// A captured picture: rows of 4-byte pixels (BGRx, or RGBx) `stride` bytes apart.
pub struct FrameRef<'a> {
    pub data: &'a [u8],
    pub size: (u32, u32),
    pub stride: usize,
}

struct Inner {
    /// The picture being recorded, BGRx at the input size.
    /// Pictures drawn from captured frames (`input` sized), drawn on again once nothing reads
    /// them; the newest is what the producer hands over, without copying it.
    pictures: Mutex<Vec<Arc<Vec<u8>>>>,
    latest: Mutex<Option<Picture>>,
    /// The newest whole frame, a VA-API surface, if frames come on the GPU (`SOURCE_COPIES`).
    source: Mutex<Option<gst::Sample>>,
    /// The webcam picture, already scaled, and where it goes.
    overlay: Mutex<Option<(Vec<u8>, PixelRect)>>,
    red_first: bool,
    ready: AtomicBool,
    clock: Arc<Clock>,
    encoded: AtomicU64,
    lagged: AtomicU64,
    stop: AtomicBool,
    fps: u64,
    origin: (u32, u32),
    input: (u32, u32),
    output_size: (u32, u32),
    video_src: gst_app::AppSrc,
    /// The video pipeline, to notice when it fails.
    video: gst::Pipeline,
    /// The H.264 encoder element, to ask it for keyframes.
    h264: gst::Element,
    output: Arc<Mutex<Outputs>>,
}

/// The webcam on the GPU: its pictures' source, the compositor's pad they reach, and where on the
/// recording they were last put.
struct Camera {
    src: gst_app::AppSrc,
    pad: gst::Pad,
    placed: Mutex<Option<PixelRect>>,
}

pub struct Encoder {
    inner: Arc<Inner>,
    producer: Option<JoinHandle<()>>,
    video: gst::Pipeline,
    /// Where frames on the GPU go to be converted, if they come so, and where the webcam's
    /// pictures go then, if there is a webcam.
    frames: Option<gst_app::AppSrc>,
    camera: Option<Camera>,
    audio: Option<(gst::Pipeline, gst_app::AppSrc)>,
    audio_frames: Mutex<u64>,
    overlay: Option<(Corner, CameraSize)>,
    name: String,
}

impl Encoder {
    /// Creates the encoder and starts `clock` (setting `started`); from then on the clock
    /// drives both video and audio.
    pub fn new(s: EncoderSettings, clock: Arc<Clock>, started: Arc<AtomicBool>, outputs: Arc<Mutex<Outputs>>) -> Result<Self, String> {
        gst::init().map_err(|e| e.to_string())?;
        let fps = s.fps.max(1);
        let (iw, ih) = s.input;
        let (name, video) = match &s.gpu {
            Some(frame) => {
                // VA-API converts the frames, so only its encoders can take them as they are.
                let va = h264_candidates(s.bitrate, fps).into_iter().filter(|(factory, ..)| factory.starts_with("va"));
                let (name, _, h264) = h264_encoder(va, |_, fragment| encodes_on_gpu(frame, fragment, s.output))?;
                let video = launch(&gpu_pipeline(&s, fps, &h264))?;
                super::gpu::share_display(&video);
                crop_on_the_way(&video, s.origin, s.input)?;
                (format!("{name}, frames on the GPU"), video)
            }
            None => {
                let (name, format, h264) = h264_encoder(h264_candidates(s.bitrate, fps), |format, fragment| encodes(format, fragment, s.output))?;
                // Two threads: one leaves 4K60 too little time per frame, and every thread there is costs
                // much more CPU (80 % more at 4K) for the work of sharing it.
                let caps = raw_caps(format, s.output);
                // The canvas is in the order the desktop sent (BGRx or RGBx), so only the encoder's own
                // conversion is left. An encoder that takes BGRA or RGBA gets it as such, its fourth byte
                // unused, so nothing is converted.
                let canvas = match (format == "BGRA", s.red_first) {
                    (true, false) => "BGRA",
                    (true, true) => "RGBA",
                    (false, false) => "BGRx",
                    (false, true) => "RGBx",
                };
                let video = launch(&format!(
                    "appsrc name=src is-live=true format=time block=false max-bytes={max} \
                       caps=video/x-raw,format={canvas},width={iw},height={ih},framerate={fps}/1 \
                     ! videoconvert n-threads=2 ! videoscale ! {caps} \
                     ! {h264} ! h264parse ! video/x-h264,stream-format=avc,alignment=au \
                     ! appsink name=sink sync=false",
                    max = iw as u64 * ih as u64 * 4 * (QUEUED_FRAMES + 2),
                ))?;
                (name, video)
            }
        };
        let video_src = by_name::<gst_app::AppSrc>(&video, "src")?;
        let frames = s.gpu.as_ref().map(|_| by_name::<gst_app::AppSrc>(&video, "frames")).transpose()?;
        let camera = match (&frames, s.overlay) {
            (Some(_), Some(_)) => {
                let pad = by_name::<gst::Element>(&video, "mix")?.static_pad("sink_1").ok_or("no webcam pad")?;
                Some(Camera { src: by_name::<gst_app::AppSrc>(&video, "camera")?, pad, placed: Mutex::new(None) })
            }
            _ => None,
        };
        let h264_element = by_name::<gst::Element>(&video, "enc")?;

        let audio = if s.audio {
            let aac = aac_encoder()?;
            let pipeline = launch(&format!(
                "appsrc name=src is-live=true format=time block=false \
                   caps=audio/x-raw,format=S16LE,layout=interleaved,rate={SAMPLE_RATE},channels={CHANNELS} \
                 ! audioconvert ! {aac} ! aacparse ! audio/mpeg,mpegversion=4,stream-format=raw \
                 ! appsink name=sink sync=false"
            ))?;
            let src = by_name::<gst_app::AppSrc>(&pipeline, "src")?;
            outputs.lock().unwrap().set_audio_format(Some(aac_caps()));
            Some((pipeline, src))
        } else {
            None
        };

        let inner = Arc::new(Inner {
            pictures: Mutex::new(Vec::new()),
            latest: Mutex::new(None),
            source: Mutex::new(None),
            overlay: Mutex::new(None),
            red_first: s.red_first,
            ready: AtomicBool::new(false),
            clock,
            encoded: AtomicU64::new(0),
            lagged: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            fps: fps as u64,
            origin: s.origin,
            input: s.input,
            output_size: s.output,
            video_src,
            video: video.clone(),
            h264: h264_element,
            output: outputs,
        });

        let sink = by_name::<gst_app::AppSink>(&video, "sink")?;
        let out = inner.output.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(packet) = to_packet(&sample) {
                        let caps = sample.caps().map(|c| c.to_owned());
                        out.lock().unwrap().video(packet, || caps.ok_or_else(|| "no H.264 caps".to_owned()));
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        if frames.is_some() {
            // The callback must not keep the encoder alive: it lives as long as the pipeline.
            let weak = Arc::downgrade(&inner);
            by_name::<gst_app::AppSink>(&video, "surfaces")?.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let (Some(inner), Some(surface)) = (weak.upgrade(), sample.buffer_owned()) else { return Ok(gst::FlowSuccess::Ok) };
                        *inner.latest.lock().unwrap() = Some(Picture::Surface(surface));
                        inner.ready.store(true, Ordering::Release);
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
            let weak = Arc::downgrade(&inner);
            by_name::<gst_app::AppSink>(&video, "sources")?.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        if let Some(inner) = weak.upgrade() {
                            *inner.source.lock().unwrap() = Some(sample);
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }
        if let Some((pipeline, _)) = &audio {
            let sink = by_name::<gst_app::AppSink>(pipeline, "sink")?;
            let out = inner.output.clone();
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |sink| {
                        let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        if let Some(packet) = to_packet(&sample) {
                            out.lock().unwrap().audio(vec![Packet { key: true, ..packet }]);
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }

        let pipelines = || std::iter::once(&video).chain(audio.as_ref().map(|(p, _)| p));
        // The probe encoded a frame, but it also has to get through the crop, the webcam and the
        // conversion: unless its surface comes before recording starts, frames are taken in memory.
        let first = |()| match (&s.gpu, &frames) {
            (Some(frame), Some(frames)) => first_surface(&video, frames, frame, &inner.ready),
            _ => Ok(()),
        };
        if let Err(e) = pipelines().try_for_each(play).and_then(first) {
            for pipeline in pipelines() {
                let _ = pipeline.set_state(gst::State::Null);
            }
            return Err(e);
        }
        // Time starts now, so encoder start-up is not counted as lag.
        inner.clock.restart();
        started.store(true, Ordering::Release);

        let producer = thread::Builder::new()
            .name("video-frames".into())
            .spawn({
                let inner = inner.clone();
                move || produce_frames(&inner)
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { inner, producer: Some(producer), video, frames, camera, audio, audio_frames: Mutex::new(0), overlay: s.overlay, name })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether frames come on the GPU (`push_gpu_frame`) rather than in memory (`push_frame`).
    pub fn on_gpu(&self) -> bool {
        self.frames.is_some()
    }

    /// The newest whole frame, a VA-API surface, if frames come on the GPU: for the preview and
    /// screenshots, which must not read the desktop's frame while it is converted (two
    /// conversions of one frame at once crashed Intel's driver).
    pub fn newest_source(&self) -> Option<gst::Sample> {
        self.inner.source.lock().unwrap().clone()
    }

    /// Draws the recorded part of a captured frame onto a picture nothing reads, which becomes
    /// the newest. With none free (the encoder far behind) the frame is skipped.
    pub fn push_frame(&self, frame: &FrameRef) {
        let size = (self.inner.input.0 as usize * self.inner.input.1 as usize * 4, self.inner.input);
        let mut pictures = self.inner.pictures.lock().unwrap();
        let Some(free) = free_picture(&mut pictures, size.0) else { return };
        let picture = Arc::get_mut(&mut pictures[free]).expect("nothing else holds a free picture");
        copy_region(frame, self.inner.origin, picture, size.1);
        *self.inner.latest.lock().unwrap() = Some(Picture::Memory(pictures[free].clone()));
        self.inner.ready.store(true, Ordering::Release);
    }

    /// Converts a frame on the GPU (a DMA-BUF) into the newest picture, there. The PipeWire
    /// buffer is let go once converted, so the desktop can draw into it again.
    pub fn push_gpu_frame(&self, frame: &gst::Sample) {
        if let Some(frames) = &self.frames {
            push_retimed(frames, frame);
        }
    }

    /// Where a `camera`-sized webcam picture goes on the recording, if there is an overlay.
    pub fn overlay_rect(&self, camera: (u32, u32)) -> Option<PixelRect> {
        self.overlay.map(|(corner, size)| camera::overlay_rect(self.inner.input, camera, corner, size))
    }

    /// Takes a webcam picture already scaled to `rect` (see `overlay_rect`).
    pub fn push_overlay(&self, rect: PixelRect, pixels: &[u8]) {
        if let Some(camera) = &self.camera {
            // The compositor works at the recording's size, and scales the picture to its place there.
            let mut placed = camera.placed.lock().unwrap();
            if *placed != Some(rect) {
                let (x, y, w, h) = scale_rect(rect, self.inner.input, self.inner.output_size);
                camera.pad.set_property("xpos", x as i32);
                camera.pad.set_property("ypos", y as i32);
                camera.pad.set_property("width", w as i32);
                camera.pad.set_property("height", h as i32);
                *placed = Some(rect);
            }
            // The compositor takes no picture under 16 pixels a side: a larger copy goes in its place.
            let size = (rect.2.max(MIN_COMPOSITED), rect.3.max(MIN_COMPOSITED));
            let pixels = if size == (rect.2, rect.3) { pixels.to_vec() } else { camera::resize(pixels, (rect.2, rect.3), size) };
            let caps = gst::Caps::builder("video/x-raw")
                .field("format", "BGRA")
                .field("width", size.0 as i32)
                .field("height", size.1 as i32)
                .field("framerate", gst::Fraction::new(0, 1))
                .build();
            let buffer = gst::Buffer::from_slice(pixels);
            let _ = camera.src.push_sample(&gst::Sample::builder().buffer(&buffer).caps(&caps).build());
            return;
        }
        // Webcam pictures are BGRA; the canvas may hold red first.
        let mut pixels = pixels.to_vec();
        if self.inner.red_first {
            swap_red_blue(&mut pixels);
        }
        *self.inner.overlay.lock().unwrap() = Some((pixels, rect));
    }

    /// Interleaved 16-bit stereo PCM from the mixer.
    pub fn push_audio(&self, pcm: Vec<u8>) {
        let Some((pipeline, src)) = &self.audio else { return };
        if let Some(e) = posted_error(pipeline) {
            self.inner.output.lock().unwrap().record_error(e);
        }
        let frames = (pcm.len() / (CHANNELS * 2)) as u64;
        let mut done = self.audio_frames.lock().unwrap();
        let mut buffer = gst::Buffer::from_mut_slice(pcm);
        {
            let buffer = buffer.get_mut().expect("new buffer");
            buffer.set_pts(gst::ClockTime::from_nseconds(*done * 1_000_000_000 / SAMPLE_RATE as u64));
            buffer.set_duration(gst::ClockTime::from_nseconds(frames * 1_000_000_000 / SAMPLE_RATE as u64));
        }
        *done += frames;
        let _ = src.push_buffer(buffer);
    }

    /// (frames encoded, frame slots skipped because the machine fell behind).
    pub fn stats(&self) -> (u64, u64) {
        (self.inner.encoded.load(Ordering::Relaxed), self.inner.lagged.load(Ordering::Relaxed))
    }

    /// Ends both streams, waiting for the encoders to hand over their last packets; what
    /// was being recorded still has to be finished.
    pub fn finish(mut self) -> Option<PendingFile> {
        self.stop();
        self.inner.output.lock().unwrap().take_file()
    }

    fn stop(&mut self) {
        let Some(producer) = self.producer.take() else { return };
        self.inner.stop.store(true, Ordering::Release);
        let _ = producer.join();
        let _ = self.inner.video_src.end_of_stream();
        // The compositor ends once both its inputs have.
        for src in self.frames.iter().chain(self.camera.as_ref().map(|c| &c.src)) {
            let _ = src.end_of_stream();
        }
        if let Some((_, src)) = &self.audio {
            let _ = src.end_of_stream();
        }
        for pipeline in std::iter::once(&self.video).chain(self.audio.as_ref().map(|(p, _)| p)) {
            if let Some(e) = wait_for_eos(pipeline) {
                self.inner.output.lock().unwrap().record_error(e);
            }
            let _ = pipeline.set_state(gst::State::Null);
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.stop();
        if let Some(file) = self.inner.output.lock().unwrap().take_file() {
            let _ = file.finish();
        }
    }
}

fn produce_frames(inner: &Inner) {
    let fps = inner.fps;
    let mut n = 0u64;
    let frame_ns = 1_000_000_000 / fps;
    while !inner.stop.load(Ordering::Acquire) {
        let now = inner.clock.elapsed();
        let due = Duration::from_nanos(n * 1_000_000_000 / fps);
        if !inner.ready.load(Ordering::Acquire) || now < due {
            thread::sleep(due.saturating_sub(now).clamp(Duration::from_millis(1), Duration::from_millis(5)));
            continue;
        }
        // Descheduled for more than a frame: skip ahead rather than drift from real time.
        let current = (now.as_nanos() * fps as u128 / 1_000_000_000) as u64;
        if current > n + 1 {
            inner.lagged.fetch_add(current - n, Ordering::Relaxed);
            n = current;
        }
        // The encoder is falling behind: drop this slot rather than queue without bound.
        if inner.video_src.property::<u64>("current-level-buffers") > QUEUED_FRAMES {
            inner.lagged.fetch_add(1, Ordering::Relaxed);
            n += 1;
            continue;
        }
        let Some(picture) = inner.latest.lock().unwrap().clone() else { continue };
        // The same picture goes out as long as nothing new is captured, shared, not copied; only
        // a webcam on top needs a picture of its own.
        let mut buffer = match (picture, inner.overlay.lock().unwrap().as_ref()) {
            (Picture::Surface(surface), _) => surface.copy(),
            (Picture::Memory(picture), Some((pixels, rect))) => {
                let mut composed = picture.as_ref().clone();
                paste(&mut composed, inner.input.0, *rect, pixels);
                gst::Buffer::from_mut_slice(composed)
            }
            (Picture::Memory(picture), None) => gst::Buffer::from_slice(Shared(picture)),
        };
        if inner.output.lock().unwrap().take_keyframe_request() {
            inner.h264.send_event(gst_video::UpstreamForceKeyUnitEvent::builder().all_headers(true).build());
        }
        {
            let buffer = buffer.get_mut().expect("new buffer");
            buffer.set_pts(gst::ClockTime::from_nseconds(n * 1_000_000_000 / fps));
            buffer.set_duration(gst::ClockTime::from_nseconds(frame_ns));
        }
        if let Err(e) = inner.video_src.push_buffer(buffer) {
            inner.output.lock().unwrap().encoder_error(e.to_string());
            break;
        }
        if let Some(e) = posted_error(&inner.video) {
            inner.output.lock().unwrap().encoder_error(e);
            break;
        }
        inner.encoded.fetch_add(1, Ordering::Relaxed);
        n += 1;
    }
}

/// Copies the part of `frame` starting at `origin` into `canvas` (`size`, BGRx), padding with
/// black what the frame does not cover (a region past its edge, or a window that shrank).
fn copy_region(frame: &FrameRef, origin: (u32, u32), canvas: &mut [u8], size: (u32, u32)) {
    let (w, h) = (size.0 as usize, size.1 as usize);
    let (ox, oy) = (origin.0 as usize, origin.1 as usize);
    let (fw, fh) = (frame.size.0 as usize, frame.size.1 as usize);
    let width = fw.saturating_sub(ox).min(w);
    for y in 0..h {
        let row = &mut canvas[y * w * 4..(y + 1) * w * 4];
        let src_y = oy + y;
        if src_y < fh && width > 0 {
            let start = src_y * frame.stride + ox * 4;
            row[..width * 4].copy_from_slice(&frame.data[start..start + width * 4]);
            row[width * 4..].fill(0);
        } else {
            row.fill(0);
        }
    }
}

/// A picture of `bytes` nothing else holds (not the newest, not one the encoder still has),
/// made if there are fewer than `MAX_PICTURES`.
fn free_picture(pictures: &mut Vec<Arc<Vec<u8>>>, bytes: usize) -> Option<usize> {
    if let Some(i) = pictures.iter().position(|p| Arc::strong_count(p) == 1) {
        return Some(i);
    }
    (pictures.len() < MAX_PICTURES).then(|| {
        pictures.push(Arc::new(vec![0; bytes]));
        pictures.len() - 1
    })
}

/// Turns BGRA pixels into RGBA, or the other way.
pub(super) fn swap_red_blue(pixels: &mut [u8]) {
    for px in pixels.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
}

/// Draws `pixels` (packed, `rect` sized) onto a `width`-pixel-wide picture.
fn paste(picture: &mut [u8], width: u32, rect: PixelRect, pixels: &[u8]) {
    let (x, y, w, h) = (rect.0 as usize, rect.1 as usize, rect.2 as usize, rect.3 as usize);
    let width = width as usize;
    for row in 0..h {
        let start = ((y + row) * width + x) * 4;
        let Some(dst) = picture.get_mut(start..start + w * 4) else { break };
        dst.copy_from_slice(&pixels[row * w * 4..(row + 1) * w * 4]);
    }
}

/// The encoded buffer of `sample` as a packet (times in 100 ns units). Times are taken as
/// running time: GStreamer video encoders shift their timestamps by 1000 hours (so decoding
/// times stay positive) and their segment by the same amount.
fn to_packet(sample: &gst::Sample) -> Option<Packet> {
    let buffer = sample.buffer()?;
    let segment = sample.segment()?.downcast_ref::<gst::ClockTime>()?;
    let time = segment.to_running_time(buffer.pts()?)?;
    let map = buffer.map_readable().ok()?;
    Some(Packet {
        data: map.as_slice().into(),
        time: (time.nseconds() / NS_PER_TICK) as i64,
        duration: buffer.duration().map_or(0, |d| (d.nseconds() / NS_PER_TICK) as i64),
        key: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
    })
}

/// H.264 encoders, hardware first: the factory, the raw format it is given (VA-API only takes
/// NV12, OpenH264 only I420) and a launch fragment named `enc`.
fn h264_candidates(bitrate: u32, fps: u32) -> [(&'static str, &'static str, String); 5] {
    let kbps = (bitrate / 1000).max(1);
    // A keyframe every 2 s: where a replay or a new recording can start.
    let gop = fps * 2;
    [
        ("vah264enc", "NV12", format!("vah264enc name=enc bitrate={kbps} key-int-max={gop} b-frames=0")),
        ("vah264lpenc", "NV12", format!("vah264lpenc name=enc bitrate={kbps} key-int-max={gop} b-frames=0")),
        // NVENC converts RGB itself, on the GPU: no conversion on the CPU at all.
        ("nvh264enc", "BGRA", format!("nvh264enc name=enc bitrate={kbps} gop-size={gop} bframes=0")),
        ("x264enc", "I420", format!("x264enc name=enc bitrate={kbps} key-int-max={gop} tune=zerolatency speed-preset=veryfast")),
        ("openh264enc", "I420", format!("openh264enc name=enc bitrate={bitrate} gop-size={gop} complexity=low")),
    ]
}

/// The first H.264 encoder that is installed and starts: its factory, raw format and launch
/// fragment. Some are installed but fail at their first picture (a driver that does not match
/// the GStreamer version, a size the GPU does not support), so each is tried on one picture of
/// the recording's size.
fn h264_encoder(
    candidates: impl IntoIterator<Item = (&'static str, &'static str, String)>,
    works: impl Fn(&str, &str) -> Result<(), String>,
) -> Result<(String, &'static str, String), String> {
    let mut failed = None;
    for (factory, format, fragment) in candidates {
        if gst::ElementFactory::find(factory).is_none() {
            continue;
        }
        match works(format, &fragment) {
            Ok(()) => return Ok((factory.to_owned(), format, fragment)),
            Err(e) => failed = Some(format!("{factory}: {e}")),
        }
    }
    Err(match failed {
        Some(e) => format!("{}: {e}", tr!("ningún codificador H.264 de GStreamer funciona", "no GStreamer H.264 encoder works")),
        None => tr!("no hay ningún codificador H.264 de GStreamer instalado", "no GStreamer H.264 encoder is installed").to_owned(),
    })
}

/// Raw `format` pictures of `size`. YUV is BT.709, as players expect of HD video; RGB has no
/// matrix to state.
fn raw_caps(format: &str, (width, height): (u32, u32)) -> String {
    let colorimetry = if format.starts_with("BGR") || format.starts_with("RGB") { "" } else { ",colorimetry=bt709" };
    format!("video/x-raw,format={format},width={width},height={height}{colorimetry}")
}

/// The pipeline for frames on the GPU. Each is cropped and scaled once, as it comes (`frames`, at
/// `crop`, to `surfaces`), with the webcam put on top (from `camera`) if there is one; a few a
/// second are also kept whole (`sources`). The newest surface is what the producer hands the
/// encoder (`src`). Both branches run in the thread frames come in, one after the other: VA-API
/// reading one frame twice at once crashed Intel's driver.
fn gpu_pipeline(s: &EncoderSettings, fps: u32, h264: &str) -> String {
    let surfaces = surface_caps(s.output);
    let src = "is-live=true do-timestamp=true format=time block=false max-bytes=0 max-buffers=2 leaky-type=downstream";
    let sink = "max-buffers=1 drop=true sync=false async=false";
    // The compositor makes pictures at the frame rate (its inputs are live), the webcam's newest on
    // the screen's newest.
    let recorded = match s.overlay {
        Some(_) => format!(
            "! {surfaces} ! mix.sink_0 appsrc name=camera {src} ! mix.sink_1 \
             vacompositor name=mix ! {surfaces},framerate={fps}/1"
        ),
        None => format!("! {surfaces}"),
    };
    format!(
        "appsrc name=frames {src} ! tee name=t \
         t. ! vapostproc name=crop add-borders=true {recorded} ! appsink name=surfaces {sink} \
         t. ! videorate drop-only=true max-rate={SOURCE_COPIES} ! vapostproc \
            ! video/x-raw(memory:VAMemory),format=BGRA ! appsink name=sources {sink} \
         appsrc name=src is-live=true format=time block=false caps=\"{surfaces},framerate={fps}/1\" \
         ! {h264} ! h264parse ! video/x-h264,stream-format=avc,alignment=au ! appsink name=sink {sink}"
    )
}

/// Crops each frame reaching `crop` in `pipeline` to the recorded part (`input` from `origin`),
/// for that branch only: the whole frames are kept too.
fn crop_on_the_way(pipeline: &gst::Pipeline, origin: (u32, u32), input: (u32, u32)) -> Result<(), String> {
    let pad = by_name::<gst::Element>(pipeline, "crop")?.static_pad("sink").ok_or("no crop pad")?;
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let frame = pad.current_caps().and_then(|caps| {
            let s = caps.structure(0)?;
            Some((s.get::<i32>("width").ok()? as u32, s.get::<i32>("height").ok()? as u32))
        });
        let (Some(frame), Some(buffer)) = (frame, info.buffer_mut()) else { return gst::PadProbeReturn::Ok };
        match crop(frame, origin, input) {
            Crop::Whole => {}
            Crop::Part(rect) => {
                gst_video::VideoCropMeta::add(buffer.make_mut(), rect);
            }
            // Nothing outside the region is recorded: the recording keeps its last picture.
            Crop::Nothing => return gst::PadProbeReturn::Drop,
        }
        gst::PadProbeReturn::Ok
    });
    Ok(())
}

/// Hands `frame` to `frames`, timed by that pipeline as it comes (`do-timestamp`), for the webcam
/// to be put on it.
fn push_retimed(frames: &gst_app::AppSrc, frame: &gst::Sample) {
    let (Some(mut buffer), Some(caps)) = (frame.buffer_owned(), frame.caps_owned()) else { return };
    let buffer_mut = buffer.make_mut();
    buffer_mut.set_pts(gst::ClockTime::NONE);
    buffer_mut.set_dts(gst::ClockTime::NONE);
    let _ = frames.push_sample(&gst::Sample::builder().buffer(&buffer).caps(&caps).build());
}

/// Hands `frame` to `frames` in `pipeline` and waits (at most 3 s) for its surface (`ready`), or
/// the error that stopped it.
fn first_surface(pipeline: &gst::Pipeline, frames: &gst_app::AppSrc, frame: &gst::Sample, ready: &AtomicBool) -> Result<(), String> {
    push_retimed(frames, frame);
    let start = std::time::Instant::now();
    while !ready.load(Ordering::Acquire) {
        if let Some(e) = posted_error(pipeline) {
            return Err(e);
        }
        if start.elapsed() > Duration::from_secs(3) {
            return Err(tr!("la GPU no convirtió la imagen", "the GPU did not convert the picture").to_owned());
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// What of a frame is recorded.
#[derive(Debug, PartialEq)]
enum Crop {
    Whole,
    /// (x, y, width, height).
    Part((u32, u32, u32, u32)),
    /// The frame has none of the recorded part (a window that shrank past the region).
    Nothing,
}

/// The part of a `frame`-sized frame recorded: `input` from `origin`, as much as the frame has of
/// it. A frame an odd number of pixels wide or high is cropped to the even size recorded, as in
/// memory, not scaled to it.
fn crop(frame: (u32, u32), origin: (u32, u32), input: (u32, u32)) -> Crop {
    let (x, y) = (origin.0.min(frame.0), origin.1.min(frame.1));
    let (w, h) = (input.0.min(frame.0 - x), input.1.min(frame.1 - y));
    match (x, y, w, h) {
        (_, _, 0, _) | (_, _, _, 0) => Crop::Nothing,
        rect if rect == (0, 0, frame.0, frame.1) => Crop::Whole,
        rect => Crop::Part(rect),
    }
}

/// `rect` on a `from`-sized picture, on the same picture scaled to `to`.
fn scale_rect((x, y, w, h): PixelRect, from: (u32, u32), to: (u32, u32)) -> PixelRect {
    let scale = |v: u32, to: u32, from: u32| (v as u64 * to as u64 / from.max(1) as u64) as u32;
    (scale(x, to.0, from.0), scale(y, to.1, from.1), scale(w, to.0, from.0).max(1), scale(h, to.1, from.1).max(1))
}

/// VA-API surfaces of `size`, as the GPU path converts frames to.
fn surface_caps((width, height): (u32, u32)) -> String {
    format!("video/x-raw(memory:VAMemory),format=NV12,width={width},height={height},colorimetry=bt709")
}

/// Encodes one `size` test picture with `fragment`, given `format` pictures.
fn encodes(format: &str, fragment: &str, (width, height): (u32, u32)) -> Result<(), String> {
    let pipeline = launch(&format!(
        "videotestsrc num-buffers=1 ! video/x-raw,width={width},height={height} \
         ! videoconvert ! {caps} ! {fragment} ! fakesink",
        caps = raw_caps(format, (width, height)),
    ))?;
    play(&pipeline)?;
    finishes(&pipeline)
}

/// Encodes `frame`, on the GPU, with `fragment` at `size`, converting it as recording does: the
/// GPU may be unable to read the desktop's frames or to encode them.
fn encodes_on_gpu(frame: &gst::Sample, fragment: &str, size: (u32, u32)) -> Result<(), String> {
    let pipeline = launch(&format!("appsrc name=src format=time ! vapostproc ! {} ! {fragment} ! fakesink", surface_caps(size)))?;
    super::gpu::share_display(&pipeline);
    let src = by_name::<gst_app::AppSrc>(&pipeline, "src")?;
    play(&pipeline)?;
    let _ = src.push_sample(frame);
    let _ = src.end_of_stream();
    finishes(&pipeline)
}

/// Waits (at most 5 s) for a pipeline given one picture to end; the error if it did not.
fn finishes(pipeline: &gst::Pipeline) -> Result<(), String> {
    let message = pipeline
        .bus()
        .and_then(|bus| bus.timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Eos, gst::MessageType::Error]));
    let _ = pipeline.set_state(gst::State::Null);
    match message.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) => Ok(()),
        Some(gst::MessageView::Error(e)) => Err(e.error().to_string()),
        _ => Err(tr!("no terminó en 5 s", "did not finish in 5 s").to_owned()),
    }
}

/// The first available AAC encoder, as a launch fragment (192 kbit/s).
fn aac_encoder() -> Result<String, String> {
    ["fdkaacenc", "avenc_aac", "voaacenc"]
        .into_iter()
        .find(|factory| gst::ElementFactory::find(factory).is_some())
        .map(|factory| format!("{factory} bitrate=192000"))
        .ok_or_else(|| tr!("no hay ningún codificador AAC de GStreamer instalado", "no GStreamer AAC encoder is installed").to_owned())
}

/// AAC-LC, 48 kHz stereo, raw: `codec_data` is the AudioSpecificConfig for exactly that, so the
/// file's audio stream can be declared before the first packet arrives.
fn aac_caps() -> gst::Caps {
    gst::Caps::builder("audio/mpeg")
        .field("mpegversion", 4i32)
        .field("stream-format", "raw")
        .field("channels", CHANNELS as i32)
        .field("rate", SAMPLE_RATE as i32)
        .field("codec_data", gst::Buffer::from_slice([0x11u8, 0x90]))
        .build()
}

fn launch(description: &str) -> Result<gst::Pipeline, String> {
    gst::parse::launch(description)
        .map_err(|e| e.to_string())?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "not a pipeline".to_owned())
}

fn by_name<T: IsA<gst::Element>>(pipeline: &gst::Pipeline, name: &str) -> Result<T, String> {
    pipeline.by_name(name).and_then(|e| e.downcast::<T>().ok()).ok_or_else(|| format!("no element {name}"))
}

/// Starts `pipeline`. If it cannot start, it goes back to NULL (a pipeline must not be dropped
/// in another state), and the error is the element's own message, such as a file that cannot
/// be opened, rather than "failed to change its state".
pub(super) fn play(pipeline: &gst::Pipeline) -> Result<(), String> {
    let Err(e) = pipeline.set_state(gst::State::Playing) else { return Ok(()) };
    let message = posted_error(pipeline);
    let _ = pipeline.set_state(gst::State::Null);
    Err(message.unwrap_or_else(|| e.to_string()))
}

/// The error a running pipeline has posted, if any, without waiting: a pipeline that fails
/// keeps taking buffers without complaint, so this is how the failure is noticed.
pub(super) fn posted_error(pipeline: &gst::Pipeline) -> Option<String> {
    let message = pipeline.bus()?.pop_filtered(&[gst::MessageType::Error])?;
    match message.view() {
        gst::MessageView::Error(e) => Some(e.error().to_string()),
        _ => None,
    }
}

/// Waits (at most 10 s) for a pipeline to drain after end-of-stream; the error if it failed.
fn wait_for_eos(pipeline: &gst::Pipeline) -> Option<String> {
    let bus = pipeline.bus()?;
    let message = bus.timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Eos, gst::MessageType::Error])?;
    match message.view() {
        gst::MessageView::Error(e) => Some(e.error().to_string()),
        _ => None,
    }
}

/// The GStreamer MP4 muxer, fed already encoded packets.
pub struct Mp4 {
    pipeline: gst::Pipeline,
    path: std::path::PathBuf,
    video: gst_app::AppSrc,
    audio: Option<gst_app::AppSrc>,
    start: i64,
    /// Why the pipeline failed, once it has: it will not reach end-of-stream.
    failed: Mutex<Option<String>>,
}

impl Muxer for Mp4 {
    type Format = gst::Caps;

    fn create(path: &Path, video: &gst::Caps, audio: Option<&gst::Caps>, start: i64) -> Result<Self, String> {
        gst::init().map_err(|e| e.to_string())?;
        // The parsers adapt the encoders' caps to what mp4mux accepts. The file is written in
        // half-second fragments, so it plays up to the last one if the process dies; `finish`
        // makes it a regular MP4. (With mp4mux's own `first-moov-then-finalise` mode, a recording
        // cut short did not open.)
        let audio_branch = if audio.is_some() { "appsrc name=a format=time ! aacparse ! queue ! mux." } else { "" };
        let pipeline = launch(&format!(
            "appsrc name=v format=time ! h264parse ! queue \
             ! mp4mux name=mux fragment-duration=500 \
             ! filesink name=file {audio_branch}"
        ))?;
        let video_src = by_name::<gst_app::AppSrc>(&pipeline, "v")?;
        video_src.set_caps(Some(video));
        let audio_src = match audio {
            Some(caps) => {
                let src = by_name::<gst_app::AppSrc>(&pipeline, "a")?;
                src.set_caps(Some(caps));
                Some(src)
            }
            None => None,
        };
        for src in std::iter::once(&video_src).chain(audio_src.as_ref()) {
            // Never block the encoder: the muxer waits for both streams to interleave them.
            src.set_max_bytes(0);
        }
        // filesink takes text: a path that is not valid UTF-8 would be written somewhere else.
        let location = path.to_str().ok_or_else(|| tr!("la ruta de grabación no es UTF-8 válido", "the recording path is not valid UTF-8").to_owned())?;
        by_name::<gst::Element>(&pipeline, "file")?.set_property("location", location);
        play(&pipeline)?;
        Ok(Self { pipeline, path: path.to_owned(), video: video_src, audio: audio_src, start, failed: Mutex::new(None) })
    }

    fn write(&self, video: bool, packet: &Packet) -> Result<(), String> {
        // A failed write (a full disk) stops the muxer, while the packets would keep queueing.
        let mut failed = self.failed.lock().unwrap();
        if let Some(e) = failed.as_ref().cloned().or_else(|| posted_error(&self.pipeline)) {
            return Err(failed.insert(e).clone());
        }
        drop(failed);
        let time = packet.time - self.start;
        let src = if video { &self.video } else { self.audio.as_ref().ok_or("no audio stream")? };
        if time < 0 {
            return Ok(());
        }
        let mut buffer = gst::Buffer::from_slice(packet.data.clone());
        {
            let buffer = buffer.get_mut().expect("new buffer");
            let at = gst::ClockTime::from_nseconds(time as u64 * NS_PER_TICK);
            buffer.set_pts(at);
            buffer.set_dts(at);
            buffer.set_duration(gst::ClockTime::from_nseconds(packet.duration.max(0) as u64 * NS_PER_TICK));
            if !packet.key {
                buffer.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
        }
        src.push_buffer(buffer).map(|_| ()).map_err(|e| e.to_string())
    }

    fn finish(self) -> Result<(), String> {
        if let Some(e) = self.failed.into_inner().unwrap() {
            let _ = self.pipeline.set_state(gst::State::Null);
            return Err(e);
        }
        let _ = self.video.end_of_stream();
        if let Some(audio) = &self.audio {
            let _ = audio.end_of_stream();
        }
        let bus = self.pipeline.bus().ok_or("no bus")?;
        let message = bus.timed_pop_filtered(gst::ClockTime::from_seconds(60), &[gst::MessageType::Eos, gst::MessageType::Error]);
        let _ = self.pipeline.set_state(gst::State::Null);
        match message.as_ref().map(|m| m.view()) {
            Some(gst::MessageView::Eos(_)) => {
                // If this fails the file stays fragmented, which still plays.
                let _ = crate::mp4::defragment(&self.path);
                Ok(())
            }
            Some(gst::MessageView::Error(e)) => Err(e.error().to_string()),
            _ => Err(tr!("el archivo MP4 no terminó de escribirse", "the MP4 file did not finish writing").to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_region_is_copied_and_what_is_missing_is_black() {
        // A 4×2 frame with 4-byte padding per row; each pixel's bytes are its index.
        let stride = 4 * 4 + 4;
        let mut data = vec![0xEE; stride * 2];
        for y in 0..2 {
            for x in 0..4 {
                data[y * stride + x * 4..y * stride + x * 4 + 4].fill((y * 4 + x) as u8 + 1);
            }
        }
        let frame = FrameRef { data: &data, size: (4, 2), stride };
        // A 3×3 canvas from (2, 1): one row and two columns of it are past the frame.
        let mut canvas = vec![0x55; 3 * 3 * 4];
        copy_region(&frame, (2, 1), &mut canvas, (3, 3));
        let pixel = |x: usize, y: usize| canvas[(y * 3 + x) * 4];
        assert_eq!([pixel(0, 0), pixel(1, 0), pixel(2, 0)], [7, 8, 0]);
        assert_eq!([pixel(0, 1), pixel(1, 1), pixel(2, 1)], [0, 0, 0]);
        assert_eq!([pixel(0, 2), pixel(1, 2), pixel(2, 2)], [0, 0, 0]);
    }

    #[test]
    fn a_picture_is_drawn_on_again_only_once_nothing_reads_it() {
        let mut pictures = Vec::new();
        // Each one drawn on is held (as the newest, then by the encoder) before the next.
        let mut held = Vec::new();
        for _ in 0..MAX_PICTURES {
            let i = free_picture(&mut pictures, 4).unwrap();
            held.push(pictures[i].clone());
        }
        assert_eq!(pictures.len(), MAX_PICTURES);
        assert_eq!(free_picture(&mut pictures, 4), None, "all in use: the frame is skipped");
        drop(held);
        assert_eq!(free_picture(&mut pictures, 4), Some(0), "drawn on again, not made anew");
        assert_eq!(pictures.len(), MAX_PICTURES);
    }

    #[test]
    fn the_webcam_is_pasted_in_place() {
        let mut picture = vec![0u8; 4 * 3 * 4];
        let webcam = vec![9u8; 2 * 2 * 4];
        paste(&mut picture, 4, (1, 1, 2, 2), &webcam);
        let painted: Vec<(usize, usize)> =
            (0..3).flat_map(|y| (0..4).map(move |x| (x, y))).filter(|&(x, y)| picture[(y * 4 + x) * 4] == 9).collect();
        assert_eq!(painted, [(1, 1), (2, 1), (1, 2), (2, 2)]);
    }

    #[test]
    fn packet_times_discount_the_encoders_offset() {
        gst::init().unwrap();
        // GStreamer video encoders shift timestamps and segment by 1000 hours.
        let offset = gst::ClockTime::from_seconds(1000 * 3600);
        let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
        segment.set_start(offset);
        let mut buffer = gst::Buffer::from_slice([1u8, 2, 3]);
        buffer.get_mut().unwrap().set_pts(offset + gst::ClockTime::from_mseconds(500));
        let sample = gst::Sample::builder().buffer(&buffer).segment(&segment).build();
        let packet = to_packet(&sample).unwrap();
        assert_eq!(packet.time, 5_000_000, "0.5 s in 100 ns units");
        assert_eq!(packet.data.as_ref(), &[1, 2, 3]);
        assert!(packet.key, "a buffer without DELTA_UNIT is a keyframe");
    }

    #[test]
    fn aac_codec_data_describes_lc_48k_stereo() {
        gst::init().unwrap();
        let caps = aac_caps();
        let s = caps.structure(0).unwrap();
        let data = s.get::<gst::Buffer>("codec_data").unwrap();
        let bytes = data.map_readable().unwrap();
        // object type 2 (LC, 5 bits) | frequency index 3 (48 kHz, 4 bits) | 2 channels (4 bits)
        let config = u16::from_be_bytes([bytes[0], bytes[1]]);
        assert_eq!(config >> 11, 2);
        assert_eq!((config >> 7) & 0xF, 3);
        assert_eq!((config >> 3) & 0xF, CHANNELS as u16);
    }

    #[test]
    fn every_installed_h264_encoder_takes_its_format() {
        gst::init().unwrap();
        for (factory, format, _) in h264_candidates(2_000_000, 30) {
            let Some(found) = gst::ElementFactory::find(factory) else { continue };
            let caps = gst::Caps::builder("video/x-raw").field("format", format).build();
            assert!(found.can_sink_any_caps(&caps), "{factory} does not take {format}");
        }
    }

    #[test]
    fn an_encoder_that_fails_at_its_first_picture_is_skipped() {
        gst::init().unwrap();
        // Core elements stand in for encoders: one rejects the pictures once they flow, one
        // takes them.
        let candidates = [
            ("capsfilter", "NV12", "capsfilter name=enc caps=video/x-raw,format=RGB".to_owned()),
            ("identity", "NV12", "identity name=enc".to_owned()),
        ];
        let (factory, _, _) = h264_encoder(candidates, |format, fragment| encodes(format, fragment, (64, 48))).unwrap();
        assert_eq!(factory, "identity");
    }

    /// `count` BGRA frames of `size` on the GPU with SMPTE colour bars, made by VA-API, and the
    /// pipeline that made them, to stop once they are used (GStreamer 1.24 drops its surfaces when
    /// it stops); `None` where VA-API cannot record them (CI). They are VA-API's own memory rather
    /// than DMA-BUFs, which it gives only to elements taking their video meta (appsink does not),
    /// but are converted the same way.
    fn gpu_frames(size: (u32, u32), count: usize) -> Option<(gst::Pipeline, Vec<gst::Sample>)> {
        super::super::gpu::formats()?;
        let pipeline = launch(&format!(
            "videotestsrc num-buffers={count} pattern=smpte ! video/x-raw,format=BGRA,width={},height={},framerate=30/1 \
             ! vapostproc ! video/x-raw(memory:VAMemory),format=BGRA ! appsink name=sink sync=false",
            size.0, size.1,
        ))
        .unwrap();
        // On the display the recording uses, as the desktop's frames are imported into it.
        super::super::gpu::share_display(&pipeline);
        let sink = by_name::<gst_app::AppSink>(&pipeline, "sink").unwrap();
        play(&pipeline).unwrap();
        let frames = std::iter::from_fn(|| sink.pull_sample().ok()).collect();
        assert_eq!(posted_error(&pipeline), None);
        Some((pipeline, frames))
    }

    /// Records 30 colour-bar frames of `size` on the GPU, the part `input` from `origin`, with a
    /// red webcam picture at `webcam` if given; the last picture of the file as RGBA, `None`
    /// where VA-API cannot record (CI).
    fn record_on_gpu(size: (u32, u32), origin: (u32, u32), input: (u32, u32), webcam: Option<PixelRect>) -> Option<(u32, u32, Vec<u8>)> {
        let (maker, frames) = gpu_frames(size, 30)?;
        assert_eq!(frames.len(), 30);
        let outputs = Arc::new(Mutex::new(Outputs::default()));
        // One file per recording: tests run at once.
        static RECORDINGS: AtomicU64 = AtomicU64::new(0);
        let n = RECORDINGS.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("obs-lite-gpu-{}-{n}.mp4", std::process::id()));
        outputs.lock().unwrap().start_file(path.clone());
        let settings = EncoderSettings {
            origin,
            input,
            output: input,
            fps: 30,
            bitrate: 2_000_000,
            audio: false,
            overlay: webcam.map(|_| (Corner::TopLeft, CameraSize::Small)),
            red_first: false,
            gpu: Some(frames[0].clone()),
        };
        let encoder = Encoder::new(settings, Arc::new(Clock::new()), Arc::default(), outputs).unwrap();
        assert!(encoder.on_gpu());
        // One picture, before the frames: it stays until the next, as a webcam's does.
        if let Some(rect) = webcam {
            let red: Vec<u8> = [0u8, 0, 255, 255].repeat((rect.2 * rect.3) as usize);
            encoder.push_overlay(rect, &red);
        }
        for frame in &frames {
            encoder.push_gpu_frame(frame);
            std::thread::sleep(Duration::from_millis(20));
        }
        let finishing = std::time::Instant::now();
        let finished = encoder.finish().expect("a recording").finish();
        assert!(finishing.elapsed() < Duration::from_secs(5), "every input of the pipeline ends");
        drop(frames);
        let _ = maker.set_state(gst::State::Null);
        finished.unwrap_or_else(|e| panic!("{size:?} {origin:?}: {e}"));
        let decoder = launch(&format!(
            "filesrc location={} ! decodebin ! videoconvert ! video/x-raw,format=RGBA ! appsink name=sink sync=false",
            path.display()
        ))
        .unwrap();
        let sink = by_name::<gst_app::AppSink>(&decoder, "sink").unwrap();
        play(&decoder).unwrap();
        let last = std::iter::from_fn(|| sink.pull_sample().ok()).last();
        let _ = decoder.set_state(gst::State::Null);
        let _ = std::fs::remove_file(&path);
        super::super::gpu::rgba(&last.expect("a decoded picture"))
    }

    /// The colour at (x, y) of an RGBA picture `width` wide.
    fn pixel((width, _, rgba): &(u32, u32, Vec<u8>), x: u32, y: u32) -> [u8; 3] {
        let i = ((y * width + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2]]
    }

    #[test]
    fn frames_on_the_gpu_are_recorded_and_cropped() {
        gst::init().unwrap();
        let Some(whole) = record_on_gpu((320, 240), (0, 0), (320, 240), None) else { return };
        assert_eq!((whole.0, whole.1), (320, 240));
        // An odd size is cropped to the even one recorded.
        let odd = record_on_gpu((321, 241), (0, 0), (320, 240), None).unwrap();
        assert_eq!((odd.0, odd.1), (320, 240));
        // A region: x 96 to 128 lies within the third bar, cyan.
        let region = record_on_gpu((320, 240), (96, 0), (32, 64), None).unwrap();
        assert_eq!((region.0, region.1), (32, 64));
        let [r, g, b] = pixel(&region, 16, 32);
        assert!(r < 60 && g > 130 && b > 130, "cyan expected, got {:?}", [r, g, b]);
    }

    #[test]
    fn the_webcam_is_put_on_frames_on_the_gpu() {
        gst::init().unwrap();
        let Some(picture) = record_on_gpu((320, 240), (0, 0), (320, 240), Some((16, 16, 64, 48))) else { return };
        let [r, g, b] = pixel(&picture, 48, 40);
        assert!(r > 150 && g < 80 && b < 80, "the webcam's red expected, got {:?}", [r, g, b]);
        // Beside it the first bar, grey, shows.
        let [r, g, b] = pixel(&picture, 30, 100);
        assert!(r > 150 && g > 150 && b > 150, "grey expected, got {:?}", [r, g, b]);
    }

    #[test]
    fn a_webcam_smaller_than_the_compositor_takes_is_still_recorded() {
        gst::init().unwrap();
        // A small webcam in a small region: 12×8 pixels.
        let Some(picture) = record_on_gpu((320, 240), (0, 0), (320, 240), Some((16, 16, 12, 8))) else { return };
        let [r, g, b] = pixel(&picture, 22, 20);
        assert!(r > 150 && g < 80 && b < 80, "the webcam's red expected, got {:?}", [r, g, b]);
    }

    #[test]
    fn a_first_frame_that_does_not_get_through_fails_at_once() {
        gst::init().unwrap();
        let frame = gst::Sample::builder()
            .buffer(&gst::Buffer::from_slice(vec![0u8; 64 * 48 * 4]))
            .caps(&gst::Caps::builder("video/x-raw").field("format", "BGRA").field("width", 64).field("height", 48).field("framerate", gst::Fraction::new(0, 1)).build())
            .build();
        let attempt = |rest: &str| {
            let pipeline = launch(&format!("appsrc name=frames is-live=true format=time {rest}")).unwrap();
            let frames = by_name::<gst_app::AppSrc>(&pipeline, "frames").unwrap();
            let ready = Arc::new(AtomicBool::new(false));
            if let Ok(sink) = by_name::<gst_app::AppSink>(&pipeline, "surfaces") {
                let ready = ready.clone();
                sink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |sink| {
                            let _ = sink.pull_sample();
                            ready.store(true, Ordering::Release);
                            Ok(gst::FlowSuccess::Ok)
                        })
                        .build(),
                );
            }
            play(&pipeline).unwrap();
            let start = std::time::Instant::now();
            let result = first_surface(&pipeline, &frames, &frame, &ready);
            let _ = pipeline.set_state(gst::State::Null);
            (result, start.elapsed())
        };
        let (result, _) = attempt("! appsink name=surfaces sync=false async=false");
        assert_eq!(result, Ok(()));
        // A conversion that refuses the frame, and one that never gives anything back.
        let (result, took) = attempt("! capsfilter caps=video/x-raw,format=RGB ! appsink name=surfaces sync=false async=false");
        assert!(result.is_err() && took < Duration::from_secs(1), "{result:?} after {took:?}");
        let (result, took) = attempt("! fakesink");
        assert!(result.is_err() && took < Duration::from_secs(4), "{result:?} after {took:?}");
    }

    #[test]
    fn the_recorded_part_is_cropped_from_what_the_frame_has() {
        assert_eq!(crop((1920, 1080), (0, 0), (1920, 1080)), Crop::Whole);
        assert_eq!(crop((1921, 1081), (0, 0), (1920, 1080)), Crop::Part((0, 0, 1920, 1080)));
        assert_eq!(crop((1920, 1080), (100, 50), (640, 360)), Crop::Part((100, 50, 640, 360)));
        // A window that shrank into the region, and one that shrank past it: nothing outside the
        // region is recorded.
        assert_eq!(crop((800, 600), (500, 400), (640, 360)), Crop::Part((500, 400, 300, 200)));
        assert_eq!(crop((800, 600), (1000, 0), (640, 360)), Crop::Nothing);
    }

    #[test]
    fn the_webcam_is_placed_at_the_recordings_size() {
        assert_eq!(scale_rect((1600, 900, 320, 180), (1920, 1080), (1280, 720)), (1066, 600, 213, 120));
        assert_eq!(scale_rect((10, 10, 20, 20), (100, 100), (100, 100)), (10, 10, 20, 20));
    }

    #[test]
    fn a_muxer_that_cannot_write_says_so_while_recording() {
        gst::init().unwrap();
        // Real H.264 from an installed encoder; where there is none (CI), there is nothing to test.
        let Ok((_, format, fragment)) = h264_encoder(h264_candidates(8_000_000, 30), |format, fragment| encodes(format, fragment, (640, 360)))
        else {
            return;
        };
        if ["mp4mux", "h264parse"].iter().any(|f| gst::ElementFactory::find(f).is_none()) {
            return;
        }
        let source = launch(&format!(
            "videotestsrc num-buffers=60 ! video/x-raw,width=640,height=360,framerate=30/1 ! videoconvert \
             ! {caps} ! {fragment} ! h264parse \
             ! video/x-h264,stream-format=avc,alignment=au ! appsink name=sink sync=false",
            caps = raw_caps(format, (640, 360)),
        ))
        .unwrap();
        let sink = by_name::<gst_app::AppSink>(&source, "sink").unwrap();
        play(&source).unwrap();
        let (mut packets, mut caps) = (Vec::new(), None);
        while let Ok(sample) = sink.pull_sample() {
            caps = sample.caps().map(|c| c.to_owned());
            packets.extend(to_packet(&sample));
        }
        let _ = source.set_state(gst::State::Null);
        // Every write to /dev/full fails as a full disk does (once filesink's 64 KB buffer fills).
        let mp4 = Mp4::create(Path::new("/dev/full"), &caps.unwrap(), None, packets[0].time).unwrap();
        for packet in &packets {
            let _ = mp4.write(true, packet);
        }
        std::thread::sleep(Duration::from_millis(500));
        assert!(mp4.write(true, &packets[0]).is_err(), "the failed write must reach the recording");
        let finishing = std::time::Instant::now();
        assert!(mp4.finish().is_err());
        assert!(finishing.elapsed() < Duration::from_secs(5), "a failed muxer does not wait for its end of stream");
    }
}
