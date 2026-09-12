//! Autostart, via the per-user `Run` key.
//!
//! Per-user so no elevation is needed and nothing lands in machine-wide state.
//! The stored command is quoted, because the exe path commonly contains spaces
//! and an unquoted path silently fails to launch.

use crate::log;
use crate::win;

const RUN_KEY: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "ClipPlus";

pub fn is_enabled() -> bool {
    unsafe {
        let subkey = win::wide(RUN_KEY);
        let mut key: win::HKEY = 0;

        if win::RegOpenKeyExW(
            win::HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            win::KEY_QUERY_VALUE,
            &mut key,
        ) != win::ERROR_SUCCESS
        {
            return false;
        }

        let name = win::wide(VALUE_NAME);
        let mut size = 0u32;

        // Querying with a null buffer returns the size if the value exists.
        let result = win::RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        );

        win::RegCloseKey(key);
        result == win::ERROR_SUCCESS
    }
}

pub fn set_enabled(enabled: bool) -> bool {
    if enabled {
        enable()
    } else {
        disable()
    }
}

fn enable() -> bool {
    let Some(path) = exe_path() else {
        log::error("cannot resolve the executable path; autostart not set");
        return false;
    };

    let command = format!("\"{path}\"");
    let data = win::wide(&command);

    unsafe {
        let subkey = win::wide(RUN_KEY);
        let mut key: win::HKEY = 0;

        if win::RegOpenKeyExW(
            win::HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            win::KEY_SET_VALUE,
            &mut key,
        ) != win::ERROR_SUCCESS
        {
            log::error("cannot open the Run key for writing");
            return false;
        }

        let name = win::wide(VALUE_NAME);
        // The value is a UTF-16 string including its terminator, hence *2.
        let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);

        let result = win::RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            win::REG_SZ,
            bytes.as_ptr(),
            bytes.len() as u32,
        );

        win::RegCloseKey(key);
        result == win::ERROR_SUCCESS
    }
}

fn disable() -> bool {
    unsafe {
        let subkey = win::wide(RUN_KEY);
        let mut key: win::HKEY = 0;

        if win::RegOpenKeyExW(
            win::HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            win::KEY_SET_VALUE,
            &mut key,
        ) != win::ERROR_SUCCESS
        {
            return true; // nothing to remove
        }

        let name = win::wide(VALUE_NAME);
        let result = win::RegDeleteValueW(key, name.as_ptr());
        win::RegCloseKey(key);

        // A missing value is the desired end state, not a failure.
        result == win::ERROR_SUCCESS || result == 2
    }
}

pub fn exe_path() -> Option<String> {
    unsafe {
        let mut buffer = vec![0u16; 1024];
        let length = win::GetModuleFileNameW(0, buffer.as_mut_ptr(), buffer.len() as u32);
        if length == 0 {
            return None;
        }

        buffer.truncate(length as usize);
        Some(String::from_utf16_lossy(&buffer))
    }
}
