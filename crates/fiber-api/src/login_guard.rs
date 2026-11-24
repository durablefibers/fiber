//! In-memory login throttle.
//!
//! Counts failed logins per (lower-cased) username and locks the username out for a
//! short period after too many failures. Keyed by username rather than client IP
//! because `fiber-api` usually sits behind a reverse proxy and `X-Forwarded-For` is
//! attacker-controlled without extra configuration. State is per process; a
//! multi-instance deployment gets `MAX_FAILURES` per instance, which is still a
//! hard ceiling on brute force against argon2id.
//!
//! An attempt is **reserved before** the password is verified and settled afterwards:
//! [`LoginGuard::begin_attempt`] counts recorded failures plus attempts still in flight,
//! so a burst of parallel requests cannot all pass a read-only check and then each get a
//! guess. The reservation is an [`Attempt`]; dropping it without [`Attempt::finish`]
//! releases the slot, so an early return in the handler cannot hold one forever.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Failures allowed inside `WINDOW` before a lockout. Also the most attempts for one
/// username that may be unsettled at once, since failures and in-flight attempts share
/// the budget.
pub const MAX_FAILURES: u32 = 10;
/// Failure counting window.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);
/// Lockout applied once `MAX_FAILURES` is reached.
pub const LOCKOUT: Duration = Duration::from_secs(60);
/// `Retry-After` for an attempt refused because the budget is held by attempts that have
/// not settled yet (rather than by a lockout, which knows its own remaining time).
pub const IN_FLIGHT_RETRY: Duration = Duration::from_secs(1);
/// The table never holds more than this many usernames.
const MAX_ENTRIES: usize = 10_000;
/// Eviction frees down to this many, so a flood pays for one sort per
/// `MAX_ENTRIES - EVICT_TO` new usernames rather than one per request.
const EVICT_TO: usize = MAX_ENTRIES - MAX_ENTRIES / 10;
/// Longest username retained as a key.
const MAX_KEY_CHARS: usize = 128;

#[derive(Default)]
pub struct LoginGuard {
    inner: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    failures: u32,
    /// Attempts admitted by `begin_attempt` and not yet settled.
    in_flight: u32,
    window_start: Instant,
    locked_until: Option<Instant>,
}

impl Entry {
    fn locked_at(&self, now: Instant) -> bool {
        self.locked_until.is_some_and(|t| t > now)
    }
}

enum Outcome {
    Success,
    Failure,
    /// Dropped without a verdict (store error, cancelled request): releases the slot
    /// and counts as neither.
    Abandoned,
}

/// One admitted login attempt. Settle it with [`Attempt::finish`]; dropping it instead
/// releases the slot without recording a failure or a success.
#[must_use = "an unsettled attempt is released on drop and records nothing"]
pub struct Attempt {
    guard: Arc<LoginGuard>,
    key: String,
    open: bool,
}

impl Attempt {
    /// Release the slot and record the verdict; returns the lockout duration if this
    /// failure triggered one.
    pub fn finish(self, success: bool) -> Option<Duration> {
        self.finish_at(success, Instant::now())
    }

    fn finish_at(mut self, success: bool, now: Instant) -> Option<Duration> {
        self.open = false;
        let outcome = if success {
            Outcome::Success
        } else {
            Outcome::Failure
        };
        self.guard.settle(&self.key, outcome, now)
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if self.open {
            self.guard
                .settle(&self.key, Outcome::Abandoned, Instant::now());
        }
    }
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

    /// Reserve one attempt for `key`. `Err(retry_after)` when the username is locked
    /// out, or when its recorded failures plus the attempts still in flight already
    /// fill the budget.
    pub fn begin_attempt(self: &Arc<Self>, key: &str) -> Result<Attempt, Duration> {
        self.begin_attempt_at(key, Instant::now())
    }

    fn begin_attempt_at(self: &Arc<Self>, key: &str, now: Instant) -> Result<Attempt, Duration> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !map.contains_key(key) && map.len() >= MAX_ENTRIES {
            evict(&mut map, now);
        }
        let entry = map.entry(key.to_string()).or_insert(Entry {
            failures: 0,
            in_flight: 0,
            window_start: now,
            locked_until: None,
        });
        if let Some(until) = entry.locked_until
            && until > now
        {
            return Err(until - now);
        }
        // A lockout that has expired, or a stale window, starts a fresh count.
        if entry.locked_until.is_some() || now - entry.window_start >= WINDOW {
            entry.failures = 0;
            entry.window_start = now;
            entry.locked_until = None;
        }
        if entry.failures + entry.in_flight >= MAX_FAILURES {
            return Err(IN_FLIGHT_RETRY);
        }
        entry.in_flight += 1;
        Ok(Attempt {
            guard: Arc::clone(self),
            key: key.to_string(),
            open: true,
        })
    }

    fn settle(&self, key: &str, outcome: Outcome, now: Instant) -> Option<Duration> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Present by construction: eviction never takes an entry with attempts in flight.
        let entry = map.get_mut(key)?;
        entry.in_flight = entry.in_flight.saturating_sub(1);
        let lock = match outcome {
            Outcome::Success => {
                entry.failures = 0;
                entry.locked_until = None;
                None
            }
            Outcome::Failure => {
                entry.failures += 1;
                if entry.failures >= MAX_FAILURES {
                    entry.locked_until = Some(now + LOCKOUT);
                    Some(LOCKOUT)
                } else {
                    None
                }
            }
            Outcome::Abandoned => None,
        };
        if entry.in_flight == 0 && entry.failures == 0 && entry.locked_until.is_none() {
            map.remove(key);
        }
        lock
    }
}

/// Make room in a full table.
///
/// Expired entries go first. If that is not enough — a flood of distinct usernames keeps
/// every entry inside `WINDOW` — the cheapest state to lose goes next: unlocked before
/// locked, fewest failures first, oldest window first. A flood of junk usernames holds one
/// failure each, so it evicts itself before it reaches a real account's partial count; the
/// old behaviour (drop everything not locked out) let that flood reset any count on demand.
/// An entry with attempts in flight is never evicted: its `Attempt` still has to settle.
fn evict(map: &mut HashMap<String, Entry>, now: Instant) {
    map.retain(|_, e| {
        e.in_flight > 0 || e.locked_at(now) || (e.failures > 0 && now - e.window_start < WINDOW)
    });
    if map.len() <= EVICT_TO {
        return;
    }
    let mut victims: Vec<(bool, u32, Instant, String)> = map
        .iter()
        .filter(|(_, e)| e.in_flight == 0)
        .map(|(k, e)| (e.locked_at(now), e.failures, e.window_start, k.clone()))
        .collect();
    victims.sort_unstable();
    let excess = map.len() - EVICT_TO;
    for (_, _, _, key) in victims.into_iter().take(excess) {
        map.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> Arc<LoginGuard> {
        Arc::new(LoginGuard::new())
    }

    /// One whole failed login at `now`: reserve, then settle as a failure.
    fn fail(g: &Arc<LoginGuard>, key: &str, now: Instant) -> Option<Duration> {
        g.begin_attempt_at(key, now)
            .unwrap_or_else(|_| panic!("attempt for {key} should be admitted"))
            .finish_at(false, now)
    }

    fn failures(g: &LoginGuard, key: &str) -> Option<u32> {
        g.inner.lock().unwrap().get(key).map(|e| e.failures)
    }

    #[test]
    fn locks_after_max_failures_and_expires() {
        let g = guard();
        let t0 = Instant::now();
        for i in 1..MAX_FAILURES {
            assert_eq!(fail(&g, "admin", t0), None, "failure {i}");
        }
        assert_eq!(fail(&g, "admin", t0), Some(LOCKOUT));
        let retry = g.begin_attempt_at("admin", t0).err().expect("locked");
        assert!(retry <= LOCKOUT && retry > Duration::ZERO);
        // After the lockout a fresh window starts.
        assert_eq!(fail(&g, "admin", t0 + LOCKOUT), None);
        assert_eq!(failures(&g, "admin"), Some(1));
    }

    #[test]
    fn success_clears_and_window_resets() {
        let g = guard();
        let t0 = Instant::now();
        for _ in 0..(MAX_FAILURES - 1) {
            fail(&g, "bob", t0);
        }
        g.begin_attempt_at("bob", t0)
            .expect("admitted")
            .finish_at(true, t0);
        assert_eq!(failures(&g, "bob"), None, "a clean entry is removed");
        assert_eq!(fail(&g, "bob", t0), None);
        // Failures older than WINDOW do not count.
        let g = guard();
        for _ in 0..(MAX_FAILURES - 1) {
            fail(&g, "eve", t0);
        }
        assert_eq!(fail(&g, "eve", t0 + WINDOW), None);
    }

    #[test]
    fn keys_are_case_insensitive_trimmed_and_capped() {
        assert_eq!(LoginGuard::key("  Admin "), "admin");
        assert_eq!(
            LoginGuard::key(&"x".repeat(10_000)).chars().count(),
            MAX_KEY_CHARS
        );
    }

    /// The bypass this module exists to close: every request in a parallel burst used to
    /// pass a read-only check before any of them recorded a failure.
    #[test]
    fn concurrent_attempts_admit_at_most_the_limit() {
        let g = guard();
        let burst = 64;
        let barrier = Arc::new(std::sync::Barrier::new(burst));
        let handles: Vec<_> = (0..burst)
            .map(|_| {
                let (g, barrier) = (Arc::clone(&g), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    g.begin_attempt("admin").ok()
                })
            })
            .collect();
        // Hold every admitted attempt open, as the handler does across the verify.
        let admitted: Vec<Attempt> = handles
            .into_iter()
            .filter_map(|h| h.join().expect("thread"))
            .collect();
        assert_eq!(admitted.len(), MAX_FAILURES as usize);
        assert_eq!(
            g.begin_attempt("admin").err(),
            Some(IN_FLIGHT_RETRY),
            "the budget is held by attempts in flight"
        );
        // All of them fail: exactly MAX_FAILURES guesses were bought, and the lockout holds.
        let locks: Vec<_> = admitted.into_iter().map(|a| a.finish(false)).collect();
        assert_eq!(locks.iter().flatten().count(), 1);
        assert!(g.begin_attempt("admin").is_err());
    }

    #[test]
    fn failures_and_in_flight_attempts_share_the_budget() {
        let g = guard();
        let t0 = Instant::now();
        for _ in 0..(MAX_FAILURES - 2) {
            fail(&g, "admin", t0);
        }
        let a = g.begin_attempt_at("admin", t0).expect("ninth");
        let b = g.begin_attempt_at("admin", t0).expect("tenth");
        assert_eq!(g.begin_attempt_at("admin", t0).err(), Some(IN_FLIGHT_RETRY));
        drop((a, b));
    }

    #[test]
    fn a_dropped_attempt_releases_its_slot_and_records_nothing() {
        let g = guard();
        let held: Vec<Attempt> = (0..MAX_FAILURES)
            .map(|_| g.begin_attempt("admin").expect("admitted"))
            .collect();
        assert!(g.begin_attempt("admin").is_err());
        // What an early `?` in the handler does to the reservation.
        drop(held);
        assert!(
            g.inner.lock().unwrap().is_empty(),
            "no slot held, no failure recorded, no entry left behind"
        );
        let again: Vec<Attempt> = (0..MAX_FAILURES)
            .map(|_| g.begin_attempt("admin").expect("slots came back"))
            .collect();
        assert_eq!(again.len(), MAX_FAILURES as usize);
    }

    #[test]
    fn flood_of_usernames_does_not_grow_without_bound() {
        let g = guard();
        let t0 = Instant::now();
        for i in 0..(MAX_ENTRIES * 2) {
            fail(&g, &format!("user{i}"), t0);
            assert!(g.inner.lock().unwrap().len() <= MAX_ENTRIES);
        }
    }

    /// The old pruning dropped every entry that was not locked out, so nine guesses, a
    /// flood of junk usernames, nine more guesses — repeated — never reached a lockout.
    #[test]
    fn a_junk_username_flood_does_not_erase_a_targets_count() {
        let g = guard();
        let t0 = Instant::now();
        for _ in 0..(MAX_FAILURES - 1) {
            fail(&g, "admin", t0);
        }
        for i in 0..(MAX_ENTRIES * 2) {
            fail(&g, &format!("junk{i}"), t0);
        }
        assert_eq!(failures(&g, "admin"), Some(MAX_FAILURES - 1));
        assert_eq!(fail(&g, "admin", t0), Some(LOCKOUT));
    }

    #[test]
    fn eviction_spares_attempts_in_flight_and_takes_expired_entries_first() {
        let g = guard();
        let t0 = Instant::now();
        let held = g.begin_attempt_at("in-flight", t0).expect("admitted");
        fail(&g, "stale", t0);
        let later = t0 + WINDOW;
        for i in 0..MAX_ENTRIES {
            fail(&g, &format!("junk{i}"), later);
        }
        let map = g.inner.lock().unwrap();
        assert!(
            map.contains_key("in-flight"),
            "zero failures, but unsettled"
        );
        assert!(!map.contains_key("stale"));
        drop(map);
        assert_eq!(held.finish_at(false, later), None);
        assert_eq!(failures(&g, "in-flight"), Some(1));
    }
}
