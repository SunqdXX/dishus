//! The network gate. Two layers, both before any application byte is read:
//!
//! 1. **Incoming filter** (before the TLS handshake): relayed connections already carry the
//!    sender's EndpointId, so unknown ids are silently ignored unless the pairing window is open.
//!    Direct UDP connections must pass QUIC address validation (Retry) and a per-IP rate limit.
//! 2. **`after_handshake` hook**: the TLS handshake has proven the peer's EndpointId. Session
//!    connections must be on the allowlist and unexpired; pairing connections require the
//!    pairing window to be open. Anything else is closed with code 0 and no reason.

use std::{fmt, sync::Arc, time::Instant};

use iroh::{
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, Incoming, IncomingAddr},
    protocol::IncomingFilterOutcome,
};

use dishus_proto::{ALPN_PAIR, ALPN_SESSION};

use crate::state::AppState;

#[derive(Clone)]
pub struct Gate(pub Arc<AppState>);

impl fmt::Debug for Gate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Gate")
    }
}

/// Pure decision used by the hook (also unit-tested).
pub fn admit(state: &AppState, alpn: &[u8], peer: &iroh::EndpointId) -> bool {
    if alpn == ALPN_SESSION {
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
            AfterHandshakeOutcome::Reject {
                error_code: 0u32.into(),
                reason: Vec::new(),
            }
        }
    }
}

/// The pre-handshake filter.
pub fn incoming_filter(state: Arc<AppState>) -> iroh::protocol::IncomingFilter {
    Arc::new(move |incoming: &Incoming| match incoming.remote_addr() {
        IncomingAddr::Relay { endpoint_id, .. } => {
            if state.is_session_allowed(&endpoint_id) || state.is_pairing_open() {
                IncomingFilterOutcome::Accept
            } else {
                IncomingFilterOutcome::Ignore
            }
        }
        IncomingAddr::Ip(addr) => {
            if !incoming.remote_addr_validated() {
                return IncomingFilterOutcome::Retry;
            }
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
