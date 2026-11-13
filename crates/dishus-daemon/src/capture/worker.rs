//! The capture worker process (`dishus-daemon --capture-worker <session|setup|probe>`).
//!
//! stdout carries framed [`WorkerEvent`]s, stdin carries framed [`WorkerCmd`]s. Logs go to
//! stderr (the journal). The worker exits when stdin closes, i.e. when the daemon drops it.

use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use anyhow::Result;

use dishus_proto::{FrameError, MonitorInfo, frame::encode_frame, read_frame};

use super::{
    WorkerOptions,
    pipeline::{self, Params, Running, Source},
    portal::{self, PortalError, PortalSession},
    wire::{MAX_CMD_FRAME, MAX_WORKER_FRAME, WorkerCmd, WorkerEvent},
};

/// Serialised writes to stdout (pipeline thread + main task).
#[derive(Clone)]
struct Out(Arc<Mutex<std::io::Stdout>>);

impl Out {
    fn send(&self, ev: &WorkerEvent) -> bool {
        let Ok(buf) = encode_frame(ev, MAX_WORKER_FRAME) else {
            return false;
        };
        let mut g = self.0.lock().expect("stdout lock");
        g.write_all(&buf).and_then(|_| g.flush()).is_ok()
    }
}

enum Capture {
    Portal(PortalSession),
    X11,
    Test,
}

impl Capture {
    fn monitors(&self) -> Vec<MonitorInfo> {
        match self {
            Capture::Portal(p) => p.monitors(),
            Capture::X11 => vec![MonitorInfo {
                index: 0,
                name: "X11 screen".into(),
                width: 0,
                height: 0,
            }],
            Capture::Test => vec![MonitorInfo {
                index: 0,
                name: "Test pattern".into(),
                width: 1920,
                height: 1080,
            }],
        }
    }
    fn source(&self, idx: u8) -> Result<(Source, Option<(u32, u32)>)> {
        match self {
            Capture::Portal(p) => p.source(idx),
            Capture::X11 => Ok((Source::X11, None)),
            Capture::Test => Ok((Source::Test, Some((1920, 1080)))),
        }
    }
}

async fn open_capture(opts: &WorkerOptions) -> Result<Capture, String> {
    if opts.test_source {
        return Ok(Capture::Test);
    }
    match portal::open(opts, false).await {
        Ok(p) => return Ok(Capture::Portal(p)),
        Err(PortalError::Unavailable(e)) => {
            tracing::info!("portal ScreenCast unavailable ({e}); trying X11")
        }
        Err(PortalError::Other(e)) => return Err(format!("{e:#}")),
    }
    if std::env::var_os("DISPLAY").is_some() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Ok(Capture::X11);
    }
    Err("no screen capture backend available (no portal ScreenCast, no X11 display)".into())
}

pub async fn run(mode: &str, opts: WorkerOptions) -> Result<()> {
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));
    if let Err(e) = pipeline::init() {
        out.send(&WorkerEvent::Failed(format!("{e:#}")));
        return Ok(());
    }
    let encoders = pipeline::candidates(&opts.encoder_pref);
    match mode {
        "probe" => {
            let names: Vec<_> = encoders.iter().map(|e| e.name()).collect();
            let src = if opts.test_source {
                "test pattern"
            } else {
                "portal/pipewire (x11 fallback)"
            };
            out.send(&WorkerEvent::Info(format!(
                "{src}; encoders: {}",
                if names.is_empty() {
                    "NONE FOUND".into()
                } else {
                    names.join(" > ")
                }
            )));
            Ok(())
        }
        "setup" => {
            let ev = match portal::open(&opts, true).await {
                Ok(s) if s.token_stored => WorkerEvent::Info(format!(
                    "screen-capture permission stored for {} monitor(s)",
                    s.monitors().len()
                )),
                Ok(_) => WorkerEvent::Failed(
                    "capture was allowed, but NOT remembered: the picker returned no restore \
                     token, so the phone could not start capture without someone at the PC. Run \
                     `dishus setup-capture` again and turn on the picker's remember / \
                     allow-restore-token option."
                        .into(),
                ),
                Err(PortalError::Unavailable(e)) if std::env::var_os("DISPLAY").is_some() => {
                    WorkerEvent::Info(format!("no portal ({e}); X11 capture needs no permission"))
                }
                Err(PortalError::Unavailable(e)) => {
                    WorkerEvent::Failed(format!("portal ScreenCast unavailable: {e}"))
                }
                Err(PortalError::Other(e)) => WorkerEvent::Failed(format!("{e:#}")),
            };
            out.send(&ev);
            Ok(())
        }
        "session" => session(opts, out, encoders).await,
        "audio" => audio(opts, out).await,
        other => anyhow::bail!("unknown worker mode {other}"),
    }
}

/// Audio worker: Opus packets out until stdin closes. No portal: PipeWire lets the user's own
/// processes record the output's monitor. Commands: `Bitrate` (kbit/s); anything else ignored.
///
/// pipewiresrc marks its stream `node.dont-reconnect`, so when WirePlumber re-links streams
/// (seen on a real PC: "target not found", twice in two minutes) the capture just dies. It is
/// restarted here (a gap of a few hundred ms); only repeated failures end the worker.
async fn audio(opts: WorkerOptions, out: Out) -> Result<()> {
    const MAX_FAILS: usize = 5;
    const FAIL_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);
    let (fail_tx, mut fail_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut kbps = crate::audio::HIGH_KBPS;
    let start = |kbps: u32| {
        let o = out.clone();
        let f = fail_tx.clone();
        pipeline::start_audio(
            opts.test_source,
            kbps,
            Arc::new(move |p: pipeline::AudioPacket| {
                o.send(&WorkerEvent::Audio {
                    pts_us: p.pts_us,
                    data: p.data,
                });
            }),
            Arc::new(move |why: String| {
                let _ = f.send(why);
            }),
        )
    };
    let mut running = match start(kbps) {
        Ok(r) => Some(r),
        Err(e) => {
            out.send(&WorkerEvent::Failed(format!("{e:#}")));
            return Ok(());
        }
    };
    if !out.send(&WorkerEvent::AudioStarted) {
        return Ok(());
    }
    // Test hook (test tone only): report a capture failure every N ms, to exercise the restart.
    if opts.test_source
        && let Some(ms) = std::env::var("DISHUS_TEST_AUDIO_FAIL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
    {
        let f = fail_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                if f.send("test failure".into()).is_err() {
                    break;
                }
            }
        });
    }
    let mut fails: std::collections::VecDeque<std::time::Instant> = Default::default();
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
                let now = std::time::Instant::now();
                fails.retain(|t| now.duration_since(*t) < FAIL_WINDOW);
                fails.push_back(now);
                if fails.len() >= MAX_FAILS {
                    out.send(&WorkerEvent::Failed(format!("{why} ({MAX_FAILS} times in 30 s)")));
                    break;
                }
                tracing::warn!("audio capture stopped ({why}); restarting");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                while fail_rx.try_recv().is_ok() {} // stale reports from the old pipeline
                match start(kbps) {
                    Ok(r) => running = Some(r),
                    Err(e) => {
                        out.send(&WorkerEvent::Failed(format!("{e:#}")));
                        break;
                    }
                }
            }
        }
    }
    drop(running);
    Ok(())
}

async fn session(opts: WorkerOptions, out: Out, encoders: Vec<pipeline::Encoder>) -> Result<()> {
    let capture = match open_capture(&opts).await {
        Ok(c) => c,
        Err(e) => {
            out.send(&WorkerEvent::Failed(e));
            return Ok(());
        }
    };
    if !out.send(&WorkerEvent::Monitors(capture.monitors())) {
        return Ok(());
    }
    let mut stdin = tokio::io::stdin();
    let mut running: Option<Running> = None;
    loop {
        let cmd: WorkerCmd = match read_frame(&mut stdin, MAX_CMD_FRAME).await {
            Ok(c) => c,
            Err(FrameError::Closed) => break,
            Err(e) => {
                tracing::error!("worker: bad command: {e}");
                break;
            }
        };
        match cmd {
            WorkerCmd::Start {
                monitor,
                fps,
                bitrate_kbps,
                max_height,
            } => {
                running = None; // stop the previous pipeline first
                let started = capture.source(monitor).and_then(|(src, src_size)| {
                    let p = Params {
                        fps,
                        bitrate_kbps,
                        max_height,
                        src_size,
                        show_cursor: opts.show_cursor,
                    };
                    let o1 = out.clone();
                    let o2 = out.clone();
                    pipeline::start(
                        &src,
                        &encoders,
                        &p,
                        Arc::new(move |f: pipeline::Frame| {
                            o1.send(&WorkerEvent::Frame {
                                pts_us: f.pts_us,
                                keyframe: f.keyframe,
                                data: f.data,
                            });
                        }),
                        Arc::new(move |why: String| {
                            o2.send(&WorkerEvent::Failed(why));
                        }),
                    )
                });
                match started {
                    Ok(r) => {
                        out.send(&WorkerEvent::Started {
                            width: r.width,
                            height: r.height,
                            encoder: r.encoder.name().into(),
                        });
                        r.control.force_keyframe();
                        running = Some(r);
                    }
                    Err(e) => {
                        out.send(&WorkerEvent::Failed(format!("{e:#}")));
                    }
                }
            }
            WorkerCmd::Stop => running = None,
            WorkerCmd::Keyframe => {
                if let Some(r) = &running {
                    r.control.force_keyframe();
                }
            }
            WorkerCmd::Bitrate(k) => {
                if let Some(r) = &running {
                    r.control.set_bitrate(k);
                }
            }
            WorkerCmd::Fps(f) => {
                if let Some(r) = &running {
                    r.control.set_fps(f);
                }
            }
        }
    }
    drop(running);
    drop(capture);
    Ok(())
}
