



use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::i18n::tr;


pub const TICKS_PER_SECOND: i64 = 10_000_000;


#[derive(Clone, Debug)]
pub struct Packet {
    pub data: Arc<[u8]>,
    
    pub time: i64,
    pub duration: i64,
    
    pub key: bool,
}


pub trait Muxer: Sized + Send {
    
    type Format: Clone + Send;

    
    fn create(path: &Path, video: &Self::Format, audio: Option<&Self::Format>, start: i64) -> Result<Self, String>;
    
    fn write(&self, video: bool, packet: &Packet) -> Result<(), String>;
    fn finish(self) -> Result<(), String>;
}




pub struct Clock(Mutex<Instant>);

impl Clock {
    pub fn new() -> Self {
        Self(Mutex::new(Instant::now()))
    }

    pub fn restart(&self) {
        *self.0.lock().unwrap() = Instant::now();
    }

    
    pub fn elapsed(&self) -> Duration {
        self.0.lock().unwrap().elapsed()
    }
}



pub struct Outputs<M: Muxer> {
    
    path: Option<PathBuf>,
    file: Option<M>,
    
    pending_audio: VecDeque<Packet>,
    replay: Option<Ring>,
    video_format: Option<M::Format>,
    audio_format: Option<M::Format>,
    
    keyframe_wanted: bool,
    error: Option<String>,
    
    
    paused: bool,
    resuming: bool,
    
    
    skipped: i64,
    paused_from: i64,
    resumed_at: i64,
    file_start: i64,
    file_end: i64,
}

impl<M: Muxer> Default for Outputs<M> {
    fn default() -> Self {
        Self {
            path: None,
            file: None,
            pending_audio: VecDeque::new(),
            replay: None,
            video_format: None,
            audio_format: None,
            keyframe_wanted: false,
            error: None,
            paused: false,
            resuming: false,
            skipped: 0,
            paused_from: 0,
            resumed_at: 0,
            file_start: 0,
            file_end: 0,
        }
    }
}

impl<M: Muxer> Outputs<M> {
    
    pub fn start_file(&mut self, path: PathBuf) {
        (self.path, self.file, self.error, self.keyframe_wanted) = (Some(path), None, None, true);
        (self.paused, self.resuming, self.skipped, self.paused_from, self.resumed_at) = (false, false, 0, 0, 0);
    }

    
    pub fn set_file_paused(&mut self, paused: bool) {
        if self.path.is_none() || paused == self.paused {
            return;
        }
        self.paused = paused;
        if paused {
            self.paused_from = self.file_end;
        } else {
            
            (self.resuming, self.keyframe_wanted) = (true, true);
            self.pending_audio.clear();
        }
    }

    pub fn file_paused(&self) -> bool {
        self.paused
    }

    
    pub fn recorded(&self) -> Duration {
        let ticks = if self.file.is_some() { self.file_end - self.file_start - self.skipped } else { 0 };
        Duration::from_nanos(ticks.max(0) as u64 * 100)
    }

    
    
    pub fn failed(&self) -> bool {
        self.path.is_some() && self.error.is_some()
    }

    
    pub fn take_file(&mut self) -> Option<PendingFile<M>> {
        let path = self.path.take()?;
        (self.paused, self.resuming) = (false, false);
        Some(PendingFile { path, file: self.file.take(), error: self.error.take() })
    }

    
    pub fn set_replay(&mut self, seconds: Option<u32>) {
        if seconds.is_some() { self.keyframe_wanted = true; }
        self.replay = seconds.map(|s| Ring::new(s as i64 * TICKS_PER_SECOND));
    }

    pub fn set_replay_on_disk(&mut self, seconds: u32) -> Result<(), String> {
        let segment = crate::disk_buffer::Segment::new().map_err(|e| e.to_string())?;
        let mut ring = Ring::new(seconds as i64 * TICKS_PER_SECOND);
        ring.segment = Some(segment);
        self.replay = Some(ring);
        self.keyframe_wanted = true;
        Ok(())
    }

    pub fn replay_error(&self) -> Option<String> {
        self.replay.as_ref().and_then(|ring| ring.error.clone())
    }

    
    pub fn replay(&self) -> Result<Replay<M>, String> {
        let empty = || tr!("el búfer de repetición todavía está vacío", "the replay buffer is still empty").to_owned();
        let ring = self.replay.as_ref().ok_or_else(empty)?;
        if let Some(error) = &ring.error { return Err(error.clone()); }
        if ring.video.is_empty() { return Err(empty()); }
        let video_format = self.video_format.clone().ok_or_else(empty)?;
        Ok(Replay {
            video_format,
            audio_format: self.audio_format.clone(),
            video: ring.video.iter().cloned().collect(),
            audio: ring.audio.iter().cloned().collect(),
        })
    }

    
    pub fn set_audio_format(&mut self, format: Option<M::Format>) {
        self.audio_format = format;
    }

    pub fn take_keyframe_request(&mut self) -> bool {
        std::mem::take(&mut self.keyframe_wanted)
    }

    
    
    pub fn video(&mut self, packet: Packet, video_format: impl FnOnce() -> Result<M::Format, String>) {
        if packet.key && self.video_format.is_none() {
            match video_format() {
                Ok(format) => self.video_format = Some(format),
                Err(e) => {
                    if let Some(ring) = &mut self.replay {
                        ring.error.get_or_insert_with(|| e.clone());
                    }
                    self.error.get_or_insert(e);
                }
            }
        }
        
        if let (Some(path), None, None, true, false, Some(video)) =
            (&self.path, &self.file, &self.error, packet.key, self.paused, &self.video_format)
        {
            match M::create(path, video, self.audio_format.as_ref(), packet.time) {
                Ok(file) => {
                    for audio in self.pending_audio.drain(..) {
                        if let Err(error) = file.write(false, &audio) {
                            self.error.get_or_insert(error);
                            break;
                        }
                    }
                    (self.file, self.file_start, self.file_end, self.resuming) = (Some(file), packet.time, packet.time, false);
                }
                Err(e) => {
                    self.error.get_or_insert(e);
                }
            }
        }
        if self.resuming && packet.key && self.file.is_some() {
            
            self.skipped += packet.time - self.file_end;
            (self.resuming, self.resumed_at) = (false, packet.time);
            for audio in std::mem::take(&mut self.pending_audio) {
                self.write_file(false, &audio);
            }
        }
        if !self.paused && !self.resuming {
            self.write_file(true, &packet);
            self.file_end = self.file_end.max(packet.time + packet.duration);
        }
        if let Some(ring) = &mut self.replay {
            ring.push_video(packet);
        }
    }

    pub fn audio(&mut self, packets: Vec<Packet>) {
        for packet in packets {
            
            
            let paused = (self.paused || self.resuming) && packet.time >= self.paused_from;
            match &self.file {
                _ if self.path.is_some() && self.error.is_some() => {}
                
                _ if self.paused && paused => {}
                Some(_) if !paused => self.write_file(false, &packet),
                
                Some(_) => self.pending_audio.push_back(packet.clone()),
                None => {
                    self.pending_audio.push_back(packet.clone());
                    
                    if self.path.is_none() || self.error.is_some() {
                        while self.pending_audio.front().is_some_and(|p| p.time < packet.time - TICKS_PER_SECOND) {
                            self.pending_audio.pop_front();
                        }
                    }
                }
            }
            if self.path.is_some() && self.error.is_none()
                && self.pending_audio.front().is_some_and(|first| packet.time.saturating_sub(first.time) > 10 * TICKS_PER_SECOND)
            {
                self.record_error("Не удалось получить опорный кадр для записи.".into());
                self.pending_audio.clear();
            }
            if let Some(ring) = &mut self.replay {
                ring.push_audio(packet);
            }
        }
    }

    pub fn record_error(&mut self, error: String) {
        self.error.get_or_insert(error);
    }

    pub fn encoder_error(&mut self, error: String) {
        if let Some(ring) = &mut self.replay {
            ring.error.get_or_insert_with(|| error.clone());
        }
        self.record_error(error);
    }

    
    
    fn write_file(&mut self, video: bool, packet: &Packet) {
        let Some(file) = &self.file else { return };
        if !video && (self.paused_from..self.resumed_at).contains(&packet.time) {
            return;
        }
        let time = packet.time - self.skipped;
        let result = if self.skipped == 0 { file.write(video, packet) } else { file.write(video, &Packet { time, ..packet.clone() }) };
        if let Err(e) = result {
            self.error.get_or_insert(e);
        }
    }
}


pub struct PendingFile<M: Muxer> {
    path: PathBuf,
    file: Option<M>,
    error: Option<String>,
}

impl<M: Muxer> PendingFile<M> {
    
    
    pub fn finish(self) -> Result<PathBuf, String> {
        let finished = self.file.map(M::finish);
        match (self.error, finished) {
            (None, Some(result)) => result.map(|()| self.path),
            (None, None) => Err(tr!("no se codificó ningún fotograma", "no frame was encoded").to_owned()),
            (Some(e), Some(Ok(()))) => Err(tr!(
                "{} (lo grabado hasta entonces está en '{}')",
                "{} (what was recorded until then is in '{}')",
                e,
                crate::platform::sys::display_path(&self.path).display()
            )),
            (Some(e), _) => Err(e),
        }
    }
}


struct Ring {
    
    length: i64,
    video: VecDeque<StoredPacket>,
    audio: VecDeque<StoredPacket>,
    segment: Option<Arc<crate::disk_buffer::Segment>>,
    segment_start: Option<i64>,
    error: Option<String>,
}

#[derive(Clone, Debug)]
enum StoredData {
    Memory(Arc<[u8]>),
    Disk(crate::disk_buffer::Slice),
}

#[derive(Clone, Debug)]
struct StoredPacket {
    data: StoredData,
    time: i64,
    duration: i64,
    key: bool,
}

impl StoredPacket {
    fn load(&self) -> Result<Packet, String> {
        let data = match &self.data {
            StoredData::Memory(data) => data.clone(),
            StoredData::Disk(data) => data.read().map_err(|error| error.to_string())?.into(),
        };
        Ok(Packet { data, time: self.time, duration: self.duration, key: self.key })
    }
}

impl Ring {
    fn new(length: i64) -> Self {
        Self { length, video: VecDeque::new(), audio: VecDeque::new(), segment: None, segment_start: None, error: None }
    }

    fn store(&mut self, packet: Packet) -> Option<StoredPacket> {
        if self.error.is_some() { return None; }
        if self.segment.is_some() {
            let start = *self.segment_start.get_or_insert(packet.time);
            if packet.time.saturating_sub(start) >= (self.length / 128).max(2 * TICKS_PER_SECOND) {
                match crate::disk_buffer::Segment::new() {
                    Ok(segment) => { self.segment = Some(segment); self.segment_start = Some(packet.time); }
                    Err(error) => { self.error = Some(format!("Ошибка создания буфера отката: {error}")); return None; }
                }
            }
        }
        let data = match &self.segment {
            Some(segment) => match segment.append(&packet.data) {
                Ok(data) => StoredData::Disk(data),
                Err(error) => { self.error = Some(format!("Ошибка записи буфера отката: {error}")); return None; }
            },
            None => StoredData::Memory(packet.data),
        };
        Some(StoredPacket { data, time: packet.time, duration: packet.duration, key: packet.key })
    }

    fn push_video(&mut self, packet: Packet) {
        if self.video.is_empty() && !packet.key {
            return;
        }
        if self.video.front().is_some_and(|first| packet.time.saturating_sub(first.time) > self.length + 10 * TICKS_PER_SECOND) {
            self.error = Some("Кодировщик слишком долго не создаёт опорный кадр.".into());
            return;
        }
        let oldest_needed = packet.time - self.length;
        let Some(packet) = self.store(packet) else { return };
        self.video.push_back(packet);
        
        while let Some(next) = self.video.iter().skip(1).position(|p| p.key).map(|i| i + 1)
            && self.video[next].time <= oldest_needed
        {
            self.video.drain(..next);
        }
        self.trim_audio();
    }

    fn push_audio(&mut self, packet: Packet) {
        if self.error.is_some() { return; }
        if self.video.back().is_some_and(|last| packet.time.saturating_sub(last.time) > 10 * TICKS_PER_SECOND) {
            self.error = Some("Откат остановлен: видеокодировщик перестал выдавать кадры.".into());
            return;
        }
        let Some(packet) = self.store(packet) else { return };
        self.audio.push_back(packet);
        self.trim_audio();
    }

    
    fn trim_audio(&mut self) {
        let start = match (self.video.front(), self.audio.back()) {
            (Some(video), _) => video.time,
            (None, Some(audio)) => audio.time - self.length,
            (None, None) => return,
        };
        while self.audio.front().is_some_and(|p| p.time < start) {
            self.audio.pop_front();
        }
    }
}


pub struct Replay<M: Muxer> {
    video_format: M::Format,
    audio_format: Option<M::Format>,
    video: Vec<StoredPacket>,
    audio: Vec<StoredPacket>,
}

impl<M: Muxer> Replay<M> {
    fn encoded_bytes(&self) -> u64 {
        self.video.iter().chain(&self.audio).fold(0u64, |total, packet| {
            let len = match &packet.data {
                StoredData::Memory(bytes) => bytes.len(),
                StoredData::Disk(bytes) => bytes.len(),
            };
            total.saturating_add(len as u64)
        })
    }

    pub fn save_unique(self, path: PathBuf) -> Result<PathBuf, String> {
        let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
        ensure_replay_space(self.encoded_bytes(), crate::platform::sys::free_space(parent))?;
        let stem = path.file_stem().ok_or("Некорректное имя отката")?.to_os_string();
        let extension = path.extension().ok_or("Некорректное расширение отката")?.to_os_string();
        let temp = tempfile::Builder::new().suffix(".mp4").tempfile_in(parent).map_err(|error| error.to_string())?.into_temp_path();
        self.save(temp.to_path_buf())?;
        let mut temp = temp;
        for index in 1..=10_000 {
            let candidate = if index == 1 { path.clone() } else {
                let mut name = stem.clone();
                name.push(format!(" ({index})."));
                name.push(&extension);
                path.with_file_name(name)
            };
            match temp.persist_noclobber(&candidate) {
                Ok(()) => return Ok(candidate),
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => temp = error.path,
                Err(error) => return Err(error.error.to_string()),
            }
        }
        Err("Не удалось подобрать свободное имя отката".into())
    }

    pub fn save(self, path: PathBuf) -> Result<PathBuf, String> {
        let start = self.video[0].time;
        let file = M::create(&path, &self.video_format, self.audio_format.as_ref(), start)?;
        
        let (mut audio, mut video) = (self.audio.iter().peekable(), self.video.iter().peekable());
        loop {
            let is_video = match (audio.peek(), video.peek()) {
                (Some(a), Some(v)) => v.time <= a.time,
                (Some(_), None) => false,
                (None, Some(_)) => true,
                (None, None) => break,
            };
            let packet = if is_video { video.next() } else { audio.next() };
            let result = packet.expect("peeked").load().and_then(|packet| file.write(is_video, &packet));
            if let Err(e) = result {
                
                let _ = file.finish();
                return Err(e);
            }
        }
        file.finish()?;
        Ok(path)
    }
}

fn ensure_replay_space(required: u64, available: Option<u64>) -> Result<(), String> {
    if let Some(available) = available && available < required {
        return Err(format!("Недостаточно места для отката: требуется не менее {:.1} МБ, доступно {:.1} МБ.", required as f64 / 1_000_000.0, available as f64 / 1_000_000.0));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    struct FileMuxer(std::fs::File);

    impl Muxer for FileMuxer {
        type Format = u8;

        fn create(path: &Path, value: &u8, _: Option<&u8>, _: i64) -> Result<Self, String> {
            use std::io::Write;
            let mut file = std::fs::File::create(path).map_err(|error| error.to_string())?;
            file.write_all(&[*value]).map_err(|error| error.to_string())?;
            Ok(Self(file))
        }

        fn write(&self, _: bool, _: &Packet) -> Result<(), String> { Ok(()) }

        fn finish(self) -> Result<(), String> {
            self.0.sync_all().map_err(|error| error.to_string())
        }
    }

    #[test]
    fn simultaneous_replays_preserve_existing_files() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("откат.mp4");
        std::fs::write(&path, b"existing").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = [1, 2].into_iter().map(|value| {
            let path = path.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                let mut outputs = Outputs::<FileMuxer>::default();
                outputs.set_replay(Some(60));
                outputs.video(packet(0, true), || Ok(value));
                let replay = outputs.replay().unwrap();
                barrier.wait();
                replay.save_unique(path).unwrap()
            })
        }).collect();
        let paths: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
        assert_ne!(paths[0], paths[1]);
        assert_eq!(std::fs::read(path).unwrap(), b"existing");
        let mut values: Vec<_> = paths.iter().map(|path| std::fs::read(path).unwrap()[0]).collect();
        values.sort();
        assert_eq!(values, [1, 2]);
        assert_eq!(std::fs::read_dir(folder.path()).unwrap().count(), 3);
    }

    fn packet(time: i64, key: bool) -> Packet {
        Packet { data: vec![0; 4].into(), time, duration: 0, key }
    }

    
    fn fill(ring: &mut Ring, seconds: i64) {
        let audio_step = 1024 * TICKS_PER_SECOND / 48_000;
        let mut audio = 0;
        for n in 0..seconds * 30 {
            let time = n * TICKS_PER_SECOND / 30;
            while audio <= time {
                ring.push_audio(packet(audio, true));
                audio += audio_step;
            }
            ring.push_video(packet(time, n % 60 == 0));
        }
    }

    #[test]
    fn replay_keeps_the_last_seconds_from_a_keyframe() {
        let mut ring = Ring::new(20 * TICKS_PER_SECOND);
        fill(&mut ring, 60);
        let (first, newest) = (&ring.video[0], ring.video.back().unwrap());
        assert!(first.key, "the replay must start on a keyframe");
        let covered = newest.time - first.time;
        assert!(covered >= 20 * TICKS_PER_SECOND, "only {covered} covered");
        assert!(covered < 22 * TICKS_PER_SECOND, "{covered} kept: more than one keyframe interval too many");
        assert!(ring.audio[0].time >= first.time, "audio from before the first frame was kept");
        assert!(ring.audio[0].time - first.time < TICKS_PER_SECOND / 40, "audio at the start is missing");
    }

    #[test]
    fn replay_shorter_than_its_length_keeps_everything() {
        let mut ring = Ring::new(20 * TICKS_PER_SECOND);
        fill(&mut ring, 5);
        assert_eq!(ring.video.len(), 150);
        assert_eq!(ring.video[0].time, 0);
    }

    #[test]
    fn replay_starts_at_the_first_keyframe() {
        let mut ring = Ring::new(20 * TICKS_PER_SECOND);
        ring.push_video(packet(0, false));
        ring.push_video(packet(1, false));
        assert!(ring.video.is_empty());
        ring.push_video(packet(2, true));
        assert_eq!(ring.video.len(), 1);
    }

    #[test]
    fn replay_stops_buffering_audio_when_video_encoder_stalls() {
        for disk in [false, true] {
            let mut ring = Ring::new(60 * TICKS_PER_SECOND);
            if disk { ring.segment = Some(crate::disk_buffer::Segment::new().unwrap()); }
            ring.push_video(packet(0, true));
            for second in 1..=120 {
                ring.push_audio(packet(second * TICKS_PER_SECOND, true));
            }
            assert_eq!(ring.audio.len(), 10);
            assert!(ring.error.as_deref().is_some_and(|error| error.contains("перестал выдавать кадры")));
            assert_eq!(ring.audio.back().unwrap().time, 10 * TICKS_PER_SECOND);
        }
    }

    #[test]
    fn recording_reports_missing_keyframe_without_unbounded_audio() {
        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.start_file("recording.mp4".into());
        for second in 0..=120 {
            outputs.audio(vec![packet(second * TICKS_PER_SECOND, true)]);
        }
        assert!(outputs.failed());
        assert!(outputs.pending_audio.len() <= 2);
        assert!(outputs.take_file().unwrap().finish().unwrap_err().contains("опорный кадр"));
    }

    #[test]
    fn recording_reports_missing_keyframe_after_resume() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.start_file("recording.mp4".into());
        outputs.video(packet(0, true), || Ok(written));
        outputs.set_file_paused(true);
        outputs.set_file_paused(false);
        for second in 1..=120 {
            outputs.audio(vec![packet(second * TICKS_PER_SECOND, true)]);
        }
        assert!(outputs.failed());
        assert!(outputs.pending_audio.is_empty());
    }

    #[test]
    fn replay_space_check_uses_encoded_packets_for_memory_and_disk() {
        for disk in [false, true] {
            let written = Arc::new(Mutex::new(Vec::new()));
            let mut outputs = Outputs::<FakeMuxer>::default();
            if disk { outputs.set_replay_on_disk(60).unwrap(); } else { outputs.set_replay(Some(60)); }
            outputs.video(packet(0, true), || Ok(written));
            outputs.audio(vec![packet(1, true)]);
            let replay = outputs.replay().unwrap();
            assert_eq!(replay.encoded_bytes(), 8);
            assert!(ensure_replay_space(replay.encoded_bytes(), Some(7)).is_err());
            assert!(ensure_replay_space(replay.encoded_bytes(), Some(8)).is_ok());
            assert!(ensure_replay_space(replay.encoded_bytes(), None).is_ok());
        }
    }

    
    struct FakeMuxer {
        start: i64,
        written: Arc<Mutex<Vec<(bool, i64)>>>,
    }

    impl Muxer for FakeMuxer {
        type Format = Arc<Mutex<Vec<(bool, i64)>>>;

        fn create(_: &Path, video: &Self::Format, _: Option<&Self::Format>, start: i64) -> Result<Self, String> {
            Ok(Self { start, written: video.clone() })
        }

        fn write(&self, video: bool, packet: &Packet) -> Result<(), String> {
            let time = packet.time - self.start;
            if time >= 0 {
                self.written.lock().unwrap().push((video, time));
            }
            Ok(())
        }

        fn finish(self) -> Result<(), String> {
            Ok(())
        }
    }

    
    #[derive(Default)]
    struct Calls {
        creates: usize,
        finished: bool,
        fail_create: bool,
        fail_write: bool,
        fail_audio: bool,
    }

    
    struct Broken(Arc<Mutex<Calls>>);

    impl Muxer for Broken {
        type Format = Arc<Mutex<Calls>>;

        fn create(_: &Path, calls: &Self::Format, _: Option<&Self::Format>, _: i64) -> Result<Self, String> {
            let mut c = calls.lock().unwrap();
            c.creates += 1;
            if c.fail_create { Err("cannot create".into()) } else { Ok(Self(calls.clone())) }
        }

        fn write(&self, video: bool, _: &Packet) -> Result<(), String> {
            let calls = self.0.lock().unwrap();
            if calls.fail_write || (!video && calls.fail_audio) { Err("disk full".into()) } else { Ok(()) }
        }

        fn finish(self) -> Result<(), String> {
            self.0.lock().unwrap().finished = true;
            Ok(())
        }
    }

    #[test]
    fn a_file_that_cannot_be_created_fails_the_recording_once() {
        let calls = Arc::new(Mutex::new(Calls { fail_create: true, ..Calls::default() }));
        let mut outputs = Outputs::<Broken>::default();
        outputs.start_file(PathBuf::from("rec.mp4"));
        outputs.video(packet(0, true), || Ok(calls.clone()));
        assert!(outputs.failed(), "the app must hear of it while recording");
        
        for n in 1..100 {
            outputs.audio(vec![packet(n * TICKS_PER_SECOND / 10, true)]);
            outputs.video(packet(n * TICKS_PER_SECOND / 10, n % 10 == 0), || unreachable!());
        }
        assert_eq!(calls.lock().unwrap().creates, 1, "not tried again at every keyframe");
        assert!(outputs.pending_audio.len() < 20, "audio is not kept for a file that will not come");
        assert!(outputs.take_file().unwrap().finish().is_err());
    }

    #[test]
    fn a_recording_that_fails_is_still_finished() {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let mut outputs = Outputs::<Broken>::default();
        outputs.start_file(PathBuf::from("rec.mp4"));
        outputs.video(packet(0, true), || Ok(calls.clone()));
        assert!(!outputs.failed());
        calls.lock().unwrap().fail_write = true;
        outputs.video(packet(10, false), || unreachable!());
        assert!(outputs.failed());
        let error = outputs.take_file().unwrap().finish().unwrap_err();
        assert!(calls.lock().unwrap().finished, "the file keeps what was recorded until then");
        assert!(error.contains("disk full") && error.contains("rec.mp4"), "{error}");
    }

    #[test]
    fn pending_audio_write_failure_is_reported() {
        let calls = Arc::new(Mutex::new(Calls { fail_audio: true, ..Calls::default() }));
        let mut outputs = Outputs::<Broken>::default();
        outputs.start_file(PathBuf::from("rec.mp4"));
        outputs.audio(vec![packet(10, true)]);
        outputs.video(packet(0, true), || Ok(calls.clone()));
        assert!(outputs.failed());
        assert!(outputs.take_file().unwrap().finish().unwrap_err().contains("disk full"));
        assert!(calls.lock().unwrap().finished);
    }

    #[test]
    fn a_replay_that_fails_to_write_is_still_finished() {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let mut outputs = Outputs::<Broken>::default();
        outputs.set_replay(Some(20));
        outputs.video(packet(0, true), || Ok(calls.clone()));
        calls.lock().unwrap().fail_write = true;
        assert!(outputs.replay().unwrap().save(PathBuf::from("replay.mp4")).is_err());
        assert!(calls.lock().unwrap().finished);
    }

    #[test]
    fn a_recording_starts_at_the_next_keyframe_with_the_audio_since_then() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut outputs = Outputs::<FakeMuxer>::default();
        
        outputs.video(packet(0, true), || Ok(written.clone()));
        outputs.audio(vec![packet(5, true)]);
        outputs.start_file(PathBuf::from("rec.mp4"));
        assert!(outputs.take_keyframe_request(), "a new recording must ask for a keyframe");
        assert!(!outputs.take_keyframe_request(), "the request is taken once");
        outputs.audio(vec![packet(15, true)]);
        outputs.video(packet(10, false), || unreachable!());
        outputs.video(packet(20, true), || unreachable!());
        outputs.audio(vec![packet(25, true)]);
        
        
        assert_eq!(*written.lock().unwrap(), vec![(true, 0), (false, 5)]);
        let file = outputs.take_file().expect("a recording was started");
        assert_eq!(file.finish().unwrap(), PathBuf::from("rec.mp4"));
    }

    #[test]
    fn a_recording_without_frames_is_an_error() {
        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.start_file(PathBuf::from("rec.mp4"));
        assert!(outputs.take_file().unwrap().finish().is_err());
        assert!(outputs.take_file().is_none(), "the file is handed out once");
    }

    #[test]
    fn a_saved_replay_is_interleaved_by_time() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.set_replay(Some(20));
        assert!(outputs.replay().is_err(), "nothing to save yet");
        outputs.video(packet(100, true), || Ok(written.clone()));
        outputs.audio(vec![packet(90, true), packet(110, true)]);
        outputs.video(packet(120, false), || unreachable!());
        let replay = outputs.replay().unwrap();
        thread::spawn(move || replay.save(PathBuf::from("replay.mp4")).unwrap()).join().unwrap();
        assert_eq!(*written.lock().unwrap(), vec![(true, 0), (false, 10), (true, 20)]);
    }

    #[test]
    fn replay_snapshot_shares_packet_data_and_survives_eviction() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.set_replay(Some(1));
        outputs.video(packet(0, true), || Ok(written.clone()));
        outputs.audio(vec![packet(1, true)]);
        let replay = outputs.replay().unwrap();
        let ring = outputs.replay.as_ref().unwrap();
        assert!(matches!((&ring.video[0].data, &replay.video[0].data), (StoredData::Memory(a), StoredData::Memory(b)) if Arc::ptr_eq(a, b)));
        assert!(matches!((&ring.audio[0].data, &replay.audio[0].data), (StoredData::Memory(a), StoredData::Memory(b)) if Arc::ptr_eq(a, b)));
        outputs.video(packet(2 * TICKS_PER_SECOND, true), || unreachable!());
        outputs.video(packet(4 * TICKS_PER_SECOND, true), || unreachable!());
        assert_eq!(outputs.replay.as_ref().unwrap().video.front().unwrap().time, 2 * TICKS_PER_SECOND);
        replay.save(PathBuf::from("replay.mp4")).unwrap();
        assert_eq!(*written.lock().unwrap(), vec![(true, 0), (false, 1)]);
    }

    #[test]
    fn disk_replay_survives_eviction_and_preserves_packet_order() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.set_replay_on_disk(1).unwrap();
        outputs.video(packet(0, true), || Ok(written.clone()));
        outputs.audio(vec![packet(1, true)]);
        let replay = outputs.replay().unwrap();
        assert!(matches!(replay.video[0].data, StoredData::Disk(_)));
        outputs.video(packet(2 * TICKS_PER_SECOND, true), || unreachable!());
        outputs.video(packet(4 * TICKS_PER_SECOND, true), || unreachable!());
        assert_eq!(outputs.replay.as_ref().unwrap().video.front().unwrap().time, 2 * TICKS_PER_SECOND);
        replay.save(PathBuf::from("replay.mp4")).unwrap();
        assert_eq!(*written.lock().unwrap(), vec![(true, 0), (false, 1)]);
    }

    #[test]
    fn replay_reports_encoder_without_keyframes_instead_of_growing_forever() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.set_replay(Some(1));
        assert!(outputs.take_keyframe_request());
        outputs.video(packet(0, true), || Ok(written.clone()));
        outputs.video(packet(12 * TICKS_PER_SECOND, false), || unreachable!());
        assert!(outputs.replay_error().is_some());
        assert!(outputs.replay().is_err());
    }

    #[test]
    fn replay_reports_video_encoder_failures() {
        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.set_replay(Some(1));
        outputs.video(packet(0, true), || Err("invalid video format".into()));
        assert_eq!(outputs.replay_error().as_deref(), Some("invalid video format"));
        assert!(outputs.replay().err().unwrap().contains("invalid video format"));

        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.set_replay(Some(1));
        outputs.encoder_error("video encoder stopped".into());
        assert_eq!(outputs.replay_error().as_deref(), Some("video encoder stopped"));
    }

    #[test]
    fn disk_segments_live_until_saved_replay_releases_them() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.set_replay_on_disk(1).unwrap();
        let segment = Arc::downgrade(outputs.replay.as_ref().unwrap().segment.as_ref().unwrap());
        outputs.video(packet(0, true), || Ok(written.clone()));
        let snapshot = outputs.replay().unwrap();
        outputs.video(packet(2 * TICKS_PER_SECOND, true), || unreachable!());
        outputs.video(packet(4 * TICKS_PER_SECOND, true), || unreachable!());
        assert!(segment.upgrade().is_some());
        drop(snapshot);
        assert!(segment.upgrade().is_none());
    }

    #[test]
    fn disk_replay_releases_old_segments_during_long_running_capture() {
        let mut ring = Ring::new(5 * TICKS_PER_SECOND);
        ring.segment = Some(crate::disk_buffer::Segment::new().unwrap());
        let mut seen = Vec::new();
        let mut previous = std::ptr::null();
        for second in 0..=120 {
            ring.push_video(packet(second * TICKS_PER_SECOND, second % 2 == 0));
            let segment = ring.segment.as_ref().unwrap();
            let current = Arc::as_ptr(segment);
            if current != previous {
                seen.push(Arc::downgrade(segment));
                previous = current;
            }
        }
        assert!(ring.error.is_none());
        assert!(seen.len() >= 50, "the test did not rotate enough segments");
        let retained = seen.iter().filter(|segment| segment.upgrade().is_some()).count();
        assert!(retained <= 5, "{retained} old segments are still retained");
        assert!(ring.video.front().unwrap().key);
        for packet in &ring.video {
            assert_eq!(&*packet.load().unwrap().data, &[0; 4]);
        }
    }

    #[test]
    #[ignore]
    fn disk_replay_limits_real_file_bytes_over_two_minutes() {
        let mut ring = Ring::new(45 * TICKS_PER_SECOND);
        ring.segment = Some(crate::disk_buffer::Segment::new().unwrap());
        let data: Arc<[u8]> = vec![42; 64 * 1024].into();
        let mut seen = Vec::new();
        let mut previous = std::ptr::null();
        let mut at_one_minute = 0;
        for frame in 0..(120 * 30) {
            ring.push_video(Packet { data: data.clone(), time: frame * TICKS_PER_SECOND / 30, duration: TICKS_PER_SECOND / 30, key: frame % 60 == 0 });
            let segment = ring.segment.as_ref().unwrap();
            let current = Arc::as_ptr(segment);
            if current != previous {
                seen.push(Arc::downgrade(segment));
                previous = current;
            }
            if frame == 60 * 30 - 1 {
                at_one_minute = seen.iter().filter_map(|segment| segment.upgrade()).map(|segment| segment.size().unwrap()).sum::<u64>();
            }
        }
        let at_two_minutes = seen.iter().filter_map(|segment| segment.upgrade()).map(|segment| segment.size().unwrap()).sum::<u64>();
        assert!(ring.error.is_none());
        assert!(at_one_minute > 80 * 1024 * 1024);
        assert!(at_two_minutes < 100 * 1024 * 1024, "{at_two_minutes} bytes retained");
        assert!(at_two_minutes <= at_one_minute + 4 * 1024 * 1024);
        assert!(ring.video.front().unwrap().key);
        println!("temporary file bytes after one minute: {at_one_minute}; after two minutes: {at_two_minutes}");
    }

    
    
    fn feed(outputs: &mut Outputs<FakeMuxer>, written: &Arc<Mutex<Vec<(bool, i64)>>>, from: i64, to: i64) {
        let step = TICKS_PER_SECOND / 30;
        for n in from * 30..to * 30 {
            outputs.audio(vec![Packet { duration: step, ..packet(n * step, true) }]);
            let key = n % 60 == 0 || outputs.take_keyframe_request();
            outputs.video(Packet { duration: step, ..packet(n * step, key) }, || Ok(written.clone()));
        }
    }

    #[test]
    fn audio_made_before_a_pause_and_coming_after_it_is_recorded() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.start_file(PathBuf::from("rec.mp4"));
        let step = TICKS_PER_SECOND / 30;
        outputs.video(Packet { duration: step, ..packet(0, true) }, || Ok(written.clone()));
        outputs.video(Packet { duration: step, ..packet(step, false) }, || unreachable!());
        outputs.set_file_paused(true);
        
        outputs.audio(vec![packet(step, true), packet(2 * step, true)]);
        outputs.set_file_paused(false);
        
        outputs.audio(vec![packet(3 * step, true)]);
        outputs.video(Packet { duration: step, ..packet(10 * step, true) }, || unreachable!());
        outputs.audio(vec![packet(5 * step, true), packet(10 * step, true)]);
        let audio: Vec<i64> = written.lock().unwrap().iter().filter(|(v, _)| !*v).map(|(_, t)| *t).collect();
        assert_eq!(audio, [step, 2 * step], "before the pause, and where the file goes on");
    }

    #[test]
    fn a_pause_is_left_out_of_the_file_but_not_out_of_the_replay_buffer() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let mut outputs = Outputs::<FakeMuxer>::default();
        outputs.set_replay(Some(20));
        outputs.start_file(PathBuf::from("rec.mp4"));
        feed(&mut outputs, &written, 0, 2);
        outputs.set_file_paused(true);
        assert!(outputs.file_paused());
        feed(&mut outputs, &written, 2, 5);
        let frames = |n: i64| Duration::from_nanos((n * (TICKS_PER_SECOND / 30) * 100) as u64);
        assert_eq!(outputs.recorded(), frames(60), "the timer stops while paused");
        
        outputs.set_file_paused(false);
        feed(&mut outputs, &written, 5, 7);
        assert_eq!(outputs.recorded(), frames(120));
        let written = written.lock().unwrap();
        let video: Vec<i64> = written.iter().filter(|(v, _)| *v).map(|(_, t)| *t).collect();
        let audio: Vec<i64> = written.iter().filter(|(v, _)| !*v).map(|(_, t)| *t).collect();
        
        let step = TICKS_PER_SECOND / 30;
        assert_eq!(video, (0..120).map(|n| n * step).collect::<Vec<_>>());
        assert!(audio.windows(2).all(|w| w[1] - w[0] == step), "a gap or overlap in the audio");
        assert!(*audio.last().unwrap() >= 119 * step - step);
        
        let replay = outputs.replay().unwrap();
        assert_eq!(replay.video.len(), 210);
    }
}
