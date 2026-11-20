use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use windows::Win32::Foundation::{FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::core::PWSTR;

use crate::audio::{InputKind, InputShared};

pub const MAX_SELECTED_INPUTS: usize = 8;
static WORKERS: AtomicUsize = AtomicUsize::new(0);

struct WorkerSlot;

impl WorkerSlot {
    fn acquire() -> std::io::Result<Self> {
        WORKERS.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < MAX_SELECTED_INPUTS).then_some(n + 1))
            .map(|_| Self)
            .map_err(|_| std::io::Error::other("Previous audio sources are still closing"))
    }
}

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessId {
    pub pid: u32,
    pub created: u64,
}

pub struct SelectedProcess {
    pub id: ProcessId,
    pub path: PathBuf,
    handle: OwnedHandle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureState {
    Starting,
    Active,
    Exited,
    Failed(String),
}

pub struct SelectedInput {
    pub muted: bool,
    pub volume: f32,
    pub reported: bool,
    pub shared: Arc<InputShared>,
    pub process: Arc<SelectedProcess>,
    state: Arc<Mutex<CaptureState>>,
}

impl SelectedInput {
    pub fn start(process: Arc<SelectedProcess>, gain: f32, ctx: eframe::egui::Context) -> std::io::Result<Self> {
        let slot = WorkerSlot::acquire()?;
        let shared = Arc::new(InputShared::new(gain, Some(ctx.clone())));
        let state = Arc::new(Mutex::new(CaptureState::Starting));
        let (worker_process, worker_shared, worker_state) = (process.clone(), shared.clone(), state.clone());
        thread::Builder::new().name("selected-application-audio".into()).spawn(move || {
            let _slot = slot;
            let result = wasapi::initialize_mta().ok().map_err(|error| -> Box<dyn std::error::Error> { Box::new(error) })
                .and_then(|()| super::stream(InputKind::Application(worker_process.id.pid), None, &worker_shared, false, |running| {
                    let alive = !worker_shared.stopped() && worker_process.alive()?;
                    if running && alive {
                        let mut state = worker_state.lock().unwrap();
                        if *state == CaptureState::Starting {
                            *state = CaptureState::Active;
                            ctx.request_repaint();
                        }
                    }
                    Ok(alive)
                }));
            worker_shared.discard_samples();
            *worker_state.lock().unwrap() = match result {
                Ok(()) => CaptureState::Exited,
                Err(error) => CaptureState::Failed(error.to_string()),
            };
            ctx.request_repaint();
        })?;
        Ok(Self { muted: false, volume: gain, reported: false, shared, process, state })
    }

    pub fn state(&self) -> CaptureState {
        self.state.lock().unwrap().clone()
    }

    pub fn can_reconnect(&self) -> bool {
        matches!(*self.state.lock().unwrap(), CaptureState::Exited | CaptureState::Failed(_))
    }
}

impl Drop for SelectedInput {
    fn drop(&mut self) {
        self.shared.request_stop();
        self.shared.discard_samples();
    }
}

impl SelectedProcess {
    pub fn open(pid: u32) -> windows::core::Result<Self> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, false, pid)? };
        let owned = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user)? };
        let created = u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime);
        let mut path = vec![0u16; 32_768];
        let mut len = path.len() as u32;
        unsafe { QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut len)? };
        let path = std::ffi::OsString::from_wide(&path[..len as usize]).into();
        Ok(Self { id: ProcessId { pid, created }, path, handle: owned })
    }

    pub fn alive(&self) -> windows::core::Result<bool> {
        match unsafe { WaitForSingleObject(HANDLE(self.handle.as_raw_handle()), 0) } {
            WAIT_OBJECT_0 => Ok(false),
            WAIT_TIMEOUT => Ok(true),
            _ => Err(windows::core::Error::from_thread()),
        }
    }
}

pub fn process_snapshot() -> windows::core::Result<Vec<PROCESSENTRY32W>> {
    use windows::Win32::Foundation::ERROR_NO_MORE_FILES;

    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? };
    let _owned = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let mut entry = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    unsafe { Process32FirstW(handle, &mut entry)? };
    let mut entries = Vec::new();
    loop {
        entries.push(entry);
        match unsafe { Process32NextW(handle, &mut entry) } {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_NO_MORE_FILES.to_hresult() => return Ok(entries),
            Err(error) => return Err(error),
        }
    }
}

pub fn overlaps(a: ProcessId, b: ProcessId) -> windows::core::Result<bool> {
    let entries = process_snapshot()?;
    Ok(trees_may_overlap(a, b, &entries))
}

fn trees_may_overlap(a: ProcessId, b: ProcessId, entries: &[PROCESSENTRY32W]) -> bool {
    if a.pid == b.pid {
        return true;
    }
    let (older, younger) = if a.created <= b.created { (a, b) } else { (b, a) };
    let mut pid = younger.pid;
    for _ in 0..entries.len() {
        let Some(entry) = entries.iter().find(|entry| entry.th32ProcessID == pid) else { return true };
        let parent = entry.th32ParentProcessID;
        if parent == older.pid {
            return true;
        }
        if parent == 0 {
            return false;
        }
        pid = parent;
    }
    true
}

pub fn same_image(a: &Path, b: &Path) -> bool {
    let a: Vec<u16> = a.as_os_str().encode_wide().collect();
    let b: Vec<u16> = b.as_os_str().encode_wide().collect();
    unsafe { CompareStringOrdinal(&a, &b, true) == CSTR_EQUAL }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore]
    fn captures_real_game_audio() {
        use std::time::{Duration, Instant};
        let pid: u32 = std::env::var("OPNOVERLAY_TEST_GAME_PID").unwrap().parse().unwrap();
        let process = Arc::new(SelectedProcess::open(pid).expect("game process not accessible"));
        let input = SelectedInput::start(process, 1.0, eframe::egui::Context::default()).expect("game audio capture failed");
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut maximum = 0.0f32;
        while Instant::now() < deadline {
            maximum = maximum.max(input.shared.peak());
            if maximum > 0.00001 { break; }
            if let CaptureState::Failed(error) = input.state() { panic!("audio capture failed: {error}"); }
            std::thread::sleep(Duration::from_millis(25));
        }
        println!("game audio peak: {maximum}; state: {:?}", input.state());
        assert!(maximum > 0.00001, "no game audio captured");
    }

    #[test]
    #[ignore]
    fn records_real_game_audio() {
        use std::time::Duration;
        let pid: u32 = std::env::var("OPNOVERLAY_TEST_GAME_PID").unwrap().parse().unwrap();
        let process = Arc::new(SelectedProcess::open(pid).expect("game process not accessible"));
        let input = SelectedInput::start(process, 1.0, eframe::egui::Context::default()).expect("game audio capture failed");
        let title = std::env::var("OPNOVERLAY_TEST_GAME_TITLE").unwrap();
        let source = crate::capture::list_sources().into_iter().find(|source| matches!(source.id, crate::capture::SourceId::Window(_)) && source.label.contains(&title)).expect("game source missing");
        let session = crate::capture::Session::start(crate::capture::SessionOptions { source: source.id, fps: 30, cursor: true, desktop_duplication: false, preview_fps: 0, record: Some(crate::capture::RecordConfig { quality: crate::config::Quality::Medium, custom_bitrate_mbps: 12, resolution: crate::config::Resolution::Native, display_canvas: None, audio: true, separate_audio_tracks: false, region: None, overlay: None }) }, None).expect("game video capture failed");
        let out = std::path::PathBuf::from(std::env::var_os("OPNOVERLAY_TEST_GAME_FILE").unwrap());
        session.shared.outputs.lock().unwrap().start_file(out);
        let shared = session.shared.clone();
        let mixer = crate::audio::Mixer::start(vec![input.shared.clone()], shared.encoder_ready.clone(), shared.clock.clone(), move |pcm| {
            if let Some(encoder) = shared.encoder.lock().unwrap().as_ref() { encoder.push_audio_track(0, pcm); }
        });
        std::thread::sleep(Duration::from_secs(8));
        drop(mixer);
        let (encoder, error) = session.stop();
        assert!(error.is_none(), "capture failed: {error:?}");
        let encoder = encoder.expect("encoder missing");
        encoder.finish().expect("recording missing").finish().expect("finalizing failed");
    }

    #[test]
    fn process_tree_overlap_rejects_duplicates_descendants_and_unknown_ancestry() {
        let entry = |pid, parent| PROCESSENTRY32W { th32ProcessID: pid, th32ParentProcessID: parent, ..Default::default() };
        let root = ProcessId { pid: 10, created: 1 };
        let child = ProcessId { pid: 11, created: 2 };
        let independent = ProcessId { pid: 20, created: 3 };
        let entries = [entry(10, 0), entry(11, 10), entry(20, 0)];
        assert!(trees_may_overlap(root, root, &entries));
        assert!(trees_may_overlap(root, child, &entries));
        assert!(trees_may_overlap(child, root, &entries));
        assert!(!trees_may_overlap(root, independent, &entries));
        assert!(trees_may_overlap(root, independent, &entries[..2]));
    }

    #[test]
    fn image_matching_uses_the_entire_path() {
        assert!(same_image(Path::new(r"C:\One\App.EXE"), Path::new(r"c:\one\app.exe")));
        assert!(!same_image(Path::new(r"C:\One\app.exe"), Path::new(r"C:\Two\app.exe")));
        assert!(!same_image(Path::new(r"C:\One\app.exe"), Path::new("app.exe")));
    }

    #[test]
    fn process_identity_remains_readable_after_the_child_exits() {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "win::audio::selected::tests::image_matching_uses_the_entire_path"])
            .creation_flags(0x0000_0800)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let process = SelectedProcess::open(child.id()).unwrap();
        assert_eq!(process.id.pid, child.id());
        assert_ne!(process.id.created, 0);
        assert!(same_image(&process.path, &std::env::current_exe().unwrap()));
        assert!(child.wait().unwrap().success());
        assert!(!process.alive().unwrap());
    }
}
