use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct Segment {
    file: Mutex<File>,
}

#[derive(Clone, Debug)]
pub struct Slice {
    segment: Arc<Segment>,
    offset: u64,
    len: usize,
}

impl Segment {
    #[cfg(test)]
    pub fn size(&self) -> io::Result<u64> {
        self.file.lock().map_err(|_| io::Error::other("replay segment lock poisoned"))?.metadata().map(|metadata| metadata.len())
    }

    pub fn new() -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self { file: Mutex::new(tempfile::tempfile()?) }))
    }

    pub fn append(self: &Arc<Self>, bytes: &[u8]) -> io::Result<Slice> {
        let mut file = self.file.lock().map_err(|_| io::Error::other("replay segment lock poisoned"))?;
        let offset = file.seek(SeekFrom::End(0))?;
        file.write_all(bytes)?;
        Ok(Slice { segment: self.clone(), offset, len: bytes.len() })
    }
}

impl Slice {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn read(&self) -> io::Result<Vec<u8>> {
        let mut file = self.segment.file.lock().map_err(|_| io::Error::other("replay segment lock poisoned"))?;
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = vec![0; self.len];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_remain_readable_after_writer_is_dropped() {
        let segment = Segment::new().unwrap();
        let first = segment.append(b"first").unwrap();
        let second = segment.append(b"second").unwrap();
        drop(segment);
        assert_eq!(second.read().unwrap(), b"second");
        assert_eq!(first.read().unwrap(), b"first");
    }
}
