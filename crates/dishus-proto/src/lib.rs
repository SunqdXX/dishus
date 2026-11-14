//! DiShus wire protocol.
//!
//! Everything that crosses the network is defined here so the daemon and the phone core agree
//! on it, and so every message is validated in exactly one place.
//!
//! * Control stream (one bidirectional QUIC stream, opened by the phone): u32 big-endian length
//!   prefix + `postcard`-encoded [`ClientMsg`] / [`ServerMsg`]. Max [`MAX_CONTROL_FRAME`] bytes.
//! * Video (one unidirectional QUIC stream per encoded frame, opened by the PC):
//!   [`VideoHeader`] followed by an H.264 Annex-B access unit. Max [`MAX_VIDEO_FRAME`] bytes.
//! * Audio (session/2 and up): one QUIC datagram per Opus packet, [`AudioHeader`] first.
//! * Files (session/3 only): one bidirectional QUIC stream per file, opened by the phone; see
//!   [`files`].
//! * Pairing (separate ALPN): a single [`PairRequest`] / [`PairResponse`] exchange.
//!
//! **Versions are ALPNs.** `proto_version` in `Hello`/`Welcome` stays 1: released builds reject
//! anything else, so bumping it would cut them off. A new phone offers session/2 and session/1,
//! a new PC prefers session/2, and old builds only know session/1. Messages added for session/2
//! are appended to the enums (postcard numbers variants by position) and are never sent on a
//! session/1 connection: an old peer would treat an unknown variant as a protocol error.
//! session/3 works the same way on top of session/2. Code checks features with
//! [`session_version`] and `>=`, never with an equality test on one ALPN.

pub mod audio;
pub mod files;
pub mod frame;
pub mod ipc;
pub mod keys;
pub mod msg;
pub mod pairing;
pub mod video;

pub use audio::AudioHeader;
pub use frame::{FrameError, read_frame, write_frame};
pub use msg::*;
pub use pairing::{PairingInfo, PairingUriError};
pub use video::VideoHeader;

/// ALPN of an authenticated remote-control session.
pub const ALPN_SESSION: &[u8] = b"dishus/session/1";
/// session/1 plus PC audio ([`ClientMsg::SetAudio`], [`ServerMsg::AudioStarted`], datagrams).
pub const ALPN_SESSION_V2: &[u8] = b"dishus/session/2";
/// session/2 plus phone → PC file send ([`ServerMsg::FilesInfo`], [`files`]).
pub const ALPN_SESSION_V3: &[u8] = b"dishus/session/3";
/// Every session ALPN, newest first (what the phone offers).
pub const SESSION_ALPNS: [&[u8]; 3] = [ALPN_SESSION_V3, ALPN_SESSION_V2, ALPN_SESSION];

/// 1, 2 or 3 for a session ALPN; None for anything else.
pub fn session_version(alpn: &[u8]) -> Option<u8> {
    match alpn {
        ALPN_SESSION => Some(1),
        ALPN_SESSION_V2 => Some(2),
        ALPN_SESSION_V3 => Some(3),
        _ => None,
    }
}

/// ALPN of the pairing handshake. Only accepted while the pairing window is open.
pub const ALPN_PAIR: &[u8] = b"dishus/pair/1";

/// The release name people see: `0.31` for crate version 0.31.0 (releases are X.31 and X.69).
pub const VERSION: &str = short_version(env!("CARGO_PKG_VERSION"));

const fn short_version(v: &'static str) -> &'static str {
    let b = v.as_bytes();
    if b.len() > 2 && b[b.len() - 2] == b'.' && b[b.len() - 1] == b'0' {
        let (head, _) = b.split_at(b.len() - 2);
        match core::str::from_utf8(head) {
            Ok(s) => s,
            Err(_) => v,
        }
    } else {
        v
    }
}

#[cfg(test)]
#[test]
fn session_versions() {
    assert_eq!(session_version(ALPN_SESSION), Some(1));
    assert_eq!(session_version(ALPN_SESSION_V3), Some(3));
    assert_eq!(session_version(ALPN_PAIR), None);
    assert_eq!(
        SESSION_ALPNS.map(|a| session_version(a).unwrap()),
        [3, 2, 1]
    );
}

#[cfg(test)]
#[test]
fn version_names() {
    assert_eq!(short_version("0.31.0"), "0.31");
    assert_eq!(short_version("1.69.0"), "1.69");
    assert_eq!(short_version("0.1.4"), "0.1.4");
    assert!(!VERSION.ends_with(".0"));
}

/// Protocol version carried in `Hello` / `Welcome`.
pub const PROTO_VERSION: u16 = 1;

/// QUIC application close codes. The PC tells a *paired* device why it refuses it (it has proven
/// its identity in the TLS handshake); anything else gets [`close_code::REFUSED`] and no reason.
pub mod close_code {
    pub const REFUSED: u32 = 0;
    pub const PROTOCOL_ERROR: u32 = 1;
    /// `dishus kill` is in effect until `dishus unlock`.
    pub const LOCKDOWN: u32 = 2;
    /// The pairing expired; `dishus renew` on the PC or pair again.
    pub const EXPIRED: u32 = 3;
}

/// Hard cap on a single control-stream frame (bytes, excluding the 4-byte length prefix).
pub const MAX_CONTROL_FRAME: usize = 64 * 1024;
/// Hard cap on a single encoded video frame including its header.
pub const MAX_VIDEO_FRAME: usize = 8 * 1024 * 1024;
/// Max length of human-readable names (device, PC, monitor).
pub const MAX_NAME_LEN: usize = 64;
/// Max length of a free-form error message.
pub const MAX_ERROR_LEN: usize = 256;
/// Max monitors reported in `Welcome`.
pub const MAX_MONITORS: usize = 16;

/// Pairing tokens are 256-bit.
pub const TOKEN_LEN: usize = 32;
/// Default lifetime of a pairing (days).
pub const DEFAULT_EXPIRY_DAYS: u16 = 180;
/// Maximum lifetime of a pairing (days).
pub const MAX_EXPIRY_DAYS: u16 = 180;

/// The phone sends something at least this often (ms); the PC drops the session after
/// [`SESSION_IDLE_TIMEOUT_MS`] without traffic and releases every key.
pub const HEARTBEAT_INTERVAL_MS: u64 = 1000;
pub const SESSION_IDLE_TIMEOUT_MS: u64 = 5000;
/// The PC opens screen capture (portal, worker) before its Welcome, for at most this long.
pub const CAPTURE_OPEN_TIMEOUT_MS: u64 = 15_000;

/// Validate a human-readable name: bounded, no control characters.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_NAME_LEN && !s.chars().any(char::is_control)
}
