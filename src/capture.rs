//! Capture sources, recording settings and picture helpers shared by every platform. The
//! capture session itself is platform code (`platform::capture`).

use crate::camera::{CameraSize, Corner};
use crate::config::{Quality, Region, Resolution, ScreenshotFormat};

pub use crate::platform::capture::{Session, list_audio_apps, list_sources};
#[cfg(target_os = "linux")]
pub use crate::platform::capture::forget_choice;

/// Asks for one repaint, for a new picture to show. egui answers an immediate request with two
/// repaints (to let layout settle), which a picture does not need; pictures arrive several times
/// a second, so the second one would be most of the idle drawing.
pub fn repaint_for_picture(ctx: &eframe::egui::Context) {
    ctx.request_repaint_after(std::time::Duration::from_millis(1));
}

/// Preview width until the UI reports how wide the preview is drawn.
pub(crate) const PREVIEW_DEFAULT_WIDTH: usize = 640;

// Windows lists displays and windows itself; on Linux the portal picks them.
#[cfg_attr(target_os = "linux", allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceId {
    Monitor(isize),
    Window(isize),
    /// Chosen by the user in the ScreenCast portal's dialog.
    #[cfg(target_os = "linux")]
    Portal,
}

#[derive(Clone, Debug)]
pub struct Source {
    pub id: SourceId,
    pub label: String,
}

/// An application whose audio can be captured on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioApp {
    pub pid: u32,
    /// Executable name, e.g. "Spotify.exe": what the settings remember.
    pub exe: String,
    pub title: String,
}

#[derive(Clone)]
pub struct RecordConfig {
    pub quality: Quality,
    pub custom_bitrate_mbps: u32,
    pub resolution: Resolution,
    pub audio: bool,
    pub region: Option<Region>,
    pub overlay: Option<(Corner, CameraSize)>,
}

impl RecordConfig {
    pub fn bitrate(&self, width: u32, height: u32, fps: u32) -> u32 {
        self.quality.selected_bitrate(self.custom_bitrate_mbps, width, height, fps)
    }
}

pub struct PreviewFrame {
    pub size: [usize; 2],
    pub rgba: Vec<u8>,
    pub seq: u64,
}

pub struct SessionOptions {
    pub source: SourceId,
    pub fps: u32,
    pub cursor: bool,
    /// Preview updates per second; 0 disables the preview.
    pub preview_fps: u32,
    pub record: Option<RecordConfig>,
}

/// Area-average downscale of BGRA rows (`pitch` bytes apart) to `size` opaque RGBA. Every
/// source pixel counts, so text stays legible; picking one pixel per block did not. This runs
/// for every preview picture, so the rows of each output row are first added up as plain
/// numbers (which the compiler vectorizes), and only then grouped into output columns.
pub(crate) fn downscale(bgra: &[u8], (w, h): (usize, usize), pitch: usize, size: (usize, usize)) -> Vec<u8> {
    let (pw, ph) = size;
    // Source column x belongs to output column x * pw / w; `starts[c]..starts[c + 1]` are
    // the source columns of output column c (empty when the preview is wider than the source).
    let starts: Vec<usize> = (0..=pw).map(|c| (c * w).div_ceil(pw)).collect();
    let mut rows = vec![0u32; w * 4];
    let mut rgba = Vec::with_capacity(pw * ph * 4);
    for y in 0..ph {
        let (top, bottom) = (y * h / ph, ((y + 1) * h / ph).max(y * h / ph + 1));
        rows.fill(0);
        for sy in top..bottom {
            for (sum, &v) in rows.iter_mut().zip(&bgra[sy * pitch..sy * pitch + w * 4]) {
                *sum += v as u32;
            }
        }
        for span in starts.windows(2) {
            let mut bgr = [0u32; 3];
            for px in rows[span[0] * 4..span[1] * 4].as_chunks::<4>().0 {
                bgr = [bgr[0] + px[0], bgr[1] + px[1], bgr[2] + px[2]];
            }
            let n = (((bottom - top) * (span[1] - span[0])) as u32).max(1);
            rgba.extend_from_slice(&[(bgr[2] / n) as u8, (bgr[1] / n) as u8, (bgr[0] / n) as u8, 255]);
        }
    }
    rgba
}

/// Converts BGRA rows (`pitch` bytes apart) into tightly packed opaque RGBA.
pub(crate) fn bgra_to_rgba(bgra: &[u8], width: usize, height: usize, pitch: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in bgra.chunks(pitch).take(height) {
        for px in row[..width * 4].as_chunks::<4>().0 {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
    }
    rgba
}

/// Cuts `region` out of a packed RGBA image that is `width` pixels wide.
pub fn crop_rgba(rgba: &[u8], width: u32, region: Region) -> Vec<u8> {
    let (x, w) = (region.x as usize * 4, region.width as usize * 4);
    let rows = rgba.chunks(width as usize * 4).skip(region.y as usize).take(region.height as usize);
    rows.flat_map(|row| &row[x..x + w]).copied().collect()
}

/// How a captured frame goes onto a recording `input` pixels in size.
#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// The frame is the recording's size.
    Copy,
    /// Copy what fits from this corner of the frame, black where it does not cover.
    Crop((u32, u32)),
    /// A window resized while recording: scale it to fit.
    Scale,
}

/// Where a `frame`-sized picture goes: `region` is the top-left corner of the recorded region,
/// if there is one. A recording is even-sized, so an odd frame loses its last column or row.
#[cfg(any(windows, test))]
pub(crate) fn placement(frame: (u32, u32), input: (u32, u32), region: Option<(u32, u32)>) -> Placement {
    match region {
        Some((0, 0)) | None if frame == input => Placement::Copy,
        Some(origin) => Placement::Crop(origin),
        None if (frame.0 & !1, frame.1 & !1) == input => Placement::Crop((0, 0)),
        None => Placement::Scale,
    }
}

/// Writes an RGBA image as PNG.
pub fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    encode_png(path, width, height, rgba)?.persist_noclobber(path).map_err(|e| e.error.to_string())?;
    Ok(())
}

fn encode_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<tempfile::NamedTempFile, String> {
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    {
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(temp.as_file()), width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(temp)
}

#[cfg(test)]
pub fn write_jpeg(path: &std::path::Path, width: u32, height: u32, rgba: &[u8], quality: u8) -> Result<(), String> {
    encode_jpeg(path, width, height, rgba, quality)?.persist_noclobber(path).map_err(|e| e.error.to_string())?;
    Ok(())
}

fn encode_jpeg(path: &std::path::Path, width: u32, height: u32, rgba: &[u8], quality: u8) -> Result<tempfile::NamedTempFile, String> {
    let expected = (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4))
        .ok_or_else(|| "Размер изображения слишком велик".to_owned())?;
    if width == 0 || height == 0 || rgba.len() != expected {
        return Err("Размер изображения не соответствует данным".into());
    }
    let rgb: Vec<u8> = rgba.chunks_exact(4).flat_map(|pixel| [pixel[0], pixel[1], pixel[2]]).collect();
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    {
        let mut writer = std::io::BufWriter::new(temp.as_file());
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, quality.clamp(1, 100));
        encoder.encode(&rgb, width, height, image::ExtendedColorType::Rgb8).map_err(|e| e.to_string())?;
        std::io::Write::flush(&mut writer).map_err(|e| e.to_string())?;
    }
    Ok(temp)
}

pub fn write_screenshot_unique(path: &std::path::Path, format: ScreenshotFormat, width: u32, height: u32, rgba: &[u8], quality: u8) -> Result<std::path::PathBuf, String> {
    let mut temp = match format {
        ScreenshotFormat::Png => encode_png(path, width, height, rgba)?,
        ScreenshotFormat::Jpeg => encode_jpeg(path, width, height, rgba, quality)?,
    };
    let stem = path.file_stem().ok_or("Некорректное имя скриншота")?;
    let extension = path.extension().ok_or("Некорректное расширение скриншота")?;
    for index in 1..=10_000 {
        let candidate = if index == 1 { path.to_owned() } else {
            let mut name = stem.to_os_string();
            name.push(format!(" ({index})."));
            name.push(extension);
            path.with_file_name(name)
        };
        match temp.persist_noclobber(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => temp = error.file,
            Err(error) => return Err(error.error.to_string()),
        }
    }
    Err("Не удалось подобрать свободное имя скриншота".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshots_do_not_overwrite_existing_files() {
        let folder = tempfile::tempdir().unwrap();
        let pixels = [255, 0, 0, 255].repeat(4);
        for (name, format) in [("image.png", ScreenshotFormat::Png), ("image.jpg", ScreenshotFormat::Jpeg)] {
            let path = folder.path().join(name);
            std::fs::write(&path, b"existing data").unwrap();
            let result = match format {
                ScreenshotFormat::Png => write_png(&path, 2, 2, &pixels),
                ScreenshotFormat::Jpeg => write_jpeg(&path, 2, 2, &pixels, 85),
            };
            assert!(result.is_err());
            assert_eq!(std::fs::read(path).unwrap(), b"existing data");
        }
    }

    #[test]
    fn concurrent_screenshots_do_not_overwrite_each_other() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("simultaneous.png");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = [0u8, 255].into_iter().map(|value| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let pixels = [value, 0, 0, 255].repeat(64 * 64);
                barrier.wait();
                write_png(&path, 64, 64, &pixels)
            })
        }).collect();
        let results: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut pixels).unwrap();
        assert_eq!((frame.width, frame.height), (64, 64));
    }

    #[test]
    fn simultaneous_screenshots_choose_distinct_names() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("image.png");
        std::fs::write(&path, b"keep").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = [0u8, 255].into_iter().map(|value| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let pixels = [value, 0, 0, 255].repeat(64 * 64);
                barrier.wait();
                write_screenshot_unique(&path, ScreenshotFormat::Png, 64, 64, &pixels, 85).unwrap()
            })
        }).collect();
        let paths: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
        assert_ne!(paths[0], paths[1]);
        assert_eq!(std::fs::read(path).unwrap(), b"keep");
        for path in paths {
            let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
            let mut reader = decoder.read_info().unwrap();
            let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
            let frame = reader.next_frame(&mut pixels).unwrap();
            assert_eq!((frame.width, frame.height), (64, 64));
        }
    }

    #[test]
    fn jpeg_screenshot_chooses_a_new_name_without_replacing_the_old_one() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("image.jpg");
        std::fs::write(&path, b"keep").unwrap();
        let pixels = [200, 40, 30, 255].repeat(16 * 16);
        let saved = write_screenshot_unique(&path, ScreenshotFormat::Jpeg, 16, 16, &pixels, 85).unwrap();
        assert_eq!(saved, folder.path().join("image (2).jpg"));
        assert_eq!(std::fs::read(path).unwrap(), b"keep");
        assert_eq!(image::open(saved).unwrap().to_rgb8().dimensions(), (16, 16));
    }

    #[test]
    fn jpeg_round_trips_and_rejects_invalid_dimensions() {
        let path = std::env::temp_dir().join(format!("opnoverlay-jpeg-{}.jpg", std::process::id()));
        let pixels = [200, 40, 30, 255].repeat(16 * 16);
        write_jpeg(&path, 16, 16, &pixels, 90).unwrap();
        let image = image::open(&path).unwrap().to_rgb8();
        assert_eq!(image.dimensions(), (16, 16));
        let pixel = image.get_pixel(8, 8).0;
        assert!((pixel[0] as i16 - 200).abs() < 10);
        assert!((pixel[1] as i16 - 40).abs() < 10);
        assert!((pixel[2] as i16 - 30).abs() < 10);
        std::fs::remove_file(&path).unwrap();
        assert!(write_jpeg(&path, 16, 16, &[0; 3], 90).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn an_odd_sized_frame_is_cropped_not_scaled() {
        assert_eq!(placement((1001, 701), (1000, 700), None), Placement::Crop((0, 0)));
        assert_eq!(placement((1000, 700), (1000, 700), None), Placement::Copy);
        assert_eq!(placement((1000, 700), (1000, 700), Some((0, 0))), Placement::Copy);
        assert_eq!(placement((1920, 1080), (1000, 700), Some((8, 4))), Placement::Crop((8, 4)));
        // A window resized while recording.
        assert_eq!(placement((1200, 800), (1000, 700), None), Placement::Scale);
        assert_eq!(placement((999, 700), (1000, 700), None), Placement::Scale);
    }

    #[test]
    fn bgra_rows_with_padding_become_packed_rgba() {
        // 2×2 image, rows padded to 12 bytes.
        let bgra = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 9, 10, 11, 12, 13, 14, 15, 16, 0, 0, 0, 0];
        assert_eq!(
            bgra_to_rgba(&bgra, 2, 2, 12),
            [3, 2, 1, 255, 7, 6, 5, 255, 11, 10, 9, 255, 15, 14, 13, 255]
        );
    }

    #[test]
    fn preview_downscale_averages_every_pixel() {
        // 4×2 BGRA, rows padded to 20 bytes: left half black/white columns, right half grey.
        let mut bgra = Vec::new();
        for _ in 0..2 {
            bgra.extend_from_slice(&[0, 0, 0, 255, 255, 255, 255, 255, 100, 100, 100, 255, 100, 100, 100, 255, 9, 9, 9, 9]);
        }
        // Half size: each output pixel is the mean of a 2×2 block.
        assert_eq!(downscale(&bgra, (4, 2), 20, (2, 1)), [127, 127, 127, 255, 100, 100, 100, 255]);
        // Same size: a plain BGRA to RGBA copy.
        let same = downscale(&bgra, (4, 2), 20, (4, 2));
        assert_eq!(same, bgra_to_rgba(&bgra, 4, 2, 20));
    }

    /// The straightforward version `downscale` must match: each source pixel added to its
    /// output column one by one.
    fn reference_downscale(bgra: &[u8], (w, h): (usize, usize), pitch: usize, size: (usize, usize)) -> Vec<u8> {
        let (pw, ph) = size;
        let column: Vec<usize> = (0..w).map(|x| x * pw / w).collect();
        let mut sums = vec![0u32; pw * 3];
        let mut counts = vec![0u32; pw];
        let mut rgba = Vec::with_capacity(pw * ph * 4);
        for y in 0..ph {
            let (top, bottom) = (y * h / ph, ((y + 1) * h / ph).max(y * h / ph + 1));
            sums.fill(0);
            counts.fill(0);
            for sy in top..bottom {
                let row = &bgra[sy * pitch..sy * pitch + w * 4];
                for (&c, &[b, g, r, _]) in column.iter().zip(row.as_chunks::<4>().0) {
                    sums[c * 3] += r as u32;
                    sums[c * 3 + 1] += g as u32;
                    sums[c * 3 + 2] += b as u32;
                    counts[c] += 1;
                }
            }
            for (rgb, &n) in sums.as_chunks::<3>().0.iter().zip(&counts) {
                let n = n.max(1);
                rgba.extend_from_slice(&[(rgb[0] / n) as u8, (rgb[1] / n) as u8, (rgb[2] / n) as u8, 255]);
            }
        }
        rgba
    }

    #[test]
    fn preview_downscale_matches_the_straightforward_average() {
        // Odd sizes and ratios, padded rows, varied pixel values.
        let (w, h, pitch) = (37, 23, 37 * 4 + 12);
        let bgra: Vec<u8> = (0..pitch * h).map(|i| (i * 7 % 251) as u8).collect();
        for size in [(37, 23), (36, 22), (20, 11), (13, 8), (5, 3), (1, 1)] {
            assert_eq!(downscale(&bgra, (w, h), pitch, size), reference_downscale(&bgra, (w, h), pitch, size), "{size:?}");
        }
    }

    #[test]
    #[ignore = "timing, run by hand: cargo test --release preview_downscale_speed -- --ignored --nocapture"]
    fn preview_downscale_speed() {
        let (w, h) = (1920, 1080);
        let bgra: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        for (name, f) in [("new", downscale as fn(&[u8], (usize, usize), usize, (usize, usize)) -> Vec<u8>), ("reference", reference_downscale)] {
            let start = std::time::Instant::now();
            for _ in 0..50 {
                std::hint::black_box(f(&bgra, (w, h), w * 4, (868, 488)));
            }
            println!("{name}: {:.2} ms", start.elapsed().as_secs_f64() * 1e3 / 50.0);
        }
    }

    #[test]
    fn crop_keeps_only_the_region() {
        // 3×2 image whose pixels are numbered 0..6 in the red channel.
        let rgba: Vec<u8> = (0..6).flat_map(|i| [i, 0, 0, 255]).collect();
        let region = Region { x: 1, y: 0, width: 2, height: 2 };
        assert_eq!(crop_rgba(&rgba, 3, region), [1, 0, 0, 255, 2, 0, 0, 255, 4, 0, 0, 255, 5, 0, 0, 255]);
    }

    #[test]
    fn png_round_trips() {
        let path = std::env::temp_dir().join(format!("obs-lite-test-{}.png", std::process::id()));
        let rgba = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255];
        write_png(&path, 2, 2, &rgba).unwrap();
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut out = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut out).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!((info.width, info.height), (2, 2));
        assert_eq!(out, rgba);
    }
}
