//! Soak/scenario client for end-to-end tests against a real daemon (see docs/TESTING.md).
//! Each command prints one `RESULT ...` line on stdout and exits non-zero on failure.
//!
//! ```text
//! soak pair <uri>                 pair (state in $DISHUS_CLIENT_DIR, password $DISHUS_CLIENT_PASSWORD)
//! soak script                     deterministic input script, then Bye
//! soak hold <code> <bye|drop>     press and hold a key, then send Bye or vanish without closing
//! soak badkey <code>              send a refused keycode; expect the session to end
//! soak wait                       connect and wait for the server's Bye; print its reason
//! soak try                        connect once: RESULT connected | RESULT refused
//! soak renew <days>               renew from inside a session
//! soak video <seconds>            receive video; print frame/keyframe/gap counts
//! ```
use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use dishus_core::{
    Client, NetConfig, Session,
    proto::{ClientMsg, MouseButton, PairingInfo, Quality, ServerMsg, StreamParams},
    vault::{self, KdfParams},
};

fn dir() -> PathBuf {
    std::env::var_os("DISHUS_CLIENT_DIR")
        .map(PathBuf::from)
        .expect("DISHUS_CLIENT_DIR")
}
fn password() -> String {
    std::env::var("DISHUS_CLIENT_PASSWORD").unwrap_or_else(|_| "soak-test-password".into())
}
const KDF: KdfParams = KdfParams {
    m_kib: 19 * 1024,
    t: 2,
    p: 1,
};
fn load() -> Result<vault::VaultData> {
    match std::fs::read(dir().join("vault.bin")) {
        Ok(b) => Ok(vault::open(&password(), &b)?),
        Err(_) => Ok(vault::new_identity()?),
    }
}
fn save(v: &vault::VaultData) -> Result<()> {
    std::fs::create_dir_all(dir())?;
    let blob = vault::seal(&password(), v, KDF)?;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir().join("vault.bin"))?
        .write_all(&blob)?;
    Ok(())
}

fn params() -> StreamParams {
    StreamParams {
        monitor: 0,
        fps: 0,
        quality: Quality::Auto,
        max_height: 0,
    }
}

/// Wait for the next non-Pong control message, with a timeout.
async fn next(s: &Session, secs: u64) -> Option<ServerMsg> {
    tokio::time::timeout(Duration::from_secs(secs), s.next_control())
        .await
        .ok()
        .flatten()
}

async fn wait_welcome(s: &Session) -> Result<()> {
    loop {
        match next(s, 20).await {
            Some(ServerMsg::Welcome { .. }) => return Ok(()),
            Some(_) => continue,
            None => bail!("no Welcome"),
        }
    }
}

/// Drain until Bye or close; returns the reason text.
async fn wait_bye(s: &Session, secs: u64) -> String {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        match next(s, 1).await {
            Some(ServerMsg::Bye { reason }) => return format!("{reason:?}"),
            Some(_) => {}
            None if s.is_closed() => return "closed-without-bye".into(),
            None => {}
        }
    }
    "timeout".into()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let mut v = load()?;
    let client = Client::new(&v.secret_key, &NetConfig::default()).await?;

    if cmd == "pair" {
        let info = PairingInfo::from_uri(args.get(1).context("uri")?)?;
        let pc = client.pair(&info, "soak").await?;
        v.pcs.retain(|p| p.endpoint_id != pc.endpoint_id);
        v.pcs.push(pc);
        save(&v)?;
        println!("RESULT paired");
        client.close().await;
        return Ok(());
    }
    let pc = v.pcs.last().cloned().context("not paired")?;
    let t0 = Instant::now();
    let s = match client.connect(&pc, "soak", params()).await {
        Ok(s) => s,
        Err(e) => {
            println!("RESULT refused ({e:#})");
            client.close().await;
            if cmd == "try" {
                return Ok(());
            }
            std::process::exit(3);
        }
    };
    if wait_welcome(&s).await.is_err() {
        println!("RESULT refused (no welcome)");
        if cmd == "try" {
            return Ok(());
        }
        std::process::exit(3);
    }
    let connect_ms = t0.elapsed().as_millis();
    let send = |m: ClientMsg| s.send(m).expect("send");

    match cmd {
        "try" => println!("RESULT connected {connect_ms}ms path={}", s.path()),
        "script" => {
            // Shift+a, b, Ctrl+c; moves; scroll; left click; right click.
            for (c, d) in [
                (42, true),
                (30, true),
                (30, false),
                (42, false),
                (48, true),
                (48, false),
                (29, true),
                (46, true),
                (46, false),
                (29, false),
            ] {
                send(ClientMsg::Key { code: c, down: d });
            }
            send(ClientMsg::MouseMove { dx: 10, dy: -5 });
            send(ClientMsg::MouseMove { dx: -3, dy: 7 });
            send(ClientMsg::Scroll { dx: 0, dy: 240 });
            send(ClientMsg::MouseButton {
                button: MouseButton::Left,
                down: true,
            });
            send(ClientMsg::MouseButton {
                button: MouseButton::Left,
                down: false,
            });
            send(ClientMsg::MouseButton {
                button: MouseButton::Right,
                down: true,
            });
            send(ClientMsg::MouseButton {
                button: MouseButton::Right,
                down: false,
            });
            tokio::time::sleep(Duration::from_millis(300)).await;
            send(ClientMsg::Bye);
            tokio::time::sleep(Duration::from_millis(300)).await;
            println!("RESULT script-sent {connect_ms}ms");
        }
        "hold" => {
            let code: u16 = args.get(1).context("code")?.parse()?;
            send(ClientMsg::Key { code, down: true });
            tokio::time::sleep(Duration::from_millis(500)).await;
            match args.get(2).map(String::as_str) {
                Some("bye") => {
                    send(ClientMsg::Bye);
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    println!("RESULT held-then-bye");
                }
                Some("drop") => {
                    // Vanish like a phone losing power: no Bye, no QUIC close.
                    println!("RESULT held-then-vanished");
                    std::io::stdout().flush()?;
                    std::process::exit(0);
                }
                _ => bail!("hold <code> <bye|drop>"),
            }
        }
        "badkey" => {
            let code: u16 = args.get(1).context("code")?.parse()?;
            // Bypass local validation: the daemon must refuse it on its own.
            let r = s.send_unchecked(ClientMsg::Key { code, down: true });
            let reason = wait_bye(&s, 8).await;
            println!("RESULT badkey send={} bye={reason}", r.is_ok());
        }
        "wait" => {
            let secs: u64 = args.get(1).and_then(|x| x.parse().ok()).unwrap_or(30);
            println!("RESULT bye={}", wait_bye(&s, secs).await);
        }
        "renew" => {
            let days: u16 = args.get(1).context("days")?.parse()?;
            send(ClientMsg::RenewPairing { days });
            let mut got = None;
            for _ in 0..10 {
                if let Some(ServerMsg::Renewed { expires_at }) = next(&s, 2).await {
                    got = Some(expires_at);
                    break;
                }
            }
            match got {
                Some(e) => println!("RESULT renewed {e}"),
                None => println!("RESULT renew-failed"),
            }
        }
        "video" => {
            let secs: u64 = args.get(1).and_then(|x| x.parse().ok()).unwrap_or(5);
            let end = Instant::now() + Duration::from_secs(secs);
            let (mut frames, mut keys, mut gaps, mut last) = (0u64, 0u64, 0u64, None::<u64>);
            let mut first_ms = None;
            while Instant::now() < end {
                match tokio::time::timeout(Duration::from_millis(500), s.next_video()).await {
                    Ok(Some(p)) => {
                        first_ms.get_or_insert(t0.elapsed().as_millis());
                        frames += 1;
                        keys += p.header.is_keyframe() as u64;
                        if let Some(l) = last
                            && p.header.seq != l + 1
                        {
                            gaps += 1;
                        }
                        last = Some(p.header.seq);
                    }
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
            println!(
                "RESULT video frames={frames} keyframes={keys} gaps={gaps} first_frame_ms={} fps={:.1}",
                first_ms.unwrap_or(0),
                frames as f64 / secs as f64
            );
        }
        _ => bail!("unknown command"),
    }
    s.close().await;
    client.close().await;
    Ok(())
}
