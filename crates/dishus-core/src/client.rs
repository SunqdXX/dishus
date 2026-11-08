//! Phone-side transport: pairing and remote-control sessions over iroh.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey, TransportAddr,
    endpoint::{Connection, presets},
};
use tokio::{sync::mpsc, task::JoinHandle, time::timeout};

use dishus_proto::{
    ALPN_PAIR, ALPN_SESSION, CAPTURE_OPEN_TIMEOUT_MS, ClientMsg, FrameError, HEARTBEAT_INTERVAL_MS,
    MAX_CONTROL_FRAME, MAX_VIDEO_FRAME, PROTO_VERSION, PairRequest, PairResponse, PairingInfo,
    SESSION_IDLE_TIMEOUT_MS, ServerMsg, StreamParams, VideoHeader, frame::Validate, read_frame,
    write_frame,
};

use crate::vault::PairedPc;

/// The PC refused this (paired) phone and said why. See [`dishus_proto::close_code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `dishus kill` is in effect on the PC.
    Lockdown,
    /// The pairing expired.
    Expired,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::Lockdown => {
                "the PC is locked down (dishus kill); run `dishus unlock` on the PC"
            }
            Refusal::Expired => {
                "the pairing has expired; run `dishus renew` on the PC or pair again"
            }
        })
    }
}
impl std::error::Error for Refusal {}

impl Refusal {
    fn from_conn_error(e: &iroh::endpoint::ConnectionError) -> Option<Self> {
        match e {
            iroh::endpoint::ConnectionError::ApplicationClosed(c) => {
                match u32::try_from(c.error_code.into_inner()).ok()? {
                    dishus_proto::close_code::LOCKDOWN => Some(Refusal::Lockdown),
                    dishus_proto::close_code::EXPIRED => Some(Refusal::Expired),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Look through an error chain (connect error, stream error, ...) for a refusal code.
    pub fn find(e: &anyhow::Error) -> Option<Self> {
        e.chain().find_map(|c| {
            if let Some(r) = c.downcast_ref::<Refusal>() {
                return Some(*r);
            }
            c.downcast_ref::<iroh::endpoint::ConnectionError>()
                .and_then(Self::from_conn_error)
        })
    }
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Default)]
pub struct NetConfig {
    /// Relay URLs; empty = iroh's public default relays.
    pub relays: Vec<String>,
    pub disable_relays: bool,
    /// Loopback only, no relays, no DNS lookup (tests).
    pub local_test: bool,
}

impl NetConfig {
    fn relay_mode(&self) -> Result<RelayMode> {
        if self.disable_relays || self.local_test {
            return Ok(RelayMode::Disabled);
        }
        if self.relays.is_empty() {
            return Ok(RelayMode::Default);
        }
        let urls = self
            .relays
            .iter()
            .map(|r| {
                r.parse::<RelayUrl>()
                    .with_context(|| format!("bad relay url {r}"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(RelayMode::custom(urls))
    }
}

pub struct Client {
    endpoint: Endpoint,
    /// When the app last reported a network change (see [`Client::network_change`]).
    net_changed: NetChanged,
}

type NetChanged = Arc<std::sync::Mutex<Option<Instant>>>;

fn pc_addr(endpoint_id: &[u8; 32], relays: &[String], addrs: &[String]) -> Result<EndpointAddr> {
    let id = EndpointId::from_bytes(endpoint_id).context("bad PC id")?;
    let mut parts: Vec<TransportAddr> = Vec::new();
    for a in addrs {
        if let Ok(sa) = a.parse::<SocketAddr>() {
            parts.push(TransportAddr::Ip(sa));
        }
    }
    for r in relays {
        if let Ok(u) = r.parse::<RelayUrl>() {
            parts.push(TransportAddr::Relay(u));
        }
    }
    Ok(EndpointAddr::from_parts(id, parts))
}

impl Client {
    pub async fn new(secret_key: &[u8; 32], net: &NetConfig) -> Result<Self> {
        let sk = SecretKey::from_bytes(secret_key);
        let mut b = if net.local_test {
            Endpoint::builder(presets::Minimal)
                .clear_ip_transports()
                .bind_addr(SocketAddr::from(([127, 0, 0, 1], 0)))?
        } else {
            Endpoint::builder(presets::N0)
        };
        b = b.secret_key(sk).relay_mode(net.relay_mode()?);
        Ok(Self {
            endpoint: b.bind().await?,
            net_changed: Arc::default(),
        })
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub async fn close(&self) {
        self.endpoint.close().await;
    }

    /// Redeem a pairing link. On success the PC has stored this phone's public key.
    pub async fn pair(&self, info: &PairingInfo, device_name: &str) -> Result<PairedPc> {
        let addr = pc_addr(&info.endpoint_id, &info.relays, &info.addrs)?;
        let conn = timeout(CONNECT_TIMEOUT, self.endpoint.connect(addr, ALPN_PAIR))
            .await
            .context("timed out reaching the PC (is pairing still open?)")?
            .context("could not reach the PC (pairing window closed or offline)")?;
        let (mut send, mut recv) = conn.open_bi().await?;
        let req = PairRequest {
            proto_version: PROTO_VERSION,
            token: info.token,
            device_name: device_name.to_owned(),
        };
        req.validate().map_err(|e| anyhow::anyhow!(e))?;
        write_frame(&mut send, &req, 512).await?;
        drop(req);
        send.finish()?;
        let resp: PairResponse = timeout(Duration::from_secs(15), read_frame(&mut recv, 512))
            .await
            .context("PC did not answer")?
            .context("pairing refused")?;
        conn.close(0u32.into(), b"");
        match resp {
            PairResponse::Accepted {
                pc_name,
                expires_at,
            } => Ok(PairedPc {
                endpoint_id: info.endpoint_id,
                name: pc_name,
                relays: info.relays.clone(),
                addrs: info.addrs.clone(),
                expires_at,
            }),
            PairResponse::Rejected => bail!("pairing rejected (wrong or expired code)"),
        }
    }

    /// Tell iroh the network may have changed (Android: iroh can't see it from native code).
    /// Re-probes paths at once so live connections migrate instead of stalling.
    /// Also lets live sessions give up quickly on a PC that went silent with the old network.
    pub async fn network_change(&self) {
        *self.net_changed.lock().expect("net_changed") = Some(Instant::now());
        self.endpoint.network_change().await;
    }

    pub async fn connect(
        &self,
        pc: &PairedPc,
        device_name: &str,
        params: StreamParams,
    ) -> Result<Session> {
        let addr = pc_addr(&pc.endpoint_id, &pc.relays, &pc.addrs)?;
        let res = async {
            let conn = timeout(CONNECT_TIMEOUT, self.endpoint.connect(addr, ALPN_SESSION))
                .await
                .context("timed out reaching the PC")?
                .context("could not connect (offline, revoked or expired)")?;
            // The PC may refuse right after the handshake: depending on timing that surfaces
            // here or as an error while opening the control stream.
            let closed = conn.clone();
            Session::start(conn, device_name, params, self.net_changed.clone())
                .await
                .map_err(|e| {
                    match closed
                        .close_reason()
                        .as_ref()
                        .and_then(Refusal::from_conn_error)
                    {
                        Some(r) => anyhow::Error::new(r),
                        None => e,
                    }
                })
        }
        .await;
        res.map_err(|e| match Refusal::find(&e) {
            Some(r) => anyhow::Error::new(r),
            None => e,
        })
    }
}

/// Video frame delivered in order, ready for the decoder.
#[derive(Debug, Clone)]
pub struct VideoPacket {
    pub header: VideoHeader,
    pub data: Bytes,
}

pub struct Session {
    conn: Connection,
    tx: mpsc::Sender<ClientMsg>,
    control_rx: tokio::sync::Mutex<mpsc::Receiver<ServerMsg>>,
    video_rx: tokio::sync::Mutex<mpsc::Receiver<VideoPacket>>,
    rtt_ms: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    /// Set when the watchdog gave up on a silent PC.
    timed_out: Arc<AtomicBool>,
    tasks: Vec<JoinHandle<()>>,
}

/// The PC answers every heartbeat; this long without any message from it means the session is
/// gone (the PC itself gives up on a silent phone after `SESSION_IDLE_TIMEOUT_MS`). Without this
/// the phone only noticed after QUIC's ~30 s idle timeout, e.g. after switching Wi-Fi off.
const PC_SILENT_MS: u64 = SESSION_IDLE_TIMEOUT_MS + 1000;
/// The PC's first message (Welcome) can take until screen capture is open.
const WELCOME_TIMEOUT_MS: u64 = CAPTURE_OPEN_TIMEOUT_MS + 5000;
/// Shortly after a network change, this much silence means the connection went down with the
/// old network: reconnecting at once beats waiting for the PC's idle timeout (measured on a
/// phone: Wi-Fi off cost ~6.4 s with the long wait).
const NET_CHANGE_SILENT_MS: u64 = 2 * HEARTBEAT_INTERVAL_MS + 500;
/// How long after a network change the short limit applies.
const NET_CHANGE_WINDOW: Duration = Duration::from_secs(20);

/// With video running, this long without a frame means it got stuck; the PC sends at least ~2 fps
/// even for a still screen (the capture re-sends the last image every 500 ms).
const VIDEO_STALL_MS: u64 = 1500;
/// How often to repeat the keyframe request while stuck.
const VIDEO_REREQUEST_MS: u64 = 1000;

/// Should the watchdog ask for a keyframe? `last_video_ms`: last frame delivered (None = video
/// never started, e.g. no capture: nothing to recover).
fn video_stalled(now_ms: u64, last_video_ms: Option<u64>, last_request_ms: u64) -> bool {
    last_video_ms.is_some_and(|t| {
        now_ms.saturating_sub(t) > VIDEO_STALL_MS
            && now_ms.saturating_sub(last_request_ms) >= VIDEO_REREQUEST_MS
    })
}

/// Watchdog decision. `last_rx_ms`: when the PC last sent anything (None = nothing yet).
fn pc_gone(now_ms: u64, last_rx_ms: Option<u64>, net_changed_recently: bool) -> bool {
    match last_rx_ms {
        None => now_ms > WELCOME_TIMEOUT_MS,
        Some(t) => {
            let silent = now_ms.saturating_sub(t);
            silent > PC_SILENT_MS || (net_changed_recently && silent > NET_CHANGE_SILENT_MS)
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
        self.conn.close(0u32.into(), b"");
    }
}

fn mono_ms(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

impl Session {
    async fn start(
        conn: Connection,
        device_name: &str,
        params: StreamParams,
        net_changed: NetChanged,
    ) -> Result<Self> {
        let (mut send, mut recv) = conn.open_bi().await?;
        let hello = ClientMsg::Hello {
            proto_version: PROTO_VERSION,
            device_name: device_name.to_owned(),
            stream: params,
        };
        hello.validate().map_err(|e| anyhow::anyhow!(e))?;
        write_frame(&mut send, &hello, MAX_CONTROL_FRAME).await?;

        let (tx, mut rx) = mpsc::channel::<ClientMsg>(256);
        let (ctl_tx, ctl_rx) = mpsc::channel::<ServerMsg>(64);
        let (vid_tx, vid_rx) = mpsc::channel::<VideoPacket>(8);
        let rtt_ms = Arc::new(AtomicU64::new(0));
        let closed = Arc::new(AtomicBool::new(false));
        let timed_out = Arc::new(AtomicBool::new(false));
        let last_rx = Arc::new(AtomicU64::new(0));
        // When the last video frame was delivered (ms since start + 1; 0 = none yet).
        let last_video = Arc::new(AtomicU64::new(0));
        let start = Instant::now();

        // Writer + heartbeat.
        let w_closed = closed.clone();
        let writer = tokio::spawn(async move {
            let mut hb = tokio::time::interval(Duration::from_millis(HEARTBEAT_INTERVAL_MS));
            loop {
                let msg = tokio::select! {
                    m = rx.recv() => match m { Some(m) => m, None => break },
                    _ = hb.tick() => ClientMsg::Ping { t: mono_ms(start) },
                };
                let bye = matches!(msg, ClientMsg::Bye);
                if write_frame(&mut send, &msg, MAX_CONTROL_FRAME)
                    .await
                    .is_err()
                {
                    break;
                }
                if bye {
                    let _ = send.finish();
                    break;
                }
            }
            w_closed.store(true, Ordering::SeqCst);
        });

        // Control reader.
        let r_rtt = rtt_ms.clone();
        let r_closed = closed.clone();
        let r_last = last_rx.clone();
        let reader = tokio::spawn(async move {
            loop {
                let frame = read_frame::<_, ServerMsg>(&mut recv, MAX_CONTROL_FRAME).await;
                r_last.store(mono_ms(start) + 1, Ordering::Relaxed); // 0 = nothing yet
                match frame {
                    Ok(ServerMsg::Pong { t }) => {
                        r_rtt.store(mono_ms(start).saturating_sub(t), Ordering::Relaxed);
                    }
                    Ok(m) => {
                        let bye = matches!(m, ServerMsg::Bye { .. });
                        if ctl_tx.send(m).await.is_err() || bye {
                            break;
                        }
                    }
                    Err(FrameError::Closed) => break,
                    Err(e) => {
                        tracing::warn!("control stream: {e}");
                        break;
                    }
                }
            }
            r_closed.store(true, Ordering::SeqCst);
        });

        // Watchdog: close a session whose PC went silent, so the app reconnects promptly.
        let wd_conn = conn.clone();
        let wd_closed = closed.clone();
        let wd_timed_out = timed_out.clone();
        let wd_tx = tx.clone();
        let wd_last_video = last_video.clone();
        let watchdog = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            let mut last_kf_req = 0u64;
            loop {
                tick.tick().await;
                let now = mono_ms(start);
                // Video stopped while the session lives (a lost keyframe request on the PC used to
                // freeze the picture for good): keep asking for a fresh IDR.
                if video_stalled(
                    now,
                    wd_last_video.load(Ordering::Relaxed).checked_sub(1),
                    last_kf_req,
                ) {
                    last_kf_req = now;
                    tracing::debug!("no video for {VIDEO_STALL_MS} ms; requesting a keyframe");
                    let _ = wd_tx.try_send(ClientMsg::RequestKeyframe);
                }
                let last = last_rx.load(Ordering::Relaxed);
                let recent = net_changed
                    .lock()
                    .expect("net_changed")
                    .is_some_and(|t| t.elapsed() < NET_CHANGE_WINDOW);
                if pc_gone(mono_ms(start), last.checked_sub(1), recent) {
                    tracing::warn!(
                        "PC not answering (network change: {recent}); closing the session"
                    );
                    wd_timed_out.store(true, Ordering::SeqCst);
                    wd_closed.store(true, Ordering::SeqCst);
                    wd_conn.close(0u32.into(), b"pc silent");
                    break;
                }
            }
        });

        // Video receiver: streams are accepted in stream-id order, which is frame order.
        let v_conn = conn.clone();
        let v_tx = tx.clone();
        let v_last = last_video.clone();
        let video = tokio::spawn(async move {
            let mut last_seq = 0u64;
            let mut need_key = true;
            let mut last_kf_req = Instant::now() - Duration::from_secs(10);
            let mut request_key = |need: &mut bool| {
                *need = true;
                if last_kf_req.elapsed() > Duration::from_millis(250) {
                    last_kf_req = Instant::now();
                    let _ = v_tx.try_send(ClientMsg::RequestKeyframe);
                }
            };
            while let Ok(mut s) = v_conn.accept_uni().await {
                let buf = match s.read_to_end(MAX_VIDEO_FRAME).await {
                    Ok(b) => b,
                    Err(_) => {
                        // reset by the PC (stalled) or oversize: next usable frame is an IDR
                        request_key(&mut need_key);
                        continue;
                    }
                };
                let Ok((h, payload)) = VideoHeader::decode(&buf) else {
                    request_key(&mut need_key);
                    continue;
                };
                let in_order = h.seq == last_seq + 1;
                if h.seq <= last_seq && last_seq != 0 && !h.is_keyframe() {
                    continue; // stale (new stream after restart starts at 1 with a keyframe)
                }
                last_seq = h.seq;
                if h.is_keyframe() {
                    need_key = false;
                } else if !in_order {
                    request_key(&mut need_key);
                }
                if need_key {
                    continue;
                }
                v_last.store(mono_ms(start) + 1, Ordering::Relaxed);
                let pkt = VideoPacket {
                    header: h,
                    data: Bytes::copy_from_slice(payload),
                };
                match vid_tx.try_send(pkt) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // decoder can't keep up: skip to the next keyframe
                        request_key(&mut need_key);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        });

        Ok(Self {
            conn,
            tx,
            control_rx: tokio::sync::Mutex::new(ctl_rx),
            video_rx: tokio::sync::Mutex::new(vid_rx),
            rtt_ms,
            closed,
            timed_out,
            tasks: vec![writer, reader, video, watchdog],
        })
    }

    /// Queue a message. Validated locally first so the PC never has to drop us for garbage.
    pub fn send(&self, msg: ClientMsg) -> Result<()> {
        msg.validate().map_err(|e| anyhow::anyhow!(e))?;
        self.tx
            .try_send(msg)
            .map_err(|_| anyhow::anyhow!("session closed or congested"))
    }

    /// Test hook: queue a message WITHOUT local validation, to check that the PC rejects it on
    /// its own. Never used by the app.
    #[doc(hidden)]
    pub fn send_unchecked(&self, msg: ClientMsg) -> Result<()> {
        self.tx
            .try_send(msg)
            .map_err(|_| anyhow::anyhow!("session closed or congested"))
    }

    pub async fn next_control(&self) -> Option<ServerMsg> {
        self.control_rx.lock().await.recv().await
    }

    pub async fn next_video(&self) -> Option<VideoPacket> {
        self.video_rx.lock().await.recv().await
    }

    /// Application-level round trip (includes PC processing), ms. 0 until the first Pong.
    pub fn rtt_ms(&self) -> u64 {
        self.rtt_ms.load(Ordering::Relaxed)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.conn.close_reason().is_some()
    }

    /// True if the session was closed because the PC stopped answering.
    pub fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }

    /// Set if the PC closed this connection with a refusal (e.g. lockdown right after connect).
    pub fn refusal(&self) -> Option<Refusal> {
        self.conn
            .close_reason()
            .as_ref()
            .and_then(Refusal::from_conn_error)
    }

    /// "direct" or "relay" plus QUIC path RTT.
    pub fn path(&self) -> String {
        let paths = self.conn.paths();
        match paths.iter().find(|p| p.is_selected()) {
            Some(p) if p.is_relay() => format!("relay · {} ms", p.rtt().as_millis()),
            Some(p) => format!("direct · {} ms", p.rtt().as_millis()),
            None => "connecting".into(),
        }
    }

    pub async fn close(&self) {
        let _ = self.tx.try_send(ClientMsg::Bye);
        let _ = timeout(Duration::from_millis(300), self.conn.closed()).await;
        self.conn.close(0u32.into(), b"");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watchdog_rules() {
        // Before the PC's first message it may still be opening screen capture.
        assert!(!pc_gone(WELCOME_TIMEOUT_MS - 1, None, false));
        assert!(!pc_gone(WELCOME_TIMEOUT_MS - 1, None, true));
        assert!(pc_gone(WELCOME_TIMEOUT_MS + 1, None, false));
        // Normal: give up only after PC_SILENT_MS of silence.
        assert!(!pc_gone(10_000 + PC_SILENT_MS, Some(10_000), false));
        assert!(pc_gone(10_001 + PC_SILENT_MS, Some(10_000), false));
        // Right after a network change, a short silence is enough: the old path is gone.
        assert!(!pc_gone(10_000 + NET_CHANGE_SILENT_MS, Some(10_000), true));
        assert!(pc_gone(10_001 + NET_CHANGE_SILENT_MS, Some(10_000), true));
        // ...but never shorter than a heartbeat round (a healthy session is never cut).
        const { assert!(NET_CHANGE_SILENT_MS > 2 * HEARTBEAT_INTERVAL_MS) };
    }

    #[test]
    fn video_stall_rules() {
        // no video yet (e.g. capture unavailable): never
        assert!(!video_stalled(60_000, None, 0));
        // flowing
        assert!(!video_stalled(10_000, Some(10_000 - VIDEO_STALL_MS), 0));
        // stuck: ask, then at most once per VIDEO_REREQUEST_MS
        assert!(video_stalled(10_000, Some(8_000), 0));
        assert!(!video_stalled(10_500, Some(8_000), 10_000));
        assert!(video_stalled(11_000, Some(8_000), 10_000));
    }
}
