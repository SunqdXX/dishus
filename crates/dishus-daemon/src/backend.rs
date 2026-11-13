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
    /// Whether events actually reach the system. A sink that is still waiting for its device
    /// may use this call to try again.
    fn ready(&mut self) -> bool {
        true
    }
}

/// Opens the real input backend, retrying until it works.
///
/// At boot the user service can start a few milliseconds before logind grants the new seat
/// session its `uaccess` ACL on `/dev/uinput`; a single attempt then leaves keyboard and mouse
/// dead for the whole boot while video works. Events that arrive while the device is missing are
/// dropped (there is nothing to deliver them to); `ready()` and events retry at most once per
/// [`RetryingInput::RETRY`]. `status` always describes the current state, for `dishus status`.
pub struct RetryingInput {
    open: Box<dyn FnMut() -> std::io::Result<Box<dyn InputSink>> + Send>,
    name: &'static str,
    inner: Option<Box<dyn InputSink>>,
    last_try: Option<std::time::Instant>,
    failures: u64,
    pub status: Arc<std::sync::Mutex<String>>,
}

impl RetryingInput {
    pub const RETRY: std::time::Duration = std::time::Duration::from_secs(1);

    pub fn new(
        name: &'static str,
        open: Box<dyn FnMut() -> std::io::Result<Box<dyn InputSink>> + Send>,
    ) -> Self {
        let mut s = Self {
            open,
            name,
            inner: None,
            last_try: None,
            failures: 0,
            status: Arc::new(std::sync::Mutex::new(String::new())),
        };
        s.try_open(std::time::Instant::now());
        s
    }

    fn set_status(&self, v: String) {
        *self.status.lock().expect("input status") = v;
    }

    fn try_open(&mut self, now: std::time::Instant) -> bool {
        if self.inner.is_some() {
            return true;
        }
        if self
            .last_try
            .is_some_and(|t| now.duration_since(t) < Self::RETRY)
        {
            return false;
        }
        self.last_try = Some(now);
        match (self.open)() {
            Ok(sink) => {
                if self.failures > 0 {
                    tracing::info!(
                        "input: {} is available now (after {} failed attempts); keyboard/mouse enabled",
                        self.name,
                        self.failures
                    );
                }
                self.inner = Some(sink);
                self.set_status(self.name.to_string());
                true
            }
            Err(e) => {
                if self.failures == 0 {
                    tracing::warn!(
                        "cannot open {} yet ({e}); keyboard/mouse disabled, retrying in the \
                         background. If this persists: README, \"Keyboard and mouse access\".",
                        self.name
                    );
                }
                self.failures += 1;
                self.set_status(format!("waiting for {} ({e}); retrying", self.name));
                false
            }
        }
    }

    fn with<F: FnOnce(&mut dyn InputSink) -> std::io::Result<()>>(
        &mut self,
        f: F,
    ) -> std::io::Result<()> {
        if self.try_open(std::time::Instant::now()) {
            f(self.inner.as_deref_mut().expect("opened"))
        } else {
            Ok(())
        }
    }
}

impl InputSink for RetryingInput {
    fn key(&mut self, code: u16, down: bool) -> std::io::Result<()> {
        self.with(|s| s.key(code, down))
    }
    fn button(&mut self, button: MouseButton, down: bool) -> std::io::Result<()> {
        self.with(|s| s.button(button, down))
    }
    fn motion(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        self.with(|s| s.motion(dx, dy))
    }
    fn scroll(&mut self, dx: i32, dy: i32) -> std::io::Result<()> {
        self.with(|s| s.scroll(dx, dy))
    }
    fn ready(&mut self) -> bool {
        self.try_open(std::time::Instant::now())
    }
}

/// Wraps an [`InputSink`] and remembers what is held down, so everything can be released on
/// disconnect/timeout no matter what the phone did. Stuck modifiers on a remote PC are a real
/// hazard (a stuck Super or Ctrl turns every later keystroke into a shortcut).
///
/// Lock keys (Caps, Num, Scroll Lock) are toggles, so "held" doesn't cover them: a tap leaves
/// nothing held but the lock on. Their toggles are counted too, and [`Self::end_session`] taps
/// each one the phone left toggled once more. That restores the state from before the session
/// (on Hyprland each keyboard has its own lock state, so DiShus's keyboard alone would stay in
/// Caps Lock, invisible on the real keyboard's LED; on Windows and X11 the state is global).
pub struct TrackedInput {
    sink: Box<dyn InputSink>,
    keys: BTreeSet<u16>,
    buttons: Vec<MouseButton>,
    /// Lock keys toggled an odd number of times this session.
    toggled: BTreeSet<u16>,
}

/// KEY_CAPSLOCK, KEY_NUMLOCK, KEY_SCROLLLOCK.
const LOCK_KEYS: [(u16, &str); 3] = [(58, "Caps Lock"), (69, "Num Lock"), (70, "Scroll Lock")];

fn lock_name(code: u16) -> Option<&'static str> {
    LOCK_KEYS.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

impl TrackedInput {
    pub fn new(sink: Box<dyn InputSink>) -> Self {
        Self {
            sink,
            keys: BTreeSet::new(),
            buttons: Vec::new(),
            toggled: BTreeSet::new(),
        }
    }

    pub fn key(&mut self, code: u16, down: bool) -> std::io::Result<()> {
        if down {
            // An auto-repeat "down" of a key already held is not a new press.
            if self.keys.insert(code)
                && let Some(name) = lock_name(code)
            {
                let changed = if self.toggled.remove(&code) {
                    false
                } else {
                    self.toggled.insert(code);
                    true
                };
                // Only lock keys are logged: logging every key would record typed passwords.
                tracing::debug!(
                    "{name} toggled by the phone ({})",
                    if changed {
                        "now the other way than before the session"
                    } else {
                        "back to how it was"
                    }
                );
            }
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

    /// The session is over: release everything held, then undo the lock keys the phone left
    /// toggled, so the PC is back to how it was. Not for mid-session releases (the phone's
    /// "Release all keys", a network blip): there the person still expects their Caps Lock.
    pub fn end_session(&mut self) {
        self.release_all();
        for code in std::mem::take(&mut self.toggled) {
            let name = lock_name(code).unwrap_or("lock key");
            let r = self
                .sink
                .key(code, true)
                .and_then(|()| self.sink.key(code, false));
            match r {
                Ok(()) => tracing::info!("{name} was left toggled by the phone; switched it back"),
                Err(e) => tracing::error!("restoring {name}: {e}"),
            }
        }
    }

    pub fn held(&self) -> (usize, usize) {
        (self.keys.len(), self.buttons.len())
    }

    /// See [`InputSink::ready`].
    pub fn ready(&mut self) -> bool {
        self.sink.ready()
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

/// One Opus packet (20 ms, 48 kHz stereo) with its capture time on the monotonic clock.
#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub pts_us: u64,
    pub data: Bytes,
}

pub trait AudioControl: Send + Sync {
    fn set_bitrate(&self, kbps: u32);
}

/// Running audio capture. Dropping it stops the capture.
pub struct AudioStream {
    pub packets: mpsc::Receiver<AudioPacket>,
    pub control: Arc<dyn AudioControl>,
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
    /// Start capturing what the PC plays. Needs no permission dialog.
    fn open_audio(&self) -> BoxFuture<anyhow::Result<AudioStream>> {
        Box::pin(async { Err(anyhow::anyhow!("audio capture is not available on this PC")) })
    }
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
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(not(windows))]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let f = opts.open(path)?;
        #[cfg(windows)]
        dishus_win::set_owner_only(path)?;
        Ok(Self(f))
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
    fn retrying_input_opens_late_and_drops_events_until_then() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let attempts = Arc::new(AtomicU32::new(0));
        let rec = Rec::default();
        let (a, r) = (attempts.clone(), rec.clone());
        let mut input = RetryingInput::new(
            "fake",
            Box::new(move || {
                // the first two attempts fail, like /dev/uinput before logind's ACL lands
                if a.fetch_add(1, Ordering::SeqCst) < 2 {
                    Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                } else {
                    Ok(Box::new(r.clone()) as Box<dyn InputSink>)
                }
            }),
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(input.status.lock().unwrap().starts_with("waiting for fake"));
        // rate-limited: events right away don't hammer the opener, and are dropped harmlessly
        input.key(30, true).unwrap();
        assert!(!input.ready());
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        // after RETRY: one more failure, then success
        input.last_try = Some(std::time::Instant::now() - RetryingInput::RETRY);
        assert!(!input.ready());
        input.last_try = Some(std::time::Instant::now() - RetryingInput::RETRY);
        assert!(input.ready());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(*input.status.lock().unwrap(), "fake");
        input.key(31, true).unwrap();
        // once open, never reopened
        assert!(input.ready());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(*rec.0.lock().unwrap(), vec!["k31+".to_string()]);
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

    #[test]
    fn end_session_turns_off_locks_the_phone_turned_on() {
        let rec = Rec::default();
        let mut t = TrackedInput::new(Box::new(rec.clone()));
        let tap = |t: &mut TrackedInput, c| {
            t.key(c, true).unwrap();
            t.key(c, false).unwrap();
        };
        tap(&mut t, 58); // Caps Lock on
        tap(&mut t, 69); // Num Lock on...
        tap(&mut t, 69); // ...and off again: nothing to undo
        t.key(70, true).unwrap(); // Scroll Lock pressed (toggles on press)...
        t.key(70, true).unwrap(); // ...an auto-repeat down is not a second toggle
        // Mid-session releases (phone's "Release all keys", a network blip) must not touch
        // locks: the person is still typing and expects Caps Lock to stay as they set it.
        t.release_all();
        let mid: Vec<String> = rec.0.lock().unwrap().clone();
        assert_eq!(mid.last().map(String::as_str), Some("k70-"));
        assert_eq!(mid.iter().filter(|l| *l == "k58+").count(), 1);
        t.key(30, true).unwrap(); // a held letter at the end
        rec.0.lock().unwrap().clear();
        t.end_session();
        // held keys first, then one tap per lock that is still toggled, low code first
        assert_eq!(
            *rec.0.lock().unwrap(),
            vec!["k30-", "k58+", "k58-", "k70+", "k70-"]
        );
        rec.0.lock().unwrap().clear();
        t.end_session(); // idempotent: the next session starts clean
        assert!(rec.0.lock().unwrap().is_empty());
    }
}
