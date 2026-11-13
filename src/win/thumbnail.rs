use std::path::Path;

use eframe::egui::ColorImage;
use windows::core::HSTRING;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HGDIOBJ, ReleaseDC};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_CROPTOSQUARE, SIIGBF_THUMBNAILONLY};

pub fn load(path: &Path) -> Option<ColorImage> {
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let result = (|| {
            let item: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None).ok()?;
            let bitmap = item.GetImage(SIZE { cx: 256, cy: 256 }, SIIGBF_THUMBNAILONLY | SIIGBF_CROPTOSQUARE).ok()?;
            let mut info = BITMAP::default();
            let object = HGDIOBJ(bitmap.0);
            let got = GetObjectW(object, std::mem::size_of::<BITMAP>() as i32, Some(std::ptr::from_mut(&mut info).cast()));
            let image = if got > 0 && info.bmWidth > 0 && info.bmHeight > 0 && info.bmWidth <= 1024 && info.bmHeight <= 1024 {
                let width = info.bmWidth as usize;
                let height = info.bmHeight as usize;
                let mut pixels = vec![0u8; width * height * 4];
                let mut format = BITMAPINFO { bmiHeader: BITMAPINFOHEADER { biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32, biWidth: info.bmWidth, biHeight: -info.bmHeight, biPlanes: 1, biBitCount: 32, biCompression: BI_RGB.0, ..Default::default() }, ..Default::default() };
                let dc = GetDC(None);
                let rows = GetDIBits(dc, bitmap, 0, height as u32, Some(pixels.as_mut_ptr().cast()), &mut format, DIB_RGB_COLORS);
                ReleaseDC(None, dc);
                if rows == height as i32 {
                    for pixel in pixels.chunks_exact_mut(4) { pixel.swap(0, 2); pixel[3] = 255; }
                    Some(ColorImage::from_rgba_unmultiplied([width, height], &pixels))
                } else { None }
            } else { None };
            let _ = DeleteObject(object);
            image
        })();
        if initialized { CoUninitialize(); }
        result
    }
}

pub fn video_frames(path: &Path, duration: f64, count: usize) -> Result<Vec<ColorImage>, String> {
    use windows::Win32::Media::MediaFoundation::{MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFVideoFormat_RGB32, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_ENDOFSTREAM};
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, StructuredStorage::PROPVARIANT};
    use windows::Win32::System::Variant::VT_I8;
    if !duration.is_finite() || duration <= 0.0 || count == 0 { return Err("Неизвестна длительность видео.".into()); }
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let result = (|| -> Result<Vec<ColorImage>, String> {
            let _media = super::mf::Media::start().map_err(|e| e.to_string())?;
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 1).map_err(|e| e.to_string())?;
            let attributes = attributes.ok_or("Не удалось создать параметры видео.")?;
            attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).map_err(|e| e.to_string())?;
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attributes).map_err(|e| e.to_string())?;
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let format = MFCreateMediaType().map_err(|e| e.to_string())?;
            format.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
            format.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(|e| e.to_string())?;
            reader.SetCurrentMediaType(stream, None, &format).map_err(|e| e.to_string())?;
            let current = reader.GetCurrentMediaType(stream).map_err(|e| e.to_string())?;
            let size = current.GetUINT64(&MF_MT_FRAME_SIZE).map_err(|e| e.to_string())?;
            let (width, height) = ((size >> 32) as usize, (size as u32) as usize);
            if width == 0 || height == 0 || width > 8192 || height > 8192 { return Err("Неподдерживаемый размер видео.".into()); }
            let stride = current.GetUINT32(&MF_MT_DEFAULT_STRIDE).map_or(-((width * 4) as i32), |s| s as i32);
            let pitch = stride.unsigned_abs() as usize;
            if pitch < width * 4 { return Err("Неверный формат кадров видео.".into()); }
            let mut images = Vec::with_capacity(count);
            for index in 0..count {
                let seconds = duration * (index as f64 + 0.5) / count as f64;
                let mut position = PROPVARIANT::default();
                (*position.Anonymous.Anonymous).vt = VT_I8;
                (*position.Anonymous.Anonymous).Anonymous.hVal = (seconds * 10_000_000.0) as i64;
                reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &position).map_err(|e| e.to_string())?;
                let mut found = None;
                for _ in 0..120 {
                    let (mut flags, mut sample) = (0, None);
                    reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample)).map_err(|e| e.to_string())?;
                    if let Some(sample) = sample { found = Some(sample); break; }
                    if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 { break; }
                }
                let sample = found.ok_or("Не удалось прочитать кадр видео.")?;
                let buffer = sample.ConvertToContiguousBuffer().map_err(|e| e.to_string())?;
                let (mut bytes, mut len) = (std::ptr::null_mut(), 0);
                buffer.Lock(&mut bytes, None, Some(&mut len)).map_err(|e| e.to_string())?;
                let image = if (len as usize) >= pitch.saturating_mul(height) {
                    let src = std::slice::from_raw_parts(bytes, len as usize);
                    let target_width = width.min(160);
                    let target_height = (height * target_width / width).max(1);
                    let mut rgba = vec![0; target_width * target_height * 4];
                    for y in 0..target_height {
                        let sy = y * height / target_height;
                        let row = if stride < 0 { height - 1 - sy } else { sy };
                        for x in 0..target_width {
                            let input = row * pitch + (x * width / target_width) * 4;
                            let output = (y * target_width + x) * 4;
                            rgba[output..output + 4].copy_from_slice(&[src[input + 2], src[input + 1], src[input], 255]);
                        }
                    }
                    Some(ColorImage::from_rgba_unmultiplied([target_width, target_height], &rgba))
                } else { None };
                buffer.Unlock().map_err(|e| e.to_string())?;
                images.push(image.ok_or("Неполный кадр видео.")?);
            }
            Ok(images)
        })();
        if initialized { CoUninitialize(); }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn video_timeline_loads_real_frames() {
        let path = std::env::var_os("OPNOVERLAY_CHECK_FILE").expect("OPNOVERLAY_CHECK_FILE is required");
        let frames = video_frames(Path::new(&path), 4.0, 4).expect("video frames could not be decoded");
        assert_eq!(frames.len(), 4);
        assert!(frames.iter().all(|image| image.size[0] > 0 && image.size[1] > 0));
        assert!(frames.iter().any(|image| image.pixels.iter().any(|pixel| pixel.r() > 20 || pixel.g() > 20 || pixel.b() > 20)));
        assert!(frames.windows(2).any(|pair| pair[0].pixels != pair[1].pixels));
    }

    #[test]
    #[ignore]
    fn extracts_thumbnail_from_real_file() {
        let path = std::env::var_os("OPNOVERLAY_CHECK_FILE").expect("OPNOVERLAY_CHECK_FILE is required");
        let image = load(Path::new(&path)).expect("Windows did not return a thumbnail");
        assert!(image.size[0] > 0 && image.size[1] > 0);
        assert!(image.pixels.iter().any(|pixel| pixel.r() > 20 || pixel.g() > 20 || pixel.b() > 20));
        println!("thumbnail {}x{}", image.size[0], image.size[1]);
    }
}
