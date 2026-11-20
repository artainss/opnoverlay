use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Diagnostics::Etw::{CONTROLTRACE_HANDLE, ControlTraceW, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_PROPERTIES};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct CsvColumns {
    process: usize,
    swap_chain: usize,
    displayed: usize,
}

fn csv_fields(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut chars = line.trim_end_matches(['\r', '\n']).chars().peekable();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => { field.push('"'); chars.next(); }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(ch),
        }
    }
    if quoted { return None; }
    fields.push(field);
    Some(fields)
}

impl CsvColumns {
    pub fn from_header(header: &str) -> Option<Self> {
        let fields = csv_fields(header)?;
        Some(Self {
            process: fields.iter().position(|field| field == "ProcessID")?,
            swap_chain: fields.iter().position(|field| field == "SwapChainAddress")?,
            displayed: fields.iter().position(|field| field == "DisplayedTime")?,
        })
    }

    pub fn frame(&self, line: &str, target: u32) -> Option<(String, f64)> {
        let fields = csv_fields(line)?;
        if fields.get(self.process)?.parse::<u32>().ok()? != target { return None; }
        let swap_chain = fields.get(self.swap_chain)?.clone();
        let milliseconds = fields.get(self.displayed)?.parse::<f64>().ok()?;
        if !milliseconds.is_finite() || milliseconds <= 0.0 || milliseconds > 10_000.0 { return None; }
        Some((swap_chain, milliseconds))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FrameTiming {
    pub frame_ms: f64,
}

impl FrameTiming {
    pub fn fps(self) -> f64 { 1000.0 / self.frame_ms }
}

#[derive(Default)]
struct FrameBucket {
    frames: u32,
    total_ms: f64,
}

impl FrameBucket {
    fn push(&mut self, milliseconds: f64) {
        self.frames += 1;
        self.total_ms += milliseconds;
    }

    fn timing(&self) -> Option<FrameTiming> {
        (self.frames > 0).then(|| FrameTiming { frame_ms: self.total_ms / f64::from(self.frames) })
    }
}

#[derive(Clone, Copy)]
pub struct FrameSample {
    at: Instant,
    process: u32,
    timing: Option<FrameTiming>,
}

impl FrameSample {
    pub fn current(self, fixed_target: Option<u32>) -> Option<FrameTiming> {
        if self.at.elapsed() >= Duration::from_secs(4) || fixed_target.or_else(foreground_process).is_some_and(|pid| pid != self.process) {
            return None;
        }
        self.timing
    }
}

pub struct FpsMonitor {
    stop: Arc<AtomicBool>,
    samples: mpsc::Receiver<FrameSample>,
    trace: Arc<Mutex<Option<(String, Child)>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl FpsMonitor {
    pub fn start(fixed_target: Option<u32>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let trace = Arc::new(Mutex::new(None::<(String, Child)>));
        let worker_trace = trace.clone();
        let (tx, samples) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let mut last_target = None;
            while !worker_stop.load(Ordering::Acquire) {
                if let Some(pid) = fixed_target.or_else(foreground_process) { last_target = Some(pid); }
                let Some(target) = last_target else { thread::sleep(Duration::from_secs(1)); continue };
                let generation = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
                let session = format!("opnoverlay-fps-{}-{generation}", std::process::id());
                let report = |timing| {
                    let _ = tx.try_send(FrameSample { at: Instant::now(), process: target, timing });
                };
                let Some(executable) = presentmon_path() else {
                    report(None);
                    break;
                };
                let started = Instant::now();
                let mut active = worker_trace.lock().unwrap();
                if worker_stop.load(Ordering::Acquire) { break; }
                let mut child = match Command::new(executable)
                    .args(["--process_id", &target.to_string(), "--output_stdout", "--no_console_stats", "--v2_metrics", "--exclude_dropped", "--no_track_gpu", "--no_track_input", "--timed", "6", "--terminate_after_timed", "--terminate_on_proc_exit", "--session_name", &session])
                    .creation_flags(0x08000000)
                    .stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null()).spawn() {
                        Ok(child) => child,
                        Err(_) => {
                            drop(active);
                            report(None);
                            thread::sleep(Duration::from_secs(3));
                            continue;
                        }
                    };
                let output = child.stdout.take();
                *active = Some((session, child));
                drop(active);
                let mut had_frames = false;
                if let Some(output) = output {
                    let mut lines = BufReader::new(output).lines();
                    let columns = lines.next().and_then(Result::ok).and_then(|header| CsvColumns::from_header(&header));
                    if let Some(columns) = columns {
                        let mut buckets: HashMap<String, FrameBucket> = HashMap::new();
                        let mut last_report = Instant::now();
                        for line in lines.map_while(Result::ok) {
                            if worker_stop.load(Ordering::Acquire) { break; }
                            if let Some((chain, duration)) = columns.frame(&line, target) {
                                had_frames = true;
                                buckets.entry(chain).or_default().push(duration);
                            }
                            if last_report.elapsed() >= Duration::from_secs(1) {
                                let timing = buckets.values().max_by_key(|bucket| bucket.frames).and_then(FrameBucket::timing);
                                report(timing);
                                buckets.clear();
                                last_report = Instant::now();
                            }
                        }
                    }
                }
                let success = loop {
                    let mut active = worker_trace.lock().unwrap();
                    let Some((_, child)) = active.as_mut() else { return };
                    match child.try_wait() {
                        Ok(Some(status)) => { *active = None; break status.success(); }
                        Err(_) => {
                            let (name, child) = active.take().unwrap();
                            drop(active);
                            stop_trace(name, child);
                            break false;
                        }
                        Ok(None) => {}
                    }
                    drop(active);
                    thread::sleep(Duration::from_millis(20));
                };
                if !success || !had_frames { report(None); }
                let until = Instant::now() + retry_delay(success, started.elapsed(), had_frames);
                while !worker_stop.load(Ordering::Acquire) && Instant::now() < until {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Self { stop, samples, trace, worker: Some(worker) }
    }

    pub fn latest(&self) -> Option<FrameSample> {
        let mut latest = None;
        while let Ok(sample) = self.samples.try_recv() { latest = Some(sample); }
        latest
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        let active = self.trace.lock().unwrap().take();
        if let Some((name, child)) = active { stop_trace(name, child); }
    }
}

fn retry_delay(success: bool, ran_for: Duration, had_frames: bool) -> Duration {
    if !success { Duration::from_secs(10) }
    else if ran_for < Duration::from_secs(2) { Duration::from_secs(2) }
    else if !had_frames { Duration::from_secs(5) }
    else { Duration::from_millis(300) }
}

impl Drop for FpsMonitor {
    fn drop(&mut self) {
        self.stop();
        let _ = self.worker.take();
    }
}

fn stop_trace(name: String, mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    #[repr(C)]
    struct TraceProperties {
        properties: EVENT_TRACE_PROPERTIES,
        session_name: [u16; 1024],
        file_name: [u16; 1024],
    }
    let mut trace = TraceProperties { properties: EVENT_TRACE_PROPERTIES::default(), session_name: [0; 1024], file_name: [0; 1024] };
    trace.properties.Wnode.BufferSize = size_of::<TraceProperties>() as u32;
    trace.properties.LoggerNameOffset = std::mem::offset_of!(TraceProperties, session_name) as u32;
    trace.properties.LogFileNameOffset = std::mem::offset_of!(TraceProperties, file_name) as u32;
    unsafe { let _ = ControlTraceW(CONTROLTRACE_HANDLE::default(), PCWSTR(name.as_ptr()), &mut trace.properties, EVENT_TRACE_CONTROL_STOP); }
}

fn presentmon_path() -> Option<PathBuf> {
    let sibling = std::env::current_exe().ok()?.parent()?.join("PresentMon.exe");
    if sibling.is_file() { return Some(sibling); }
    #[cfg(debug_assertions)]
    {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("third-party/presentmon/PresentMon.exe");
        if source.is_file() { return Some(source); }
    }
    None
}

fn foreground_process() -> Option<u32> {
    window_process(unsafe { GetForegroundWindow() })
}

pub fn window_process(window: HWND) -> Option<u32> {
    if window.0.is_null() { return None; }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)); }
    (pid != 0 && pid != unsafe { GetCurrentProcessId() }).then_some(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_displayed_frames_for_only_the_selected_process() {
        let columns = CsvColumns::from_header("Application,ProcessID,SwapChainAddress,DisplayedTime").unwrap();
        assert_eq!(columns.frame("\"game, test.exe\",42,0x123,16.6667", 42), Some(("0x123".into(), 16.6667)));
        assert!(columns.frame("game.exe,7,0x123,16.6", 42).is_none());
        assert!(columns.frame("game.exe,42,0x123,NA", 42).is_none());
        assert!(columns.frame("game.exe,42,0x123,0", 42).is_none());
        assert!(columns.frame("game.exe,42,0x123,NaN", 42).is_none());
        assert!(columns.frame("\"unfinished,42,0x123,16.6", 42).is_none());
        assert!(CsvColumns::from_header("Application,FrameTime").is_none());
    }

    #[test]
    fn fps_uses_total_frame_time_instead_of_averaging_reciprocals() {
        let mut bucket = FrameBucket::default();
        assert!(bucket.timing().is_none());
        bucket.push(10.0);
        bucket.push(30.0);
        let timing = bucket.timing().unwrap();
        assert_eq!(timing.frame_ms, 20.0);
        assert_eq!(timing.fps(), 50.0);
    }

    #[test]
    fn a_frame_sample_is_valid_only_for_its_target_and_while_fresh() {
        let sample = FrameSample {
            at: Instant::now(),
            process: 42,
            timing: Some(FrameTiming { frame_ms: 16.0 }),
        };
        assert_eq!(sample.current(Some(42)).unwrap().frame_ms, 16.0);
        assert!(sample.current(Some(43)).is_none());
        let expired = FrameSample { at: sample.at - Duration::from_secs(5), ..sample };
        assert!(expired.current(Some(42)).is_none());
    }

    #[test]
    fn failed_or_immediately_exited_traces_are_not_restarted_in_a_tight_loop() {
        assert_eq!(retry_delay(false, Duration::ZERO, false), Duration::from_secs(10));
        assert_eq!(retry_delay(true, Duration::ZERO, false), Duration::from_secs(2));
        assert_eq!(retry_delay(true, Duration::from_secs(6), false), Duration::from_secs(5));
        assert_eq!(retry_delay(true, Duration::from_secs(6), true), Duration::from_millis(300));
    }

    #[test]
    #[ignore]
    fn captures_displayed_fps_and_stops_without_leaving_a_worker() {
        let target = std::env::var("OPNOVERLAY_FPS_TEST_PID").unwrap().parse().unwrap();
        let mut monitor = FpsMonitor::start(Some(target));
        let until = Instant::now() + Duration::from_secs(15);
        let mut measured = false;
        while Instant::now() < until {
            if let Some(FrameSample { timing: Some(timing), .. }) = monitor.latest() {
                assert!(timing.fps() > 0.0 && timing.fps() < 1000.0);
                println!("Displayed FPS: {:.2}, displayed frame time: {:.2} ms", timing.fps(), timing.frame_ms);
                measured = true;
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        monitor.stop();
        monitor.worker.take().unwrap().join().unwrap();
        assert!(measured, "no displayed FPS samples for isolated test process");
    }
}

