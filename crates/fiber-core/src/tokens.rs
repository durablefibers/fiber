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

/// A password for `fiber-api reset-password` to print when none is given: 128 bits from
/// the same CSPRNG as the tokens, hex so it survives any terminal and any paste.
pub fn generate_password() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
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
    // Legacy: `{salt_hex}${sha256(salt||password)}`. One SHA-256 is microseconds where
    // every other outcome is an argon2id verify, so an account still on a legacy hash
    // answered measurably faster than a current one or an unknown name. Pay for the
    // verify here too, and compare without stopping at the first differing byte.
    burn_one_verify(password);
    let Some((salt_hex, expected)) = stored.split_once('$') else {
        return false;
    };
    let mut hasher = Sha256::new();
    hasher.update(salt_hex.as_bytes());
    hasher.update(password.as_bytes());
    constant_time_eq(
        hex::encode(hasher.finalize()).as_bytes(),
        expected.as_bytes(),
    )
}

/// One argon2id verify whose answer is thrown away, so a path that has no argon2 hash to
/// check costs what one that has does.
fn burn_one_verify(password: &str) {
    #[cfg(test)]
    BURNED_VERIFIES.with(|n| n.set(n.get() + 1));
    if let Ok(parsed) = PasswordHash::new(dummy_password_hash()) {
        let _ = Argon2::default().verify_password(password.as_bytes(), &parsed);
    }
}

#[cfg(test)]
thread_local! {
    /// How many times this thread has paid for a throwaway verify.
    static BURNED_VERIFIES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Byte equality that takes the same time wherever the first difference is. Length is
/// not hidden: the values compared here (hex digests, HMAC signatures) have public lengths.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// argon2id runs at once, process-wide, across every caller of the `*_blocking` helpers.
///
/// Each is tens of milliseconds of CPU and 19 MiB by design. `spawn_blocking` alone
/// bounds nothing: tokio's blocking pool grows to 512 threads, so a flood of logins —
/// which need no credentials — was 512 × 19 MiB and every core. Four keeps the worst
/// case under 80 MiB and leaves cores for the scheduler.
pub const MAX_CONCURRENT_HASHES: usize = 4;
/// How long a caller waits for one of those slots before giving up with [`HashBusy`].
/// Slots are handed out in arrival order, so a real login behind a flood is served in
/// turn rather than refused; this bounds how long "in turn" may be.
pub const HASH_SLOT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// The most callers that may be waiting for a slot. A backstop on memory, set so that
/// it refuses nobody who could have been served: with `MAX_CONCURRENT_HASHES` slots and
/// a verify that takes 25 ms or more, at most 800 waiters can reach a slot inside
/// `HASH_SLOT_WAIT`. Anyone further back would have timed out anyway.
pub const MAX_WAITING_HASHES: usize = 1024;

/// No password-hashing slot came free in time. The server is busy, not the caller wrong:
/// handlers answer `503`, never the `401`/`429` that a bad password or a lockout get.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("password hashing is saturated; no slot came free in time")]
pub struct HashBusy;

/// Bounds how many blocking jobs run at once, and how many callers wait for a turn.
struct HashGate {
    slots: std::sync::Arc<tokio::sync::Semaphore>,
    waiting: std::sync::atomic::AtomicUsize,
    max_waiting: usize,
    wait: std::time::Duration,
}

impl HashGate {
    fn new(slots: usize, max_waiting: usize, wait: std::time::Duration) -> Self {
        Self {
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(slots)),
            waiting: std::sync::atomic::AtomicUsize::new(0),
            max_waiting,
            wait,
        }
    }

    /// Run `f` on the blocking pool once a slot is free.
    ///
    /// The permit moves **into** the blocking task and is released when `f` returns —
    /// not when this future is dropped. A request that is cancelled (client gone, the
    /// 30 s request timeout) stops waiting for the answer, but the work it started is
    /// still running, and a permit released at cancellation would let the next request
    /// start more on top of it.
    async fn run<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        use std::sync::atomic::Ordering;
        /// Leaves the queue however the wait ends, cancellation included.
        struct Waiting<'a>(&'a std::sync::atomic::AtomicUsize);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        if self.waiting.fetch_add(1, Ordering::SeqCst) >= self.max_waiting {
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            return Err(HashBusy.into());
        }
        let waiting = Waiting(&self.waiting);
        let permit = tokio::time::timeout(self.wait, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| HashBusy)?
            .map_err(|_| HashBusy)?;
        drop(waiting);
        Ok(tokio::task::spawn_blocking(move || {
            let _permit = permit;
            f()
        })
        .await?)
    }
}

fn hash_gate() -> &'static HashGate {
    static GATE: std::sync::OnceLock<HashGate> = std::sync::OnceLock::new();
    GATE.get_or_init(|| HashGate::new(MAX_CONCURRENT_HASHES, MAX_WAITING_HASHES, HASH_SLOT_WAIT))
}

/// [`hash_password`] off the async workers, and inside [`MAX_CONCURRENT_HASHES`].
///
/// argon2id is tens of milliseconds of CPU and 19 MiB by design. Run inline in a handler
/// it parks a tokio worker for that long, so a burst of logins stalls everything else the
/// runtime is driving — agent heartbeats and lease renewals included.
///
/// Fails with [`HashBusy`] (downcast it) when no slot came free within [`HASH_SLOT_WAIT`].
pub async fn hash_password_blocking(password: &str) -> anyhow::Result<String> {
    let password = password.to_owned();
    hash_gate().run(move || hash_password(&password)).await
}

/// [`verify_password`] off the async workers and inside [`MAX_CONCURRENT_HASHES`], at
/// the same cost whether or not there is a stored hash to verify against.
///
/// `stored` is `None` when no such user exists. Answering that case without doing the
/// work made "unknown username" return in microseconds and "wrong password" in tens of
/// milliseconds — an oracle for which usernames exist. The password is checked against
/// [`dummy_password_hash`] instead and the answer is always `false`.
///
/// Fails with [`HashBusy`] (downcast it) when no slot came free within [`HASH_SLOT_WAIT`].
pub async fn verify_password_blocking(
    password: &str,
    stored: Option<&str>,
) -> anyhow::Result<bool> {
    let password = password.to_owned();
    let stored = stored.map(str::to_owned);
    hash_gate()
        .run(move || match stored {
            Some(stored) => verify_password(&password, &stored),
            None => {
                burn_one_verify(&password);
                false
            }
        })
        .await
}

/// An argon2id hash of a random password nobody knows, made once per process with the
/// same parameters as [`hash_password`], so verifying against it costs what verifying a
/// real one does.
pub fn dummy_password_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| hash_password(&generate_password()))
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
    fn a_generated_password_is_long_random_and_verifies() {
        let a = generate_password();
        let b = generate_password();
        assert_eq!(a.len(), 32, "16 random bytes, hex");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert!(verify_password(&a, &hash_password(&a)));
    }

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

    /// Everything in a PHC string before the salt: algorithm, version, cost parameters.
    fn phc_params(hash: &str) -> &str {
        let hash_at = hash.rfind('$').unwrap();
        &hash[..hash[..hash_at].rfind('$').unwrap()]
    }

    #[test]
    fn the_dummy_hash_costs_what_a_real_one_does() {
        let dummy = dummy_password_hash();
        assert!(phc_params(dummy).starts_with("$argon2id$"));
        assert_eq!(phc_params(dummy), phc_params(&hash_password("fiber")));
        assert_eq!(dummy, dummy_password_hash(), "computed once");
    }

    #[tokio::test]
    async fn blocking_verify_matches_the_inline_one_and_never_accepts_a_missing_user() {
        let h = hash_password_blocking("fiber").await.unwrap();
        assert!(verify_password_blocking("fiber", Some(&h)).await.unwrap());
        assert!(!verify_password_blocking("wrong", Some(&h)).await.unwrap());
        assert!(!verify_password_blocking("fiber", None).await.unwrap());
        assert!(!verify_password_blocking("", None).await.unwrap());
    }

    fn legacy_hash(password: &str) -> String {
        let salt_hex = hex::encode([1u8; 16]);
        let mut hasher = Sha256::new();
        hasher.update(salt_hex.as_bytes());
        hasher.update(password.as_bytes());
        format!("{salt_hex}${}", hex::encode(hasher.finalize()))
    }

    /// A legacy hash is one SHA-256. Unless it also pays for an argon2id verify, an
    /// account still on one answers in microseconds and stands out from every other.
    #[test]
    fn a_legacy_hash_pays_for_an_argon2_verify_too() {
        let burned = || BURNED_VERIFIES.with(|n| n.get());
        let stored = legacy_hash("fiber");
        let before = burned();
        assert!(verify_password("fiber", &stored));
        assert!(!verify_password("wrong", &stored));
        assert_eq!(
            burned() - before,
            2,
            "one per legacy verify, right or wrong"
        );
        // A current hash is already an argon2id verify and pays once, not twice.
        let before = burned();
        assert!(verify_password("fiber", &hash_password("fiber")));
        assert_eq!(burned(), before);
    }

    #[test]
    fn constant_time_eq_is_plain_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"xbc", b"abc"));
        assert!(constant_time_eq(b"", b""));
    }

    fn is_busy(e: &anyhow::Error) -> bool {
        e.downcast_ref::<HashBusy>().is_some()
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..400 {
            if done() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// The hole in a permit held by the caller: a client that sends a login and hangs up
    /// frees the slot while argon2 is still running, so the next request starts another.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slot_is_held_until_the_work_ends_even_if_the_caller_is_dropped() {
        use std::sync::{Arc, mpsc};
        let gate = Arc::new(HashGate::new(1, 8, std::time::Duration::from_millis(30)));
        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let caller = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move {
                gate.run(move || {
                    started_tx.send(()).unwrap();
                    // Stands in for argon2: runs until told to stop.
                    let _ = release_rx.recv();
                })
                .await
            }
        });
        tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
            .await
            .unwrap();
        // The request goes away mid-verify.
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());

        assert_eq!(
            gate.slots.available_permits(),
            0,
            "the work is still running"
        );
        let err = gate.run(|| ()).await.unwrap_err();
        assert!(is_busy(&err), "{err:#}");

        release_tx.send(()).unwrap();
        until("the slot to come back", || {
            gate.slots.available_permits() == 1
        })
        .await;
        gate.run(|| ()).await.unwrap();
        assert_eq!(gate.waiting.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_more_than_the_limit_run_at_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        const LIMIT: usize = 3;
        let gate = Arc::new(HashGate::new(
            LIMIT,
            100,
            std::time::Duration::from_secs(30),
        ));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let jobs: Vec<_> = (0..24)
            .map(|_| {
                let (gate, running, peak) = (gate.clone(), running.clone(), peak.clone());
                tokio::spawn(async move {
                    gate.run(move || {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        running.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                })
            })
            .collect();
        for job in jobs {
            job.await.unwrap().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), LIMIT, "reached, never passed");
        assert_eq!(gate.slots.available_permits(), LIMIT);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_waiter_gives_up_after_the_wait_and_the_queue_itself_is_capped() {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, mpsc};
        let gate = Arc::new(HashGate::new(1, 1, std::time::Duration::from_millis(100)));
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let holder = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.run(move || release_rx.recv().unwrap()).await }
        });
        until("the slot to be taken", || {
            gate.slots.available_permits() == 0
        })
        .await;

        let waiter = tokio::spawn({
            let gate = Arc::clone(&gate);
            async move { gate.run(|| ()).await }
        });
        until("the waiter to queue", || {
            gate.waiting.load(Ordering::SeqCst) == 1
        })
        .await;
        // The queue is full: refused at once, not after the wait.
        let started = std::time::Instant::now();
        assert!(is_busy(&gate.run(|| ()).await.unwrap_err()));
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        // The one in the queue waits its turn out, then gives up and leaves the queue.
        assert!(is_busy(&waiter.await.unwrap().unwrap_err()));
        assert_eq!(gate.waiting.load(Ordering::SeqCst), 0);

        release_tx.send(()).unwrap();
        holder.await.unwrap().unwrap();
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
