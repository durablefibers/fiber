//! Project secret encryption at rest (AES-256-GCM).
//!
//! Set `FIBER_SECRETS_KEY` to a 64-char hex string (32 bytes). When unset, values
//! are stored plaintext (local/dev). Ciphertext is prefixed with `enc:v1:`.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow};
use getrandom::getrandom;
use std::sync::OnceLock;
use zeroize::Zeroizing;

const PREFIX: &str = "enc:v1:";

static KEY: OnceLock<Option<[u8; 32]>> = OnceLock::new();

fn load_key() -> Option<[u8; 32]> {
    let raw = std::env::var("FIBER_SECRETS_KEY").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let bytes = hex::decode(raw).ok()?;
    if bytes.len() != 32 {
        tracing::warn!(
            len = bytes.len(),
            "FIBER_SECRETS_KEY must be 32 bytes (64 hex chars); ignoring"
        );
        return None;
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Some(key)
}

fn key() -> Option<&'static [u8; 32]> {
    KEY.get_or_init(load_key).as_ref()
}

/// Call once at process start so missing-key warning is visible.
pub fn init_from_env() {
    match key() {
        Some(_) => tracing::info!("project secrets encryption enabled (FIBER_SECRETS_KEY)"),
        None => tracing::warn!(
            "FIBER_SECRETS_KEY unset — project secrets stored in plaintext (dev only)"
        ),
    }
}

pub fn encrypt_secret(plaintext: &str) -> Result<String> {
    let Some(k) = key() else {
        return Ok(plaintext.to_string());
    };
    let cipher = Aes256Gcm::new_from_slice(k).context("aes key")?;
    let mut nonce_bytes = [0u8; 12];
    getrandom(&mut nonce_bytes).context("nonce rng")?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| anyhow!("encrypt secret: {e}"))?;
    let mut out = Vec::with_capacity(12 + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(format!("{PREFIX}{}", hex::encode(out)))
}

pub fn decrypt_secret(stored: &str) -> Result<String> {
    let Some(rest) = stored.strip_prefix(PREFIX) else {
        // Plaintext (legacy / no key).
        return Ok(stored.to_string());
    };
    let Some(k) = key() else {
        return Err(anyhow!(
            "encrypted secret found but FIBER_SECRETS_KEY is not set"
        ));
    };
    let raw = hex::decode(rest).context("decode ciphertext")?;
    if raw.len() < 13 {
        return Err(anyhow!("ciphertext too short"));
    }
    let (nonce_bytes, ct) = raw.split_at(12);
    let cipher = Aes256Gcm::new_from_slice(k).context("aes key")?;
    let nonce = Nonce::from_slice(nonce_bytes);
    let pt = cipher
        .decrypt(nonce, ct)
        .map_err(|_| anyhow!("decrypt secret failed (wrong key?)"))?;
    let s = Zeroizing::new(String::from_utf8(pt).context("secret utf8")?);
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_passthrough_without_key() {
        // KEY may already be init from other tests; only assert roundtrip API.
        let s = "hello";
        // encrypt without key returns plaintext
        if key().is_none() {
            assert_eq!(encrypt_secret(s).unwrap(), s);
            assert_eq!(decrypt_secret(s).unwrap(), s);
        }
    }
}
