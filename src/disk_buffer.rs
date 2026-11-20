use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct DiskBudget {
    used: AtomicU64,
}

impl DiskBudget {
    fn reserve(&self, len: u64, limit: u64) -> bool {
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let Some(next) = used.checked_add(len).filter(|next| *next <= limit) else { return false };
            match self.used.compare_exchange_weak(used, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(current) => used = current,
            }
        }
    }
}

#[derive(Debug)]
struct Reservation {
    budget: Arc<DiskBudget>,
    bytes: AtomicU64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(*self.bytes.get_mut(), Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct Segment {
    file: Mutex<File>,
    reservation: Reservation,
    limit: u64,
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

    pub fn new(budget: Arc<DiskBudget>, limit: u64) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            file: Mutex::new(tempfile::tempfile()?),
            reservation: Reservation { budget, bytes: AtomicU64::new(0) },
            limit,
        }))
    }

    pub fn next(&self) -> io::Result<Arc<Self>> {
        Self::new(self.reservation.budget.clone(), self.limit)
    }

    pub fn has_space(&self, len: usize) -> bool {
        self.reservation.budget.used.load(Ordering::Relaxed).saturating_add(len as u64) <= self.limit
    }

    pub fn is_empty(&self) -> bool {
        self.reservation.bytes.load(Ordering::Relaxed) == 0
    }

    pub fn ready_to_rotate(&self) -> bool {
        self.reservation.bytes.load(Ordering::Relaxed) >= (self.limit / 8).max(1)
    }

    pub fn append(self: &Arc<Self>, bytes: &[u8]) -> io::Result<Slice> {
        let mut file = self.file.lock().map_err(|_| io::Error::other("replay segment lock poisoned"))?;
        let offset = file.seek(SeekFrom::End(0))?;
        let len = bytes.len() as u64;
        if !self.reservation.budget.reserve(len, self.limit) {
            return Err(io::Error::other("replay buffer byte limit reached"));
        }
        self.reservation.bytes.fetch_add(len, Ordering::Relaxed);
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
        let segment = Segment::new(Arc::default(), u64::MAX).unwrap();
        let first = segment.append(b"first").unwrap();
        let second = segment.append(b"second").unwrap();
        drop(segment);
        assert_eq!(second.read().unwrap(), b"second");
        assert_eq!(first.read().unwrap(), b"first");
    }

    #[test]
    fn rotations_and_new_sessions_share_the_budget_with_saved_snapshots() {
        let budget = Arc::new(DiskBudget::default());
        let first = Segment::new(budget.clone(), 8).unwrap();
        let snapshot = first.append(b"first").unwrap();
        let next = first.next().unwrap();
        drop(first);
        let restarted = Segment::new(budget, 8).unwrap();
        assert!(next.append(b"1234").is_err());
        assert!(restarted.append(b"1234").is_err());
        assert_eq!(snapshot.read().unwrap(), b"first");
        drop(snapshot);
        next.append(b"1234").unwrap();
        restarted.append(b"abcd").unwrap();
        assert!(next.append(b"!").is_err());
    }
}
