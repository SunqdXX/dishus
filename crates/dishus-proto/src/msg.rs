//! Control-stream and pairing messages. Every type implements [`Validate`]; framing calls it on
//! every decode, so a message that exists in memory has already passed its bounds checks.

use serde::{Deserialize, Serialize};

pub use crate::frame::Validate;
use crate::{
    MAX_ERROR_LEN, MAX_EXPIRY_DAYS, MAX_MONITORS, PROTO_VERSION, TOKEN_LEN, keys, valid_name,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Encoder quality preset; bounds the adaptive bitrate controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Quality {
    Low,
    Medium,
    High,
    #[default]
    Auto,
}

/// What the phone wants to see.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamParams {
    /// Index into `Welcome::monitors`.
    pub monitor: u8,
    /// 0 = adaptive (start at 30, go up to 60 when bandwidth and encoder keep up).
    /// 1..=120 = fixed target.
    pub fps: u8,
    pub quality: Quality,
    /// 0 = native resolution, otherwise downscale so height <= this (240..=4320).
    pub max_height: u16,
}

impl Default for StreamParams {
    fn default() -> Self {
        Self {
            monitor: 0,
            fps: 0,
            quality: Quality::Auto,
            max_height: 0,
        }
    }
}

impl Validate for StreamParams {
    fn validate(&self) -> Result<(), &'static str> {
        if usize::from(self.monitor) >= MAX_MONITORS {
            return Err("monitor index out of range");
        }
        if self.fps > 120 {
            return Err("fps out of range");
        }
        if self.max_height != 0 && !(240..=4320).contains(&self.max_height) {
            return Err("max_height out of range");
        }
        Ok(())
    }
}

/// Phone → PC on the control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Must be the first message.
    Hello {
        proto_version: u16,
        device_name: String,
        stream: StreamParams,
    },
    /// Heartbeat; `t` is echoed back in `Pong` for RTT measurement.
    Ping {
        t: u64,
    },
    /// Linux evdev keycode press/release.
    Key {
        code: u16,
        down: bool,
    },
    /// Relative pointer motion in pixels.
    MouseMove {
        dx: i16,
        dy: i16,
    },
    MouseButton {
        button: MouseButton,
        down: bool,
    },
    /// High-resolution scroll; 120 = one wheel notch. Positive dy = scroll down.
    Scroll {
        dx: i16,
        dy: i16,
    },
    /// Release every pressed key and button (phone lost focus, app backgrounded, ...).
    ReleaseAll,
    RequestKeyframe,
    SetStream(StreamParams),
    /// Extend this phone's pairing to now + `days` (1..=60). Only valid inside a session.
    RenewPairing {
        days: u16,
    },
    Bye,
}

impl Validate for ClientMsg {
    fn validate(&self) -> Result<(), &'static str> {
        match self {
            ClientMsg::Hello {
                proto_version,
                device_name,
                stream,
            } => {
                if *proto_version != PROTO_VERSION {
                    return Err("unsupported protocol version");
                }
                if !valid_name(device_name) {
                    return Err("bad device name");
                }
                stream.validate()
            }
            ClientMsg::Key { code, .. } => {
                if keys::is_allowed_keycode(*code) {
                    Ok(())
                } else {
                    Err("keycode not allowed")
                }
            }
            ClientMsg::MouseMove { dx, dy } => {
                // Larger jumps are never produced by a finger; reject instead of clamping so a
                // buggy/malicious client is visible in the logs.
                if dx.unsigned_abs() > 4096 || dy.unsigned_abs() > 4096 {
                    Err("mouse delta too large")
                } else {
                    Ok(())
                }
            }
            ClientMsg::Scroll { dx, dy } => {
                if dx.unsigned_abs() > 120 * 20 || dy.unsigned_abs() > 120 * 20 {
                    Err("scroll delta too large")
                } else {
                    Ok(())
                }
            }
            ClientMsg::SetStream(p) => p.validate(),
            ClientMsg::RenewPairing { days } => {
                if (1..=MAX_EXPIRY_DAYS).contains(days) {
                    Ok(())
                } else {
                    Err("days out of range")
                }
            }
            ClientMsg::Ping { .. }
            | ClientMsg::MouseButton { .. }
            | ClientMsg::ReleaseAll
            | ClientMsg::RequestKeyframe
            | ClientMsg::Bye => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub index: u8,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeKind {
    Info,
    /// Screen capture is unavailable (portal denied, no token, pipeline error). Input still works.
    CaptureUnavailable,
    /// The request was understood but refused.
    Refused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ByeReason {
    Normal,
    /// Another session from a paired phone took over.
    Replaced,
    /// `dishus kill` on the PC.
    Killed,
    /// Pairing revoked or expired while connected.
    Revoked,
    Timeout,
    ProtocolError,
}

/// PC → phone on the control stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome {
        proto_version: u16,
        pc_name: String,
        monitors: Vec<MonitorInfo>,
        /// Unix seconds at which this phone's pairing expires.
        expires_at: u64,
    },
    Pong {
        t: u64,
    },
    /// A (new) video stream starts with the next frame, which is always a keyframe.
    StreamInfo {
        monitor: u8,
        width: u32,
        height: u32,
        fps: u8,
        bitrate_kbps: u32,
        encoder: String,
    },
    Renewed {
        expires_at: u64,
    },
    Notice {
        kind: NoticeKind,
        message: String,
    },
    Bye {
        reason: ByeReason,
    },
}

impl Validate for ServerMsg {
    fn validate(&self) -> Result<(), &'static str> {
        match self {
            ServerMsg::Welcome {
                proto_version,
                pc_name,
                monitors,
                ..
            } => {
                if *proto_version != PROTO_VERSION {
                    return Err("unsupported protocol version");
                }
                if !valid_name(pc_name) {
                    return Err("bad pc name");
                }
                if monitors.len() > MAX_MONITORS {
                    return Err("too many monitors");
                }
                for m in monitors {
                    if !valid_name(&m.name) || m.width > 16384 || m.height > 16384 {
                        return Err("bad monitor");
                    }
                }
                Ok(())
            }
            ServerMsg::StreamInfo {
                width,
                height,
                fps,
                encoder,
                ..
            } => {
                if *width > 16384 || *height > 16384 || *fps > 240 || !valid_name(encoder) {
                    Err("bad stream info")
                } else {
                    Ok(())
                }
            }
            ServerMsg::Notice { message, .. } => {
                if message.len() > MAX_ERROR_LEN {
                    Err("notice too long")
                } else {
                    Ok(())
                }
            }
            ServerMsg::Pong { .. } | ServerMsg::Renewed { .. } | ServerMsg::Bye { .. } => Ok(()),
        }
    }
}

/// Phone → PC, the only message on a pairing connection.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRequest {
    pub proto_version: u16,
    pub token: [u8; TOKEN_LEN],
    pub device_name: String,
}

impl std::fmt::Debug for PairRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // never print the token
        f.debug_struct("PairRequest")
            .field("proto_version", &self.proto_version)
            .field("device_name", &self.device_name)
            .finish_non_exhaustive()
    }
}

impl Drop for PairRequest {
    fn drop(&mut self) {
        // best-effort wipe; the authoritative copy lives zeroized in the daemon
        self.token.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&self.token);
    }
}

impl Validate for PairRequest {
    fn validate(&self) -> Result<(), &'static str> {
        if self.proto_version != PROTO_VERSION {
            return Err("unsupported protocol version");
        }
        if !valid_name(&self.device_name) {
            return Err("bad device name");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairResponse {
    Accepted {
        pc_name: String,
        expires_at: u64,
    },
    /// Deliberately carries no reason.
    Rejected,
}

impl Validate for PairResponse {
    fn validate(&self) -> Result<(), &'static str> {
        match self {
            PairResponse::Accepted { pc_name, .. } if !valid_name(pc_name) => Err("bad pc name"),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(name: &str) -> ClientMsg {
        ClientMsg::Hello {
            proto_version: PROTO_VERSION,
            device_name: name.into(),
            stream: StreamParams::default(),
        }
    }

    #[test]
    fn hello_validation() {
        assert!(hello("Pixel 9").validate().is_ok());
        assert!(hello("").validate().is_err());
        assert!(hello(&"x".repeat(65)).validate().is_err());
        assert!(hello("evil\nname").validate().is_err());
        let bad_ver = ClientMsg::Hello {
            proto_version: 99,
            device_name: "p".into(),
            stream: StreamParams::default(),
        };
        assert!(bad_ver.validate().is_err());
    }

    #[test]
    fn stream_params_validation() {
        let mut p = StreamParams::default();
        assert!(p.validate().is_ok());
        p.fps = 121;
        assert!(p.validate().is_err());
        p.fps = 60;
        p.max_height = 100;
        assert!(p.validate().is_err());
        p.max_height = 720;
        assert!(p.validate().is_ok());
        p.monitor = 200;
        assert!(p.validate().is_err());
    }

    #[test]
    fn renew_bounds() {
        assert!(ClientMsg::RenewPairing { days: 0 }.validate().is_err());
        assert!(ClientMsg::RenewPairing { days: 30 }.validate().is_ok());
        assert!(ClientMsg::RenewPairing { days: 180 }.validate().is_ok());
        assert!(ClientMsg::RenewPairing { days: 181 }.validate().is_err());
    }

    #[test]
    fn mouse_bounds() {
        assert!(
            ClientMsg::MouseMove {
                dx: 4096,
                dy: -4096
            }
            .validate()
            .is_ok()
        );
        assert!(
            ClientMsg::MouseMove {
                dx: i16::MIN,
                dy: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            ClientMsg::Scroll {
                dx: 0,
                dy: 120 * 21
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn pair_request_debug_hides_token() {
        let r = PairRequest {
            proto_version: 1,
            token: [0xAB; 32],
            device_name: "p".into(),
        };
        let s = format!("{r:?}");
        assert!(!s.to_lowercase().contains("ab, "), "{s}");
        assert!(!s.contains("171"), "{s}");
    }
}
