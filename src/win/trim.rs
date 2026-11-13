use std::path::Path;

use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::{
    MFCreateSourceReaderFromURL, MFMediaType_Audio, MFMediaType_Video,
    MF_MT_MAJOR_TYPE, MF_SOURCE_READER_ANY_STREAM, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READERF_ERROR,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::mf::Media;

pub fn trim(source: &Path, destination: &Path, start: i64, end: i64) -> Result<(i64, i64), String> {
    if start < 0 || end <= start { return Err("Неверные границы обрезки.".into()); }
    let reserved = std::fs::OpenOptions::new().write(true).create_new(true).open(destination).map_err(|e| e.to_string())?;
    drop(reserved);
    let result = trim_exact(source, destination, start, end);
    if result.is_err() { let _ = std::fs::remove_file(destination); }
    result
}

fn pack_nv12_sample(sample: &windows::Win32::Media::MediaFoundation::IMFSample, width: u32, height: u32, stride: i32) -> Result<windows::Win32::Media::MediaFoundation::IMFSample, String> {
    use windows::Win32::Media::MediaFoundation::{MFCreateMemoryBuffer, MFCreateSample};
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 || stride < width as i32 { return Err("Неподдерживаемый формат кадра.".into()); }
    let pitch = stride as usize;
    let total = (width as usize).checked_mul(height as usize).and_then(|value| value.checked_mul(3)).map(|value| value / 2).ok_or("Слишком большой кадр.")?;
    let input = unsafe { sample.ConvertToContiguousBuffer() }.map_err(|e| e.to_string())?;
    let (mut data, mut length) = (std::ptr::null_mut(), 0);
    unsafe { input.Lock(&mut data, None, Some(&mut length)) }.map_err(|e| e.to_string())?;
    let result = (|| -> Result<_, String> {
        let len = length as usize;
        let aligned_height = len.checked_mul(2).and_then(|n| n.checked_div(3 * pitch)).ok_or("Неверный размер кадра.")?;
        if aligned_height < height as usize || aligned_height % 2 != 0 || aligned_height.checked_mul(pitch).and_then(|n| n.checked_mul(3)).map(|n| n / 2) != Some(len) {
            return Err("Неподдерживаемая разметка кадра.".into());
        }
        let output = unsafe { MFCreateMemoryBuffer(u32::try_from(total).map_err(|_| "Слишком большой кадр.")?) }.map_err(|e| e.to_string())?;
        let mut target = std::ptr::null_mut();
        unsafe { output.Lock(&mut target, None, None) }.map_err(|e| e.to_string())?;
        unsafe {
            for row in 0..height as usize {
                std::ptr::copy_nonoverlapping(data.add(row * pitch), target.add(row * width as usize), width as usize);
            }
            for row in 0..height as usize / 2 {
                std::ptr::copy_nonoverlapping(data.add(aligned_height * pitch + row * pitch), target.add(height as usize * width as usize + row * width as usize), width as usize);
            }
            output.Unlock().map_err(|e| e.to_string())?;
            output.SetCurrentLength(total as u32).map_err(|e| e.to_string())?;
            let packed = MFCreateSample().map_err(|e| e.to_string())?;
            sample.CopyAllItems(&packed).map_err(|e| e.to_string())?;
            packed.SetSampleFlags(sample.GetSampleFlags().unwrap_or(0)).map_err(|e| e.to_string())?;
            packed.SetSampleTime(sample.GetSampleTime().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            packed.SetSampleDuration(sample.GetSampleDuration().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            packed.AddBuffer(&output).map_err(|e| e.to_string())?;
            Ok(packed)
        }
    })();
    unsafe { input.Unlock() }.map_err(|e| e.to_string())?;
    result
}

fn trim_exact(source: &Path, destination: &Path, start: i64, end: i64) -> Result<(i64, i64), String> {
    use windows::Win32::Media::MediaFoundation::{MFCreateAttributes, MFCreateMediaType, MFCreateSinkWriterFromURL, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_CHROMA_SITING, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_DISABLE_THROTTLING};
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Variant::VT_I8;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _media = Media::start().map_err(|e| e.to_string())?;
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(source.as_os_str()), None).map_err(|e| e.to_string())?;
        let mut video = None;
        let mut audio = None;
        for index in 0..32 {
            let Ok(format) = reader.GetNativeMediaType(index, 0) else { break };
            let major = format.GetGUID(&MF_MT_MAJOR_TYPE).map_err(|e| e.to_string())?;
            if major == MFMediaType_Video && video.is_none() { video = Some((index, format)); }
            else if major == MFMediaType_Audio && audio.is_none() { audio = Some((index, format)); }
            else { reader.SetStreamSelection(index, false).map_err(|e| e.to_string())?; }
        }
        let (video_index, native_video) = video.ok_or("В файле нет видеодорожки.")?;
        let input_type = MFCreateMediaType().map_err(|e| e.to_string())?;
        input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
        input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).map_err(|e| e.to_string())?;
        reader.SetCurrentMediaType(video_index, None, &input_type).map_err(|e| e.to_string())?;
        let decoded_type = reader.GetCurrentMediaType(video_index).map_err(|e| e.to_string())?;
        let size = decoded_type.GetUINT64(&MF_MT_FRAME_SIZE).map_err(|e| e.to_string())?;
        let (width, height) = ((size >> 32) as u32, size as u32);
        let stride = decoded_type.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or(width) as i32;
        let output_type = MFCreateMediaType().map_err(|e| e.to_string())?;
        output_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(|e| e.to_string())?;
        output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).map_err(|e| e.to_string())?;
        output_type.SetUINT64(&MF_MT_FRAME_SIZE, decoded_type.GetUINT64(&MF_MT_FRAME_SIZE).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        output_type.SetUINT64(&MF_MT_FRAME_RATE, decoded_type.GetUINT64(&MF_MT_FRAME_RATE).or_else(|_| native_video.GetUINT64(&MF_MT_FRAME_RATE)).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        output_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, decoded_type.GetUINT64(&MF_MT_PIXEL_ASPECT_RATIO).unwrap_or((1u64 << 32) | 1)).map_err(|e| e.to_string())?;
        output_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).map_err(|e| e.to_string())?;
        output_type.SetUINT32(&MF_MT_AVG_BITRATE, native_video.GetUINT32(&MF_MT_AVG_BITRATE).unwrap_or(16_000_000).max(1_000_000)).map_err(|e| e.to_string())?;
        for key in [MF_MT_VIDEO_PRIMARIES, MF_MT_TRANSFER_FUNCTION, MF_MT_YUV_MATRIX, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_CHROMA_SITING] {
            if let Ok(value) = decoded_type.GetUINT32(&key).or_else(|_| native_video.GetUINT32(&key)) {
                output_type.SetUINT32(&key, value).map_err(|e| e.to_string())?;
            }
        }
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 2).map_err(|e| e.to_string())?;
        let attributes = attributes.ok_or("Не удалось создать параметры экспорта.")?;
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 0).map_err(|e| e.to_string())?;
        attributes.SetUINT32(&MF_SINK_WRITER_DISABLE_THROTTLING, 1).map_err(|e| e.to_string())?;
        let writer = MFCreateSinkWriterFromURL(&HSTRING::from(destination.as_os_str()), None, &attributes).map_err(|e| e.to_string())?;
        let output_video = writer.AddStream(&output_type).map_err(|e| e.to_string())?;
        writer.SetInputMediaType(output_video, &decoded_type, None).map_err(|e| e.to_string())?;
        let output_audio = if let Some((_, format)) = &audio {
            let stream = writer.AddStream(format).map_err(|e| e.to_string())?;
            writer.SetInputMediaType(stream, format, None).map_err(|e| e.to_string())?;
            Some(stream)
        } else { None };
        writer.BeginWriting().map_err(|e| e.to_string())?;
        let mut position = PROPVARIANT::default();
        (*position.Anonymous.Anonymous).vt = VT_I8;
        (*position.Anonymous.Anonymous).Anonymous.hVal = start;
        reader.SetCurrentPosition(&windows::core::GUID::zeroed(), &position).map_err(|e| e.to_string())?;
        let mut first = None;
        let mut last = 0;
        let mut remaining = if audio.is_some() { 2 } else { 1 };
        while remaining > 0 {
            let (mut stream, mut flags, mut timestamp, mut sample) = (0, 0, 0, None);
            reader.ReadSample(MF_SOURCE_READER_ANY_STREAM.0 as u32, 0, Some(&mut stream), Some(&mut flags), Some(&mut timestamp), Some(&mut sample)).map_err(|e| e.to_string())?;
            if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 { return Err("Не удалось прочитать исходное видео.".into()); }
            let finished = flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0;
            if finished || timestamp >= end {
                reader.SetStreamSelection(stream, false).map_err(|e| e.to_string())?;
                remaining -= 1;
            }
            let Some(sample) = sample else { continue };
            if timestamp < start || timestamp >= end { continue; }
            let is_video = stream == video_index;
            if first.is_none() && is_video { first = Some(timestamp); }
            let Some(first) = first else { continue };
            if timestamp < first { continue; }
            let duration = sample.GetSampleDuration().unwrap_or(0).max(0).min(end - timestamp);
            sample.SetSampleTime(timestamp - first).map_err(|e| e.to_string())?;
            sample.SetSampleDuration(duration).map_err(|e| e.to_string())?;
            if is_video {
                let packed = pack_nv12_sample(&sample, width, height, stride)?;
                writer.WriteSample(output_video, &packed).map_err(|e| e.to_string())?;
                last = timestamp + duration;
            } else if audio.as_ref().is_some_and(|(index, _)| *index == stream) && let Some(output_audio) = output_audio {
                writer.WriteSample(output_audio, &sample).map_err(|e| e.to_string())?;
            }
        }
        let first = first.ok_or("В выбранном отрезке нет кадров.")?;
        if last <= first { return Err("В выбранном отрезке нет кадров.".into()) }
        writer.Finalize().map_err(|e| e.to_string())?;
        Ok((first, last))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_nv12_skips_aligned_padding_between_luma_and_chroma() {
        use windows::Win32::Media::MediaFoundation::{MFCreateMemoryBuffer, MFCreateSample};
        unsafe {
            let sample = MFCreateSample().unwrap();
            let buffer = MFCreateMemoryBuffer(36).unwrap();
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None).unwrap();
            let bytes = std::slice::from_raw_parts_mut(data, 36);
            bytes[..16].fill(40);
            bytes[16..24].fill(250);
            bytes[24..32].fill(120);
            bytes[32..].fill(250);
            buffer.Unlock().unwrap();
            buffer.SetCurrentLength(36).unwrap();
            sample.AddBuffer(&buffer).unwrap();
            sample.SetSampleTime(1_000_000).unwrap();
            sample.SetSampleDuration(166_667).unwrap();
            let packed = pack_nv12_sample(&sample, 4, 4, 4).unwrap();
            assert_eq!(packed.GetSampleTime().unwrap(), 1_000_000);
            let buffer = packed.ConvertToContiguousBuffer().unwrap();
            let (mut data, mut len) = (std::ptr::null_mut(), 0);
            buffer.Lock(&mut data, None, Some(&mut len)).unwrap();
            let bytes = std::slice::from_raw_parts(data, len as usize);
            assert_eq!(bytes.len(), 24);
            assert!(bytes[..16].iter().all(|&value| value == 40));
            assert!(bytes[16..].iter().all(|&value| value == 120));
            buffer.Unlock().unwrap();
        }
    }

    #[test]
    fn failed_trim_preserves_source_and_existing_destination() {
        let folder = tempfile::tempdir().unwrap();
        let source = folder.path().join("source.mp4");
        let destination = folder.path().join("destination.mp4");
        std::fs::write(&source, b"invalid media").unwrap();
        std::fs::write(&destination, b"existing file").unwrap();
        assert!(trim(&source, &destination, 0, 10).is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"existing file");
        assert!(trim(&source, &source, 0, 10).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"invalid media");
        let failed = folder.path().join("failed.mp4");
        assert!(trim(&source, &failed, 0, 10).is_err());
        assert!(!failed.exists());
        assert!(trim(&source, &failed, 10, 10).is_err());
        assert!(!failed.exists());
    }

    #[test]
    #[ignore]
    fn recorded_game_sample_decodes_nonblack_frames() {
        use windows::Win32::Media::MediaFoundation::{MFCreateMediaType, MFVideoFormat_NV12, MF_MT_SUBTYPE, MF_SOURCE_READER_FIRST_VIDEO_STREAM};
        let source = std::path::PathBuf::from(std::env::var_os("OPNOVERLAY_CHECK_FILE").unwrap());
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _media = Media::start().unwrap();
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(source.as_os_str()), None).unwrap();
            let format = MFCreateMediaType().unwrap();
            format.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
            format.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).unwrap();
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            reader.SetCurrentMediaType(stream, None, &format).unwrap();
            let mut frames = 0;
            let mut brightest = 0.0f64;
            while frames < 120 {
                let (mut flags, mut sample) = (0, None);
                reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample)).unwrap();
                if let Some(sample) = sample {
                    let buffer = sample.ConvertToContiguousBuffer().unwrap();
                    let (mut bytes, mut len) = (std::ptr::null_mut(), 0);
                    buffer.Lock(&mut bytes, None, Some(&mut len)).unwrap();
                    if len >= 2 {
                        let data = std::slice::from_raw_parts(bytes, len as usize);
                        let luma = &data[..data.len() * 2 / 3];
                        let mean = luma.iter().map(|&value| u64::from(value)).sum::<u64>() as f64 / luma.len() as f64;
                        brightest = brightest.max(mean);
                    }
                    buffer.Unlock().unwrap();
                    frames += 1;
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 { break; }
            }
            assert!(frames > 0 && brightest > 25.0, "decoded {frames} frames; brightest luma {brightest:.1}");
            println!("decoded {frames} frames; brightest luma {brightest:.1}");
        }
    }

    #[test]
    #[ignore]
    fn exact_trim_keeps_selected_boundaries() {
        let source = std::path::PathBuf::from(std::env::var_os("OPNOVERLAY_CHECK_FILE").unwrap());
        let folder = tempfile::tempdir().unwrap();
        let copy = folder.path().join("source.mp4");
        let output = folder.path().join("result.mp4");
        std::fs::copy(source, &copy).unwrap();
        let before = std::fs::read(&copy).unwrap();
        let (first, last) = trim(&copy, &output, 90_000_000, 450_000_000).unwrap();
        assert!((90_000_000..=90_200_000).contains(&first), "start shifted to {first}");
        assert!((449_000_000..=450_000_000).contains(&last), "end shifted to {last}");
        assert_eq!(std::fs::read(&copy).unwrap(), before);
        assert!(std::fs::metadata(&output).unwrap().len() > 1000);
        println!("source bytes: {}, trimmed bytes: {}", before.len(), std::fs::metadata(&output).unwrap().len());
    }

    #[test]
    #[ignore]
    fn trims_a_real_recording_without_overwriting() {
        use windows::Win32::Media::MediaFoundation::{MFAudioFormat_PCM, MFCreateMediaType, MFVideoFormat_NV12, MF_MT_SUBTYPE, MF_SOURCE_READER_FIRST_AUDIO_STREAM, MF_SOURCE_READER_FIRST_VIDEO_STREAM};
        let source = std::path::PathBuf::from(std::env::var_os("OPNOVERLAY_CHECK_FILE").unwrap());
        let folder = tempfile::tempdir().unwrap();
        let output = folder.path().join("trimmed.mp4");
        let (first, last) = trim(&source, &output, 0, 30_000_000).unwrap();
        assert!(first >= 0 && last > first && last <= 31_000_000);
        assert!(std::fs::metadata(&output).unwrap().len() > 1000);
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _media = Media::start().unwrap();
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(output.as_os_str()), None).unwrap();
            let format = MFCreateMediaType().unwrap();
            format.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
            format.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).unwrap();
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            reader.SetCurrentMediaType(stream, None, &format).unwrap();
            let mut frames = 0;
            loop {
                let (mut flags, mut sample) = (0, None);
                reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample)).unwrap();
                if sample.is_some() { frames += 1; }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 { break; }
            }
            assert!(frames >= 30, "decoded only {frames} frames");
            let audio = MFCreateMediaType().unwrap();
            audio.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).unwrap();
            audio.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM).unwrap();
            let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
            reader.SetCurrentMediaType(stream, None, &audio).unwrap();
            let mut bytes = 0;
            let mut peak = 0i32;
            loop {
                let (mut flags, mut sample) = (0, None);
                reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample)).unwrap();
                if let Some(sample) = sample {
                    bytes += sample.GetTotalLength().unwrap();
                    let buffer = sample.ConvertToContiguousBuffer().unwrap();
                    let (mut data, mut len) = (std::ptr::null_mut(), 0);
                    buffer.Lock(&mut data, None, Some(&mut len)).unwrap();
                    for pair in std::slice::from_raw_parts(data, len as usize).chunks_exact(2) {
                        peak = peak.max(i16::from_le_bytes([pair[0], pair[1]]).unsigned_abs() as i32);
                    }
                    buffer.Unlock().unwrap();
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 { break; }
            }
            assert!(bytes > 0, "no decoded audio");
            if std::env::var_os("OPNOVERLAY_REQUIRE_TONE").is_some() { assert!(peak > 10, "audio signal missing: peak {peak}"); }
        }
        let original = std::fs::read(&output).unwrap();
        assert!(trim(&source, &output, 0, 30_000_000).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), original);
        let later = folder.path().join("later.mp4");
        let (first, last) = trim(&source, &later, 11_230_000, 34_560_000).unwrap();
        assert!((11_230_000..=11_430_000).contains(&first), "start shifted to {first}");
        assert!(last > first && last <= 34_560_000);
    }
}
