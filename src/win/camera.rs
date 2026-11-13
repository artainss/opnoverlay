//! Webcam capture through Media Foundation.

use std::thread;

use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFActivate, IMFMediaSource, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
    IMFSourceReader, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromMediaSource, MFEnumDeviceSources, MFMediaType_Video, MFSTARTUP_FULL,
    MFShutdown, MFStartup, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::{GUID, Interface, PWSTR};

use crate::camera::{CameraDevice, CameraFrame, CameraShared};
use crate::i18n::tr;

/// Video capture devices, from a thread of its own (COM and Media Foundation are per thread).
pub fn list_devices() -> Vec<CameraDevice> {
    let list = || {
        with_media_foundation(|| {
            Ok(devices()?.iter().filter_map(|a| Some(CameraDevice { id: link(a)?, name: name(a)? })).collect())
        })
    };
    thread::spawn(list).join().ok().and_then(Result::ok).unwrap_or_default()
}

fn with_media_foundation<T>(f: impl FnOnce() -> windows::core::Result<T>) -> windows::core::Result<T> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
    }
    let result = f();
    let _ = unsafe { MFShutdown() };
    result
}

fn devices() -> windows::core::Result<Vec<IMFActivate>> {
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.expect("MFCreateAttributes returned nothing");
        attributes.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)?;
        let mut array = std::ptr::null_mut();
        let mut count = 0;
        MFEnumDeviceSources(&attributes, &mut array, &mut count)?;
        // With no camera the array may be null, which a slice must never be.
        if array.is_null() {
            return Ok(Vec::new());
        }
        let activates = std::slice::from_raw_parts_mut(array, count as usize).iter_mut().filter_map(Option::take).collect();
        CoTaskMemFree(Some(array as *const _));
        Ok(activates)
    }
}

fn string(activate: &IMFActivate, key: &GUID) -> Option<String> {
    unsafe {
        let mut value = PWSTR::null();
        let mut len = 0;
        activate.GetAllocatedString(key, &mut value, &mut len).ok()?;
        let text = value.to_string().ok();
        CoTaskMemFree(Some(value.0 as *const _));
        text
    }
}

fn name(activate: &IMFActivate) -> Option<String> {
    string(activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
}

fn link(activate: &IMFActivate) -> Option<String> {
    string(activate, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK)
}

/// The size of the reader's pictures and the distance between their rows (negative when the
/// picture is stored bottom-up).
unsafe fn picture_format(reader: &IMFSourceReader, stream: u32) -> windows::core::Result<(u32, u32, i32)> {
    unsafe {
        let current = reader.GetCurrentMediaType(stream)?;
        let size = current.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let (width, height) = ((size >> 32) as u32, size as u32);
        // Uncompressed RGB is bottom-up (negative stride) unless the type says otherwise.
        let stride = current.GetUINT32(&MF_MT_DEFAULT_STRIDE).map_or(-(width as i32 * 4), |s| s as i32);
        Ok((width, height, stride))
    }
}

/// Captures from `device` until asked to stop, handing every picture to `shared`.
pub fn capture(device: &str, shared: &CameraShared, repaint: Option<&eframe::egui::Context>) -> Result<(), String> {
    with_media_foundation(|| capture_loop(device, shared, repaint)).map_err(|e| e.to_string())
}

fn capture_loop(device: &str, shared: &CameraShared, repaint: Option<&eframe::egui::Context>) -> windows::core::Result<()> {
    let not_found = || windows::core::Error::new(windows::core::HRESULT(-1), tr!("cámara no encontrada", "camera not found"));
    let activate = devices()?.into_iter().find(|a| link(a).as_deref() == Some(device)).ok_or_else(not_found)?;
    unsafe {
        let source: IMFMediaSource = activate.ActivateObject()?;
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 1)?;
        let attributes = attributes.expect("MFCreateAttributes returned nothing");
        // Lets the reader convert whatever the camera delivers (YUY2, NV12, MJPG…) to RGB32.
        attributes.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        let reader = MFCreateSourceReaderFromMediaSource(&source, &attributes)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

        let mut natives = Vec::new();
        for i in 0.. {
            let Ok(native) = reader.GetNativeMediaType(stream, i) else { break };
            let Ok(size) = native.GetUINT64(&MF_MT_FRAME_SIZE) else { continue };
            // Numerator and denominator; a mode that does not say counts as slow.
            let rate = native.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
            let fps = (rate >> 32) as f64 / (rate as u32).max(1) as f64;
            natives.push((native, ((size >> 32) as u32, size as u32, fps)));
        }
        if let Some(i) = crate::camera::best_mode(natives.iter().map(|(_, mode)| *mode)) {
            reader.SetCurrentMediaType(stream, None, &natives[i].0)?;
        }
        let rgb = MFCreateMediaType()?;
        rgb.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        rgb.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader.SetCurrentMediaType(stream, None, &rgb)?;
        let (mut width, mut height, mut stride) = picture_format(&reader, stream)?;

        while !shared.stopped() {
            let mut flags = 0u32;
            let mut sample = None;
            reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            // The camera may change its picture size mid-stream.
            if flags & MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0 as u32 != 0 {
                (width, height, stride) = picture_format(&reader, stream)?;
            }
            let Some(sample) = sample else { continue };
            // Never read past the picture: a sample that is not the size expected is skipped.
            let needed = width as usize * 4 * height as usize;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
            // Rows are `pitch` bytes apart starting from the top row; negative for bottom-up images.
            let mut copy = |top: *const u8, pitch: isize| {
                for y in 0..height as isize {
                    let row = std::slice::from_raw_parts(top.offset(y * pitch), width as usize * 4);
                    bgra.extend(row.as_chunks::<4>().0.iter().flat_map(|&[b, g, r, _]| [b, g, r, 255]));
                }
            };
            if let Ok(buffer) = buffer.cast::<IMF2DBuffer>() {
                let (mut scan0, mut pitch) = (std::ptr::null_mut(), 0i32);
                buffer.Lock2D(&mut scan0, &mut pitch)?;
                let fits = buffer.GetContiguousLength().is_ok_and(|len| (pitch.unsigned_abs() as usize) * height as usize <= len as usize)
                    && pitch.unsigned_abs() as usize >= width as usize * 4;
                if fits {
                    copy(scan0, pitch as isize);
                }
                buffer.Unlock2D()?;
                if !fits {
                    continue;
                }
            } else {
                let (mut data, mut length) = (std::ptr::null_mut(), 0u32);
                buffer.Lock(&mut data, None, Some(&mut length))?;
                let fits = (length as usize) >= needed.max(stride.unsigned_abs() as usize * height as usize);
                if fits {
                    let top = if stride < 0 { data.offset(-stride as isize * (height as isize - 1)) } else { data };
                    copy(top, stride as isize);
                }
                buffer.Unlock()?;
                if !fits {
                    continue;
                }
            }
            shared.deliver(CameraFrame { width, height, bgra }, repaint);
        }
        // Releases the camera (its light goes off).
        let _ = source.Shutdown();
    }
    Ok(())
}
