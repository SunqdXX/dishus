//! XDG locations and private-file helpers. Every file we write is 0600, every dir 0700.
//!
//! On Windows: `%APPDATA%\dishus` (config), `%LOCALAPPDATA%\dishus` (state, logs) and
//! `%LOCALAPPDATA%\dishus\run` (instance lock); files and dirs get an owner-only DACL instead
//! of modes (see `dishus_win`).

use std::{
    fs,
    path::{Path, PathBuf},
};
#[cfg(not(windows))]
use std::{
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Paths {
    /// `$XDG_CONFIG_HOME/dishus`: config.toml, devices.toml, secret.key
    pub config_dir: PathBuf,
    /// `$XDG_STATE_HOME/dishus`: portal restore token, lockdown flag
    pub state_dir: PathBuf,
    /// `$XDG_RUNTIME_DIR/dishus`: control socket
    pub runtime_dir: PathBuf,
}

#[cfg(not(windows))]
fn env_dir(var: &str, fallback: impl FnOnce(PathBuf) -> PathBuf) -> Result<PathBuf> {
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        let p = PathBuf::from(v);
        if p.is_absolute() {
            return Ok(p);
        }
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(fallback(PathBuf::from(home)))
}

impl Paths {
    #[cfg(windows)]
    pub fn from_env() -> Result<Self> {
        let state_dir = dishus_win::state_dir()?;
        Ok(Self {
            config_dir: dishus_win::config_dir()?,
            runtime_dir: state_dir.join("run"),
            state_dir,
        })
    }

    #[cfg(not(windows))]
    pub fn from_env() -> Result<Self> {
        let config = env_dir("XDG_CONFIG_HOME", |h| h.join(".config"))?;
        let state = env_dir("XDG_STATE_HOME", |h| h.join(".local/state"))?;
        let runtime = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            Some(v) => PathBuf::from(v),
            None => bail!("XDG_RUNTIME_DIR is not set (is this a login session?)"),
        };
        Ok(Self::with_roots(&config, &state, &runtime))
    }

    pub fn with_roots(config: &Path, state: &Path, runtime: &Path) -> Self {
        Self {
            config_dir: config.join("dishus"),
            state_dir: state.join("dishus"),
            runtime_dir: runtime.join("dishus"),
        }
    }

    /// Same root for everything; used by tests.
    pub fn under(root: &Path) -> Self {
        Self::with_roots(&root.join("config"), &root.join("state"), &root.join("run"))
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [&self.config_dir, &self.state_dir, &self.runtime_dir] {
            private_dir(d)?;
        }
        Ok(())
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn devices_file(&self) -> PathBuf {
        self.config_dir.join("devices.toml")
    }
    pub fn secret_key_file(&self) -> PathBuf {
        self.config_dir.join("secret.key")
    }
    pub fn restore_token_file(&self) -> PathBuf {
        self.state_dir.join("portal-restore-token")
    }
    /// The UDP port bound last time (reused so phones' stored addresses stay valid).
    pub fn udp_port_file(&self) -> PathBuf {
        self.state_dir.join("udp-port")
    }

    /// Held (flock) for the daemon's whole lifetime: one instance per user session.
    pub fn instance_lock_file(&self) -> PathBuf {
        self.runtime_dir.join("daemon.lock")
    }

    /// Take the single-instance lock, waiting up to `wait` for a previous instance that is
    /// still shutting down. The lock is released by the kernel when the process exits, so a
    /// crash can't leave it stuck. The returned file must be kept alive.
    pub fn lock_instance(&self, wait: std::time::Duration) -> Result<fs::File> {
        let path = self.instance_lock_file();
        let mut opts = fs::OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(not(windows))]
        opts.mode(0o600);
        let f = opts
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let end = std::time::Instant::now() + wait;
        loop {
            match f.try_lock() {
                Ok(()) => return Ok(f),
                Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < end => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(fs::TryLockError::WouldBlock) => {
                    bail!(
                        "another dishus-daemon is already running (lock {})",
                        path.display()
                    )
                }
                Err(fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("locking {}", path.display()));
                }
            }
        }
    }

    pub fn lockdown_file(&self) -> PathBuf {
        self.state_dir.join("lockdown")
    }
    #[cfg(not(windows))]
    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("control.sock")
    }
    /// The control pipe, `\\.\pipe\dishus-<user SID>`.
    #[cfg(windows)]
    pub fn socket(&self) -> PathBuf {
        PathBuf::from(dishus_win::pipe_name().unwrap_or_else(|_| r"\\.\pipe\dishus".into()))
    }
    /// Daemon log files (Windows has no journal).
    pub fn log_dir(&self) -> PathBuf {
        self.state_dir.join("logs")
    }
}

/// Create `dir` (and parents) with an owner-only DACL, and tighten it if it already exists.
#[cfg(windows)]
pub fn private_dir(dir: &Path) -> Result<()> {
    dishus_win::private_dir(dir).with_context(|| format!("creating {}", dir.display()))
}

/// Atomically write `data` to `path`, readable by the current user only.
#[cfg(windows)]
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    dishus_win::write_private(path, data).with_context(|| format!("writing {}", path.display()))
}

/// Read a private file, refusing it if another account can access it or someone else owns it.
/// Returns `Ok(None)` if it doesn't exist.
#[cfg(windows)]
pub fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    }
    dishus_win::check_private(path)?;
    Ok(Some(
        fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    ))
}

/// Create `dir` (and parents) with mode 0700, and tighten it if it already exists.
#[cfg(not(windows))]
pub fn private_dir(dir: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", dir.display()))?;
    Ok(())
}

/// Atomically write `data` to `path` with mode 0600 (write temp file, fsync, rename).
#[cfg(not(windows))]
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    private_dir(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("f"),
        std::process::id()
    ));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("opening {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

/// Read a private file, refusing it if group/other can access it (someone loosened it, or it
/// was planted). Returns `Ok(None)` if it doesn't exist.
#[cfg(not(windows))]
pub fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} has mode {:o}; refusing to use it. Fix with: chmod 600 {}",
            path.display(),
            mode,
            path.display()
        );
    }
    let uid = rustix::process::getuid().as_raw();
    if std::os::unix::fs::MetadataExt::uid(&meta) != uid {
        bail!("{} is not owned by the current user", path.display());
    }
    Ok(Some(
        fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_instance_lock() {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::under(t.path());
        p.ensure().unwrap();
        let short = std::time::Duration::from_millis(200);
        let held = p.lock_instance(short).unwrap();
        let err = p.lock_instance(short).unwrap_err().to_string();
        assert!(err.contains("already running"), "{err}");
        // The previous instance exits while we wait: we get the lock.
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(held);
        });
        p.lock_instance(std::time::Duration::from_secs(3)).unwrap();
        releaser.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn private_roundtrip_and_acl_check() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a/b/secret");
        write_private(&p, b"hi").unwrap();
        assert_eq!(read_private(&p).unwrap().unwrap(), b"hi");
        let st = std::process::Command::new("icacls")
            .arg(&p)
            .args(["/grant", "*S-1-5-32-545:R"]) // BUILTIN\Users
            .output()
            .unwrap();
        assert!(st.status.success());
        assert!(read_private(&p).is_err());
        assert!(read_private(&t.path().join("nope")).unwrap().is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn private_roundtrip_and_mode_check() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a/b/secret");
        write_private(&p, b"hi").unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(p.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(read_private(&p).unwrap().unwrap(), b"hi");
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private(&p).is_err());
        assert!(read_private(&t.path().join("nope")).unwrap().is_none());
    }
}
