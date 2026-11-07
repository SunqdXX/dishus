//! `config.toml`. Every field has a default, so the daemon works with no config at all.

use serde::{Deserialize, Serialize};

use anyhow::{Context, Result, bail};
use dishus_proto::{DEFAULT_EXPIRY_DAYS, MAX_EXPIRY_DAYS, valid_name};

use crate::paths::{Paths, read_private, write_private};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Name shown on the phone. Defaults to the hostname.
    pub pc_name: String,
    /// Relay servers. Empty = iroh's public default relays.
    /// Example: `relays = ["https://relay.example.org/"]`.
    pub relays: Vec<String>,
    /// Disable relays entirely (direct / LAN connections only).
    pub disable_relays: bool,
    /// Lifetime of a new pairing, days (1..=60).
    pub expiry_days: u16,
    /// Pairing window length, seconds (10..=300).
    pub pairing_ttl_secs: u64,
    /// Fixed UDP port for direct connections (0 = random). Not a listening TCP port; QUIC only
    /// answers peers that pass the allowlist gate. Set it only if you forward it on your router.
    pub udp_port: u16,
    pub video: VideoConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct VideoConfig {
    /// "auto", "nvenc", "vaapi", "x264", "openh264"
    pub encoder: String,
    /// Absolute ceiling for the adaptive bitrate controller (kbit/s).
    pub max_bitrate_kbps: u32,
    /// Show the cursor in the captured image.
    pub show_cursor: bool,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            encoder: "auto".into(),
            max_bitrate_kbps: 20_000,
            show_cursor: true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pc_name: hostname(),
            relays: Vec::new(),
            disable_relays: false,
            expiry_days: DEFAULT_EXPIRY_DAYS,
            pairing_ttl_secs: 60,
            udp_port: 0,
            video: VideoConfig::default(),
        }
    }
}

pub fn hostname() -> String {
    let raw = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| valid_name(s))
        .unwrap_or_else(|| {
            rustix::system::uname()
                .nodename()
                .to_string_lossy()
                .into_owned()
        });
    let mut s: String = raw.chars().filter(|c| !c.is_control()).take(64).collect();
    if s.is_empty() {
        s = "Linux PC".into();
    }
    s
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        if !valid_name(&self.pc_name) {
            bail!("pc_name must be 1..=64 printable characters");
        }
        if !(1..=MAX_EXPIRY_DAYS).contains(&self.expiry_days) {
            bail!("expiry_days must be within 1..={MAX_EXPIRY_DAYS}");
        }
        if !(10..=300).contains(&self.pairing_ttl_secs) {
            bail!("pairing_ttl_secs must be within 10..=300");
        }
        for r in &self.relays {
            let u: iroh::RelayUrl = r.parse().with_context(|| format!("bad relay url {r}"))?;
            if u.scheme() != "https" && u.scheme() != "http" {
                bail!("relay url must be http(s): {r}");
            }
        }
        if !["auto", "nvenc", "vaapi", "x264", "openh264"].contains(&self.video.encoder.as_str()) {
            bail!("video.encoder must be one of auto, nvenc, vaapi, x264, openh264");
        }
        if !(300..=200_000).contains(&self.video.max_bitrate_kbps) {
            bail!("video.max_bitrate_kbps must be within 300..=200000");
        }
        Ok(())
    }

    pub fn load(paths: &Paths) -> Result<Self> {
        let cfg = match read_private(&paths.config_file())? {
            Some(raw) => {
                let s = String::from_utf8(raw).context("config.toml is not UTF-8")?;
                toml::from_str(&s).context("parsing config.toml")?
            }
            None => Config::default(),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        self.validate()?;
        write_private(
            &paths.config_file(),
            toml::to_string_pretty(self)?.as_bytes(),
        )
    }

    pub fn relay_mode(&self) -> Result<iroh::RelayMode> {
        if self.disable_relays {
            return Ok(iroh::RelayMode::Disabled);
        }
        if self.relays.is_empty() {
            return Ok(iroh::RelayMode::Default);
        }
        let urls = self
            .relays
            .iter()
            .map(|r| {
                r.parse::<iroh::RelayUrl>()
                    .with_context(|| format!("bad relay url {r}"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(iroh::RelayMode::custom(urls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn load_save() {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::under(t.path());
        assert_eq!(Config::load(&p).unwrap(), Config::default());
        let c = Config {
            relays: vec!["https://relay.example.org/".into()],
            expiry_days: 60,
            ..Default::default()
        };
        c.save(&p).unwrap();
        assert_eq!(Config::load(&p).unwrap(), c);
        assert!(matches!(
            c.relay_mode().unwrap(),
            iroh::RelayMode::Custom(_)
        ));
    }

    #[test]
    fn rejects_bad() {
        let c = Config {
            expiry_days: 61,
            ..Default::default()
        };
        assert!(c.validate().is_err());
        let c = Config {
            relays: vec!["ftp://x".into()],
            ..Default::default()
        };
        assert!(c.validate().is_err());
        assert!(toml::from_str::<Config>("bogus_key = 1").is_err());
    }
}
