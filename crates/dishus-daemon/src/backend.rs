//! Abstractions over the platform pieces (uinput, capture/encode) so the session logic can be
//! tested with fakes, and so capture backends can be swapped (portal/PipeWire, X11).

use std::{collections::BTreeSet, sync::Arc};

use std::{future::Future, pin::Pin};

use bytes::Bytes;
use tokio::sync::mpsc;

/// Boxed future that may borrow (`'a`).
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type BoxFuture<T> = BoxFut<'static, T>;

use dishus_proto::{MonitorInfo, MouseButton};

/// Raw injection target. Implementations must be cheap per call.
pub trait InputSink: Send {
    fn key(&mut self, code: u16, down: bool) -> std::io::Result<()>;
    fn button(&mut self, button: MouseButton, down: bool) -> std::io::Result<()>;
    fn motion(&mut self, dx: i32, dy: i32) -> std::io::Result<()>;
    /// High-resolution units: 120 per notch. Positive dy scrolls down.
    fn scroll(&mut self, dx: i32, dy: i32) -> std::io::Result<()>;
}

/// Wraps an [`InputSink`] and remembers what is held down, so everything can be released on
/// disconnect/timeout no matter what the phone did. Stuck modifiers on a remote PC are a real
/// hazard (a stuck Super or Ctrl turns every later keystroke into a shortcut).
pub struct TrackedInput {
    sink: Box<dyn InputSink>,
    keys: BTreeSet<u16>,
    buttons: Vec<MouseButton>,
}

impl TrackedInput {
    pub fn new(sink: Box<dyn InputSink>) -> Self {
        Self {
            sink,
            keys: BTreeSet::new(),
            buttons: Vec::new(),
        }
    }

    pub fn key(&mut self, code: u16, down: bool) -> std::io::Result<()> {
        if down {
            self.keys.insert(code);
        } else if !self.keys.remove(&code) {
            return Ok(()); // release of something not held: ignore
        }
        self.sink.key(code, down)
    }

    pub fn button(&mut self, b: MouseButton, down: bool) -> std::io::Result<()> {
        if down {
            if !self.buttons.contains(&b) {
                self.buttons.push(b);
            }
        } else if let Some(i) = self.buttons.iter().position(|x| *x == b) {
            self.buttons.remove(i);
        } else {
            return Ok(());
        }
        self.sink.button(b, down)
    }

    pub fn motion(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        self.sink.motion(dx, dy)
    }

    pub fn scroll(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        self.sink.scroll(dx, dy)
    }

    /// Release everything held. Never fails half-way: errors are logged and the rest continue.
    pub fn release_all(&mut self) {
        let keys: Vec<u16> = std::mem::take(&mut self.keys).into_iter().rev().collect();
        for k in keys {
            if let Err(e) = self.sink.key(k, false) {
                tracing::error!("releasing key {k}: {e}");
            }
        }
        for b in std::mem::take(&mut self.buttons) {
            if let Err(e) = self.sink.button(b, false) {
                tracing::error!("releasing button {b:?}: {e}");
            }
        }
    }

    pub fn held(&self) -> (usize, usize) {
        (self.keys.len(), self.buttons.len())
    }
}

/// One encoded H.264 access unit.
#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub data: Bytes,
    pub pts_us: u64,
    pub keyframe: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamConfig {
    pub monitor: u8,
    pub fps: u8,
    pub bitrate_kbps: u32,
    /// 0 = native
    pub max_height: u16,
}

/// Live knobs on a running encoder.
pub trait StreamControl: Send + Sync {
    fn force_keyframe(&self);
    fn set_bitrate(&self, kbps: u32);
    fn set_fps(&self, fps: u8);
}

/// A running capture→encode pipeline. Dropping it stops the pipeline.
pub struct EncodedStream {
    pub frames: mpsc::Receiver<VideoFrame>,
    pub control: Arc<dyn StreamControl>,
    pub width: u32,
    pub height: u32,
    pub encoder: String,
    /// Keeps the pipeline alive; dropped with the stream.
    pub _guard: Box<dyn Send>,
}

/// An open capture permission (e.g. a portal ScreenCast session) covering one or more monitors.
pub trait CaptureSession: Send {
    fn monitors(&self) -> Vec<MonitorInfo>;
    /// Start (or restart) encoding one monitor. Any previous stream must be dropped first.
    fn start_stream(&mut self, cfg: StreamConfig) -> BoxFut<'_, anyhow::Result<EncodedStream>>;
}

pub trait CaptureProvider: Send + Sync {
    /// Open a capture session non-interactively (must not pop up a dialog nobody can answer).
    fn open(&self) -> BoxFuture<anyhow::Result<Box<dyn CaptureSession>>>;
    /// Open interactively while the user is at the PC, storing whatever lets later `open()`
    /// calls succeed without a prompt. Returns a human-readable summary.
    fn setup_interactive(&self) -> BoxFuture<anyhow::Result<String>>;
}

/// Capture provider used when the daemon is built without capture or none works.
pub struct NoCapture(pub String);

impl CaptureProvider for NoCapture {
    fn open(&self) -> BoxFuture<anyhow::Result<Box<dyn CaptureSession>>> {
        let msg = self.0.clone();
        Box::pin(async move { Err(anyhow::anyhow!(msg)) })
    }
    fn setup_interactive(&self) -> BoxFuture<anyhow::Result<String>> {
        let msg = self.0.clone();
        Box::pin(async move { Err(anyhow::anyhow!(msg)) })
    }
}

/// Input sink that drops everything; used when uinput is unavailable so video still works.
pub struct NullInput;

impl InputSink for NullInput {
    fn key(&mut self, _: u16, _: bool) -> std::io::Result<()> {
        Ok(())
    }
    fn button(&mut self, _: MouseButton, _: bool) -> std::io::Result<()> {
        Ok(())
    }
    fn motion(&mut self, _: i32, _: i32) -> std::io::Result<()> {
        Ok(())
    }
    fn scroll(&mut self, _: i32, _: i32) -> std::io::Result<()> {
        Ok(())
    }
}

/// Test sink: appends one line per event to a file instead of injecting input. Selected with
/// `DISHUS_INPUT=record:<path>`; used to drive the daemon from a real phone without touching the
/// desktop. Format: `key <code> <0|1>`, `btn <name> <0|1>`, `move <dx> <dy>`, `scroll <dx> <dy>`.
pub struct RecordInput(std::fs::File);

impl RecordInput {
    pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .map(Self)
    }
    fn line(&mut self, s: std::fmt::Arguments<'_>) -> std::io::Result<()> {
        use std::io::Write;
        writeln!(self.0, "{s}")
    }
}

impl InputSink for RecordInput {
    fn key(&mut self, code: u16, down: bool) -> std::io::Result<()> {
        self.line(format_args!("key {code} {}", down as u8))
    }
    fn button(&mut self, button: MouseButton, down: bool) -> std::io::Result<()> {
        self.line(format_args!("btn {button:?} {}", down as u8))
    }
    fn motion(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        self.line(format_args!("move {dx} {dy}"))
    }
    fn scroll(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        self.line(format_args!("scroll {dx} {dy}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Rec(Arc<Mutex<Vec<String>>>);
    impl InputSink for Rec {
        fn key(&mut self, c: u16, d: bool) -> std::io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("k{c}{}", if d { "+" } else { "-" }));
            Ok(())
        }
        fn button(&mut self, b: MouseButton, d: bool) -> std::io::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("{b:?}{}", if d { "+" } else { "-" }));
            Ok(())
        }
        fn motion(&mut self, _: i32, _: i32) -> std::io::Result<()> {
            Ok(())
        }
        fn scroll(&mut self, _: i32, _: i32) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn release_all_releases_everything_once() {
        let rec = Rec::default();
        let mut t = TrackedInput::new(Box::new(rec.clone()));
        t.key(125, true).unwrap(); // super
        t.key(29, true).unwrap(); // ctrl
        t.key(30, true).unwrap();
        t.key(30, false).unwrap();
        t.key(31, false).unwrap(); // never pressed: swallowed
        t.button(MouseButton::Left, true).unwrap();
        t.button(MouseButton::Left, true).unwrap(); // repeat down is forwarded but tracked once
        assert_eq!(t.held(), (2, 1));
        t.release_all();
        assert_eq!(t.held(), (0, 0));
        t.release_all(); // idempotent
        let log = rec.0.lock().unwrap().clone();
        assert_eq!(
            log,
            vec![
                "k125+", "k29+", "k30+", "k30-", "Left+", "Left+", "k125-", "k29-", "Left-"
            ]
        );
    }
}
