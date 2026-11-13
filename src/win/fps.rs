use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

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

pub fn average_fps(frame_times_ms: &[f64]) -> Option<f64> {
    if frame_times_ms.is_empty() || frame_times_ms.iter().any(|time| !time.is_finite() || *time <= 0.0) { return None; }
    Some(1000.0 * frame_times_ms.len() as f64 / frame_times_ms.iter().sum::<f64>())
}

pub struct FpsMonitor {
    stop: Arc<AtomicBool>,
    samples: mpsc::Receiver<(Instant, Option<f64>)>,
    worker: Option<thread::JoinHandle<()>>,
}

impl FpsMonitor {
    pub fn start() -> Self {
        Self::start_for_target(None)
    }

    fn start_for_target(fixed_target: Option<u32>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (tx, samples) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let mut last_target = None;
            while !worker_stop.load(Ordering::Acquire) {
                if let Some(pid) = fixed_target.or_else(foreground_process) { last_target = Some(pid); }
                let Some(target) = last_target else { thread::sleep(Duration::from_secs(1)); continue };
                let generation = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
                let session = format!("opnoverlay-fps-{}-{generation}", std::process::id());
                let Some(executable) = presentmon_path() else {
                    let _ = tx.try_send((Instant::now(), None));
                    break;
                };
                let started = Instant::now();
                let mut child = match Command::new(executable)
                    .args(["--process_id", &target.to_string(), "--output_stdout", "--no_console_stats", "--v2_metrics", "--exclude_dropped", "--no_track_gpu", "--no_track_input", "--timed", "6", "--terminate_after_timed", "--terminate_on_proc_exit", "--session_name", &session])
                    .creation_flags(0x08000000)
                    .stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null()).spawn() {
                        Ok(child) => child,
                        Err(_) => { let _ = tx.try_send((Instant::now(), None)); thread::sleep(Duration::from_secs(3)); continue; }
                    };
                let mut had_frames = false;
                if let Some(output) = child.stdout.take() {
                    let mut lines = BufReader::new(output).lines();
                    let columns = lines.next().and_then(Result::ok).and_then(|header| CsvColumns::from_header(&header));
                    if let Some(columns) = columns {
                        let mut buckets: HashMap<String, Vec<f64>> = HashMap::new();
                        let mut last_report = Instant::now();
                        for line in lines.map_while(Result::ok) {
                            if worker_stop.load(Ordering::Acquire) { break; }
                            if let Some((chain, duration)) = columns.frame(&line, target) {
                                had_frames = true;
                                let values = buckets.entry(chain).or_default();
                                if values.len() < 1000 { values.push(duration); }
                            }
                            if last_report.elapsed() >= Duration::from_secs(1) {
                                let fps = buckets.values().max_by_key(|values| values.len()).and_then(|values| average_fps(values));
                                let _ = tx.try_send((Instant::now(), fps));
                                buckets.clear();
                                last_report = Instant::now();
                            }
                        }
                    }
                }
                let success = child.wait().is_ok_and(|status| status.success());
                let _ = tx.try_send((Instant::now(), None));
                let until = Instant::now() + retry_delay(success, started.elapsed(), had_frames);
                while !worker_stop.load(Ordering::Acquire) && Instant::now() < until {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Self { stop, samples, worker: Some(worker) }
    }

    pub fn latest(&self) -> Option<(Instant, Option<f64>)> {
        let mut latest = None;
        while let Ok(sample) = self.samples.try_recv() { latest = Some(sample); }
        latest
    }
}

fn retry_delay(success: bool, ran_for: Duration, had_frames: bool) -> Duration {
    if !success { Duration::from_secs(10) }
    else if ran_for < Duration::from_secs(2) { Duration::from_secs(2) }
    else if !had_frames { Duration::from_secs(5) }
    else { Duration::from_millis(300) }
}

impl Drop for FpsMonitor {
    fn drop(&mut self) { self.stop.store(true, Ordering::Release); let _ = self.worker.take(); }
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
    let window = unsafe { GetForegroundWindow() };
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
        assert!((average_fps(&[10.0, 30.0]).unwrap() - 50.0).abs() < 0.001);
        assert!(average_fps(&[]).is_none());
        assert!(average_fps(&[10.0, 0.0]).is_none());
        assert!(average_fps(&[f64::NAN]).is_none());
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
        let mut monitor = FpsMonitor::start_for_target(Some(target));
        let until = Instant::now() + Duration::from_secs(15);
        let mut measured = false;
        while Instant::now() < until {
            if let Some((_, Some(fps))) = monitor.latest() {
                assert!(fps > 0.0 && fps < 1000.0);
                measured = true;
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        monitor.stop.store(true, Ordering::Release);
        monitor.worker.take().unwrap().join().unwrap();
        assert!(measured, "no displayed FPS samples for isolated test process");
    }
}

