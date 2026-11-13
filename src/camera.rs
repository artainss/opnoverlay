//! Webcam sources and the picture-in-picture overlay maths. Capturing from the device is
//! platform code (`platform::camera`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use serde::{Deserialize, Serialize};
use crate::i18n::tr;

pub use crate::platform::camera::list_devices;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraDevice {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    pub const ALL: [Self; 4] = [Self::TopLeft, Self::TopRight, Self::BottomLeft, Self::BottomRight];

    pub fn label(self) -> &'static str {
        match self {
            Self::TopLeft => tr!("Arriba a la izquierda", "Top left"),
            Self::TopRight => tr!("Arriba a la derecha", "Top right"),
            Self::BottomLeft => tr!("Abajo a la izquierda", "Bottom left"),
            Self::BottomRight => tr!("Abajo a la derecha", "Bottom right"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum CameraSize {
    Small,
    Medium,
    Large,
}

impl CameraSize {
    pub const ALL: [Self; 3] = [Self::Small, Self::Medium, Self::Large];

    pub fn label(self) -> &'static str {
        match self {
            Self::Small => tr!("Pequeña", "Small"),
            Self::Medium => tr!("Mediana", "Medium"),
            Self::Large => tr!("Grande", "Large"),
        }
    }

    /// Overlay width as a share of the picture width.
    fn fraction(self) -> f32 {
        match self {
            Self::Small => 0.2,
            Self::Medium => 0.3,
            Self::Large => 0.4,
        }
    }
}

/// The webcam source: which device, and where its overlay goes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraConfig {
    /// Platform device id (on Windows, the Media Foundation symbolic link).
    pub device: String,
    pub name: String,
    pub corner: Corner,
    pub size: CameraSize,
}

/// `(x, y, width, height)` in pixels.
pub type PixelRect = (u32, u32, u32, u32);

/// Overlay rectangle `(x, y, width, height)` for a `camera`-sized image on a `picture`.
/// Keeps the camera's aspect ratio and even sides, with a small margin from the edges.
pub fn overlay_rect(picture: (u32, u32), camera: (u32, u32), corner: Corner, size: CameraSize) -> PixelRect {
    let margin = (picture.0.min(picture.1) as f32 * 0.02) as u32;
    let mut w = (picture.0 as f32 * size.fraction()) as u32;
    let mut h = (w as u64 * camera.1 as u64 / camera.0.max(1) as u64) as u32;
    let max_h = picture.1.saturating_sub(2 * margin);
    if h > max_h {
        w = (w as u64 * max_h as u64 / h.max(1) as u64) as u32;
        h = max_h;
    }
    let (w, h) = ((w & !1).max(2), (h & !1).max(2));
    let right = picture.0.saturating_sub(w + margin);
    let bottom = picture.1.saturating_sub(h + margin);
    let (x, y) = match corner {
        Corner::TopLeft => (margin, margin),
        Corner::TopRight => (right, margin),
        Corner::BottomLeft => (margin, bottom),
        Corner::BottomRight => (right, bottom),
    };
    (x, y, w, h)
}

/// Bilinear resize of a packed 4-bytes-per-pixel image.
pub fn resize(src: &[u8], (sw, sh): (u32, u32), (dw, dh): (u32, u32)) -> Vec<u8> {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let mut out = Vec::with_capacity(dw * dh * 4);
    let fx = sw as f32 / dw as f32;
    let fy = sh as f32 / dh as f32;
    for y in 0..dh {
        let sy = ((y as f32 + 0.5) * fy - 0.5).clamp(0.0, (sh - 1) as f32);
        let (y0, ty) = (sy as usize, sy.fract());
        let y1 = (y0 + 1).min(sh - 1);
        for x in 0..dw {
            let sx = ((x as f32 + 0.5) * fx - 0.5).clamp(0.0, (sw - 1) as f32);
            let (x0, tx) = (sx as usize, sx.fract());
            let x1 = (x0 + 1).min(sw - 1);
            for c in 0..4 {
                let p = |x: usize, y: usize| src[(y * sw + x) * 4 + c] as f32;
                let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * tx;
                let bottom = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * tx;
                out.push((top + (bottom - top) * ty).round() as u8);
            }
        }
    }
    out
}

/// One camera picture, top-down BGRA.
pub struct CameraFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

type Sink = Box<dyn Fn(&CameraFrame) + Send>;

pub struct CameraShared {
    pub latest: Mutex<Option<Arc<CameraFrame>>>,
    pub seq: AtomicU64,
    pub error: Mutex<Option<String>>,
    /// Receives every frame while recording.
    pub sink: Mutex<Option<Sink>>,
    stop: AtomicBool,
}

pub struct Camera {
    pub shared: Arc<CameraShared>,
    thread: Option<JoinHandle<()>>,
}

impl Camera {
    pub fn start(device: String, repaint: Option<eframe::egui::Context>) -> Self {
        let shared = Arc::new(CameraShared {
            latest: Mutex::new(None),
            seq: AtomicU64::new(0),
            error: Mutex::new(None),
            sink: Mutex::new(None),
            stop: AtomicBool::new(false),
        });
        let thread = thread::Builder::new()
            .name("camera".into())
            .spawn({
                let shared = shared.clone();
                move || {
                    if let Err(e) = crate::platform::camera::capture(&device, &shared, repaint.as_ref()) {
                        *shared.error.lock().unwrap() = Some(e);
                    }
                }
            })
            .expect("spawn camera thread");
        Self { shared, thread: Some(thread) }
    }
}

impl CameraShared {
    /// Whether the capture thread should stop.
    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// A captured picture: to the recording (if any), then to the preview.
    pub(crate) fn deliver(&self, frame: CameraFrame, repaint: Option<&eframe::egui::Context>) {
        if let Some(sink) = self.sink.lock().unwrap().as_ref() {
            sink(&frame);
        }
        *self.latest.lock().unwrap() = Some(Arc::new(frame));
        self.seq.fetch_add(1, Ordering::Relaxed);
        if let Some(ctx) = repaint {
            crate::capture::repaint_for_picture(ctx);
        }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Which camera mode to use, of `modes` as (width, height, frames per second): one at least 25
/// fps if there is one (many webcams offer 720p uncompressed at only 10 fps, and compressed at
/// 30), then the largest up to 1280 wide, which is plenty for an overlay, then the fastest.
#[cfg(any(windows, test))]
pub(crate) fn best_mode(modes: impl IntoIterator<Item = (u32, u32, f64)>) -> Option<usize> {
    let rank = |(w, h, fps): (u32, u32, f64)| (fps >= 25.0, u64::from(w) * u64::from(h), (fps * 1000.0) as u64);
    let mut best: Option<(usize, (bool, u64, u64))> = None;
    for (i, mode) in modes.into_iter().enumerate() {
        if mode.0 <= 1280 && best.is_none_or(|(_, b)| rank(mode) > b) {
            best = Some((i, rank(mode)));
        }
    }
    best.map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_camera_mode_fast_enough_wins_over_a_slow_one() {
        // Uncompressed 720p at 10 fps listed before compressed 720p at 30.
        assert_eq!(best_mode([(1280, 720, 10.0), (1280, 720, 30.0), (640, 480, 30.0)]), Some(1));
        // Nothing fast enough: the largest.
        assert_eq!(best_mode([(640, 480, 15.0), (1280, 720, 10.0)]), Some(1));
        // Fast enough beats larger, and nothing wider than 1280.
        assert_eq!(best_mode([(1280, 720, 10.0), (640, 360, 30.0), (1920, 1080, 30.0)]), Some(1));
        assert_eq!(best_mode([(1920, 1080, 30.0)]), None);
    }

    #[test]
    fn overlay_sits_in_the_corner_with_the_camera_aspect() {
        let pic = (1920, 1080);
        assert_eq!(overlay_rect(pic, (640, 480), Corner::BottomRight, CameraSize::Medium), (1323, 627, 576, 432));
        assert_eq!(overlay_rect(pic, (1280, 720), Corner::TopLeft, CameraSize::Small), (21, 21, 384, 216));
        // A tall picture: the overlay is limited by the height.
        let (_, _, w, h) = overlay_rect((400, 200), (640, 480), Corner::TopLeft, CameraSize::Large);
        assert!(h <= 200 - 2 * 4 && w <= 160, "{w}×{h}");
    }

    #[test]
    fn resize_keeps_flat_colour_and_identity() {
        let flat = [10, 20, 30, 255].repeat(4 * 3);
        assert_eq!(resize(&flat, (4, 3), (7, 5)), [10, 20, 30, 255].repeat(7 * 5));
        let img: Vec<u8> = (0..16).collect();
        assert_eq!(resize(&img, (2, 2), (2, 2)), img);
    }
}
