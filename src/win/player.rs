use std::path::Path;
use windows::core::{PCWSTR, w};
use windows::Win32::Foundation::HWND;
use windows::Win32::Media::MediaFoundation::{IMFPMediaPlayer, IMFPMediaPlayerCallback, MFPCreateMediaPlayer, MFP_CREATION_OPTIONS, MFP_MEDIAPLAYER_STATE_PAUSED, MFP_MEDIAPLAYER_STATE_PLAYING, MFP_POSITIONTYPE_100NS};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PropVariantClear, PropVariantToInt64};
use windows::Win32::System::Variant::VT_I8;
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER, WINDOW_EX_STYLE, WS_CHILD, WS_CLIPSIBLINGS, WS_VISIBLE};

pub struct Player {
    window: HWND,
    player: IMFPMediaPlayer,
    bounds: Option<(i32, i32, i32, i32)>,
}

impl Player {
    pub fn open(parent: isize, path: &Path) -> Result<Self, String> {
        use std::os::windows::ffi::OsStrExt;
        if !path.is_absolute() || !path.is_file() { return Err("Видеофайл не найден.".into()); }
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let window = CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"), w!("opnoverlay video"), WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS, 0, 0, 1, 1, Some(HWND(parent as *mut _)), None, None, None).map_err(|e| e.to_string())?;
            let mut player = None;
            if let Err(error) = MFPCreateMediaPlayer(PCWSTR(path.as_ptr()), true, MFP_CREATION_OPTIONS(0), None::<&IMFPMediaPlayerCallback>, Some(window), Some(&mut player)) {
                let _ = DestroyWindow(window);
                return Err(error.to_string());
            }
            match player {
                Some(player) => Ok(Self { window, player, bounds: None }),
                None => { let _ = DestroyWindow(window); Err("Не удалось создать проигрыватель.".into()) }
            }
        }
    }

    pub fn set_bounds(&mut self, bounds: (i32, i32, i32, i32)) {
        if self.bounds == Some(bounds) {
            if !self.playing() { let _ = unsafe { self.player.UpdateVideo() }; }
            return;
        }
        let (x, y, width, height) = bounds;
        unsafe {
            let _ = SetWindowPos(self.window, None, x, y, width.max(1), height.max(1), SWP_NOACTIVATE | SWP_NOZORDER);
            let _ = self.player.UpdateVideo();
        }
        self.bounds = Some(bounds);
    }

    pub fn playing(&self) -> bool {
        unsafe { self.player.GetState().is_ok_and(|state| state == MFP_MEDIAPLAYER_STATE_PLAYING) }
    }

    pub fn toggle(&self) -> Result<(), String> {
        unsafe {
            let state = self.player.GetState().map_err(|e| e.to_string())?;
            if state == MFP_MEDIAPLAYER_STATE_PLAYING { self.player.Pause() }
            else if state == MFP_MEDIAPLAYER_STATE_PAUSED { self.player.Play() }
            else { self.player.Play() }
        }.map_err(|e| e.to_string())
    }

    fn position_value(&self, duration: bool) -> Option<f64> {
        unsafe {
            let mut value = if duration { self.player.GetDuration(&MFP_POSITIONTYPE_100NS) } else { self.player.GetPosition(&MFP_POSITIONTYPE_100NS) }.ok()?;
            let result = PropVariantToInt64(&value).ok().map(|v| v as f64 / 10_000_000.0);
            let _ = PropVariantClear(&mut value);
            result.filter(|v| v.is_finite() && *v >= 0.0)
        }
    }

    pub fn position(&self) -> Option<f64> { self.position_value(false) }
    pub fn duration(&self) -> Option<f64> { self.position_value(true) }

    pub fn seek(&self, seconds: f64) -> Result<(), String> {
        if !seconds.is_finite() || seconds < 0.0 { return Err("Неверная позиция видео.".into()); }
        let mut value = PROPVARIANT::default();
        unsafe {
            (*value.Anonymous.Anonymous).vt = VT_I8;
            (*value.Anonymous.Anonymous).Anonymous.hVal = (seconds * 10_000_000.0) as i64;
            self.player.SetPosition(&MFP_POSITIONTYPE_100NS, &value).map_err(|e| e.to_string())
        }
    }

    pub fn set_volume(&self, volume: f32) -> Result<(), String> {
        unsafe { self.player.SetVolume(volume.clamp(0.0, 1.0)) }.map_err(|e| e.to_string())
    }

    pub fn rate(&self) -> Result<f32, String> {
        unsafe { self.player.GetRate() }.map_err(|e| e.to_string())
    }

    pub fn set_rate(&self, rate: f32) -> Result<(), String> {
        if !rate.is_finite() || rate <= 0.0 { return Err("Неверная скорость видео.".into()); }
        unsafe { self.player.SetRate(rate) }.map_err(|e| e.to_string())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        unsafe {
            let _ = self.player.Shutdown();
            let _ = DestroyWindow(self.window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::time::{Duration, Instant};
    use windows::core::BOOL;
    use windows::Win32::Media::MediaFoundation::MFP_MEDIAPLAYER_STATE_STOPPED;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};

    #[test]
    #[ignore]
    fn direct_player_emits_audio_to_default_device() {
        use crate::audio::{Input, InputKind};
        use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPED};
        let path = std::path::PathBuf::from(std::env::var_os("OPNOVERLAY_CHECK_FILE").unwrap());
        let capture = Input::start(InputKind::Application(std::process::id()), None, 1.0, None);
        std::thread::sleep(Duration::from_millis(500));
        assert!(capture.shared.error().is_none(), "{:?}", capture.shared.error());
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let parent = CreateWindowExW(WINDOW_EX_STYLE::default(), w!("STATIC"), w!("test"), WS_OVERLAPPED, 0, 0, 64, 64, None, None, None, None).unwrap();
            let player = Player::open(parent.0 as isize, &path).unwrap();
            player.set_volume(0.03).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut message = MSG::default();
            let mut peak = 0f32;
            while Instant::now() < deadline {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                peak = peak.max(capture.shared.peak());
                if player.playing() && peak > 0.001 { break; }
                std::thread::sleep(Duration::from_millis(10));
            }
            println!("playing={} process_loopback_peak={peak:.5}", player.playing());
            assert!(player.playing());
            assert!(peak > 0.001, "no playback audio: {peak}");
            drop(player);
            let _ = DestroyWindow(parent);
            CoUninitialize();
        }
    }

    #[test]
    #[ignore]
    fn playback_emits_audio_to_an_isolated_virtual_endpoint() {
        use windows::core::HSTRING;
        use windows::Win32::Media::MediaFoundation::{MFCreateAudioRendererActivate, MFCreateSourceReaderFromURL, MFMediaType_Audio, MF_AUDIO_RENDERER_ATTRIBUTE_ENDPOINT_ID, MF_AUDIO_RENDERER_ATTRIBUTE_FLAGS, MF_AUDIO_RENDERER_ATTRIBUTE_FLAGS_NOPERSIST, MF_MT_MAJOR_TYPE};
        use crate::audio::{Input, InputKind};
        use wasapi::{DeviceEnumerator, Direction};

        let path = Path::new(&std::env::var_os("OPNOVERLAY_CHECK_FILE").expect("test video is required")).to_owned();
        let devices = crate::platform::audio::list_devices(InputKind::System);
        let endpoint = devices.iter().find(|device| device.name.contains("Steam Streaming Speakers")).expect("virtual Steam endpoint not available");
        let default_id = {
            let handle = std::thread::spawn(|| {
                wasapi::initialize_mta().ok().unwrap();
                DeviceEnumerator::new().unwrap().get_default_device(&Direction::Render).unwrap().get_id().unwrap()
            });
            handle.join().unwrap()
        };
        assert_ne!(endpoint.id, default_id, "test endpoint must not be the system default");
        let capture = Input::start(InputKind::System, Some(endpoint.id.clone()), 1.0, None);
        std::thread::sleep(Duration::from_millis(500));
        assert!(capture.shared.error().is_none(), "capture error: {:?}", capture.shared.error());
        assert!(capture.shared.peak() < 0.01, "virtual endpoint is already in use");
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let mut player = None;
            MFPCreateMediaPlayer(PCWSTR::null(), false, MFP_CREATION_OPTIONS(0), None::<&IMFPMediaPlayerCallback>, None, Some(&mut player)).unwrap();
            let player = player.unwrap();
            let mut item = None;
            player.CreateMediaItemFromURL(PCWSTR(wide.as_ptr()), true, 0, Some(&mut item)).unwrap();
            let item = item.unwrap();
            let reader = MFCreateSourceReaderFromURL(PCWSTR(wide.as_ptr()), None).unwrap();
            let mut audio_index = None;
            for index in 0..item.GetNumberOfStreams().unwrap() {
                let media_type = reader.GetNativeMediaType(index, 0).unwrap();
                if media_type.GetGUID(&MF_MT_MAJOR_TYPE).unwrap() == MFMediaType_Audio { audio_index = Some(index); }
                else { item.SetStreamSelection(index, false).unwrap(); }
            }
            let audio_index = audio_index.expect("no audio stream");
            let activate = MFCreateAudioRendererActivate().unwrap();
            activate.SetString(&MF_AUDIO_RENDERER_ATTRIBUTE_ENDPOINT_ID, &HSTRING::from(&endpoint.id)).unwrap();
            activate.SetUINT32(&MF_AUDIO_RENDERER_ATTRIBUTE_FLAGS, MF_AUDIO_RENDERER_ATTRIBUTE_FLAGS_NOPERSIST).unwrap();
            item.SetStreamSink(audio_index, &activate).unwrap();
            player.SetMediaItem(&item).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut message = MSG::default();
            while player.GetState().ok() != Some(MFP_MEDIAPLAYER_STATE_STOPPED) && Instant::now() < deadline {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            assert_eq!(player.GetState().unwrap(), MFP_MEDIAPLAYER_STATE_STOPPED);
            player.Play().unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut peak = 0.0f32;
            while Instant::now() < deadline {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                peak = peak.max(capture.shared.peak());
                std::thread::sleep(Duration::from_millis(10));
            }
            player.Shutdown().unwrap();
            drop(reader);
            drop(item);
            drop(player);
            CoUninitialize();
            assert!(peak > 0.03, "virtual output contained no expected audio, peak={peak}");
            println!("virtual playback peak={peak:.3}");
        }
    }

    #[test]
    #[ignore]
    fn muted_player_selects_audio_and_advances() {
        let path = Path::new(&std::env::var_os("OPNOVERLAY_CHECK_FILE").expect("test video is required")).to_owned();
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let mut player = None;
            MFPCreateMediaPlayer(PCWSTR::null(), false, MFP_CREATION_OPTIONS(0), None::<&IMFPMediaPlayerCallback>, None, Some(&mut player)).unwrap();
            let player = player.unwrap();
            player.SetVolume(0.0).unwrap();
            assert_eq!(player.GetVolume().unwrap(), 0.0);
            let mut item = None;
            player.CreateMediaItemFromURL(PCWSTR(wide.as_ptr()), true, 0, Some(&mut item)).unwrap();
            let item = item.unwrap();
            let mut has_audio = BOOL(0);
            let mut selected = BOOL(0);
            item.HasAudio(Some(&mut has_audio), Some(&mut selected)).unwrap();
            assert!(has_audio.as_bool() && selected.as_bool());
            player.SetMediaItem(&item).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut message = MSG::default();
            while player.GetState().ok() != Some(MFP_MEDIAPLAYER_STATE_STOPPED) && Instant::now() < deadline {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            assert_eq!(player.GetState().unwrap(), MFP_MEDIAPLAYER_STATE_STOPPED);
            player.Play().unwrap();
            let deadline = Instant::now() + Duration::from_millis(600);
            while Instant::now() < deadline {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            assert_eq!(player.GetState().unwrap(), MFP_MEDIAPLAYER_STATE_PLAYING);
            let mut position = player.GetPosition(&MFP_POSITIONTYPE_100NS).unwrap();
            assert!(PropVariantToInt64(&position).unwrap() > 0);
            PropVariantClear(&mut position).unwrap();
            player.Shutdown().unwrap();
            drop(item);
            drop(player);
            CoUninitialize();
        }
    }
}
