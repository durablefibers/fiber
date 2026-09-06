//! In-memory login throttle.
//!
//! Counts failed logins per (lower-cased) username and locks the username out for a
//! short period after too many failures. Keyed by username rather than client IP
//! because `fiber-api` usually sits behind a reverse proxy and `X-Forwarded-For` is
//! attacker-controlled without extra configuration. State is per process; a
//! multi-instance deployment gets `MAX_FAILURES` per instance, which is still a
//! hard ceiling on brute force against argon2id.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Failures allowed inside `WINDOW` before a lockout.
pub const MAX_FAILURES: u32 = 10;
/// Failure counting window.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);
/// Lockout applied once `MAX_FAILURES` is reached.
pub const LOCKOUT: Duration = Duration::from_secs(60);
/// Prune the table when it grows past this many usernames.
const PRUNE_ABOVE: usize = 10_000;
/// Longest username retained as a key.
const MAX_KEY_CHARS: usize = 128;

#[derive(Default)]
pub struct LoginGuard {
    inner: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    failures: u32,
    window_start: Instant,
    locked_until: Option<Instant>,
}

impl LoginGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Normalised, length-capped map key: the login body is attacker-controlled, so an
    /// unbounded username must not become an unbounded allocation per failed attempt.
    pub fn key(username: &str) -> String {
        username
            .trim()
            .chars()
            .take(MAX_KEY_CHARS)
            .flat_map(char::to_lowercase)
            .collect()
    }

    /// `Err(retry_after)` when the username is currently locked out.
    pub fn check(&self, key: &str) -> Result<(), Duration> {
        self.check_at(key, Instant::now())
    }

    /// Record a failed attempt; returns the lockout duration if this attempt triggered one.
    pub fn record_failure(&self, key: &str) -> Option<Duration> {
        self.record_failure_at(key, Instant::now())
    }

    pub fn record_success(&self, key: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(key);
    }

    fn check_at(&self, key: &str, now: Instant) -> Result<(), Duration> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > PRUNE_ABOVE {
            map.retain(|_, e| {
                e.locked_until.is_some_and(|t| t > now) || now - e.window_start < WINDOW
            });
            // A flood of distinct usernames keeps every entry inside WINDOW; under that
            // pressure keep only active lockouts — losing partial counts is the cheap side.
            if map.len() > PRUNE_ABOVE {
                map.retain(|_, e| e.locked_until.is_some_and(|t| t > now));
            }
        }
        let Some(entry) = map.get(key) else {
            return Ok(());
        };
        match entry.locked_until {
            Some(until) if until > now => Err(until - now),
            _ => Ok(()),
        }
    }

    fn record_failure_at(&self, key: &str, now: Instant) -> Option<Duration> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(key.to_string()).or_insert(Entry {
            failures: 0,
            window_start: now,
            locked_until: None,
        });
        // A lockout that has expired, or a stale window, starts a fresh count.
        let lock_expired = entry.locked_until.is_some_and(|t| t <= now);
        if lock_expired || now - entry.window_start >= WINDOW {
            entry.failures = 0;
            entry.window_start = now;
            entry.locked_until = None;
        }
        entry.failures += 1;
        if entry.failures >= MAX_FAILURES {
            entry.locked_until = Some(now + LOCKOUT);
            Some(LOCKOUT)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_after_max_failures_and_expires() {
        let g = LoginGuard::new();
        let t0 = Instant::now();
        for i in 1..MAX_FAILURES {
            assert_eq!(g.record_failure_at("admin", t0), None, "failure {i}");
            assert!(g.check_at("admin", t0).is_ok());
        }
        assert_eq!(g.record_failure_at("admin", t0), Some(LOCKOUT));
        let retry = g.check_at("admin", t0).unwrap_err();
        assert!(retry <= LOCKOUT && retry > Duration::ZERO);
        assert!(g.check_at("admin", t0 + LOCKOUT).is_ok());
        // After the lockout a fresh window starts.
        assert_eq!(g.record_failure_at("admin", t0 + LOCKOUT), None);
    }

    #[test]
    fn success_clears_and_window_resets() {
        let g = LoginGuard::new();
        let t0 = Instant::now();
        for _ in 0..(MAX_FAILURES - 1) {
            g.record_failure_at("bob", t0);
        }
        g.record_success("bob");
        assert_eq!(g.record_failure_at("bob", t0), None);
        // Failures older than WINDOW do not count.
        let g = LoginGuard::new();
        for _ in 0..(MAX_FAILURES - 1) {
            g.record_failure_at("eve", t0);
        }
        assert_eq!(g.record_failure_at("eve", t0 + WINDOW), None);
    }

    #[test]
    fn keys_are_case_insensitive_trimmed_and_capped() {
        assert_eq!(LoginGuard::key("  Admin "), "admin");
        assert_eq!(
            LoginGuard::key(&"x".repeat(10_000)).chars().count(),
            MAX_KEY_CHARS
        );
    }

    #[test]
    fn flood_of_usernames_does_not_grow_without_bound() {
        let g = LoginGuard::new();
        let t0 = Instant::now();
        for i in 0..(PRUNE_ABOVE + 500) {
            g.record_failure_at(&format!("user{i}"), t0);
        }
        // The next check prunes: nothing is locked, so the table collapses.
        assert!(g.check_at("anyone", t0).is_ok());
        assert!(g.inner.lock().unwrap().len() <= PRUNE_ABOVE);
    }
}
