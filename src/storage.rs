use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Default, Debug)]
pub struct StorageReport {
    pub videos: u64,
    pub replays: u64,
    pub screenshots: u64,
    pub originals: u64,
    pub other: u64,
    pub missing: usize,
    pub inaccessible: usize,
    pub free: Option<u64>,
    pub folder_exists: bool,
}

impl StorageReport {
    pub fn total(&self) -> u64 {
        self.videos.saturating_add(self.replays).saturating_add(self.screenshots)
            .saturating_add(self.originals).saturating_add(self.other)
    }
}

pub fn measure(folder: &Path, files: &[PathBuf]) -> StorageReport {
    let mut report = StorageReport::default();
    let mut seen = HashSet::new();
    for path in files {
        let identity = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
        if !seen.insert(identity) { continue; }
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => {
                let extension = path.extension().unwrap_or_default().to_string_lossy();
                let stem = path.file_stem().unwrap_or_default().to_string_lossy().to_lowercase();
                let target = if extension.eq_ignore_ascii_case("mp4") {
                    if stem.contains(" — replay") || stem.contains(" — откат") { &mut report.replays } else { &mut report.videos }
                } else if extension.eq_ignore_ascii_case("png") || extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
                    &mut report.screenshots
                } else { &mut report.other };
                *target = target.saturating_add(meta.len());
            }
            Ok(_) => report.inaccessible += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => report.missing += 1,
            Err(_) => report.inaccessible += 1,
        }
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            let original = parent.join(".opnoverlay-originals").join(name);
            match std::fs::metadata(original) {
                Ok(meta) if meta.is_file() => {
                    report.originals = report.originals.saturating_add(meta.len());
                    let mut marker = name.to_os_string();
                    marker.push(".json");
                    match std::fs::metadata(parent.join(".opnoverlay-originals").join(marker)) {
                        Ok(meta) if meta.is_file() => report.originals = report.originals.saturating_add(meta.len()),
                        Ok(_) => report.inaccessible += 1,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                        Err(_) => report.inaccessible += 1,
                    }
                }
                Ok(_) => report.inaccessible += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(_) => report.inaccessible += 1,
            }
        }
    }
    (report.folder_exists, report.free) = disk_space(folder);
    report
}

pub fn disk_space(folder: &Path) -> (bool, Option<u64>) {
    let mut folder_exists = false;
    let mut free = None;
    if folder.is_absolute() {
        folder_exists = folder.is_dir();
        let mut existing = Some(folder);
        while let Some(path) = existing {
            match std::fs::metadata(path) {
                Ok(meta) if meta.is_dir() => { free = crate::platform::sys::free_space(path); break; }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => existing = path.parent(),
                _ => break,
            }
        }
    }
    (folder_exists, free)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_known_files_and_associated_originals_without_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let video = root.join("2026-01-01 — Video.mp4");
        let replay = root.join("2026-01-01 — Replay.mp4");
        let image = root.join("2026-01-01 — Screenshot.png");
        let missing = root.join("missing.mp4");
        std::fs::write(&video, [1; 10]).unwrap();
        std::fs::write(&replay, [1; 20]).unwrap();
        std::fs::write(&image, [1; 30]).unwrap();
        std::fs::write(root.join("unrelated.mp4"), [1; 90]).unwrap();
        let backups = root.join(".opnoverlay-originals");
        std::fs::create_dir(&backups).unwrap();
        std::fs::write(backups.join(video.file_name().unwrap()), [1; 40]).unwrap();
        let marker = backups.join(format!("{}.json", video.file_name().unwrap().to_string_lossy()));
        std::fs::write(&marker, b"{}").unwrap();
        let report = measure(root, &[video.clone(), video, replay, image, missing]);
        assert_eq!((report.videos, report.replays, report.screenshots, report.originals, report.total()), (10, 20, 30, 42, 102));
        assert_eq!(report.missing, 1);
        assert!(report.folder_exists);
        assert!(report.free.is_some());
        std::fs::remove_file(marker).unwrap();
        let report = measure(root, &[root.join("2026-01-01 — Video.mp4")]);
        assert_eq!(report.originals, 40);
    }

    #[test]
    fn missing_destination_uses_existing_parent_without_creating_directories() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("new").join("folder");
        let report = measure(&destination, &[]);
        assert!(!report.folder_exists);
        assert!(report.free.is_some());
        assert!(!destination.exists());
    }

    #[test]
    fn relative_destination_does_not_report_unrelated_disk_space() {
        let report = measure(Path::new("not-an-absolute-folder"), &[]);
        assert!(report.free.is_none());
        assert!(!report.folder_exists);
    }
}
