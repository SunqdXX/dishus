//! Screen capture + H.264 encoding, run in a per-session worker process.
//!
//! * [`client`] (daemon side, always built): spawns and drives the worker.
//! * `worker`, `pipeline`, `portal` (feature `capture`, Linux): the worker itself.
//! * `win` (feature `capture`, Windows): the same worker on Desktop Duplication and
//!   Media Foundation, speaking the same [`wire`] protocol.
//!
//! Capture sources inside the worker:
//! * **Portal** (default; any compositor with xdg-desktop-portal ScreenCast: Hyprland via
//!   xdg-desktop-portal-hyprland, niri via -gnome, GNOME, KDE): PipeWire node + restore token so
//!   it doesn't prompt every time.
//! * **X11** fallback: `ximagesrc` of the whole screen when no portal ScreenCast is available.
//! * **Windows**: DXGI Desktop Duplication, one output per monitor, no permission dialog.
//! * **Test**: `DISHUS_TEST_SOURCE=1` streams a test pattern (no permission needed).

pub mod client;
pub mod throttle;
pub mod wire;

#[cfg(all(feature = "capture", not(windows)))]
pub mod pipeline;
#[cfg(all(feature = "capture", not(windows)))]
mod portal;
#[cfg(all(feature = "capture", windows))]
pub mod win;
#[cfg(all(feature = "capture", not(windows)))]
pub mod worker;
#[cfg(all(feature = "capture", windows))]
pub use win::worker;

use std::path::PathBuf;

/// Worker configuration, passed through the environment by [`client::WorkerProvider`].
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    pub token_file: PathBuf,
    pub encoder_pref: String,
    pub show_cursor: bool,
    pub test_source: bool,
}

impl WorkerOptions {
    pub fn from_env() -> anyhow::Result<Self> {
        let token_file = std::env::var_os("DISHUS_W_TOKEN_FILE")
            .map(PathBuf::from)
            .ok_or_else(|| {
                anyhow::anyhow!("DISHUS_W_TOKEN_FILE not set (the worker is started by the daemon)")
            })?;
        Ok(Self {
            token_file,
            encoder_pref: std::env::var("DISHUS_W_ENCODER").unwrap_or_else(|_| "auto".into()),
            show_cursor: std::env::var("DISHUS_W_CURSOR")
                .map(|v| v != "0")
                .unwrap_or(true),
            test_source: std::env::var_os("DISHUS_TEST_SOURCE").is_some_and(|v| v == "1"),
        })
    }
}
