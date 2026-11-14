//! X11 capture against a real X server (Xvfb with an auth cookie), through the actual capture
//! worker process. Skipped (with a note) when Xvfb, xauth, ximagesrc or x264enc is missing.

#![cfg(all(target_os = "linux", feature = "capture"))]

use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

use dishus_daemon::capture::wire::{MAX_CMD_FRAME, MAX_WORKER_FRAME, WorkerCmd, WorkerEvent};
use dishus_proto::{read_frame, write_frame};
use tokio::process::Command as TokioCommand;

/// One Xvfb at a time: parallel tests picking the same free display number would talk to each
/// other's server (and cookie).
static ONE_X: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn have(cmd: &str, arg: &str) -> bool {
    Command::new(cmd)
        .arg(arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn ready() -> bool {
    let ok = have("Xvfb", "-help")
        && have("xauth", "-V")
        && have("gst-inspect-1.0", "ximagesrc")
        && have("gst-inspect-1.0", "x264enc")
        && have("gst-inspect-1.0", "h264parse");
    if !ok {
        eprintln!(
            "SKIPPED: needs Xvfb, xauth, ximagesrc (plugins-good), x264enc (plugins-ugly), h264parse (plugins-bad)"
        );
    }
    ok
}

/// An Xvfb that only accepts clients with the cookie in `auth`.
struct Xvfb {
    child: Child,
    display: String,
    auth: PathBuf,
}

impl Xvfb {
    fn start(dir: &Path) -> Xvfb {
        let n = (90..140)
            .find(|n| {
                !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
                    && !Path::new(&format!("/tmp/.X{n}-lock")).exists()
            })
            .expect("a free display number");
        let display = format!(":{n}");
        let auth = dir.join("xauth");
        let cookie: String = (0..16).map(|_| format!("{:02x}", rand_byte())).collect();
        let st = Command::new("xauth")
            .args(["-q", "-f"])
            .arg(&auth)
            .args(["add", &display, ".", &cookie])
            .status()
            .unwrap();
        assert!(st.success(), "xauth add");
        let mut child = Command::new("Xvfb")
            .arg(&display)
            .arg("-auth")
            .arg(&auth)
            .args(["-screen", "0", "1280x720x24", "-nolisten", "tcp"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let sock = format!("/tmp/.X11-unix/X{n}");
        for _ in 0..100 {
            if Path::new(&sock).exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(Path::new(&sock).exists(), "Xvfb didn't start");
        assert!(
            child.try_wait().unwrap().is_none(),
            "Xvfb {display} exited (display taken?)"
        );
        Xvfb {
            child,
            display,
            auth,
        }
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn rand_byte() -> u8 {
    let mut b = [0u8; 1];
    getrandom::fill(&mut b).unwrap();
    b[0]
}

/// The worker as the daemon starts it, with this X display and auth file and no portal.
fn worker(dir: &Path, display: &str, xauthority: &Path) -> tokio::process::Child {
    TokioCommand::new(env!("CARGO_BIN_EXE_dishus-daemon"))
        .args(["--capture-worker", "session"])
        .env("DISHUS_W_TOKEN_FILE", dir.join("token"))
        .env("DISHUS_W_ENCODER", "x264")
        .env("DISPLAY", display)
        .env("XAUTHORITY", xauthority)
        .env_remove("WAYLAND_DISPLAY")
        // no session bus: the portal is "unavailable", as on a plain X11 desktop
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent/dishus-test-bus",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn next(out: &mut tokio::process::ChildStdout) -> WorkerEvent {
    tokio::time::timeout(Duration::from_secs(20), read_frame(out, MAX_WORKER_FRAME))
        .await
        .expect("worker answered")
        .expect("worker event")
}

async fn start(w: &mut tokio::process::Child) {
    let cmd = WorkerCmd::Start {
        monitor: 0,
        fps: 30,
        bitrate_kbps: 2000,
        max_height: 0,
    };
    write_frame(w.stdin.as_mut().unwrap(), &cmd, MAX_CMD_FRAME)
        .await
        .unwrap();
}

#[tokio::test]
async fn x11_screen_streams_with_the_session_cookie() {
    if !ready() {
        return;
    }
    let _one = ONE_X.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let x = Xvfb::start(dir.path());
    let mut w = worker(dir.path(), &x.display, &x.auth);
    let mut out = w.stdout.take().unwrap();
    assert!(matches!(next(&mut out).await, WorkerEvent::Monitors(m) if m.len() == 1));
    start(&mut w).await;
    // Started and the first frames, in either order.
    let (mut started, mut key) = (None, false);
    for _ in 0..10 {
        match next(&mut out).await {
            WorkerEvent::Started {
                width,
                height,
                encoder,
            } => started = Some((width, height, encoder)),
            WorkerEvent::Frame { keyframe, data, .. } => {
                assert!(data.len() > 100);
                key |= keyframe;
            }
            other => panic!("unexpected {other:?}"),
        }
        if started.is_some() && key {
            break;
        }
    }
    assert_eq!(started, Some((1280, 720, "x264".to_string())));
    assert!(key, "a keyframe arrived");
}

#[tokio::test]
async fn hidden_cookie_file_is_named_in_the_error() {
    if !ready() {
        return;
    }
    let _one = ONE_X.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let x = Xvfb::start(dir.path());
    // What a PrivateTmp service sees: XAUTHORITY points at a file that isn't there.
    let mut w = worker(dir.path(), &x.display, &dir.path().join("missing-xauth"));
    let mut out = w.stdout.take().unwrap();
    match next(&mut out).await {
        WorkerEvent::Failed(e) => {
            assert!(e.contains("can't read the X auth file"), "{e}");
            assert!(e.contains("PrivateTmp"), "{e}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn wrong_cookie_names_the_failing_element() {
    if !ready() {
        return;
    }
    let _one = ONE_X.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let x = Xvfb::start(dir.path());
    // A readable auth file without this display's cookie: X says "Authorization required".
    let empty = dir.path().join("empty-xauth");
    std::fs::write(&empty, b"").unwrap();
    let mut w = worker(dir.path(), &x.display, &empty);
    let mut out = w.stdout.take().unwrap();
    assert!(matches!(next(&mut out).await, WorkerEvent::Monitors(_)));
    start(&mut w).await;
    match next(&mut out).await {
        WorkerEvent::Failed(e) => {
            assert!(e.contains("ximagesrc"), "{e}");
            assert!(e.contains("XAUTHORITY"), "{e}");
            assert!(!e.contains("Element failed to change its state"), "{e}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}
