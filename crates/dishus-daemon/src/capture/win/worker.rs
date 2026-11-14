//! The capture worker process on Windows
//! (`dishus-daemon --capture-worker <session|audio|setup|probe|dupcheck>`).
//!
//! Same contract as the Linux worker: stdout carries framed [`WorkerEvent`]s, stdin carries
//! framed [`WorkerCmd`]s, logs go to stderr (the daemon points it at its log file). The worker
//! exits when stdin closes, i.e. when the daemon drops it.

use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};

use dishus_proto::{FrameError, MonitorInfo, frame::encode_frame, read_frame};

use super::{
    dxgi, mf,
    pipeline::{self, Params, Running},
};
use crate::capture::{
    WorkerOptions,
    wire::{MAX_CMD_FRAME, MAX_WORKER_FRAME, WorkerCmd, WorkerEvent},
};

/// Serialised writes to stdout (capture thread + main task).
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

fn monitors(opts: &WorkerOptions) -> Result<Vec<MonitorInfo>> {
    if opts.test_source {
        return Ok(vec![MonitorInfo {
            index: 0,
            name: "Test pattern".into(),
            width: 1920,
            height: 1080,
        }]);
    }
    Ok(dxgi::outputs()?
        .into_iter()
        .take(dishus_proto::MAX_MONITORS)
        .enumerate()
        .map(|(i, o)| MonitorInfo {
            index: i as u8,
            name: o.name,
            width: o.width,
            height: o.height,
        })
        .collect())
}

/// `encoders: qsv > software` for the primary monitor's GPU.
fn encoder_summary(opts: &WorkerOptions) -> Result<String> {
    mf::available()?;
    let outs = dxgi::outputs()?;
    let Some(o) = outs.first() else {
        anyhow::bail!("no monitor found");
    };
    let names: Vec<String> = mf::list(o.luid, o.vendor_id)?
        .into_iter()
        .filter(|e| e.matches(&opts.encoder_pref))
        .map(|e| e.label)
        .collect();
    Ok(if names.is_empty() {
        "NONE FOUND".into()
    } else {
        names.join(" > ")
    })
}

pub async fn run(mode: &str, opts: WorkerOptions) -> Result<()> {
    let _com = pipeline::Com::init();
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));
    crate::winsys::set_dpi_aware();
    match mode {
        "probe" => {
            let ev = match encoder_summary(&opts) {
                Ok(s) => WorkerEvent::Info(format!(
                    "{}; encoders: {s}",
                    if opts.test_source {
                        "test pattern"
                    } else {
                        "desktop duplication"
                    }
                )),
                Err(e) => WorkerEvent::Failed(format!("{e:#}")),
            };
            out.send(&ev);
            Ok(())
        }
        "setup" => {
            // Desktop Duplication needs no permission; check that it works on every monitor.
            let ev = match check_capture() {
                Ok(n) => WorkerEvent::Info(format!(
                    "screen capture works on {n} monitor(s); Windows asks no permission for it"
                )),
                Err(e) => WorkerEvent::Failed(format!("{e:#}")),
            };
            out.send(&ev);
            Ok(())
        }
        // Exit code only, for gpupref::ensure: 0 works, 3 unsupported from this GPU, 1 other.
        "dupcheck" => match check_capture() {
            Ok(_) => std::process::exit(0),
            Err(e) => {
                let unsupported = e
                    .chain()
                    .filter_map(|c| c.downcast_ref::<windows::core::Error>())
                    .any(|w| w.code() == windows::Win32::Graphics::Dxgi::DXGI_ERROR_UNSUPPORTED);
                eprintln!("{e:#}");
                std::process::exit(if unsupported {
                    super::gpupref::EXIT_UNSUPPORTED
                } else {
                    1
                })
            }
        },
        "session" => session(opts, out).await,
        "audio" => {
            let test = opts.test_source;
            crate::capture::audio_worker::run(
                Arc::new(move |ev: &WorkerEvent| out.send(ev)),
                test,
                move |kbps, on_packet, on_fail| {
                    super::audio::start_audio(test, kbps, on_packet, on_fail)
                },
            )
            .await
        }
        other => anyhow::bail!("unknown worker mode {other}"),
    }
}

/// Open Desktop Duplication on every monitor once.
pub fn check_capture() -> Result<usize> {
    let outs = dxgi::outputs()?;
    if outs.is_empty() {
        anyhow::bail!("no monitor found");
    }
    for o in &outs {
        let (dev, _) = dxgi::create_device(&o.adapter)?;
        dxgi::Duplicator::new(&o.output, &dev).with_context(|| o.name.clone())?;
    }
    Ok(outs.len())
}

async fn session(opts: WorkerOptions, out: Out) -> Result<()> {
    let mons = match monitors(&opts).and_then(|m| mf::available().map(|_| m)) {
        Ok(m) if !m.is_empty() => m,
        Ok(_) => {
            out.send(&WorkerEvent::Failed("no monitor found".into()));
            return Ok(());
        }
        Err(e) => {
            out.send(&WorkerEvent::Failed(format!("{e:#}")));
            return Ok(());
        }
    };
    if !out.send(&WorkerEvent::Monitors(mons)) {
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
                running = None; // stop the previous stream first
                let p = Params {
                    monitor,
                    fps,
                    bitrate_kbps,
                    max_height,
                    show_cursor: opts.show_cursor,
                    test_source: opts.test_source,
                    encoder_pref: opts.encoder_pref.clone(),
                    encoder_name: None,
                    adapter: None,
                };
                let o1 = out.clone();
                let o2 = out.clone();
                // Starting blocks until the first frame is encoded: keep it off the runtime.
                let started = tokio::task::spawn_blocking(move || {
                    pipeline::start(
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
                })
                .await?;
                match started {
                    Ok(r) => {
                        tracing::info!(
                            "worker: streaming monitor {monitor} at {}x{} via {}",
                            r.width,
                            r.height,
                            r.encoder
                        );
                        out.send(&WorkerEvent::Started {
                            width: r.width,
                            height: r.height,
                            encoder: r.encoder.clone(),
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
    Ok(())
}
