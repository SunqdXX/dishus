//! Windows helpers shared by the daemon and the CLI.
//!
//! These are the Windows counterparts of what Linux gets from file modes and `SO_PEERCRED`:
//! * private files and directories: an owner-only, protected DACL instead of `0600`/`0700`, and
//!   on read, a refusal of anything another account can reach;
//! * the control pipe: a per-user name, a DACL that only lets this user open it, and a check of
//!   the SID of the process at the other end (in both directions).
//!
//! Win32 calls need `unsafe`; on Windows it is confined to this crate and the daemon's capture
//! and input modules (see docs/SECURITY.md). Empty on other platforms.
#![cfg(windows)]
#![allow(unsafe_code)]

pub mod reg;

use std::{
    ffi::c_void,
    io,
    os::windows::{ffi::OsStrExt, io::AsRawHandle},
    path::{Path, PathBuf},
};

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorDacl, GetTokenInformation,
            INHERIT_ONLY_ACE, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::{
            Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
            Threading::{
                GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        },
    },
    core::{HSTRING, PCWSTR, PWSTR},
};

/// Accounts allowed in a private file's DACL besides the user: the OS itself (SYSTEM) and the
/// local Administrators group. Both can take any file anyway; they are Windows' `root`.
const TRUSTED_SIDS: [&str; 2] = ["S-1-5-18", "S-1-5-32-544"];

/// ACE types that grant access (allowed, allowed-object, allowed-callback, allowed-callback-object).
const ALLOW_ACE_TYPES: [u8; 4] = [0, 5, 9, 11];

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: we own the handle and close it once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn win_err(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
}

/// SID string ("S-1-5-21-…") of a SID pointer.
///
/// # Safety
/// `sid` must point to a valid SID.
unsafe fn sid_string(sid: PSID) -> io::Result<String> {
    let mut s = PWSTR::null();
    // SAFETY: valid SID per contract; `s` receives a LocalAlloc'd string we free below.
    unsafe {
        ConvertSidToStringSidW(sid, &mut s).map_err(win_err)?;
        let out = s.to_string();
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
        out.map_err(|_| io::Error::other("SID is not valid UTF-16"))
    }
}

fn token_user_sid(process: HANDLE) -> io::Result<String> {
    // SAFETY: plain Win32 calls on handles we own; the buffer is u64-aligned and large enough
    // for TOKEN_USER as reported by the first call.
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).map_err(win_err)?;
        let token = Handle(token);
        let mut len = 0u32;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            len,
            &mut len,
        )
        .map_err(win_err)?;
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        sid_string(user.User.Sid)
    }
}

/// The SID of the account this process runs as.
pub fn current_user_sid() -> io::Result<String> {
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing.
    token_user_sid(unsafe { GetCurrentProcess() })
}

/// The SID of the account another process runs as.
pub fn process_user_sid(pid: u32) -> io::Result<String> {
    // SAFETY: OpenProcess returns an owned handle, closed by `Handle`.
    let h = Handle(
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.map_err(win_err)?,
    );
    token_user_sid(h.0)
}

/// A self-relative security descriptor built from SDDL, freed on drop.
pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// The descriptor is an immutable heap block; nothing in it is thread-affine.
unsafe impl Send for SecurityDescriptor {}
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    pub fn from_sddl(sddl: &str) -> io::Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sd` receives a LocalAlloc'd descriptor that Drop frees.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .map_err(win_err)?;
        }
        Ok(Self(sd))
    }

    /// Full control for the current user only, inherited by everything created inside, and
    /// nothing inherited from the parent (`P` = protected).
    pub fn owner_only() -> io::Result<Self> {
        Self::from_sddl(&format!("D:P(A;OICI;FA;;;{})", current_user_sid()?))
    }

    /// For the control pipe: generic-all for the current user only.
    pub fn pipe() -> io::Result<Self> {
        Self::from_sddl(&format!("D:P(A;;GA;;;{})", current_user_sid()?))
    }

    fn dacl(&self) -> io::Result<*mut ACL> {
        let mut present = false.into();
        let mut defaulted = false.into();
        let mut acl: *mut ACL = std::ptr::null_mut();
        // SAFETY: valid descriptor; the ACL pointer points into it and lives as long as self.
        unsafe {
            GetSecurityDescriptorDacl(self.0, &mut present, &mut acl, &mut defaulted)
                .map_err(win_err)?;
        }
        if acl.is_null() {
            return Err(io::Error::other("descriptor has no DACL"));
        }
        Ok(acl)
    }

    /// `SECURITY_ATTRIBUTES` pointing at this descriptor; valid while `self` lives.
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

/// Replace the DACL of `path` with "current user only", not inherited from the parent.
pub fn set_owner_only(path: &Path) -> io::Result<()> {
    let sd = SecurityDescriptor::owner_only()?;
    let dacl = sd.dacl()?;
    let w = wide(path);
    // SAFETY: NUL-terminated path; the DACL is valid while `sd` lives.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(w.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
    };
    if rc.is_err() {
        return Err(io::Error::from_raw_os_error(rc.0 as i32));
    }
    Ok(())
}

/// Why a private file was refused.
fn refuse(path: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} {why}; refusing to use it. Fix with: icacls \"{}\" /inheritance:r /grant:r \"%USERNAME%:F\"",
            path.display(),
            path.display()
        ),
    )
}

/// Check that `path` is owned by the current user and that no other account (except SYSTEM and
/// Administrators) is granted any access. The Windows version of the Linux "group/other bits
/// must be clear and the uid must be ours" check.
pub fn check_private(path: &Path) -> io::Result<()> {
    let me = current_user_sid()?;
    let w = wide(path);
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: GetNamedSecurityInfoW allocates `sd` (freed below); `owner` and `dacl` point into it.
    unsafe {
        let rc = GetNamedSecurityInfoW(
            PCWSTR(w.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            &mut sd,
        );
        if rc.is_err() {
            return Err(io::Error::from_raw_os_error(rc.0 as i32));
        }
        let result = (|| {
            if sid_string(owner)? != me {
                return Err(refuse(path, "is not owned by the current user"));
            }
            if dacl.is_null() {
                return Err(refuse(path, "has no access list (everyone can open it)"));
            }
            for i in 0..u32::from((*dacl).AceCount) {
                let mut ace: *mut c_void = std::ptr::null_mut();
                GetAce(dacl, i, &mut ace).map_err(win_err)?;
                let header = &*ace.cast::<ACE_HEADER>();
                if u32::from(header.AceFlags) & INHERIT_ONLY_ACE.0 != 0 {
                    continue; // applies to children only, grants nothing on this object
                }
                if !ALLOW_ACE_TYPES.contains(&header.AceType) {
                    continue; // deny and audit entries take nothing away from privacy
                }
                if header.AceType != 0 {
                    return Err(refuse(path, "has an unusual access entry"));
                }
                let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                let sid = PSID(std::ptr::from_ref(&allowed.SidStart).cast_mut().cast());
                let s = sid_string(sid)?;
                if s != me && !TRUSTED_SIDS.contains(&s.as_str()) {
                    return Err(refuse(
                        path,
                        &format!("grants access to another account ({s})"),
                    ));
                }
            }
            Ok(())
        })();
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        result
    }
}

/// Create `dir` (and parents) and give it an owner-only DACL, even if it already existed.
pub fn private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    set_owner_only(dir)
}

/// Atomically write `data` to `path` (temp file with an owner-only DACL, flush, rename).
pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    private_dir(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("f"),
        std::process::id()
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        set_owner_only(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Per-user folders: (`%APPDATA%`, `%LOCALAPPDATA%`).
pub fn user_dirs() -> io::Result<(PathBuf, PathBuf)> {
    let get = |v: &str| {
        std::env::var_os(v)
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| io::Error::other(format!("{v} is not set")))
    };
    Ok((get("APPDATA")?, get("LOCALAPPDATA")?))
}

/// `%APPDATA%\dishus`: config.toml, devices.toml, secret.key.
pub fn config_dir() -> io::Result<PathBuf> {
    Ok(user_dirs()?.0.join("dishus"))
}

/// `%LOCALAPPDATA%\dishus`: lockdown flag, UDP port, logs.
pub fn state_dir() -> io::Result<PathBuf> {
    Ok(user_dirs()?.1.join("dishus"))
}

/// The control pipe. Pipe names are machine-wide, so the user's SID is part of it; the DACL
/// and the SID checks below are what actually keep other users out.
pub fn pipe_name() -> io::Result<String> {
    Ok(format!(r"\\.\pipe\dishus-{}", current_user_sid()?))
}

/// Create one instance of the control pipe: current user only, no remote clients. The first
/// instance is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, so if anyone else already owns the
/// name, this fails instead of joining their pipe.
pub fn create_pipe_server(
    name: &str,
    first: bool,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    let sd = SecurityDescriptor::pipe()?;
    let mut sa = sd.attributes();
    // SAFETY: `sa` and the descriptor it points to are valid for the duration of the call.
    unsafe {
        tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(name, std::ptr::from_mut(&mut sa).cast())
    }
}

fn raw(h: &impl AsRawHandle) -> HANDLE {
    HANDLE(h.as_raw_handle())
}

/// SID of the process that connected to our pipe server.
pub fn pipe_client_sid(pipe: &impl AsRawHandle) -> io::Result<String> {
    let mut pid = 0u32;
    // SAFETY: valid pipe handle borrowed from the caller.
    unsafe { GetNamedPipeClientProcessId(raw(pipe), &mut pid) }.map_err(win_err)?;
    process_user_sid(pid)
}

/// SID of the process serving the pipe we connected to.
pub fn pipe_server_sid(pipe: &impl AsRawHandle) -> io::Result<String> {
    let mut pid = 0u32;
    // SAFETY: valid pipe handle borrowed from the caller.
    unsafe { GetNamedPipeServerProcessId(raw(pipe), &mut pid) }.map_err(win_err)?;
    process_user_sid(pid)
}

/// End every process named `image` (e.g. `dishus-daemon.exe`) that runs as the current user,
/// except this one, and wait up to 5 s for each to exit. Returns how many were ended.
///
/// (`taskkill /FI "USERNAME eq …"` does the same but looks up every process owner by name,
/// which took over 30 s on a normal PC.)
pub fn kill_own(image: &str) -> io::Result<usize> {
    use windows::Win32::System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::{
            PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
        },
    };
    let me = current_user_sid()?;
    let own_pid = std::process::id();
    let mut pids = Vec::new();
    // SAFETY: a process snapshot we own and close; PROCESSENTRY32W is plain data with its size
    // set as the API requires.
    unsafe {
        let snap = Handle(CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).map_err(win_err)?);
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap.0, &mut e).is_ok();
        while ok {
            let n = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..n]);
            if name.eq_ignore_ascii_case(image) && e.th32ProcessID != own_pid {
                pids.push(e.th32ProcessID);
            }
            ok = Process32NextW(snap.0, &mut e).is_ok();
        }
    }
    let mut ended = 0;
    for pid in pids {
        if process_user_sid(pid).ok().as_deref() != Some(me.as_str()) {
            continue; // another account's process: not ours to end (and we couldn't anyway)
        }
        // SAFETY: an owned process handle, closed by `Handle`.
        unsafe {
            let Ok(h) = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, false, pid) else {
                continue;
            };
            let h = Handle(h);
            if TerminateProcess(h.0, 1).is_ok() {
                WaitForSingleObject(h.0, 5000);
                ended += 1;
            }
        }
    }
    Ok(ended)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_own_ends_only_matching_processes() {
        // A copy of a harmless system program under a unique name.
        let t = tempfile::tempdir().unwrap();
        let name = format!("dishus-killtest-{}.exe", std::process::id());
        let exe = t.path().join(&name);
        std::fs::copy(r"C:\Windows\System32\ping.exe", &exe).unwrap();
        let mut child = std::process::Command::new(&exe)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(kill_own("no-such-image.exe").unwrap(), 0);
        assert_eq!(kill_own(&name.to_uppercase()).unwrap(), 1);
        assert!(child.wait().is_ok());
    }

    #[test]
    fn sid_looks_like_a_user() {
        let s = current_user_sid().unwrap();
        assert!(s.starts_with("S-1-5-"), "{s}");
        assert_eq!(process_user_sid(std::process::id()).unwrap(), s);
        assert!(pipe_name().unwrap().ends_with(&s));
    }

    #[test]
    fn private_roundtrip_and_acl_check() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a/b/secret");
        write_private(&p, b"hi").unwrap();
        check_private(&p).unwrap();
        check_private(p.parent().unwrap()).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"hi");
        // Grant Everyone read: must be refused.
        let st = std::process::Command::new("icacls")
            .arg(&p)
            .args(["/grant", "*S-1-1-0:R"])
            .output()
            .unwrap();
        assert!(st.status.success());
        let e = check_private(&p).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(e.to_string().contains("S-1-1-0"), "{e}");
        // Rewriting restores owner-only.
        write_private(&p, b"again").unwrap();
        check_private(&p).unwrap();
    }

    #[tokio::test]
    async fn pipe_peers_are_this_user() {
        let name = format!(r"\\.\pipe\dishus-test-{}", std::process::id());
        let server = create_pipe_server(&name, true).unwrap();
        // A second "first instance" of the same name must fail (squatting protection).
        assert!(create_pipe_server(&name, true).is_err());
        let client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .unwrap();
        server.connect().await.unwrap();
        let me = current_user_sid().unwrap();
        assert_eq!(pipe_client_sid(&server).unwrap(), me);
        assert_eq!(pipe_server_sid(&client).unwrap(), me);
    }
}
