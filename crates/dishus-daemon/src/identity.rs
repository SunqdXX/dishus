//! The PC's long-term iroh identity (ed25519). Its public half is the EndpointId phones dial.

use anyhow::{Result, bail};
use iroh::SecretKey;
use zeroize::Zeroizing;

use crate::paths::{Paths, read_private, write_private};

pub fn load_or_create(paths: &Paths) -> Result<SecretKey> {
    let file = paths.secret_key_file();
    if let Some(raw) = read_private(&file)? {
        let raw = Zeroizing::new(raw);
        let Ok(bytes) = <&[u8; 32]>::try_from(raw.as_slice()) else {
            bail!("{} is corrupt (expected 32 bytes)", file.display());
        };
        return Ok(SecretKey::from_bytes(bytes));
    }
    let mut bytes = Zeroizing::new([0u8; 32]);
    getrandom::fill(bytes.as_mut()).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let key = SecretKey::from_bytes(&bytes);
    write_private(&file, bytes.as_ref())?;
    tracing::info!("generated new PC identity {}", key.public());
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable() {
        let t = tempfile::tempdir().unwrap();
        let p = Paths::under(t.path());
        let a = load_or_create(&p).unwrap();
        let b = load_or_create(&p).unwrap();
        assert_eq!(a.public(), b.public());
    }
}
