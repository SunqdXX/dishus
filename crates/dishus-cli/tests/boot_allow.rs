//! `dishus boot allow` run as the normal user, against fake daemons: it shows the login code,
//! waits for the phone's new identity, then asks sudo to allow exactly that identity. The root
//! step itself (the daemon side of `Allow`) is covered by the daemon's e2e tests.
#![cfg(target_os = "linux")]

use std::{os::unix::fs::PermissionsExt, path::Path, process::Command};

use dishus_proto::{
    LoginInfo, PairingInfo,
    ipc::{IpcRequest, IpcResponse, IpcStatus, MAX_IPC_FRAME},
    read_frame, write_frame,
};
use tokio::net::UnixListener;

const PHONE: &str = "ab00000000000000000000000000000000000000000000000000000000000001";
const BOOT: &str = "cd00000000000000000000000000000000000000000000000000000000000002";

/// Answer control requests on `path` with `reply` until the test ends.
fn fake_daemon(path: &Path, reply: impl Fn(IpcRequest) -> IpcResponse + Send + 'static) {
    let path = path.to_owned();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let l = UnixListener::bind(&path).unwrap();
            tx.send(()).unwrap();
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let req: IpcRequest = read_frame(&mut s, MAX_IPC_FRAME).await.unwrap();
                write_frame(&mut s, &reply(req), MAX_IPC_FRAME)
                    .await
                    .unwrap();
            }
        });
    });
    rx.recv().unwrap();
}

fn pairing() -> PairingInfo {
    PairingInfo {
        endpoint_id: [5; 32],
        token: [6; 32],
        pc_name: "desk".into(),
        relays: vec![],
        addrs: vec!["10.0.0.5:7001".into()],
    }
}

#[test]
fn allow_as_the_user_shows_the_code_and_sudo_allows_the_new_identity() {
    let dir = tempfile::tempdir().unwrap();
    let rt = dir.path().join("run");
    std::fs::create_dir_all(rt.join("dishus")).unwrap();
    let boot_sock = dir.path().join("boot.sock");
    // login mode: who it is and where
    fake_daemon(&boot_sock, |req| match req {
        IpcRequest::Status => IpcResponse::Status(IpcStatus {
            version: "t".into(),
            pc_name: "desk".into(),
            endpoint_id: BOOT.into(),
            relays: vec!["https://relay.example/".into()],
            direct_addrs: vec!["127.0.0.1:9".into(), "10.0.0.5:7000".into()],
            lockdown: false,
            pairing_open: false,
            sessions: vec![],
            devices: 0,
            capture_backend: String::new(),
            input_backend: String::new(),
        }),
        r => IpcResponse::Error(format!("login mode got {r:?}")),
    });
    // the session daemon: an enroll window, and the phone shows up on the second poll
    let polls = std::sync::atomic::AtomicU32::new(0);
    fake_daemon(&rt.join("dishus/control.sock"), move |req| match req {
        IpcRequest::LoginEnroll => IpcResponse::Pairing {
            uri: pairing().to_uri(),
            ttl_secs: 30,
        },
        IpcRequest::LoginEnrolled => {
            let n = polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            IpcResponse::Enrolled((n >= 1).then(|| (PHONE.into(), "phone login".into())))
        }
        r => IpcResponse::Error(format!("session daemon got {r:?}")),
    });
    // sudo records what it would run as root
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = dir.path().join("sudo.log");
    let sudo = bin.join("sudo");
    std::fs::write(
        &sudo,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_dishus"))
        .args(["boot", "allow", "--days", "20"])
        .env("XDG_RUNTIME_DIR", &rt)
        .env("DISHUS_BOOT_SOCKET", &boot_sock)
        .env(
            "PATH",
            format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // the code: the session's pairing window + where login mode is (no loopback address)
    let code = stdout
        .lines()
        .find(|l| l.starts_with("dishus://login?"))
        .expect("no code printed");
    let info = LoginInfo::from_uri(code).unwrap();
    assert_eq!(info.pair, pairing());
    assert_eq!(info.boot_id[0], 0xcd);
    assert_eq!(info.boot_id[31], 0x02);
    assert_eq!(info.boot_addrs, vec!["10.0.0.5:7000".to_string()]);
    assert_eq!(info.boot_relays, vec!["https://relay.example/".to_string()]);
    assert!(stdout.contains("\"phone login\" scanned it"), "{stdout}");

    // sudo was asked to allow exactly the enrolled identity
    let args: Vec<String> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        &args[1..],
        [
            "boot",
            "allow-id",
            PHONE,
            "--name",
            "phone login",
            "--days",
            "20"
        ]
    );
    assert!(args[0].ends_with("/dishus"), "{}", args[0]);
}

#[test]
fn allow_id_needs_root() {
    let out = Command::new(env!("CARGO_BIN_EXE_dishus"))
        .args(["boot", "allow-id", PHONE, "--name", "x"])
        .env("DISHUS_BOOT_SOCKET", "/nonexistent")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("dishus boot allow"));
}
