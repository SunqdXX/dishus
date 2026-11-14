//! The pairing URI carried by the QR code:
//!
//! ```text
//! dishus://pair?v=1&id=<base32 endpoint id>&t=<base32 token>&n=<pc name>&r=<relay url>...&a=<ip:port>...
//! ```
//!
//! The token is a secret: it is only shown on the PC screen and lives for 60 seconds.

use data_encoding::BASE32_NOPAD;

use crate::{MAX_NAME_LEN, TOKEN_LEN, valid_name};

const SCHEME_PREFIX: &str = "dishus://pair?";
const MAX_RELAYS: usize = 8;
const MAX_ADDRS: usize = 8;
const MAX_URI_LEN: usize = 2048;

#[derive(Clone, PartialEq, Eq)]
pub struct PairingInfo {
    /// The PC's iroh EndpointId (ed25519 public key bytes).
    pub endpoint_id: [u8; 32],
    pub token: [u8; TOKEN_LEN],
    pub pc_name: String,
    /// Relay URL hints (the PC's configured relays).
    pub relays: Vec<String>,
    /// Direct address hints (`ip:port`), useful on a LAN without internet.
    pub addrs: Vec<String>,
}

impl std::fmt::Debug for PairingInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingInfo")
            .field("endpoint_id", &BASE32_NOPAD.encode(&self.endpoint_id))
            .field("pc_name", &self.pc_name)
            .field("relays", &self.relays)
            .field("addrs", &self.addrs)
            .finish_non_exhaustive()
    }
}

impl Drop for PairingInfo {
    fn drop(&mut self) {
        self.token.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&self.token);
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PairingUriError {
    #[error("not a dishus pairing link")]
    Scheme,
    #[error("unsupported pairing link version")]
    Version,
    #[error("missing or malformed field: {0}")]
    Field(&'static str),
    #[error("pairing link too long")]
    TooLong,
}

impl PairingInfo {
    pub fn to_uri(&self) -> String {
        let mut s = String::from(SCHEME_PREFIX);
        s.push_str("v=1&id=");
        s.push_str(&BASE32_NOPAD.encode(&self.endpoint_id));
        s.push_str("&t=");
        s.push_str(&BASE32_NOPAD.encode(&self.token));
        s.push_str("&n=");
        s.push_str(&pct_encode(&self.pc_name));
        for r in &self.relays {
            s.push_str("&r=");
            s.push_str(&pct_encode(r));
        }
        for a in &self.addrs {
            s.push_str("&a=");
            s.push_str(&pct_encode(a));
        }
        s
    }

    pub fn from_uri(uri: &str) -> Result<Self, PairingUriError> {
        if uri.len() > MAX_URI_LEN {
            return Err(PairingUriError::TooLong);
        }
        let query = uri
            .trim()
            .strip_prefix(SCHEME_PREFIX)
            .ok_or(PairingUriError::Scheme)?;
        let mut version = None;
        let mut id = None;
        let mut token = None;
        let mut name = None;
        let mut relays = Vec::new();
        let mut addrs = Vec::new();
        for pair in query.split('&') {
            let (k, v) = pair
                .split_once('=')
                .ok_or(PairingUriError::Field("query"))?;
            match k {
                "v" => version = Some(v),
                "id" => id = Some(decode32(v).ok_or(PairingUriError::Field("id"))?),
                "t" => token = Some(decode32(v).ok_or(PairingUriError::Field("t"))?),
                "n" => name = Some(pct_decode(v).ok_or(PairingUriError::Field("n"))?),
                "r" => {
                    let r = pct_decode(v).ok_or(PairingUriError::Field("r"))?;
                    if !(r.starts_with("https://") || r.starts_with("http://")) || r.len() > 256 {
                        return Err(PairingUriError::Field("r"));
                    }
                    relays.push(r);
                }
                "a" => {
                    let a = pct_decode(v).ok_or(PairingUriError::Field("a"))?;
                    if a.parse::<std::net::SocketAddr>().is_err() {
                        return Err(PairingUriError::Field("a"));
                    }
                    addrs.push(a);
                }
                _ => {} // forward compatible: ignore unknown keys
            }
        }
        if version != Some("1") {
            return Err(PairingUriError::Version);
        }
        if relays.len() > MAX_RELAYS || addrs.len() > MAX_ADDRS {
            return Err(PairingUriError::TooLong);
        }
        let pc_name = name.ok_or(PairingUriError::Field("n"))?;
        if !valid_name(&pc_name) || pc_name.len() > MAX_NAME_LEN {
            return Err(PairingUriError::Field("n"));
        }
        Ok(PairingInfo {
            endpoint_id: id.ok_or(PairingUriError::Field("id"))?,
            token: token.ok_or(PairingUriError::Field("t"))?,
            pc_name,
            relays,
            addrs,
        })
    }
}

/// What `dishus boot allow` shows: a one-time pairing window on the logged-in PC (for the
/// phone's new login-screen identity) plus where to find login mode, which has its own id:
///
/// ```text
/// dishus://login?v=1&id=..&t=..&n=..&r=..&a=..&b=<base32 login-mode id>&br=<relay>..&ba=<ip:port>..
/// ```
///
/// Secret like a pairing link (the token), single use, short-lived.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoginInfo {
    pub pair: PairingInfo,
    pub boot_id: [u8; 32],
    pub boot_relays: Vec<String>,
    pub boot_addrs: Vec<String>,
}

const LOGIN_PREFIX: &str = "dishus://login?";

impl LoginInfo {
    pub fn to_uri(&self) -> String {
        let pair = self.pair.to_uri();
        let mut s = String::from(LOGIN_PREFIX);
        s.push_str(pair.strip_prefix(SCHEME_PREFIX).unwrap_or(&pair));
        s.push_str("&b=");
        s.push_str(&BASE32_NOPAD.encode(&self.boot_id));
        for r in &self.boot_relays {
            s.push_str("&br=");
            s.push_str(&pct_encode(r));
        }
        for a in &self.boot_addrs {
            s.push_str("&ba=");
            s.push_str(&pct_encode(a));
        }
        s
    }

    pub fn from_uri(uri: &str) -> Result<Self, PairingUriError> {
        if uri.len() > MAX_URI_LEN {
            return Err(PairingUriError::TooLong);
        }
        let query = uri
            .trim()
            .strip_prefix(LOGIN_PREFIX)
            .ok_or(PairingUriError::Scheme)?;
        // the pairing part ignores the b/br/ba keys
        let pair = PairingInfo::from_uri(&format!("{SCHEME_PREFIX}{query}"))?;
        let mut boot_id = None;
        let (mut boot_relays, mut boot_addrs) = (Vec::new(), Vec::new());
        for kv in query.split('&') {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            match k {
                "b" => boot_id = Some(decode32(v).ok_or(PairingUriError::Field("b"))?),
                "br" => {
                    let r = pct_decode(v).ok_or(PairingUriError::Field("br"))?;
                    if !(r.starts_with("https://") || r.starts_with("http://")) || r.len() > 256 {
                        return Err(PairingUriError::Field("br"));
                    }
                    boot_relays.push(r);
                }
                "ba" => {
                    let a = pct_decode(v).ok_or(PairingUriError::Field("ba"))?;
                    if a.parse::<std::net::SocketAddr>().is_err() {
                        return Err(PairingUriError::Field("ba"));
                    }
                    boot_addrs.push(a);
                }
                _ => {}
            }
        }
        if boot_relays.len() > MAX_RELAYS || boot_addrs.len() > MAX_ADDRS {
            return Err(PairingUriError::TooLong);
        }
        Ok(LoginInfo {
            pair,
            boot_id: boot_id.ok_or(PairingUriError::Field("b"))?,
            boot_relays,
            boot_addrs,
        })
    }
}

fn decode32(s: &str) -> Option<[u8; 32]> {
    let v = BASE32_NOPAD.decode(s.as_bytes()).ok()?;
    v.try_into().ok()
}

fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PairingInfo {
        PairingInfo {
            endpoint_id: [7; 32],
            token: [9; 32],
            pc_name: "alice's desk & co".into(),
            relays: vec!["https://euc1-1.relay.n0.iroh-canary.iroh.link./".into()],
            addrs: vec!["192.168.1.10:41641".into(), "[fe80::1]:5000".into()],
        }
    }

    #[test]
    fn login_roundtrip() {
        let l = LoginInfo {
            pair: sample(),
            boot_id: [3; 32],
            boot_relays: vec!["https://relay.example/".into()],
            boot_addrs: vec!["10.0.0.2:4000".into()],
        };
        let uri = l.to_uri();
        assert!(uri.starts_with("dishus://login?v=1&"));
        assert_eq!(LoginInfo::from_uri(&uri).unwrap(), l);
        // a pairing link is not a login code, and the other way round
        assert_eq!(
            LoginInfo::from_uri(&sample().to_uri()),
            Err(PairingUriError::Scheme)
        );
        assert_eq!(PairingInfo::from_uri(&uri), Err(PairingUriError::Scheme));
        // no login-mode id
        let no_b = uri.split("&b=").next().unwrap();
        assert_eq!(LoginInfo::from_uri(no_b), Err(PairingUriError::Field("b")));
    }

    #[test]
    fn roundtrip() {
        let p = sample();
        let uri = p.to_uri();
        assert!(uri.starts_with("dishus://pair?v=1&"));
        assert_eq!(PairingInfo::from_uri(&uri).unwrap(), p);
    }

    #[test]
    fn rejects() {
        assert_eq!(
            PairingInfo::from_uri("https://x"),
            Err(PairingUriError::Scheme)
        );
        let uri = sample().to_uri().replace("v=1", "v=2");
        assert_eq!(PairingInfo::from_uri(&uri), Err(PairingUriError::Version));
        let uri = sample().to_uri().replace("&t=", "&t=AA");
        assert!(PairingInfo::from_uri(&uri).is_err());
        let uri = sample().to_uri() + "&r=javascript%3Aalert(1)";
        assert_eq!(
            PairingInfo::from_uri(&uri),
            Err(PairingUriError::Field("r"))
        );
        let uri = sample().to_uri() + "&a=notanaddr";
        assert_eq!(
            PairingInfo::from_uri(&uri),
            Err(PairingUriError::Field("a"))
        );
        assert_eq!(
            PairingInfo::from_uri(&"a".repeat(5000)),
            Err(PairingUriError::TooLong)
        );
    }

    #[test]
    fn debug_hides_token() {
        let s = format!("{:?}", sample());
        assert!(!s.contains(&BASE32_NOPAD.encode(&[9u8; 32])));
    }
}
