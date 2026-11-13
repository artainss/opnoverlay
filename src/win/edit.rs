use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct History {
    original: u64,
    current: u64,
    previous: u64,
}

fn paths(path: &Path) -> Result<(PathBuf, PathBuf), String> {
    let name = path.file_name().ok_or("Не удалось определить имя видео.")?;
    let directory = path.parent().ok_or("Не удалось определить папку видео.")?.join(".opnoverlay-originals");
    let original = directory.join(name);
    let mut marker_name = name.to_os_string();
    marker_name.push(".json");
    Ok((original, directory.join(marker_name)))
}

fn ensure_backup_dir(path: &Path) -> Result<(), String> {
    match fs::create_dir(path) {
        Ok(()) => {
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn GetFileAttributesW(path: *const u16) -> u32;
                fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32;
            }
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
            if attributes == u32::MAX || unsafe { SetFileAttributesW(wide.as_ptr(), attributes | 2) } == 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn fingerprint(path: &Path) -> Result<u64, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hash = 0xcbf29ce484222325u64;
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 { break; }
        for byte in &buffer[..count] { hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3); }
    }
    Ok(hash)
}

struct EditGuard(PathBuf);

fn active_edits() -> &'static Mutex<std::collections::HashSet<PathBuf>> {
    static ACTIVE: OnceLock<Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

impl EditGuard {
    fn acquire(path: &Path) -> Result<Self, String> {
        let mut active = active_edits().lock().map_err(|e| e.to_string())?;
        if !active.insert(path.to_owned()) { return Err("Видео сейчас обрабатывается. Дождитесь завершения.".into()); }
        Ok(Self(path.to_owned()))
    }
}

impl Drop for EditGuard {
    fn drop(&mut self) { if let Ok(mut active) = active_edits().lock() { active.remove(&self.0); } }
}

fn replace(target: &Path, source: &Path) -> Result<(), String> {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ReplaceFileW(target: *const u16, source: *const u16, backup: *const u16, flags: u32, exclude: *const std::ffi::c_void, reserved: *const std::ffi::c_void) -> i32;
    }
    let metadata = fs::metadata(target).map_err(|e| e.to_string())?;
    let expected = (metadata.len(), metadata.modified().map_err(|e| e.to_string())?);
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    for attempt in 0..51 {
        let metadata = fs::metadata(target).map_err(|e| e.to_string())?;
        if (metadata.len(), metadata.modified().map_err(|e| e.to_string())?) != expected {
            return Err("Файл изменился во время сохранения; замена отменена.".into());
        }
        if unsafe { ReplaceFileW(target_wide.as_ptr(), source_wide.as_ptr(), std::ptr::null(), 0, std::ptr::null(), std::ptr::null()) } != 0 { return Ok(()); }
        let error = std::io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(32 | 33)) || attempt == 50 { return Err(error.to_string()); }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err("Не удалось заменить файл.".into())
}

fn write_history(marker: &Path, history: &History) -> Result<(), String> {
    let parent = marker.parent().ok_or("Не удалось определить папку истории.")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    serde_json::to_writer(&mut tmp, history).map_err(|e| e.to_string())?;
    tmp.flush().map_err(|e| e.to_string())?;
    tmp.as_file().sync_all().map_err(|e| e.to_string())?;
    if marker.exists() {
        let path = tmp.into_temp_path();
        replace(marker, &path)
    } else {
        tmp.persist_noclobber(marker).map(|_| ()).map_err(|e| e.to_string())
    }
}

fn read_history(path: &Path) -> Result<Option<History>, String> {
    let (original, marker) = paths(path)?;
    match (original.is_file(), marker.is_file()) {
        (false, false) => Ok(None),
        (true, true) => {
            let history: History = serde_json::from_reader(File::open(&marker).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            if fingerprint(&original)? != history.original { return Err("Резервная копия оригинала изменилась; автоматическое восстановление остановлено.".into()); }
            let current = fingerprint(path)?;
            if current != history.current && current != history.previous && current != history.original {
                return Err("Видео изменилось вне opnoverlay; оригинал сохранён в папке .opnoverlay-originals.".into());
            }
            Ok(Some(history))
        }
        _ => Err("История обрезки неполная; проверьте папку .opnoverlay-originals.".into()),
    }
}

pub fn has_original(path: &Path) -> bool {
    paths(path).is_ok_and(|(original, marker)| original.is_file() && marker.is_file())
}

pub fn trim_current(path: &Path, start: i64, end: i64) -> Result<(i64, i64), String> {
    let _guard = EditGuard::acquire(path)?;
    let (original, marker) = paths(path)?;
    let parent = original.parent().ok_or("Не удалось определить папку истории.")?;
    ensure_backup_dir(parent)?;
    let before = fingerprint(path)?;
    let history = read_history(path)?;
    let temp = tempfile::Builder::new().prefix("trim-").tempdir_in(parent).map_err(|e| e.to_string())?;
    let result_path = temp.path().join("edited.mp4");
    let bounds = super::trim::trim(path, &result_path, start, end)?;
    if fingerprint(path)? != before { return Err("Видео изменилось во время обрезки; результат не заменил файл.".into()); }
    let result_hash = fingerprint(&result_path)?;
    let created_backup = history.is_none();
    let original_hash = if let Some(history) = history { history.original } else {
        let mut source = File::open(path).map_err(|e| e.to_string())?;
        let mut backup = OpenOptions::new().write(true).create_new(true).open(&original).map_err(|e| e.to_string())?;
        let copied = std::io::copy(&mut source, &mut backup).and_then(|_| backup.sync_all());
        drop(backup);
        if let Err(error) = copied { let _ = fs::remove_file(&original); return Err(error.to_string()); }
        if fingerprint(&original)? != before || fingerprint(path)? != before { let _ = fs::remove_file(&original); return Err("Не удалось проверить резервную копию оригинала.".into()); }
        before
    };
    if let Err(error) = write_history(&marker, &History { original: original_hash, current: result_hash, previous: before }) {
        if created_backup && fingerprint(path).ok() == Some(before) { let _ = fs::remove_file(&original); }
        return Err(error);
    }
    replace(path, &result_path)?;
    Ok(bounds)
}

pub fn restore(path: &Path) -> Result<(), String> {
    let _guard = EditGuard::acquire(path)?;
    let (original, marker) = paths(path)?;
    let history = read_history(path)?.ok_or("Оригинал для восстановления не найден.")?;
    let before = fingerprint(path)?;
    let parent = original.parent().ok_or("Не удалось определить папку истории.")?;
    let temp = tempfile::Builder::new().prefix("restore-").tempdir_in(parent).map_err(|e| e.to_string())?;
    let replacement = temp.path().join("original.mp4");
    fs::copy(&original, &replacement).map_err(|e| e.to_string())?;
    if fingerprint(&replacement)? != history.original { return Err("Не удалось проверить копию оригинала.".into()); }
    if fingerprint(path)? != before { return Err("Видео изменилось во время восстановления; файл не заменён.".into()); }
    replace(path, &replacement)?;
    if fingerprint(path)? != history.original { return Err("Проверка восстановленного видео не прошла; резервная копия сохранена.".into()); }
    fs::remove_file(&original).map_err(|e| e.to_string())?;
    fs::remove_file(&marker).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn rename(path: &Path, target: &Path) -> Result<(), String> {
    let _guard = EditGuard::acquire(path)?;
    let (original, marker) = paths(path)?;
    let (new_original, new_marker) = paths(target)?;
    if !original.exists() && !marker.exists() { return super::sys::rename_file(path, target); }
    if !original.is_file() || !marker.is_file() || File::open(&marker).ok().and_then(|file| serde_json::from_reader::<_, History>(file).ok()).is_none() {
        return Err("История обрезки неполная; проверьте папку .opnoverlay-originals.".into());
    }
    if new_original.exists() || new_marker.exists() { return Err("Для этого имени уже существует история обрезки.".into()); }
    super::sys::rename_file(&original, &new_original)?;
    if let Err(error) = super::sys::rename_file(&marker, &new_marker) {
        if let Err(rollback) = super::sys::rename_file(&new_original, &original) {
            return Err(format!("Не удалось переименовать видео: {error}. Резервный оригинал остался в {}: {rollback}", new_original.display()));
        }
        return Err(error);
    }
    if let Err(error) = super::sys::rename_file(path, target) {
        let marker_result = super::sys::rename_file(&new_marker, &marker);
        let original_result = super::sys::rename_file(&new_original, &original);
        if marker_result.is_err() || original_result.is_err() {
            return Err(format!("Не удалось переименовать видео: {error}. Проверьте резервные файлы в папке .opnoverlay-originals."));
        }
        return Err(error);
    }
    Ok(())
}

pub fn recycle(path: &Path) -> Result<(), String> {
    let _guard = EditGuard::acquire(path)?;
    if !path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("mp4")) { return super::sys::recycle_file(path); }
    let (original, marker) = paths(path)?;
    let history = (original.is_file(), marker.is_file());
    super::sys::recycle_file(path)?;
    match history {
        (true, true) if File::open(&marker).ok().and_then(|file| serde_json::from_reader::<_, History>(file).ok()).is_some() => {
            super::sys::recycle_file(&original).map_err(|error| format!("Видео уже в корзине, но резервный оригинал остался на диске: {error}"))?;
            super::sys::recycle_file(&marker).map_err(|error| format!("Видео и оригинал уже в корзине, но служебный файл остался на диске: {error}"))?;
        }
        (false, false) => {}
        _ => return Err("Видео уже в корзине, но неполная резервная копия осталась в папке .opnoverlay-originals.".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_changes_do_not_replace_or_restore_video() {
        let folder = tempfile::tempdir().unwrap();
        let video = folder.path().join("video.mp4");
        fs::write(&video, b"unexpected change").unwrap();
        let (original, marker) = paths(&video).unwrap();
        fs::create_dir_all(original.parent().unwrap()).unwrap();
        fs::write(&original, b"original").unwrap();
        write_history(&marker, &History { original: fingerprint(&original).unwrap(), current: 1, previous: 2 }).unwrap();
        assert!(restore(&video).is_err());
        assert_eq!(fs::read(&video).unwrap(), b"unexpected change");
        assert_eq!(fs::read(&original).unwrap(), b"original");
    }

    #[test]
    #[ignore]
    fn deleting_an_edited_video_recycles_the_original_and_history() {
        let folder = tempfile::tempdir().unwrap();
        let name = format!("opnoverlay-test-{}-{}.mp4", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let video = folder.path().join(name);
        fs::write(&video, b"edited test video").unwrap();
        let (original, marker) = paths(&video).unwrap();
        ensure_backup_dir(original.parent().unwrap()).unwrap();
        fs::write(&original, b"original test video").unwrap();
        write_history(&marker, &History { original: fingerprint(&original).unwrap(), current: fingerprint(&video).unwrap(), previous: 0 }).unwrap();
        recycle(&video).unwrap();
        assert!(!video.exists());
        assert!(!original.exists());
        assert!(!marker.exists());
    }

    #[test]
    #[ignore]
    fn repeated_trims_rename_and_restore_preserve_original_bytes() {
        let source = PathBuf::from(std::env::var_os("OPNOVERLAY_CHECK_FILE").unwrap());
        let folder = tempfile::tempdir().unwrap();
        let video = folder.path().join("video.mp4");
        fs::copy(source, &video).unwrap();
        let original = fs::read(&video).unwrap();
        trim_current(&video, 0, 60_000_000).unwrap();
        assert!(has_original(&video));
        assert_ne!(fs::read(&video).unwrap(), original);
        trim_current(&video, 10_000_000, 40_000_000).unwrap();
        let renamed = folder.path().join("renamed.mp4");
        rename(&video, &renamed).unwrap();
        assert!(!has_original(&video));
        assert!(has_original(&renamed));
        restore(&renamed).unwrap();
        assert_eq!(fs::read(&renamed).unwrap(), original);
        assert!(!has_original(&renamed));
    }
}
