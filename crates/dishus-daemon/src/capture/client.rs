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
    AudioControl, AudioPacket, AudioStream, BoxFut, BoxFuture, CaptureProvider, CaptureSession,
    EncodedStream, StreamConfig, StreamControl, VideoFrame,
};

/// The audio worker's first answer (capture running, or why not).
const AUDIO_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct WorkerProvider {
    exe: PathBuf,
    env: Vec<(String, String)>,
    /// Windows: the daemon log, which the worker's stderr appends to (there is no journal and
    /// the daemon itself has no stderr).
    #[cfg(windows)]
    log: Option<PathBuf>,
}

impl WorkerProvider {
    #[cfg(windows)]
    pub fn with_log(mut self, log: PathBuf) -> Self {
        self.log = Some(log);
        self
    }

    #[cfg(windows)]
    fn stderr(&self) -> Stdio {
        self.log
            .as_ref()
            .and_then(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .ok()
            })
            .map(Stdio::from)
            .unwrap_or_else(Stdio::null)
    }

    #[cfg(not(windows))]
    fn stderr(&self) -> Stdio {
        Stdio::inherit()
    }

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
            #[cfg(windows)]
            log: None,
        })
    }

    /// Same worker with one more environment variable (login mode: DISPLAY, X11 only).
    pub fn with_env(mut self, k: &str, v: &str) -> Self {
        self.env.retain(|(key, _)| key != k);
        self.env.push((k.into(), v.into()));
        self
    }

    fn spawn(&self, mode: &str) -> Result<Child> {
        let mut c = Command::new(&self.exe);
        c.arg("--capture-worker")
            .arg(mode)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(self.stderr())
            .kill_on_drop(true);
        #[cfg(windows)]
        c.creation_flags(crate::winsys::CREATE_NO_WINDOW);
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

    /// A separate worker (`--capture-worker audio`): no GPU, and a monitor switch restarting
    /// the video worker doesn't interrupt the sound.
    fn open_audio(&self) -> BoxFuture<Result<AudioStream>> {
        let this = self.clone();
        Box::pin(async move {
            let mut child = this.spawn("audio")?;
            let mut stdout = child.stdout.take().context("worker stdout")?;
            let mut stdin = child.stdin.take().context("worker stdin")?;
            let first = tokio::time::timeout(
                AUDIO_START_TIMEOUT,
                read_frame::<_, WorkerEvent>(&mut stdout, MAX_WORKER_FRAME),
            )
            .await
            .context("audio worker did not start")?
            .map_err(|e| anyhow!("audio worker: {e}"))?;
            match first {
                WorkerEvent::AudioStarted => {}
                WorkerEvent::Failed(s) => bail!("{s}"),
                other => bail!("unexpected audio worker event {other:?}"),
            }
            let (tx, rx) = mpsc::channel::<AudioPacket>(16);
            let reader = tokio::spawn(async move {
                loop {
                    match read_frame::<_, WorkerEvent>(&mut stdout, MAX_WORKER_FRAME).await {
                        Ok(WorkerEvent::Audio { pts_us, data }) => {
                            let p = AudioPacket {
                                pts_us,
                                data: Bytes::from(data),
                            };
                            // Never queue audio behind a slow consumer: late sound is useless.
                            if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(p) {
                                break;
                            }
                        }
                        Ok(WorkerEvent::Failed(s)) => {
                            tracing::error!("audio: {s}");
                            break;
                        }
                        Ok(_) => {}
                        Err(FrameError::Closed) => break,
                        Err(e) => {
                            tracing::error!("audio worker stream: {e}");
                            break;
                        }
                    }
                }
            });
            let (cmd_tx, mut cmd_rx) = mpsc::channel::<WorkerCmd>(8);
            let writer = tokio::spawn(async move {
                while let Some(c) = cmd_rx.recv().await {
                    if write_frame(&mut stdin, &c, MAX_CMD_FRAME).await.is_err() {
                        break;
                    }
                }
            });
            Ok(AudioStream {
                packets: rx,
                control: Arc::new(WorkerAudioControl(cmd_tx)),
                _guard: Box::new(AudioGuard {
                    _child: child,
                    tasks: vec![reader, writer],
                }),
            })
        })
    }
}

struct WorkerAudioControl(mpsc::Sender<WorkerCmd>);

impl AudioControl for WorkerAudioControl {
    fn set_bitrate(&self, kbps: u32) {
        let _ = self.0.try_send(WorkerCmd::Bitrate(kbps));
    }
}

/// Kills the audio worker (kill_on_drop) and its pipe tasks.
struct AudioGuard {
    _child: Child,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for AudioGuard {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
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
