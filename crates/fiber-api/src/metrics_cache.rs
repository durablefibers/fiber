//! A short-lived cache for the `/metrics` snapshot.
//!
//! Every scrape used to run six queries, two of them full scans of `step_attempts`. At a
//! 15-second scrape interval and a 30-day retention that is two sequential scans a minute
//! per replica, competing with the lease path for the same connection pool — and every
//! monitoring system that points a second scraper at the instance doubles it. The numbers
//! describe minutes of build activity; serving them a few seconds stale costs nothing.

use fiber_core::MetricsSnapshot;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a snapshot is served before the database is asked again. Below a typical
/// 15-second scrape interval, so a single Prometheus still sees fresh numbers, while a
/// burst of scrapes (or a second scraper) collapses onto one query round.
pub const METRICS_TTL: Duration = Duration::from_secs(10);

pub struct MetricsCache {
    ttl: Duration,
    inner: Mutex<Option<(Instant, MetricsSnapshot)>>,
}

impl MetricsCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(None),
        }
    }

    /// The cached snapshot if it is younger than the TTL at `now`.
    pub fn get_at(&self, now: Instant) -> Option<MetricsSnapshot> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let (at, snapshot) = guard.as_ref()?;
        // `saturating_duration_since`: a clock that appears to go backwards must read as
        // "no time has passed", not as a duration that wraps.
        (now.saturating_duration_since(*at) < self.ttl).then(|| snapshot.clone())
    }

    pub fn put_at(&self, now: Instant, snapshot: MetricsSnapshot) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some((now, snapshot));
    }

    pub fn get(&self) -> Option<MetricsSnapshot> {
        self.get_at(Instant::now())
    }

    pub fn put(&self, snapshot: MetricsSnapshot) {
        self.put_at(Instant::now(), snapshot);
    }
}

impl Default for MetricsCache {
    fn default() -> Self {
        Self::new(METRICS_TTL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(runs: i64) -> MetricsSnapshot {
        MetricsSnapshot {
            runs: vec![("running".into(), runs)],
            ..Default::default()
        }
    }

    #[test]
    fn an_empty_cache_has_nothing_to_serve() {
        assert!(MetricsCache::new(METRICS_TTL).get().is_none());
    }

    #[test]
    fn a_scrape_inside_the_ttl_is_served_from_memory() {
        let cache = MetricsCache::new(Duration::from_secs(10));
        let t0 = Instant::now();
        cache.put_at(t0, snapshot(3));
        let got = cache
            .get_at(t0 + Duration::from_secs(9))
            .expect("still fresh");
        assert_eq!(got.runs, vec![("running".to_string(), 3)]);
    }

    #[test]
    fn a_scrape_past_the_ttl_goes_back_to_the_database() {
        let cache = MetricsCache::new(Duration::from_secs(10));
        let t0 = Instant::now();
        cache.put_at(t0, snapshot(3));
        assert!(cache.get_at(t0 + Duration::from_secs(10)).is_none());
        assert!(cache.get_at(t0 + Duration::from_secs(60)).is_none());
    }

    #[test]
    fn a_fresh_snapshot_replaces_the_stale_one() {
        let cache = MetricsCache::new(Duration::from_secs(10));
        let t0 = Instant::now();
        cache.put_at(t0, snapshot(3));
        let t1 = t0 + Duration::from_secs(11);
        cache.put_at(t1, snapshot(7));
        assert_eq!(
            cache.get_at(t1).expect("the new one is fresh").runs,
            vec![("running".to_string(), 7)]
        );
    }
}
