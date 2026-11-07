//! Messages between the daemon and its capture worker (stdin/stdout pipes, same length-prefixed
//! postcard framing as the network protocol).

use serde::{Deserialize, Serialize};

use dishus_proto::{MAX_VIDEO_FRAME, MonitorInfo, frame::Validate};

pub const MAX_WORKER_FRAME: usize = MAX_VIDEO_FRAME + 1024;
pub const MAX_CMD_FRAME: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerCmd {
    Start {
        monitor: u8,
        fps: u8,
        bitrate_kbps: u32,
        max_height: u16,
    },
    Stop,
    Keyframe,
    Bitrate(u32),
    Fps(u8),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerEvent {
    Monitors(Vec<MonitorInfo>),
    Started {
        width: u32,
        height: u32,
        encoder: String,
    },
    Frame {
        pts_us: u64,
        keyframe: bool,
        data: Vec<u8>,
    },
    /// The stream (or the whole capture, before `Monitors`) failed.
    Failed(String),
    /// Result line of `setup` / `probe` modes.
    Info(String),
}

impl Validate for WorkerCmd {
    fn validate(&self) -> Result<(), &'static str> {
        Ok(())
    }
}

impl Validate for WorkerEvent {
    fn validate(&self) -> Result<(), &'static str> {
        match self {
            WorkerEvent::Monitors(m) if m.len() > dishus_proto::MAX_MONITORS => {
                Err("too many monitors")
            }
            WorkerEvent::Frame { data, .. } if data.is_empty() => Err("empty frame"),
            WorkerEvent::Failed(s) | WorkerEvent::Info(s) if s.len() > 4096 => {
                Err("message too long")
            }
            _ => Ok(()),
        }
    }
}
