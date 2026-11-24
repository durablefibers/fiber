//! Project secret encryption at rest (AES-256-GCM).
//!
//! Set `FIBER_SECRETS_KEY` to a 64-char hex string (32 bytes). When unset, values
//! are stored plaintext (local/dev). Ciphertext is prefixed with `enc:v1:`.
//!
//! Unset and unusable are different things. A key that is set but is not 64 hex
//! characters is an error — at boot, and on every encrypt or decrypt — never a quiet
//! fall back to plaintext: an operator who set a key believes their secrets are encrypted.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, anyhow};
// `getrandom` was renamed `fill` in 0.3; same CSPRNG, same failure mode.
use getrandom::fill as getrandom;
use std::sync::OnceLock;
use zeroize::Zeroizing;

const PREFIX: &str = "enc:v1:";

static KEY: OnceLock<Result<Option<[u8; 32]>, KeyError>> = OnceLock::new();

/// Why a `FIBER_SECRETS_KEY` that is set cannot be used. Carries a length, never the
/// value: this ends up in a boot log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error(
        "FIBER_SECRETS_KEY is set but is not hexadecimal ({chars} characters; it must be exactly 64 hex characters — generate one with `openssl rand -hex 32`, and check for quotes or stray characters)"
    )]
    NotHex { chars: usize },
    #[error(
        "FIBER_SECRETS_KEY is set but is {chars} hex characters long; it must be exactly 64 (32 bytes) — generate one with `openssl rand -hex 32`"
    )]
    WrongLength { chars: usize },
}

/// The key in `raw`, the value of `FIBER_SECRETS_KEY`.
///
/// `Ok(None)` only when there is no key at all — unset, empty, or whitespace (Compose
/// passes an empty string for an unset variable). Anything else that is not 64 hex
/// characters is an error. It used to be `None` as well, which logged the same "unset
/// (dev only)" line as a missing key and stored every secret written afterwards in
/// plaintext.
pub fn parse_key(raw: Option<&str>) -> Result<Option<[u8; 32]>, KeyError> {
    let raw = raw.unwrap_or_default().trim();
    if raw.is_empty() {
        return Ok(None);
    }
    if !raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(KeyError::NotHex {
            chars: raw.chars().count(),
        });
    }
    let mut key = [0u8; 32];
    hex::decode_to_slice(raw, &mut key).map_err(|_| KeyError::WrongLength { chars: raw.len() })?;
    Ok(Some(key))
}

fn key_from_env() -> Result<Option<[u8; 32]>, KeyError> {
    match std::env::var("FIBER_SECRETS_KEY") {
        Ok(raw) => parse_key(Some(&raw)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(raw)) => Err(KeyError::NotHex { chars: raw.len() }),
    }
}

fn key() -> Result<Option<&'static [u8; 32]>, KeyError> {
    match KEY.get_or_init(key_from_env) {
        Ok(key) => Ok(key.as_ref()),
        Err(e) => Err(e.clone()),
    }
}

/// Call once at process start: an unusable key stops the boot, and a missing one is
/// warned about where it will be seen.
pub fn init_from_env() -> Result<(), KeyError> {
    match key()? {
        Some(_) => tracing::info!("project secrets encryption enabled (FIBER_SECRETS_KEY)"),
        None => tracing::warn!(
            "FIBER_SECRETS_KEY unset — project secrets stored in plaintext (dev only)"
        ),
    }
    Ok(())
}

pub fn encrypt_secret(plaintext: &str) -> Result<String> {
    let Some(k) = key()? else {
        return Ok(plaintext.to_string());
    };
    encrypt_with(k, plaintext)
}

pub fn decrypt_secret(stored: &str) -> Result<String> {
    if !stored.starts_with(PREFIX) {
        // Plaintext (legacy / no key).
        return Ok(stored.to_string());
    }
    let Some(k) = key()? else {
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
    let nonce = Nonce::from(nonce_bytes);
    let ct = cipher
        .encrypt(&nonce, plaintext.as_bytes())
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
    let nonce = Nonce::try_from(nonce_bytes).map_err(|_| anyhow!("nonce length"))?;
    let pt = cipher
        .decrypt(&nonce, ct)
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
    fn no_key_is_allowed_and_means_plaintext() {
        for raw in [None, Some(""), Some("   "), Some("\n")] {
            assert_eq!(parse_key(raw), Ok(None), "{raw:?}");
        }
    }

    #[test]
    fn a_64_hex_key_is_accepted_in_either_case_and_trimmed() {
        let lower = "07".repeat(32);
        assert_eq!(parse_key(Some(&lower)), Ok(Some(KEY_A)));
        assert_eq!(parse_key(Some(&format!("  {lower}\n"))), Ok(Some(KEY_A)));
        assert_eq!(parse_key(Some(&"AB".repeat(32))), Ok(Some([0xab; 32])));
    }

    /// The fail-open this replaces: each of these was treated as "unset", and secrets
    /// were then written in plaintext under a key the operator believed was in force.
    #[test]
    fn a_key_that_is_set_but_unusable_is_an_error_not_no_key() {
        let hex64 = "07".repeat(32);
        for (raw, want) in [
            (hex64[..63].to_string(), KeyError::WrongLength { chars: 63 }),
            (hex64[..62].to_string(), KeyError::WrongLength { chars: 62 }),
            (format!("{hex64}07"), KeyError::WrongLength { chars: 66 }),
            ("abcd".to_string(), KeyError::WrongLength { chars: 4 }),
            (format!("\"{hex64}\""), KeyError::NotHex { chars: 66 }),
            (
                "correct horse battery staple".to_string(),
                KeyError::NotHex { chars: 28 },
            ),
            (
                format!("{}zz", &hex64[..62]),
                KeyError::NotHex { chars: 64 },
            ),
        ] {
            assert_eq!(parse_key(Some(&raw)), Err(want), "{raw}");
        }
    }

    #[test]
    fn a_key_error_names_the_problem_and_never_the_value() {
        let secretish = "s3cr3t-value-that-is-not-hex";
        for raw in [secretish.to_string(), "deadbeefcafe".to_string()] {
            let msg = parse_key(Some(&raw)).unwrap_err().to_string();
            assert!(msg.contains("FIBER_SECRETS_KEY"), "{msg}");
            assert!(msg.contains("openssl rand -hex 32"), "{msg}");
            assert!(!msg.contains(&raw), "{msg}");
        }
    }

    /// Written by aes-gcm 0.10 with `KEY_A`. Secrets already in the database were
    /// encrypted by whatever version was current when they were written, so a crate
    /// upgrade that changed the nonce layout or the tag position has to fail here
    /// rather than in production, where it would read as every secret being corrupt.
    #[test]
    fn a_ciphertext_from_an_older_aes_gcm_still_decrypts() {
        let stored =
            "enc:v1:571a3cf8ac5dc884ae49b4be41355364d05b34a45e66dd7c92b2a88d76eb4e57b02236";
        assert_eq!(decrypt_with(&KEY_A, stored).unwrap(), "hunter2");
    }

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
        if matches!(key(), Ok(None)) {
            let sealed = encrypt_with(&KEY_A, "hunter2").unwrap();
            let err = decrypt_secret(&sealed).unwrap_err().to_string();
            assert!(err.contains("FIBER_SECRETS_KEY is not set"), "{err}");
        }
    }

    #[test]
    fn without_a_key_the_env_facing_api_stores_plaintext() {
        if matches!(key(), Ok(None)) {
            assert_eq!(encrypt_secret("hello").unwrap(), "hello");
            assert_eq!(decrypt_secret("hello").unwrap(), "hello");
        }
    }
}
