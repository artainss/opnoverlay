//! Small Windows system calls the rest of the app needs.

use std::path::{Path, PathBuf};
use std::process::Command;

use windows::Win32::Foundation::HWND;

use crate::i18n::Language;

pub struct SingleInstance(windows::Win32::Foundation::HANDLE);

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe { let _ = windows::Win32::Foundation::CloseHandle(self.0); }
    }
}

pub fn single_instance() -> windows::core::Result<Option<SingleInstance>> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    let path = crate::config::path().ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32), "Не найдена папка настроек"))?;
    let mut hasher = DefaultHasher::new();
    path.to_string_lossy().to_lowercase().hash(&mut hasher);
    let name: Vec<u16> = format!("Local\\opnoverlay-{:016x}", hasher.finish()).encode_utf16().chain(Some(0)).collect();
    unsafe {
        let handle = CreateMutexW(None, false, PCWSTR(name.as_ptr()))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            Ok(None)
        } else {
            Ok(Some(SingleInstance(handle)))
        }
    }
}

pub fn show_startup_error(error: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let text: Vec<u16> = format!("Не удалось проверить запуск opnoverlay: {error}").encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = "opnoverlay".encode_utf16().chain(Some(0)).collect();
    unsafe { MessageBoxW(None, PCWSTR(text.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONERROR); }
}

#[cfg(test)]
mod single_instance_tests {
    #[test]
    fn duplicate_is_rejected_until_first_handle_is_dropped() {
        let first = super::single_instance().unwrap().unwrap();
        assert!(super::single_instance().unwrap().is_none());
        drop(first);
        assert!(super::single_instance().unwrap().is_some());
    }
}

pub fn interactive_desktop_available() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, UOI_NAME};
    unsafe {
        let Ok(desktop) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) else { return false };
        let mut name = [0u16; 256];
        let ok = GetUserObjectInformationW(HANDLE(desktop.0), UOI_NAME, Some(name.as_mut_ptr().cast()), (name.len() * 2) as u32, None).is_ok();
        let _ = CloseDesktop(desktop);
        let end = name.iter().position(|c| *c == 0).unwrap_or(name.len());
        ok && String::from_utf16_lossy(&name[..end]).eq_ignore_ascii_case("Default")
    }
}

pub fn free_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::SHGetDiskFreeSpaceExW;
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0u64;
    unsafe { SHGetDiskFreeSpaceExW(PCWSTR(path.as_ptr()), Some(&mut available), None, None) }
        .as_bool().then_some(available)
}

/// The main window's handle, for the calls below.
pub fn window_handle(cc: &eframe::CreationContext<'_>) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

#[cfg(feature = "visual-test")]
pub fn move_to_test_desktop(window: isize) -> windows::core::Result<()> {
    use windows::core::GUID;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
    use windows::Win32::UI::Shell::{IVirtualDesktopManager, VirtualDesktopManager};
    let desktop = std::env::var("OPNOVERLAY_TEST_DESKTOP").expect("test desktop is required");
    let desktop = GUID::try_from(desktop.as_str())?;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let manager: IVirtualDesktopManager = CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_INPROC_SERVER)?;
        manager.MoveWindowToDesktop(HWND(window as *mut _), &desktop)
    }
}

pub fn square_window_corners(window: isize) {
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND};
    let preference = DWMWCP_DONOTROUND;
    let _ = unsafe {
        DwmSetWindowAttribute(HWND(window as *mut _), DWMWA_WINDOW_CORNER_PREFERENCE, std::ptr::from_ref(&preference).cast(), std::mem::size_of_val(&preference) as u32)
    };
    let _ = unsafe {
        DwmSetWindowAttribute(HWND(window as *mut _), DWMWA_BORDER_COLOR, std::ptr::from_ref(&DWMWA_COLOR_NONE).cast(), std::mem::size_of_val(&DWMWA_COLOR_NONE) as u32)
    };
}

pub fn prepare_slide_in(window: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowPlacement, IsIconic, SetWindowPlacement, WINDOWPLACEMENT};
    let hwnd = HWND(window as *mut _);
    unsafe {
        let mut placement = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
        if IsIconic(hwnd).as_bool() && GetWindowPlacement(hwnd, &mut placement).is_ok() {
            let width = placement.rcNormalPosition.right - placement.rcNormalPosition.left;
            placement.rcNormalPosition.left = -width;
            placement.rcNormalPosition.right = 0;
            let _ = SetWindowPlacement(hwnd, &placement);
        }
    }
}

pub fn move_minimized_offscreen(window: isize) {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsIconic, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER};
    let hwnd = HWND(window as *mut _);
    unsafe {
        let mut rect = RECT::default();
        if IsIconic(hwnd).as_bool() && GetWindowRect(hwnd, &mut rect).is_ok() && (rect.left > -32000 || rect.top > -32000) {
            let _ = SetWindowPos(hwnd, None, -32000, -32000, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

/// Shows or removes the window's taskbar button (minimize to tray).
pub fn set_taskbar_button(window: isize, visible: bool) {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx};
    use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW};
    unsafe {
        let hwnd = HWND(window as *mut _);
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let desired = if visible {
            (style & !(WS_EX_TOOLWINDOW.0 as isize)) | WS_EX_APPWINDOW.0 as isize
        } else {
            (style & !(WS_EX_APPWINDOW.0 as isize)) | WS_EX_TOOLWINDOW.0 as isize
        };
        if style != desired { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, desired); }
        // Already initialized as STA by winit on this thread; this only makes sure.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let Ok(list) = CoCreateInstance::<_, ITaskbarList>(&TaskbarList, None, CLSCTX_INPROC_SERVER) else { return };
        if list.HrInit().is_ok() {
            let _ = if visible { list.AddTab(hwnd) } else { list.DeleteTab(hwnd) };
        }
    }
}

/// Keeps the window out of screen captures (ours and other programs').
pub fn exclude_from_capture(window: isize, exclude: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE};
    let affinity = if exclude { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE };
    // Fails before Windows 10 2004; the window then simply stays capturable.
    let _ = unsafe { SetWindowDisplayAffinity(HWND(window as *mut _), affinity) };
}

pub fn foreground_is_external() -> bool {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let window = unsafe { GetForegroundWindow() };
    if window.0.is_null() { return false; }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)); }
    pid != 0 && pid != unsafe { GetCurrentProcessId() }
}

pub fn prepare_hud_window(title: &str, exclude: bool) {
    use windows::core::HSTRING;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, GetWindowLongPtrW, GetWindowThreadProcessId, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW};
    let Ok(window) = (unsafe { FindWindowW(None, &HSTRING::from(title)) }) else { return };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)); }
    if pid != unsafe { GetCurrentProcessId() } { return; }
    if title == "opnoverlay notification" {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Gdi::{CreateRoundRectRgn, DeleteObject, GetWindowRgnBox, SetWindowRgn, ERROR};
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        square_window_corners(window.0 as isize);
        let mut bounds = RECT::default();
        let mut region_bounds = RECT::default();
        if unsafe { GetWindowRect(window, &mut bounds) }.is_ok() {
            let (width, height) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
            let region_type = unsafe { GetWindowRgnBox(window, &mut region_bounds) };
            if width > 0 && height > 1 && (region_type.0 == ERROR || region_bounds.top != 1 || region_bounds.right != width || region_bounds.bottom != height) {
                let region = unsafe { CreateRoundRectRgn(0, 1, width, height, 20, 20) };
                if !region.is_invalid() && unsafe { SetWindowRgn(window, Some(region), true) } == 0 {
                    let _ = unsafe { DeleteObject(region.into()) };
                }
            }
        }
    }
    let style = unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) };
    let desired = (style & !(WS_EX_APPWINDOW.0 as isize)) | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize;
    if style != desired {
        unsafe { SetWindowLongPtrW(window, GWL_EXSTYLE, desired); }
        set_taskbar_button(window.0 as isize, false);
    }
    exclude_from_capture(window.0 as isize, exclude);
}

/// CPU time used by this process so far, in 100 ns units.
pub fn process_cpu_time() -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    Some(ticks(kernel) + ticks(user))
}

pub fn system_cpu_times() -> Option<(u64, u64)> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::GetSystemTimes;
    let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    Some((ticks(idle), ticks(kernel).checked_add(ticks(user))?))
}

pub fn physical_memory() -> Option<(u64, u64)> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut memory = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    unsafe { GlobalMemoryStatusEx(&mut memory) }.ok()?;
    Some((memory.ullTotalPhys.checked_sub(memory.ullAvailPhys)?, memory.ullTotalPhys))
}

#[cfg(test)]
mod system_metrics_tests {
    #[test]
    fn windows_returns_physical_memory_and_cpu_counters() {
        let (used, total) = super::physical_memory().unwrap();
        assert!(total > 0 && used <= total);
        let (idle, total) = super::system_cpu_times().unwrap();
        assert!(total > 0 && idle <= total);
    }
}

/// The Windows display language.
pub fn system_language() -> Language {
    Language::from_lang_id(unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() })
}

/// Release builds use the GUI subsystem; reattach to the parent console for selftest output.
pub fn attach_console() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    unsafe {
        AttachConsole(u32::MAX);
    }
}

/// Opens `dir` in Explorer.
pub fn open_folder(dir: &Path) {
    let _ = Command::new("explorer").arg(dir).spawn();
}

/// Opens the folder that holds `file` in Explorer, with the file selected.
pub fn show_in_folder(file: &Path) {
    let _ = Command::new("explorer").arg("/select,").arg(file).spawn();
}

pub fn open_file(file: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{PCWSTR, w};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let path: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe { ShellExecuteW(None, w!("open"), PCWSTR(path.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    if result.0 as isize > 32 { Ok(()) } else { Err(crate::i18n::tr3!("Windows no pudo abrir el archivo (código {}).", "Windows could not open the file (code {}).", "Windows не смогла открыть файл (код {}).", result.0 as isize)) }
}

pub fn recycle_file(file: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName, FOF_ALLOWUNDO, FOF_SILENT, FOF_WANTNUKEWARNING, FOFX_RECYCLEONDELETE};
    if !file.is_absolute() || !file.is_file() {
        return Err(crate::i18n::tr3!("Selecciona un archivo existente con su ruta completa.", "Select an existing file using its full path.", "Укажите существующий файл с полным путём.").into());
    }
    let path: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() }.map_err(|e| e.to_string())?;
    let result = (|| -> Result<(), String> {
        let operation: IFileOperation = unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) }.map_err(|e| e.to_string())?;
        unsafe { operation.SetOperationFlags(FOFX_RECYCLEONDELETE | FOF_ALLOWUNDO | FOF_WANTNUKEWARNING | FOF_SILENT) }.map_err(|e| e.to_string())?;
        let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None) }.map_err(|e| e.to_string())?;
        unsafe { operation.DeleteItem(&item, None) }.map_err(|e| e.to_string())?;
        unsafe { operation.PerformOperations() }.map_err(|e| e.to_string())?;
        if unsafe { operation.GetAnyOperationsAborted() }.map_err(|e| e.to_string())?.as_bool() {
            return Err(crate::i18n::tr3!("Operación cancelada", "Operation cancelled", "Операция отменена").into());
        }
        if file.exists() {
            return Err(crate::i18n::tr3!("El archivo no se ha movido.", "The file was not moved.", "Файл остался на месте.").into());
        }
        Ok(())
    })();
    unsafe { CoUninitialize() };
    result
}

pub fn rename_file(source: &Path, target: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileW(existing: *const u16, new: *const u16) -> i32;
    }
    let old: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let new: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe { MoveFileW(old.as_ptr(), new.as_ptr()) } == 0 {
        Err(std::io::Error::last_os_error().to_string())
    } else {
        Ok(())
    }
}

fn startup_command(executable: &Path) -> Result<String, String> {
    if !executable.is_absolute() { return Err(crate::i18n::tr3!("La ruta de la aplicación debe ser absoluta.", "The application path must be absolute.", "Путь к приложению должен быть абсолютным.").into()); }
    let command = format!("\"{}\" --start-minimized", executable.display());
    if command.encode_utf16().count() > 259 { return Err(crate::i18n::tr3!("La ruta de la aplicación es demasiado larga para el inicio de Windows.", "The application path is too long for Windows startup.", "Путь к приложению слишком длинный для автозапуска Windows.").into()); }
    Ok(command)
}

fn startup_entry(write: Option<bool>) -> Result<bool, String> {
    use windows::core::w;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    startup_entry_at(w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"), w!("opnoverlay"), &exe, write)
}

fn startup_entry_at(key_path: windows::core::PCWSTR, value_name: windows::core::PCWSTR, exe: &Path, write: Option<bool>) -> Result<bool, String> {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, REG_VALUE_TYPE, RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW};
    struct Key(HKEY);
    impl Drop for Key { fn drop(&mut self) { unsafe { let _ = RegCloseKey(self.0); } } }
    let expected = startup_command(exe)?;
    let mut raw = HKEY::default();
    let access = if write.is_some() { KEY_QUERY_VALUE | KEY_SET_VALUE } else { KEY_QUERY_VALUE };
    let result = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key_path, None, access, &mut raw) };
    if result != ERROR_SUCCESS { return Err(crate::i18n::tr3!("No se pudo abrir la configuración de inicio de Windows (código {}).", "Could not open Windows startup settings (code {}).", "Не удалось открыть настройки автозапуска Windows (код {}).", result.0)); }
    let key = Key(raw);
    let mut kind = REG_VALUE_TYPE::default();
    let mut size = 0u32;
    let query = unsafe { RegQueryValueExW(key.0, value_name, None, Some(&mut kind), None, Some(&mut size)) };
    let existing = if query == ERROR_FILE_NOT_FOUND {
        None
    } else if query != ERROR_SUCCESS {
        return Err(crate::i18n::tr3!("No se pudo leer el inicio automático (código {}).", "Could not read startup entry (code {}).", "Не удалось прочитать автозапуск (код {}).", query.0));
    } else {
        if kind != REG_SZ || size > 4096 || size % 2 != 0 { return Err(crate::i18n::tr3!("La entrada de inicio tiene un formato desconocido.", "The startup entry has an unknown format.", "Занята запись автозапуска с неизвестным форматом.").into()); }
        let mut bytes = vec![0u8; size as usize];
        let query = unsafe { RegQueryValueExW(key.0, value_name, None, Some(&mut kind), Some(bytes.as_mut_ptr()), Some(&mut size)) };
        if query != ERROR_SUCCESS || size as usize > bytes.len() || size % 2 != 0 { return Err(crate::i18n::tr3!("No se pudo leer la entrada de inicio.", "Could not read the startup entry.", "Не удалось прочитать запись автозапуска.").into()); }
        let wide: Vec<u16> = bytes[..size as usize].chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        Some(String::from_utf16(&wide).map_err(|_| crate::i18n::tr3!("Entrada de inicio no válida.", "Invalid startup entry.", "Некорректная запись автозапуска."))?.trim_end_matches('\0').to_owned())
    };
    if existing.as_ref().is_some_and(|value| !value.eq_ignore_ascii_case(&expected)) {
        return Err(crate::i18n::tr3!("El nombre opnoverlay ya pertenece a otra entrada de inicio.", "The opnoverlay startup entry belongs to another application.", "Имя opnoverlay уже занято другой записью автозапуска.").into());
    }
    match write {
        Some(true) if existing.is_none() => {
            let bytes: Vec<u8> = expected.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
            let result = unsafe { RegSetValueExW(key.0, value_name, None, REG_SZ, Some(&bytes)) };
            if result != ERROR_SUCCESS { return Err(crate::i18n::tr3!("No se pudo activar el inicio automático (código {}).", "Could not enable startup (code {}).", "Не удалось включить автозапуск (код {}).", result.0)); }
            Ok(true)
        }
        Some(false) if existing.is_some() => {
            let result = unsafe { RegDeleteValueW(key.0, value_name) };
            if result != ERROR_SUCCESS { return Err(crate::i18n::tr3!("No se pudo desactivar el inicio automático (código {}).", "Could not disable startup (code {}).", "Не удалось выключить автозапуск (код {}).", result.0)); }
            Ok(false)
        }
        Some(enabled) => Ok(enabled),
        None => Ok(existing.is_some()),
    }
}

pub fn startup_enabled() -> Result<bool, String> { startup_entry(None) }

pub fn set_startup(enabled: bool) -> Result<(), String> { startup_entry(Some(enabled)).map(|_| ()) }

#[cfg(test)]
mod recycle_tests {
    use super::*;

    #[test]
    fn rejects_relative_and_missing_files() {
        assert!(recycle_file(Path::new("relative.mp4")).is_err());
        let missing = std::env::temp_dir().join(format!("opnoverlay-missing-{}", std::process::id()));
        assert!(recycle_file(&missing).is_err());
    }

    #[test]
    fn startup_command_quotes_paths_and_rejects_invalid_ones() {
        let path = Path::new(r"C:\Program Files\opnoverlay\opnoverlay.exe");
        assert_eq!(startup_command(path).unwrap(), r#""C:\Program Files\opnoverlay\opnoverlay.exe" --start-minimized"#);
        assert!(startup_command(Path::new("relative.exe")).is_err());
        let long = format!("C:\\{}\\app.exe", "a".repeat(260));
        assert!(startup_command(Path::new(&long)).is_err());
    }

    #[test]
    fn startup_registry_round_trip_uses_only_an_isolated_test_key() {
        use windows::core::{PCWSTR, w};
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, RegCloseKey, RegCreateKeyW, RegDeleteKeyW};
        let suffix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let key_path = format!("Software\\opnoverlay-test-{}-{suffix}", std::process::id());
        let wide: Vec<u16> = key_path.encode_utf16().chain(Some(0)).collect();
        let key_name = PCWSTR(wide.as_ptr());
        let mut key = HKEY::default();
        assert_eq!(unsafe { RegCreateKeyW(HKEY_CURRENT_USER, key_name, &mut key) }, ERROR_SUCCESS);
        unsafe { let _ = RegCloseKey(key); }
        let exe = Path::new(r"C:\Program Files\opnoverlay\opnoverlay.exe");
        let other = Path::new(r"C:\Other\opnoverlay.exe");
        let result = || {
            assert!(!startup_entry_at(key_name, w!("test"), exe, None).unwrap());
            assert!(startup_entry_at(key_name, w!("test"), exe, Some(true)).unwrap());
            assert!(startup_entry_at(key_name, w!("test"), exe, None).unwrap());
            assert!(startup_entry_at(key_name, w!("test"), other, Some(false)).is_err());
            assert!(startup_entry_at(key_name, w!("test"), exe, None).unwrap());
            assert!(!startup_entry_at(key_name, w!("test"), exe, Some(false)).unwrap());
            assert!(!startup_entry_at(key_name, w!("test"), exe, None).unwrap());
        };
        let outcome = std::panic::catch_unwind(result);
        assert_eq!(unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, key_name) }, ERROR_SUCCESS);
        outcome.unwrap();
    }

    #[test]
    #[ignore]
    fn recycles_explicit_test_file() {
        let path = PathBuf::from(std::env::var_os("OPNOVERLAY_RECYCLE_TEST_FILE").expect("test file path required"));
        assert!(path.is_file());
        recycle_file(&path).unwrap();
        assert!(!path.exists());
    }
}

/// The path as the user should see it; on Windows, the path itself.
pub fn display_path(path: &Path) -> PathBuf {
    path.to_owned()
}

/// Files are never kept only in memory here (that is the Linux sandbox's case).
pub fn volatile(_path: &Path) -> bool {
    false
}

/// GET `url` and return the body as text.
pub fn http_get(url: &str) -> Result<String, String> {
    use windows::Foundation::Uri;
    use windows::Web::Http::HttpClient;
    use windows::core::HSTRING;
    let get = || -> windows::core::Result<String> {
        let client = HttpClient::new()?;
        // GitHub's API rejects requests without a User-Agent.
        let agent = concat!("obs-lite/", env!("CARGO_PKG_VERSION"));
        client.DefaultRequestHeaders()?.TryAppendWithoutValidation(&HSTRING::from("User-Agent"), &HSTRING::from(agent))?;
        Ok(client.GetStringAsync(&Uri::CreateUri(&HSTRING::from(url))?)?.join()?.to_string())
    };
    get().map_err(|e| e.to_string())
}
