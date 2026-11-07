//! `dishus-daemon`: runs as a systemd user service.

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use dishus_daemon::{
    backend::{CaptureProvider, InputSink, NoCapture, NullInput, TrackedInput},
    config::Config,
    devices::DeviceStore,
    identity,
    ipc::{self, IpcContext},
    net::{NetOptions, spawn_router},
    notify::Notifier,
    paths::Paths,
    state::AppState,
};

fn usage() -> ! {
    eprintln!(
        "usage: dishus-daemon [--version]\n\nConfiguration: $XDG_CONFIG_HOME/dishus/config.toml\nControl it with the `dishus` CLI."
    );
    std::process::exit(2);
}

fn make_input() -> (Box<dyn InputSink>, String) {
    #[cfg(feature = "uinput")]
    {
        match dishus_daemon::input::UinputSink::new() {
            Ok(u) => return (Box::new(u), "uinput".into()),
            Err(e) => {
                tracing::error!(
                    "cannot open /dev/uinput ({e}); keyboard/mouse control disabled. \
                     See README: add yourself to the `input` group or install the udev rule."
                );
                return (Box::new(NullInput), format!("disabled ({e})"));
            }
        }
    }
    #[allow(unreachable_code)]
    (
        Box::new(NullInput),
        "disabled (built without uinput)".into(),
    )
}

fn make_capture(
    paths: &Paths,
    cfg: &Config,
) -> Option<dishus_daemon::capture::client::WorkerProvider> {
    match dishus_daemon::capture::client::WorkerProvider::new(
        paths.restore_token_file(),
        &cfg.video.encoder,
        cfg.video.show_cursor,
    ) {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::error!("capture disabled: {e:#}");
            None
        }
    }
}

#[cfg(feature = "capture")]
async fn capture_worker(mode: &str) -> Result<()> {
    use dishus_daemon::capture::{WorkerOptions, worker};
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_env("DISHUS_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    worker::run(mode, WorkerOptions::from_env()?).await
}

#[cfg(not(feature = "capture"))]
async fn capture_worker(_mode: &str) -> Result<()> {
    use dishus_daemon::capture::wire::WorkerEvent;
    let buf = dishus_proto::frame::encode_frame(
        &WorkerEvent::Failed("built without capture support".into()),
        4096,
    )?;
    std::io::Write::write_all(&mut std::io::stdout(), &buf)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    if let Some(a) = args.next() {
        match a.as_str() {
            "--version" | "-V" => {
                println!("dishus-daemon {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--capture-worker" => {
                rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
                let mode = args.next().unwrap_or_default();
                return capture_worker(&mode).await;
            }
            _ => usage(),
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("DISHUS_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info,iroh=warn,noq=warn,noq_proto=warn")),
        )
        .with_target(false)
        .init();

    // Nothing we create should be readable by others.
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));

    let paths = Paths::from_env()?;
    paths.ensure()?;
    let cfg = Config::load(&paths)?;
    let devices = DeviceStore::load(&paths)?;
    let key = identity::load_or_create(&paths)?;
    let (input, input_desc) = make_input();
    let worker = make_capture(&paths, &cfg);
    let capture: Arc<dyn CaptureProvider> = match &worker {
        Some(w) => Arc::new(w.clone()),
        None => Arc::new(NoCapture("capture worker unavailable".into())),
    };
    tracing::info!("input: {input_desc}");

    let notifier = Notifier::spawn();
    let net = NetOptions::from_config(&cfg)?;
    let state = Arc::new(AppState::new(
        paths,
        cfg,
        devices,
        TrackedInput::new(input),
        capture,
        notifier,
    ));
    let router = spawn_router(state.clone(), key, &net)
        .await
        .context("starting iroh endpoint")?;
    tracing::info!("PC id {}", router.endpoint().id());

    let ctx = Arc::new(IpcContext {
        state: state.clone(),
        endpoint: router.endpoint().clone(),
        capture_backend: std::sync::RwLock::new("probing…".into()),
        input_backend: input_desc,
    });
    if let Some(w) = worker {
        let ctx2 = ctx.clone();
        tokio::spawn(async move {
            let d = w.probe().await;
            tracing::info!("capture: {d}");
            *ctx2.capture_backend.write().expect("capture desc") = d;
        });
    }
    let ipc_task = tokio::spawn(ipc::serve(ctx));

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
        r = ipc_task => {
            match r {
                Ok(Err(e)) => tracing::error!("control socket failed: {e:#}"),
                Err(e) => tracing::error!("control socket task: {e}"),
                Ok(Ok(())) => {}
            }
        }
    }
    tracing::info!("shutting down");
    state.end_all_sessions(dishus_proto::ByeReason::Normal);
    state.release_input();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), router.shutdown()).await;
    let _ = std::fs::remove_file(state.paths.socket());
    Ok(())
}
