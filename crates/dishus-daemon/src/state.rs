//! Shared daemon state. Locks are std (never held across `.await`) because the network gate
//! runs in synchronous filter callbacks.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use iroh::EndpointId;
use tokio::sync::watch;
use zeroize::Zeroizing;

use dishus_proto::{ByeReason, PairRequest, PairResponse, TOKEN_LEN};

use crate::{
    backend::{CaptureProvider, TrackedInput},
    config::Config,
    devices::{Device, DeviceStore, now_unix},
    notify::Notifier,
    pairing::{PairCheck, Pairing},
    paths::{Paths, write_private},
};

pub struct ActiveSession {
    pub id: u64,
    pub peer: EndpointId,
    pub name: String,
    pub since: u64,
    pub path: String,
    cancel: watch::Sender<Option<ByeReason>>,
}

/// Snapshot for `dishus status`.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub peer: EndpointId,
    pub name: String,
    pub since: u64,
    pub path: String,
}

pub struct AppState {
    pub paths: Paths,
    pub config: Config,
    pub devices: RwLock<DeviceStore>,
    pub pairing: Mutex<Pairing>,
    pub lockdown: AtomicBool,
    session: Mutex<Option<ActiveSession>>,
    next_session_id: AtomicU64,
    pub limiter: Mutex<RateLimiter>,
    pub notifier: Notifier,
    pub input: Mutex<TrackedInput>,
    pub capture: Arc<dyn CaptureProvider>,
    /// Persist devices.toml on changes (off in some tests).
    pub persist: bool,
}

impl AppState {
    pub fn new(
        paths: Paths,
        config: Config,
        devices: DeviceStore,
        input: TrackedInput,
        capture: Arc<dyn CaptureProvider>,
        notifier: Notifier,
    ) -> Self {
        let lockdown = paths.lockdown_file().exists();
        if lockdown {
            tracing::warn!("daemon is in LOCKDOWN (dishus kill); run `dishus unlock` to allow sessions");
        }
        Self {
            paths,
            config,
            devices: RwLock::new(devices),
            pairing: Mutex::new(Pairing::default()),
            lockdown: AtomicBool::new(lockdown),
            session: Mutex::new(None),
            next_session_id: AtomicU64::new(1),
            limiter: Mutex::new(RateLimiter::default()),
            notifier,
            input: Mutex::new(input),
            capture,
            persist: true,
        }
    }

    // ---- gate questions (sync, cheap) ----

    pub fn is_session_allowed(&self, id: &EndpointId) -> bool {
        !self.lockdown.load(Ordering::SeqCst)
            && self.devices.read().expect("devices lock").is_allowed(id, now_unix())
    }

    pub fn is_pairing_open(&self) -> bool {
        !self.lockdown.load(Ordering::SeqCst)
            && self.pairing.lock().expect("pairing lock").is_open(Instant::now())
    }

    pub fn device(&self, id: &EndpointId) -> Option<Device> {
        self.devices.read().expect("devices lock").get(id).cloned()
    }

    // ---- pairing ----

    pub fn open_pairing(&self) -> Result<(Zeroizing<[u8; TOKEN_LEN]>, Duration)> {
        if self.lockdown.load(Ordering::SeqCst) {
            anyhow::bail!("daemon is in lockdown; run `dishus unlock` first");
        }
        let ttl = Duration::from_secs(self.config.pairing_ttl_secs);
        let tok = self.pairing.lock().expect("pairing lock").open(ttl, Instant::now())?;
        Ok((tok, ttl))
    }

    pub fn close_pairing(&self) {
        self.pairing.lock().expect("pairing lock").close();
    }

    /// Handle a pairing request from a TLS-authenticated `peer`. The device is stored under the
    /// handshake-proven EndpointId, never anything the message claims.
    pub fn try_pair(&self, peer: &EndpointId, req: &PairRequest) -> PairResponse {
        if self.lockdown.load(Ordering::SeqCst) {
            return PairResponse::Rejected;
        }
        let verdict = self.pairing.lock().expect("pairing lock").check(&req.token, Instant::now());
        match verdict {
            PairCheck::Accepted => {}
            PairCheck::Rejected | PairCheck::Closed => {
                tracing::warn!("pairing attempt from {} rejected ({verdict:?})", peer.fmt_short());
                return PairResponse::Rejected;
            }
        }
        let now = now_unix();
        let res = {
            let mut d = self.devices.write().expect("devices lock");
            d.add(peer, &req.device_name, self.config.expiry_days, now).and_then(|exp| {
                if self.persist {
                    d.save(&self.paths)?;
                }
                Ok(exp)
            })
        };
        match res {
            Ok(expires_at) => {
                tracing::info!("paired {} ({})", req.device_name, peer.fmt_short());
                self.notifier.notify(
                    "DiShus: phone paired",
                    &format!("{} can now control this PC", req.device_name),
                );
                PairResponse::Accepted { pc_name: self.config.pc_name.clone(), expires_at }
            }
            Err(e) => {
                tracing::error!("storing paired device failed: {e:#}");
                PairResponse::Rejected
            }
        }
    }

    // ---- device management ----

    pub fn renew(&self, id: &EndpointId, days: u16) -> Result<u64> {
        let mut d = self.devices.write().expect("devices lock");
        let exp = d.renew(id, days, now_unix())?;
        if self.persist {
            d.save(&self.paths)?;
        }
        Ok(exp)
    }

    pub fn renew_selector(&self, selector: &str, days: u16) -> Result<(Device, u64)> {
        let id = {
            let d = self.devices.read().expect("devices lock");
            let idx = d.resolve(selector)?;
            d.list()[idx].clone()
        };
        let peer = id.endpoint_id().ok_or_else(|| anyhow::anyhow!("corrupt id"))?;
        let exp = self.renew(&peer, days)?;
        Ok((id, exp))
    }

    pub fn revoke(&self, selector: &str) -> Result<Device> {
        let dev = {
            let mut d = self.devices.write().expect("devices lock");
            let dev = d.revoke(selector)?;
            if self.persist {
                d.save(&self.paths)?;
            }
            dev
        };
        if let Some(id) = dev.endpoint_id() {
            self.end_sessions_of(&id, ByeReason::Revoked);
        }
        self.notifier.notify("DiShus: device revoked", &dev.name);
        Ok(dev)
    }

    pub fn touch(&self, id: &EndpointId) {
        let mut d = self.devices.write().expect("devices lock");
        d.touch(id, now_unix());
        if self.persist
            && let Err(e) = d.save(&self.paths)
        {
            tracing::warn!("saving last_seen: {e:#}");
        }
    }

    // ---- panic button ----

    pub fn kill(&self) -> Result<usize> {
        self.lockdown.store(true, Ordering::SeqCst);
        self.close_pairing();
        let n = self.end_all_sessions(ByeReason::Killed);
        self.release_input();
        write_private(&self.paths.lockdown_file(), b"locked by `dishus kill`\n")?;
        self.notifier.notify("DiShus: KILLED", "All sessions cut. Locked until `dishus unlock`.");
        Ok(n)
    }

    pub fn unlock(&self) -> Result<()> {
        match std::fs::remove_file(self.paths.lockdown_file()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        self.lockdown.store(false, Ordering::SeqCst);
        Ok(())
    }

    // ---- sessions ----

    /// Register a new active session, replacing (and ending) any existing one.
    pub fn register_session(
        &self,
        peer: EndpointId,
        name: String,
        path: String,
    ) -> (u64, watch::Receiver<Option<ByeReason>>) {
        let id = self.next_session_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = watch::channel(None);
        let old = self.session.lock().expect("session lock").replace(ActiveSession {
            id,
            peer,
            name,
            since: now_unix(),
            path,
            cancel: tx,
        });
        if let Some(old) = old {
            tracing::info!("session from {} replaced", old.name);
            let _ = old.cancel.send(Some(ByeReason::Replaced));
        }
        (id, rx)
    }

    /// Unregister if still the active session. Returns true if it was.
    pub fn unregister_session(&self, id: u64) -> bool {
        let mut s = self.session.lock().expect("session lock");
        if s.as_ref().is_some_and(|a| a.id == id) {
            *s = None;
            true
        } else {
            false
        }
    }

    pub fn set_session_path(&self, id: u64, path: String) {
        let mut s = self.session.lock().expect("session lock");
        if let Some(a) = s.as_mut().filter(|a| a.id == id) {
            a.path = path;
        }
    }

    pub fn end_all_sessions(&self, reason: ByeReason) -> usize {
        match self.session.lock().expect("session lock").take() {
            Some(a) => {
                let _ = a.cancel.send(Some(reason));
                1
            }
            None => 0,
        }
    }

    pub fn end_sessions_of(&self, peer: &EndpointId, reason: ByeReason) {
        let mut s = self.session.lock().expect("session lock");
        if s.as_ref().is_some_and(|a| a.peer == *peer)
            && let Some(a) = s.take()
        {
            let _ = a.cancel.send(Some(reason));
        }
    }

    pub fn sessions(&self) -> Vec<SessionInfo> {
        self.session
            .lock()
            .expect("session lock")
            .iter()
            .map(|a| SessionInfo { peer: a.peer, name: a.name.clone(), since: a.since, path: a.path.clone() })
            .collect()
    }

    pub fn release_input(&self) {
        self.input.lock().expect("input lock").release_all();
    }
}

/// Fixed-window per-IP limiter for incoming connection attempts (direct UDP only; relayed
/// connections are filtered by EndpointId instead).
#[derive(Default)]
pub struct RateLimiter {
    by_ip: HashMap<IpAddr, (Instant, u32)>,
}

impl RateLimiter {
    pub const WINDOW: Duration = Duration::from_secs(60);
    pub const MAX_PER_WINDOW: u32 = 30;
    const MAX_ENTRIES: usize = 4096;

    /// Record an attempt; returns false if the IP is over its budget.
    pub fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        if self.by_ip.len() >= Self::MAX_ENTRIES {
            self.by_ip.retain(|_, (start, _)| now.duration_since(*start) < Self::WINDOW);
            if self.by_ip.len() >= Self::MAX_ENTRIES {
                // under a flood from many IPs: refuse new ones rather than grow unbounded
                return self.by_ip.contains_key(&ip) && self.bump(ip, now);
            }
        }
        self.bump(ip, now)
    }

    fn bump(&mut self, ip: IpAddr, now: Instant) -> bool {
        let e = self.by_ip.entry(ip).or_insert((now, 0));
        if now.duration_since(e.0) >= Self::WINDOW {
            *e = (now, 0);
        }
        e.1 += 1;
        e.1 <= Self::MAX_PER_WINDOW
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter() {
        let mut l = RateLimiter::default();
        let ip: IpAddr = "203.0.113.5".parse().unwrap();
        let other: IpAddr = "203.0.113.6".parse().unwrap();
        let now = Instant::now();
        for _ in 0..RateLimiter::MAX_PER_WINDOW {
            assert!(l.allow(ip, now));
        }
        assert!(!l.allow(ip, now));
        assert!(l.allow(other, now), "independent per IP");
        assert!(l.allow(ip, now + RateLimiter::WINDOW), "window resets");
    }
}
