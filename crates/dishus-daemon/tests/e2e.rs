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
        BoxFut, BoxFuture, CaptureProvider, CaptureSession, EncodedStream, InputSink, StreamConfig,
        StreamControl, TrackedInput, VideoFrame,
    },
    config::Config,
    devices::DeviceStore,
    net::{NetOptions, spawn_router},
    notify::Notifier,
    paths::Paths,
    state::AppState,
};
use dishus_proto::{
    ALPN_SESSION, ByeReason, ClientMsg, MonitorInfo, MouseButton, PROTO_VERSION, PairingInfo,
    ServerMsg, StreamParams, write_frame,
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
        self.0.lock().unwrap().push(format!("key {c} {}", if d { "down" } else { "up" }));
        Ok(())
    }
    fn button(&mut self, b: MouseButton, d: bool) -> std::io::Result<()> {
        self.0.lock().unwrap().push(format!("btn {b:?} {}", if d { "down" } else { "up" }));
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

struct FakeCaptureSession;
impl CaptureSession for FakeCaptureSession {
    fn monitors(&self) -> Vec<MonitorInfo> {
        vec![MonitorInfo { index: 0, name: "FAKE-1".into(), width: 640, height: 360 }]
    }
    fn start_stream(&mut self, _cfg: StreamConfig) -> BoxFut<'_, anyhow::Result<EncodedStream>> {
        Box::pin(async { fake_stream() })
    }
}

fn fake_stream() -> anyhow::Result<EncodedStream> {
    {
        let (tx, rx) = mpsc::channel(4);
        let ctl = Arc::new(FakeCtl { key: AtomicBool::new(true) });
        let c2 = ctl.clone();
        let task = tokio::spawn(async move {
            let mut pts = 0u64;
            loop {
                tokio::time::sleep(Duration::from_millis(15)).await;
                let key = c2.key.swap(false, Ordering::SeqCst);
                pts += 15_000;
                let nal: &[u8] = if key { &[0, 0, 0, 1, 0x65, 1, 2, 3] } else { &[0, 0, 0, 1, 0x41, 4, 5] };
                let f = VideoFrame { data: Bytes::from_static(nal), pts_us: pts, keyframe: key };
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

struct FakeCapture;
impl CaptureProvider for FakeCapture {
    fn open(&self) -> BoxFuture<anyhow::Result<Box<dyn CaptureSession>>> {
        Box::pin(async { Ok(Box::new(FakeCaptureSession) as Box<dyn CaptureSession>) })
    }
    fn setup_interactive(&self) -> BoxFuture<anyhow::Result<String>> {
        Box::pin(async { Ok("fake".into()) })
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
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paths.ensure().unwrap();
        let mut cfg = Config::default();
        cfg.pc_name = "test-pc".into();
        let input = RecInput::default();
        let state = Arc::new(AppState::new(
            paths,
            cfg,
            DeviceStore::default(),
            TrackedInput::new(Box::new(input.clone())),
            Arc::new(FakeCapture),
            Notifier::disabled(),
        ));
        let router = spawn_router(state.clone(), SecretKey::generate(), &NetOptions::local_test())
            .await
            .unwrap();
        Pc { _dir: dir, state, router, input }
    }

    fn pairing_link(&self) -> PairingInfo {
        let (tok, _) = self.state.open_pairing().unwrap();
        PairingInfo {
            endpoint_id: *self.router.endpoint().id().as_bytes(),
            token: *tok,
            pc_name: "test-pc".into(),
            relays: vec![],
            addrs: self.router.endpoint().bound_sockets().iter().map(|a| a.to_string()).collect(),
        }
    }

    fn as_paired(&self) -> PairedPc {
        PairedPc {
            endpoint_id: *self.router.endpoint().id().as_bytes(),
            name: "test-pc".into(),
            relays: vec![],
            addrs: self.router.endpoint().bound_sockets().iter().map(|a| a.to_string()).collect(),
            expires_at: 0,
        }
    }
}

async fn phone() -> Client {
    let id = vault::new_identity().unwrap();
    Client::new(&id.secret_key, &NetConfig { local_test: true, ..Default::default() }).await.unwrap()
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
        match timeout(Duration::from_secs(5), s.next_control()).await.expect("no bye in time") {
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
    let v = timeout(Duration::from_secs(5), s.next_video()).await.unwrap().unwrap();
    assert!(v.header.is_keyframe());
    let v2 = timeout(Duration::from_secs(5), s.next_video()).await.unwrap().unwrap();
    assert!(v2.header.seq > v.header.seq);

    // Super held down, plus a click held, then the phone vanishes
    s.send(ClientMsg::Key { code: 125, down: true }).unwrap();
    s.send(ClientMsg::MouseButton { button: MouseButton::Left, down: true }).unwrap();
    s.send(ClientMsg::MouseMove { dx: 3, dy: -2 }).unwrap();
    wait_for(|| pc.input.log().iter().any(|l| l == "move 3 -2")).await;
    drop(s);
    wait_for(|| pc.input.log().contains(&"key 125 up".to_string())).await;
    let log = pc.input.log();
    assert!(log.contains(&"btn Left up".to_string()), "{log:?}");
    assert!(pc.state.sessions().is_empty());
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
    a.pair(&link, "a").await.expect("right token still works after one miss");
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
    assert!(connect(&ph, &pc.as_paired()).await.is_err(), "expired");
    pc.state.renew(&ph.endpoint_id(), 30).unwrap();
    connect(&ph, &pc.as_paired()).await.expect("renewed");
}

#[tokio::test(flavor = "multi_thread")]
async fn revoke_kicks_live_session_and_blocks_reconnect() {
    let pc = Pc::start().await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "victim").await.unwrap();
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    s.send(ClientMsg::Key { code: 29, down: true }).unwrap();
    wait_for(|| pc.input.log().contains(&"key 29 down".to_string())).await;
    pc.state.revoke("victim").unwrap();
    assert_eq!(expect_bye(&s).await, ByeReason::Revoked);
    wait_for(|| pc.input.log().contains(&"key 29 up".to_string())).await;
    assert!(connect(&ph, &pc.as_paired()).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn renew_from_inside_session() {
    let pc = Pc::start().await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "p").await.unwrap();
    let before = pc.state.device(&ph.endpoint_id()).unwrap().expires_at;
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    s.send(ClientMsg::RenewPairing { days: 60 }).unwrap();
    let exp = loop {
        match timeout(Duration::from_secs(5), s.next_control()).await.unwrap().unwrap() {
            ServerMsg::Renewed { expires_at } => break expires_at,
            _ => continue,
        }
    };
    assert!(exp > before);
    assert_eq!(pc.state.device(&ph.endpoint_id()).unwrap().expires_at, exp);
    assert!(s.send(ClientMsg::RenewPairing { days: 61 }).is_err(), "validated client side too");
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_cuts_and_locks_until_unlock() {
    let pc = Pc::start().await;
    let ph = phone().await;
    ph.pair(&pc.pairing_link(), "p").await.unwrap();
    let s = connect(&ph, &pc.as_paired()).await.unwrap();
    assert_eq!(pc.state.kill().unwrap(), 1);
    assert_eq!(expect_bye(&s).await, ByeReason::Killed);
    assert!(connect(&ph, &pc.as_paired()).await.is_err(), "locked down");
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
        d.add(&raw.id(), "raw", 1, dishus_daemon::devices::now_unix()).unwrap();
    }
    let addr = iroh::EndpointAddr::from_parts(
        pc.router.endpoint().id(),
        pc.router.endpoint().bound_sockets().into_iter().map(iroh::TransportAddr::Ip),
    );
    let conn = raw.connect(addr, ALPN_SESSION).await.unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    let hello = ClientMsg::Hello {
        proto_version: PROTO_VERSION,
        device_name: "raw".into(),
        stream: StreamParams::default(),
    };
    write_frame(&mut send, &hello, 4096).await.unwrap();
    write_frame(&mut send, &ClientMsg::Key { code: 56, down: true }, 4096).await.unwrap();
    wait_for(|| pc.input.log().contains(&"key 56 down".to_string())).await;
    // now silence; the daemon must release within SESSION_IDLE_TIMEOUT (5 s) + slack
    timeout(Duration::from_secs(9), async {
        while !pc.input.log().contains(&"key 56 up".to_string()) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("stuck key was not released after heartbeat timeout");
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
        d.add(&raw.id(), "raw", 1, dishus_daemon::devices::now_unix()).unwrap();
    }
    let addr = iroh::EndpointAddr::from_parts(
        pc.router.endpoint().id(),
        pc.router.endpoint().bound_sockets().into_iter().map(iroh::TransportAddr::Ip),
    );
    let conn = raw.connect(addr, ALPN_SESSION).await.unwrap();
    let (mut send, _recv) = conn.open_bi().await.unwrap();
    let hello = ClientMsg::Hello { proto_version: PROTO_VERSION, device_name: "raw".into(), stream: StreamParams::default() };
    write_frame(&mut send, &hello, 4096).await.unwrap();
    // KEY_POWER is refused by validation -> protocol error -> session ends, nothing injected
    write_frame(&mut send, &ClientMsg::Key { code: 116, down: true }, 4096).await.unwrap();
    // oversize length prefix
    let _ = send.write_all(&u32::MAX.to_be_bytes()).await;
    timeout(Duration::from_secs(5), conn.closed()).await.expect("daemon should close the connection");
    assert!(!pc.input.log().iter().any(|l| l.starts_with("key 116")));
}
