//! Daemon side of the capture worker. Each remote session spawns one worker process
//! (`dishus-daemon --capture-worker session`) that holds the portal session and the GStreamer
//! pipeline. When the session ends the worker is killed, so the daemon itself never keeps a CUDA
//! context, GPU memory, or the media stack loaded, and a crash in the media stack can't take the
//! daemon down.

use std::{path::PathBuf, process::Stdio, sync::Arc};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use tokio::{
    process::{Child, Command},
    sync::{Mutex, mpsc},
};

use dishus_proto::{FrameError, MonitorInfo, read_frame, write_frame};

use super::wire::{MAX_CMD_FRAME, MAX_WORKER_FRAME, WorkerCmd, WorkerEvent};
use crate::backend::{
    BoxFut, BoxFuture, CaptureProvider, CaptureSession, EncodedStream, StreamConfig, StreamControl,
    VideoFrame,
};

#[derive(Debug, Clone)]
pub struct WorkerProvider {
    exe: PathBuf,
    env: Vec<(String, String)>,
}

impl WorkerProvider {
    pub fn new(token_file: PathBuf, encoder_pref: &str, show_cursor: bool) -> Result<Self> {
        let exe = std::env::current_exe().context("locating own executable")?;
        Ok(Self {
            exe,
            env: vec![
                (
                    "DISHUS_W_TOKEN_FILE".into(),
                    token_file.to_string_lossy().into_owned(),
                ),
                ("DISHUS_W_ENCODER".into(), encoder_pref.into()),
                (
                    "DISHUS_W_CURSOR".into(),
                    if show_cursor { "1" } else { "0" }.into(),
                ),
            ],
        })
    }

    fn spawn(&self, mode: &str) -> Result<Child> {
        let mut c = Command::new(&self.exe);
        c.arg("--capture-worker")
            .arg(mode)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        c.spawn().context("spawning capture worker")
    }

    /// Run a one-shot worker mode (`setup`, `probe`) and return its Info line.
    async fn one_shot(&self, mode: &str) -> Result<String> {
        let mut child = self.spawn(mode)?;
        let mut out = child.stdout.take().context("worker stdout")?;
        let ev: WorkerEvent = read_frame(&mut out, MAX_WORKER_FRAME)
            .await
            .map_err(|e| anyhow!("worker: {e}"))?;
        let _ = child.wait().await;
        match ev {
            WorkerEvent::Info(s) => Ok(s),
            WorkerEvent::Failed(s) => bail!("{s}"),
            other => bail!("unexpected worker reply {other:?}"),
        }
    }

    pub async fn probe(&self) -> String {
        match self.one_shot("probe").await {
            Ok(s) => s,
            Err(e) => format!("worker probe failed: {e:#}"),
        }
    }
}

impl CaptureProvider for WorkerProvider {
    fn open(&self) -> BoxFuture<Result<Box<dyn CaptureSession>>> {
        let this = self.clone();
        Box::pin(async move {
            let mut child = this.spawn("session")?;
            let mut stdout = child.stdout.take().context("worker stdout")?;
            let mut stdin = child.stdin.take().context("worker stdin")?;
            // First event: Monitors or Failed.
            let first: WorkerEvent = read_frame(&mut stdout, MAX_WORKER_FRAME)
                .await
                .map_err(|e| anyhow!("capture worker: {e}"))?;
            let monitors = match first {
                WorkerEvent::Monitors(m) if !m.is_empty() => m,
                WorkerEvent::Monitors(_) => bail!("capture worker found no monitors"),
                WorkerEvent::Failed(s) => bail!("{s}"),
                other => bail!("unexpected worker event {other:?}"),
            };
            // Reader: worker stdout -> events. Awaiting `send` gives natural back-pressure: a
            // slow consumer fills the pipe, the worker blocks, and its leaky queue drops *raw*
            // frames before the encoder (never encoded ones).
            let (ev_tx, ev_rx) = mpsc::channel::<WorkerEvent>(4);
            let reader = tokio::spawn(async move {
                loop {
                    match read_frame::<_, WorkerEvent>(&mut stdout, MAX_WORKER_FRAME).await {
                        Ok(ev) => {
                            if ev_tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                        Err(FrameError::Closed) => break,
                        Err(e) => {
                            tracing::error!("capture worker stream: {e}");
                            break;
                        }
                    }
                }
            });
            let (cmd_tx, mut cmd_rx) = mpsc::channel::<WorkerCmd>(32);
            let writer = tokio::spawn(async move {
                while let Some(c) = cmd_rx.recv().await {
                    if write_frame(&mut stdin, &c, MAX_CMD_FRAME).await.is_err() {
                        break;
                    }
                }
            });
            Ok(Box::new(WorkerSession {
                _child: child,
                cmd_tx,
                events: Arc::new(Mutex::new(ev_rx)),
                monitors,
                tasks: vec![reader, writer],
            }) as Box<dyn CaptureSession>)
        })
    }

    fn setup_interactive(&self) -> BoxFuture<Result<String>> {
        let this = self.clone();
        Box::pin(async move { this.one_shot("setup").await })
    }
}

struct WorkerSession {
    _child: Child, // kill_on_drop
    cmd_tx: mpsc::Sender<WorkerCmd>,
    events: Arc<Mutex<mpsc::Receiver<WorkerEvent>>>,
    monitors: Vec<MonitorInfo>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for WorkerSession {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

struct WorkerControl(mpsc::Sender<WorkerCmd>);

impl StreamControl for WorkerControl {
    fn force_keyframe(&self) {
        let _ = self.0.try_send(WorkerCmd::Keyframe);
    }
    fn set_bitrate(&self, kbps: u32) {
        let _ = self.0.try_send(WorkerCmd::Bitrate(kbps));
    }
    fn set_fps(&self, fps: u8) {
        let _ = self.0.try_send(WorkerCmd::Fps(fps));
    }
}

/// Stops the worker's pipeline and the forwarding task when the stream is dropped.
struct StreamGuard {
    cmd: mpsc::Sender<WorkerCmd>,
    forwarder: tokio::task::JoinHandle<()>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.forwarder.abort();
        let _ = self.cmd.try_send(WorkerCmd::Stop);
    }
}

impl CaptureSession for WorkerSession {
    fn monitors(&self) -> Vec<MonitorInfo> {
        self.monitors.clone()
    }

    fn start_stream(&mut self, cfg: StreamConfig) -> BoxFut<'_, Result<EncodedStream>> {
        Box::pin(async move {
            self.cmd_tx
                .send(WorkerCmd::Start {
                    monitor: cfg.monitor,
                    fps: cfg.fps,
                    bitrate_kbps: cfg.bitrate_kbps,
                    max_height: cfg.max_height,
                })
                .await
                .map_err(|_| anyhow!("capture worker exited"))?;
            let mut events = self.events.clone().lock_owned().await;
            let (width, height, encoder) = loop {
                match events.recv().await {
                    Some(WorkerEvent::Started {
                        width,
                        height,
                        encoder,
                    }) => break (width, height, encoder),
                    Some(WorkerEvent::Failed(s)) => bail!("{s}"),
                    Some(_) => continue, // stale frames from a previous pipeline
                    None => bail!("capture worker exited"),
                }
            };
            let (tx, rx) = mpsc::channel::<VideoFrame>(4);
            let forwarder = tokio::spawn(async move {
                while let Some(ev) = events.recv().await {
                    match ev {
                        WorkerEvent::Frame {
                            pts_us,
                            keyframe,
                            data,
                        } => {
                            if tx
                                .send(VideoFrame {
                                    data: Bytes::from(data),
                                    pts_us,
                                    keyframe,
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        WorkerEvent::Failed(s) => {
                            tracing::error!("capture: {s}");
                            break; // closes `rx`; the session restarts the stream
                        }
                        _ => {}
                    }
                }
            });
            Ok(EncodedStream {
                frames: rx,
                control: Arc::new(WorkerControl(self.cmd_tx.clone())),
                width,
                height,
                encoder,
                _guard: Box::new(StreamGuard {
                    cmd: self.cmd_tx.clone(),
                    forwarder,
                }),
            })
        })
    }
}
