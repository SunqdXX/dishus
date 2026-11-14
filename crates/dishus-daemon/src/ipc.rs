//! Local control socket for the `dishus` CLI (a named pipe on Windows).

#[cfg(not(windows))]
use std::os::unix::fs::PermissionsExt;
use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use iroh::Endpoint;
#[cfg(not(windows))]
use tokio::net::UnixListener;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::timeout,
};

use dishus_proto::{
    PairingInfo,
    ipc::{IpcDevice, IpcRequest, IpcResponse, IpcSession, IpcStatus, MAX_IPC_FRAME},
    read_frame, write_frame,
};

use crate::{net::pairing_info, state::AppState};

pub struct IpcContext {
    pub state: Arc<AppState>,
    pub endpoint: Endpoint,
    pub capture_backend: std::sync::RwLock<String>,
    /// Live: the input sink may become available after startup (see `RetryingInput`).
    pub input_backend: std::sync::Arc<std::sync::Mutex<String>>,
}

#[cfg(not(windows))]
pub async fn serve(ctx: Arc<IpcContext>) -> Result<()> {
    let path = ctx.state.paths.socket();
    // We hold the single-instance lock (main.rs), so any socket file here is stale.
    if path.exists() {
        std::fs::remove_file(&path).ok();
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    let login_mode = ctx.state.boot.is_some();
    // Login mode runs as its own user: root and the PC's owner must reach it. Who may do
    // what is checked per request (`authorized`).
    let mode = if login_mode { 0o666 } else { 0o600 };
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
    let my_uid = rustix::process::getuid().as_raw();
    let owner = ctx.state.boot.as_ref().and_then(|b| b.owner_uid);
    loop {
        let (stream, _) = listener.accept().await?;
        let caller = match stream.peer_cred() {
            Ok(c) => classify(c.uid(), my_uid, login_mode, owner),
            Err(e) => {
                tracing::warn!("control socket: no peer credentials: {e}");
                None
            }
        };
        let Some(caller) = caller else {
            tracing::warn!("rejected control-socket client: {:?}", stream.peer_cred());
            continue;
        };
        spawn_client(ctx.clone(), stream, caller);
    }
}

/// Windows: a named pipe whose DACL only admits the current user, and whose clients' process
/// SID is checked again on connect (the counterpart of the uid check above).
#[cfg(windows)]
pub async fn serve(ctx: Arc<IpcContext>) -> Result<()> {
    let name = dishus_win::pipe_name()?;
    let my_sid = dishus_win::current_user_sid()?;
    // First instance: fails if anyone else already holds this name.
    let mut server = dishus_win::create_pipe_server(&name, true)
        .with_context(|| format!("creating control pipe {name}"))?;
    loop {
        server.connect().await?;
        let stream = server;
        server = dishus_win::create_pipe_server(&name, false)
            .with_context(|| format!("creating control pipe {name}"))?;
        match dishus_win::pipe_client_sid(&stream) {
            Ok(sid) if sid == my_sid => {}
            other => {
                tracing::warn!("rejected control-pipe client: {other:?}");
                continue;
            }
        }
        spawn_client(ctx.clone(), stream, Caller::Same);
    }
}

/// Who is on the other end of the control socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// The daemon's own user (the only caller outside login mode).
    Same,
    /// Login mode: root (sudo).
    Root,
    /// Login mode: the PC's owner, without sudo.
    Owner,
}

/// Map a peer uid to a caller; None = refused.
pub fn classify(uid: u32, my_uid: u32, login_mode: bool, owner: Option<u32>) -> Option<Caller> {
    if uid == my_uid {
        Some(Caller::Same)
    } else if !login_mode {
        None
    } else if uid == 0 {
        Some(Caller::Root)
    } else if Some(uid) == owner {
        Some(Caller::Owner)
    } else {
        None
    }
}

/// Login mode: the owner may look, revoke, kill and unlock, never add or renew a phone.
pub fn authorized(caller: Caller, req: &IpcRequest) -> bool {
    match caller {
        Caller::Same | Caller::Root => true,
        Caller::Owner => matches!(
            req,
            IpcRequest::Status
                | IpcRequest::List
                | IpcRequest::Revoke { .. }
                | IpcRequest::Kill
                | IpcRequest::Unlock
        ),
    }
}

fn spawn_client<S>(ctx: Arc<IpcContext>, mut stream: S, caller: Caller)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let res: Result<()> = async {
            let req: IpcRequest = timeout(
                Duration::from_secs(5),
                read_frame(&mut stream, MAX_IPC_FRAME),
            )
            .await??;
            let resp = if authorized(caller, &req) {
                handle(&ctx, req).await
            } else {
                tracing::warn!("control socket: {caller:?} may not {req:?}");
                IpcResponse::Error("not allowed: run it with sudo".into())
            };
            write_frame(&mut stream, &resp, MAX_IPC_FRAME).await?;
            Ok(())
        }
        .await;
        if let Err(e) = res {
            tracing::debug!("ipc: {e:#}");
        }
    });
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
                .map(|s| IpcSession {
                    id: s.peer.to_string(),
                    name: s.name,
                    since: s.since,
                    path: s.path,
                })
                .collect();
            IpcResponse::Status(IpcStatus {
                version: dishus_proto::VERSION.into(),
                pc_name: st.config.pc_name.clone(),
                endpoint_id: ctx.endpoint.id().to_string(),
                relays: addr.relay_urls().map(|u| u.to_string()).collect(),
                direct_addrs: addr.ip_addrs().map(|a| a.to_string()).collect(),
                lockdown: st.lockdown.load(std::sync::atomic::Ordering::SeqCst),
                pairing_open: st.is_pairing_open(),
                sessions,
                devices: st.devices.read().expect("devices").list().len(),
                capture_backend: ctx.capture_backend.read().expect("capture desc").clone(),
                input_backend: ctx.input_backend.lock().expect("input status").clone(),
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
                st.notifier.notify(
                    "DiShus: pairing open",
                    &format!("for {} seconds", ttl.as_secs()),
                );
                IpcResponse::Pairing {
                    uri: info.to_uri(),
                    ttl_secs: ttl.as_secs(),
                }
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
            Ok(n) => IpcResponse::Ok(format!(
                "killed {n} session(s); locked down until `dishus unlock`"
            )),
            Err(e) => err(format!(
                "sessions cut, but persisting lockdown failed: {e:#}"
            )),
        },
        IpcRequest::Unlock => match st.unlock() {
            Ok(()) => IpcResponse::Ok("unlocked".into()),
            Err(e) => err(e),
        },
        IpcRequest::SetupCapture => match st.capture.setup_interactive().await {
            Ok(s) => IpcResponse::Ok(s),
            Err(e) => err(format!("{e:#}")),
        },
        IpcRequest::LoginEnroll => match st.open_login_enroll() {
            Ok((token, ttl)) => {
                let _ = timeout(Duration::from_secs(5), ctx.endpoint.online()).await;
                let info: PairingInfo = pairing_info(&ctx.endpoint, st, *token);
                IpcResponse::Pairing {
                    uri: info.to_uri(),
                    ttl_secs: ttl.as_secs(),
                }
            }
            Err(e) => err(format!("{e:#}")),
        },
        IpcRequest::LoginEnrolled => {
            IpcResponse::Enrolled(st.take_enrolled().map(|(id, name)| (id.to_string(), name)))
        }
        IpcRequest::Allow { id, name, days } => {
            let Ok(peer) = id.parse::<iroh::EndpointId>() else {
                return err("not a phone id (see `dishus list` in your session)");
            };
            match st.allow_device(&peer, &name, days) {
                Ok(exp) => {
                    IpcResponse::Ok(format!("{name} may use the login screen until unix {exp}"))
                }
                Err(e) => err(format!("{e:#}")),
            }
        }
    }
}

#[cfg(test)]
mod caller_tests {
    use super::*;

    #[test]
    fn who_may_do_what() {
        // normal daemon: only its own user
        assert_eq!(classify(1000, 1000, false, None), Some(Caller::Same));
        assert_eq!(classify(0, 1000, false, None), None);
        // login mode (runs as 970): root, the owner, nobody else
        assert_eq!(classify(0, 970, true, Some(1000)), Some(Caller::Root));
        assert_eq!(classify(1000, 970, true, Some(1000)), Some(Caller::Owner));
        assert_eq!(classify(1001, 970, true, Some(1000)), None);
        assert_eq!(classify(1000, 970, true, None), None);
        let allow = IpcRequest::Allow {
            id: "x".into(),
            name: "p".into(),
            days: 30,
        };
        assert!(authorized(Caller::Root, &allow));
        assert!(!authorized(Caller::Owner, &allow));
        assert!(!authorized(
            Caller::Owner,
            &IpcRequest::Renew {
                selector: "p".into(),
                days: 30
            }
        ));
        assert!(!authorized(Caller::Owner, &IpcRequest::Pair));
        for ok in [
            IpcRequest::Status,
            IpcRequest::List,
            IpcRequest::Kill,
            IpcRequest::Unlock,
        ] {
            assert!(authorized(Caller::Owner, &ok));
        }
        assert!(authorized(
            Caller::Owner,
            &IpcRequest::Revoke {
                selector: "p".into()
            }
        ));
    }
}
