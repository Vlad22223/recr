use crate::locale::tr;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;

use std::os::windows::ffi::{OsStrExt, OsStringExt};

use std::path::{Path, PathBuf};

use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::{Com::CoTaskMemFree, Registry::*, Threading::CreateMutexW};
use windows_sys::Win32::UI::{Shell::*, WindowsAndMessaging::*};

pub fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain([0]).collect()
}

pub fn message(hwnd: HWND, text: &str, error: bool) {
    unsafe {
        MessageBoxW(
            hwnd,
            wide(text).as_ptr(),
            wide("recr").as_ptr(),
            MB_OK
                | if error {
                    MB_ICONWARNING
                } else {
                    MB_ICONINFORMATION
                },
        );
    }
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("RECR_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(
                std::env::var_os("LOCALAPPDATA")
                    .unwrap_or_else(|| std::env::temp_dir().into_os_string()),
            )
            .join("recr-native")
        })
}

pub fn videos_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("RECR_VIDEOS") {
        return path.into();
    }
    let mut raw = null_mut();
    if unsafe { SHGetKnownFolderPath(&FOLDERID_Videos, 0, null_mut(), &mut raw) } >= 0 {
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) != 0 }).count();
        let path = PathBuf::from(OsString::from_wide(unsafe {
            std::slice::from_raw_parts(raw, len)
        }));
        unsafe { CoTaskMemFree(raw.cast()) };
        return path.join("Кадр");
    }
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Videos")
        .join("Кадр")
}

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

pub fn startup_enabled() -> bool {
    unsafe {
        let mut key = null_mut();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        ) != 0
        {
            return false;
        }
        let result = RegQueryValueExW(
            key,
            wide("recr-native").as_ptr(),
            null(),
            null_mut(),
            null_mut(),
            null_mut(),
        );
        RegCloseKey(key);
        result == 0
    }
}

pub fn set_startup(enabled: bool) -> io::Result<()> {
    let mut command = OsString::from("\"");
    command.push(std::env::current_exe()?);
    command.push("\" --tray");
    let command = wide(command);
    if enabled && command.len() > 260 {
        return Err(io::Error::other(tr(
            "Переместите приложение в папку с более коротким путём.",
            "Move the app to a folder with a shorter path.",
        )));
    }
    unsafe {
        let mut key = null_mut();
        let name = wide("recr-native");
        let result = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(RUN_KEY).as_ptr(),
            0,
            null(),
            0,
            KEY_SET_VALUE,
            null(),
            &mut key,
            null_mut(),
        );
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        let result = if enabled {
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                command.as_ptr().cast(),
                (command.len() * 2) as u32,
            )
        } else {
            RegDeleteValueW(key, name.as_ptr())
        };
        RegCloseKey(key);
        if result == 0 || (!enabled && result == ERROR_FILE_NOT_FOUND) {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(result as i32))
        }
    }
}

pub fn single_instance() -> io::Result<Option<Handle>> {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut mutex = "Local\\recr.Native.Recorder".to_owned();
    if let Some(profile) = std::env::var_os("RECR_DATA") {
        let mut hash = DefaultHasher::new();
        profile.hash(&mut hash);
        mutex.push_str(&format!(".{:016x}", hash.finish()));
    }
    unsafe {
        let handle = CreateMutexW(null(), 0, wide(mutex).as_ptr());
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let already_exists = GetLastError() == ERROR_ALREADY_EXISTS;
        let owned = Handle(handle);
        Ok((!already_exists).then_some(owned))
    }
}

pub struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

pub fn persist(path: &Path, value: &str) -> io::Result<()> {
    let draft = path.with_extension("tmp");
    fs::write(&draft, value)?;
    fs::rename(draft, path)
}
