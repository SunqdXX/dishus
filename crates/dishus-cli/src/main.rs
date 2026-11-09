//! `dishus` — control the DiShus daemon.

#[cfg(not(windows))]
use std::{
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
};
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
#[cfg(not(windows))]
use tokio::net::UnixStream;
use tokio::time::timeout;

use dishus_proto::{
    DEFAULT_EXPIRY_DAYS, MAX_EXPIRY_DAYS,
    ipc::{IpcDevice, IpcRequest, IpcResponse, MAX_IPC_FRAME},
    read_frame, write_frame,
};

#[derive(Parser)]
#[command(
    name = "dishus",
    version,
    about = "Control the DiShus remote-control daemon"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Daemon state, PC id, relays, active sessions.
    Status,
    /// Open the pairing window and show a QR code for the phone.
    Pair {
        /// Don't run the screen-capture permission dialog first.
        #[arg(long)]
        skip_capture_setup: bool,
    },
    /// List paired phones.
    List,
    /// Remove a phone (by name or id prefix) and cut its session.
    Revoke { device: String },
    /// Extend a phone's pairing.
    Renew {
        device: String,
        #[arg(long, default_value_t = DEFAULT_EXPIRY_DAYS, value_parser = clap::value_parser!(u16).range(1..=MAX_EXPIRY_DAYS as i64))]
        days: u16,
    },
    /// PANIC: cut every session and refuse all connections until `dishus unlock`.
    Kill,
    /// Leave lockdown.
    Unlock,
    /// Grant screen-capture permission now (shows the portal dialog on this PC).
    SetupCapture,
    /// Check that this PC can stream: test-encodes with every encoder and says what to install.
    Doctor,
}

/// `dishus doctor` runs `dishus-daemon --doctor`, preferring the daemon installed next to this
/// binary (the tarball and `cargo install` put them together), then PATH.
fn doctor() -> Result<()> {
    let name = format!("dishus-daemon{}", std::env::consts::EXE_SUFFIX);
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&name)))
        .filter(|p| p.is_file());
    let daemon = sibling.unwrap_or_else(|| PathBuf::from("dishus-daemon"));
    let status = std::process::Command::new(&daemon)
        .arg("--doctor")
        .status()
        .with_context(|| format!("running {} --doctor", daemon.display()))?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(not(windows))]
fn runtime_socket() -> Result<PathBuf> {
    let rt = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR not set")?;
    Ok(PathBuf::from(rt).join("dishus/control.sock"))
}

/// How to start the daemon, for error messages.
#[cfg(not(windows))]
const START_HINT: &str = "systemctl --user start dishus";
#[cfg(windows)]
const START_HINT: &str = "dishus-daemon (or sign out and in again)";

#[cfg(windows)]
fn devices_file() -> Result<PathBuf> {
    Ok(dishus_win::config_dir()?.join("devices.toml"))
}

#[cfg(windows)]
fn lockdown_file() -> Result<PathBuf> {
    Ok(dishus_win::state_dir()?.join("lockdown"))
}

/// Same file and protection the daemon uses; the daemon only checks that it exists.
#[cfg(windows)]
fn write_lockdown_offline() -> Result<PathBuf> {
    let f = lockdown_file()?;
    dishus_win::write_private(&f, b"locked by `dishus kill` (offline)\n")?;
    Ok(f)
}

#[cfg(not(windows))]
fn devices_file() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME not set")?).join(".config"),
    };
    Ok(base.join("dishus/devices.toml"))
}

/// Same path the daemon uses (`$XDG_STATE_HOME/dishus/lockdown`); the daemon only checks that
/// it exists, so the CLI can enforce lockdown even when the daemon is hung or stopped.
#[cfg(not(windows))]
fn lockdown_file() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => {
            PathBuf::from(std::env::var_os("HOME").context("HOME not set")?).join(".local/state")
        }
    };
    Ok(base.join("dishus/lockdown"))
}

#[cfg(not(windows))]
fn write_lockdown_offline() -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let f = lockdown_file()?;
    if let Some(dir) = f.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&f)?;
    std::io::Write::write_all(&mut file, b"locked by `dishus kill` (offline)\n")?;
    Ok(f)
}

#[cfg(not(windows))]
async fn connect() -> Result<UnixStream> {
    let path = runtime_socket()?;
    UnixStream::connect(&path).await.with_context(|| {
        format!(
            "daemon not running? ({}). Try: {START_HINT}",
            path.display()
        )
    })
}

/// The daemon's control pipe. Refuses a pipe served by another account (pipe names are
/// machine-wide, so someone could squat on ours before the daemon starts).
#[cfg(windows)]
async fn connect() -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    use tokio::net::windows::named_pipe::ClientOptions;
    const ERROR_PIPE_BUSY: i32 = 231;
    let name = dishus_win::pipe_name()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let pipe = loop {
        match ClientOptions::new().open(&name) {
            Ok(p) => break p,
            // All instances busy for a moment (another `dishus` call): wait for one.
            Err(e)
                if e.raw_os_error() == Some(ERROR_PIPE_BUSY)
                    && std::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("daemon not running? ({name}). Try: {START_HINT}"));
            }
        }
    };
    let me = dishus_win::current_user_sid()?;
    match dishus_win::pipe_server_sid(&pipe) {
        Ok(sid) if sid == me => Ok(pipe),
        Ok(sid) => bail!("{name} is served by another account ({sid}); refusing to use it"),
        Err(e) => bail!("cannot verify who serves {name}: {e}"),
    }
}

async fn call(req: IpcRequest, wait: Duration) -> Result<IpcResponse> {
    let mut s = connect().await?;
    write_frame(&mut s, &req, MAX_IPC_FRAME).await?;
    let resp: IpcResponse = timeout(wait, read_frame(&mut s, MAX_IPC_FRAME))
        .await
        .context("daemon did not answer")??;
    if let IpcResponse::Error(e) = &resp {
        bail!("{e}");
    }
    Ok(resp)
}

/// What `dishus pair` step 1 and `dishus setup-capture` say first.
#[cfg(not(windows))]
const SETUP_TEXT: [&str; 2] = [
    "Step 1/2: screen-capture permission. A dialog should appear on this PC:\n          select ALL monitors and allow it to be remembered.",
    "A screen-share dialog should appear on this PC. Select ALL monitors and allow it to be remembered.",
];
/// Windows asks no permission for screen capture; the step only checks that it works.
#[cfg(windows)]
const SETUP_TEXT: [&str; 2] = [
    "Step 1/2: checking screen capture (Windows shows no dialog for it).",
    "Checking screen capture (Windows shows no dialog for it).",
];

#[cfg(not(windows))]
const STOP_DESC: &str = "`systemctl --user stop dishus`";
#[cfg(not(windows))]
fn stop_daemon() -> std::io::Result<std::process::ExitStatus> {
    std::process::Command::new("systemctl")
        .args(["--user", "stop", "dishus.service"])
        .status()
}

/// End this user's daemon (and its capture worker). taskkill only ends processes the caller
/// may end, and the filter keeps it to this account's processes anyway.
#[cfg(windows)]
const STOP_DESC: &str = "`taskkill /F /IM dishus-daemon.exe` (this user's only)";
#[cfg(windows)]
fn stop_daemon() -> std::io::Result<std::process::ExitStatus> {
    let user = format!(
        "USERNAME eq {}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME").unwrap_or_default()
    );
    std::process::Command::new("taskkill")
        .args(["/F", "/T", "/FI", &user, "/IM", "dishus-daemon.exe"])
        .status()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn human_until(t: u64) -> String {
    let n = now();
    if t <= n {
        return "EXPIRED".into();
    }
    let d = t - n;
    if d >= 86_400 {
        // round to the nearest day so a fresh 30-day pairing shows "30d", not "29d"
        format!("{}d", (d + 43_200) / 86_400)
    } else if d >= 3600 {
        format!("{}h", d / 3600)
    } else {
        format!("{}m", d / 60)
    }
}

fn human_ago(t: u64) -> String {
    if t == 0 {
        return "never".into();
    }
    let d = now().saturating_sub(t);
    if d < 120 {
        "just now".into()
    } else if d < 7200 {
        format!("{}m ago", d / 60)
    } else if d < 172_800 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

fn print_devices(devs: &[IpcDevice]) {
    if devs.is_empty() {
        println!("No paired phones. Run `dishus pair`.");
        return;
    }
    println!("{:<14} {:<24} {:<9} LAST SEEN", "ID", "NAME", "EXPIRES");
    for d in devs {
        println!(
            "{:<14} {:<24} {:<9} {:<12} {}",
            &d.id[..d.id.len().min(12)],
            d.name,
            human_until(d.expires_at),
            human_ago(d.last_seen),
            if d.connected { "● connected" } else { "" }
        );
    }
}

/// Offline view of devices.toml (daemon not running).
fn offline_devices() -> Result<Vec<IpcDevice>> {
    let path = devices_file()?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let v: toml::Table = raw.parse()?;
    let mut out = Vec::new();
    if let Some(arr) = v.get("device").and_then(|d| d.as_array()) {
        for d in arr {
            let s = |k: &str| d.get(k).and_then(|x| x.as_str()).unwrap_or("").to_owned();
            let n = |k: &str| d.get(k).and_then(|x| x.as_integer()).unwrap_or(0).max(0) as u64;
            out.push(IpcDevice {
                id: s("id"),
                name: s("name"),
                paired_at: n("paired_at"),
                expires_at: n("expires_at"),
                last_seen: n("last_seen"),
                connected: false,
            });
        }
    }
    Ok(out)
}

/// Offline revoke: rewrite devices.toml without the device (0600, atomic).
fn offline_revoke(sel: &str) -> Result<String> {
    let path = devices_file()?;
    let raw = std::fs::read_to_string(&path).context("no devices.toml")?;
    let mut v: toml::Table = raw.parse()?;
    let arr = v
        .get_mut("device")
        .and_then(|d| d.as_array_mut())
        .context("no devices")?;
    let matches: Vec<usize> = arr
        .iter()
        .enumerate()
        .filter(|(_, d)| {
            let name = d.get("name").and_then(|x| x.as_str()).unwrap_or("");
            let id = d.get("id").and_then(|x| x.as_str()).unwrap_or("");
            name == sel || (sel.len() >= 6 && id.starts_with(&sel.to_ascii_lowercase()))
        })
        .map(|(i, _)| i)
        .collect();
    let idx = match matches.as_slice() {
        [i] => *i,
        [] => bail!("no device matches '{sel}'"),
        _ => bail!("'{sel}' is ambiguous"),
    };
    let removed = arr.remove(idx);
    #[cfg(windows)]
    dishus_win::write_private(&path, toml::to_string_pretty(&v)?.as_bytes())?;
    #[cfg(not(windows))]
    {
        let tmp = path.with_extension("toml.tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(toml::to_string_pretty(&v)?.as_bytes())?;
        f.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, &path)?;
    }
    Ok(removed
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("?")
        .to_owned())
}

fn print_qr(uri: &str) -> Result<()> {
    let code = qrcode::QrCode::new(uri.as_bytes())?;
    let s = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build();
    println!("{s}");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let short = Duration::from_secs(10);
    match cli.cmd {
        Cmd::Status => match call(IpcRequest::Status, short).await? {
            IpcResponse::Status(s) => {
                println!("DiShus {} on \"{}\"", s.version, s.pc_name);
                println!("  PC id       {}", s.endpoint_id);
                println!(
                    "  relays      {}",
                    if s.relays.is_empty() {
                        "(none yet)".into()
                    } else {
                        s.relays.join(", ")
                    }
                );
                println!(
                    "  direct      {}",
                    if s.direct_addrs.is_empty() {
                        "-".into()
                    } else {
                        s.direct_addrs.join(", ")
                    }
                );
                println!("  capture     {}", s.capture_backend);
                println!("  input       {}", s.input_backend);
                println!("  devices     {} paired", s.devices);
                println!(
                    "  pairing     {}",
                    if s.pairing_open { "OPEN" } else { "closed" }
                );
                if s.lockdown {
                    println!("  LOCKDOWN    all connections refused (dishus unlock)");
                }
                if s.sessions.is_empty() {
                    println!("  sessions    none");
                }
                for x in s.sessions {
                    println!(
                        "  session     {} ({}) since {} · {}",
                        x.name,
                        &x.id[..12],
                        human_ago(x.since),
                        x.path
                    );
                }
            }
            r => bail!("unexpected reply {r:?}"),
        },
        Cmd::List => match call(IpcRequest::List, short).await {
            Ok(IpcResponse::Devices(d)) => print_devices(&d),
            Ok(r) => bail!("unexpected reply {r:?}"),
            Err(e) if format!("{e}").contains("daemon not running") => {
                eprintln!("(daemon not running; reading devices.toml directly)");
                print_devices(&offline_devices()?);
            }
            Err(e) => return Err(e),
        },
        Cmd::Revoke { device } => match call(
            IpcRequest::Revoke {
                selector: device.clone(),
            },
            short,
        )
        .await
        {
            Ok(IpcResponse::Ok(m)) => println!("{m}"),
            Ok(r) => bail!("unexpected reply {r:?}"),
            Err(e) if format!("{e}").contains("daemon not running") => {
                let n = offline_revoke(&device)?;
                println!("revoked {n} (daemon was not running; edited devices.toml)");
            }
            Err(e) => return Err(e),
        },
        Cmd::Renew { device, days } => match call(
            IpcRequest::Renew {
                selector: device,
                days,
            },
            short,
        )
        .await?
        {
            IpcResponse::Ok(m) => println!("{m}"),
            r => bail!("unexpected reply {r:?}"),
        },
        // Panic button: don't wait long on a hung daemon before falling back.
        Cmd::Kill => match call(IpcRequest::Kill, Duration::from_secs(3)).await {
            Ok(IpcResponse::Ok(m)) => println!("{m}"),
            Ok(r) => bail!("unexpected reply {r:?}"),
            Err(e) => {
                eprintln!("kill via daemon failed: {e:#}\nStopping the service instead.");
                // Persist lockdown first so a restart (or reboot) of the daemon stays locked.
                let lock = write_lockdown_offline();
                let st = stop_daemon();
                match &lock {
                    Ok(p) => eprintln!("lockdown flag written: {}", p.display()),
                    Err(e) => eprintln!("could not write lockdown flag: {e:#}"),
                }
                bail!("daemon unreachable; {STOP_DESC} -> {st:?}");
            }
        },
        Cmd::Unlock => match call(IpcRequest::Unlock, short).await {
            Ok(IpcResponse::Ok(m)) => println!("{m}"),
            Ok(r) => bail!("unexpected reply {r:?}"),
            Err(e) if format!("{e}").contains("daemon not running") => {
                match std::fs::remove_file(lockdown_file()?) {
                    Ok(()) => println!("lockdown lifted (daemon was not running)"),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        println!("not in lockdown")
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(e) => return Err(e),
        },
        Cmd::Doctor => doctor()?,
        Cmd::SetupCapture => {
            println!("{}", SETUP_TEXT[1]);
            match call(IpcRequest::SetupCapture, Duration::from_secs(180)).await? {
                IpcResponse::Ok(m) => println!("{m}"),
                r => bail!("unexpected reply {r:?}"),
            }
        }
        Cmd::Pair { skip_capture_setup } => {
            if !skip_capture_setup {
                println!("{}", SETUP_TEXT[0]);
                match call(IpcRequest::SetupCapture, Duration::from_secs(180)).await {
                    Ok(IpcResponse::Ok(m)) => println!("          ✓ {m}"),
                    Ok(_) => {}
                    Err(e) => println!(
                        "          ! capture setup failed: {e:#}\n            (pairing continues; video will be unavailable until fixed)"
                    ),
                }
            }
            let before: Vec<String> = match call(IpcRequest::List, short).await? {
                IpcResponse::Devices(d) => d
                    .into_iter()
                    .map(|d| format!("{}:{}", d.id, d.paired_at))
                    .collect(),
                _ => vec![],
            };
            let (uri, ttl) = match call(IpcRequest::Pair, short).await? {
                IpcResponse::Pairing { uri, ttl_secs } => (uri, ttl_secs),
                r => bail!("unexpected reply {r:?}"),
            };
            println!(
                "\nStep 2/2: scan with the DiShus app (Pair new PC). Valid for {ttl} s, single use.\n"
            );
            print_qr(&uri)?;
            println!("Or paste this link into the app (it is a secret, don't share it):\n{uri}\n");
            let deadline = std::time::Instant::now() + Duration::from_secs(ttl);
            loop {
                tokio::time::sleep(Duration::from_millis(700)).await;
                if let Ok(IpcResponse::Devices(d)) = call(IpcRequest::List, short).await
                    && let Some(n) = d
                        .iter()
                        .find(|d| !before.contains(&format!("{}:{}", d.id, d.paired_at)))
                {
                    println!(
                        "✓ Paired \"{}\" — expires in {}",
                        n.name,
                        human_until(n.expires_at)
                    );
                    break;
                }
                if std::time::Instant::now() > deadline {
                    let _ = call(IpcRequest::ClosePairing, short).await;
                    bail!("pairing window expired; run `dishus pair` again");
                }
            }
        }
    }
    Ok(())
}
