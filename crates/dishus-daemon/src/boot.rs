//! Login-screen mode (`dishus-daemon --boot`): a second daemon, run by systemd as its own
//! unprivileged user, that lets an already allowed phone see and type into the login screen.
//!
//! Rules (owner's, 2026-11-14):
//! * no pairing at all: phones are allowed from inside a logged-in session (`sudo dishus boot
//!   allow`), revoked by root or the owner;
//! * only while the login screen is the active session on seat0; idle otherwise (sessions
//!   ended, virtual keyboard closed, new connections refused);
//! * no picture, no typing: input is dropped unless a frame of the login screen went out
//!   within [`VIDEO_FRESH`], and a session whose capture fails is ended;
//! * limits a phone can't dodge ([`Limits`]): sessions last at most [`SESSION_MAX`], at most
//!   [`PER_HOUR`] per phone and hour, and after [`STRIKES_PER_DAY`] sessions in a day that no
//!   login followed, the phone is blocked until allowed again.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::{
    devices::now_unix,
    paths::{read_private, write_private},
    state::AppState,
};

/// Login-mode allowances last at most this long; renew from a session.
pub const MAX_ALLOW_DAYS: u16 = 30;
pub const SESSION_MAX: Duration = Duration::from_secs(180);
pub const PER_HOUR: usize = 3;
pub const STRIKES_PER_DAY: usize = 5;
/// A login this soon after a session ended means that session typed the right password.
pub const LOGIN_GRACE_SECS: u64 = 120;
/// Input needs a frame sent this recently.
pub const VIDEO_FRESH: Duration = Duration::from_millis(1000);

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// [`PER_HOUR`] sessions in the last hour.
    Hourly,
    /// Too many sessions without a login; allow the phone again from a session.
    Blocked,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Phone {
    /// Session start times (unix seconds), last hour only.
    starts: Vec<u64>,
    /// End times of sessions no login followed, last day only.
    strikes: Vec<u64>,
    blocked: bool,
}

/// Per-phone session limits, keyed by the phone's id (hex). Persisted so a reboot doesn't
/// reset them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Limits {
    phones: BTreeMap<String, Phone>,
}

impl Limits {
    pub fn load(path: &Path) -> Result<Self> {
        match read_private(path)? {
            Some(raw) => toml::from_str(std::str::from_utf8(&raw)?).context("parsing limits"),
            None => Ok(Self::default()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_private(path, toml::to_string(self)?.as_bytes())
    }

    fn prune(p: &mut Phone, now: u64) {
        p.starts.retain(|&t| now.saturating_sub(t) < HOUR);
        p.strikes.retain(|&t| now.saturating_sub(t) < DAY);
    }

    /// May this phone start a session now?
    pub fn check(&mut self, id: &str, now: u64) -> Result<(), Refused> {
        let p = self.phones.entry(id.to_owned()).or_default();
        Self::prune(p, now);
        if p.blocked {
            Err(Refused::Blocked)
        } else if p.starts.len() >= PER_HOUR {
            Err(Refused::Hourly)
        } else {
            Ok(())
        }
    }

    pub fn started(&mut self, id: &str, now: u64) {
        self.phones
            .entry(id.to_owned())
            .or_default()
            .starts
            .push(now);
    }

    /// A session ended; it counts against the phone unless a login follows soon.
    pub fn ended(&mut self, id: &str, now: u64) {
        let p = self.phones.entry(id.to_owned()).or_default();
        Self::prune(p, now);
        p.strikes.push(now);
        if p.strikes.len() >= STRIKES_PER_DAY {
            p.blocked = true;
        }
    }

    /// Someone logged in at the PC: the session(s) that just ended were the right password.
    pub fn logged_in(&mut self, now: u64) {
        for p in self.phones.values_mut() {
            p.strikes
                .retain(|&t| now.saturating_sub(t) > LOGIN_GRACE_SECS || t > now);
        }
    }

    /// `sudo dishus boot allow` again: a fresh start for this phone.
    pub fn reset(&mut self, id: &str) {
        self.phones.remove(id);
    }

    pub fn is_blocked(&self, id: &str) -> bool {
        self.phones.get(id).is_some_and(|p| p.blocked)
    }
}

/// What seat0 shows right now, from logind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seat {
    /// The login screen, on this X display.
    Greeter { display: String },
    /// A logged-in session (or nothing): login mode is idle.
    Other,
}

/// Map logind's view of the active session to [`Seat`]. `class` is the session's Class
/// ("greeter", "user", ...), `display` its X11 display (empty on Wayland).
pub fn seat_from(class: Option<&str>, display: Option<&str>) -> Seat {
    match (class, display) {
        (Some("greeter"), Some(d)) if valid_display(d) => Seat::Greeter {
            display: d.to_owned(),
        },
        _ => Seat::Other,
    }
}

/// ":0", ":1.0"; nothing else ends up in the worker's DISPLAY.
fn valid_display(d: &str) -> bool {
    d.strip_prefix(':').is_some_and(|rest| {
        !rest.is_empty() && rest.len() <= 8 && rest.chars().all(|c| c.is_ascii_digit() || c == '.')
    })
}

/// Login-mode state shared by the gate, the sessions and the seat watcher.
pub struct BootState {
    seat: Mutex<Seat>,
    limits: Mutex<Limits>,
    limits_file: PathBuf,
    /// The PC owner's uid: may revoke, kill and unlock through the control socket (root may
    /// do everything).
    pub owner_uid: Option<u32>,
    /// One line per event, readable by the owner (`$RUNTIME_DIRECTORY/history`).
    history_file: Option<PathBuf>,
}

impl BootState {
    pub fn new(
        limits_file: PathBuf,
        owner_uid: Option<u32>,
        history_file: Option<PathBuf>,
    ) -> Result<Self> {
        Ok(Self {
            seat: Mutex::new(Seat::Other),
            limits: Mutex::new(Limits::load(&limits_file)?),
            limits_file,
            owner_uid,
            history_file,
        })
    }

    pub fn seat(&self) -> Seat {
        self.seat.lock().expect("seat").clone()
    }

    /// Returns true if it changed.
    pub fn set_seat(&self, s: Seat) -> bool {
        let mut g = self.seat.lock().expect("seat");
        let changed = *g != s;
        *g = s;
        changed
    }

    pub fn greeter_display(&self) -> Option<String> {
        match self.seat() {
            Seat::Greeter { display } => Some(display),
            Seat::Other => None,
        }
    }

    fn with_limits<T>(&self, f: impl FnOnce(&mut Limits) -> T) -> T {
        let mut l = self.limits.lock().expect("limits");
        let before = l.clone();
        let r = f(&mut l);
        if *l != before
            && let Err(e) = l.save(&self.limits_file)
        {
            tracing::warn!("login mode: saving limits: {e:#}");
        }
        r
    }

    /// May this (allowed) phone start a session now? Logs why not.
    pub fn may_start(&self, id: &EndpointId) -> bool {
        if self.greeter_display().is_none() {
            tracing::info!(
                "login mode: refused {}: not at the login screen",
                id.fmt_short()
            );
            return false;
        }
        let now = now_unix();
        match self.with_limits(|l| l.check(&id.to_string(), now)) {
            Ok(()) => true,
            Err(r) => {
                tracing::warn!("login mode: refused {}: {r:?}", id.fmt_short());
                false
            }
        }
    }

    /// May a running session go on?
    pub fn may_stay(&self, id: &EndpointId) -> bool {
        self.greeter_display().is_some()
            && !self
                .limits
                .lock()
                .expect("limits")
                .is_blocked(&id.to_string())
    }

    pub fn started(&self, id: &EndpointId, name: &str) {
        let now = now_unix();
        self.with_limits(|l| l.started(&id.to_string(), now));
        self.record(&format!("{now} start {name}"));
    }

    pub fn ended(&self, id: &EndpointId, name: &str) {
        let now = now_unix();
        let blocked = self.with_limits(|l| {
            l.ended(&id.to_string(), now);
            l.is_blocked(&id.to_string())
        });
        self.record(&format!("{now} end {name}"));
        if blocked {
            tracing::warn!(
                "login mode: {name} is blocked (too many sessions with no login); allow it again from a session"
            );
            self.record(&format!("{now} blocked {name}"));
        }
    }

    pub fn logged_in(&self) {
        let now = now_unix();
        self.with_limits(|l| l.logged_in(now));
        self.record(&format!("{now} login"));
    }

    pub fn reset(&self, id: &EndpointId) {
        self.with_limits(|l| l.reset(&id.to_string()));
    }

    fn record(&self, line: &str) {
        tracing::info!("login mode: {line}");
        let Some(f) = &self.history_file else { return };
        let res = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut o = std::fs::OpenOptions::new();
            o.create(true).append(true);
            let mut file = o.open(f)?;
            // the owner reads it (the daemon's umask would leave it 0600)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
            }
            writeln!(file, "{line}")
        })();
        if let Err(e) = res {
            tracing::debug!("login mode history: {e}");
        }
    }
}

/// Capture for login mode: the capture worker, pointed at the login screen's X display (and
/// never a portal). Fails when seat0 isn't showing the login screen. No sound.
#[cfg(not(windows))]
pub struct BootCapture {
    pub worker: crate::capture::client::WorkerProvider,
    pub boot: Arc<BootState>,
}

#[cfg(not(windows))]
impl crate::backend::CaptureProvider for BootCapture {
    fn open(&self) -> crate::backend::BoxFuture<Result<Box<dyn crate::backend::CaptureSession>>> {
        match self.boot.greeter_display() {
            Some(d) => self
                .worker
                .clone()
                .with_env("DISPLAY", &d)
                .with_env("DISHUS_W_X11_ONLY", "1")
                .open(),
            None => Box::pin(async { Err(anyhow::anyhow!("not at the login screen")) }),
        }
    }
    fn setup_interactive(&self) -> crate::backend::BoxFuture<Result<String>> {
        Box::pin(async { Err(anyhow::anyhow!("login mode needs no capture setup")) })
    }
}

/// Follow seat0 through logind: login screen or not. Leaving the login screen ends every
/// session and closes the virtual keyboard; a user session appearing counts as a login.
#[cfg(not(windows))]
pub async fn watch_seat(state: Arc<AppState>) {
    let Some(boot) = state.boot.clone() else {
        return;
    };
    let mut conn = None;
    let mut was_user = false;
    loop {
        if conn.is_none() {
            conn = zbus::Connection::system()
                .await
                .map_err(|e| tracing::warn!("login mode: no system bus: {e}"))
                .ok();
        }
        let (seat, user) = match &conn {
            Some(c) => read_seat(c).await.unwrap_or_else(|e| {
                tracing::debug!("login mode: logind: {e:#}");
                (Seat::Other, false)
            }),
            None => (Seat::Other, false),
        };
        let before = boot.greeter_display();
        if boot.set_seat(seat.clone()) {
            tracing::info!("login mode: seat0 now {seat:?}");
            if let Some(d) = before {
                drop_x_access(&d).await;
            }
            if seat == Seat::Other {
                let n = state.end_all_sessions(dishus_proto::ByeReason::Normal);
                if n > 0 {
                    tracing::info!("login mode: left the login screen; ended {n} session(s)");
                }
                state.input.lock().expect("input lock").close_device();
            }
        }
        if user && !was_user {
            boot.logged_in();
        }
        was_user = user;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Give back the X access SDDM's Xsetup granted us (`xhost +SI:localuser:dishus-boot`), so a
/// user's X11 session on the same display is never open to this user. Best effort: the X
/// server usually goes away with the login screen anyway.
#[cfg(not(windows))]
async fn drop_x_access(disp: &str) {
    let Ok(me) = std::env::var("USER") else {
        return;
    };
    let run = tokio::process::Command::new("xhost")
        .arg(format!("-SI:localuser:{me}"))
        .env("DISPLAY", disp)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match tokio::time::timeout(Duration::from_secs(3), run).await {
        Ok(Ok(st)) if st.success() => tracing::info!("login mode: gave back X access to {disp}"),
        other => tracing::debug!("login mode: xhost -SI on {disp}: {other:?}"),
    }
}

/// (seat, is a user session active)
#[cfg(not(windows))]
async fn read_seat(c: &zbus::Connection) -> Result<(Seat, bool)> {
    use zbus::OwnedObjectPath;
    let seat = zbus::Proxy::new(
        c,
        "org.freedesktop.login1",
        "/org/freedesktop/login1/seat/seat0",
        "org.freedesktop.login1.Seat",
    )
    .await?;
    let (_, path): (String, OwnedObjectPath) = seat.get_property("ActiveSession").await?;
    if path.as_str() == "/" {
        return Ok((Seat::Other, false));
    }
    let s = zbus::Proxy::new(
        c,
        "org.freedesktop.login1",
        path,
        "org.freedesktop.login1.Session",
    )
    .await?;
    let class: String = s.get_property("Class").await?;
    let display: String = s.get_property("Display").await?;
    Ok((seat_from(Some(&class), Some(&display)), class == "user"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaa";
    const B: &str = "bbbb";

    #[test]
    fn three_sessions_an_hour() {
        let mut l = Limits::default();
        let t = 1_000_000;
        for i in 0..PER_HOUR as u64 {
            assert_eq!(l.check(A, t + i * 60), Ok(()));
            l.started(A, t + i * 60);
        }
        assert_eq!(l.check(A, t + 300), Err(Refused::Hourly));
        assert_eq!(l.check(B, t + 300), Ok(()), "per phone");
        assert_eq!(
            l.check(A, t + HOUR + 1),
            Ok(()),
            "an hour after the first one"
        );
    }

    #[test]
    fn sessions_without_a_login_block_the_phone() {
        let mut l = Limits::default();
        let mut t = 1_000_000;
        for _ in 0..STRIKES_PER_DAY - 1 {
            l.started(A, t);
            l.ended(A, t + 60);
            t += HOUR; // stay under the hourly limit
            assert_eq!(l.check(A, t), Ok(()));
        }
        l.started(A, t);
        l.ended(A, t + 60);
        assert_eq!(l.check(A, t + HOUR), Err(Refused::Blocked));
        // stays blocked, even a day later, until allowed again
        assert_eq!(l.check(A, t + 3 * DAY), Err(Refused::Blocked));
        l.reset(A);
        assert_eq!(l.check(A, t + 3 * DAY), Ok(()));
    }

    #[test]
    fn a_login_right_after_clears_that_session() {
        let mut l = Limits::default();
        let mut t = 1_000_000;
        for _ in 0..STRIKES_PER_DAY + 2 {
            l.started(A, t);
            l.ended(A, t + 60);
            l.logged_in(t + 90); // the password worked
            t += HOUR;
        }
        assert_eq!(l.check(A, t), Ok(()));
        // a login long after doesn't wipe older failures
        let mut l = Limits::default();
        l.started(A, t);
        l.ended(A, t + 60);
        l.logged_in(t + 60 + LOGIN_GRACE_SECS + 1);
        assert!(!l.phones[A].strikes.is_empty());
    }

    #[test]
    fn strikes_expire_after_a_day() {
        let mut l = Limits::default();
        let t = 1_000_000;
        for i in 0..STRIKES_PER_DAY as u64 - 1 {
            l.ended(A, t + i);
        }
        l.ended(A, t + DAY + 10); // the old ones are gone by now
        assert_eq!(l.check(A, t + DAY + 20), Ok(()));
    }

    #[test]
    fn survives_a_restart() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("limits.toml");
        let mut l = Limits::default();
        for i in 0..STRIKES_PER_DAY as u64 {
            l.ended(A, 1_000_000 + i);
        }
        l.save(&f).unwrap();
        let mut back = Limits::load(&f).unwrap();
        assert_eq!(back, l);
        assert_eq!(back.check(A, 1_000_100), Err(Refused::Blocked));
        assert_eq!(
            Limits::load(&d.path().join("none")).unwrap(),
            Limits::default()
        );
    }

    #[test]
    fn only_the_greeter_counts() {
        assert_eq!(
            seat_from(Some("greeter"), Some(":0")),
            Seat::Greeter {
                display: ":0".into()
            }
        );
        assert_eq!(seat_from(Some("user"), Some(":0")), Seat::Other);
        assert_eq!(
            seat_from(Some("greeter"), Some("")),
            Seat::Other,
            "Wayland greeter"
        );
        assert_eq!(
            seat_from(Some("greeter"), Some(":0; rm -rf /")),
            Seat::Other
        );
        assert_eq!(seat_from(None, None), Seat::Other);
    }
}
