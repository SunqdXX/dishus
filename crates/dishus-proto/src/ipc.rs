//! Local control socket protocol between `dishus` (CLI) and the daemon. Same framing as the
//! network protocol. The socket is 0600 in `$XDG_RUNTIME_DIR/dishus/` and the daemon also checks
//! the peer's uid.

use serde::{Deserialize, Serialize};

use crate::frame::Validate;

pub const MAX_IPC_FRAME: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcRequest {
    Status,
    List,
    /// Open the pairing window and return the pairing link.
    Pair,
    ClosePairing,
    Revoke { selector: String },
    Renew { selector: String, days: u16 },
    /// Cut every session and lock down until `Unlock`.
    Kill,
    Unlock,
    /// Run the screen-capture permission dialog now (user is at the PC).
    SetupCapture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcDevice {
    pub id: String,
    pub name: String,
    pub paired_at: u64,
    pub expires_at: u64,
    pub last_seen: u64,
    pub connected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcSession {
    pub id: String,
    pub name: String,
    pub since: u64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcStatus {
    pub version: String,
    pub pc_name: String,
    pub endpoint_id: String,
    pub relays: Vec<String>,
    pub direct_addrs: Vec<String>,
    pub lockdown: bool,
    pub pairing_open: bool,
    pub sessions: Vec<IpcSession>,
    pub devices: usize,
    pub capture_backend: String,
    pub input_backend: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcResponse {
    Status(IpcStatus),
    Devices(Vec<IpcDevice>),
    Pairing { uri: String, ttl_secs: u64 },
    Ok(String),
    Error(String),
}

impl Validate for IpcRequest {
    fn validate(&self) -> Result<(), &'static str> {
        match self {
            IpcRequest::Revoke { selector } | IpcRequest::Renew { selector, .. }
                if selector.is_empty() || selector.len() > 128 =>
            {
                Err("bad selector")
            }
            _ => Ok(()),
        }
    }
}

impl Validate for IpcResponse {
    fn validate(&self) -> Result<(), &'static str> {
        Ok(())
    }
}
