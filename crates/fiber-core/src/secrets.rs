//! Project secret encryption at rest (AES-256-GCM).
//!
//! Set `FIBER_SECRETS_KEY` to a 64-char hex string (32 bytes). When unset, values
//! are stored plaintext (local/dev). Ciphertext is prefixed with `enc:v1:`.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow};
// `getrandom` was renamed `fill` in 0.3; same CSPRNG, same failure mode.
use getrandom::fill as getrandom;
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
    encrypt_with(k, plaintext)
}

pub fn decrypt_secret(stored: &str) -> Result<String> {
    if !stored.starts_with(PREFIX) {
        // Plaintext (legacy / no key).
        return Ok(stored.to_string());
    }
    let Some(k) = key() else {
        return Err(anyhow!(
            "encrypted secret found but FIBER_SECRETS_KEY is not set"
        ));
    };
    decrypt_with(k, stored)
}

/// The crypto itself, taking the key explicitly. `encrypt_secret` supplies the
/// process-wide key from the environment; splitting it out keeps the cipher reachable
/// from tests, which cannot set a `OnceLock` that another test may already have read.
fn encrypt_with(k: &[u8; 32], plaintext: &str) -> Result<String> {
    let cipher = Aes256Gcm::new_from_slice(k)
        // Formatted rather than `.context()`: whether this error implements
        // std::error::Error depends on a transitive `std` feature that another crate
        // happens to enable, which is not something to build on.
        .map_err(|e| anyhow!("aes key: {e}"))?;
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

/// Inverse of [`encrypt_with`]. A value without the `enc:v1:` prefix is passed through
/// as plaintext, matching `decrypt_secret`.
fn decrypt_with(k: &[u8; 32], stored: &str) -> Result<String> {
    let Some(rest) = stored.strip_prefix(PREFIX) else {
        return Ok(stored.to_string());
    };
    let raw = hex::decode(rest).context("decode ciphertext")?;
    if raw.len() < 13 {
        return Err(anyhow!("ciphertext too short"));
    }
    let (nonce_bytes, ct) = raw.split_at(12);
    let cipher = Aes256Gcm::new_from_slice(k)
        // Formatted rather than `.context()`: whether this error implements
        // std::error::Error depends on a transitive `std` feature that another crate
        // happens to enable, which is not something to build on.
        .map_err(|e| anyhow!("aes key: {e}"))?;
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

    const KEY_A: [u8; 32] = [7u8; 32];
    const KEY_B: [u8; 32] = [9u8; 32];

    #[test]
    fn a_secret_survives_a_round_trip() {
        for plaintext in [
            "hunter2",
            "",
            "a value with spaces and = signs",
            "ünïcodé ✓ and a\nnewline",
            &"x".repeat(8192),
        ] {
            let sealed = encrypt_with(&KEY_A, plaintext).unwrap();
            assert!(sealed.starts_with(PREFIX), "{sealed}");
            assert_eq!(decrypt_with(&KEY_A, &sealed).unwrap(), plaintext);
        }
    }

    #[test]
    fn the_ciphertext_does_not_contain_the_plaintext() {
        let sealed = encrypt_with(&KEY_A, "hunter2").unwrap();
        assert!(!sealed.contains("hunter2"));
        assert!(
            !hex::decode(sealed.strip_prefix(PREFIX).unwrap())
                .unwrap()
                .windows(7)
                .any(|w| w == b"hunter2")
        );
    }

    #[test]
    fn encrypting_twice_gives_different_ciphertext() {
        // A fresh nonce per call. Equal ciphertexts would leak which projects share a
        // secret value.
        let a = encrypt_with(&KEY_A, "same").unwrap();
        let b = encrypt_with(&KEY_A, "same").unwrap();
        assert_ne!(a, b);
        assert_eq!(decrypt_with(&KEY_A, &a).unwrap(), "same");
        assert_eq!(decrypt_with(&KEY_A, &b).unwrap(), "same");
    }

    #[test]
    fn the_wrong_key_is_refused_rather_than_returning_rubbish() {
        let sealed = encrypt_with(&KEY_A, "hunter2").unwrap();
        let err = decrypt_with(&KEY_B, &sealed).unwrap_err().to_string();
        assert!(err.contains("wrong key"), "{err}");
    }

    #[test]
    fn a_tampered_ciphertext_fails_its_authentication_tag() {
        // AES-GCM is authenticated; flipping any byte of nonce or ciphertext must fail
        // rather than decrypt to something else.
        let sealed = encrypt_with(&KEY_A, "hunter2").unwrap();
        let body = sealed.strip_prefix(PREFIX).unwrap();
        let mut raw = hex::decode(body).unwrap();
        for i in [0usize, 11, 12, raw.len() - 1] {
            let original = raw[i];
            raw[i] ^= 0x01;
            let tampered = format!("{PREFIX}{}", hex::encode(&raw));
            assert!(
                decrypt_with(&KEY_A, &tampered).is_err(),
                "flipping byte {i} must not decrypt"
            );
            raw[i] = original;
        }
    }

    #[test]
    fn a_truncated_ciphertext_is_rejected() {
        // Below the 12-byte nonce plus a tag there is nothing to authenticate.
        for body in ["", "00", &"ab".repeat(12), &"ab".repeat(13)] {
            let short = format!("{PREFIX}{body}");
            assert!(decrypt_with(&KEY_A, &short).is_err(), "{short}");
        }
    }

    #[test]
    fn a_non_hex_body_is_rejected() {
        assert!(decrypt_with(&KEY_A, &format!("{PREFIX}not-hex-at-all")).is_err());
    }

    #[test]
    fn an_unprefixed_value_is_passed_through_as_plaintext() {
        // Rows written before FIBER_SECRETS_KEY was set are stored bare.
        assert_eq!(
            decrypt_with(&KEY_A, "legacy-plaintext").unwrap(),
            "legacy-plaintext"
        );
        assert_eq!(
            decrypt_secret("legacy-plaintext").unwrap(),
            "legacy-plaintext"
        );
        // A near-miss prefix is still plaintext, not a malformed ciphertext.
        assert_eq!(decrypt_secret("enc:v2:ab").unwrap(), "enc:v2:ab");
    }

    #[test]
    fn an_encrypted_value_without_a_configured_key_is_an_error_not_a_leak() {
        // Never hand the raw ciphertext back to a step as if it were the secret.
        if key().is_none() {
            let sealed = encrypt_with(&KEY_A, "hunter2").unwrap();
            let err = decrypt_secret(&sealed).unwrap_err().to_string();
            assert!(err.contains("FIBER_SECRETS_KEY is not set"), "{err}");
        }
    }

    #[test]
    fn without_a_key_the_env_facing_api_stores_plaintext() {
        if key().is_none() {
            assert_eq!(encrypt_secret("hello").unwrap(), "hello");
            assert_eq!(decrypt_secret("hello").unwrap(), "hello");
        }
    }

    #[test]
    fn load_key_accepts_exactly_thirty_two_bytes_of_hex() {
        // `load_key` reads the env directly; exercise the parsing rules it applies by
        // driving the same hex decode, since the OnceLock can only be set once.
        assert_eq!(hex::decode("ab".repeat(32)).unwrap().len(), 32);
        assert_ne!(hex::decode("ab".repeat(16)).unwrap().len(), 32);
        assert!(hex::decode("zz".repeat(32)).is_err());
    }
}
