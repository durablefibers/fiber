//! Background retention / GC for finished runs, artifact blobs, durable fibers and
//! sessions.

use crate::artifacts::ArtifactBackend;
use anyhow::Result;
use chrono::{Duration, Utc};
use fiber_core::Store;
use std::collections::BTreeSet;
use std::time::Duration as StdDuration;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct RetentionConfig {
    /// Delete terminal runs older than this many days. `0` disables.
    pub days: u64,
    /// Always keep this many newest terminal runs per pipeline (0 = age-only).
    pub keep_per_pipeline: i64,
    /// Max runs deleted per tick.
    pub batch: i64,
    /// Loop sleep between ticks.
    pub interval: StdDuration,
    /// Delete terminal fibers older than this many days. `0` disables. Separate from
    /// `days`: a run is a build someone may want to look back at, while a fiber is usually
    /// a notification that either worked or did not.
    pub fiber_days: u64,
}

impl RetentionConfig {
    pub fn from_env() -> Self {
        let get = |k: &str| std::env::var(k).ok();
        Self::from_raw(
            get("FIBER_RETENTION_DAYS").as_deref(),
            get("FIBER_RETENTION_KEEP_RUNS").as_deref(),
            get("FIBER_RETENTION_BATCH").as_deref(),
            get("FIBER_RETENTION_INTERVAL_SECS").as_deref(),
            get("FIBER_RETENTION_FIBER_DAYS").as_deref(),
        )
    }

    /// The parsing and clamping behind [`Self::from_env`], separated from the environment
    /// so the defaults and the floors can be tested. This config decides what gets
    /// deleted, so "unset" and "nonsense" must both land on the documented default rather
    /// than on zero.
    fn from_raw(
        days: Option<&str>,
        keep_per_pipeline: Option<&str>,
        batch: Option<&str>,
        interval_secs: Option<&str>,
        fiber_days: Option<&str>,
    ) -> Self {
        fn parsed<T: std::str::FromStr>(raw: Option<&str>, default: T) -> T {
            raw.and_then(|v| v.trim().parse().ok()).unwrap_or(default)
        }
        Self {
            days: parsed(days, 30),
            keep_per_pipeline: parsed(keep_per_pipeline, 20),
            // A batch of 0 would delete nothing for ever while looking healthy.
            batch: parsed::<i64>(batch, 100).max(1),
            // A floor on the interval keeps a typo from turning GC into a hot loop.
            interval: StdDuration::from_secs(parsed::<u64>(interval_secs, 3600).max(60)),
            fiber_days: parsed(fiber_days, 7),
        }
    }

    pub fn enabled(&self) -> bool {
        self.days > 0
    }
}

pub async fn run_once(
    store: &Store,
    artifacts: &ArtifactBackend,
    fibers: &fiber_durable::FiberStore,
    cfg: &RetentionConfig,
) -> Result<u64> {
    let sessions = store.purge_expired_sessions().await.unwrap_or(0);
    if sessions > 0 {
        info!(sessions, "purged expired sessions");
    }

    // Independent of the run cutoff: `fibers` has no artifact blobs to reconcile and its
    // own retention period, so it runs whether or not run retention is on.
    if cfg.fiber_days > 0 {
        let cutoff = Utc::now() - Duration::days(cfg.fiber_days as i64);
        match fibers.delete_terminal_before(cutoff, cfg.batch).await {
            Ok(n) if n > 0 => info!(
                fibers = n,
                days = cfg.fiber_days,
                "retention purged terminal fibers"
            ),
            Ok(_) => {}
            Err(e) => warn!(error = %e, "fiber retention failed (continuing)"),
        }
    }

    if !cfg.enabled() {
        return Ok(0);
    }

    let cutoff = Utc::now() - Duration::days(cfg.days as i64);
    let rows = store
        .list_runs_for_retention(cutoff, cfg.keep_per_pipeline, cfg.batch)
        .await?;

    let (run_ids, paths) = partition_retention_rows(rows);

    if run_ids.is_empty() {
        return Ok(0);
    }

    // Delete the rows first, then only those blobs nothing points at any more: a retry
    // carries its predecessor's artifact rows forward, so two runs can share one blob.
    let ids: Vec<Uuid> = run_ids.into_iter().collect();
    let deleted = store.delete_runs(&ids).await?;

    let candidates: Vec<String> = paths.iter().cloned().collect();
    // A failure here must not read as "nothing references these". `unwrap_or_default`
    // made an empty result out of a transient error, and every candidate blob then looked
    // unreferenced — deleting artifacts that a surviving retry still points at. Leaking a
    // few blobs until someone reconciles them is the better failure.
    let still_referenced = match store.artifact_paths_still_referenced(&candidates).await {
        Ok(paths) => paths,
        Err(e) => {
            warn!(
                error = %e,
                candidates = candidates.len(),
                "could not check artifact references; keeping every blob this tick"
            );
            return Ok(deleted);
        }
    };
    let mut removed = 0usize;
    for path in unreferenced_blobs(&paths, &still_referenced) {
        match artifacts.delete(path).await {
            Ok(()) => removed += 1,
            Err(e) => warn!(%path, error = %e, "artifact blob delete failed (continuing)"),
        }
    }
    info!(
        deleted,
        blobs = removed,
        blobs_kept = still_referenced.len(),
        days = cfg.days,
        keep = cfg.keep_per_pipeline,
        "retention purged terminal runs"
    );
    Ok(deleted)
}

/// Split the `(run_id, artifact_path)` rows into the runs to delete and the distinct
/// blob paths they reference. A run with many artifacts appears on many rows, and a run
/// with none has `None`.
fn partition_retention_rows(
    rows: Vec<(Uuid, Option<String>)>,
) -> (BTreeSet<Uuid>, BTreeSet<String>) {
    let mut run_ids: BTreeSet<Uuid> = BTreeSet::new();
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for (id, path) in rows {
        run_ids.insert(id);
        if let Some(p) = path
            && !p.is_empty()
        {
            paths.insert(p);
        }
    }
    (run_ids, paths)
}

/// Candidate blobs that nothing points at any more, and so are safe to delete.
///
/// A retry carries its predecessor's artifact rows forward, so two runs can share one
/// blob: deleting by run alone would break the surviving run's download. Anything still
/// referenced is kept, and an empty `still_referenced` means everything goes — which is
/// why the caller must distinguish "nothing references these" from "the query failed".
fn unreferenced_blobs<'a>(
    candidates: &'a BTreeSet<String>,
    still_referenced: &[String],
) -> Vec<&'a String> {
    candidates
        .iter()
        .filter(|p| !still_referenced.contains(p))
        .collect()
}

pub async fn retention_loop(
    store: Store,
    artifacts: ArtifactBackend,
    fibers: fiber_durable::FiberStore,
    cfg: RetentionConfig,
) {
    // One loop, and `run_once` decides what is switched on. The disabled branch used to
    // loop separately, purging only sessions and never reaching the tick — so anything
    // added to retention later silently did not run when FIBER_RETENTION_DAYS was 0.
    info!(
        runs_days = cfg.days,
        keep = cfg.keep_per_pipeline,
        fiber_days = cfg.fiber_days,
        batch = cfg.batch,
        interval_secs = cfg.interval.as_secs(),
        "retention GC configured (0 days = that part is off)"
    );
    loop {
        if let Err(e) = run_once(&store, &artifacts, &fibers, &cfg).await {
            warn!(error = %e, "retention tick failed");
        }
        tokio::time::sleep(cfg.interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(of: &[&str]) -> BTreeSet<String> {
        of.iter().map(|s| s.to_string()).collect()
    }

    fn owned(of: &[&str]) -> Vec<String> {
        of.iter().map(|s| s.to_string()).collect()
    }

    // --- config ------------------------------------------------------------------------

    #[test]
    fn an_unset_environment_gives_the_documented_defaults() {
        let c = RetentionConfig::from_raw(None, None, None, None, None);
        assert_eq!(c.days, 30);
        assert_eq!(c.keep_per_pipeline, 20);
        assert_eq!(c.batch, 100);
        assert_eq!(c.interval, StdDuration::from_secs(3600));
        assert_eq!(c.fiber_days, 7);
    }

    #[test]
    fn values_are_taken_as_given_when_usable() {
        let c =
            RetentionConfig::from_raw(Some("90"), Some("5"), Some("250"), Some("600"), Some("14"));
        assert_eq!(c.days, 90);
        assert_eq!(c.keep_per_pipeline, 5);
        assert_eq!(c.batch, 250);
        assert_eq!(c.interval, StdDuration::from_secs(600));
        assert_eq!(c.fiber_days, 14);
    }

    #[test]
    fn an_unparseable_value_falls_back_rather_than_becoming_zero() {
        // Zero days would mean "disabled" and zero batch would mean "delete nothing" —
        // both look like working configuration while quietly doing nothing.
        let c = RetentionConfig::from_raw(
            Some("forever"),
            Some(""),
            Some("lots"),
            Some("-"),
            Some("soon"),
        );
        assert_eq!(c.days, 30);
        assert_eq!(c.keep_per_pipeline, 20);
        assert_eq!(c.batch, 100);
        assert_eq!(c.interval, StdDuration::from_secs(3600));
        assert_eq!(c.fiber_days, 7);
    }

    #[test]
    fn a_batch_floor_of_one_keeps_gc_making_progress() {
        assert_eq!(
            RetentionConfig::from_raw(None, None, Some("0"), None, None).batch,
            1
        );
        assert_eq!(
            RetentionConfig::from_raw(None, None, Some("-5"), None, None).batch,
            1
        );
    }

    #[test]
    fn an_interval_floor_keeps_gc_from_becoming_a_hot_loop() {
        let c = RetentionConfig::from_raw(None, None, None, Some("1"), None);
        assert_eq!(c.interval, StdDuration::from_secs(60));
    }

    #[test]
    fn zero_days_disables_run_retention_but_is_still_a_valid_config() {
        let c = RetentionConfig::from_raw(Some("0"), None, None, None, None);
        assert!(!c.enabled());
        assert!(RetentionConfig::from_raw(Some("1"), None, None, None, None).enabled());
    }

    // --- row partitioning --------------------------------------------------------------

    #[test]
    fn a_run_with_several_artifacts_is_counted_once() {
        let run = Uuid::new_v4();
        let (ids, blobs) = partition_retention_rows(vec![
            (run, Some("a.txt".into())),
            (run, Some("b.txt".into())),
        ]);
        assert_eq!(ids.len(), 1);
        assert_eq!(blobs, paths(&["a.txt", "b.txt"]));
    }

    #[test]
    fn a_run_with_no_artifacts_is_still_deleted() {
        let run = Uuid::new_v4();
        let (ids, blobs) = partition_retention_rows(vec![(run, None)]);
        assert_eq!(ids.len(), 1, "the run must still be collected");
        assert!(blobs.is_empty());
    }

    #[test]
    fn an_empty_path_is_not_treated_as_a_blob() {
        // An empty string would be handed to the artifact backend as a key.
        let (_, blobs) = partition_retention_rows(vec![(Uuid::new_v4(), Some(String::new()))]);
        assert!(blobs.is_empty());
    }

    #[test]
    fn two_runs_sharing_a_blob_yield_one_candidate() {
        let (ids, blobs) = partition_retention_rows(vec![
            (Uuid::new_v4(), Some("shared".into())),
            (Uuid::new_v4(), Some("shared".into())),
        ]);
        assert_eq!(ids.len(), 2);
        assert_eq!(blobs.len(), 1);
    }

    #[test]
    fn no_rows_means_nothing_to_do() {
        let (ids, blobs) = partition_retention_rows(vec![]);
        assert!(ids.is_empty() && blobs.is_empty());
    }

    // --- blob selection ----------------------------------------------------------------

    #[test]
    fn a_blob_nothing_points_at_is_deleted() {
        let candidates = paths(&["a", "b"]);
        assert_eq!(unreferenced_blobs(&candidates, &[]), vec!["a", "b"]);
    }

    #[test]
    fn a_blob_a_surviving_retry_still_points_at_is_kept() {
        // The real hazard: a retry carries its predecessor's artifact rows forward, so
        // deleting purely by run would break the surviving run's download.
        let candidates = paths(&["kept", "orphan"]);
        let keep = unreferenced_blobs(&candidates, &owned(&["kept"]));
        assert_eq!(keep, vec!["orphan"], "only the orphan may be deleted");
    }

    #[test]
    fn everything_referenced_means_nothing_is_deleted() {
        let candidates = paths(&["a", "b"]);
        assert!(unreferenced_blobs(&candidates, &owned(&["a", "b"])).is_empty());
    }

    #[test]
    fn a_reference_to_something_we_are_not_considering_is_ignored() {
        let candidates = paths(&["a"]);
        assert_eq!(unreferenced_blobs(&candidates, &owned(&["z"])), vec!["a"]);
    }
}
