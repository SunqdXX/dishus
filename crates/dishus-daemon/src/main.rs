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

type Status = std::sync::Arc<std::sync::Mutex<String>>;

fn fixed(desc: String) -> Status {
    std::sync::Arc::new(std::sync::Mutex::new(desc))
}

/// The input sink plus a live description for `dishus status`. `true` = it may become ready
/// later (retrying), so the caller should keep poking `ready()`.
fn make_input() -> (Box<dyn InputSink>, Status, bool) {
    // Test hook: `DISHUS_INPUT=record:<file>` logs input instead of injecting it.
    if let Some(path) = std::env::var("DISHUS_INPUT")
        .ok()
        .and_then(|v| v.strip_prefix("record:").map(std::path::PathBuf::from))
    {
        match dishus_daemon::backend::RecordInput::open(&path) {
            Ok(r) => {
                return (
                    Box::new(r),
                    fixed(format!("record:{}", path.display())),
                    false,
                );
            }
            Err(e) => tracing::error!("DISHUS_INPUT record file {}: {e}", path.display()),
        }
    }
    #[cfg(feature = "uinput")]
    {
        // Retry: at boot logind may grant /dev/uinput a moment after this service starts.
        let r = dishus_daemon::backend::RetryingInput::new(
            "/dev/uinput",
            Box::new(|| {
                dishus_daemon::input::UinputSink::new().map(|u| Box::new(u) as Box<dyn InputSink>)
            }),
        );
        let status = r.status.clone();
        return (Box::new(r), status, true);
    }
    #[allow(unreachable_code)]
    (
        Box::new(NullInput),
        fixed("disabled (built without uinput)".into()),
        false,
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
    // Before touching any state: a previous instance may still be shutting down (restart),
    // and two daemons must never share the control socket or write devices.toml together.
    let _instance = paths.lock_instance(std::time::Duration::from_secs(10))?;
    let cfg = Config::load(&paths)?;
    let devices = DeviceStore::load(&paths)?;
    let key = identity::load_or_create(&paths)?;
    let (input, input_status, input_retrying) = make_input();
    let worker = make_capture(&paths, &cfg);
    let capture: Arc<dyn CaptureProvider> = match &worker {
        Some(w) => Arc::new(w.clone()),
        None => Arc::new(NoCapture("capture worker unavailable".into())),
    };
    tracing::info!("input: {}", input_status.lock().expect("input status"));

    let notifier = Notifier::spawn();
    let net = NetOptions::from_config(&cfg, &paths)?;
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
    if input_retrying && !state.input.lock().expect("input lock").ready() {
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                if st.input.lock().expect("input lock").ready() {
                    break;
                }
            }
        });
    }

    let ctx = Arc::new(IpcContext {
        state: state.clone(),
        endpoint: router.endpoint().clone(),
        capture_backend: std::sync::RwLock::new("probing…".into()),
        input_backend: input_status,
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
