//! What control keeps secret in Postgres (ChatGPT links' refresh tokens, users' API keys, their
//! sessions' secrets) is sealed with its link key: XChaCha20-Poly1305, a random nonce of its own
//! each time, before the ciphertext.

use std::path::Path;

use anyhow::Context;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;

#[derive(Clone)]
pub struct Seal {
    cipher: XChaCha20Poly1305,
}

impl Seal {
    /// With the link key in `path` (32 bytes, base64).
    pub fn load(path: &Path) -> anyhow::Result<Seal> {
        Ok(Seal {
            cipher: XChaCha20Poly1305::new(&load_key(path)?.into()),
        })
    }

    pub fn seal(&self, plain: &[u8]) -> Vec<u8> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let mut sealed = nonce.to_vec();
        sealed.extend(
            self.cipher
                .encrypt(&nonce, plain)
                .expect("encrypting in memory"),
        );
        sealed
    }

    pub fn open(&self, sealed: &[u8]) -> anyhow::Result<String> {
        anyhow::ensure!(sealed.len() > 24, "a sealed value too short");
        let (nonce, text) = sealed.split_at(24);
        let plain = self
            .cipher
            .decrypt(XNonce::from_slice(nonce), text)
            .map_err(|_| anyhow::anyhow!("a sealed value isn't the link key's"))?;
        Ok(String::from_utf8(plain)?)
    }
}

pub fn load_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the link key {}", path.display()))?;
    let bytes = STANDARD
        .decode(text.trim())
        .with_context(|| format!("{}: not base64", path.display()))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("{}: not 32 bytes", path.display()))
}

/// A new link key in `path` (32 random bytes, base64), unless there's one.
pub fn keygen(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    crate::token::write_private(path, STANDARD.encode(key).as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seal(dir: &Path) -> Seal {
        let key = dir.join("link.key");
        keygen(&key).unwrap();
        Seal::load(&key).unwrap()
    }

    #[test]
    fn a_sealed_value_opens_only_with_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let s = seal(dir.path());
        let sealed = s.seal(b"rt-secret");
        assert!(!sealed.windows(9).any(|w| w == b"rt-secret"));
        assert_ne!(sealed, s.seal(b"rt-secret"), "a nonce of its own each time");
        assert_eq!(s.open(&sealed).unwrap(), "rt-secret");
        let other = tempfile::tempdir().unwrap();
        assert!(seal(other.path()).open(&sealed).is_err());
        assert!(s.open(&sealed[..10]).is_err());
    }

    #[test]
    fn keygen_keeps_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("link.key");
        keygen(&key).unwrap();
        let first = std::fs::read(&key).unwrap();
        keygen(&key).unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), first);
        assert_eq!(load_key(&key).unwrap().len(), 32);
    }
}
