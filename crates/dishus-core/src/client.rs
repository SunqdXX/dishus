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
    ALPN_PAIR, ALPN_SESSION, ClientMsg, FrameError, HEARTBEAT_INTERVAL_MS, MAX_CONTROL_FRAME,
    MAX_VIDEO_FRAME, PROTO_VERSION, PairRequest, PairResponse, PairingInfo, ServerMsg,
    StreamParams, VideoHeader, frame::Validate, read_frame, write_frame,
};

use crate::vault::PairedPc;

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
}

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

    pub async fn connect(
        &self,
        pc: &PairedPc,
        device_name: &str,
        params: StreamParams,
    ) -> Result<Session> {
        let addr = pc_addr(&pc.endpoint_id, &pc.relays, &pc.addrs)?;
        let conn = timeout(CONNECT_TIMEOUT, self.endpoint.connect(addr, ALPN_SESSION))
            .await
            .context("timed out reaching the PC")?
            .context("could not connect (offline, revoked or expired)")?;
        Session::start(conn, device_name, params).await
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
    tasks: Vec<JoinHandle<()>>,
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
    async fn start(conn: Connection, device_name: &str, params: StreamParams) -> Result<Self> {
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
        let reader = tokio::spawn(async move {
            loop {
                match read_frame::<_, ServerMsg>(&mut recv, MAX_CONTROL_FRAME).await {
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

        // Video receiver: streams are accepted in stream-id order, which is frame order.
        let v_conn = conn.clone();
        let v_tx = tx.clone();
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
            tasks: vec![writer, reader, video],
        })
    }

    /// Queue a message. Validated locally first so the PC never has to drop us for garbage.
    pub fn send(&self, msg: ClientMsg) -> Result<()> {
        msg.validate().map_err(|e| anyhow::anyhow!(e))?;
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
