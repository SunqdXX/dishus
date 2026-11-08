//! One authenticated remote-control session (ALPN `dishus/session/1`).
//!
//! The gate has already verified the peer against the allowlist; we check again here (the
//! device could have been revoked between handshake and now) and on a timer while connected.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use iroh::endpoint::{Connection, SendStream};
use tokio::{sync::mpsc, time::timeout};

use dishus_proto::{
    ByeReason, ClientMsg, FrameError, MAX_CONTROL_FRAME, MonitorInfo, NoticeKind, PROTO_VERSION,
    SESSION_IDLE_TIMEOUT_MS, ServerMsg, StreamParams, VideoHeader, read_frame,
    video::FLAG_KEYFRAME, write_frame,
};

use crate::{
    abr::{self, Abr, TickStats},
    backend::{CaptureSession, EncodedStream, StreamConfig, StreamControl, VideoFrame},
    state::AppState,
};

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const CAPTURE_OPEN_TIMEOUT: Duration = Duration::from_secs(15);
const EXPIRY_CHECK: Duration = Duration::from_secs(15);
/// Frames allowed in flight (sent, not yet fully acknowledged) before we start dropping.
const MAX_INFLIGHT: usize = 4;
/// A frame not delivered within this is reset; the next frame sent is an IDR.
const FRAME_DEADLINE: Duration = Duration::from_millis(1000);

enum VideoCmd {
    Keyframe,
    SetStream(StreamParams),
}

/// Ends the session cleanly whatever path we leave through: release held input, unregister.
struct SessionGuard {
    state: Arc<AppState>,
    id: u64,
    name: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.state.release_input();
        self.state.unregister_session(self.id);
        self.state
            .notifier
            .notify("DiShus: phone disconnected", &self.name);
    }
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
    let video = capture.map(|cap| {
        tokio::spawn(video_task(
            conn.clone(),
            cap,
            params,
            vid_rx,
            out_tx.clone(),
            state.config.video.max_bitrate_kbps,
        ))
    });

    let idle = Duration::from_millis(SESSION_IDLE_TIMEOUT_MS);
    let mut expiry = tokio::time::interval(EXPIRY_CHECK);
    expiry.tick().await;
    let reason = loop {
        tokio::select! {
            r = timeout(idle, read_frame::<_, ClientMsg>(&mut recv, MAX_CONTROL_FRAME)) => {
                let msg = match r {
                    Err(_) => break ByeReason::Timeout,
                    Ok(Err(FrameError::Closed)) => break ByeReason::Normal,
                    Ok(Err(e)) => {
                        tracing::warn!("{name}: protocol error: {e}");
                        break ByeReason::ProtocolError;
                    }
                    Ok(Ok(m)) => m,
                };
                match handle_msg(&state, &peer, msg, &out_tx, &vid_tx).await {
                    Ok(true) => {}
                    Ok(false) => break ByeReason::Normal,
                    Err(e) => {
                        tracing::warn!("{name}: {e:#}");
                        break ByeReason::ProtocolError;
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
    tracing::info!("{name}: session ended ({reason:?})");
    // Release input *before* anything that can block.
    state.release_input();
    if let Some(v) = video {
        v.abort();
    }
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
async fn handle_msg(
    state: &AppState,
    peer: &iroh::EndpointId,
    msg: ClientMsg,
    out: &mpsc::Sender<ServerMsg>,
    vid: &mpsc::Sender<VideoCmd>,
) -> Result<bool> {
    let input_res = {
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
    waiting_for_key: bool,
    stats: TickStats,
}

impl FrameSender {
    fn new(conn: Connection, monitor: u8) -> Self {
        Self {
            conn,
            seq: 0,
            monitor,
            inflight: Arc::new(AtomicUsize::new(0)),
            need_key: Arc::new(AtomicBool::new(false)),
            stalled: Arc::new(AtomicU32::new(0)),
            waiting_for_key: true, // a fresh stream must start with an IDR
            stats: TickStats::default(),
        }
    }

    fn take_stats(&mut self) -> TickStats {
        let mut s = std::mem::take(&mut self.stats);
        s.stalled = self.stalled.swap(0, Ordering::Relaxed);
        s.peak_inflight = s.peak_inflight.max(self.inflight.load(Ordering::Relaxed));
        s
    }

    fn push(&mut self, f: VideoFrame, ctl: &dyn StreamControl) {
        if self.need_key.swap(false, Ordering::AcqRel) {
            self.waiting_for_key = true;
            ctl.force_keyframe();
        }
        if self.waiting_for_key && !f.keyframe {
            self.stats.dropped += 1;
            return;
        }
        let inflight = self.inflight.load(Ordering::Acquire);
        self.stats.peak_inflight = self.stats.peak_inflight.max(inflight);
        if inflight >= MAX_INFLIGHT {
            self.stats.dropped += 1;
            if !self.waiting_for_key {
                self.waiting_for_key = true;
                ctl.force_keyframe();
            }
            return;
        }
        self.waiting_for_key = false;
        self.seq += 1;
        let header = VideoHeader {
            seq: self.seq,
            pts_us: f.pts_us,
            flags: if f.keyframe { FLAG_KEYFRAME } else { 0 },
            monitor: self.monitor,
        };
        self.inflight.fetch_add(1, Ordering::AcqRel);
        let conn = self.conn.clone();
        let inflight = self.inflight.clone();
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
        });
    }
}

async fn video_task(
    conn: Connection,
    mut capture: Box<dyn CaptureSession>,
    mut params: StreamParams,
    mut cmds: mpsc::Receiver<VideoCmd>,
    out: mpsc::Sender<ServerMsg>,
    ceiling_kbps: u32,
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
        let mut sender = FrameSender::new(conn.clone(), params.monitor);
        let mut tick = tokio::time::interval(Duration::from_millis(abr::TICK_MS));
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
                    let ch = abr.tick(sender.take_stats());
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
