use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};
// rand 0.10 renamed the core trait: upstream `rand_core::RngCore` is now `rand_core::Rng`,
// and rand's old extension trait `Rng` became `RngExt`. `fill_bytes` lives on this one.
// Note this is *not* the `rand_core` that `argon2::password_hash` re-exports below — that
// is rand_core 0.6, a separate major resolved separately, with its own `OsRng`.
use rand::Rng;
use sha2::{Digest, Sha256};

pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("fiber_agent_{}", hex::encode(bytes))
}

pub fn generate_session_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("fiber_sess_{}", hex::encode(bytes))
}

pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// PHC-encoded argon2id hash.
pub fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 hash")
        .to_string()
}

/// Verify argon2id (preferred) or legacy `salt$sha256` hashes.
pub fn verify_password(password: &str, stored: &str) -> bool {
    if stored.starts_with("$argon2") {
        let Ok(parsed) = PasswordHash::new(stored) else {
            return false;
        };
        return Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
    }
    // Legacy: `{salt_hex}${sha256(salt||password)}`
    let Some((salt_hex, expected)) = stored.split_once('$') else {
        return false;
    };
    let mut hasher = Sha256::new();
    hasher.update(salt_hex.as_bytes());
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize()) == expected
}

pub fn password_needs_rehash(stored: &str) -> bool {
    !stored.starts_with("$argon2")
}

pub fn slugify(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = s.trim_matches('-');
    if trimmed.is_empty() {
        "project".into()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The token bytes must come from a cryptographically secure generator.
    ///
    /// rand 0.10 made `SmallRng` (Xoshiro256++, *not* a CSPRNG) available unconditionally
    /// by removing the `small_rng` feature gate, so "it compiles and returns 32 bytes" no
    /// longer implies the entropy is fit to mint a credential. This bound fails to compile
    /// if the generator behind `rand::rng()` is ever swapped for one that is not.
    #[test]
    fn token_entropy_comes_from_a_csprng() {
        fn assert_crypto<R: rand::CryptoRng>(_: &R) {}
        assert_crypto(&rand::rng());
    }

    #[test]
    fn tokens_are_32_bytes_of_hex_behind_their_prefix() {
        for (token, prefix) in [
            (generate_token(), "fiber_agent_"),
            (generate_session_token(), "fiber_sess_"),
        ] {
            let hex_part = token.strip_prefix(prefix).expect("prefix");
            assert_eq!(hex_part.len(), 64, "32 bytes, hex-encoded");
            assert_eq!(hex::decode(hex_part).expect("hex").len(), 32);
            // Two calls must not collide; a constant generator would fail here.
            assert_ne!(token, generate_token());
        }
    }

    #[test]
    fn argon2_roundtrip() {
        let h = hash_password("fiber");
        assert!(h.starts_with("$argon2"));
        assert!(verify_password("fiber", &h));
        assert!(!verify_password("wrong", &h));
    }

    #[test]
    fn legacy_sha_still_verifies() {
        let mut salt = [0u8; 16];
        salt.fill(1);
        let salt_hex = hex::encode(salt);
        let mut hasher = Sha256::new();
        hasher.update(salt_hex.as_bytes());
        hasher.update(b"fiber");
        let stored = format!("{salt_hex}${}", hex::encode(hasher.finalize()));
        assert!(verify_password("fiber", &stored));
        assert!(password_needs_rehash(&stored));
    }
}
