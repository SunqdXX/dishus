//! Hybrid graphics (laptops with an integrated and a discrete GPU).
//!
//! Desktop Duplication only works from the GPU that drives the monitor. Windows decides per
//! program which GPU it runs on, and when a program runs on the "wrong" one, `DuplicateOutput`
//! fails with `DXGI_ERROR_UNSUPPORTED`. That choice is the per-app setting in Settings > System >
//! Display > Graphics, stored per user in `HKCU\Software\Microsoft\DirectX\UserGpuPreferences`.
//!
//! [`ensure`] checks capture in a fresh worker process (the preference is read when a process
//! starts using DXGI) and, only if it fails that way, sets dishus-daemon.exe's own preference to
//! power saving, then high performance, keeping the first that works. Nothing changes on PCs
//! where capture already works, and a failed attempt restores the previous value.

use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE,
            REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegGetValueW,
            RegSetValueExW,
        },
    },
    core::PCWSTR,
};

const KEY: &str = r"Software\Microsoft\DirectX\UserGpuPreferences";

/// Exit code of `--capture-worker dupcheck` when Desktop Duplication is unsupported from this
/// process's GPU.
pub const EXIT_UNSUPPORTED: i32 = 3;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// The stored preference for `exe` ("GpuPreference=1;"), if any.
pub fn get(exe: &Path) -> Option<String> {
    let (k, v) = (wide(KEY), wide(&exe.to_string_lossy()));
    let mut buf = vec![0u16; 256];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: NUL-terminated names; `len` is the buffer size in bytes.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(k.as_ptr()),
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

/// Store `value` for `exe`, or delete it with `None`.
pub fn set(exe: &Path, value: Option<&str>) -> Result<()> {
    let (k, v) = (wide(KEY), wide(&exe.to_string_lossy()));
    // SAFETY: NUL-terminated strings; the key handle is closed before returning.
    unsafe {
        let Some(value) = value else {
            let rc = RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(k.as_ptr()), PCWSTR(v.as_ptr()));
            return if rc == ERROR_SUCCESS || rc.0 == 2 {
                Ok(())
            } else {
                bail!("deleting GPU preference: error {}", rc.0)
            };
        };
        let mut h = HKEY::default();
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(k.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_QUERY_VALUE,
            None,
            &mut h,
            None,
        );
        if rc != ERROR_SUCCESS {
            bail!("opening {KEY}: error {}", rc.0);
        }
        let data: Vec<u8> = wide(value).iter().flat_map(|c| c.to_le_bytes()).collect();
        let rc = RegSetValueExW(h, PCWSTR(v.as_ptr()), None, REG_SZ, Some(&data));
        let _ = RegCloseKey(h);
        if rc != ERROR_SUCCESS {
            bail!("writing GPU preference: error {}", rc.0);
        }
    }
    Ok(())
}

enum Check {
    Works,
    Unsupported,
    Failed(String),
}

fn check(exe: &Path) -> Result<Check> {
    let out = std::process::Command::new(exe)
        .args(["--capture-worker", "dupcheck"])
        .env("DISHUS_W_TOKEN_FILE", "NUL")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .context("running the capture check")?;
    Ok(match out.status.code() {
        Some(0) => Check::Works,
        Some(EXIT_UNSUPPORTED) => Check::Unsupported,
        _ => Check::Failed(
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or("capture check failed")
                .to_string(),
        ),
    })
}

/// What [`ensure`] did.
pub enum Outcome {
    /// Capture works as things are.
    Works,
    /// Capture works after setting this preference ("power saving" / "high performance").
    Set(&'static str),
}

/// Make Desktop Duplication work for this program on hybrid-graphics PCs (see module docs).
pub fn ensure() -> Result<Outcome> {
    let exe = std::env::current_exe().context("locating own executable")?;
    match check(&exe)? {
        Check::Works => return Ok(Outcome::Works),
        Check::Unsupported => {}
        Check::Failed(e) => bail!("{e}"),
    }
    let previous = get(&exe);
    for (value, name) in [
        ("GpuPreference=1;", "power saving"),
        ("GpuPreference=2;", "high performance"),
    ] {
        if previous.as_deref() == Some(value) {
            continue;
        }
        set(&exe, Some(value))?;
        if matches!(check(&exe)?, Check::Works) {
            tracing::info!(
                "hybrid graphics: set Windows' graphics preference for {} to {name}, so the screen can be captured",
                exe.display()
            );
            return Ok(Outcome::Set(name));
        }
    }
    set(&exe, previous.as_deref())?;
    bail!(
        "Desktop Duplication is not supported from either GPU (DXGI_ERROR_UNSUPPORTED). On a \
         laptop, check Settings > System > Display > Graphics for dishus-daemon.exe."
    )
}
