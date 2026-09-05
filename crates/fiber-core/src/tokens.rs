use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::RngCore;
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
