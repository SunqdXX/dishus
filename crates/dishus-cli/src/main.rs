//! `dishus` — control the DiShus daemon.

use std::{
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::{net::UnixStream, time::timeout};

use dishus_proto::{
    MAX_EXPIRY_DAYS, read_frame, write_frame,
    ipc::{IpcDevice, IpcRequest, IpcResponse, MAX_IPC_FRAME},
};

#[derive(Parser)]
#[command(name = "dishus", version, about = "Control the DiShus remote-control daemon")]
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
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u16).range(1..=MAX_EXPIRY_DAYS as i64))]
        days: u16,
    },
    /// PANIC: cut every session and refuse all connections until `dishus unlock`.
    Kill,
    /// Leave lockdown.
    Unlock,
    /// Grant screen-capture permission now (shows the portal dialog on this PC).
    SetupCapture,
}

fn runtime_socket() -> Result<PathBuf> {
    let rt = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR not set")?;
    Ok(PathBuf::from(rt).join("dishus/control.sock"))
}

fn devices_file() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME not set")?).join(".config"),
    };
    Ok(base.join("dishus/devices.toml"))
}

async fn call(req: IpcRequest, wait: Duration) -> Result<IpcResponse> {
    let path = runtime_socket()?;
    let mut s = UnixStream::connect(&path)
        .await
        .with_context(|| format!("daemon not running? ({}). Try: systemctl --user start dishus", path.display()))?;
    write_frame(&mut s, &req, MAX_IPC_FRAME).await?;
    let resp: IpcResponse = timeout(wait, read_frame(&mut s, MAX_IPC_FRAME)).await.context("daemon did not answer")??;
    if let IpcResponse::Error(e) = &resp {
        bail!("{e}");
    }
    Ok(resp)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn human_until(t: u64) -> String {
    let n = now();
    if t <= n {
        return "EXPIRED".into();
    }
    let d = t - n;
    if d >= 86_400 { format!("{}d", d / 86_400) } else if d >= 3600 { format!("{}h", d / 3600) } else { format!("{}m", d / 60) }
}

fn human_ago(t: u64) -> String {
    if t == 0 {
        return "never".into();
    }
    let d = now().saturating_sub(t);
    if d < 120 { "just now".into() } else if d < 7200 { format!("{}m ago", d / 60) } else if d < 172_800 { format!("{}h ago", d / 3600) } else { format!("{}d ago", d / 86_400) }
}

fn print_devices(devs: &[IpcDevice]) {
    if devs.is_empty() {
        println!("No paired phones. Run `dishus pair`.");
        return;
    }
    println!("{:<14} {:<24} {:<9} {:<12} {}", "ID", "NAME", "EXPIRES", "LAST SEEN", "");
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
            out.push(IpcDevice { id: s("id"), name: s("name"), paired_at: n("paired_at"), expires_at: n("expires_at"), last_seen: n("last_seen"), connected: false });
        }
    }
    Ok(out)
}

/// Offline revoke: rewrite devices.toml without the device (0600, atomic).
fn offline_revoke(sel: &str) -> Result<String> {
    let path = devices_file()?;
    let raw = std::fs::read_to_string(&path).context("no devices.toml")?;
    let mut v: toml::Table = raw.parse()?;
    let arr = v.get_mut("device").and_then(|d| d.as_array_mut()).context("no devices")?;
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
    let tmp = path.with_extension("toml.tmp");
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(toml::to_string_pretty(&v)?.as_bytes())?;
    f.sync_all()?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, &path)?;
    Ok(removed.get("name").and_then(|x| x.as_str()).unwrap_or("?").to_owned())
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
                println!("  relays      {}", if s.relays.is_empty() { "(none yet)".into() } else { s.relays.join(", ") });
                println!("  direct      {}", if s.direct_addrs.is_empty() { "-".into() } else { s.direct_addrs.join(", ") });
                println!("  capture     {}", s.capture_backend);
                println!("  input       {}", s.input_backend);
                println!("  devices     {} paired", s.devices);
                println!("  pairing     {}", if s.pairing_open { "OPEN" } else { "closed" });
                if s.lockdown {
                    println!("  LOCKDOWN    all connections refused (dishus unlock)");
                }
                if s.sessions.is_empty() {
                    println!("  sessions    none");
                }
                for x in s.sessions {
                    println!("  session     {} ({}) since {} · {}", x.name, &x.id[..12], human_ago(x.since), x.path);
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
        Cmd::Revoke { device } => match call(IpcRequest::Revoke { selector: device.clone() }, short).await {
            Ok(IpcResponse::Ok(m)) => println!("{m}"),
            Ok(r) => bail!("unexpected reply {r:?}"),
            Err(e) if format!("{e}").contains("daemon not running") => {
                let n = offline_revoke(&device)?;
                println!("revoked {n} (daemon was not running; edited devices.toml)");
            }
            Err(e) => return Err(e),
        },
        Cmd::Renew { device, days } => match call(IpcRequest::Renew { selector: device, days }, short).await? {
            IpcResponse::Ok(m) => println!("{m}"),
            r => bail!("unexpected reply {r:?}"),
        },
        Cmd::Kill => match call(IpcRequest::Kill, short).await {
            Ok(IpcResponse::Ok(m)) => println!("{m}"),
            Ok(r) => bail!("unexpected reply {r:?}"),
            Err(e) => {
                eprintln!("kill via daemon failed: {e:#}\nStopping the service instead.");
                let st = std::process::Command::new("systemctl").args(["--user", "stop", "dishus.service"]).status();
                bail!("daemon unreachable; `systemctl --user stop dishus` -> {st:?}");
            }
        },
        Cmd::Unlock => match call(IpcRequest::Unlock, short).await? {
            IpcResponse::Ok(m) => println!("{m}"),
            r => bail!("unexpected reply {r:?}"),
        },
        Cmd::SetupCapture => {
            println!("A screen-share dialog should appear on this PC. Select ALL monitors and allow it to be remembered.");
            match call(IpcRequest::SetupCapture, Duration::from_secs(180)).await? {
                IpcResponse::Ok(m) => println!("{m}"),
                r => bail!("unexpected reply {r:?}"),
            }
        }
        Cmd::Pair { skip_capture_setup } => {
            if !skip_capture_setup {
                println!("Step 1/2: screen-capture permission. A dialog should appear on this PC:");
                println!("          select ALL monitors and allow it to be remembered.");
                match call(IpcRequest::SetupCapture, Duration::from_secs(180)).await {
                    Ok(IpcResponse::Ok(m)) => println!("          ✓ {m}"),
                    Ok(_) => {}
                    Err(e) => println!("          ! capture setup failed: {e:#}\n            (pairing continues; video will be unavailable until fixed)"),
                }
            }
            let before: Vec<String> = match call(IpcRequest::List, short).await? {
                IpcResponse::Devices(d) => d.into_iter().map(|d| format!("{}:{}", d.id, d.paired_at)).collect(),
                _ => vec![],
            };
            let (uri, ttl) = match call(IpcRequest::Pair, short).await? {
                IpcResponse::Pairing { uri, ttl_secs } => (uri, ttl_secs),
                r => bail!("unexpected reply {r:?}"),
            };
            println!("\nStep 2/2: scan with the DiShus app (Pair new PC). Valid for {ttl} s, single use.\n");
            print_qr(&uri)?;
            println!("Or paste this link into the app (it is a secret, don't share it):\n{uri}\n");
            let deadline = std::time::Instant::now() + Duration::from_secs(ttl);
            loop {
                tokio::time::sleep(Duration::from_millis(700)).await;
                if let Ok(IpcResponse::Devices(d)) = call(IpcRequest::List, short).await
                    && let Some(n) = d.iter().find(|d| !before.contains(&format!("{}:{}", d.id, d.paired_at)))
                {
                    println!("✓ Paired \"{}\" — expires in {}", n.name, human_until(n.expires_at));
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
