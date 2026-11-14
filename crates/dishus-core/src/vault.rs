//! Password-encrypted local store for the phone's secret key and paired PCs.
//!
//! `password --Argon2id--> 256-bit key --XChaCha20-Poly1305--> blob`. The header (magic, KDF
//! parameters, salt, nonce) is authenticated as associated data, so it can't be tampered with to
//! weaken the KDF. On Android the resulting blob is additionally wrapped with a hardware-backed
//! Android Keystore key (optionally biometric-gated) by the Kotlin layer.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

const MAGIC: &[u8; 4] = b"DSK1";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const HEADER_LEN: usize = 4 + 12 + SALT_LEN + NONCE_LEN;
const MAX_BLOB: usize = 1024 * 1024;

/// KDF cost. Defaults: 64 MiB, 3 passes, 1 lane (≈0.3–1 s on a mid-range phone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl Default for KdfParams {
    fn default() -> Self {
        Self {
            m_kib: 64 * 1024,
            t: 3,
            p: 1,
        }
    }
}

impl KdfParams {
    fn check(&self) -> Result<(), VaultError> {
        // Refuse parameters below a floor (a tampered header can't downgrade us; it's AAD, but
        // also refuse absurd values that would OOM the phone).
        if self.m_kib < 19 * 1024
            || self.m_kib > 1024 * 1024
            || self.t < 2
            || self.t > 20
            || self.p == 0
            || self.p > 8
        {
            return Err(VaultError::Format);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VaultError {
    #[error("wrong password or corrupted store")]
    Decrypt,
    #[error("unrecognised store format")]
    Format,
    #[error("password must be at least 8 characters")]
    WeakPassword,
    #[error("internal: {0}")]
    Internal(String),
}

/// What the vault protects.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct VaultData {
    /// Phone's iroh secret key (ed25519 seed).
    pub secret_key: [u8; 32],
    pub pcs: Vec<PairedPc>,
    /// Login screens (`dishus boot allow`), each with its own phone identity. Added after the
    /// first release: older stores lack it (see [`open`]).
    pub login: Vec<LoginScreen>,
}

/// The layout before login screens existed.
#[derive(Deserialize)]
struct VaultDataV1 {
    secret_key: [u8; 32],
    pcs: Vec<PairedPc>,
}

/// A PC's login-screen mode. It has its own id, and this phone talks to it with an identity
/// used for nothing else, so the PC can allow and revoke it separately.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoginScreen {
    pub secret_key: [u8; 32],
    /// The login-mode daemon's EndpointId.
    pub endpoint_id: [u8; 32],
    /// The PC's name.
    pub name: String,
    pub relays: Vec<String>,
    pub addrs: Vec<String>,
}

impl std::fmt::Debug for LoginScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginScreen")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Drop for LoginScreen {
    fn drop(&mut self) {
        self.secret_key.zeroize();
    }
}

impl std::fmt::Debug for VaultData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultData")
            .field("pcs", &self.pcs)
            .field("login", &self.login)
            .finish_non_exhaustive()
    }
}

impl Drop for VaultData {
    fn drop(&mut self) {
        self.secret_key.zeroize();
    }
}

/// A PC this phone is paired with.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedPc {
    pub endpoint_id: [u8; 32],
    pub name: String,
    pub relays: Vec<String>,
    pub addrs: Vec<String>,
    pub expires_at: u64,
}

fn derive(password: &[u8], salt: &[u8], p: KdfParams) -> Result<Zeroizing<[u8; 32]>, VaultError> {
    p.check()?;
    let params = Params::new(p.m_kib, p.t, p.p, Some(32))
        .map_err(|e| VaultError::Internal(e.to_string()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(password, salt, key.as_mut())
        .map_err(|e| VaultError::Internal(e.to_string()))?;
    Ok(key)
}

pub fn seal(password: &str, data: &VaultData, kdf: KdfParams) -> Result<Vec<u8>, VaultError> {
    let plain =
        Zeroizing::new(postcard::to_stdvec(data).map_err(|e| VaultError::Internal(e.to_string()))?);
    seal_bytes(password, &plain, kdf)
}

fn seal_bytes(password: &str, plain: &[u8], kdf: KdfParams) -> Result<Vec<u8>, VaultError> {
    if password.chars().count() < 8 {
        return Err(VaultError::WeakPassword);
    }
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut salt).map_err(|e| VaultError::Internal(e.to_string()))?;
    getrandom::fill(&mut nonce).map_err(|e| VaultError::Internal(e.to_string()))?;
    let key = derive(password.as_bytes(), &salt, kdf)?;

    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&kdf.m_kib.to_be_bytes());
    header.extend_from_slice(&kdf.t.to_be_bytes());
    header.extend_from_slice(&kdf.p.to_be_bytes());
    header.extend_from_slice(&salt);
    header.extend_from_slice(&nonce);

    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref())
        .map_err(|_| VaultError::Internal("key".into()))?;
    let n = XNonce::try_from(&nonce[..]).map_err(|_| VaultError::Internal("nonce".into()))?;
    let ct = cipher
        .encrypt(
            &n,
            Payload {
                msg: plain,
                aad: &header,
            },
        )
        .map_err(|_| VaultError::Internal("encrypt".into()))?;
    header.extend_from_slice(&ct);
    Ok(header)
}

pub fn open(password: &str, blob: &[u8]) -> Result<VaultData, VaultError> {
    if blob.len() < HEADER_LEN + 16 || blob.len() > MAX_BLOB || &blob[..4] != MAGIC {
        return Err(VaultError::Format);
    }
    let be = |i: usize| u32::from_be_bytes(blob[i..i + 4].try_into().expect("4 bytes"));
    let kdf = KdfParams {
        m_kib: be(4),
        t: be(8),
        p: be(12),
    };
    let salt = &blob[16..16 + SALT_LEN];
    let nonce = &blob[16 + SALT_LEN..HEADER_LEN];
    let key = derive(password.as_bytes(), salt, kdf)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).map_err(|_| VaultError::Format)?;
    let n = XNonce::try_from(nonce).map_err(|_| VaultError::Format)?;
    let plain = Zeroizing::new(
        cipher
            .decrypt(
                &n,
                Payload {
                    msg: &blob[HEADER_LEN..],
                    aad: &blob[..HEADER_LEN],
                },
            )
            .map_err(|_| VaultError::Decrypt)?,
    );
    postcard::from_bytes::<VaultData>(&plain).or_else(|_| {
        let old: VaultDataV1 = postcard::from_bytes(&plain).map_err(|_| VaultError::Format)?;
        Ok(VaultData {
            secret_key: old.secret_key,
            pcs: old.pcs,
            login: Vec::new(),
        })
    })
}

/// Generate a fresh identity.
pub fn new_identity() -> Result<VaultData, VaultError> {
    let mut d = VaultData::default();
    getrandom::fill(&mut d.secret_key).map_err(|e| VaultError::Internal(e.to_string()))?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Minimum allowed params keep tests fast.
    const FAST: KdfParams = KdfParams {
        m_kib: 19 * 1024,
        t: 2,
        p: 1,
    };

    #[test]
    fn a_store_from_before_login_screens_still_opens() {
        #[derive(Serialize)]
        struct V1<'a> {
            secret_key: [u8; 32],
            pcs: &'a [PairedPc],
        }
        let d = sample();
        let plain = postcard::to_stdvec(&V1 {
            secret_key: d.secret_key,
            pcs: &d.pcs,
        })
        .unwrap();
        let blob = seal_bytes("password123", &plain, FAST).unwrap();
        let back = open("password123", &blob).unwrap();
        assert_eq!(back.secret_key, d.secret_key);
        assert_eq!(back.pcs, d.pcs);
        assert!(back.login.is_empty());
        // and with a login screen it round-trips
        let mut d = sample();
        d.login.push(LoginScreen {
            secret_key: [4; 32],
            endpoint_id: [5; 32],
            name: "desk".into(),
            relays: vec![],
            addrs: vec!["10.0.0.2:1".into()],
        });
        let blob = seal("password123", &d, FAST).unwrap();
        assert_eq!(open("password123", &blob).unwrap(), d);
    }

    fn sample() -> VaultData {
        let mut d = new_identity().unwrap();
        d.pcs.push(PairedPc {
            endpoint_id: [1; 32],
            name: "desk".into(),
            relays: vec![],
            addrs: vec![],
            expires_at: 9,
        });
        d
    }

    #[test]
    fn roundtrip() {
        let d = sample();
        let blob = seal("correct horse", &d, FAST).unwrap();
        assert_eq!(open("correct horse", &blob).unwrap(), d);
    }

    #[test]
    fn wrong_password() {
        let blob = seal("correct horse", &sample(), FAST).unwrap();
        assert_eq!(open("wrong horse!", &blob), Err(VaultError::Decrypt));
    }

    #[test]
    fn tamper_detected() {
        let mut blob = seal("correct horse", &sample(), FAST).unwrap();
        let n = blob.len();
        blob[n - 1] ^= 1;
        assert_eq!(open("correct horse", &blob), Err(VaultError::Decrypt));
        let mut blob = seal("correct horse", &sample(), FAST).unwrap();
        blob[20] ^= 1; // salt (AAD)
        assert_eq!(open("correct horse", &blob), Err(VaultError::Decrypt));
    }

    #[test]
    fn kdf_downgrade_refused() {
        let mut blob = seal("correct horse", &sample(), FAST).unwrap();
        blob[4..8].copy_from_slice(&8u32.to_be_bytes()); // 8 KiB
        assert_eq!(open("correct horse", &blob), Err(VaultError::Format));
    }

    #[test]
    fn weak_password() {
        assert_eq!(
            seal("short", &sample(), FAST),
            Err(VaultError::WeakPassword)
        );
    }
}
