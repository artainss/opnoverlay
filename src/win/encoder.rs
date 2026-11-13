//! Hardware H.264 encoder producing constant-frame-rate MP4.
//!
//! Windows Graphics Capture only delivers frames when the screen changes, so the capture side
//! (and the webcam) just draw into a GPU "canvas" texture. Like OBS's video thread, a producer
//! thread converts the canvas to NV12 at the output size every 1/fps of recording time and
//! queues it; the hardware encoder of the same GPU takes frames from the queue, and the
//! encoded packets are written to the MP4 (see `mf`). Pausing leaves the time out of the file
//! only (see `media::Outputs`), so the replay buffer goes on.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Query,
    ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::IMFMediaType;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::core::BOOL;
use windows_capture::frame::Frame;

use crate::camera::{self, CameraSize, Corner, PixelRect};
use crate::capture::{Placement, placement};
use crate::i18n::tr;
use crate::media::{self, Clock, Muxer, Packet, TICKS_PER_SECOND};
use super::mf::{Aac, Converter, H264, Media, Mp4, Scaler, VideoEvent};

/// Queued NV12 frames (200 ms at 60 fps, about 3 MB each at 1080p). If the encoder falls
/// further behind, the oldest queued frame is dropped and counted as lagged.
const FRAME_TEXTURES: usize = 12;
const AUDIO_QUEUE_CAPACITY: usize = 500;
/// Frames kept out of reuse after the encoder takes them: it reads them on another GPU engine
/// and does not report when it is done.
const IN_FLIGHT: usize = 4;

pub struct EncoderSettings {
    pub input: (u32, u32),
    pub output: (u32, u32),
    pub fps: u32,
    pub bitrate: u32,
    pub audio: bool,
    /// Where the webcam goes, if there is one.
    pub overlay: Option<(Corner, CameraSize)>,
}

/// A scaler and the frame size it was made for; no scaler if the GPU cannot scale.
type SizedScaler = ((u32, u32), Option<Com<Scaler>>);

/// COM pointer used from several threads; D3D11 devices are free-threaded and the
/// immediate context is multithread-protected (see `capture::keep_last_frame`).
#[derive(Clone)]
pub struct Com<T>(pub T);
unsafe impl<T> Send for Com<T> {}
unsafe impl<T> Sync for Com<T> {}

pub type Outputs = media::Outputs<Com<Mp4>>;
pub type PendingFile = media::PendingFile<Com<Mp4>>;

/// The MP4 writer of Media Foundation, for the shared outputs (`media`).
impl Muxer for Com<Mp4> {
    type Format = Com<IMFMediaType>;

    fn create(path: &Path, video: &Self::Format, audio: Option<&Self::Format>, start: i64) -> Result<Self, String> {
        // Replays are saved from a thread of their own.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Mp4::create(path, &video.0, audio.map(|a| &a.0), start).map(Com).map_err(|e| e.to_string())
    }

    fn write(&self, video: bool, packet: &Packet) -> Result<(), String> {
        self.0.write(video, packet).map_err(|e| e.to_string())
    }

    fn finish(self) -> Result<(), String> {
        self.0.finish().map_err(|e| e.to_string())
    }
}

/// The picture being recorded: capture plus webcam, at the capture (or region) size.
struct Canvas {
    texture: Com<ID3D11Texture2D>,
    target: Com<ID3D11RenderTargetView>,
    context: Com<ID3D11DeviceContext>,
    converter: Com<Converter>,
    /// Signals when the GPU has finished a conversion.
    converted: Com<ID3D11Query>,
    /// Set once the first frame has been drawn.
    ready: AtomicBool,
    /// Keeps each group of GPU commands (a frame with its overlay, or a conversion) together.
    lock: Mutex<()>,
}

impl Canvas {
    /// Converts the canvas into NV12 frame `slot` and waits until the GPU has done it: the
    /// hardware encoder reads the frame on another GPU engine, without waiting for this
    /// context, so handing it over earlier showed up as half-drawn frames. A GPU that cannot
    /// convert fails the recording: its frames would be empty (green).
    fn snapshot(&self, slot: usize) -> windows::core::Result<()> {
        let ctx = &self.context.0;
        unsafe {
            {
                let _gpu = self.lock.lock().unwrap();
                self.converter.0.convert(slot)?;
                ctx.End(&self.converted.0);
                ctx.Flush();
            }
            for _ in 0..50 {
                let mut done = BOOL(0);
                let data = Some((&raw mut done).cast());
                if ctx.GetData(&self.converted.0, data, size_of::<BOOL>() as u32, 0).is_ok() && done.as_bool() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }
}

struct Inner {
    canvas: Canvas,
    clock: Arc<Clock>,
    /// Set when the encoder is running; the clock starts then.
    started: Arc<AtomicBool>,
    /// Frames handed to the encoder.
    encoded: AtomicU64,
    /// Frame slots skipped because the machine or the encoder fell behind real time.
    lagged: AtomicU64,
    stop: AtomicBool,
    fps: u64,
    queue: Mutex<FrameQueue>,
    queued: Condvar,
    output: Arc<Mutex<Outputs>>,
}

pub struct Encoder {
    inner: Arc<Inner>,
    audio_tx: Option<SyncSender<Vec<u8>>>,
    threads: Vec<JoinHandle<()>>,
    input: (u32, u32),
    device: Com<ID3D11Device>,
    overlay: Option<(Corner, CameraSize)>,
    /// The newest webcam picture, scaled, and where it goes: (x, y, width, height).
    overlay_texture: Mutex<Option<(Com<ID3D11Texture2D>, PixelRect)>>,
    /// For frames of another size than the recording (a resized window).
    scaler: Mutex<Option<SizedScaler>>,
    /// Name of the H.264 encoder in use, for diagnostics.
    encoder_name: String,
    /// Dropped last, after everything that uses Media Foundation.
    _media: Media,
}

impl Encoder {
    /// Creates the encoder and starts `clock` (setting `started`); from then on the clock
    /// drives both video and audio.
    pub fn new(
        device: &ID3D11Device,
        s: EncoderSettings,
        clock: Arc<Clock>,
        started: Arc<AtomicBool>,
        outputs: Arc<Mutex<Outputs>>,
    ) -> Result<Self, String> {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let media = Media::start().map_err(|e| e.to_string())?;
        Self::create(device, s, clock, started, outputs, media)
    }

    fn create(
        device: &ID3D11Device,
        s: EncoderSettings,
        clock: Arc<Clock>,
        started: Arc<AtomicBool>,
        outputs: Arc<Mutex<Outputs>>,
        media: Media,
    ) -> Result<Self, String> {
        let fps = s.fps.max(1);
        let h264 = H264::new(device, s.output, fps, s.bitrate)
            .map_err(|e| tr!("no se pudo iniciar el codificador H.264: {}", "could not start the H.264 encoder: {}", e))?;
        let encoder_name = h264.name.clone();
        let aac = if s.audio { Some(Aac::new().map_err(|e| e.to_string())?) } else { None };
        let audio_format = match &aac {
            Some(aac) => Some(Com(aac.output_type().map_err(|e| e.to_string())?)),
            None => None,
        };
        outputs.lock().unwrap().set_audio_format(audio_format);
        let canvas = create_canvas(device, s.input, s.output).map_err(|e| e.to_string())?;
        let inner = Arc::new(Inner {
            canvas,
            clock,
            started,
            encoded: AtomicU64::new(0),
            lagged: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            fps: fps as u64,
            queue: Mutex::new(FrameQueue::default()),
            queued: Condvar::new(),
            output: outputs,
        });

        let spawn = |name: &str, work: Box<dyn FnOnce() + Send>| {
            thread::Builder::new().name(name.into()).spawn(work).map_err(|e| e.to_string())
        };
        let mut threads = Vec::new();
        let h264 = Com(h264);
        threads.push(spawn("video-encoder", Box::new({
            let inner = inner.clone();
            move || encode_video(&inner, h264)
        }))?);
        threads.push(spawn("video-frames", Box::new({
            let inner = inner.clone();
            move || produce_frames(&inner)
        }))?);
        let (audio_tx, audio_rx) = mpsc::sync_channel(AUDIO_QUEUE_CAPACITY);
        if let Some(aac) = aac {
            let aac = Com(aac);
            threads.push(spawn("audio-encoder", Box::new({
                let inner = inner.clone();
                move || encode_audio(&inner, aac, audio_rx)
            }))?);
        }

        Ok(Self {
            inner,
            audio_tx: s.audio.then_some(audio_tx),
            threads,
            input: s.input,
            device: Com(device.clone()),
            overlay: s.overlay,
            overlay_texture: Mutex::new(None),
            scaler: Mutex::new(None),
            encoder_name,
            _media: media,
        })
    }

    /// Draws a captured frame onto the canvas, with the webcam on top (capture thread only):
    /// the part of it starting at `region`'s origin, or all of it, scaled to fit if the source
    /// changed size (a resized window).
    pub fn push_frame(&self, frame: &Frame, region: Option<(u32, u32)>) {
        let canvas = &self.inner.canvas;
        let (ctx, dst, source) = (&canvas.context.0, &canvas.texture.0, frame.as_raw_texture());
        let (w, h) = (frame.width(), frame.height());
        let _gpu = canvas.lock.lock().unwrap();
        unsafe {
            let placement = placement((w, h), self.input, region);
            if placement == Placement::Copy {
                ctx.CopyResource(dst, source);
            } else if placement == Placement::Scale && self.draw_scaled(source, (w, h)) {
            } else {
                // A region, an odd-sized frame, or a resized window the GPU cannot scale: crop,
                // padding with black what is missing.
                let (left, top) = if let Placement::Crop(origin) = placement { origin } else { (0, 0) };
                let (right, bottom) = ((left + self.input.0).min(w), (top + self.input.1).min(h));
                if (right.saturating_sub(left), bottom.saturating_sub(top)) != self.input {
                    ctx.ClearRenderTargetView(&canvas.target.0, &[0.0, 0.0, 0.0, 1.0]);
                }
                if right > left && bottom > top {
                    let region = D3D11_BOX { left, top, front: 0, right, bottom, back: 1 };
                    ctx.CopySubresourceRegion(dst, 0, 0, 0, 0, source, 0, Some(&region));
                }
            }
            if let Some((texture, (x, y, _, _))) = self.overlay_texture.lock().unwrap().as_ref() {
                ctx.CopySubresourceRegion(dst, 0, *x, *y, 0, &texture.0, 0, None);
            }
            ctx.Flush();
        }
        canvas.ready.store(true, Ordering::Release);
    }

    /// Draws a `size`-sized frame scaled to fit the canvas; false if the GPU cannot. The scaler
    /// is made again only when the size changes.
    fn draw_scaled(&self, source: &ID3D11Texture2D, size: (u32, u32)) -> bool {
        let mut scaler = self.scaler.lock().unwrap();
        if scaler.as_ref().is_none_or(|(made_for, _)| *made_for != size) {
            let canvas = &self.inner.canvas.texture.0;
            let made = Scaler::new(&self.device.0, canvas, size, self.input, fit(size, self.input)).ok().map(Com);
            *scaler = Some((size, made));
        }
        let Some((_, Some(scaler))) = scaler.as_ref() else { return false };
        scaler.0.draw(&self.inner.canvas.context.0, source).is_ok()
    }

    /// Where a `camera`-sized webcam picture goes on the recording, if there is an overlay.
    pub fn overlay_rect(&self, camera: (u32, u32)) -> Option<PixelRect> {
        self.overlay.map(|(corner, size)| camera::overlay_rect(self.input, camera, corner, size))
    }

    /// Takes a webcam picture already scaled to `rect` (see `overlay_rect`) and draws it onto
    /// the canvas right away, so the camera keeps moving while the screen does not.
    pub fn push_overlay(&self, rect: PixelRect, pixels: &[u8]) {
        let (x, y, w, h) = rect;
        let canvas = &self.inner.canvas;
        let ctx = &canvas.context.0;
        let mut overlay = self.overlay_texture.lock().unwrap();
        let _gpu = canvas.lock.lock().unwrap();
        unsafe {
            if overlay.as_ref().is_none_or(|(_, r)| *r != rect) {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    ..Default::default()
                };
                let mut texture = None;
                if self.device.0.CreateTexture2D(&desc, None, Some(&mut texture)).is_err() {
                    return;
                }
                *overlay = Some((Com(texture.expect("CreateTexture2D returned no texture")), rect));
            }
            let texture = &overlay.as_ref().expect("set above").0.0;
            ctx.UpdateSubresource(texture, 0, None, pixels.as_ptr().cast(), w * 4, 0);
            if canvas.ready.load(Ordering::Acquire) {
                ctx.CopySubresourceRegion(&canvas.texture.0, 0, x, y, 0, texture, 0, None);
            }
            ctx.Flush();
        }
    }

    pub fn name(&self) -> &str {
        &self.encoder_name
    }

    pub fn push_audio(&self, pcm: Vec<u8>) {
        if let Some(tx) = &self.audio_tx {
            queue_audio(tx, &self.inner.output, pcm);
        }
    }

    /// (frames encoded, frame slots skipped because the machine fell behind).
    pub fn stats(&self) -> (u64, u64) {
        (self.inner.encoded.load(Ordering::Relaxed), self.inner.lagged.load(Ordering::Relaxed))
    }

    /// Ends both streams; what was being recorded still has to be finished.
    pub fn finish(mut self) -> Option<PendingFile> {
        self.stop_threads();
        self.inner.output.lock().unwrap().take_file()
    }

    fn stop_threads(&mut self) {
        self.audio_tx = None;
        self.inner.stop.store(true, Ordering::Release);
        self.inner.queued.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.threads.is_empty() {
            self.stop_threads();
            if let Some(file) = self.inner.output.lock().unwrap().take_file() {
                let _ = file.finish();
            }
        }
    }
}

/// Where a `picture`-sized frame goes on a `canvas`-sized one: scaled down to fit, never up
/// (so text stays sharp), and centred.
fn fit((w, h): (u32, u32), (cw, ch): (u32, u32)) -> RECT {
    let (w, h, cw, ch) = (u64::from(w.max(1)), u64::from(h.max(1)), u64::from(cw), u64::from(ch));
    let (fw, fh) = if w <= cw && h <= ch {
        (w, h)
    } else if cw * h <= ch * w {
        (cw, h * cw / w)
    } else {
        (w * ch / h, ch)
    };
    let (left, top) = ((cw - fw) / 2, (ch - fh) / 2);
    RECT { left: left as i32, top: top as i32, right: (left + fw) as i32, bottom: (top + fh) as i32 }
}

fn create_canvas(device: &ID3D11Device, (w, h): (u32, u32), output: (u32, u32)) -> windows::core::Result<Canvas> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture = None;
    let mut target = None;
    let mut converted = None;
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut texture))?;
        let texture = texture.as_ref().expect("CreateTexture2D returned no texture");
        device.CreateRenderTargetView(texture, None, Some(&mut target))?;
        device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut converted))?;
    }
    let texture = texture.expect("CreateTexture2D returned no texture");
    let converter = Converter::new(device, &texture, (w, h), output, FRAME_TEXTURES)?;
    Ok(Canvas {
        texture: Com(texture),
        target: Com(target.expect("CreateRenderTargetView returned no view")),
        context: Com(unsafe { device.GetImmediateContext()? }),
        converter: Com(converter),
        converted: Com(converted.expect("CreateQuery returned no query")),
        ready: AtomicBool::new(false),
        lock: Mutex::new(()),
    })
}

/// The video thread: every 1/fps of recording time, converts the canvas into the queue.
fn produce_frames(inner: &Inner) {
    let fps = inner.fps;
    let mut n = 0u64;
    while !inner.stop.load(Ordering::Acquire) {
        let now = inner.clock.elapsed();
        let due = Duration::from_nanos(n * 1_000_000_000 / fps);
        if !inner.started.load(Ordering::Acquire) || !inner.canvas.ready.load(Ordering::Acquire) || now < due {
            thread::sleep(due.saturating_sub(now).clamp(Duration::from_millis(1), Duration::from_millis(5)));
            continue;
        }
        // Descheduled for more than a frame: skip ahead rather than drift from real time.
        let current = (now.as_nanos() * fps as u128 / 1_000_000_000) as u64;
        if current > n + 1 {
            inner.lagged.fetch_add(current - n, Ordering::Relaxed);
            n = current;
        }
        let (slot, dropped) = inner.queue.lock().unwrap().free_slot();
        inner.lagged.fetch_add(dropped, Ordering::Relaxed);
        if let Err(e) = inner.canvas.snapshot(slot) {
            let what = tr!("la GPU no pudo convertir la imagen", "the GPU could not convert the picture");
            inner.output.lock().unwrap().encoder_error(format!("{what}: {e}"));
            break;
        }
        inner.queue.lock().unwrap().frames.push_back((n, slot));
        inner.queued.notify_one();
        n += 1;
    }
    inner.queued.notify_all();
}

/// Frames waiting for the encoder, and which NV12 slot each one is in.
#[derive(Default)]
struct FrameQueue {
    /// (frame number, NV12 slot), oldest first.
    frames: VecDeque<(u64, usize)>,
    /// Slots of the last `IN_FLIGHT` frames the encoder took: it may still be reading them.
    taken: VecDeque<usize>,
    next: usize,
}

impl FrameQueue {
    /// A slot no queued or in-flight frame uses, dropping the oldest queued frames if the encoder
    /// is that far behind; also how many were dropped. At most `FRAME_TEXTURES - IN_FLIGHT - 1`
    /// frames stay queued, so with the in-flight ones a slot is always free.
    fn free_slot(&mut self) -> (usize, u64) {
        let mut dropped = 0;
        while self.frames.len() >= FRAME_TEXTURES - IN_FLIGHT {
            self.frames.pop_front();
            dropped += 1;
        }
        loop {
            let slot = self.next;
            self.next = (slot + 1) % FRAME_TEXTURES;
            if !self.taken.contains(&slot) && !self.frames.iter().any(|&(_, s)| s == slot) {
                return (slot, dropped);
            }
        }
    }

    /// The oldest queued frame, for the encoder.
    fn take(&mut self) -> Option<(u64, usize)> {
        let frame = self.frames.pop_front()?;
        self.taken.push_back(frame.1);
        if self.taken.len() > IN_FLIGHT {
            self.taken.pop_front();
        }
        Some(frame)
    }
}

/// The next queued frame; once stopped and empty, `None`.
fn next_frame(inner: &Inner) -> Option<(u64, usize)> {
    let mut queue = inner.queue.lock().unwrap();
    loop {
        if let Some(frame) = queue.take() {
            return Some(frame);
        }
        if inner.stop.load(Ordering::Acquire) {
            return None;
        }
        queue = inner.queued.wait_timeout(queue, Duration::from_millis(20)).unwrap().0;
    }
}

/// Feeds queued frames to the hardware encoder and passes its packets on.
fn encode_video(inner: &Inner, h264: Com<H264>) {
    let h264 = h264.0;
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // Time starts now, so encoder start-up is not counted as lag.
    inner.clock.restart();
    inner.started.store(true, Ordering::Release);
    let fps = inner.fps as i64;
    let mut draining = false;
    loop {
        let event = match h264.next_event() {
            Ok(event) => event,
            Err(e) => {
                inner.output.lock().unwrap().encoder_error(e.to_string());
                break;
            }
        };
        match event {
            VideoEvent::NeedInput if !draining => match next_frame(inner) {
                Some((n, slot)) => {
                    if inner.output.lock().unwrap().take_keyframe_request() {
                        h264.force_keyframe();
                    }
                    let texture = inner.canvas.converter.0.texture(slot);
                    // The encoder would not ask again: stop, keeping what was encoded.
                    if let Err(e) = h264.push(texture, n as i64 * TICKS_PER_SECOND / fps, TICKS_PER_SECOND / fps) {
                        inner.output.lock().unwrap().encoder_error(e.to_string());
                        break;
                    }
                    inner.encoded.fetch_add(1, Ordering::Relaxed);
                }
                None => {
                    draining = true;
                    if h264.drain().is_err() {
                        break;
                    }
                }
            },
            VideoEvent::HaveOutput => match h264.pull() {
                Ok(Some(packet)) => inner.output.lock().unwrap().video(packet, || h264.output_type().map(Com).map_err(|e| e.to_string())),
                Ok(None) => {}
                // Asking again would fail the same way, forever on the software encoder.
                Err(e) => {
                    inner.output.lock().unwrap().encoder_error(e.to_string());
                    break;
                }
            },
            VideoEvent::Drained => break,
            _ => {}
        }
    }
}

/// Encodes the mixer's PCM to AAC until the encoder is finished.
fn queue_audio(tx: &SyncSender<Vec<u8>>, outputs: &Mutex<Outputs>, pcm: Vec<u8>) {
    if let Err(error) = tx.try_send(pcm) {
        let reason = match error {
            mpsc::TrySendError::Full(_) => "Кодировщик звука не успевает обрабатывать данные.",
            mpsc::TrySendError::Disconnected(_) => "Кодировщик звука остановился.",
        };
        outputs.lock().unwrap().encoder_error(reason.into());
    }
}

fn encode_audio(inner: &Inner, aac: Com<Aac>, rx: Receiver<Vec<u8>>) {
    let mut aac = aac.0;
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    for pcm in rx {
        match aac.encode(&pcm) {
            Ok(packets) => inner.output.lock().unwrap().audio(packets),
            Err(e) => {
                inner.output.lock().unwrap().encoder_error(format!("Ошибка кодирования звука: {e}"));
                break;
            }
        }
    }
    if let Ok(packets) = aac.drain() {
        inner.output.lock().unwrap().audio(packets);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_queue_failure_is_reported_for_recording_and_replay() {
        let outputs = Mutex::new(Outputs::default());
        {
            let mut outputs = outputs.lock().unwrap();
            outputs.start_file("recording.mp4".into());
            outputs.set_replay(Some(45));
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        queue_audio(&sender, &outputs, vec![1]);
        assert!(!outputs.lock().unwrap().failed());
        queue_audio(&sender, &outputs, vec![2]);
        let outputs = outputs.lock().unwrap();
        assert!(outputs.failed());
        assert!(outputs.replay_error().is_some());
        assert_eq!(receiver.try_recv().unwrap(), vec![1]);
    }

    fn rect(r: RECT) -> (i32, i32, i32, i32) {
        (r.left, r.top, r.right, r.bottom)
    }

    #[test]
    fn a_slot_is_never_reused_while_queued_or_in_flight() {
        // The encoder falls behind for 20 frames (the queue fills and drops), then catches up.
        let mut queue = FrameQueue::default();
        let mut taken: Vec<usize> = Vec::new();
        for n in 0..2000u64 {
            let (slot, _) = queue.free_slot();
            assert!(!queue.frames.iter().any(|&(_, s)| s == slot), "frame {n}: slot {slot} is queued");
            let in_flight = &taken[taken.len().saturating_sub(IN_FLIGHT)..];
            assert!(!in_flight.contains(&slot), "frame {n}: slot {slot} is still being read");
            queue.frames.push_back((n, slot));
            for _ in 0..if n / 20 % 2 == 0 { 0 } else { 1 + n % 2 } {
                if let Some((_, s)) = queue.take() {
                    taken.push(s);
                }
            }
        }
    }

    #[test]
    fn a_resized_window_is_scaled_down_to_fit_and_centred() {
        let canvas = (1280, 720);
        assert_eq!(rect(fit((1280, 720), canvas)), (0, 0, 1280, 720), "same size");
        assert_eq!(rect(fit((2560, 1440), canvas)), (0, 0, 1280, 720), "larger, same shape");
        assert_eq!(rect(fit((2560, 720), canvas)), (0, 180, 1280, 540), "wider: bands above and below");
        assert_eq!(rect(fit((1280, 1440), canvas)), (320, 0, 960, 720), "taller: bands at the sides");
        assert_eq!(rect(fit((640, 360), canvas)), (320, 180, 960, 540), "smaller: centred, not enlarged");
        assert_eq!(rect(fit((1600, 500), canvas)), (0, 160, 1280, 560), "wider than the canvas only: 1280 × 400");
    }
}
