//! Small Windows pieces of the daemon process: toast notifications, DPI awareness, and a
//! console for `--version` / `--doctor` (the daemon is a windows-subsystem program so that it
//! never opens a console window at login).
#![allow(unsafe_code)]

use windows::{
    Data::Xml::Dom::XmlDocument,
    UI::Notifications::{ToastNotification, ToastNotificationManager},
    Win32::{
        Foundation::{ERROR_SUCCESS, HANDLE},
        System::{
            Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE},
            Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
                RegCloseKey, RegCreateKeyExW, RegSetValueExW,
            },
        },
        UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
    },
    core::{HSTRING, PCWSTR},
};

/// The AppUserModelID our toasts are sent under. Unpackaged programs must register one under
/// `HKCU\Software\Classes\AppUserModelId` for Windows to show their toasts (no admin needed).
pub const AUMID: &str = "DiShus.Daemon";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Register [`AUMID`] with a display name, so toasts say "DiShus".
pub fn register_toast_app() -> std::io::Result<()> {
    let key = wide(&format!(r"Software\Classes\AppUserModelId\{AUMID}"));
    let name = wide("DisplayName");
    let value: Vec<u8> = wide("DiShus")
        .iter()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    // SAFETY: NUL-terminated wide strings; the key handle is closed below.
    unsafe {
        let mut hkey = HKEY::default();
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        );
        if rc != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(rc.0 as i32));
        }
        let rc = RegSetValueExW(hkey, PCWSTR(name.as_ptr()), None, REG_SZ, Some(&value));
        let _ = RegCloseKey(hkey);
        if rc != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(rc.0 as i32));
        }
    }
    Ok(())
}

fn xml_escape(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&apos;".into(),
            c => c.to_string(),
        })
        .collect()
}

/// Show a toast with a title and one line of text.
pub fn toast(summary: &str, body: &str) -> windows::core::Result<()> {
    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(summary),
        xml_escape(body)
    )))?;
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)
}

/// Real pixels on every monitor (Desktop Duplication requires it for `DuplicateOutput1`, and
/// monitor sizes are reported unscaled).
pub fn set_dpi_aware() {
    // SAFETY: process-wide setting with no pointers involved; fails harmlessly if already set.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// When started from a terminal without redirected output (`dishus-daemon --version`), print
/// into that terminal. With redirected output (the CLI runs `--doctor` through a pipe or its own
/// console handles) the inherited handles already work and nothing changes.
pub fn attach_parent_console() {
    // SAFETY: plain Win32 calls without pointers.
    unsafe {
        let has_stdout = GetStdHandle(STD_OUTPUT_HANDLE)
            .map(|h| h != HANDLE::default() && !h.is_invalid())
            .unwrap_or(false);
        if !has_stdout {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

/// A string value under `HKEY_LOCAL_MACHINE\<path>` (read only).
pub fn hklm_string(path: &str, value: &str) -> Option<String> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let (p, v) = (wide(path), wide(value));
    let mut buf = vec![0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: NUL-terminated names; `len` is the buffer size in bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(p.as_ptr()),
            PCWSTR(v.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut len),
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    buf.truncate((len as usize / 2).saturating_sub(1));
    Some(String::from_utf16_lossy(&buf))
}

/// `CREATE_NO_WINDOW`: child processes (the capture worker) never get a console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(test)]
mod tests {
    #[test]
    fn escaping() {
        assert_eq!(
            super::xml_escape("a<b>&\"c'"),
            "a&lt;b&gt;&amp;&quot;c&apos;"
        );
    }
}
