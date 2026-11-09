//! Per-user registry values (`HKEY_CURRENT_USER` only) for the installer: the log-on Run key,
//! the user PATH, and clean-up on uninstall.

use std::io;

use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, LPARAM, WIN32_ERROR, WPARAM},
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE, REG_SZ,
            RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW,
            RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetValueExW,
        },
        UI::WindowsAndMessaging::{
            HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
        },
    },
    core::{PCWSTR, w},
};

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const ENV_KEY: &str = "Environment";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn check(rc: WIN32_ERROR) -> io::Result<()> {
    if rc == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(rc.0 as i32))
    }
}

/// A string value under `HKCU\<subkey>`, environment references left unexpanded.
pub fn get(subkey: &str, name: &str) -> io::Result<Option<String>> {
    let (k, n) = (wide(subkey), wide(name));
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut len = 0u32;
    // SAFETY: NUL-terminated names; the first call only reports the size in bytes, the second
    // fills a buffer of that size.
    unsafe {
        let rc = RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(k.as_ptr()),
            PCWSTR(n.as_ptr()),
            flags,
            None,
            None,
            Some(&mut len),
        );
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check(rc)?;
        let mut buf = vec![0u16; (len as usize).div_ceil(2)];
        check(RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(k.as_ptr()),
            PCWSTR(n.as_ptr()),
            flags,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        ))?;
        buf.truncate(len as usize / 2);
        while buf.last() == Some(&0) {
            buf.pop();
        }
        Ok(Some(String::from_utf16_lossy(&buf)))
    }
}

/// Set a string value under `HKCU\<subkey>` (`REG_EXPAND_SZ` if `expand`).
pub fn set(subkey: &str, name: &str, value: &str, expand: bool) -> io::Result<()> {
    let (k, n) = (wide(subkey), wide(name));
    let data: Vec<u8> = wide(value).iter().flat_map(|c| c.to_le_bytes()).collect();
    // SAFETY: NUL-terminated strings; the key handle is closed before returning.
    unsafe {
        let mut h = HKEY::default();
        check(RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(k.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut h,
            None,
        ))?;
        let rc = RegSetValueExW(
            h,
            PCWSTR(n.as_ptr()),
            None,
            if expand { REG_EXPAND_SZ } else { REG_SZ },
            Some(&data),
        );
        let _ = RegCloseKey(h);
        check(rc)
    }
}

/// Delete a value; a missing one is fine.
pub fn delete(subkey: &str, name: &str) -> io::Result<()> {
    let (k, n) = (wide(subkey), wide(name));
    // SAFETY: NUL-terminated strings.
    let rc =
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(k.as_ptr()), PCWSTR(n.as_ptr())) };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(rc)
}

/// Delete a key and everything under it; a missing one is fine.
pub fn delete_tree(subkey: &str) -> io::Result<()> {
    let k = wide(subkey);
    // SAFETY: NUL-terminated string.
    let rc = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(k.as_ptr())) };
    if rc == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(rc)
}

/// Tell Explorer the user environment changed, so new terminals see the new PATH. Runs on its
/// own thread: the broadcast waits on every top-level window in turn and can take many seconds.
pub fn broadcast_env_change() {
    std::thread::spawn(broadcast_now);
}

fn broadcast_now() {
    // SAFETY: broadcast with a static NUL-terminated string; the timeout keeps a hung window
    // from blocking us.
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(w!("Environment").as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            3000,
            None,
        );
    }
}

fn same_dir(a: &str, b: &str) -> bool {
    a.trim()
        .trim_end_matches('\\')
        .eq_ignore_ascii_case(b.trim().trim_end_matches('\\'))
}

/// `path` with `dir` added at the end, or `None` if it is already there.
pub fn path_with(path: &str, dir: &str) -> Option<String> {
    if path.split(';').any(|p| same_dir(p, dir)) {
        return None;
    }
    let base = path.trim_end_matches(';');
    Some(if base.is_empty() {
        dir.to_string()
    } else {
        format!("{base};{dir}")
    })
}

/// `path` without `dir`, or `None` if it isn't there.
pub fn path_without(path: &str, dir: &str) -> Option<String> {
    if !path.split(';').any(|p| same_dir(p, dir)) {
        return None;
    }
    Some(
        path.split(';')
            .filter(|p| !p.is_empty() && !same_dir(p, dir))
            .collect::<Vec<_>>()
            .join(";"),
    )
}

/// Add `dir` to the user PATH (kept as `REG_EXPAND_SZ`, other entries untouched).
pub fn add_to_user_path(dir: &str) -> io::Result<bool> {
    let cur = get(ENV_KEY, "Path")?.unwrap_or_default();
    match path_with(&cur, dir) {
        Some(new) => {
            set(ENV_KEY, "Path", &new, true)?;
            broadcast_env_change();
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Remove `dir` from the user PATH.
pub fn remove_from_user_path(dir: &str) -> io::Result<bool> {
    let cur = get(ENV_KEY, "Path")?.unwrap_or_default();
    match path_without(&cur, dir) {
        Some(new) => {
            set(ENV_KEY, "Path", &new, true)?;
            broadcast_env_change();
            Ok(true)
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_edits() {
        let d = r"C:\Users\x\AppData\Local\Programs\dishus";
        assert_eq!(path_with("", d).unwrap(), d);
        assert_eq!(
            path_with(r"%USERPROFILE%\bin;", d).unwrap(),
            format!(r"%USERPROFILE%\bin;{d}")
        );
        assert_eq!(path_with(&format!(r"a;{d}\;b"), d), None);
        assert_eq!(
            path_without(&format!("a;{d};b"), &d.to_uppercase()).unwrap(),
            "a;b"
        );
        assert_eq!(path_without("a;b", d), None);
    }

    #[test]
    fn values_roundtrip() {
        let key = r"Software\DiShusTest";
        set(key, "v", "%TEMP%\\x", true).unwrap();
        assert_eq!(get(key, "v").unwrap().unwrap(), "%TEMP%\\x");
        delete(key, "v").unwrap();
        assert_eq!(get(key, "v").unwrap(), None);
        delete_tree(key).unwrap();
        delete_tree(key).unwrap();
    }
}
