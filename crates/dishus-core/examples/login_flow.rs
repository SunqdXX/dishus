//! The phone app's login-screen path, through the same FFI calls the app makes, for VM tests.
//!
//! ```text
//! cargo build -p dishus-core --features ffi --example login_flow
//! login_flow <file with the dishus://login code> [--vault FILE] [--no-relay] [--type 20,18,31,20,28] [--seconds 30]
//! ```
//!
//! Redeems the code (`dishus boot allow` must be waiting), checks the app-lock call and that
//! the entry survives export/lock/unlock, then connects with the entry's own identity, waits for
//! the picture, optionally types evdev codes, and prints how the session ended. Never prints
//! the code.

use std::time::{Duration, Instant};

use dishus_core::ffi::{
    ControlEvent, Core, DishusError, RelaySettings, StreamQuality, StreamSettings,
};

fn arg_val(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = std::fs::read_to_string(args.first().expect("usage: login_flow <code file>"))
        .expect("read code file")
        .trim()
        .to_string();
    let codes: Vec<u16> = arg_val(&args, "--type")
        .map(|s| {
            s.split(',')
                .map(|c| c.parse().expect("evdev code"))
                .collect()
        })
        .unwrap_or_default();
    let seconds: u64 = arg_val(&args, "--seconds").map_or(30, |s| s.parse().expect("seconds"));
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let code_ok = rt.block_on(run(&args, code, codes, seconds));
    // Core owns a runtime; exiting skips dropping it inside ours.
    std::process::exit(if code_ok { 0 } else { 1 });
}

async fn run(args: &[String], code: String, codes: Vec<u16>, seconds: u64) -> bool {
    assert!(
        dishus_core::ffi::is_login_code(code.clone()),
        "not a login code"
    );
    // Core owns a runtime that must not be dropped inside this one: keep it for the process.
    let core: &'static _ = Box::leak(Box::new(Core::new().unwrap()));
    core.set_relays(RelaySettings {
        relays: vec![],
        disable_relays: args.iter().any(|a| a == "--no-relay"),
    })
    .unwrap();
    // --vault FILE keeps the identity between runs (the limits are per phone).
    let vault_file = arg_val(args, "--vault").map(std::path::PathBuf::from);
    let stored = vault_file.as_ref().and_then(|f| std::fs::read(f).ok());
    match stored {
        Some(blob) => core.unlock("app-password-1".into(), blob).unwrap(),
        None => drop(core.create_vault("app-password-1".into()).unwrap()),
    }
    let pw_ok = core.verify_password("app-password-1".into()).unwrap()
        && !core.verify_password("app-password-2".into()).unwrap()
        && !core.verify_password("app-password-".into()).unwrap();
    println!("app lock check: {}", if pw_ok { "ok" } else { "WRONG" });

    let entry = if let Some(e) = core.login_screens().unwrap().into_iter().next() {
        println!("using the stored login screen");
        e
    } else {
        match core.add_login_screen(code, "vm-phone login".into()).await {
            Ok(e) => e,
            Err(e) => {
                println!("add_login_screen failed: {e}");
                return false;
            }
        }
    };
    if let Some(f) = &vault_file {
        std::fs::write(f, core.export_vault().unwrap()).unwrap();
    }
    println!(
        "added login screen of {:?} ({}…)",
        entry.name,
        &entry.id[..12]
    );
    let blob = core.export_vault().unwrap();
    core.lock();
    core.unlock("app-password-1".into(), blob).unwrap();
    let kept = core.login_screens().unwrap();
    println!(
        "after lock/unlock: {} login screen(s), {} paired PC(s)",
        kept.len(),
        core.paired_pcs().unwrap().len()
    );

    // `dishus boot allow` asks sudo after it sees the enrollment; an unknown phone is dropped
    // without a word, so give it time first (in the app, the app lock takes longer than this).
    tokio::time::sleep(Duration::from_secs(6)).await;
    let settings = StreamSettings {
        monitor: 0,
        fps: 0,
        quality: StreamQuality::Auto,
        max_height: 0,
    };
    let deadline = Instant::now() + Duration::from_secs(40);
    let session = loop {
        match core
            .connect_login_screen(entry.id.clone(), "vm-phone".into(), settings.clone())
            .await
        {
            Ok(s) => break s,
            Err(DishusError::Rejected { msg }) => {
                println!("refused: {msg}");
                return true;
            }
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                println!("connect failed: {e}");
                return false;
            }
        }
    };
    println!("connected");
    let start = Instant::now();
    // Video in its own task: FFI next_control() isn't cancel-safe, so it is never raced.
    let frames = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (vs, vf) = (session.clone(), frames.clone());
    tokio::spawn(async move {
        let mut typed = false;
        while vs.next_video().await.is_some() {
            let n = vf.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if !typed && n >= 30 && !codes.is_empty() {
                typed = true;
                for &c in &codes {
                    vs.key(c, true).unwrap();
                    vs.key(c, false).unwrap();
                    tokio::time::sleep(Duration::from_millis(80)).await;
                }
                println!("typed {} keys after {n} frames", codes.len());
            }
        }
    });
    let control = async {
        loop {
            match session.next_control().await {
                Some(ControlEvent::Welcome { pc_name, .. }) => println!("welcome from {pc_name:?}"),
                Some(ControlEvent::StreamInfo {
                    width,
                    height,
                    encoder,
                    ..
                }) => {
                    println!("stream {width}x{height} {encoder}")
                }
                Some(ControlEvent::Notice { message }) => println!("notice: {message}"),
                Some(ControlEvent::Closed { reason }) => break reason,
                _ => {}
            }
        }
    };
    let reason = tokio::time::timeout(Duration::from_secs(seconds), control)
        .await
        .unwrap_or_else(|_| "still open at the end".into());
    println!(
        "closed after {:.0} s: {reason} ({} frames)",
        start.elapsed().as_secs_f32(),
        frames.load(std::sync::atomic::Ordering::Relaxed)
    );
    session.disconnect().await;
    true
}
