//! Local control socket for the `dishus` CLI.

use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use iroh::Endpoint;
use tokio::{net::UnixListener, time::timeout};

use dishus_proto::{
    PairingInfo, read_frame, write_frame,
    ipc::{IpcDevice, IpcRequest, IpcResponse, IpcSession, IpcStatus, MAX_IPC_FRAME},
};

use crate::{net::pairing_info, state::AppState};

pub struct IpcContext {
    pub state: Arc<AppState>,
    pub endpoint: Endpoint,
    pub capture_backend: String,
    pub input_backend: String,
}

pub async fn serve(ctx: Arc<IpcContext>) -> Result<()> {
    let path = ctx.state.paths.socket();
    // A stale socket from a crashed daemon: only remove it if nothing answers.
    if path.exists() {
        if tokio::net::UnixStream::connect(&path).await.is_ok() {
            bail!("another dishus-daemon is already running ({})", path.display());
        }
        std::fs::remove_file(&path).ok();
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let my_uid = rustix::process::getuid().as_raw();
    loop {
        let (mut stream, _) = listener.accept().await?;
        match stream.peer_cred() {
            Ok(c) if c.uid() == my_uid => {}
            other => {
                tracing::warn!("rejected control-socket client: {other:?}");
                continue;
            }
        }
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let res: Result<()> = async {
                let req: IpcRequest =
                    timeout(Duration::from_secs(5), read_frame(&mut stream, MAX_IPC_FRAME)).await??;
                let resp = handle(&ctx, req).await;
                write_frame(&mut stream, &resp, MAX_IPC_FRAME).await?;
                Ok(())
            }
            .await;
            if let Err(e) = res {
                tracing::debug!("ipc: {e:#}");
            }
        });
    }
}

fn err(e: impl std::fmt::Display) -> IpcResponse {
    IpcResponse::Error(e.to_string())
}

async fn handle(ctx: &IpcContext, req: IpcRequest) -> IpcResponse {
    let st = &ctx.state;
    match req {
        IpcRequest::Status => {
            let addr = ctx.endpoint.addr();
            let sessions = st
                .sessions()
                .into_iter()
                .map(|s| IpcSession { id: s.peer.to_string(), name: s.name, since: s.since, path: s.path })
                .collect();
            IpcResponse::Status(IpcStatus {
                version: env!("CARGO_PKG_VERSION").into(),
                pc_name: st.config.pc_name.clone(),
                endpoint_id: ctx.endpoint.id().to_string(),
                relays: addr.relay_urls().map(|u| u.to_string()).collect(),
                direct_addrs: addr.ip_addrs().map(|a| a.to_string()).collect(),
                lockdown: st.lockdown.load(std::sync::atomic::Ordering::SeqCst),
                pairing_open: st.is_pairing_open(),
                sessions,
                devices: st.devices.read().expect("devices").list().len(),
                capture_backend: ctx.capture_backend.clone(),
                input_backend: ctx.input_backend.clone(),
            })
        }
        IpcRequest::List => {
            let connected: Vec<String> = st.sessions().iter().map(|s| s.peer.to_string()).collect();
            let devs = st
                .devices
                .read()
                .expect("devices")
                .list()
                .iter()
                .map(|d| IpcDevice {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    paired_at: d.paired_at,
                    expires_at: d.expires_at,
                    last_seen: d.last_seen,
                    connected: connected.contains(&d.id),
                })
                .collect();
            IpcResponse::Devices(devs)
        }
        IpcRequest::Pair => match st.open_pairing() {
            Ok((token, ttl)) => {
                // Make sure we have a relay/addresses to put in the link.
                let _ = timeout(Duration::from_secs(5), ctx.endpoint.online()).await;
                let info: PairingInfo = pairing_info(&ctx.endpoint, st, *token);
                st.notifier.notify("DiShus: pairing open", &format!("for {} seconds", ttl.as_secs()));
                IpcResponse::Pairing { uri: info.to_uri(), ttl_secs: ttl.as_secs() }
            }
            Err(e) => err(e),
        },
        IpcRequest::ClosePairing => {
            st.close_pairing();
            IpcResponse::Ok("pairing closed".into())
        }
        IpcRequest::Revoke { selector } => match st.revoke(&selector) {
            Ok(d) => IpcResponse::Ok(format!("revoked {} ({})", d.name, &d.id[..12])),
            Err(e) => err(e),
        },
        IpcRequest::Renew { selector, days } => match st.renew_selector(&selector, days) {
            Ok((d, exp)) => IpcResponse::Ok(format!("renewed {} until unix {exp}", d.name)),
            Err(e) => err(e),
        },
        IpcRequest::Kill => match st.kill() {
            Ok(n) => IpcResponse::Ok(format!("killed {n} session(s); locked down until `dishus unlock`")),
            Err(e) => err(format!("sessions cut, but persisting lockdown failed: {e:#}")),
        },
        IpcRequest::Unlock => match st.unlock() {
            Ok(()) => IpcResponse::Ok("unlocked".into()),
            Err(e) => err(e),
        },
        IpcRequest::SetupCapture => match st.capture.setup_interactive().await {
            Ok(s) => IpcResponse::Ok(s),
            Err(e) => err(format!("{e:#}")),
        },
    }
}
