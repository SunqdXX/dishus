//! DiShus wire protocol.
//!
//! Everything that crosses the network is defined here so the daemon and the phone core agree
//! on it, and so every message is validated in exactly one place.
//!
//! * Control stream (one bidirectional QUIC stream, opened by the phone): u32 big-endian length
//!   prefix + `postcard`-encoded [`ClientMsg`] / [`ServerMsg`]. Max [`MAX_CONTROL_FRAME`] bytes.
//! * Video (one unidirectional QUIC stream per encoded frame, opened by the PC):
//!   [`VideoHeader`] followed by an H.264 Annex-B access unit. Max [`MAX_VIDEO_FRAME`] bytes.
//! * Pairing (separate ALPN): a single [`PairRequest`] / [`PairResponse`] exchange.

pub mod frame;
pub mod ipc;
pub mod keys;
pub mod msg;
pub mod pairing;
pub mod video;

pub use frame::{FrameError, read_frame, write_frame};
pub use msg::*;
pub use pairing::{PairingInfo, PairingUriError};
pub use video::VideoHeader;

/// ALPN of an authenticated remote-control session.
pub const ALPN_SESSION: &[u8] = b"dishus/session/1";
/// ALPN of the pairing handshake. Only accepted while the pairing window is open.
pub const ALPN_PAIR: &[u8] = b"dishus/pair/1";

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
pub const DEFAULT_EXPIRY_DAYS: u16 = 30;
/// Maximum lifetime of a pairing (days).
pub const MAX_EXPIRY_DAYS: u16 = 60;

/// The phone sends something at least this often (ms); the PC drops the session after
/// [`SESSION_IDLE_TIMEOUT_MS`] without traffic and releases every key.
pub const HEARTBEAT_INTERVAL_MS: u64 = 1000;
pub const SESSION_IDLE_TIMEOUT_MS: u64 = 5000;

/// Validate a human-readable name: bounded, no control characters.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_NAME_LEN && !s.chars().any(char::is_control)
}
