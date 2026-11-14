//! One authenticated remote-control session (ALPN `dishus/session/1`).
//!
//! The gate has already verified the peer against the allowlist; we check again here (the
//! device could have been revoked between handshake and now) and on a timer while connected.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use iroh::endpoint::{Connection, SendStream};
use tokio::{sync::mpsc, time::timeout};

use dishus_proto::{
    ALPN_SESSION_V2, ByeReason, CAPTURE_OPEN_TIMEOUT_MS, ClientMsg, FrameError,
    HEARTBEAT_INTERVAL_MS, MAX_CONTROL_FRAME, MonitorInfo, NoticeKind, PROTO_VERSION,
    SESSION_IDLE_TIMEOUT_MS, ServerMsg, StreamParams, VideoHeader,
    audio::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, AudioHeader},
    read_frame,
    video::FLAG_KEYFRAME,
    write_frame,
};

use crate::{
    abr::{self, Abr, TickStats},
    audio::{self as audio_policy, AudioPolicy, Dropped, Pressure},
    backend::{CaptureSession, EncodedStream, StreamConfig, StreamControl, VideoFrame},
    state::AppState,
};

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const CAPTURE_OPEN_TIMEOUT: Duration = Duration::from_millis(CAPTURE_OPEN_TIMEOUT_MS);
const EXPIRY_CHECK: Duration = Duration::from_secs(15);
/// Release held input after this long without any message (heartbeats come every second).
const RELEASE_ON_SILENCE: Duration = Duration::from_millis(2 * HEARTBEAT_INTERVAL_MS + 500);
const SILENCE_CHECK: Duration = Duration::from_millis(250);
/// While waiting for an IDR, ask the encoder again this often: one request can get lost (e.g.
/// in the encoder reconfigure a bitrate change causes), and then the picture froze for good.
const KEY_REREQUEST: Duration = Duration::from_millis(250);

/// Frames allowed in flight (sent, not yet fully acknowledged by the phone) before we drop.
/// A frame is in flight for about one round trip, so the limit must grow with the RTT: a fixed 4
/// was fine on a LAN but on mobile data (60 ms RTT, 40 fps) that is the *normal* count, and every
/// session collapsed to the minimum bitrate and dropped frames.
pub fn inflight_limit(rtt: Duration, fps: u8) -> usize {
    let budget_ms = rtt.as_millis() as u64 + 100;
    let per_rtt = (budget_ms * u64::from(fps.max(1))).div_ceil(1000) as usize;
    (per_rtt + 2).clamp(4, 32)
}

/// Should this frame be dropped to keep latency bounded? While a keyframe is still in flight
/// the frames queued behind it may use twice the limit: dropping them only forces another
/// (large) keyframe, which queues the next frames again, a keyframe storm on slow links.
fn over_limit(inflight: usize, limit: usize, key_in_flight: bool) -> bool {
    inflight >= if key_in_flight { limit * 2 } else { limit }
}

/// Gate for "the next frame sent must be an IDR". Drops other frames meanwhile and keeps asking
/// the encoder until a keyframe actually shows up.
#[derive(Debug)]
struct KeyGate {
    waiting: bool,
    last_request: Option<Instant>,
}

impl KeyGate {
    /// A fresh stream starts with an IDR (the caller has just asked for one).
    fn new(now: Instant) -> Self {
        Self {
            waiting: true,
            last_request: Some(now),
        }
    }

    /// Start waiting; returns true if a keyframe should be requested now.
    fn require_key(&mut self, now: Instant) -> bool {
        if self.waiting {
            return false; // already asked; on_frame repeats it
        }
        self.waiting = true;
        self.last_request = Some(now);
        true
    }

    /// For each encoded frame: (may this frame be sent?, request a keyframe now?)
    fn on_frame(&mut self, keyframe: bool, now: Instant) -> (bool, bool) {
        if !self.waiting || keyframe {
            return (true, false);
        }
        let due = self
            .last_request
            .is_none_or(|t| now.duration_since(t) >= KEY_REREQUEST);
        if due {
            self.last_request = Some(now);
        }
        (false, due)
    }

    /// The keyframe went out.
    fn satisfied(&mut self) {
        self.waiting = false;
    }
}
/// A frame not delivered within this is reset; the next frame sent is an IDR.
const FRAME_DEADLINE: Duration = Duration::from_millis(1000);

/// When a frame last went out (login mode types nothing without a fresh picture).
pub struct VideoSeen {
    t0: Instant,
    /// ms since `t0` + 1; 0 = never
    last: AtomicU64,
}

impl VideoSeen {
    fn new() -> Self {
        Self {
            t0: Instant::now(),
            last: AtomicU64::new(0),
        }
    }
    fn mark(&self) {
        self.last
            .store(self.t0.elapsed().as_millis() as u64 + 1, Ordering::Relaxed);
    }
    fn age_text(&self) -> String {
        match self.last.load(Ordering::Relaxed).checked_sub(1) {
            None => "none yet".into(),
            Some(t) => format!(
                "{} ms ago",
                (self.t0.elapsed().as_millis() as u64).saturating_sub(t)
            ),
        }
    }
    fn fresh(&self, within: Duration) -> bool {
        match self.last.load(Ordering::Relaxed).checked_sub(1) {
            None => false,
            Some(t) => {
                (self.t0.elapsed().as_millis() as u64).saturating_sub(t)
                    <= within.as_millis() as u64
            }
        }
    }
}

/// Login mode: a session counts against the phone's limits when it ends.
struct BootGuard {
    boot: Arc<crate::boot::BootState>,
    peer: iroh::EndpointId,
    name: String,
}

impl Drop for BootGuard {
    fn drop(&mut self) {
        self.boot.ended(&self.peer, &self.name);
    }
}

enum VideoCmd {
    Keyframe,
    SetStream(StreamParams),
}

/// Ends the session cleanly whatever path we leave through: release held input (and undo lock
/// keys), unregister.
struct SessionGuard {
    state: Arc<AppState>,
    id: u64,
    name: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.state.end_session_input();
        self.state.unregister_session(self.id);
        self.state
            .notifier
            .notify("DiShus: phone disconnected", &self.name);
    }
}

fn selected_rtt(conn: &Connection) -> Duration {
    conn.paths()
        .iter()
        .find(|p| p.is_selected())
        .map(|p| p.rtt())
        .unwrap_or(Duration::from_millis(100))
}

/// Selected path: (RTT, QUIC packets sent, packets lost not counting MTU probes), as totals.
fn path_counters(conn: &Connection) -> Option<(Duration, u64, u64)> {
    conn.paths().iter().find(|p| p.is_selected()).map(|p| {
        let s = p.stats();
        let lost = s.lost_packets.saturating_sub(s.lost_plpmtud_probes);
        (p.rtt(), s.sent_packets, lost)
    })
}

fn path_summary(conn: &Connection) -> String {
    let paths = conn.paths();
    match paths.iter().find(|p| p.is_selected()) {
        Some(p) if p.is_relay() => format!("relay, rtt {} ms", p.rtt().as_millis()),
        Some(p) => format!("direct, rtt {} ms", p.rtt().as_millis()),
        None => "connecting".into(),
    }
}

pub async fn handle(state: Arc<AppState>, conn: Connection) -> Result<()> {
    let peer = conn.remote_id();
    if !state.is_session_allowed(&peer) {
        conn.close(state.refusal_code(&peer).into(), b"");
        bail!("peer no longer allowed");
    }
    let device = state.device(&peer).context("device vanished")?;
    // session/2 phones may ask for audio; to a session/1 phone nothing new is ever sent.
    let v2 = conn.alpn() == ALPN_SESSION_V2;

    let (send, mut recv) = timeout(HELLO_TIMEOUT, conn.accept_bi())
        .await
        .context("no control stream")??;
    let hello: ClientMsg = timeout(HELLO_TIMEOUT, read_frame(&mut recv, MAX_CONTROL_FRAME))
        .await
        .context("no hello")??;
    let ClientMsg::Hello { stream: params, .. } = hello else {
        conn.close(1u32.into(), b"");
        bail!("first message was not Hello");
    };

    // Use the name stored at pairing time, not whatever this connection claims.
    let name = device.name.clone();
    let (sid, mut cancel) = state.register_session(peer, name.clone(), path_summary(&conn));
    let _guard = SessionGuard {
        state: state.clone(),
        id: sid,
        name: name.clone(),
    };
    state.touch(&peer);
    let _boot_guard = state.boot.clone().map(|boot| {
        boot.started(&peer, &name);
        BootGuard {
            boot,
            peer,
            name: name.clone(),
        }
    });
    state.notifier.notify(
        "DiShus: phone connected",
        &format!("{name} is controlling this PC"),
    );

    // Writer task: everything we send on the control stream goes through here.
    let (out_tx, out_rx) = mpsc::channel::<ServerMsg>(64);
    let writer = tokio::spawn(writer_task(send, out_rx));

    // Capture permission (portal session) for the lifetime of this connection.
    let capture = match timeout(CAPTURE_OPEN_TIMEOUT, state.capture.open()).await {
        Ok(Ok(c)) => Some(c),
        Ok(Err(e)) => {
            tracing::warn!("capture unavailable: {e:#}");
            let _ = out_tx
                .send(ServerMsg::Notice {
                    kind: NoticeKind::CaptureUnavailable,
                    message: truncate(&format!("{e:#}")),
                })
                .await;
            None
        }
        Err(_) => {
            let _ = out_tx
                .send(ServerMsg::Notice {
                    kind: NoticeKind::CaptureUnavailable,
                    message:
                        "screen capture permission timed out (run `dishus setup-capture` at the PC)"
                            .into(),
                })
                .await;
            None
        }
    };
    if state.boot.is_some() && capture.is_none() {
        // owner's rule: a password is never typed unseen
        tracing::warn!("{name}: login mode without a picture of the login screen; ending");
        let _ = out_tx
            .send(ServerMsg::Notice {
                kind: NoticeKind::CaptureUnavailable,
                message: "the login screen can't be shown, so login mode won't type anything"
                    .into(),
            })
            .await;
        let _ = out_tx
            .send(ServerMsg::Bye {
                reason: ByeReason::Normal,
            })
            .await;
        drop(out_tx);
        let _ = timeout(Duration::from_millis(1500), writer).await;
        conn.close(0u32.into(), b"bye");
        return Ok(());
    }
    let monitors: Vec<MonitorInfo> = capture.as_ref().map(|c| c.monitors()).unwrap_or_default();
    let expires_at = state.device(&peer).map(|d| d.expires_at).unwrap_or(0);
    out_tx
        .send(ServerMsg::Welcome {
            proto_version: PROTO_VERSION,
            pc_name: state.config.pc_name.clone(),
            monitors,
            expires_at,
        })
        .await
        .ok();

    let (vid_tx, vid_rx) = mpsc::channel::<VideoCmd>(16);
    // The video controller's view of the link, for the audio sender.
    let pressure = Arc::new(AtomicU8::new(Pressure::Clean as u8));
    let seen = Arc::new(VideoSeen::new());
    let video = capture.map(|cap| {
        tokio::spawn(video_task(
            conn.clone(),
            cap,
            params,
            vid_rx,
            out_tx.clone(),
            state.config.video.max_bitrate_kbps,
            pressure.clone(),
            seen.clone(),
        ))
    });
    // Login mode: sessions are short, and input needs a fresh picture.
    let login_mode = state.boot.is_some();
    let session_end = tokio::time::Instant::now()
        + if login_mode {
            crate::boot::SESSION_MAX
        } else {
            Duration::from_secs(100 * 365 * 24 * 3600)
        };
    let mut warned_unseen = false;
    let mut audio = AudioSlot {
        conn: conn.clone(),
        state: state.clone(),
        pressure,
        out: out_tx.clone(),
        task: None,
    };

    // Reader task: a frame half-read when some other select! branch fires would be lost (and
    // the stream misparsed), so reading never happens inside the select below.
    let (msg_tx, mut msg_rx) = mpsc::channel::<Result<ClientMsg, FrameError>>(64);
    let reader = tokio::spawn(async move {
        loop {
            let r = read_frame::<_, ClientMsg>(&mut recv, MAX_CONTROL_FRAME).await;
            let last = r.is_err();
            if msg_tx.send(r).await.is_err() || last {
                break;
            }
        }
    });

    let idle = Duration::from_millis(SESSION_IDLE_TIMEOUT_MS);
    let mut last_rx = tokio::time::Instant::now();
    let mut held_released = true;
    let mut silence = tokio::time::interval(SILENCE_CHECK);
    silence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut expiry = tokio::time::interval(EXPIRY_CHECK);
    expiry.tick().await;
    let reason = loop {
        tokio::select! {
            r = msg_rx.recv() => {
                let msg = match r {
                    None | Some(Err(FrameError::Closed)) => break ByeReason::Normal,
                    Some(Err(e)) => {
                        tracing::warn!("{name}: protocol error: {e}");
                        break ByeReason::ProtocolError;
                    }
                    Some(Ok(m)) => m,
                };
                last_rx = tokio::time::Instant::now();
                held_released = false;
                // Login mode: no live picture, no input. Releases always pass: they can't type
                // anything, and dropping one would leave a key held down (auto-repeating).
                let is_release = matches!(
                    msg,
                    ClientMsg::Key { down: false, .. } | ClientMsg::MouseButton { down: false, .. }
                );
                let input_ok =
                    !login_mode || is_release || seen.fresh(crate::boot::VIDEO_FRESH);
                let is_input = matches!(
                    msg,
                    ClientMsg::Key { .. }
                        | ClientMsg::MouseMove { .. }
                        | ClientMsg::MouseButton { .. }
                        | ClientMsg::Scroll { .. }
                );
                if !input_ok && is_input && !warned_unseen {
                    warned_unseen = true;
                    tracing::warn!(
                        "{name}: login mode: input dropped, no fresh picture of the login screen (last frame: {})",
                        seen.age_text()
                    );
                }
                match handle_msg(&state, &peer, v2, input_ok, msg, &out_tx, &vid_tx, &mut audio).await {
                    Ok(true) => {}
                    Ok(false) => break ByeReason::Normal,
                    Err(e) => {
                        tracing::warn!("{name}: {e:#}");
                        break ByeReason::ProtocolError;
                    }
                }
            }
            _ = tokio::time::sleep_until(last_rx + idle) => break ByeReason::Timeout,
            _ = tokio::time::sleep_until(session_end) => {
                tracing::info!("{name}: login mode session reached its {} s limit", crate::boot::SESSION_MAX.as_secs());
                let _ = out_tx.try_send(ServerMsg::Notice {
                    kind: NoticeKind::Info,
                    message: "login mode sessions last at most 3 minutes".into(),
                });
                break ByeReason::Timeout;
            }
            _ = silence.tick() => {
                // Nothing (not even a heartbeat) for a while: the network is probably switching
                // or gone. Let go of held keys/buttons now, so nothing auto-repeats until the
                // session times out; a late "up" from the phone is ignored by TrackedInput.
                if !held_released && last_rx.elapsed() >= RELEASE_ON_SILENCE {
                    held_released = true;
                    let held = state.input.lock().expect("input lock").held();
                    if held != (0, 0) {
                        tracing::info!("{name}: phone silent; released held input {held:?}");
                        state.release_input();
                    }
                }
            }
            _ = cancel.changed() => {
                break cancel.borrow().unwrap_or(ByeReason::Normal);
            }
            _ = expiry.tick() => {
                if !state.is_session_allowed(&peer) {
                    break ByeReason::Revoked;
                }
                state.set_session_path(sid, path_summary(&conn));
            }
            _ = conn.closed() => break ByeReason::Normal,
        }
    };
    reader.abort();
    tracing::info!("{name}: session ended ({reason:?})");
    // Release input (and undo lock keys) *before* anything that can block.
    state.end_session_input();
    if let Some(v) = video {
        v.abort();
    }
    drop(audio);
    let _ = out_tx.send(ServerMsg::Bye { reason }).await;
    drop(out_tx);
    let _ = timeout(Duration::from_millis(1500), writer).await;
    conn.close(0u32.into(), b"bye");
    Ok(())
}

fn truncate(s: &str) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).take(200).collect();
    if out.is_empty() {
        out.push('?');
    }
    out
}

/// Returns Ok(false) when the client said goodbye.
#[allow(clippy::too_many_arguments)]
async fn handle_msg(
    state: &AppState,
    peer: &iroh::EndpointId,
    v2: bool,
    input_ok: bool,
    msg: ClientMsg,
    out: &mpsc::Sender<ServerMsg>,
    vid: &mpsc::Sender<VideoCmd>,
    audio: &mut AudioSlot,
) -> Result<bool> {
    let input_res = if !input_ok {
        None
    } else {
        let mut input = state.input.lock().expect("input lock");
        match &msg {
            ClientMsg::Key { code, down } => Some(input.key(*code, *down)),
            ClientMsg::MouseMove { dx, dy } => Some(input.motion((*dx).into(), (*dy).into())),
            ClientMsg::MouseButton { button, down } => Some(input.button(*button, *down)),
            ClientMsg::Scroll { dx, dy } => Some(input.scroll((*dx).into(), (*dy).into())),
            ClientMsg::ReleaseAll => {
                input.release_all();
                None
            }
            _ => None,
        }
    };
    if let Some(Err(e)) = input_res {
        tracing::error!("input injection failed: {e}");
    }
    match msg {
        ClientMsg::Hello { .. } => bail!("duplicate Hello"),
        ClientMsg::Ping { t } => {
            let _ = out.try_send(ServerMsg::Pong { t });
        }
        ClientMsg::RequestKeyframe => {
            let _ = vid.try_send(VideoCmd::Keyframe);
        }
        ClientMsg::SetStream(p) => {
            let _ = vid.send(VideoCmd::SetStream(p)).await;
        }
        ClientMsg::RenewPairing { days } => match state.renew(peer, days) {
            Ok(expires_at) => {
                tracing::info!("pairing renewed from session for {days} days");
                let _ = out.send(ServerMsg::Renewed { expires_at }).await;
            }
            Err(e) => {
                let _ = out
                    .send(ServerMsg::Notice {
                        kind: NoticeKind::Refused,
                        message: truncate(&format!("{e:#}")),
                    })
                    .await;
            }
        },
        ClientMsg::Bye => return Ok(false),
        ClientMsg::SetAudio { .. } if !v2 => bail!("SetAudio on a session/1 connection"),
        ClientMsg::SetAudio { on } => audio.set(on),
        ClientMsg::Key { .. }
        | ClientMsg::MouseMove { .. }
        | ClientMsg::MouseButton { .. }
        | ClientMsg::Scroll { .. }
        | ClientMsg::ReleaseAll => {}
    }
    Ok(true)
}

async fn writer_task(mut send: SendStream, mut rx: mpsc::Receiver<ServerMsg>) {
    while let Some(m) = rx.recv().await {
        let bye = matches!(m, ServerMsg::Bye { .. });
        if let Err(e) = write_frame(&mut send, &m, MAX_CONTROL_FRAME).await {
            tracing::debug!("control write: {e}");
            return;
        }
        if bye {
            let _ = send.finish();
            // Wait until the peer has the Bye before the connection is closed (closing discards
            // unacknowledged stream data).
            let _ = timeout(Duration::from_secs(1), send.stopped()).await;
            return;
        }
    }
}

/// Sends encoded frames, one QUIC uni-stream each, dropping frames when the link can't keep up.
struct FrameSender {
    conn: Connection,
    seq: u64,
    monitor: u8,
    inflight: Arc<AtomicUsize>,
    /// Set by a stalled frame task; makes the next pushed frame wait for an IDR.
    need_key: Arc<AtomicBool>,
    stalled: Arc<AtomicU32>,
    /// Keyframes sent and not yet delivered (see [`over_limit`]).
    keys_inflight: Arc<AtomicUsize>,
    key: KeyGate,
    /// Current in-flight limit (see [`inflight_limit`]); updated every ABR tick.
    max_inflight: usize,
    stats: TickStats,
    seen: Arc<VideoSeen>,
}

impl FrameSender {
    fn new(conn: Connection, monitor: u8, seen: Arc<VideoSeen>) -> Self {
        Self {
            seen,
            conn,
            seq: 0,
            monitor,
            inflight: Arc::new(AtomicUsize::new(0)),
            need_key: Arc::new(AtomicBool::new(false)),
            stalled: Arc::new(AtomicU32::new(0)),
            keys_inflight: Arc::new(AtomicUsize::new(0)),
            key: KeyGate::new(Instant::now()),
            max_inflight: 4,
            stats: TickStats::default(),
        }
    }

    fn take_stats(&mut self) -> TickStats {
        let mut s = std::mem::take(&mut self.stats);
        s.stalled = self.stalled.swap(0, Ordering::Relaxed);
        s.inflight_limit = self.max_inflight;
        s.peak_inflight = s.peak_inflight.max(self.inflight.load(Ordering::Relaxed));
        s
    }

    fn push(&mut self, f: VideoFrame, ctl: &dyn StreamControl) {
        let now = Instant::now();
        if self.need_key.swap(false, Ordering::AcqRel) && self.key.require_key(now) {
            ctl.force_keyframe();
        }
        let (send, ask) = self.key.on_frame(f.keyframe, now);
        if ask {
            self.stats.key_requests += 1;
            ctl.force_keyframe();
        }
        if !send {
            self.stats.dropped += 1;
            return;
        }
        let inflight = self.inflight.load(Ordering::Acquire);
        self.stats.peak_inflight = self.stats.peak_inflight.max(inflight);
        let key_in_flight = self.keys_inflight.load(Ordering::Acquire) > 0;
        if over_limit(inflight, self.max_inflight, key_in_flight) {
            self.stats.dropped += 1;
            if self.key.require_key(now) {
                ctl.force_keyframe();
            }
            return;
        }
        self.key.satisfied();
        self.seen.mark();
        self.stats.sent += 1;
        self.stats.bytes += f.data.len() as u64;
        self.stats.keys_sent += u32::from(f.keyframe);
        self.seq += 1;
        let header = VideoHeader {
            seq: self.seq,
            pts_us: f.pts_us,
            flags: if f.keyframe { FLAG_KEYFRAME } else { 0 },
            monitor: self.monitor,
        };
        self.inflight.fetch_add(1, Ordering::AcqRel);
        let keyframe = f.keyframe;
        if keyframe {
            self.keys_inflight.fetch_add(1, Ordering::AcqRel);
        }
        let conn = self.conn.clone();
        let inflight = self.inflight.clone();
        let keys_inflight = self.keys_inflight.clone();
        let need_key = self.need_key.clone();
        let stalled = self.stalled.clone();
        // Newer frames first when the link is congested? No: P-frames depend on their
        // predecessors, so older frames keep priority (higher number = higher priority).
        let prio = -((self.seq % (i32::MAX as u64)) as i32);
        tokio::spawn(async move {
            let res: Result<()> = async {
                let mut s = conn.open_uni().await?;
                let _ = s.set_priority(prio);
                let sent = timeout(FRAME_DEADLINE, async {
                    s.write_all(&header.encode()).await?;
                    s.write_chunk(f.data).await?;
                    s.finish()?;
                    let _ = s.stopped().await;
                    anyhow::Ok(())
                })
                .await;
                match sent {
                    Ok(r) => r,
                    Err(_) => {
                        let _ = s.reset(1u32.into());
                        stalled.fetch_add(1, Ordering::Relaxed);
                        need_key.store(true, Ordering::Release);
                        Ok(())
                    }
                }
            }
            .await;
            if let Err(e) = res {
                tracing::trace!("frame send: {e:#}");
                need_key.store(true, Ordering::Release);
            }
            inflight.fetch_sub(1, Ordering::AcqRel);
            if keyframe {
                keys_inflight.fetch_sub(1, Ordering::AcqRel);
            }
        });
    }
}

/// PC audio for one session: off until the phone asks, stopped (worker killed) when it says
/// off or the session ends.
struct AudioSlot {
    conn: Connection,
    state: Arc<AppState>,
    pressure: Arc<AtomicU8>,
    out: mpsc::Sender<ServerMsg>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl AudioSlot {
    fn set(&mut self, on: bool) {
        if !on {
            if let Some(t) = self.task.take() {
                t.abort();
                tracing::info!("audio off");
            }
        } else if self.task.as_ref().is_none_or(|t| t.is_finished()) {
            self.task = Some(tokio::spawn(audio_task(
                self.conn.clone(),
                self.state.clone(),
                self.pressure.clone(),
                self.out.clone(),
            )));
        }
    }
}

impl Drop for AudioSlot {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

async fn audio_unavailable(out: &mpsc::Sender<ServerMsg>, why: String) {
    let _ = out
        .send(ServerMsg::Notice {
            kind: NoticeKind::AudioUnavailable,
            message: truncate(&why),
        })
        .await;
}

/// Capture, then one datagram per Opus packet, under the rules in [`crate::audio`].
async fn audio_task(
    conn: Connection,
    state: Arc<AppState>,
    pressure: Arc<AtomicU8>,
    out: mpsc::Sender<ServerMsg>,
) {
    if conn.max_datagram_size().is_none() {
        return audio_unavailable(&out, "the phone does not accept audio datagrams".into()).await;
    }
    let mut stream = match state.capture.open_audio().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("audio unavailable: {e:#}");
            return audio_unavailable(&out, format!("{e:#}")).await;
        }
    };
    tracing::info!("audio on (opus, {} kbps)", audio_policy::HIGH_KBPS);
    let _ = out
        .send(ServerMsg::AudioStarted {
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS,
        })
        .await;
    let mut policy = AudioPolicy::default();
    let mut seq = 0u32;
    // Over ~10 s: sent, dropped under congestion, dropped for queueing, too big for a datagram.
    let (mut sent, mut congested, mut queued_out, mut too_big) = (0u32, 0u32, 0u32, 0u32);
    let mut ticks = 0u32;
    let mut tick = tokio::time::interval(Duration::from_millis(abr::TICK_MS));
    loop {
        tokio::select! {
            p = stream.packets.recv() => {
                let Some(p) = p else {
                    return audio_unavailable(&out, "audio capture stopped".into()).await;
                };
                seq = seq.wrapping_add(1);
                let queued = audio_policy::DATAGRAM_SEND_BUFFER
                    .saturating_sub(conn.datagram_send_buffer_space());
                match audio_policy::admit(Pressure::load(&pressure), queued) {
                    Err(Dropped::Congestion) => congested += 1,
                    Err(Dropped::Queue) => queued_out += 1,
                    Ok(()) => {
                        let mut buf = Vec::with_capacity(16 + p.data.len());
                        buf.extend_from_slice(&AudioHeader { seq, pts_us: p.pts_us }.encode());
                        buf.extend_from_slice(&p.data);
                        if buf.len() > conn.max_datagram_size().unwrap_or(0) {
                            too_big += 1;
                        } else {
                            match conn.send_datagram(buf.into()) {
                                Ok(()) => sent += 1,
                                Err(iroh::endpoint::SendDatagramError::ConnectionLost(_)) => return,
                                Err(e) => {
                                    tracing::warn!("audio datagram: {e}");
                                    return audio_unavailable(&out, format!("audio: {e}")).await;
                                }
                            }
                        }
                    }
                }
            }
            _ = tick.tick() => {
                if let Some(k) = policy.tick(Pressure::load(&pressure)) {
                    tracing::debug!("audio bitrate -> {k} kbps");
                    stream.control.set_bitrate(k);
                }
                ticks += 1;
                if ticks * abr::TICK_MS as u32 >= 10_000 {
                    let msg = format!(
                        "audio: {sent} packets sent, dropped {congested} (congestion) \
                         {queued_out} (queue) {too_big} (too big), {} kbps",
                        policy.kbps
                    );
                    if congested + queued_out + too_big > 0 {
                        tracing::info!("{msg}");
                    } else {
                        tracing::debug!("{msg}");
                    }
                    (sent, congested, queued_out, too_big, ticks) = (0, 0, 0, 0, 0);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn video_task(
    conn: Connection,
    mut capture: Box<dyn CaptureSession>,
    mut params: StreamParams,
    mut cmds: mpsc::Receiver<VideoCmd>,
    out: mpsc::Sender<ServerMsg>,
    ceiling_kbps: u32,
    pressure: Arc<AtomicU8>,
    seen: Arc<VideoSeen>,
) {
    'restart: loop {
        let monitors = capture.monitors();
        if monitors.is_empty() {
            return;
        }
        if usize::from(params.monitor) >= monitors.len() {
            params.monitor = 0;
        }
        let mut abr = Abr::new(abr::bounds(params.quality, ceiling_kbps), params.fps);
        let cfg = StreamConfig {
            monitor: params.monitor,
            fps: abr.fps,
            bitrate_kbps: abr.bitrate,
            max_height: params.max_height,
        };
        let mut stream: EncodedStream = match capture.start_stream(cfg).await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("starting video stream: {e:#}");
                let _ = out
                    .send(ServerMsg::Notice {
                        kind: NoticeKind::CaptureUnavailable,
                        message: truncate(&format!("{e:#}")),
                    })
                    .await;
                // wait for the phone to ask for something else
                loop {
                    match cmds.recv().await {
                        None => return,
                        Some(VideoCmd::SetStream(p)) => {
                            params = p;
                            continue 'restart;
                        }
                        Some(VideoCmd::Keyframe) => {}
                    }
                }
            }
        };
        tracing::info!(
            "streaming monitor {} at {}x{} via {} ({} kbps, {} fps)",
            params.monitor,
            stream.width,
            stream.height,
            stream.encoder,
            abr.bitrate,
            abr.fps
        );
        let _ = out
            .send(ServerMsg::StreamInfo {
                monitor: params.monitor,
                width: stream.width,
                height: stream.height,
                fps: abr.fps,
                bitrate_kbps: abr.bitrate,
                encoder: stream.encoder.clone(),
            })
            .await;
        let mut sender = FrameSender::new(conn.clone(), params.monitor, seen.clone());
        let mut tick = tokio::time::interval(Duration::from_millis(abr::TICK_MS));
        // Video health over ~5 s: INFO when frames were dropped or stalled, else DEBUG.
        let mut window = TickStats::default();
        let mut window_ticks = 0u32;
        // Per-second stats at DEBUG (DISHUS_LOG=dishus_daemon::session=debug): the controller's
        // target next to what the encoder really produced tells rate control from content.
        let mut second = TickStats::default();
        let mut second_ticks = 0u32;
        // Packet totals at the last tick (per path: a path change restarts them, hence saturating).
        let mut counted = (0u64, 0u64);
        stream.control.force_keyframe();
        loop {
            tokio::select! {
                f = stream.frames.recv() => match f {
                    Some(f) => sender.push(f, stream.control.as_ref()),
                    None => {
                        tracing::error!("video pipeline ended");
                        let _ = out.send(ServerMsg::Notice {
                            kind: NoticeKind::CaptureUnavailable,
                            message: "video pipeline stopped".into(),
                        }).await;
                        // try a restart after a pause (e.g. monitor unplugged / mode change)
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue 'restart;
                    }
                },
                c = cmds.recv() => match c {
                    None => return,
                    Some(VideoCmd::Keyframe) => stream.control.force_keyframe(),
                    Some(VideoCmd::SetStream(p)) => {
                        let restart = p.monitor != params.monitor || p.max_height != params.max_height;
                        let retune = p.quality != params.quality || p.fps != params.fps;
                        params = p;
                        if restart {
                            continue 'restart;
                        }
                        if retune {
                            abr = Abr::new(abr::bounds(params.quality, ceiling_kbps), params.fps);
                            stream.control.set_bitrate(abr.bitrate);
                            stream.control.set_fps(abr.fps);
                        }
                    }
                },
                _ = tick.tick() => {
                    sender.max_inflight = inflight_limit(selected_rtt(&conn), abr.fps);
                    let mut st = sender.take_stats();
                    if let Some((rtt, sent, lost)) = path_counters(&conn) {
                        st.rtt_ms = u32::try_from(rtt.as_millis()).unwrap_or(u32::MAX);
                        st.sent_packets = sent.saturating_sub(counted.0);
                        st.lost_packets = lost.saturating_sub(counted.1);
                        counted = (sent, lost);
                    }
                    window.sent += st.sent;
                    window.keys_sent += st.keys_sent;
                    window.dropped += st.dropped;
                    window.stalled += st.stalled;
                    window.peak_inflight = window.peak_inflight.max(st.peak_inflight);
                    window.key_requests += st.key_requests;
                    window_ticks += 1;
                    second.sent += st.sent;
                    second.bytes += st.bytes;
                    second.keys_sent += st.keys_sent;
                    second.dropped += st.dropped;
                    second.stalled += st.stalled;
                    second.peak_inflight = second.peak_inflight.max(st.peak_inflight);
                    second.sent_packets += st.sent_packets;
                    second.lost_packets += st.lost_packets;
                    second_ticks += 1;
                    if second_ticks * abr::TICK_MS as u32 >= 1000 {
                        let s = std::mem::take(&mut second);
                        tracing::debug!(
                            "video 1s: target {} kbps {} fps, sent {} kbps {} frames ({} key), \
                             {} dropped, {} stalled, in flight {}/{}, rtt {} ms, \
                             lost {}/{} packets",
                            abr.bitrate,
                            abr.fps,
                            s.bytes * 8 / (u64::from(second_ticks) * abr::TICK_MS), // bits per ms = kbit/s
                            s.sent,
                            s.keys_sent,
                            s.dropped,
                            s.stalled,
                            s.peak_inflight,
                            sender.max_inflight,
                            selected_rtt(&conn).as_millis(),
                            s.lost_packets,
                            s.sent_packets
                        );
                        second_ticks = 0;
                    }
                    if window_ticks * abr::TICK_MS as u32 >= 5000 {
                        let w = std::mem::take(&mut window);
                        window_ticks = 0;
                        let msg = format!(
                            "video: {} frames sent ({} key), {} dropped, {} stalled, {} key requests, \
                             peak in flight {}/{}, {} kbps, {} fps",
                            w.sent,
                            w.keys_sent,
                            w.dropped,
                            w.stalled,
                            w.key_requests,
                            w.peak_inflight,
                            sender.max_inflight,
                            abr.bitrate,
                            abr.fps
                        );
                        if w.dropped > 0 || w.stalled > 0 {
                            tracing::info!("{msg}");
                        } else {
                            tracing::debug!("{msg}");
                        }
                    }
                    let ch = abr.tick(st);
                    abr.pressure().store(&pressure);
                    if let Some(b) = ch.bitrate {
                        stream.control.set_bitrate(b);
                    }
                    if let Some(fps) = ch.fps {
                        tracing::debug!("adaptive fps -> {fps}");
                        stream.control.set_fps(fps);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_behind_a_keyframe_get_twice_the_limit() {
        assert!(!over_limit(6, 7, false));
        assert!(over_limit(7, 7, false));
        assert!(!over_limit(13, 7, true), "queued behind a keyframe: keep");
        assert!(over_limit(14, 7, true), "still bounded");
    }

    #[test]
    fn inflight_limit_grows_with_rtt() {
        let ms = Duration::from_millis;
        assert_eq!(inflight_limit(ms(3), 30), 6); // LAN
        assert_eq!(inflight_limit(ms(60), 40), 9); // mobile data, measured on a phone
        assert_eq!(inflight_limit(ms(300), 60), 26); // far relay
        assert_eq!(inflight_limit(ms(5000), 60), 32); // capped
        assert_eq!(inflight_limit(ms(0), 1), 4); // floor
    }

    #[test]
    fn key_gate_rerequests_until_a_keyframe_arrives() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut g = KeyGate::new(t0);
        // fresh stream: drops non-key frames, re-asks every KEY_REREQUEST
        assert_eq!(g.on_frame(false, at(10)), (false, false));
        assert_eq!(
            g.on_frame(false, at(260)),
            (false, true),
            "first request was lost"
        );
        assert_eq!(g.on_frame(false, at(300)), (false, false));
        assert_eq!(g.on_frame(false, at(520)), (false, true), "and again");
        // the keyframe arrives and is sent
        assert_eq!(g.on_frame(true, at(530)), (true, false));
        g.satisfied();
        assert_eq!(g.on_frame(false, at(560)), (true, false));
        // congestion: one immediate request, no duplicate while already waiting
        assert!(g.require_key(at(600)));
        assert!(!g.require_key(at(610)));
        assert_eq!(g.on_frame(false, at(700)), (false, false));
        assert_eq!(g.on_frame(false, at(860)), (false, true));
    }
}
