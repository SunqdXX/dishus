//! Desktop test client — a stand-in for the phone, for development and the LAN prototype.
//!
//! ```text
//! cargo run -p dishus-core --example desktop_client -- pair 'dishus://pair?...'
//! cargo run -p dishus-core --example desktop_client -- view --seconds 20 > out.h264
//! cargo run -p dishus-core --example desktop_client -- view | ffplay -fflags nobuffer -flags low_delay -framedrop -f h264 -
//! cargo run -p dishus-core --example desktop_client -- keys 125,2   # tap Super+1
//! ```
//!
//! State (identity + paired PC) is kept in `$DISHUS_CLIENT_DIR` (default
//! `$XDG_STATE_HOME/dishus-test-client`) encrypted with `$DISHUS_CLIENT_PASSWORD`.

use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Windows: files under %LOCALAPPDATA% are private to the user already; `mode` is a no-op.
#[cfg(not(unix))]
trait OpenOptionsExt {
    fn mode(&mut self, _: u32) -> &mut Self;
}
#[cfg(not(unix))]
impl OpenOptionsExt for std::fs::OpenOptions {
    fn mode(&mut self, _: u32) -> &mut Self {
        self
    }
}

use dishus_core::{
    Client, NetConfig,
    proto::{ClientMsg, PairingInfo, Quality, ServerMsg, StreamParams},
    vault::{self, KdfParams},
};

fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DISHUS_CLIENT_DIR") {
        return d.into();
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join(".local/state"));
    base.join("dishus-test-client")
}

fn password() -> String {
    std::env::var("DISHUS_CLIENT_PASSWORD").unwrap_or_else(|_| "insecure-test-password".into())
}

fn load() -> Result<vault::VaultData> {
    let p = dir().join("vault.bin");
    match std::fs::read(&p) {
        Ok(b) => Ok(vault::open(&password(), &b)?),
        Err(_) => Ok(vault::new_identity()?),
    }
}

fn save(v: &vault::VaultData) -> Result<()> {
    let d = dir();
    std::fs::create_dir_all(&d)?;
    let blob = vault::seal(&password(), v, KdfParams::default())?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(d.join("vault.bin"))?;
    f.write_all(&blob)?;
    Ok(())
}

fn arg_val(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("DISHUS_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let mut v = load()?;
    let net = NetConfig {
        relays: arg_val(&args, "--relay").into_iter().collect(),
        disable_relays: args.iter().any(|a| a == "--no-relay"),
        local_test: false,
    };
    let client = Client::new(&v.secret_key, &net).await?;
    eprintln!("client id {}", client.endpoint_id());

    match cmd {
        "pair" => {
            let uri = args.get(1).context("usage: pair <dishus://pair?...>")?;
            let info = PairingInfo::from_uri(uri)?;
            let pc = client.pair(&info, "desktop-test-client").await?;
            eprintln!("paired with {} (expires unix {})", pc.name, pc.expires_at);
            v.pcs.retain(|p| p.endpoint_id != pc.endpoint_id);
            v.pcs.push(pc);
            save(&v)?;
        }
        "view" | "keys" | "renew" => {
            let pc = v
                .pcs
                .last()
                .cloned()
                .context("not paired; run `pair` first")?;
            let params = StreamParams {
                monitor: arg_val(&args, "--monitor")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                fps: arg_val(&args, "--fps")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                quality: Quality::Auto,
                max_height: arg_val(&args, "--max-height")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
            };
            let t0 = Instant::now();
            let s = client.connect(&pc, "desktop-test-client", params).await?;
            eprintln!("connected in {} ms", t0.elapsed().as_millis());
            let seconds: u64 = arg_val(&args, "--seconds")
                .and_then(|s| s.parse().ok())
                .unwrap_or(3600);
            if cmd == "keys" {
                // comma separated keycodes, pressed in order then released in reverse ("chord")
                let codes: Vec<u16> = args
                    .get(1)
                    .context("usage: keys 125,2")?
                    .split(',')
                    .map(|c| c.parse())
                    .collect::<Result<_, _>>()?;
                for c in &codes {
                    s.send(ClientMsg::Key {
                        code: *c,
                        down: true,
                    })?;
                }
                tokio::time::sleep(Duration::from_millis(60)).await;
                for c in codes.iter().rev() {
                    s.send(ClientMsg::Key {
                        code: *c,
                        down: false,
                    })?;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
                s.close().await;
                return Ok(());
            }
            if cmd == "renew" {
                s.send(ClientMsg::RenewPairing {
                    days: arg_val(&args, "--days")
                        .and_then(|d| d.parse().ok())
                        .unwrap_or(30),
                })?;
            }
            let mut out = std::io::stdout().lock();
            let deadline = Instant::now() + Duration::from_secs(seconds);
            let (mut frames, mut bytes, mut keyframes) = (0u64, 0u64, 0u64);
            let mut last_report = Instant::now();
            let mut first_frame: Option<Duration> = None;
            loop {
                tokio::select! {
                    c = s.next_control() => match c {
                        Some(ServerMsg::Bye { reason }) => { eprintln!("bye: {reason:?}"); break; }
                        Some(m) => eprintln!("<- {m:?}"),
                        None => { eprintln!("control closed"); break; }
                    },
                    vpk = s.next_video() => {
                        let Some(p) = vpk else { break };
                        if first_frame.is_none() {
                            first_frame = Some(t0.elapsed());
                            eprintln!("first frame after {} ms (keyframe={})", t0.elapsed().as_millis(), p.header.is_keyframe());
                        }
                        frames += 1;
                        bytes += p.data.len() as u64;
                        keyframes += u64::from(p.header.is_keyframe());
                        if out.write_all(&p.data).is_err() { break; }
                    }
                    _ = tokio::time::sleep_until(deadline.into()) => break,
                }
                if last_report.elapsed() >= Duration::from_secs(2) {
                    let secs = last_report.elapsed().as_secs_f64();
                    eprintln!(
                        "{:.1} fps  {:.0} kbit/s  keyframes {}  rtt {} ms  path {}",
                        frames as f64 / secs,
                        bytes as f64 * 8.0 / 1000.0 / secs,
                        keyframes,
                        s.rtt_ms(),
                        s.path()
                    );
                    frames = 0;
                    bytes = 0;
                    keyframes = 0;
                    last_report = Instant::now();
                }
            }
            s.close().await;
        }
        _ => bail!(
            "usage: desktop_client pair <uri> | view [--seconds N] [--fps N] [--max-height N] [--monitor N] | keys 125,2 | renew [--days N]"
        ),
    }
    client.close().await;
    Ok(())
}
