//! iroh endpoint + protocol handlers. The PC never opens a listening TCP port; it binds UDP
//! sockets for QUIC and is reachable by EndpointId through hole-punching or a relay.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use iroh::{
    Endpoint, RelayMode, SecretKey,
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use tokio::time::timeout;

use dishus_proto::{ALPN_PAIR, ALPN_SESSION, PairRequest, PairingInfo, read_frame, write_frame};

use crate::{
    gate::{Gate, incoming_filter},
    session,
    state::AppState,
};

#[derive(Debug, Clone)]
pub struct NetOptions {
    pub relay_mode: RelayMode,
    /// Publish/resolve addresses via n0's DNS (pkarr). Off in tests.
    pub n0_lookup: bool,
    pub bind_addr: Option<SocketAddr>,
}

impl NetOptions {
    pub fn from_config(cfg: &crate::config::Config) -> Result<Self> {
        Ok(Self {
            relay_mode: cfg.relay_mode()?,
            n0_lookup: true,
            bind_addr: (cfg.udp_port != 0).then(|| SocketAddr::from(([0, 0, 0, 0], cfg.udp_port))),
        })
    }

    /// Loopback only, no relays, no DNS: for tests.
    pub fn local_test() -> Self {
        Self {
            relay_mode: RelayMode::Disabled,
            n0_lookup: false,
            bind_addr: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        }
    }
}

pub async fn bind_endpoint(
    secret_key: SecretKey,
    opts: &NetOptions,
    hooks: Option<Gate>,
) -> Result<Endpoint> {
    let mut b = if opts.n0_lookup {
        Endpoint::builder(presets::N0)
    } else {
        Endpoint::builder(presets::Minimal)
    };
    b = b.secret_key(secret_key).relay_mode(opts.relay_mode.clone());
    if let Some(a) = opts.bind_addr {
        b = b
            .clear_ip_transports()
            .bind_addr(a)
            .context("bind address")?;
    }
    if let Some(h) = hooks {
        b = b.hooks(h);
    }
    Ok(b.bind().await?)
}

pub async fn spawn_router(
    state: Arc<AppState>,
    secret_key: SecretKey,
    opts: &NetOptions,
) -> Result<Router> {
    let ep = bind_endpoint(secret_key, opts, Some(Gate(state.clone()))).await?;
    Ok(Router::builder(ep)
        .incoming_filter(incoming_filter(state.clone()))
        .accept(ALPN_SESSION, SessionHandler(state.clone()))
        .accept(ALPN_PAIR, PairHandler(state))
        .spawn())
}

/// Build the pairing link for the QR code.
pub fn pairing_info(ep: &Endpoint, state: &AppState, token: [u8; 32]) -> PairingInfo {
    let addr = ep.addr();
    PairingInfo {
        endpoint_id: *ep.id().as_bytes(),
        token,
        pc_name: state.config.pc_name.clone(),
        relays: addr.relay_urls().take(4).map(|u| u.to_string()).collect(),
        addrs: addr
            .ip_addrs()
            .filter(|a| !a.ip().is_loopback())
            .take(6)
            .map(|a| a.to_string())
            .collect(),
    }
}

#[derive(Clone)]
struct SessionHandler(Arc<AppState>);
#[derive(Clone)]
struct PairHandler(Arc<AppState>);

impl std::fmt::Debug for SessionHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionHandler")
    }
}
impl std::fmt::Debug for PairHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairHandler")
    }
}

impl ProtocolHandler for SessionHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        if let Err(e) = session::handle(self.0.clone(), conn).await {
            tracing::debug!("session: {e:#}");
        }
        Ok(())
    }
}

/// Max size of a pairing request on the wire.
const PAIR_FRAME_CAP: usize = 512;

impl ProtocolHandler for PairHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let peer = conn.remote_id();
        let res: Result<()> = async {
            let (mut send, mut recv) = timeout(Duration::from_secs(10), conn.accept_bi()).await??;
            let req: PairRequest = timeout(
                Duration::from_secs(10),
                read_frame(&mut recv, PAIR_FRAME_CAP),
            )
            .await??;
            let resp = self.0.try_pair(&peer, &req);
            drop(req);
            write_frame(&mut send, &resp, PAIR_FRAME_CAP).await?;
            send.finish()?;
            // give the response a moment to arrive before tearing down
            let _ = timeout(Duration::from_secs(3), conn.closed()).await;
            Ok(())
        }
        .await;
        if let Err(e) = res {
            tracing::debug!("pairing connection from {}: {e:#}", peer.fmt_short());
        }
        conn.close(0u32.into(), b"");
        Ok(())
    }
}
