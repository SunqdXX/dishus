//! End-to-end tests: real daemon network stack + real client core over loopback QUIC, with fake
//! input and capture backends. No relays, no DNS, no GPU, no uinput needed.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
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
    config::{Config, FilesConfig},
    devices::DeviceStore,
    net::{NetOptions, spawn_router},
    notify::Notifier,
    paths::Paths,
    state::AppState,
};
use dishus_proto::{
    ALPN_SESSION, ALPN_SESSION_V2, ALPN_SESSION_V3, ByeReason, ClientMsg, MonitorInfo, MouseButton,
    PROTO_VERSION, PairingInfo, ServerMsg, StreamParams, read_frame, write_frame,
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
/// forced (models the lost-keyframe-request freeze).
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
/// Audio: counts how often it was opened; `slow_first_audio` makes the first audio capture
/// deliver 30 packets a second (still 20 ms each, timestamps without gaps), like the slow
/// PipeWire capture seen on the owner's PC.
struct FakeCapture {
    open_delay: Duration,
    stall_after: Option<u64>,
    slow_first_audio: bool,
    audio_opens: Arc<AtomicU32>,
}
impl CaptureProvider for FakeCapture {
    fn open(&self) -> BoxFuture<anyhow::Result<Box<dyn CaptureSession>>> {
        let (d, stall_after) = (self.open_delay, self.stall_after);
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
        let n = self.audio_opens.fetch_add(1, Ordering::SeqCst);
        let every = if self.slow_first_audio && n == 0 {
            33
        } else {
            20
        };
        Box::pin(async move {
            struct NoCtl;
            impl AudioControl for NoCtl {
                fn set_bitrate(&self, _: u32) {}
            }
            let (tx, rx) = mpsc::channel(16);
            let task = tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_millis(every));
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

// ---------- harness ----------

struct Pc {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
    router: Router,
    input: RecInput,
    audio_opens: Arc<AtomicU32>,
}

impl Pc {
    async fn start() -> Self {
        Self::start_with(Duration::ZERO).await
    }

    async fn start_with(capture_open: Duration) -> Self {
        Self::start_full(capture_open, None).await
    }

    async fn start_full(capture_open: Duration, stall_after: Option<u64>) -> Self {
        Self::start_inner(capture_open, stall_after, false, FilesConfig::default()).await
    }

    async fn start_slow_audio() -> Self {
        Self::start_inner(Duration::ZERO, None, true, FilesConfig::default()).await
    }

    /// File send turned on, files up to `max_file_mb`.
    async fn start_files(max_file_mb: u64) -> Self {
        let files = FilesConfig {
            enabled: true,
            max_file_mb,
        };
        Self::start_inner(Duration::ZERO, None, false, files).await
    }

    async fn start_inner(
        capture_open: Duration,
        stall_after: Option<u64>,
        slow_first_audio: bool,
        files: FilesConfig,
    ) -> Self {
        let audio_opens = Arc::new(AtomicU32::new(0));
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let cfg = Config {
            pc_name: "test-pc".into(),
            files,
            ..Default::default()
        };
        let input = RecInput::default();
        let state = Arc::new(AppState::new(
            paths,
            cfg,
            DeviceStore::default(),
            TrackedInput::new(Box::new(input.clone())),
            Arc::new(FakeCapture {
                open_delay: capture_open,
                stall_after,
                slow_first_audio,
                audio_opens: audio_opens.clone(),
            }),
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
            audio_opens,
        }
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

#[tokio::test]
async fn a_capture_that_stays_slow_is_restarted_and_numbering_continues() {
    let pc = Pc::start_slow_audio().await;
    let (conn, mut send, _recv, _ep) = raw_session(&pc, ALPN_SESSION_V2).await;
    write_frame(&mut send, &ClientMsg::SetAudio { on: true }, 4096)
        .await
        .unwrap();
    // A phone pings; without it the PC ends a silent session long before this test is done.
    let pinger = tokio::spawn(async move {
        for t in 0u64.. {
            if write_frame(&mut send, &ClientMsg::Ping { t }, 4096)
                .await
                .is_err()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    // Slow (30/s) for SLOW_SECS seconds, then a new capture at 50/s.
    let start = tokio::time::Instant::now();
    let mut seqs = Vec::new();
    while pc.audio_opens.load(Ordering::SeqCst) < 2 {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "capture never restarted"
        );
        if let Ok(Ok(d)) = timeout(Duration::from_millis(200), conn.read_datagram()).await {
            seqs.push(dishus_proto::AudioHeader::decode(&d).unwrap().0.seq);
        }
    }
    let restarted_after = start.elapsed();
    assert!(
        restarted_after >= Duration::from_secs(3),
        "{restarted_after:?}"
    );
    // After the restart: 50 a second, numbers carrying on without a reset.
    let t = tokio::time::Instant::now();
    let mut after = 0;
    while t.elapsed() < Duration::from_secs(2) {
        let d = timeout(Duration::from_secs(1), conn.read_datagram())
            .await
            .unwrap()
            .unwrap();
        seqs.push(dishus_proto::AudioHeader::decode(&d).unwrap().0.seq);
        after += 1;
    }
    assert!(
        (90..=110).contains(&after),
        "{after} packets in 2 s after the restart"
    );
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
    assert_eq!(
        pc.audio_opens.load(Ordering::SeqCst),
        2,
        "restarted only once"
    );
    pinger.abort();
}

// ---------- file send (session/3) ----------

use dishus_proto::files::{
    CANCELLED, FileAnswer, FileOffer, FileRefusal, FileResult, MAX_FILE_FRAME,
};

/// Everything under `dir`, as sorted relative paths.
fn tree(dir: &std::path::Path) -> Vec<String> {
    fn walk(base: &std::path::Path, d: &std::path::Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd {
            let p = e.unwrap().path();
            out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            if p.is_dir() {
                walk(base, &p, out);
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, dir, &mut v);
    v.sort();
    v
}

impl Pc {
    fn files_dir(&self) -> std::path::PathBuf {
        self.state.paths.files_dir.clone()
    }
    fn received(&self) -> Vec<String> {
        tree(&self.files_dir())
    }
}

/// session/3 straight over QUIC, without the phone core, so the PC's own checks are what's
/// tested. Returns the connection and FilesInfo.
async fn raw_v3(
    pc: &Pc,
) -> (
    iroh::endpoint::Connection,
    ServerMsg,
    iroh::Endpoint,
    tokio::task::JoinHandle<()>,
) {
    let (conn, mut send, mut recv, ep) = raw_session(pc, ALPN_SESSION_V3).await;
    assert_eq!(conn.alpn(), ALPN_SESSION_V3);
    let info: ServerMsg = timeout(Duration::from_secs(5), read_frame(&mut recv, 65536))
        .await
        .unwrap()
        .unwrap();
    // keep the session alive while files go back and forth
    let pinger = tokio::spawn(async move {
        let _recv = recv;
        for t in 0u64.. {
            if write_frame(&mut send, &ClientMsg::Ping { t }, 4096)
                .await
                .is_err()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    (conn, info, ep, pinger)
}

/// Offer a file; if accepted send `data` (which may differ from `size`) and finish.
async fn raw_send(
    conn: &iroh::endpoint::Connection,
    name: &str,
    size: u64,
    data: &[u8],
) -> (FileAnswer, Option<FileResult>) {
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let offer = FileOffer {
        name: name.into(),
        size,
    };
    write_frame(&mut send, &offer, MAX_FILE_FRAME)
        .await
        .unwrap();
    let answer: FileAnswer = timeout(
        Duration::from_secs(5),
        read_frame(&mut recv, MAX_FILE_FRAME),
    )
    .await
    .expect("no answer")
    .unwrap();
    if answer != FileAnswer::Accept {
        return (answer, None);
    }
    // the PC may stop reading early (too much data); that's fine here
    let _ = send.write_all(data).await;
    let _ = send.finish();
    let result = timeout(
        Duration::from_secs(5),
        read_frame::<_, FileResult>(&mut recv, MAX_FILE_FRAME),
    )
    .await
    .expect("no result")
    .ok();
    (answer, result)
}

fn saved(r: Option<FileResult>) -> String {
    match r {
        Some(FileResult::Saved { name }) => name,
        other => panic!("not saved: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn files_are_refused_while_turned_off() {
    let pc = Pc::start().await;
    let (conn, info, _ep, _p) = raw_v3(&pc).await;
    assert!(
        matches!(info, ServerMsg::FilesInfo { enabled: false, .. }),
        "{info:?}"
    );
    let (answer, _) = raw_send(&conn, "a.txt", 3, b"abc").await;
    assert_eq!(answer, FileAnswer::Refuse(FileRefusal::Disabled));
    assert!(
        !pc.files_dir().exists(),
        "nothing is even created while off"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn received_files_keep_their_bytes_and_never_overwrite() {
    let pc = Pc::start_files(1).await;
    let (conn, info, _ep, _p) = raw_v3(&pc).await;
    assert_eq!(
        info,
        ServerMsg::FilesInfo {
            enabled: true,
            max_bytes: 1 << 20
        }
    );
    let big: Vec<u8> = (0..700_000u32).map(|i| (i * 7 % 251) as u8).collect();
    assert_eq!(
        saved(raw_send(&conn, "a.bin", big.len() as u64, &big).await.1),
        "a.bin"
    );
    assert_eq!(
        saved(raw_send(&conn, "a.bin", 3, b"two").await.1),
        "a (1).bin"
    );
    std::fs::write(pc.files_dir().join("b.txt"), b"the user's own").unwrap();
    assert_eq!(
        saved(raw_send(&conn, "b.txt", 4, b"mine").await.1),
        "b (1).txt"
    );
    assert_eq!(std::fs::read(pc.files_dir().join("a.bin")).unwrap(), big);
    assert_eq!(
        std::fs::read(pc.files_dir().join("a (1).bin")).unwrap(),
        b"two"
    );
    assert_eq!(
        std::fs::read(pc.files_dir().join("b.txt")).unwrap(),
        b"the user's own"
    );
    assert_eq!(
        std::fs::read(pc.files_dir().join("b (1).txt")).unwrap(),
        b"mine"
    );
    assert_eq!(pc.received(), ["a (1).bin", "a.bin", "b (1).txt", "b.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn hostile_names_are_cleaned_and_stay_in_the_folder() {
    let pc = Pc::start_files(1).await;
    let root = pc._dir.path().to_path_buf();
    let (conn, _, _ep, _p) = raw_v3(&pc).await;
    let before = tree(&root);
    let long = format!("{}.txt", "ş".repeat(150)); // 300 bytes + ".txt"
    let cases = [
        ("../../x", "x"),
        ("..\\..\\y", "y"),
        ("/etc/cron.d/evil", "evil"),
        ("CON.txt", "_CON.txt"),
        ("nul", "_nul"),
        ("a:b", "ab"),
        (".bashrc", "bashrc"),
        ("evil\u{202E}gpj.exe", "evilgpj.exe"),
        ("..", "file"),
    ];
    for (offered, want) in cases {
        let got = saved(raw_send(&conn, offered, 1, b"z").await.1);
        assert_eq!(got, want, "{offered:?}");
    }
    let got = saved(raw_send(&conn, &long, 1, b"z").await.1);
    assert!(
        got.len() <= 200 && got.ends_with(".txt") && got.starts_with('ş'),
        "{got}"
    );
    // nothing appeared anywhere but the DiShus folder
    let outside: Vec<String> = tree(&root)
        .into_iter()
        .filter(|p| !before.contains(p) && !p.starts_with("downloads"))
        .collect();
    assert!(outside.is_empty(), "{outside:?}");
    assert_eq!(pc.received().len(), cases.len() + 1, "{:?}", pc.received());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_halfway_leaves_nothing_and_the_session_goes_on() {
    let pc = Pc::start_files(1).await;
    let (conn, _, _ep, _p) = raw_v3(&pc).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let offer = FileOffer {
        name: "half.bin".into(),
        size: 1 << 20,
    };
    write_frame(&mut send, &offer, MAX_FILE_FRAME)
        .await
        .unwrap();
    let answer: FileAnswer = read_frame(&mut recv, MAX_FILE_FRAME).await.unwrap();
    assert_eq!(answer, FileAnswer::Accept);
    send.write_all(&[1u8; 300_000]).await.unwrap();
    wait_for(|| pc.received().iter().any(|n| n.ends_with(".part"))).await;
    send.reset(CANCELLED.into()).unwrap();
    wait_for(|| pc.received().is_empty()).await;
    // the session is fine: the next file goes through
    assert_eq!(
        saved(raw_send(&conn, "next.txt", 2, b"ok").await.1),
        "next.txt"
    );
    assert_eq!(pc.received(), ["next.txt"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_sizes_and_limits_save_nothing() {
    let pc = Pc::start_files(1).await;
    let (conn, _, _ep, _p) = raw_v3(&pc).await;
    let (a, _) = raw_send(&conn, "big.bin", (1 << 20) + 1, b"").await;
    assert_eq!(
        a,
        FileAnswer::Refuse(FileRefusal::TooBig { max_bytes: 1 << 20 })
    );
    // fewer bytes than announced, then the end
    let (a, r) = raw_send(&conn, "short.bin", 10, b"12345").await;
    assert_eq!((a, r), (FileAnswer::Accept, Some(FileResult::Failed)));
    // more bytes than announced
    let (a, r) = raw_send(&conn, "long.bin", 5, b"1234567890").await;
    assert_eq!(a, FileAnswer::Accept);
    assert_ne!(
        r,
        Some(FileResult::Saved {
            name: "long.bin".into()
        })
    );
    wait_for(|| pc.received().is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_file_at_a_time() {
    let pc = Pc::start_files(1).await;
    let (conn, _, _ep, _p) = raw_v3(&pc).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let offer = FileOffer {
        name: "first.bin".into(),
        size: 100,
    };
    write_frame(&mut send, &offer, MAX_FILE_FRAME)
        .await
        .unwrap();
    let a: FileAnswer = read_frame(&mut recv, MAX_FILE_FRAME).await.unwrap();
    assert_eq!(a, FileAnswer::Accept);
    let (second, _) = raw_send(&conn, "second.bin", 1, b"x").await;
    assert_eq!(second, FileAnswer::Refuse(FileRefusal::Busy));
    send.write_all(&[0; 100]).await.unwrap();
    send.finish().unwrap();
    let r: FileResult = read_frame(&mut recv, MAX_FILE_FRAME).await.unwrap();
    assert_eq!(
        r,
        FileResult::Saved {
            name: "first.bin".into()
        }
    );
    // free again
    assert_eq!(
        saved(raw_send(&conn, "third.bin", 1, b"x").await.1),
        "third.bin"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_the_phone_mid_transfer_deletes_the_partial_file() {
    let pc = Pc::start_files(1).await;
    let (conn, _, _ep, _p) = raw_v3(&pc).await;
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let offer = FileOffer {
        name: "half.bin".into(),
        size: 1 << 20,
    };
    write_frame(&mut send, &offer, MAX_FILE_FRAME)
        .await
        .unwrap();
    let _: FileAnswer = read_frame(&mut recv, MAX_FILE_FRAME).await.unwrap();
    send.write_all(&[1u8; 200_000]).await.unwrap();
    wait_for(|| pc.received().iter().any(|n| n.ends_with(".part"))).await;
    pc.state.revoke("raw").unwrap();
    timeout(Duration::from_secs(5), conn.closed())
        .await
        .expect("session cut");
    wait_for(|| pc.received().is_empty()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn older_sessions_get_no_file_messages() {
    let pc = Pc::start_files(1).await;
    // session/2: no FilesInfo before the answer to our ping, and no file streams
    let (conn, mut send, mut recv, _ep) = raw_session(&pc, ALPN_SESSION_V2).await;
    write_frame(&mut send, &ClientMsg::Ping { t: 7 }, 4096)
        .await
        .unwrap();
    loop {
        let m: ServerMsg = timeout(Duration::from_secs(5), read_frame(&mut recv, 65536))
            .await
            .unwrap()
            .unwrap();
        assert!(
            !matches!(m, ServerMsg::FilesInfo { .. }),
            "FilesInfo on session/2"
        );
        if m == (ServerMsg::Pong { t: 7 }) {
            break;
        }
    }
    let (mut fs, mut fr) = conn.open_bi().await.unwrap();
    let offer = FileOffer {
        name: "a".into(),
        size: 1,
    };
    write_frame(&mut fs, &offer, MAX_FILE_FRAME).await.unwrap();
    assert!(
        timeout(
            Duration::from_millis(500),
            read_frame::<_, FileAnswer>(&mut fr, MAX_FILE_FRAME)
        )
        .await
        .is_err(),
        "a session/2 PC must not answer file offers"
    );
    assert!(pc.received().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn sound_still_works_on_session3() {
    let pc = Pc::start().await;
    let (conn, mut send, mut recv, _ep) = raw_session(&pc, ALPN_SESSION_V3).await;
    write_frame(&mut send, &ClientMsg::SetAudio { on: true }, 4096)
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            let m: ServerMsg = read_frame(&mut recv, 65536).await.unwrap();
            if matches!(m, ServerMsg::AudioStarted { .. }) {
                break;
            }
        }
    })
    .await
    .expect("AudioStarted on session/3");
    timeout(Duration::from_secs(2), conn.read_datagram())
        .await
        .unwrap()
        .unwrap();
}

// ---------- file send through the phone core (what the app uses) ----------

#[tokio::test(flavor = "multi_thread")]
async fn phone_core_sends_files_and_cancels() {
    let pc = Pc::start_files(8).await;
    let ph = phone().await;
    let paired = ph.pair(&pc.pairing_link(), "Pixel").await.unwrap();
    let s = connect(&ph, &paired).await.unwrap();
    assert!(s.audio_supported(), "session/3 keeps sound");
    wait_for(|| s.files().is_some()).await;
    assert_eq!(s.files(), Some((true, 8 << 20)));

    let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
    let mut up = s
        .send_file("../photo.jpg", data.len() as u64)
        .await
        .unwrap();
    for c in data.chunks(64 * 1024) {
        up.write(c).await.unwrap();
    }
    assert_eq!(up.sent(), data.len() as u64);
    assert_eq!(up.finish().await.unwrap(), "photo.jpg");
    assert_eq!(
        std::fs::read(pc.files_dir().join("photo.jpg")).unwrap(),
        data
    );

    // more than announced is caught on the phone already
    let mut up = s.send_file("x.bin", 2).await.unwrap();
    assert!(up.write(b"abc").await.is_err());
    up.cancel();

    // cancel halfway
    let mut up = s.send_file("half.bin", data.len() as u64).await.unwrap();
    up.write(&data[..1_000_000]).await.unwrap();
    up.cancel();
    wait_for(|| pc.received() == ["photo.jpg"]).await;

    // input still flows during and after
    s.send(ClientMsg::MouseMove { dx: 1, dy: 1 }).unwrap();
    wait_for(|| pc.input.log().iter().any(|l| l == "move 1 1")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn phone_core_reports_file_send_off() {
    let pc = Pc::start().await;
    let ph = phone().await;
    let paired = ph.pair(&pc.pairing_link(), "Pixel").await.unwrap();
    let s = connect(&ph, &paired).await.unwrap();
    wait_for(|| s.files().is_some()).await;
    assert_eq!(s.files().map(|f| f.0), Some(false));
    let e = s.send_file("a.txt", 1).await.err().expect("refused");
    assert_eq!(e.to_string(), "file sending is off on this PC");
    assert!(!pc.files_dir().exists());
}
