//! Draws the app icon and embeds it in the executable.
//!
//! The same drawing is written as a multi-size .ico (for Explorer and the taskbar), as raw
//! RGBA (for the window icon) and as a 256 px PNG (for the Linux desktop entry, installed by
//! the Flatpak build), so there is a single source of truth.

use std::env;
use std::fs;
use std::path::PathBuf;

/// Renders the icon at `size`×`size` as straight-alpha RGBA, with 4×4 supersampling.
fn render(size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let s = size as f32;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let mut acc = [0f32; 4]; // premultiplied
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = (x as f32 + (sx as f32 + 0.5) / SS as f32) / s;
                    let py = (y as f32 + (sy as f32 + 0.5) / SS as f32) / s;
                    let [r, g, b, a] = shade(px, py);
                    acc[0] += r * a;
                    acc[1] += g * a;
                    acc[2] += b * a;
                    acc[3] += a;
                }
            }
            let n = (SS * SS) as f32;
            let a = acc[3] / n;
            let i = ((y * size + x) * 4) as usize;
            if a > 0.0 {
                for c in 0..3 {
                    out[i + c] = ((acc[c] / n / a).clamp(0.0, 1.0) * 255.0).round() as u8;
                }
            }
            out[i + 3] = (a * 255.0).round() as u8;
        }
    }
    out
}

fn shade(x: f32, y: f32) -> [f32; 4] {
    let line = |ax: f32, ay: f32, bx: f32, by: f32| {
        let (vx, vy) = (bx - ax, by - ay);
        let t = (((x - ax) * vx + (y - ay) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
        ((x - ax - t * vx).powi(2) + (y - ay - t * vy).powi(2)).sqrt()
    };
    let lower = line(0.18, 0.34, 0.18, 0.69)
        .min(line(0.18, 0.69, 0.25, 0.76))
        .min(line(0.25, 0.76, 0.63, 0.76));
    let upper = line(0.38, 0.24, 0.75, 0.24)
        .min(line(0.75, 0.24, 0.82, 0.31))
        .min(line(0.82, 0.31, 0.82, 0.65));
    let dot = ((x - 0.82).powi(2) + (y - 0.79).powi(2)).sqrt();
    if dot < 0.065 { return [0.82, 0.29, 0.22, 1.0]; }
    if lower < 0.055 { return [0.12, 0.55, 0.47, 1.0]; }
    if upper < 0.055 { return [0.27, 0.65, 0.55, 1.0]; }
    if x >= 0.39 && x <= 0.64 && y >= 0.37 && y <= 0.63
        && y >= 0.37 + (x - 0.39) * 0.52
        && y <= 0.63 - (x - 0.39) * 0.52 {
        return [0.20, 0.60, 0.51, 1.0];
    }
    [0.0, 0.0, 0.0, 0.0]
}
/// Encodes RGBA images as an .ico with 32-bit BMP entries.
fn ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&size| bmp_entry(size, &render(size))).collect();
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&size, data) in sizes.iter().zip(&images) {
        let dim = if size >= 256 { 0 } else { size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for data in images {
        out.extend_from_slice(&data);
    }
    out
}

fn bmp_entry(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&(size as i32 * 2).to_le_bytes()); // colour + mask
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    let mask_row = size.div_ceil(32) * 4;
    out.resize(out.len() + (mask_row * size) as usize, 0);
    out
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("icon64.rgba"), render(64)).unwrap();
    let ico_path = out.join("icon.ico");
    fs::write(&ico_path, ico(&[16, 24, 32, 48, 64, 256])).unwrap();
    let png = fs::File::create(out.join("icon256.png")).unwrap();
    let mut encoder = png::Encoder::new(png, 256, 256);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().unwrap().write_image_data(&render(256)).unwrap();

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico_path.to_str().unwrap());
        res.set("FileDescription", "opnoverlay");
        res.set("ProductName", "opnoverlay");
        res.compile().expect("embed Windows resources");
    }
}
