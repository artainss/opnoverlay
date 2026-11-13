







use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nnnoiseless::DenoiseState;

use crate::media::Clock;

pub use crate::platform::audio::list_devices;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;

const MAX_BUFFERED: usize = SAMPLE_RATE as usize * CHANNELS / 2; 

pub const METER_FLOOR: f32 = 0.001;

const MAX_LATENCY: usize = SAMPLE_RATE as usize * CHANNELS / 10; 

const CUSHION: usize = SAMPLE_RATE as usize * CHANNELS / 100; 


const HOLD: f32 = 0.05;

const RELEASE: f32 = 0.2;

const LIMITING_HOLD: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InputKind {
    System,
    
    
    SystemExcept(u32),
    Microphone,
    
    Application(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
}


pub struct InputShared {
    gain: AtomicU32,
    peak: AtomicU32,
    denoise: AtomicBool,
    stop: AtomicBool,
    buffer: Mutex<VecDeque<f32>>,
    error: Mutex<Option<String>>,
    
    denoiser: Mutex<Option<Denoiser>>,
    
    repaint: Option<eframe::egui::Context>,
}

impl InputShared {
    fn new(gain: f32, repaint: Option<eframe::egui::Context>) -> Self {
        Self {
            gain: AtomicU32::new(gain.to_bits()),
            peak: AtomicU32::new(0),
            denoise: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            buffer: Mutex::new(VecDeque::with_capacity(MAX_BUFFERED)),
            error: Mutex::new(None),
            denoiser: Mutex::new(None),
            repaint,
        }
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn set_denoise(&self, on: bool) {
        self.denoise.store(on, Ordering::Relaxed);
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak.load(Ordering::Relaxed))
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    
    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    
    pub(crate) fn deliver(&self, mut samples: Vec<f32>) {
        let mut denoiser = self.denoiser.lock().unwrap();
        if self.denoise.load(Ordering::Relaxed) {
            samples = denoiser.get_or_insert_with(Denoiser::new).process(&samples);
        } else {
            *denoiser = None;
        }
        drop(denoiser);
        let peak = samples.iter().fold(0f32, |p, s| p.max(s.abs()));
        let mut buf = self.buffer.lock().unwrap();
        buf.extend(samples);
        let excess = buf.len().saturating_sub(MAX_BUFFERED);
        buf.drain(..excess);
        drop(buf);
        let before = f32::from_bits(self.peak.swap(peak.to_bits(), Ordering::Relaxed));
        if wakes(before, peak)
            && let Some(ctx) = &self.repaint
        {
            ctx.request_repaint();
        }
    }

    
    pub(crate) fn silent(&self) {
        self.peak.store(0, Ordering::Relaxed);
    }
}

pub struct Input {
    pub kind: InputKind,
    pub shared: Arc<InputShared>,
    thread: Option<JoinHandle<()>>,
}

impl Input {
    
    
    pub fn start(kind: InputKind, device_id: Option<String>, gain: f32, repaint: Option<eframe::egui::Context>) -> Self {
        let shared = Arc::new(InputShared::new(gain, repaint));
        let thread = thread::Builder::new()
            .name(format!("audio-{kind:?}"))
            .spawn({
                let shared = shared.clone();
                move || {
                    if let Err(e) = crate::platform::audio::capture(kind, device_id.as_deref(), &shared) {
                        *shared.error.lock().unwrap() = Some(e.to_string());
                        if let Some(ctx) = &shared.repaint { ctx.request_repaint(); }
                    }
                    shared.peak.store(0, Ordering::Relaxed);
                }
            })
            .expect("spawn audio thread");
        Self { kind, shared, thread: Some(thread) }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}



struct Denoiser {
    state: Box<DenoiseState<'static>>,
    pending: Vec<f32>,
    
    first: bool,
}

impl Denoiser {
    fn new() -> Self {
        Self { state: DenoiseState::new(), pending: Vec::new(), first: true }
    }

    
    fn process(&mut self, stereo: &[f32]) -> Vec<f32> {
        
        self.pending.extend(stereo.as_chunks::<2>().0.iter().map(|[l, r]| (l + r) * 0.5 * 32768.0));
        let mut out = Vec::with_capacity(stereo.len() + DenoiseState::FRAME_SIZE * 2);
        let mut frame = [0f32; DenoiseState::FRAME_SIZE];
        for input in self.pending.as_chunks::<{ DenoiseState::FRAME_SIZE }>().0 {
            self.state.process_frame(&mut frame, input);
            if std::mem::take(&mut self.first) {
                frame.fill(0.0);
            }
            out.extend(frame.iter().flat_map(|s| [s / 32768.0; 2]));
        }
        let done = self.pending.len() / DenoiseState::FRAME_SIZE * DenoiseState::FRAME_SIZE;
        self.pending.drain(..done);
        out
    }
}


pub struct Mixer {
    stop: Arc<AtomicBool>,
    inputs: Arc<Mutex<Vec<Arc<InputShared>>>>,
    
    limited_at: Arc<Mutex<Option<Instant>>>,
    thread: Option<JoinHandle<()>>,
}

impl Mixer {
    
    
    pub fn start(
        inputs: Vec<Arc<InputShared>>,
        ready: Arc<AtomicBool>,
        clock: Arc<Clock>,
        mut sink: impl FnMut(Vec<u8>) + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let inputs = Arc::new(Mutex::new(inputs));
        let limited_at: Arc<Mutex<Option<Instant>>> = Arc::default();
        let thread = thread::Builder::new()
            .name("audio-mixer".into())
            .spawn({
                let (stop, limited_at, inputs) = (stop.clone(), limited_at.clone(), inputs.clone());
                move || {
                    while !ready.load(Ordering::Acquire) {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    let mut current = inputs.lock().unwrap().clone();
                    current.iter().for_each(|i| i.buffer.lock().unwrap().clear());
                    let mut produced: u64 = 0;
                    let mut mix = Vec::new();
                    let mut backlogs = vec![Backlog::default(); current.len()];
                    let mut limiter = Limiter::new();
                    while !stop.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(10));
                        let due = (clock.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64;
                        let frames = due.saturating_sub(produced) as usize;
                        if frames == 0 {
                            continue;
                        }
                        produced += frames as u64;
                        let next = inputs.lock().unwrap().clone();
                        if !same_inputs(&current, &next) {
                            for input in &next {
                                if !current.iter().any(|old| Arc::ptr_eq(old, input)) {
                                    input.buffer.lock().unwrap().clear();
                                }
                            }
                            current = next;
                            backlogs = vec![Backlog::default(); current.len()];
                        }
                        mix_block(&current, frames * CHANNELS, &mut mix, &mut backlogs);
                        if limiter.process(&mut mix) {
                            *limited_at.lock().unwrap() = Some(Instant::now());
                        }
                        let pcm =
                            mix.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
                        sink(pcm.collect());
                    }
                }
            })
            .expect("spawn mixer thread");
        Self { stop, inputs, limited_at, thread: Some(thread) }
    }

    pub fn set_inputs(&self, inputs: Vec<Arc<InputShared>>) {
        *self.inputs.lock().unwrap() = inputs;
    }

    
    
    pub fn limiting(&self) -> bool {
        self.limited_at.lock().unwrap().is_some_and(|t| t.elapsed() < LIMITING_HOLD)
    }
}

fn same_inputs(a: &[Arc<InputShared>], b: &[Arc<InputShared>]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(left, right)| Arc::ptr_eq(left, right))
}

impl Drop for Mixer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}


fn wakes(before: f32, now: f32) -> bool {
    before <= METER_FLOOR && now > METER_FLOOR
}





struct Limiter {
    gain: f32,
    
    hold: u32,
    
    release: f32,
}

impl Limiter {
    fn new() -> Self {
        Self { gain: 1.0, hold: 0, release: 1.0 - (-1.0 / (RELEASE * SAMPLE_RATE as f32)).exp() }
    }

    
    fn process(&mut self, mix: &mut [f32]) -> bool {
        let mut over = false;
        for frame in mix.as_chunks_mut::<CHANNELS>().0 {
            let peak = frame.iter().fold(0f32, |p, s| p.max(s.abs()));
            
            let fit = if peak > 1.0 { 1.0 / peak } else { 1.0 };
            over |= peak > 1.0;
            if fit <= self.gain {
                
                if fit < 1.0 {
                    self.hold = (HOLD * SAMPLE_RATE as f32) as u32;
                }
                self.gain = fit;
            } else if self.hold > 0 {
                self.hold -= 1;
            } else {
                self.gain += (fit - self.gain) * self.release;
            }
            if self.gain < 1.0 {
                frame.iter_mut().for_each(|s| *s *= self.gain);
            }
        }
        over
    }
}




#[derive(Clone)]
struct Backlog {
    least: usize,
    mixed: usize,
}

impl Default for Backlog {
    fn default() -> Self {
        Self { least: usize::MAX, mixed: 0 }
    }
}

fn mix_block(inputs: &[Arc<InputShared>], samples: usize, out: &mut Vec<f32>, backlogs: &mut [Backlog]) {
    out.clear();
    out.resize(samples, 0.0);
    for (input, backlog) in inputs.iter().zip(backlogs) {
        let gain = input.gain();
        let mut buf = input.buffer.lock().unwrap();
        
        let excess = buf.len().saturating_sub(samples + MAX_LATENCY);
        buf.drain(..excess);
        let take = buf.len().min(samples);
        for (o, s) in out.iter_mut().zip(buf.drain(..take)) {
            *o += s * gain;
        }
        
        backlog.least = backlog.least.min(buf.len());
        backlog.mixed += samples;
        if backlog.mixed >= SAMPLE_RATE as usize * CHANNELS {
            let extra = backlog.least.saturating_sub(CUSHION);
            buf.drain(..extra - extra % CHANNELS);
            *backlog = Backlog::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixer_accepts_a_new_input_during_capture() {
        let ready = Arc::new(AtomicBool::new(true));
        let clock = Arc::new(Clock::new());
        let (tx, rx) = std::sync::mpsc::channel();
        let mixer = Mixer::start(Vec::new(), ready, clock, move |pcm| { let _ = tx.send(pcm); });
        let input = Arc::new(InputShared::new(1.0, None));
        mixer.set_inputs(vec![input.clone()]);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut heard = false;
        while Instant::now() < deadline {
            input.deliver(vec![0.5; 960]);
            if let Ok(pcm) = rx.recv_timeout(Duration::from_millis(30))
                && pcm.chunks_exact(2).any(|s| i16::from_le_bytes([s[0], s[1]]).abs() > 1000)
            {
                heard = true;
                break;
            }
        }
        assert!(heard);
        mixer.set_inputs(Vec::new());
        drop(mixer);
    }

    fn input_with(samples: &[f32], gain: f32) -> Arc<InputShared> {
        let input = InputShared::new(gain, None);
        input.buffer.lock().unwrap().extend(samples);
        Arc::new(input)
    }

    #[test]
    fn mixes_with_gain_and_pads_silence() {
        let a = input_with(&[0.5, 0.5], 1.0);
        let b = input_with(&[0.2, 0.2, 0.2, 0.2], 0.5);
        let mut out = Vec::new();
        mix_block(&[a.clone(), b], 4, &mut out, &mut [Backlog::default(), Backlog::default()]);
        assert_eq!(out, vec![0.6, 0.6, 0.1, 0.1]);
        assert!(a.buffer.lock().unwrap().is_empty());
    }

    
    fn sine(amplitude: f32, seconds: f32) -> Vec<f32> {
        let frames = (SAMPLE_RATE as f32 * seconds) as usize;
        (0..frames)
            .flat_map(|i| [amplitude * (i as f32 * 50.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin(); 2])
            .collect()
    }

    #[test]
    fn inputs_adding_up_past_full_scale_are_limited_not_clipped() {
        
        let samples = sine(0.7, 0.5).len();
        let inputs = [input_with(&sine(0.7, 0.5), 1.0), input_with(&sine(0.7, 0.5), 1.0)];
        let mut mix = Vec::new();
        mix_block(&inputs, samples, &mut mix, &mut [Backlog::default(), Backlog::default()]);
        let summed = mix.clone();
        let limited = Limiter::new().process(&mut mix);
        assert!(mix.iter().all(|s| s.abs() <= 1.0), "nothing left over full scale");
        
        
        let ratios: Vec<f32> =
            summed.iter().zip(&mix).skip(SAMPLE_RATE as usize * CHANNELS / 50).filter(|(s, _)| s.abs() > 0.1).map(|(s, o)| o / s).collect();
        let (low, high) = ratios.iter().fold((f32::MAX, 0f32), |(l, h), r| (l.min(*r), h.max(*r)));
        assert!(high / low < 1.001, "gain varied from {low} to {high}");
        assert!((0.70..0.73).contains(&low), "gain {low} is about 1/1.4");
        assert!(limited);
    }

    #[test]
    fn a_mix_within_full_scale_is_untouched() {
        let mut mix = [sine(0.6, 0.1), sine(-0.4, 0.1)].concat();
        let before = mix.clone();
        let mut limiter = Limiter::new();
        assert!(!limiter.process(&mut mix));
        assert_eq!(mix, before);
    }

    #[test]
    fn only_sound_after_silence_wakes_the_ui() {
        assert!(wakes(0.0, 0.5));
        assert!(wakes(METER_FLOOR, 0.01));
        assert!(!wakes(0.5, 0.6), "already sounding: the UI is repainting anyway");
        assert!(!wakes(0.0, 0.0005), "below the floor the meter still shows -inf");
        assert!(!wakes(0.5, 0.0));
    }

    #[test]
    fn denoiser_keeps_the_stream_length_in_whole_frames() {
        let mut d = Denoiser::new();
        let frame = DenoiseState::FRAME_SIZE;
        assert_eq!(d.process(&vec![0.0; 300 * 2]).len(), 0); 
        assert_eq!(d.process(&vec![0.0; 700 * 2]).len(), 2 * frame * 2); 
        assert_eq!(d.pending.len(), 1000 - 2 * frame);
    }

    #[test]
    fn denoiser_keeps_silence_silent() {
        let output = Denoiser::new().process(&vec![0.0; SAMPLE_RATE as usize * 2]);
        assert!(!output.is_empty());
        assert!(output.iter().all(|s| s.abs() < 1e-4));
    }

    #[test]
    fn a_stall_does_not_leave_the_input_late_for_good() {
        let input = input_with(&[], 1.0);
        let (mut out, mut backlog) = (Vec::new(), [Backlog::default()]);
        let block = SAMPLE_RATE as usize * CHANNELS / 100; 
        let deliver = |blocks: usize| input.buffer.lock().unwrap().extend(vec![0.5f32; block * blocks]);
        
        
        for tick in 0..300 {
            match tick {
                100..105 => {}
                105 => deliver(6),
                _ => deliver(1),
            }
            mix_block(std::slice::from_ref(&input), block, &mut out, &mut backlog);
        }
        let left = input.buffer.lock().unwrap().len();
        assert!(left <= CUSHION, "{left} samples still waiting: the delay stayed");
    }

    #[test]
    fn drops_oldest_samples_beyond_latency_bound() {
        let data: Vec<f32> = (0..(MAX_LATENCY + 10)).map(|i| i as f32).collect();
        let a = input_with(&data, 1.0);
        let mut out = Vec::new();
        mix_block(std::slice::from_ref(&a), 4, &mut out, &mut [Backlog::default()]);
        
        assert_eq!(out[0], 6.0);
        assert_eq!(a.buffer.lock().unwrap().len(), MAX_LATENCY);
    }
}
