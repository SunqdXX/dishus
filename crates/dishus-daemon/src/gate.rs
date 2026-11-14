//! The network gate. Two layers, both before any application byte is read:
//!
//! 1. **Incoming filter** (before the TLS handshake): relayed connections already carry the
//!    sender's EndpointId, so unknown ids are silently ignored unless the pairing window is open.
//!    Direct UDP connections get a per-IP handshake rate limit. (No QUIC Retry: iroh's client
//!    sends its Initial over several paths at once, and a Retry token is bound to one source
//!    address, so the copies arriving from other addresses made the server abort with "invalid
//!    retry token". QUIC's 3x anti-amplification limit still applies before validation.)
//! 2. **`after_handshake` hook**: the TLS handshake has proven the peer's EndpointId. Session
//!    connections must be on the allowlist and unexpired; pairing connections require the
//!    pairing window to be open. A refused device that is paired (but expired, or the PC is in
//!    lockdown) is told why via the close code, so the phone can say so instead of retrying
//!    forever; anything else is closed with code 0 and no reason.

use std::{fmt, sync::Arc, time::Instant};

use iroh::{
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, Incoming, IncomingAddr},
    protocol::IncomingFilterOutcome,
};

use dishus_proto::{ALPN_PAIR, session_version};

use crate::state::AppState;

#[derive(Clone)]
pub struct Gate(pub Arc<AppState>);

impl fmt::Debug for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Gate")
    }
}

/// Any session version: the same allowlist and the same refusal reasons apply.
fn is_session(alpn: &[u8]) -> bool {
    session_version(alpn).is_some()
}

/// Pure decision used by the hook (also unit-tested).
pub fn admit(state: &AppState, alpn: &[u8], peer: &iroh::EndpointId) -> bool {
    if is_session(alpn) {
        state.is_session_allowed(peer)
    } else if alpn == ALPN_PAIR {
        state.is_pairing_open()
    } else {
        false
    }
}

impl EndpointHooks for Gate {
    async fn after_handshake<'a>(&'a self, conn: &'a Connection) -> AfterHandshakeOutcome {
        let peer = conn.remote_id();
        if admit(&self.0, conn.alpn(), &peer) {
            AfterHandshakeOutcome::Accept
        } else {
            tracing::debug!(
                "gate: dropped {} (alpn {:?})",
                peer.fmt_short(),
                String::from_utf8_lossy(conn.alpn())
            );
            let code = if is_session(conn.alpn()) {
                self.0.refusal_code(&peer)
            } else {
                dishus_proto::close_code::REFUSED
            };
            AfterHandshakeOutcome::Reject {
                error_code: code.into(),
                reason: Vec::new(),
            }
        }
    }
}

/// The pre-handshake filter.
pub fn incoming_filter(state: Arc<AppState>) -> iroh::protocol::IncomingFilter {
    Arc::new(move |incoming: &Incoming| match incoming.remote_addr() {
        IncomingAddr::Relay { endpoint_id, .. } => {
            // Known devices get through even when refused, so the hook can tell them why.
            if state.device(&endpoint_id).is_some() || state.is_pairing_open() {
                IncomingFilterOutcome::Accept
            } else {
                IncomingFilterOutcome::Ignore
            }
        }
        IncomingAddr::Ip(addr) => {
            if state
                .limiter
                .lock()
                .expect("limiter")
                .allow(addr.ip(), Instant::now())
            {
                IncomingFilterOutcome::Accept
            } else {
                IncomingFilterOutcome::Ignore
            }
        }
        // Unknown/future transports: don't let them in.
        #[allow(unreachable_patterns)]
        _ => IncomingFilterOutcome::Ignore,
    })
}
