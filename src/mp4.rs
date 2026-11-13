//! Turns a fragmented MP4 into a regular one, in place. Recordings are written fragmented, so a
//! file left by a crash or a power cut still plays up to its last fragment; on a normal stop it
//! becomes a regular MP4, which every editor opens. The audio and video data stay where they are:
//! a full index (`moov`) is written at the end, and the empty `moov` at the start and the
//! fragments' own indexes (`moof`) become padding (`free`).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Converts the finished fragmented MP4 at `path`; a file that is not fragmented is left as it is.
pub fn defragment(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    convert(&mut file, false).map(|_| ())
}

/// Converts a recording whose writing stopped partway (a crash, a power cut), keeping every whole
/// fragment. Returns whether it was fragmented: a finished recording is already regular.
pub fn repair(path: &Path) -> io::Result<bool> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    convert(&mut file, true)
}

/// What converting needs of a file besides reading and writing.
trait Storage: Read + Write + Seek {
    fn truncate(&mut self, len: u64) -> io::Result<()>;
    /// Puts what was written so far on the disk. The system may otherwise write the small
    /// in-place changes before the new index at the end, and a power cut between them would
    /// leave a file with no index at all.
    fn sync(&mut self) -> io::Result<()>;
}

impl Storage for File {
    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.set_len(len)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.sync_data()
    }
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("fragmented MP4: {what}"))
}

/// A box's place in the file.
struct Header {
    start: u64,
    size: u64,
    kind: [u8; 4],
}

impl Header {
    fn read(r: &mut (impl Read + Seek), start: u64, end: u64) -> io::Result<Self> {
        r.seek(SeekFrom::Start(start))?;
        let mut head = [0u8; 8];
        r.read_exact(&mut head)?;
        let kind = [head[4], head[5], head[6], head[7]];
        let size = match u32::from_be_bytes([head[0], head[1], head[2], head[3]]) {
            0 => end - start,
            1 => {
                let mut large = [0u8; 8];
                r.read_exact(&mut large)?;
                u64::from_be_bytes(large)
            }
            n => u64::from(n),
        };
        if size < 8 {
            return Err(invalid("bad box size"));
        }
        if start + size > end {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "box past the end of the file"));
        }
        Ok(Self { start, size, kind })
    }

    fn read_body(&self, r: &mut (impl Read + Seek)) -> io::Result<Vec<u8>> {
        let header = if self.size > u64::from(u32::MAX) { 16 } else { 8 };
        r.seek(SeekFrom::Start(self.start + header))?;
        let mut body = vec![0; (self.size - header) as usize];
        r.read_exact(&mut body)?;
        Ok(body)
    }
}

/// A box of the index, kept as bytes except for the containers that are rebuilt.
enum Node {
    Leaf([u8; 4], Vec<u8>),
    Container([u8; 4], Vec<Node>),
}

const CONTAINERS: [&[u8; 4]; 5] = [b"moov", b"trak", b"mdia", b"minf", b"stbl"];

fn parse(mut data: &[u8]) -> io::Result<Vec<Node>> {
    let mut nodes = Vec::new();
    while data.len() >= 8 {
        let size = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
        if size < 8 || size > data.len() {
            return Err(invalid("bad box in the index"));
        }
        let kind: [u8; 4] = data[4..8].try_into().unwrap();
        let body = &data[8..size];
        nodes.push(if CONTAINERS.contains(&&kind) { Node::Container(kind, parse(body)?) } else { Node::Leaf(kind, body.to_vec()) });
        data = &data[size..];
    }
    Ok(nodes)
}

fn serialize(nodes: &[Node], out: &mut Vec<u8>) {
    for node in nodes {
        let start = out.len();
        out.extend_from_slice(&[0; 4]);
        match node {
            Node::Leaf(kind, body) => {
                out.extend_from_slice(kind);
                out.extend_from_slice(body);
            }
            Node::Container(kind, children) => {
                out.extend_from_slice(kind);
                serialize(children, out);
            }
        }
        let size = (out.len() - start) as u32;
        out[start..start + 4].copy_from_slice(&size.to_be_bytes());
    }
}

fn find<'a>(nodes: &'a [Node], kind: &[u8; 4]) -> Option<&'a Node> {
    nodes.iter().find(|n| matches!(n, Node::Leaf(k, _) | Node::Container(k, _) if k == kind))
}

fn leaf<'a>(nodes: &'a [Node], kind: &[u8; 4]) -> io::Result<&'a [u8]> {
    match find(nodes, kind) {
        Some(Node::Leaf(_, body)) => Ok(body),
        _ => Err(invalid("missing box")),
    }
}

fn children<'a>(nodes: &'a [Node], kind: &[u8; 4]) -> io::Result<&'a [Node]> {
    match find(nodes, kind) {
        Some(Node::Container(_, c)) => Ok(c),
        _ => Err(invalid("missing box")),
    }
}

fn u32_at(data: &[u8], at: usize) -> io::Result<u32> {
    data.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or_else(|| invalid("short box"))
}

fn u64_at(data: &[u8], at: usize) -> io::Result<u64> {
    data.get(at..at + 8).map(|b| u64::from_be_bytes(b.try_into().unwrap())).ok_or_else(|| invalid("short box"))
}

/// `mvhd`, `mdhd` and `tkhd` share a layout: version and flags, two dates (4 or 8 bytes each),
/// `between` bytes (a timescale, or a track id and a reserved word), then the duration.
fn duration_at(body: &[u8], between: usize) -> usize {
    if body[0] == 1 { 20 + between } else { 12 + between }
}

/// The timescale of an `mvhd` or `mdhd`.
fn timescale(body: &[u8]) -> io::Result<u32> {
    u32_at(body, duration_at(body, 0))
}

/// Sets the duration of an `mvhd`/`mdhd` (`between` 4) or `tkhd` (`between` 8), moving a
/// version 0 box to version 1 if the duration does not fit in 32 bits.
fn set_duration(body: &mut Vec<u8>, between: usize, duration: u64) {
    if body[0] == 0 && duration > u64::from(u32::MAX) {
        let mut v1 = vec![1, body[1], body[2], body[3]];
        v1.extend_from_slice(&u64::from(u32::from_be_bytes(body[4..8].try_into().unwrap())).to_be_bytes());
        v1.extend_from_slice(&u64::from(u32::from_be_bytes(body[8..12].try_into().unwrap())).to_be_bytes());
        v1.extend_from_slice(&body[12..12 + between]);
        v1.extend_from_slice(&[0; 8]);
        v1.extend_from_slice(&body[16 + between..]);
        *body = v1;
    }
    let at = duration_at(body, between);
    if body[0] == 1 {
        body[at..at + 8].copy_from_slice(&duration.to_be_bytes());
    } else {
        body[at..at + 4].copy_from_slice(&(duration as u32).to_be_bytes());
    }
}

#[derive(Clone, Copy, Default)]
struct Defaults {
    description: u32,
    duration: u32,
    size: u32,
    flags: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sample {
    size: u32,
    duration: u32,
    sync: bool,
    /// Composition time offset.
    offset: i64,
}

/// A run of one track's samples stored back to back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Chunk {
    position: u64,
    samples: u32,
    description: u32,
}

#[derive(Default)]
struct Track {
    id: u32,
    defaults: Defaults,
    samples: Vec<Sample>,
    chunks: Vec<Chunk>,
}

/// A sample is a sync sample unless its flags say "non-sync" (`sample_is_non_sync_sample`).
fn is_sync(flags: u32) -> bool {
    flags & 0x1_0000 == 0
}

/// Reads one `moof` (starting at `start`) into the tracks' samples and chunks.
fn read_fragment(moof: &[u8], start: u64, tracks: &mut [Track]) -> io::Result<()> {
    // Where the previous track's data ended: the default start of the next one's.
    let mut data_end = start;
    for node in parse(moof)? {
        let Node::Leaf(kind, traf) = node else { continue };
        if &kind != b"traf" {
            continue;
        }
        let traf = parse(&traf)?;
        let tfhd = leaf(&traf, b"tfhd")?;
        let flags = u32_at(tfhd, 0)? & 0xFF_FFFF;
        let id = u32_at(tfhd, 4)?;
        let track = tracks.iter_mut().find(|t| t.id == id).ok_or_else(|| invalid("fragment of an unknown track"))?;
        let mut at = 8;
        let mut field = |present: bool, wide: bool| -> io::Result<Option<u64>> {
            if !present {
                return Ok(None);
            }
            let value = if wide { u64_at(tfhd, at)? } else { u64::from(u32_at(tfhd, at)?) };
            at += if wide { 8 } else { 4 };
            Ok(Some(value))
        };
        // Media Foundation gives every track's data position; GStreamer leaves it to the
        // default: the `moof` for the first track, then where the previous track's data ended.
        let base = match field(flags & 0x1 != 0, true)? {
            Some(base) => base,
            None if flags & 0x2_0000 != 0 => start,
            None => data_end,
        };
        let d = track.defaults;
        let defaults = Defaults {
            description: field(flags & 0x2 != 0, false)?.map_or(d.description, |v| v as u32),
            duration: field(flags & 0x8 != 0, false)?.map_or(d.duration, |v| v as u32),
            size: field(flags & 0x10 != 0, false)?.map_or(d.size, |v| v as u32),
            flags: field(flags & 0x20 != 0, false)?.map_or(d.flags, |v| v as u32),
        };
        let mut position = base;
        for node in &traf {
            let Node::Leaf(kind, trun) = node else { continue };
            if kind != b"trun" {
                continue;
            }
            let version_flags = u32_at(trun, 0)?;
            let (signed, flags) = (version_flags >> 24 == 1, version_flags & 0xFF_FFFF);
            let count = u32_at(trun, 4)?;
            let mut at = 8;
            if flags & 0x1 != 0 {
                position = base.checked_add_signed(i64::from(u32_at(trun, at)? as i32)).ok_or_else(|| invalid("bad data offset"))?;
                at += 4;
            }
            let mut first_flags = None;
            if flags & 0x4 != 0 {
                first_flags = Some(u32_at(trun, at)?);
                at += 4;
            }
            let chunk_start = position;
            for i in 0..count {
                let mut next = |present: bool, default: u32| -> io::Result<u32> {
                    if !present {
                        return Ok(default);
                    }
                    let value = u32_at(trun, at)?;
                    at += 4;
                    Ok(value)
                };
                let duration = next(flags & 0x100 != 0, defaults.duration)?;
                let size = next(flags & 0x200 != 0, defaults.size)?;
                let sample_flags = next(flags & 0x400 != 0, defaults.flags)?;
                let offset = next(flags & 0x800 != 0, 0)?;
                let sample_flags = if i == 0 { first_flags.unwrap_or(sample_flags) } else { sample_flags };
                let offset = if signed { i64::from(offset as i32) } else { i64::from(offset) };
                track.samples.push(Sample { size, duration, sync: is_sync(sample_flags), offset });
                position += u64::from(size);
            }
            if count > 0 {
                track.chunks.push(Chunk { position: chunk_start, samples: count, description: defaults.description.max(1) });
            }
        }
        data_end = position;
    }
    Ok(())
}

fn full_box(kind: &[u8; 4], version: u8, body: &[u8]) -> Node {
    let mut data = vec![version, 0, 0, 0];
    data.extend_from_slice(body);
    Node::Leaf(*kind, data)
}

/// Run-length entries of a sequence.
fn runs<T: PartialEq + Copy>(values: impl Iterator<Item = T>) -> Vec<(u32, T)> {
    let mut out: Vec<(u32, T)> = Vec::new();
    for v in values {
        match out.last_mut() {
            Some((n, last)) if *last == v => *n += 1,
            _ => out.push((1, v)),
        }
    }
    out
}

/// The sample tables that replace a track's empty ones.
fn tables(track: &Track) -> Vec<Node> {
    let samples = &track.samples;
    let mut nodes = Vec::new();
    let mut body = Vec::new();
    let stts = runs(samples.iter().map(|s| s.duration));
    body.extend_from_slice(&(stts.len() as u32).to_be_bytes());
    for (count, delta) in stts {
        body.extend_from_slice(&count.to_be_bytes());
        body.extend_from_slice(&delta.to_be_bytes());
    }
    nodes.push(full_box(b"stts", 0, &body));

    if samples.iter().any(|s| s.offset != 0) {
        let ctts = runs(samples.iter().map(|s| s.offset));
        let mut body = (ctts.len() as u32).to_be_bytes().to_vec();
        for (count, offset) in &ctts {
            body.extend_from_slice(&count.to_be_bytes());
            body.extend_from_slice(&(*offset as u32).to_be_bytes());
        }
        nodes.push(full_box(b"ctts", u8::from(ctts.iter().any(|(_, o)| *o < 0)), &body));
    }

    if samples.iter().any(|s| !s.sync) {
        let sync: Vec<u32> = (1..).zip(samples).filter(|(_, s)| s.sync).map(|(n, _)| n).collect();
        let mut body = (sync.len() as u32).to_be_bytes().to_vec();
        sync.iter().for_each(|n| body.extend_from_slice(&n.to_be_bytes()));
        nodes.push(full_box(b"stss", 0, &body));
    }

    let mut body = Vec::new();
    let first = samples.first().map_or(0, |s| s.size);
    if samples.iter().all(|s| s.size == first) {
        body.extend_from_slice(&first.to_be_bytes());
        body.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    } else {
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&(samples.len() as u32).to_be_bytes());
        samples.iter().for_each(|s| body.extend_from_slice(&s.size.to_be_bytes()));
    }
    nodes.push(full_box(b"stsz", 0, &body));

    let mut entries: Vec<(u32, u32, u32)> = Vec::new();
    for (n, c) in (1..).zip(&track.chunks) {
        if entries.last().is_none_or(|&(_, samples, description)| (samples, description) != (c.samples, c.description)) {
            entries.push((n, c.samples, c.description));
        }
    }
    let mut body = (entries.len() as u32).to_be_bytes().to_vec();
    for (first, samples, description) in entries {
        [first, samples, description].iter().for_each(|v| body.extend_from_slice(&v.to_be_bytes()));
    }
    nodes.push(full_box(b"stsc", 0, &body));

    let mut body = (track.chunks.len() as u32).to_be_bytes().to_vec();
    if track.chunks.iter().any(|c| c.position > u64::from(u32::MAX)) {
        track.chunks.iter().for_each(|c| body.extend_from_slice(&c.position.to_be_bytes()));
        nodes.push(full_box(b"co64", 0, &body));
    } else {
        track.chunks.iter().for_each(|c| body.extend_from_slice(&(c.position as u32).to_be_bytes()));
        nodes.push(full_box(b"stco", 0, &body));
    }
    nodes
}

/// The full `moov` for the fragments read into `tracks`, from the initial (empty) one.
fn full_index(init: Vec<Node>, tracks: &[Track]) -> io::Result<Vec<u8>> {
    let movie_scale = u64::from(timescale(leaf(&init, b"mvhd")?)?.max(1));
    let mut movie_duration = 0;
    let mut moov = Vec::new();
    let mut traks = tracks.iter();
    for node in init {
        match node {
            Node::Leaf(kind, _) if &kind == b"mvex" => {}
            Node::Container(kind, trak) if &kind == b"trak" => {
                let track = traks.next().ok_or_else(|| invalid("track count"))?;
                let media_scale = u64::from(timescale(leaf(children(&trak, b"mdia")?, b"mdhd")?)?.max(1));
                let duration: u64 = track.samples.iter().map(|s| u64::from(s.duration)).sum();
                let in_movie = (duration * movie_scale).div_ceil(media_scale);
                movie_duration = movie_duration.max(in_movie);
                moov.push(Node::Container(kind, rebuild_trak(trak, track, duration, in_movie)));
            }
            other => moov.push(other),
        }
    }
    for node in &mut moov {
        if let Node::Leaf(kind, body) = node
            && kind == b"mvhd"
        {
            set_duration(body, 4, movie_duration);
        }
    }
    let mut out = Vec::new();
    serialize(&[Node::Container(*b"moov", moov)], &mut out);
    Ok(out)
}

fn rebuild_trak(trak: Vec<Node>, track: &Track, duration: u64, in_movie: u64) -> Vec<Node> {
    trak.into_iter()
        .map(|node| match node {
            Node::Leaf(kind, mut body) if &kind == b"tkhd" => {
                set_duration(&mut body, 8, in_movie);
                Node::Leaf(kind, body)
            }
            Node::Container(kind, mdia) if &kind == b"mdia" => Node::Container(
                kind,
                mdia.into_iter()
                    .map(|node| match node {
                        Node::Leaf(kind, mut body) if &kind == b"mdhd" => {
                            set_duration(&mut body, 4, duration);
                            Node::Leaf(kind, body)
                        }
                        Node::Container(kind, minf) if &kind == b"minf" => Node::Container(
                            kind,
                            minf.into_iter()
                                .map(|node| match node {
                                    Node::Container(kind, stbl) if &kind == b"stbl" => {
                                        let mut stbl: Vec<Node> =
                                            stbl.into_iter().filter(|n| matches!(n, Node::Leaf(k, _) if k == b"stsd")).collect();
                                        stbl.extend(tables(track));
                                        Node::Container(kind, stbl)
                                    }
                                    other => other,
                                })
                                .collect(),
                        ),
                        other => other,
                    })
                    .collect(),
            ),
            other => other,
        })
        .collect()
}

/// Converts in place, returning whether the file was fragmented. With `cut_short`, a file whose
/// writing stopped partway is accepted: its unfinished last box, and any fragment whose data it
/// held, are dropped. `truncate` cuts the file when the new index ends before the old end.
/// Marks the old indexes as padding, the initial `moov` last.
fn pad(file: &mut impl Storage, boxes: &[Header], init: &Header) -> io::Result<()> {
    for header in boxes.iter().filter(|h| &h.kind == b"moof").chain([init]) {
        file.seek(SeekFrom::Start(header.start + 4))?;
        file.write_all(b"free")?;
    }
    file.flush()
}

fn convert(file: &mut impl Storage, cut_short: bool) -> io::Result<bool> {
    let len = file.seek(SeekFrom::End(0))?;
    let mut top = Vec::new();
    let mut end = 0;
    while end < len {
        match Header::read(file, end, len) {
            Ok(header) => {
                end += header.size;
                top.push(header);
            }
            Err(e) if cut_short && e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
    }
    if !top.iter().any(|h| &h.kind == b"moof") {
        return Ok(false);
    }
    let init_header = top.iter().find(|h| &h.kind == b"moov").ok_or_else(|| invalid("no moov"))?;
    // A conversion that was interrupted (obs-lite stopped while marking the padding) already
    // wrote the full index after the fragments: only the padding is left to do, rather than
    // adding a second index made from the fragments not yet marked.
    let first_moof = top.iter().position(|h| &h.kind == b"moof").expect("checked above");
    let indexed = top[first_moof..].iter().any(|h| &h.kind == b"moov" && h.read_body(file).is_ok_and(|b| parse(&b).is_ok()));
    if indexed {
        pad(file, &top, init_header)?;
        return Ok(true);
    }
    let init = parse(&init_header.read_body(file)?)?;

    let mut tracks = Vec::new();
    for node in &init {
        if let Node::Container(kind, trak) = node
            && kind == b"trak"
        {
            let tkhd = leaf(trak, b"tkhd")?;
            let id_at = if tkhd[0] == 1 { 20 } else { 12 };
            tracks.push(Track { id: u32_at(tkhd, id_at)?, ..Track::default() });
        }
    }
    if let Ok(mvex) = leaf(&init, b"mvex") {
        for node in parse(mvex)? {
            if let Node::Leaf(kind, trex) = node
                && &kind == b"trex"
            {
                let id = u32_at(&trex, 4)?;
                if let Some(track) = tracks.iter_mut().find(|t| t.id == id) {
                    track.defaults =
                        Defaults { description: u32_at(&trex, 8)?, duration: u32_at(&trex, 12)?, size: u32_at(&trex, 16)?, flags: u32_at(&trex, 20)? };
                }
            }
        }
    }
    for header in top.iter().filter(|h| &h.kind == b"moof") {
        let before: Vec<(usize, usize)> = tracks.iter().map(|t| (t.samples.len(), t.chunks.len())).collect();
        read_fragment(&header.read_body(file)?, header.start, &mut tracks)?;
        let whole = tracks.iter().zip(&before).all(|(t, &(samples, chunks))| {
            let mut samples = t.samples[samples..].iter();
            t.chunks[chunks..].iter().all(|c| c.position + samples.by_ref().take(c.samples as usize).map(|s| u64::from(s.size)).sum::<u64>() <= end)
        });
        if !whole {
            if !cut_short {
                return Err(invalid("samples past the end of the file"));
            }
            for (t, (samples, chunks)) in tracks.iter_mut().zip(before) {
                t.samples.truncate(samples);
                t.chunks.truncate(chunks);
            }
            break;
        }
    }
    let index = full_index(init, &tracks)?;

    // The new index goes where the fragment index (`mfra`) was, or at the end; the old indexes
    // become padding last, so the file plays at every step.
    let at = top.iter().find(|h| &h.kind == b"mfra").map_or(end, |h| h.start);
    file.seek(SeekFrom::Start(at))?;
    file.write_all(&index)?;
    let new_end = at + index.len() as u64;
    if new_end < len {
        file.truncate(new_end)?;
    }
    file.sync()?;
    pad(file, &top, init_header)?;
    Ok(true)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    #[ignore]
    fn recovery_is_idempotent_on_a_copy_of_a_real_recording() {
        let source = std::env::var_os("OPNOVERLAY_TEST_RECOVERY_SOURCE").expect("recording source required");
        let directory = tempfile::tempdir().unwrap();
        let copy = directory.path().join("recording.mp4");
        std::fs::copy(source, &copy).unwrap();
        let repaired = repair(&copy).expect("recording copy could not be inspected or repaired");
        assert!(!repair(&copy).expect("second repair failed"), "recovery must be idempotent");
        eprintln!("Copy required recovery: {repaired}");
    }

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut data = (u32::from(version) << 24 | flags).to_be_bytes().to_vec();
        data.extend_from_slice(body);
        bx(kind, &data)
    }

    fn be(values: &[u32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_be_bytes()).collect()
    }

    /// An initial `moov` laid out as Media Foundation writes it: `mvhd` version 1, `tkhd` and
    /// `mdhd` version 0, empty sample tables, and `mvex`.
    fn init_moov() -> Vec<u8> {
        let mut mvhd = vec![0; 16];
        mvhd.extend_from_slice(&60_000u32.to_be_bytes());
        mvhd.extend_from_slice(&[0; 88]);
        let trak = |id: u32, scale: u32, handler: &[u8; 4], entry: &[u8; 4]| {
            let tkhd = full(b"tkhd", 0, 1, &[be(&[0, 0, id, 0, 0]), vec![0; 60]].concat());
            let mdhd = full(b"mdhd", 0, 0, &[be(&[0, 0, scale, 0]), vec![0; 4]].concat());
            let hdlr = full(b"hdlr", 0, 0, &[vec![0; 4], handler.to_vec(), vec![0; 13]].concat());
            let stsd = full(b"stsd", 0, 0, &[be(&[1]), bx(entry, &[7; 20])].concat());
            let stbl = bx(
                b"stbl",
                &[
                    stsd,
                    full(b"stts", 0, 0, &be(&[0])),
                    full(b"stsc", 0, 0, &be(&[0])),
                    full(b"stsz", 0, 0, &be(&[0, 0])),
                    full(b"stco", 0, 0, &be(&[0])),
                    full(b"stss", 0, 0, &be(&[0])),
                ]
                .concat(),
            );
            let minf = bx(b"minf", &[full(b"vmhd", 0, 1, &[0; 8]), bx(b"dinf", &full(b"dref", 0, 0, &be(&[0]))), stbl].concat());
            bx(b"trak", &[tkhd, bx(b"mdia", &[mdhd, hdlr, minf].concat())].concat())
        };
        let trex = |id: u32| full(b"trex", 0, 0, &be(&[id, 1, 0, 0, 0]));
        bx(
            b"moov",
            &[
                full(b"mvhd", 1, 0, &mvhd),
                trak(1, 60_000, b"vide", b"avc1"),
                trak(2, 48_000, b"soun", b"mp4a"),
                bx(b"mvex", &[trex(1), trex(2)].concat()),
            ]
            .concat(),
        )
    }

    /// Bytes of a sample, so each can be told apart.
    fn content(track: u32, n: usize, size: u32) -> Vec<u8> {
        vec![(track as usize * 100 + n) as u8; size as usize]
    }

    /// How a fragment says where its data is.
    #[derive(Clone, Copy)]
    enum Style {
        /// Media Foundation: each track gives the absolute position of its data (video first).
        Absolute,
        /// GStreamer: no position; the first track's (audio's) data offset counts from the
        /// `moof`, and the next track's data follows it.
        Default,
    }

    /// One `moof` + `mdat` at `at`, with `video` and `audio` samples (duration, size, flags,
    /// composition offset), numbered from `first`.
    fn fragment(at: usize, video: &[Sample], audio: &[Sample], first: (usize, usize), style: Style) -> Vec<u8> {
        let build = |base: u64| {
            let traf = |id: u32, samples: &[Sample], data_offset: Option<u32>| {
                let tfhd = match style {
                    Style::Absolute => full(b"tfhd", 0, 1, &[be(&[id]), base.to_be_bytes().to_vec(), be(&[0])].concat()),
                    Style::Default => full(b"tfhd", 0, 0, &be(&[id])),
                };
                let mut body = be(&[samples.len() as u32]);
                body.extend(data_offset.map(|o| be(&[o])).unwrap_or_default());
                for s in samples {
                    let flags = if s.sync { 0 } else { 0x1_0000 };
                    body.extend(be(&[s.duration, s.size, flags, s.offset as i32 as u32]));
                }
                let trun_flags = if data_offset.is_some() { 0xF01 } else { 0xF00 };
                bx(b"traf", &[tfhd, full(b"trun", 1, trun_flags, &body)].concat())
            };
            let video_bytes: u32 = video.iter().map(|s| s.size).sum();
            let trafs = match style {
                Style::Absolute => [traf(1, video, Some(0)), traf(2, audio, Some(video_bytes))].concat(),
                // `base` here is the data's distance from the `moof`.
                Style::Default => [traf(2, audio, Some(base as u32)), traf(1, video, None)].concat(),
            };
            bx(b"moof", &[full(b"mfhd", 0, 0, &be(&[1])), trafs].concat())
        };
        let moof = build(0);
        let moof = match style {
            Style::Absolute => build((at + moof.len() + 8) as u64),
            Style::Default => build((moof.len() + 8) as u64),
        };
        let mut mdat = Vec::new();
        let video_data = video.iter().enumerate().flat_map(|(i, s)| content(1, first.0 + i, s.size));
        let audio_data = audio.iter().enumerate().flat_map(|(i, s)| content(2, first.1 + i, s.size));
        match style {
            Style::Absolute => mdat.extend(video_data.chain(audio_data)),
            Style::Default => mdat.extend(audio_data.chain(video_data)),
        }
        [moof, bx(b"mdat", &mdat)].concat()
    }

    fn video(sizes: &[u32], offsets: &[i64]) -> Vec<Sample> {
        sizes.iter().zip(offsets).enumerate().map(|(i, (&size, &offset))| Sample { size, duration: 1000, sync: i == 0, offset }).collect()
    }

    fn audio(count: usize) -> Vec<Sample> {
        vec![Sample { size: 40, duration: 1024, sync: true, offset: 0 }; count]
    }

    /// A fragmented file: three fragments, then a fragment index.
    pub(crate) fn fragmented() -> (Vec<u8>, Vec<Sample>, Vec<Sample>) {
        let mut file = [bx(b"ftyp", b"mp42\0\0\0\0mp41isom"), init_moov()].concat();
        let fragments = [
            (video(&[300, 20, 30], &[0, 2000, 0]), audio(2), Style::Absolute),
            (video(&[310, 25, 35], &[0, 0, 0]), audio(3), Style::Default),
            (video(&[290, 22], &[0, 0]), audio(2), Style::Absolute),
        ];
        let (mut all_video, mut all_audio) = (Vec::new(), Vec::new());
        for (v, a, style) in fragments {
            let f = fragment(file.len(), &v, &a, (all_video.len(), all_audio.len()), style);
            file.extend(f);
            all_video.extend(v);
            all_audio.extend(a);
        }
        file.extend(bx(b"mfra", &[0; 24]));
        (file, all_video, all_audio)
    }

    fn top_level(file: &[u8]) -> Vec<(u64, [u8; 4])> {
        let mut cursor = Cursor::new(file);
        let (mut at, mut out) = (0, Vec::new());
        while at < file.len() as u64 {
            let h = Header::read(&mut cursor, at, file.len() as u64).unwrap();
            out.push((h.start, h.kind));
            at += h.size;
        }
        out
    }

    fn table(stbl: &[Node], kind: &[u8; 4]) -> Vec<u32> {
        let body = leaf(stbl, kind).unwrap();
        body[4..].chunks(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect()
    }

    /// Reads a track's samples back through the regular index: bytes, durations, sync and
    /// composition offsets.
    fn read_back(file: &[u8], trak: &[Node]) -> Vec<(Vec<u8>, u32, bool, u32)> {
        let stbl = children(children(children(trak, b"mdia").unwrap(), b"minf").unwrap(), b"stbl").unwrap();
        let stsz = table(stbl, b"stsz");
        let count = stsz[1] as usize;
        let sizes: Vec<u32> = if stsz[0] != 0 { vec![stsz[0]; count] } else { stsz[2..].to_vec() };
        let stco = table(stbl, b"stco");
        let stsc = table(stbl, b"stsc");
        let mut per_chunk = Vec::new();
        for (i, entry) in stsc[1..].chunks(3).enumerate() {
            let last = stsc[1..].chunks(3).nth(i + 1).map_or(stco[0] + 1, |e| e[0]);
            per_chunk.extend((entry[0]..last).map(|_| entry[1]));
        }
        let mut positions = Vec::new();
        for (chunk, &n) in stco[1..].iter().zip(&per_chunk) {
            let mut p = *chunk as usize;
            for _ in 0..n {
                positions.push(p);
                p += sizes[positions.len() - 1] as usize;
            }
        }
        let stts = table(stbl, b"stts");
        let durations: Vec<u32> = stts[1..].chunks(2).flat_map(|e| vec![e[1]; e[0] as usize]).collect();
        let sync: Vec<bool> = match leaf(stbl, b"stss") {
            Ok(_) => {
                let list = table(stbl, b"stss");
                (1..=count as u32).map(|n| list[1..].contains(&n)).collect()
            }
            Err(_) => vec![true; count],
        };
        let offsets: Vec<u32> = match leaf(stbl, b"ctts") {
            Ok(_) => table(stbl, b"ctts")[1..].chunks(2).flat_map(|e| vec![e[1]; e[0] as usize]).collect(),
            Err(_) => vec![0; count],
        };
        (0..count).map(|i| (file[positions[i]..positions[i] + sizes[i] as usize].to_vec(), durations[i], sync[i], offsets[i])).collect()
    }

    impl Storage for Cursor<Vec<u8>> {
        fn truncate(&mut self, len: u64) -> io::Result<()> {
            self.get_mut().truncate(len as usize);
            Ok(())
        }

        fn sync(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A file in memory that logs what is done to it, and can stop working after some writes,
    /// as when obs-lite is stopped in the middle.
    struct Logged {
        file: Cursor<Vec<u8>>,
        log: Vec<&'static str>,
        writes_left: usize,
    }

    impl Read for Logged {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.file.read(buf)
        }
    }

    impl Seek for Logged {
        fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
            self.file.seek(to)
        }
    }

    impl Write for Logged {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.writes_left == 0 {
                return Err(io::Error::other("stopped"));
            }
            self.writes_left -= 1;
            self.log.push(if buf == b"free" { "pad" } else { "index" });
            self.file.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Storage for Logged {
        fn truncate(&mut self, len: u64) -> io::Result<()> {
            self.log.push("truncate");
            self.file.truncate(len)
        }

        fn sync(&mut self) -> io::Result<()> {
            self.log.push("sync");
            Ok(())
        }
    }

    /// `file` converted in memory.
    fn converted(file: Vec<u8>, cut_short: bool) -> io::Result<Vec<u8>> {
        let mut cursor = Cursor::new(file);
        convert(&mut cursor, cut_short)?;
        Ok(cursor.into_inner())
    }

    /// Checks that `file` is a regular MP4 holding exactly `video` and `audio`.
    fn assert_regular(file: &[u8], video: &[Sample], audio: &[Sample]) {
        let kinds: Vec<[u8; 4]> = top_level(file).into_iter().map(|(_, k)| k).collect();
        assert_eq!(kinds.first(), Some(b"ftyp"));
        assert_eq!(kinds.last(), Some(b"moov"), "one full index, at the end");
        assert_eq!(kinds.iter().filter(|k| *k == b"moov").count(), 1);
        assert!(!kinds.iter().any(|k| k == b"moof" || k == b"mfra"), "boxes: {kinds:?}");

        let (at, _) = *top_level(file).last().unwrap();
        let moov = parse(&file[at as usize + 8..]).unwrap();
        assert!(find(&moov, b"mvex").is_none(), "no fragment defaults left");
        let traks: Vec<&[Node]> = moov
            .iter()
            .filter_map(|n| match n {
                Node::Container([b't', b'r', b'a', b'k'], c) => Some(c.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(traks.len(), 2);

        let mut longest = 0;
        for (track, expected, trak, scale) in [(1, video, traks[0], 60_000), (2, audio, traks[1], 48_000)] {
            let samples = read_back(file, trak);
            assert_eq!(samples.len(), expected.len(), "track {track}");
            for (i, ((bytes, duration, sync, offset), want)) in samples.iter().zip(expected.iter()).enumerate() {
                assert_eq!(*bytes, content(track, i, want.size), "track {track} sample {i} data");
                assert_eq!((*duration, *sync, i64::from(*offset)), (want.duration, want.sync, want.offset), "track {track} sample {i}");
            }
            let mdhd = leaf(children(trak, b"mdia").unwrap(), b"mdhd").unwrap();
            let total: u64 = expected.iter().map(|s| u64::from(s.duration)).sum();
            assert_eq!(u64::from(u32_at(mdhd, 16).unwrap()), total, "track {track} duration");
            longest = longest.max((total * 60_000).div_ceil(scale));
        }
        // The movie lasts as long as its longest track, in the movie's timescale (60 000).
        assert_eq!(u64_at(leaf(&moov, b"mvhd").unwrap(), 24).unwrap(), longest);
    }

    #[test]
    fn a_fragmented_file_becomes_a_regular_one_with_every_sample_in_place() {
        let (original, video, audio) = fragmented();
        assert_regular(&converted(original, false).unwrap(), &video, &audio);
    }

    #[test]
    fn a_file_cut_short_is_repaired_up_to_its_last_whole_fragment() {
        // Writing stopped inside the third fragment's data: no fragment index, and part of the
        // last box missing. The first two fragments hold 6 video and 5 audio samples.
        let (mut file, video, audio) = fragmented();
        file.truncate(file.len() - 32 - 10);
        assert!(converted(file.clone(), false).is_err(), "a finished file must be whole");
        assert_regular(&converted(file, true).unwrap(), &video[..6], &audio[..5]);
    }

    #[test]
    fn a_regular_file_is_left_alone() {
        let regular = [bx(b"ftyp", b"mp42\0\0\0\0mp41isom"), bx(b"mdat", &[1; 50]), bx(b"moov", &[])].concat();
        let mut file = Logged { file: Cursor::new(regular.clone()), log: Vec::new(), writes_left: usize::MAX };
        assert!(!convert(&mut file, true).unwrap());
        assert!(file.log.is_empty(), "untouched: {:?}", file.log);
        assert_eq!(file.file.into_inner(), regular);
    }

    #[test]
    fn the_new_index_is_on_disk_before_the_old_ones_become_padding() {
        let (original, _, _) = fragmented();
        let mut file = Logged { file: Cursor::new(original), log: Vec::new(), writes_left: usize::MAX };
        convert(&mut file, false).unwrap();
        let first_pad = file.log.iter().position(|&op| op == "pad").unwrap();
        assert_eq!(file.log[first_pad - 1], "sync", "log: {:?}", file.log);
        assert_eq!(file.log[0], "index");
    }

    #[test]
    fn a_conversion_stopped_halfway_is_finished_with_one_index() {
        let (original, video, audio) = fragmented();
        // Stopped after the new index and the first padding mark.
        let mut file = Logged { file: Cursor::new(original), log: Vec::new(), writes_left: 2 };
        assert!(convert(&mut file, false).is_err());
        assert_eq!(file.log.iter().filter(|&&op| op == "pad").count(), 1);
        // Repaired at the next start.
        assert_regular(&converted(file.file.into_inner(), true).unwrap(), &video, &audio);
    }

    #[test]
    fn data_past_4_gib_is_indexed_with_64_bit_offsets() {
        let track = Track {
            samples: vec![Sample { size: 10, duration: 1, sync: true, offset: 0 }; 2],
            chunks: vec![Chunk { position: 100, samples: 1, description: 1 }, Chunk { position: 5_000_000_000, samples: 1, description: 1 }],
            ..Track::default()
        };
        let nodes = tables(&track);
        let Some(Node::Leaf(_, co64)) = find(&nodes, b"co64") else { panic!("no co64") };
        assert_eq!(u64_at(co64, 16).unwrap(), 5_000_000_000);
        assert!(find(&nodes, b"stco").is_none());
    }

    #[test]
    fn a_duration_too_long_for_32_bits_moves_the_box_to_version_1() {
        let mut mdhd = [be(&[0, 1, 2, 48_000, 0]), vec![0x55, 0xC4, 0, 0]].concat();
        set_duration(&mut mdhd, 4, 5_000_000_000);
        assert_eq!(mdhd[0], 1);
        assert_eq!((u64_at(&mdhd, 4).unwrap(), u64_at(&mdhd, 12).unwrap(), u32_at(&mdhd, 20).unwrap()), (1, 2, 48_000));
        assert_eq!(u64_at(&mdhd, 24).unwrap(), 5_000_000_000);
        assert_eq!(&mdhd[32..], &[0x55, 0xC4, 0, 0], "language kept");
    }
}
