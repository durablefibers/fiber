//! Keeping the background loops alive, and admitting when they are not.
//!
//! `reclaim`, `schedules`, `events`, `agent_cmds`, `fibers`, `github_status` and
//! `retention` are what make the control plane do anything between requests. Spawned
//! bare, a panic in one kills that task silently: the process stays up, `/ready` keeps
//! answering `ok`, and leases stop being reclaimed or schedules stop firing with nothing
//! to say so. That is the worst shape of failure — healthy-looking and invisible.

use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Restart backoff, capped. A loop that panics on every iteration should not spin.
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A loop that stayed up this long before dying had recovered from whatever it was
/// failing on. Its next restart starts the backoff over rather than inheriting a 60 s
/// wait from a bad hour last week — with `/ready` answering 503 for the whole of it.
const HEALTHY_AFTER: Duration = Duration::from_secs(60);

/// How long to wait before restarting a loop, given the delay used for its previous
/// restart (`None` for the first) and how long it ran this time.
fn restart_delay(last: Option<Duration>, ran_for: Duration) -> Duration {
    match last {
        Some(d) if ran_for < HEALTHY_AFTER => (d * 2).min(MAX_BACKOFF),
        _ => MIN_BACKOFF,
    }
}

#[derive(Debug, Clone)]
pub struct LoopState {
    /// False while the loop is down and waiting to be restarted.
    pub running: bool,
    /// How many times it has come back. Non-zero is worth looking at even when running.
    pub restarts: u64,
    pub last_exit: Option<String>,
    pub since: DateTime<Utc>,
}

#[derive(Default)]
pub struct LoopHealth {
    loops: Mutex<BTreeMap<&'static str, LoopState>>,
}

impl LoopHealth {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn mark(&self, name: &'static str, running: bool, exit: Option<String>) {
        let mut g = self.loops.lock().expect("loop health lock");
        let e = g.entry(name).or_insert_with(|| LoopState {
            running,
            restarts: 0,
            last_exit: None,
            since: Utc::now(),
        });
        if !running {
            e.restarts += 1;
            e.last_exit = exit;
        }
        e.running = running;
        e.since = Utc::now();
    }

    pub fn snapshot(&self) -> BTreeMap<&'static str, LoopState> {
        self.loops.lock().expect("loop health lock").clone()
    }

    /// Loops that are down right now. Empty is the healthy answer.
    pub fn down(&self) -> Vec<&'static str> {
        self.snapshot()
            .into_iter()
            .filter(|(_, s)| !s.running)
            .map(|(n, _)| n)
            .collect()
    }
}

/// Spawn `make()` and keep it running.
///
/// The future is spawned as its own task so a panic is caught by the join handle rather
/// than taking this supervisor with it. `make` is called again for each restart, so the
/// loop starts from a clean state instead of resuming whatever it panicked in the middle
/// of.
pub fn supervise<F, Fut>(name: &'static str, health: Arc<LoopHealth>, make: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut last_delay = None;
        loop {
            health.mark(name, true, None);
            let started = std::time::Instant::now();
            let exit = match tokio::spawn(make()).await {
                // These loops are written to run forever, so returning is itself wrong.
                Ok(()) => "returned unexpectedly".to_string(),
                Err(e) if e.is_panic() => "panicked".to_string(),
                Err(_) => "cancelled".to_string(),
            };
            let delay = restart_delay(last_delay, started.elapsed());
            tracing::error!(
                loop_name = name,
                reason = %exit,
                delay_secs = delay.as_secs(),
                "background loop stopped; restarting"
            );
            health.mark(name, false, Some(exit));
            tokio::time::sleep(delay).await;
            last_delay = Some(delay);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_while_the_loop_keeps_dying_quickly() {
        let quick = Duration::from_millis(100);
        assert_eq!(restart_delay(None, quick), MIN_BACKOFF);
        assert_eq!(
            restart_delay(Some(Duration::from_secs(1)), quick),
            Duration::from_secs(2)
        );
        assert_eq!(
            restart_delay(Some(Duration::from_secs(32)), quick),
            Duration::from_secs(60)
        );
        assert_eq!(restart_delay(Some(MAX_BACKOFF), quick), MAX_BACKOFF);
    }

    #[test]
    fn backoff_resets_after_a_healthy_run() {
        // A loop at the 60 s cap that then ran for an hour is not the loop that was
        // crashing: its next restart should be quick again.
        assert_eq!(
            restart_delay(Some(MAX_BACKOFF), Duration::from_secs(3600)),
            MIN_BACKOFF
        );
        assert_eq!(restart_delay(Some(MAX_BACKOFF), HEALTHY_AFTER), MIN_BACKOFF);
        // Just short of healthy still counts as a quick death.
        assert_eq!(
            restart_delay(
                Some(Duration::from_secs(4)),
                HEALTHY_AFTER - Duration::from_secs(1)
            ),
            Duration::from_secs(8)
        );
    }

    #[test]
    fn a_fresh_supervisor_reports_nothing_down() {
        let h = LoopHealth::new();
        assert!(h.down().is_empty());
    }

    #[tokio::test]
    async fn a_panicking_loop_is_restarted_and_counted() {
        let h = LoopHealth::new();
        let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let c = calls.clone();
        supervise("boom", h.clone(), move || {
            let c = c.clone();
            async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                panic!("boom");
            }
        });
        // Long enough for the first failure plus one backoff.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "loop was not restarted"
        );
        let s = h.snapshot();
        assert!(s["boom"].restarts >= 1, "restarts not counted: {s:?}");
        assert_eq!(s["boom"].last_exit.as_deref(), Some("panicked"));
    }

    #[tokio::test]
    async fn a_loop_that_returns_counts_as_a_failure() {
        // Every one of these is an infinite loop; returning means something ended it.
        let h = LoopHealth::new();
        supervise("quitter", h.clone(), || async {});
        tokio::time::sleep(Duration::from_millis(300)).await;
        let s = h.snapshot();
        assert_eq!(
            s["quitter"].last_exit.as_deref(),
            Some("returned unexpectedly")
        );
    }
}
