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

use dishus_proto::{
    ALPN_PAIR, ALPN_SESSION, ALPN_SESSION_V2, ALPN_SESSION_V3, PairRequest, PairingInfo,
    read_frame, write_frame,
};

use crate::{
    gate::{Gate, incoming_filter},
    session,
    state::AppState,
};

#[derive(Debug, Clone)]
pub enum Bind {
    /// Loopback, random port (tests).
    Loopback,
    /// IPv4 + IPv6 on all interfaces. `port` 0 = random. If `remember` is set, the port that was
    /// actually bound is stored there and preferred next time, so the addresses a phone learned
    /// at pairing stay valid across daemon restarts and reboots.
    All {
        port: u16,
        required: bool,
        remember: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Clone)]
pub struct NetOptions {
    pub relay_mode: RelayMode,
    /// Publish/resolve addresses via n0's DNS (pkarr). Off in tests.
    pub n0_lookup: bool,
    pub bind: Bind,
}

impl NetOptions {
    pub fn from_config(cfg: &crate::config::Config, paths: &crate::paths::Paths) -> Result<Self> {
        let bind = if cfg.udp_port != 0 {
            Bind::All {
                port: cfg.udp_port,
                required: true,
                remember: None,
            }
        } else {
            let f = paths.udp_port_file();
            let port = std::fs::read_to_string(&f)
                .ok()
                .and_then(|s| s.trim().parse::<u16>().ok())
                .unwrap_or(0);
            Bind::All {
                port,
                required: false,
                remember: Some(f),
            }
        };
        Ok(Self {
            relay_mode: cfg.relay_mode()?,
            n0_lookup: true,
            bind,
        })
    }

    /// Loopback only, no relays, no DNS: for tests.
    pub fn local_test() -> Self {
        Self {
            relay_mode: RelayMode::Disabled,
            n0_lookup: false,
            bind: Bind::Loopback,
        }
    }
}

pub async fn bind_endpoint(
    secret_key: SecretKey,
    opts: &NetOptions,
    hooks: Option<Gate>,
) -> Result<Endpoint> {
    // Builder isn't Clone; make a fresh one per bind attempt.
    let builder = || {
        let mut b = if opts.n0_lookup {
            Endpoint::builder(presets::N0)
        } else {
            Endpoint::builder(presets::Minimal)
        };
        b = b
            .secret_key(secret_key.clone())
            .relay_mode(opts.relay_mode.clone());
        if let Some(h) = hooks.clone() {
            b = b.hooks(h);
        }
        b
    };
    match &opts.bind {
        Bind::Loopback => Ok(builder()
            .clear_ip_transports()
            .bind_addr(SocketAddr::from(([127, 0, 0, 1], 0)))
            .context("bind address")?
            .bind()
            .await?),
        Bind::All {
            port,
            required,
            remember,
        } => {
            let ep = match if *port == 0 {
                bind_random(&builder).await
            } else {
                bind_all(builder(), *port).await
            } {
                Ok(ep) => ep,
                Err(e) if !*required && *port != 0 => {
                    tracing::warn!("UDP port {port} unavailable ({e:#}); using a random port");
                    bind_random(&builder).await?
                }
                Err(e) => return Err(e),
            };
            if let Some(f) = remember {
                let bound = ep
                    .bound_sockets()
                    .iter()
                    .find(|a| a.is_ipv4())
                    .map(|a| a.port());
                if let Some(p) = bound.filter(|p| p != port)
                    && let Err(e) = crate::paths::write_private(f, p.to_string().as_bytes())
                {
                    tracing::warn!("could not remember UDP port: {e:#}");
                }
            }
            Ok(ep)
        }
    }
}

/// A random port shared by IPv4 and IPv6 (port 0 would give each family a different one,
/// and only one of them could then be remembered).
async fn bind_random(builder: &impl Fn() -> iroh::endpoint::Builder) -> Result<Endpoint> {
    let mut last = None;
    for _ in 0..8 {
        let p = std::net::UdpSocket::bind(("0.0.0.0", 0))?
            .local_addr()?
            .port();
        match bind_all(builder(), p).await {
            Ok(ep) => return Ok(ep),
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("at least one attempt"))
}

/// IPv4 and IPv6 on the same port (IPv6 optional: hosts without IPv6 still work).
async fn bind_all(b: iroh::endpoint::Builder, port: u16) -> Result<Endpoint> {
    let b = b
        .bind_addr(SocketAddr::from(([0, 0, 0, 0], port)))
        .context("IPv4 bind address")?
        .bind_addr_with_opts(
            SocketAddr::from(([0u16; 8], port)),
            iroh::endpoint::BindOpts::default().set_is_required(false),
        )
        .context("IPv6 bind address")?;
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
        // Preferred first: a phone that offers several gets the newest (3: files, 2: audio).
        .accept(ALPN_SESSION_V3, SessionHandler(state.clone()))
        .accept(ALPN_SESSION_V2, SessionHandler(state.clone()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(port: u16, required: bool, remember: Option<std::path::PathBuf>) -> NetOptions {
        NetOptions {
            relay_mode: RelayMode::Disabled,
            n0_lookup: false,
            bind: Bind::All {
                port,
                required,
                remember,
            },
        }
    }

    fn v4_port(ep: &Endpoint) -> u16 {
        ep.bound_sockets()
            .iter()
            .find(|a| a.is_ipv4())
            .expect("ipv4 socket")
            .port()
    }

    #[tokio::test]
    async fn port_is_remembered_and_reused_and_falls_back_when_taken() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("udp-port");
        let key = SecretKey::generate();

        // First start: random port, remembered.
        let ep = bind_endpoint(key.clone(), &opts(0, false, Some(f.clone())), None)
            .await
            .unwrap();
        let p1 = v4_port(&ep);
        assert_eq!(std::fs::read_to_string(&f).unwrap().trim(), p1.to_string());
        let socks = ep.bound_sockets();
        assert!(
            socks.iter().all(|a| a.port() == p1),
            "v4/v6 on one port: {socks:?}"
        );
        ep.close().await;
        drop(ep);

        // Restart: same port again.
        let ep = bind_endpoint(key.clone(), &opts(p1, false, Some(f.clone())), None)
            .await
            .unwrap();
        assert_eq!(v4_port(&ep), p1);

        // Port taken by someone else (here: the still-running endpoint): fall back, remember new.
        let ep2 = bind_endpoint(key.clone(), &opts(p1, false, Some(f.clone())), None)
            .await
            .unwrap();
        let p2 = v4_port(&ep2);
        assert_ne!(p2, p1);
        assert_eq!(std::fs::read_to_string(&f).unwrap().trim(), p2.to_string());

        // A configured (required) port that is taken is an error, not a silent fallback.
        assert!(
            bind_endpoint(key, &opts(p2, true, None), None)
                .await
                .is_err(),
            "required port must not fall back"
        );
        ep.close().await;
        ep2.close().await;
    }
}
