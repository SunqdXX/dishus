//! The audio worker's main loop, shared by Linux (PipeWire + GStreamer) and Windows (WASAPI +
//! libopus): Opus packets out until stdin closes; `Bitrate` (kbit/s) in, anything else ignored.
//!
//! A capture that stops (Linux: WirePlumber re-links streams and pipewiresrc gives up, "target
//! not found"; Windows: the output device goes away or the default one changes) is restarted
//! here after a short gap. Only repeated failures end the worker.

use std::{collections::VecDeque, sync::Arc, time::Duration, time::Instant};

use anyhow::Result;

use dishus_proto::{FrameError, read_frame};

use super::wire::{MAX_CMD_FRAME, WorkerCmd, WorkerEvent};

const MAX_FAILS: usize = 5;
const FAIL_WINDOW: Duration = Duration::from_secs(30);

/// A running capture: stops when dropped.
pub trait RunningAudio {
    fn set_bitrate(&self, kbps: u32);
}

/// Writes one event to the daemon; false when the daemon is gone.
pub type SendEvent = Arc<dyn Fn(&WorkerEvent) -> bool + Send + Sync>;
/// One Opus packet: (pts in microseconds, packet).
pub type OnPacket = Arc<dyn Fn(u64, Vec<u8>) + Send + Sync>;
/// The capture stopped, with the reason.
pub type OnFail = Arc<dyn Fn(String) + Send + Sync>;

/// `start(kbps, on_packet, on_fail)` opens the capture. `test_source` enables the
/// `DISHUS_TEST_AUDIO_FAIL_MS` hook (a fake failure every N ms, to exercise the restart).
pub async fn run<R, F>(send: SendEvent, test_source: bool, start: F) -> Result<()>
where
    R: RunningAudio,
    F: Fn(u32, OnPacket, OnFail) -> Result<R>,
{
    let (fail_tx, mut fail_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut kbps = crate::audio::HIGH_KBPS;
    let open = |kbps: u32| {
        let s = send.clone();
        let f = fail_tx.clone();
        start(
            kbps,
            Arc::new(move |pts_us, data| {
                s(&WorkerEvent::Audio { pts_us, data });
            }),
            Arc::new(move |why: String| {
                let _ = f.send(why);
            }),
        )
    };
    let mut running = match open(kbps) {
        Ok(r) => Some(r),
        Err(e) => {
            send(&WorkerEvent::Failed(format!("{e:#}")));
            return Ok(());
        }
    };
    if !send(&WorkerEvent::AudioStarted) {
        return Ok(());
    }
    if test_source
        && let Some(ms) = std::env::var("DISHUS_TEST_AUDIO_FAIL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
    {
        let f = fail_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                if f.send("test failure".into()).is_err() {
                    break;
                }
            }
        });
    }
    let mut fails: VecDeque<Instant> = Default::default();
    let mut stdin = tokio::io::stdin();
    loop {
        tokio::select! {
            cmd = read_frame::<_, WorkerCmd>(&mut stdin, MAX_CMD_FRAME) => match cmd {
                Ok(WorkerCmd::Bitrate(k)) => {
                    kbps = k;
                    if let Some(r) = &running {
                        r.set_bitrate(k);
                    }
                }
                Ok(WorkerCmd::Stop) | Err(FrameError::Closed) => break,
                Ok(_) => {}
                Err(e) => {
                    tracing::error!("audio worker: bad command: {e}");
                    break;
                }
            },
            Some(why) = fail_rx.recv() => {
                running = None;
                let now = Instant::now();
                fails.retain(|t| now.duration_since(*t) < FAIL_WINDOW);
                fails.push_back(now);
                if fails.len() >= MAX_FAILS {
                    send(&WorkerEvent::Failed(format!("{why} ({MAX_FAILS} times in 30 s)")));
                    break;
                }
                tracing::warn!("audio capture stopped ({why}); restarting");
                tokio::time::sleep(Duration::from_millis(200)).await;
                while fail_rx.try_recv().is_ok() {} // stale reports from the old capture
                match open(kbps) {
                    Ok(r) => running = Some(r),
                    Err(e) => {
                        send(&WorkerEvent::Failed(format!("{e:#}")));
                        break;
                    }
                }
            }
        }
    }
    drop(running);
    Ok(())
}
