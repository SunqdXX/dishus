//! End-to-end tests: real daemon network stack + real client core over loopback QUIC, with fake
//! input and capture backends. No relays, no DNS, no GPU, no uinput needed.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use dishus_core::{Client, NetConfig, PairedPc, Session, vault};
use dishus_daemon::{
    backend::{
        AudioControl, AudioPacket, AudioStream, BoxFut, BoxFuture, CaptureProvider, CaptureSession,
        EncodedStream, InputSink, StreamConfig, StreamControl, TrackedInput, VideoFrame,
    },
    boot::{BootState, Rules, Seat},
    config::Config,
    devices::DeviceStore,
    net::{NetOptions, spawn_router},
    notify::Notifier,
    paths::Paths,
    state::AppState,
};
use dishus_proto::{
    ALPN_SESSION, ALPN_SESSION_V2, ByeReason, ClientMsg, MonitorInfo, MouseButton, PROTO_VERSION,
    PairingInfo, ServerMsg, StreamParams, read_frame, write_frame,
};
use iroh::{SecretKey, protocol::Router};
use tokio::{sync::mpsc, time::timeout};

// ---------- fakes ----------

#[derive(Clone, Default)]
struct RecInput(Arc<Mutex<Vec<String>>>);

impl RecInput {
    fn log(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl InputSink for RecInput {
    fn key(&mut self, c: u16, d: bool) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("key {c} {}", if d { "down" } else { "up" }));
        Ok(())
    }
    fn button(&mut self, b: MouseButton, d: bool) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap()
            .push(format!("btn {b:?} {}", if d { "down" } else { "up" }));
        Ok(())
    }
    fn motion(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        self.0.lock().unwrap().push(format!("move {dx} {dy}"));
        Ok(())
    }
    fn scroll(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        self.0.lock().unwrap().push(format!("scroll {dx} {dy}"));
        Ok(())
    }
}

struct FakeCtl {
    key: AtomicBool,
}
impl StreamControl for FakeCtl {
    fn force_keyframe(&self) {
        self.key.store(true, Ordering::SeqCst);
    }
    fn set_bitrate(&self, _: u32) {}
    fn set_fps(&self, _: u8) {}
}

/// `stall_after`: the fake encoder stops producing frames after this many, until a keyframe is
/// forced (models the lost-keyframe-request freeze). `Some(0)`: one keyframe, then frozen.
struct FakeCaptureSession {
    stall_after: Option<u64>,
}
impl CaptureSession for FakeCaptureSession {
    fn monitors(&self) -> Vec<MonitorInfo> {
        vec![MonitorInfo {
            index: 0,
            name: "FAKE-1".into(),
            width: 640,
            height: 360,
        }]
    }
    fn start_stream(&mut self, _cfg: StreamConfig) -> BoxFut<'_, anyhow::Result<EncodedStream>> {
        let stall = self.stall_after;
        Box::pin(async move { fake_stream(stall) })
    }
}

fn fake_stream(stall_after: Option<u64>) -> anyhow::Result<EncodedStream> {
    {
        let (tx, rx) = mpsc::channel(4);
        let ctl = Arc::new(FakeCtl {
            key: AtomicBool::new(true),
        });
        let c2 = ctl.clone();
        let task = tokio::spawn(async move {
            let mut pts = 0u64;
            let mut n = 0u64;
            loop {
                tokio::time::sleep(Duration::from_millis(15)).await;
                n += 1;
                if stall_after == Some(0) && n > 1 {
                    continue; // frozen for good: one keyframe, then nothing, whatever is asked
                }
                if stall_after.is_some_and(|s| n > s) && !c2.key.load(Ordering::SeqCst) {
                    continue; // stuck until someone asks for a keyframe
                }
                let key = c2.key.swap(false, Ordering::SeqCst);
                pts += 15_000;
                let nal: &[u8] = if key {
                    &[0, 0, 0, 1, 0x65, 1, 2, 3]
                } else {
                    &[0, 0, 0, 1, 0x41, 4, 5]
                };
                let f = VideoFrame {
                    data: Bytes::from_static(nal),
                    pts_us: pts,
                    keyframe: key,
                };
                if tx.send(f).await.is_err() {
                    break;
                }
            }
        });
        struct Abort(tokio::task::JoinHandle<()>);
        impl Drop for Abort {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        Ok(EncodedStream {
            frames: rx,
            control: ctl,
            width: 640,
            height: 360,
            encoder: "fake".into(),
            _guard: Box::new(Abort(task)),
        })
    }
}

/// Opening capture takes this long (a slow portal / first worker start); optional stall.
struct FakeCapture(Duration, Option<u64>);
impl CaptureProvider for FakeCapture {
    fn open(&self) -> BoxFuture<anyhow::Result<Box<dyn CaptureSession>>> {
        let (d, stall_after) = (self.0, self.1);
        Box::pin(async move {
            tokio::time::sleep(d).await;
            Ok(Box::new(FakeCaptureSession { stall_after }) as Box<dyn CaptureSession>)
        })
    }
    fn setup_interactive(&self) -> BoxFuture<anyhow::Result<String>> {
        Box::pin(async { Ok("fake".into()) })
    }
    /// A 20 ms fake Opus packet stream, pts in microseconds from 1 s.
    fn open_audio(&self) -> BoxFuture<anyhow::Result<AudioStream>> {
        Box::pin(async {
            struct NoCtl;
            impl AudioControl for NoCtl {
                fn set_bitrate(&self, _: u32) {}
            }
            let (tx, rx) = mpsc::channel(16);
            let task = tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                for i in 0u64.. {
                    tick.tick().await;
                    let p = AudioPacket {
                        pts_us: 1_000_000 + i * 20_000,
                        data: Bytes::from_static(&[0xFC, 0xFF, 0xFE]),
                    };
                    if tx.send(p).await.is_err() {
                        break;
                    }
                }
            });
            struct Abort(tokio::task::JoinHandle<()>);
            impl Drop for Abort {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            Ok(AudioStream {
                packets: rx,
                control: Arc::new(NoCtl),
                _guard: Box::new(Abort(task)),
            })
        })
    }
}

/// The login screen can't be captured (no X access, no display).
struct NoCapture;
impl CaptureProvider for NoCapture {
    fn open(&self) -> BoxFuture<anyhow::Result<Box<dyn CaptureSession>>> {
        Box::pin(async { Err(anyhow::anyhow!("cannot open display")) })
    }
    fn setup_interactive(&self) -> BoxFuture<anyhow::Result<String>> {
        Box::pin(async { Err(anyhow::anyhow!("no")) })
    }
}

// ---------- harness ----------

struct Pc {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
    router: Router,
    input: RecInput,
}

impl Pc {
    async fn start() -> Self {
        Self::start_with(Duration::ZERO).await
    }

    async fn start_with(capture_open: Duration) -> Self {
        Self::start_full(capture_open, None).await
    }

    async fn start_full(capture_open: Duration, stall_after: Option<u64>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let cfg = Config {
            pc_name: "test-pc".into(),
            ..Default::default()
        };
        let input = RecInput::default();
        let state = Arc::new(AppState::new(
            paths,
            cfg,
            DeviceStore::default(),
            TrackedInput::new(Box::new(input.clone())),
            Arc::new(FakeCapture(capture_open, stall_after)),
            Notifier::disabled(),
        ));
        let router = spawn_router(
            state.clone(),
            SecretKey::generate(),
            &NetOptions::local_test(),
        )
        .await
        .unwrap();
        Pc {
            _dir: dir,
            state,
            router,
            input,
        }
    }

    /// Login mode at the login screen, with `phone` allowed (`stall_after` as in
    /// [`FakeCaptureSession`]; `fail_capture`: no picture at all).
    async fn start_boot(stall_after: Option<u64>, fail_capture: bool, phone: &Client) -> Self {
        Self::start_boot_with(stall_after, fail_capture, phone, Rules::default()).await
    }

    async fn start_boot_with(
        stall_after: Option<u64>,
        fail_capture: bool,
        phone: &Client,
        rules: Rules,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let boot = Arc::new(
            BootState::new(dir.path().join("limits.toml"), Some(1000), None)
                .unwrap()
                .with_rules(rules),
        );
        boot.set_seat(Seat::Greeter {
            display: ":0".into(),
        });
        let input = RecInput::default();
        let capture: Arc<dyn CaptureProvider> = if fail_capture {
            Arc::new(NoCapture)
        } else {
            Arc::new(FakeCapture(Duration::ZERO, stall_after))
        };
        let state = Arc::new(
            AppState::new(
                paths,
                Config {
                    pc_name: "test-pc".into(),
                    ..Default::default()
                },
                DeviceStore::default(),
                TrackedInput::new(Box::new(input.clone())),
                capture,
                Notifier::disabled(),
            )
            .with_boot(boot),
        );
        state
            .allow_device(&phone.endpoint_id(), "phone", 90)
            .unwrap();
        let router = spawn_router(
            state.clone(),
            SecretKey::generate(),
            &NetOptions::local_test(),
        )
        .await
        .unwrap();
        Pc {
            _dir: dir,
            state,
            router,
            input,
        }
    }

    fn boot(&self) -> &BootState {
        self.state.boot.as_deref().unwrap()
    }

    fn pairing_link(&self) -> PairingInfo {
        let (tok, _) = self.state.open_pairing().unwrap();
        PairingInfo {
            endpoint_id: *self.router.endpoint().id().as_bytes(),
            token: *tok,
            pc_name: "test-pc".into(),
            relays: vec![],
            addrs: self
                .router
                .endpoint()
                .bound_sockets()
                .iter()
                .map(|a| a.to_string())
                .collect(),
        }
    }

    fn as_paired(&self) -> PairedPc {
        PairedPc {
            endpoint_id: *self.router.endpoint().id().as_bytes(),
            name: "test-pc".into(),
            relays: vec![],
            addrs: self
                .router
                .endpoint()
                .bound_sockets()
                .iter()
                .map(|a| a.to_string())
                .collect(),
            expires_at: 0,
        }
    }
}

async fn phone() -> Client {
    let id = vault::new_identity().unwrap();
    Client::new(
        &id.secret_key,
        &NetConfig {
            local_test: true,
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

async fn connect(c: &Client, pc: &PairedPc) -> anyhow::Result<Session> {
    let s = c.connect(pc, "phone", StreamParams::default()).await?;
    // The gate closes rejected connections right after the handshake; that surfaces on the
    // first read, not in connect().
    match timeout(Duration::from_secs(5), s.next_control()).await {
        Ok(Some(ServerMsg::Welcome { .. })) => Ok(s),
        other => anyhow::bail!("no welcome: {other:?}"),
    }
}

/// Connect expecting a refusal; returns what the PC said (None = refused without a reason).
async fn refusal(c: &Client, pc: &PairedPc) -> Option<dishus_core::Refusal> {
    match c.connect(pc, "phone", StreamParams::default()).await {
        Err(e) => dishus_core::Refusal::find(&e),
        Ok(s) => {
            // Refused right after the handshake: the session sees the close on first read.
            match timeout(Duration::from_secs(5), s.next_control()).await {
                Ok(None) => s.refusal(),
                other => panic!("expected a refusal, got {other:?}"),
            }
        }
    }
}

async fn wait_for<F: Fn() -> bool>(f: F) {
    timeout(Duration::from_secs(5), async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not met in time");
}

async fn expect_bye(s: &Session) -> ByeReason {
    loop {
        match timeout(Duration::from_secs(5), s.next_control())
            .await
            .expect("no bye in time")
        {
            Some(ServerMsg::Bye { reason }) => return reason,
            Some(_) => continue,
            None => panic!("closed without bye"),
        }
    }
}

// ---------- tests ----------

#[tokio::test(flavor = "multi_thread")]
async fn pair_connect_input_video_release() {
    let pc = Pc::start().await;
    let ph = phone().await;
    let paired = ph.pair(&pc.pairing_link(), "Pixel").await.unwrap();
    assert_eq!(paired.name, "test-pc");
    assert!(pc.state.device(&ph.endpoint_id()).is_some());

    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    // video: first frame must be a keyframe, sequence increasing
    let v = timeout(Duration::from_secs(5), s.next_video())
        .await
        .unwrap()
        .unwrap();
    assert!(v.header.is_keyframe());
    let v2 = timeout(Duration::from_secs(5), s.next_video())
        .await
        .unwrap()
        .unwrap();
    assert!(v2.header.seq > v.header.seq);

    // Super held down, plus a click held, then the phone vanishes
    s.send(ClientMsg::Key {
        code: 125,
        down: true,
    })
    .unwrap();
    s.send(ClientMsg::MouseButton {
        button: MouseButton::Left,
        down: true,
    })
    .unwrap();
    s.send(ClientMsg::MouseMove { dx: 3, dy: -2 }).unwrap();
    wait_for(|| pc.input.log().iter().any(|l| l == "move 3 -2")).await;
    drop(s);
    wait_for(|| pc.input.log().contains(&"key 125 up".to_string())).await;
    let log = pc.input.log();
    assert!(log.contains(&"btn Left up".to_string()), "{log:?}");
    // Held input is released first, then the session is unregistered: wait for the second
    // step too instead of racing it (a slow CI runner caught the gap between them).
    wait_for(|| pc.state.sessions().is_empty()).await;
}

/// Caps Lock is a toggle, not a held key: a tap leaves nothing "held", so it used to stay on
/// after the phone left (on Hyprland only on DiShus's own keyboard, invisible on the real one's
/// LED). The session end must turn it back off, and only then, not on "Release all keys".
#[tokio::test(flavor = "multi_thread")]
async fn caps_lock_left_on_is_turned_off_at_disconnect() {
    let pc = Pc::start().await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "p").await.unwrap();
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    let caps = |log: &[String]| log.iter().filter(|l| *l == "key 58 down").count();
    for down in [true, false] {
        s.send(ClientMsg::Key { code: 58, down }).unwrap();
    }
    s.send(ClientMsg::ReleaseAll).unwrap();
    s.send(ClientMsg::MouseMove { dx: 1, dy: 1 }).unwrap();
    wait_for(|| pc.input.log().iter().any(|l| l == "move 1 1")).await;
    assert_eq!(caps(&pc.input.log()), 1, "{:?}", pc.input.log());
    drop(s);
    wait_for(|| caps(&pc.input.log()) == 2).await;
    wait_for(|| pc.state.sessions().is_empty()).await;
    let log = pc.input.log();
    assert_eq!(log.last().map(String::as_str), Some("key 58 up"), "{log:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_peer_is_dropped() {
    let pc = Pc::start().await;
    let stranger = phone().await;
    assert!(connect(&stranger, &pc.as_paired()).await.is_err());
    assert!(pc.input.log().is_empty());
    assert!(pc.state.sessions().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_closed_by_default() {
    let pc = Pc::start().await;
    let link = pc.pairing_link();
    pc.state.close_pairing();
    let ph = phone().await;
    assert!(ph.pair(&link, "p").await.is_err());
    assert!(pc.state.device(&ph.endpoint_id()).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_token_and_reuse_rejected() {
    let pc = Pc::start().await;
    let mut link = pc.pairing_link();
    let good = link.token;
    link.token[0] ^= 0xff;
    let a = phone().await;
    assert!(a.pair(&link, "a").await.is_err(), "wrong token");
    link.token = good;
    a.pair(&link, "a")
        .await
        .expect("right token still works after one miss");
    let b = phone().await;
    assert!(b.pair(&link, "b").await.is_err(), "single use");
    assert!(pc.state.device(&b.endpoint_id()).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_device_is_dropped_and_cli_renew_restores() {
    let pc = Pc::start().await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "p").await.unwrap();
    {
        // force expiry into the past
        let mut d = pc.state.devices.write().unwrap();
        d.renew(&ph.endpoint_id(), 1, 0).unwrap();
    }
    assert_eq!(
        refusal(&ph, &pc.as_paired()).await,
        Some(dishus_core::Refusal::Expired)
    );
    pc.state.renew(&ph.endpoint_id(), 30).unwrap();
    connect(&ph, &pc.as_paired()).await.expect("renewed");
}

#[tokio::test(flavor = "multi_thread")]
async fn revoke_kicks_live_session_and_blocks_reconnect() {
    let pc = Pc::start().await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "victim").await.unwrap();
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    s.send(ClientMsg::Key {
        code: 29,
        down: true,
    })
    .unwrap();
    wait_for(|| pc.input.log().contains(&"key 29 down".to_string())).await;
    pc.state.revoke("victim").unwrap();
    assert_eq!(expect_bye(&s).await, ByeReason::Revoked);
    wait_for(|| pc.input.log().contains(&"key 29 up".to_string())).await;
    // A revoked device is unknown again: refused without being told anything.
    assert_eq!(refusal(&ph, &pc.as_paired()).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn renew_from_inside_session() {
    let pc = Pc::start().await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "p").await.unwrap();
    let before = pc.state.device(&ph.endpoint_id()).unwrap().expires_at;
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    s.send(ClientMsg::RenewPairing {
        days: dishus_proto::MAX_EXPIRY_DAYS,
    })
    .unwrap();
    let exp = loop {
        match timeout(Duration::from_secs(5), s.next_control())
            .await
            .unwrap()
            .unwrap()
        {
            ServerMsg::Renewed { expires_at } => break expires_at,
            _ => continue,
        }
    };
    assert!(exp >= before);
    assert_eq!(pc.state.device(&ph.endpoint_id()).unwrap().expires_at, exp);
    assert!(
        s.send(ClientMsg::RenewPairing {
            days: dishus_proto::MAX_EXPIRY_DAYS + 1
        })
        .is_err(),
        "validated client side too"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_cuts_and_locks_until_unlock() {
    let pc = Pc::start().await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "p").await.unwrap();
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    assert_eq!(pc.state.kill().unwrap(), 1);
    assert_eq!(expect_bye(&s).await, ByeReason::Killed);
    assert_eq!(
        refusal(&ph, &pc.as_paired()).await,
        Some(dishus_core::Refusal::Lockdown)
    );
    assert!(pc.state.open_pairing().is_err(), "no pairing in lockdown");
    assert!(pc.state.paths.lockdown_file().exists(), "persisted");
    pc.state.unlock().unwrap();
    connect(&ph, &pc.as_paired()).await.expect("unlocked");
}

#[tokio::test(flavor = "multi_thread")]
async fn silent_phone_times_out_and_keys_release() {
    let pc = Pc::start().await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "p").await.unwrap();
    // Raw connection that never sends heartbeats after pressing a key.
    let raw = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")
        .unwrap()
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    // pair the raw endpoint too
    {
        let mut d = pc.state.devices.write().unwrap();
        d.add(&raw.id(), "raw", 1, dishus_daemon::devices::now_unix())
            .unwrap();
    }
    let addr = iroh::EndpointAddr::from_parts(
        pc.router.endpoint().id(),
        pc.router
            .endpoint()
            .bound_sockets()
            .into_iter()
            .map(iroh::TransportAddr::Ip),
    );
    let conn = raw.connect(addr, ALPN_SESSION).await.unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    let hello = ClientMsg::Hello {
        proto_version: PROTO_VERSION,
        device_name: "raw".into(),
        stream: StreamParams::default(),
    };
    write_frame(&mut send, &hello, 4096).await.unwrap();
    write_frame(
        &mut send,
        &ClientMsg::Key {
            code: 56,
            down: true,
        },
        4096,
    )
    .await
    .unwrap();
    wait_for(|| pc.input.log().contains(&"key 56 down".to_string())).await;
    // Now silence (a network blackout looks like this). Held input must be released long
    // before the session times out, so a held key can't auto-repeat for seconds...
    let t = std::time::Instant::now();
    timeout(Duration::from_secs(4), async {
        while !pc.input.log().contains(&"key 56 up".to_string()) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("held key was not released during silence");
    assert!(
        t.elapsed() >= Duration::from_millis(1500),
        "released too eagerly ({:?}): one late heartbeat must not drop held keys",
        t.elapsed()
    );
    // ...while the session itself survives (the phone may just be switching networks)...
    assert!(
        conn.close_reason().is_none(),
        "session ended on short silence"
    );
    // ...and only ends at the idle timeout.
    timeout(Duration::from_secs(6), conn.closed())
        .await
        .expect("silent session should time out");
}

/// The PC sends nothing until screen capture is open (up to 15 s on a slow portal); the phone
/// must not take that for a dead PC and loop reconnecting.
#[tokio::test(flavor = "multi_thread")]
async fn slow_capture_open_is_not_a_dead_pc() {
    let pc = Pc::start_with(Duration::from_secs(8)).await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "p").await.unwrap();
    let s = ph
        .connect(&pc.as_paired(), "phone", StreamParams::default())
        .await
        .unwrap();
    let welcome = timeout(Duration::from_secs(12), s.next_control())
        .await
        .expect("no message within 12 s");
    assert!(
        matches!(welcome, Some(ServerMsg::Welcome { .. })),
        "{welcome:?} (timed out: {})",
        s.timed_out()
    );
    s.send(ClientMsg::Key {
        code: 30,
        down: true,
    })
    .unwrap();
    s.send(ClientMsg::Key {
        code: 30,
        down: false,
    })
    .unwrap();
    wait_for(|| pc.input.log().contains(&"key 30 up".to_string())).await;
}

/// Android reports network changes often (link properties, a second network appearing): a
/// session that is still healthy must not be cut by them.
#[tokio::test(flavor = "multi_thread")]
async fn network_change_keeps_a_healthy_session() {
    let pc = Pc::start().await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "p").await.unwrap();
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    for _ in 0..6 {
        ph.network_change().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(!s.is_closed() && !s.timed_out(), "healthy session was cut");
}

/// The picture froze for good when the encoder never produced the keyframe the PC asked for.
/// The phone notices the missing video and keeps asking, so video comes back by itself.
#[tokio::test(flavor = "multi_thread")]
async fn stuck_video_recovers() {
    let pc = Pc::start_full(Duration::ZERO, Some(20)).await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "p").await.unwrap();
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    let mut last_seq = 0;
    // the first ~20 frames, then silence
    while let Ok(Some(v)) = timeout(Duration::from_millis(600), s.next_video()).await {
        last_seq = v.header.seq;
    }
    assert!(last_seq > 0, "video never started");
    // frames resume, starting with a keyframe
    let v = timeout(Duration::from_secs(4), s.next_video())
        .await
        .expect("video stayed frozen")
        .unwrap();
    assert!(v.header.seq > last_seq);
    assert!(v.header.is_keyframe());
}

/// A frame split across packets with a pause in the middle must not be lost: the session
/// loop's other timers must never cancel a half-read frame.
#[tokio::test(flavor = "multi_thread")]
async fn split_frame_survives_timers() {
    let pc = Pc::start().await;
    let raw = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")
        .unwrap()
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    {
        let mut d = pc.state.devices.write().unwrap();
        d.add(&raw.id(), "raw", 1, dishus_daemon::devices::now_unix())
            .unwrap();
    }
    let addr = iroh::EndpointAddr::from_parts(
        pc.router.endpoint().id(),
        pc.router
            .endpoint()
            .bound_sockets()
            .into_iter()
            .map(iroh::TransportAddr::Ip),
    );
    let conn = raw.connect(addr, ALPN_SESSION).await.unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    let hello = ClientMsg::Hello {
        proto_version: PROTO_VERSION,
        device_name: "raw".into(),
        stream: StreamParams::default(),
    };
    write_frame(&mut send, &hello, 4096).await.unwrap();
    let mut buf = Vec::new();
    write_frame(
        &mut buf,
        &ClientMsg::Key {
            code: 30,
            down: true,
        },
        4096,
    )
    .await
    .unwrap();
    // pings keep the session alive; the key frame is split around them in time
    let ping = |t| ClientMsg::Ping { t };
    write_frame(&mut send, &ping(1), 4096).await.unwrap();
    send.write_all(&buf[..3]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    send.write_all(&buf[3..]).await.unwrap();
    write_frame(&mut send, &ping(2), 4096).await.unwrap();
    write_frame(
        &mut send,
        &ClientMsg::Key {
            code: 30,
            down: false,
        },
        4096,
    )
    .await
    .unwrap();
    wait_for(|| pc.input.log().contains(&"key 30 up".to_string())).await;
    assert!(
        conn.close_reason().is_none(),
        "split frame broke the session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_message_ends_session() {
    let pc = Pc::start().await;
    let raw = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")
        .unwrap()
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    {
        let mut d = pc.state.devices.write().unwrap();
        d.add(&raw.id(), "raw", 1, dishus_daemon::devices::now_unix())
            .unwrap();
    }
    let addr = iroh::EndpointAddr::from_parts(
        pc.router.endpoint().id(),
        pc.router
            .endpoint()
            .bound_sockets()
            .into_iter()
            .map(iroh::TransportAddr::Ip),
    );
    let conn = raw.connect(addr, ALPN_SESSION).await.unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    let hello = ClientMsg::Hello {
        proto_version: PROTO_VERSION,
        device_name: "raw".into(),
        stream: StreamParams::default(),
    };
    write_frame(&mut send, &hello, 4096).await.unwrap();
    // KEY_POWER is refused by validation -> protocol error -> session ends, nothing injected
    write_frame(
        &mut send,
        &ClientMsg::Key {
            code: 116,
            down: true,
        },
        4096,
    )
    .await
    .unwrap();
    // oversize length prefix
    let _ = send.write_all(&u32::MAX.to_be_bytes()).await;
    timeout(Duration::from_secs(5), conn.closed())
        .await
        .expect("daemon should close the connection");
    assert!(!pc.input.log().iter().any(|l| l.starts_with("key 116")));
}

/// A phone speaking raw protocol on `alpn`: Hello sent, Welcome read.
async fn raw_session(
    pc: &Pc,
    alpn: &[u8],
) -> (
    iroh::endpoint::Connection,
    iroh::endpoint::SendStream,
    iroh::endpoint::RecvStream,
    iroh::Endpoint,
) {
    let raw = iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0")
        .unwrap()
        .relay_mode(iroh::RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    {
        let mut d = pc.state.devices.write().unwrap();
        d.add(&raw.id(), "raw", 1, dishus_daemon::devices::now_unix())
            .unwrap();
    }
    let addr = iroh::EndpointAddr::from_parts(
        pc.router.endpoint().id(),
        pc.router
            .endpoint()
            .bound_sockets()
            .into_iter()
            .map(iroh::TransportAddr::Ip),
    );
    let conn = raw.connect(addr, alpn).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let hello = ClientMsg::Hello {
        proto_version: PROTO_VERSION,
        device_name: "raw".into(),
        stream: StreamParams::default(),
    };
    write_frame(&mut send, &hello, 4096).await.unwrap();
    let welcome: ServerMsg = timeout(Duration::from_secs(5), read_frame(&mut recv, 65536))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        welcome,
        ServerMsg::Welcome {
            proto_version: 1,
            ..
        }
    ));
    (conn, send, recv, raw)
}

#[tokio::test(flavor = "multi_thread")]
async fn new_phone_gets_session2_old_phone_keeps_session1() {
    let pc = Pc::start().await;
    // The app's client offers both and gets session/2.
    let c = phone().await;
    let paired = c.pair(&pc.pairing_link(), "phone").await.unwrap();
    let s = connect(&c, &paired).await.unwrap();
    assert!(s.audio_supported());
    s.send(ClientMsg::SetAudio { on: false }).unwrap();
    s.close().await;

    // A released app (session/1 only) still gets a working session, input included...
    let (conn, mut send, _recv, _ep) = raw_session(&pc, ALPN_SESSION).await;
    assert_eq!(conn.alpn(), ALPN_SESSION);
    write_frame(
        &mut send,
        &ClientMsg::Key {
            code: 30,
            down: true,
        },
        4096,
    )
    .await
    .unwrap();
    wait_for(|| pc.input.log().iter().any(|l| l == "key 30 down")).await;
    // ...and a session/2 message on session/1 is a protocol error.
    write_frame(&mut send, &ClientMsg::SetAudio { on: true }, 4096)
        .await
        .unwrap();
    timeout(Duration::from_secs(5), conn.closed())
        .await
        .expect("SetAudio on session/1 must end the session");

    // session/2 asked for directly works too.
    let (conn, _send, _recv, _ep) = raw_session(&pc, ALPN_SESSION_V2).await;
    assert_eq!(conn.alpn(), ALPN_SESSION_V2);
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_on_sends_numbered_datagrams_and_off_stops_them() {
    let pc = Pc::start().await;
    let (conn, mut send, mut recv, _ep) = raw_session(&pc, ALPN_SESSION_V2).await;
    // Off by default: nothing arrives.
    assert!(
        timeout(Duration::from_millis(300), conn.read_datagram())
            .await
            .is_err()
    );
    write_frame(&mut send, &ClientMsg::SetAudio { on: true }, 4096)
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            let m: ServerMsg = read_frame(&mut recv, 65536).await.unwrap();
            if let ServerMsg::AudioStarted {
                sample_rate,
                channels,
            } = m
            {
                assert_eq!((sample_rate, channels), (48_000, 2));
                break;
            }
        }
    })
    .await
    .expect("AudioStarted");
    let mut seqs = Vec::new();
    for _ in 0..10 {
        let d = timeout(Duration::from_secs(2), conn.read_datagram())
            .await
            .unwrap()
            .unwrap();
        let (h, opus) = dishus_proto::AudioHeader::decode(&d).unwrap();
        assert_eq!(opus, [0xFC, 0xFF, 0xFE]);
        seqs.push(h.seq);
    }
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
    write_frame(&mut send, &ClientMsg::SetAudio { on: false }, 4096)
        .await
        .unwrap();
    // Datagrams already in flight may still land; then silence.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while timeout(Duration::from_millis(50), conn.read_datagram())
        .await
        .is_ok()
    {}
    assert!(
        timeout(Duration::from_millis(500), conn.read_datagram())
            .await
            .is_err(),
        "audio still flowing after SetAudio off"
    );
}

// ---------- login-screen mode ----------

#[tokio::test(flavor = "multi_thread")]
async fn login_mode_never_pairs() {
    let ph = phone().await;
    let pc = Pc::start_boot(None, false, &ph).await;
    assert!(!pc.state.is_pairing_open());
    assert!(pc.state.open_pairing().is_err(), "pairing must not open");
    // Even a phone holding a (made-up) link gets nowhere: the pairing protocol isn't served.
    let stranger = phone().await;
    let link = PairingInfo {
        endpoint_id: *pc.router.endpoint().id().as_bytes(),
        token: [7; 32],
        pc_name: "test-pc".into(),
        relays: vec![],
        addrs: pc
            .router
            .endpoint()
            .bound_sockets()
            .iter()
            .map(|a| a.to_string())
            .collect(),
    };
    assert!(stranger.pair(&link, "x").await.is_err());
    assert!(pc.state.device(&stranger.endpoint_id()).is_none());
    // and a stranger can't open a session
    assert!(connect(&stranger, &pc.as_paired()).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn login_mode_types_with_a_live_picture() {
    let ph = phone().await;
    let pc = Pc::start_boot(None, false, &ph).await;
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    timeout(Duration::from_secs(5), s.next_video())
        .await
        .unwrap()
        .unwrap();
    s.send(ClientMsg::Key {
        code: 20,
        down: true,
    })
    .unwrap();
    s.send(ClientMsg::Key {
        code: 20,
        down: false,
    })
    .unwrap();
    wait_for(|| pc.input.log().contains(&"key 20 up".to_string())).await;
}

/// Owner's rule: a password is never typed unseen.
#[tokio::test(flavor = "multi_thread")]
async fn login_mode_without_a_picture_types_nothing() {
    let ph = phone().await;
    let pc = Pc::start_boot(None, true, &ph).await;
    let s = ph
        .connect(&pc.as_paired(), "phone", StreamParams::default())
        .await
        .unwrap();
    let mut saw_notice = false;
    loop {
        match timeout(Duration::from_secs(5), s.next_control())
            .await
            .unwrap()
        {
            Some(ServerMsg::Notice { message, .. }) => {
                saw_notice |= message.contains("won't type");
            }
            Some(ServerMsg::Welcome { .. }) => panic!("login mode went on without a picture"),
            Some(ServerMsg::Bye { .. }) | None => break,
            Some(_) => {}
        }
    }
    assert!(saw_notice);
    let _ = s.send(ClientMsg::Key {
        code: 20,
        down: true,
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(pc.input.log().is_empty(), "{:?}", pc.input.log());
}

/// The picture froze (one frame, then nothing): input stops a second later.
#[tokio::test(flavor = "multi_thread")]
async fn login_mode_drops_input_when_the_picture_freezes() {
    let ph = phone().await;
    let pc = Pc::start_boot(Some(0), false, &ph).await;
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    timeout(Duration::from_secs(5), s.next_video())
        .await
        .unwrap()
        .unwrap();
    s.send(ClientMsg::Key {
        code: 20,
        down: true,
    })
    .unwrap();
    s.send(ClientMsg::Key {
        code: 20,
        down: false,
    })
    .unwrap();
    wait_for(|| pc.input.log().contains(&"key 20 up".to_string())).await;
    // held down while the picture is live
    s.send(ClientMsg::Key {
        code: 40,
        down: true,
    })
    .unwrap();
    wait_for(|| pc.input.log().contains(&"key 40 down".to_string())).await;
    tokio::time::sleep(dishus_daemon::boot::VIDEO_FRESH + Duration::from_millis(500)).await;
    s.send(ClientMsg::Key {
        code: 30,
        down: true,
    })
    .unwrap();
    s.send(ClientMsg::MouseMove { dx: 5, dy: 5 }).unwrap();
    // a release still passes: dropping it would leave the key held (auto-repeating)
    s.send(ClientMsg::Key {
        code: 40,
        down: false,
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    let log = pc.input.log();
    assert!(log.contains(&"key 40 up".to_string()), "{log:?}");
    assert!(
        !log.iter()
            .any(|l| l.starts_with("key 30") || l.starts_with("move")),
        "{log:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn login_mode_refuses_away_from_the_login_screen() {
    let ph = phone().await;
    let pc = Pc::start_boot(None, false, &ph).await;
    pc.boot().set_seat(Seat::Other);
    assert_eq!(
        refusal(&ph, &pc.as_paired()).await,
        Some(dishus_core::Refusal::NotAtLoginScreen)
    );
    pc.boot().set_seat(Seat::Greeter {
        display: ":0".into(),
    });
    connect(&ph, &pc.as_paired())
        .await
        .expect("back at the login screen");
}

/// A login-mode session that types one key (after the picture), then hangs up.
async fn typed_session(ph: &Client, pc: &Pc) -> anyhow::Result<()> {
    let s = connect(ph, &pc.as_paired()).await?;
    timeout(Duration::from_secs(5), s.next_video())
        .await?
        .ok_or_else(|| anyhow::anyhow!("no picture"))?;
    let before = pc.input.log().len();
    for down in [true, false] {
        s.send(ClientMsg::Key { code: 20, down })?;
    }
    wait_for(|| pc.input.log().len() >= before + 2).await;
    drop(s);
    wait_for(|| pc.state.sessions().is_empty()).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn login_mode_limits_typed_sessions_per_hour() {
    let ph = phone().await;
    let pc = Pc::start_boot(None, false, &ph).await;
    // Sessions that type nothing (a link that drops before the password) cost nothing.
    for _ in 0..dishus_daemon::boot::PER_HOUR + 2 {
        let s = connect(&ph, &pc.as_paired())
            .await
            .expect("untyped sessions are free");
        drop(s);
        wait_for(|| pc.state.sessions().is_empty()).await;
    }
    for i in 0..dishus_daemon::boot::PER_HOUR {
        typed_session(&ph, &pc)
            .await
            .unwrap_or_else(|e| panic!("session {i}: {e}"));
    }
    // The phone is told why and until when.
    match refusal(&ph, &pc.as_paired()).await {
        Some(dishus_core::Refusal::LoginLimit { retry_secs }) => {
            assert!((3500..=3600).contains(&retry_secs), "{retry_secs}")
        }
        other => panic!("expected the hourly limit, got {other:?}"),
    }
    // Allowing the phone again (sudo dishus boot allow) clears it.
    pc.state
        .allow_device(&ph.endpoint_id(), "phone", 30)
        .unwrap();
    connect(&ph, &pc.as_paired()).await.expect("allowed again");
}

#[tokio::test(flavor = "multi_thread")]
async fn login_mode_blocks_after_five_typed_sessions_with_no_login() {
    let ph = phone().await;
    let rules = Rules {
        per_hour: 100, // only the strikes matter here
        ..Rules::default()
    };
    let pc = Pc::start_boot_with(None, false, &ph, rules).await;
    for i in 0..dishus_daemon::boot::STRIKES_PER_DAY {
        typed_session(&ph, &pc)
            .await
            .unwrap_or_else(|e| panic!("session {i}: {e}"));
    }
    assert_eq!(
        refusal(&ph, &pc.as_paired()).await,
        Some(dishus_core::Refusal::LoginBlocked)
    );
    pc.state
        .allow_device(&ph.endpoint_id(), "phone", 30)
        .unwrap();
    typed_session(&ph, &pc).await.expect("allowed again");
}

#[tokio::test(flavor = "multi_thread")]
async fn login_mode_a_login_clears_the_strike() {
    let ph = phone().await;
    let rules = Rules {
        per_hour: 100,
        ..Rules::default()
    };
    let pc = Pc::start_boot_with(None, false, &ph, rules).await;
    for _ in 0..dishus_daemon::boot::STRIKES_PER_DAY + 1 {
        typed_session(&ph, &pc).await.unwrap();
        pc.boot().logged_in(); // the password worked
    }
    typed_session(&ph, &pc).await.expect("not blocked");
}

#[tokio::test(flavor = "multi_thread")]
async fn login_mode_session_ends_at_the_cap() {
    let ph = phone().await;
    let rules = Rules {
        session_max: Duration::from_secs(2),
        ..Rules::default()
    };
    let pc = Pc::start_boot_with(None, false, &ph, rules).await;
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    let t0 = std::time::Instant::now();
    let mut told = false;
    let reason = loop {
        match timeout(Duration::from_secs(5), s.next_control())
            .await
            .unwrap()
        {
            Some(ServerMsg::Notice { message, .. }) => told |= message.contains("at most"),
            Some(ServerMsg::Bye { reason }) => break reason,
            Some(_) => {}
            None => panic!("closed without bye"),
        }
    };
    assert_eq!(reason, ByeReason::Timeout);
    assert!(told, "no notice about the cap");
    let took = t0.elapsed();
    assert!(
        took >= Duration::from_millis(1500) && took < Duration::from_secs(4),
        "{took:?}"
    );
    wait_for(|| pc.state.sessions().is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_is_login_mode_only_and_capped() {
    let ph = phone().await;
    let pc = Pc::start_boot(None, false, &ph).await;
    let d = pc.state.device(&ph.endpoint_id()).unwrap();
    let max = dishus_daemon::boot::MAX_ALLOW_DAYS as u64 * 86_400;
    assert!(
        d.expires_at <= d.paired_at + max,
        "asked 90 days, got more than 30"
    );
    let normal = Pc::start().await;
    assert!(
        normal
            .state
            .allow_device(&ph.endpoint_id(), "x", 30)
            .is_err()
    );
}

/// Only root (`sudo dishus boot allow`) extends a phone's login-mode allowance; a phone (say
/// a stolen one) can't renew itself from a session.
#[tokio::test(flavor = "multi_thread")]
async fn login_mode_phone_cannot_renew_itself() {
    let ph = phone().await;
    let pc = Pc::start_boot(None, false, &ph).await;
    let before = pc.state.device(&ph.endpoint_id()).unwrap().expires_at;
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    s.send(ClientMsg::RenewPairing { days: 90 }).unwrap();
    let refused = loop {
        match timeout(Duration::from_secs(5), s.next_control())
            .await
            .unwrap()
        {
            Some(ServerMsg::Renewed { .. }) => break false,
            Some(ServerMsg::Notice {
                kind: dishus_proto::NoticeKind::Refused,
                ..
            }) => {
                break true;
            }
            Some(_) => continue,
            None => panic!("closed"),
        }
    };
    assert!(refused, "renewed from a login-mode session");
    assert_eq!(
        pc.state.device(&ph.endpoint_id()).unwrap().expires_at,
        before
    );
    assert!(pc.state.renew(&ph.endpoint_id(), 30).is_err());
}

/// `dishus boot allow`: the phone pairs a new identity in a login-enroll window. The session
/// daemon only records it (for root to allow in login mode); it gets no access here.
#[tokio::test(flavor = "multi_thread")]
async fn login_enroll_records_the_identity_without_pairing_it() {
    let pc = Pc::start().await;
    let (tok, _) = pc.state.open_login_enroll().unwrap();
    let mut link = pc.pairing_link();
    pc.state.close_pairing(); // pairing_link() opened a normal window; use the enroll one
    let (tok2, _) = pc.state.open_login_enroll().unwrap();
    assert_ne!(*tok, *tok2);
    link.token = *tok2;
    let login_id = phone().await; // the phone's new, separate identity
    login_id.pair(&link, "phone login").await.expect("enrolled");
    assert_eq!(
        pc.state.take_enrolled(),
        Some((login_id.endpoint_id(), "phone login".to_string()))
    );
    assert!(pc.state.take_enrolled().is_none(), "taken once");
    assert!(
        pc.state.device(&login_id.endpoint_id()).is_none(),
        "not paired with the session"
    );
    assert!(connect(&login_id, &pc.as_paired()).await.is_err());
    assert!(pc.input.log().is_empty());
    // single use, like pairing
    let other = phone().await;
    assert!(other.pair(&link, "x").await.is_err());
    // and login mode itself never opens one
    let ph = phone().await;
    let boot = Pc::start_boot(None, false, &ph).await;
    assert!(boot.state.open_login_enroll().is_err());
}
