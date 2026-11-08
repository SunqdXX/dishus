//! Allowlist of paired phones (`devices.toml`). A phone is identified *only* by its iroh
//! EndpointId (ed25519 public key, proven by the QUIC/TLS handshake).

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use dishus_proto::{MAX_EXPIRY_DAYS, valid_name};

use crate::paths::{Paths, read_private, write_private};

pub const DAY_SECS: u64 = 86_400;
/// Hard cap on stored devices; pairing beyond this is refused.
pub const MAX_DEVICES: usize = 32;

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    /// Hex EndpointId.
    pub id: String,
    pub name: String,
    pub paired_at: u64,
    pub expires_at: u64,
    #[serde(default)]
    pub last_seen: u64,
}

impl Device {
    pub fn endpoint_id(&self) -> Option<EndpointId> {
        self.id.parse().ok()
    }
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceStore {
    #[serde(default, rename = "device")]
    devices: Vec<Device>,
}

impl DeviceStore {
    pub fn load(paths: &Paths) -> Result<Self> {
        let Some(raw) = read_private(&paths.devices_file())? else {
            return Ok(Self::default());
        };
        let s = String::from_utf8(raw).context("devices.toml is not UTF-8")?;
        let store: Self = toml::from_str(&s).context("parsing devices.toml")?;
        for d in &store.devices {
            if d.endpoint_id().is_none() || !valid_name(&d.name) {
                bail!(
                    "devices.toml has an invalid entry ({}); fix or remove it",
                    d.id
                );
            }
        }
        Ok(store)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let header =
            "# DiShus paired devices. Managed by `dishus`; edits are picked up on restart.\n";
        let body = toml::to_string_pretty(self)?;
        write_private(&paths.devices_file(), format!("{header}{body}").as_bytes())
    }

    pub fn list(&self) -> &[Device] {
        &self.devices
    }

    pub fn get(&self, id: &EndpointId) -> Option<&Device> {
        let hex = id.to_string();
        self.devices.iter().find(|d| d.id == hex)
    }

    /// The single question the network gate asks.
    pub fn is_allowed(&self, id: &EndpointId, now: u64) -> bool {
        self.get(id).is_some_and(|d| !d.is_expired(now))
    }

    /// Add (or re-pair) a device. Re-pairing an existing id refreshes name and expiry.
    pub fn add(&mut self, id: &EndpointId, name: &str, days: u16, now: u64) -> Result<u64> {
        if !valid_name(name) {
            bail!("invalid device name");
        }
        let days = days.clamp(1, MAX_EXPIRY_DAYS);
        let expires_at = now + u64::from(days) * DAY_SECS;
        let hex = id.to_string();
        if let Some(d) = self.devices.iter_mut().find(|d| d.id == hex) {
            d.name = name.to_owned();
            d.paired_at = now;
            d.expires_at = expires_at;
            return Ok(expires_at);
        }
        // Make room by dropping expired entries first.
        self.devices.retain(|d| !d.is_expired(now));
        if self.devices.len() >= MAX_DEVICES {
            bail!("too many paired devices ({MAX_DEVICES}); revoke one first");
        }
        self.devices.push(Device {
            id: hex,
            name: name.to_owned(),
            paired_at: now,
            expires_at,
            last_seen: 0,
        });
        Ok(expires_at)
    }

    /// Extend a device to now + days. Works on expired devices too (CLI renew); the session
    /// path can only reach this for non-expired devices since expired ones can't connect.
    pub fn renew(&mut self, id: &EndpointId, days: u16, now: u64) -> Result<u64> {
        if !(1..=MAX_EXPIRY_DAYS).contains(&days) {
            bail!("days must be within 1..={MAX_EXPIRY_DAYS}");
        }
        let hex = id.to_string();
        let d = self
            .devices
            .iter_mut()
            .find(|d| d.id == hex)
            .context("unknown device")?;
        d.expires_at = now + u64::from(days) * DAY_SECS;
        Ok(d.expires_at)
    }

    pub fn touch(&mut self, id: &EndpointId, now: u64) {
        let hex = id.to_string();
        if let Some(d) = self.devices.iter_mut().find(|d| d.id == hex) {
            d.last_seen = now;
        }
    }

    /// Remove by full hex id, unique id prefix (≥ 6 chars) or exact name.
    pub fn revoke(&mut self, selector: &str) -> Result<Device> {
        let idx = self.resolve(selector)?;
        Ok(self.devices.remove(idx))
    }

    pub fn resolve(&self, selector: &str) -> Result<usize> {
        let sel = selector.trim();
        if sel.is_empty() {
            bail!("empty device selector");
        }
        let by_name: Vec<_> = self
            .devices
            .iter()
            .enumerate()
            .filter(|(_, d)| d.name == sel)
            .collect();
        if by_name.len() == 1 {
            return Ok(by_name[0].0);
        }
        let lower = sel.to_ascii_lowercase();
        if lower.len() >= 6 {
            let by_id: Vec<_> = self
                .devices
                .iter()
                .enumerate()
                .filter(|(_, d)| d.id.starts_with(&lower))
                .collect();
            if by_id.len() == 1 {
                return Ok(by_id[0].0);
            }
            if by_id.len() > 1 {
                bail!("'{sel}' matches several devices; use a longer id");
            }
        }
        if by_name.len() > 1 {
            bail!("several devices are named '{sel}'; use the id");
        }
        bail!("no device matches '{sel}'");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    fn id(n: u8) -> EndpointId {
        SecretKey::from_bytes(&[n; 32]).public()
    }

    #[test]
    fn allow_expire_renew_revoke() {
        let mut s = DeviceStore::default();
        let now = 1_000_000;
        let a = id(1);
        assert!(!s.is_allowed(&a, now), "unknown peer");
        let exp = s.add(&a, "phone", 30, now).unwrap();
        assert_eq!(exp, now + 30 * DAY_SECS);
        assert!(s.is_allowed(&a, now));
        assert!(s.is_allowed(&a, exp - 1));
        assert!(!s.is_allowed(&a, exp), "expired exactly at expires_at");
        // renew after expiry (CLI path)
        let exp2 = s.renew(&a, 60, exp + 10).unwrap();
        assert_eq!(exp2, exp + 10 + 60 * DAY_SECS);
        assert!(s.is_allowed(&a, exp + 11));
        assert!(s.renew(&a, MAX_EXPIRY_DAYS + 1, now).is_err());
        assert!(s.renew(&id(2), 10, now).is_err());
        s.revoke("phone").unwrap();
        assert!(!s.is_allowed(&a, now));
    }

    #[test]
    fn add_clamps_days() {
        let mut s = DeviceStore::default();
        let exp = s.add(&id(1), "p", 999, 0).unwrap();
        assert_eq!(exp, u64::from(MAX_EXPIRY_DAYS) * DAY_SECS);
    }

    #[test]
    fn selectors() {
        let mut s = DeviceStore::default();
        s.add(&id(1), "same", 30, 0).unwrap();
        s.add(&id(2), "same", 30, 0).unwrap();
        assert!(s.resolve("same").is_err(), "ambiguous name");
        let full = id(2).to_string();
        assert_eq!(s.resolve(&full[..10]).unwrap(), 1);
        assert!(s.resolve(&full[..3]).is_err(), "prefix too short");
        assert!(s.resolve("").is_err());
    }

    #[test]
    fn persist() {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::under(t.path());
        let mut s = DeviceStore::default();
        s.add(&id(1), "phone", 30, 5).unwrap();
        s.save(&p).unwrap();
        assert_eq!(DeviceStore::load(&p).unwrap(), s);
    }

    #[test]
    fn rejects_tampered_file() {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::under(t.path());
        write_private(
            &p.devices_file(),
            b"[[device]]\nid='zz'\nname='x'\npaired_at=0\nexpires_at=99999999999\n",
        )
        .unwrap();
        assert!(DeviceStore::load(&p).is_err());
    }

    #[test]
    fn capacity() {
        let mut s = DeviceStore::default();
        for i in 0..MAX_DEVICES as u8 {
            s.add(&id(i + 1), "p", 1, 0).unwrap();
        }
        assert!(s.add(&id(200), "p", 1, 0).is_err());
        // once they've expired there is room again
        s.add(&id(200), "p", 1, 2 * DAY_SECS).unwrap();
    }
}
