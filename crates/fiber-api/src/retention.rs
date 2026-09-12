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
        let days = std::env::var("FIBER_RETENTION_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let keep_per_pipeline = std::env::var("FIBER_RETENTION_KEEP_RUNS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);
        let batch = std::env::var("FIBER_RETENTION_BATCH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100);
        let interval_secs = std::env::var("FIBER_RETENTION_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);
        let fiber_days = std::env::var("FIBER_RETENTION_FIBER_DAYS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(7);
        Self {
            days,
            keep_per_pipeline,
            batch: batch.max(1),
            interval: StdDuration::from_secs(interval_secs.max(60)),
            fiber_days,
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

    let mut run_ids: BTreeSet<Uuid> = BTreeSet::new();
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for (id, path) in rows {
        run_ids.insert(id);
        if let Some(p) = path {
            if !p.is_empty() {
                paths.insert(p);
            }
        }
    }

    if run_ids.is_empty() {
        return Ok(0);
    }

    // Delete the rows first, then only those blobs nothing points at any more: a retry
    // carries its predecessor's artifact rows forward, so two runs can share one blob.
    let ids: Vec<Uuid> = run_ids.into_iter().collect();
    let deleted = store.delete_runs(&ids).await?;

    let candidates: Vec<String> = paths.iter().cloned().collect();
    let still_referenced = store
        .artifact_paths_still_referenced(&candidates)
        .await
        .unwrap_or_default();
    let mut removed = 0usize;
    for path in &paths {
        if still_referenced.contains(path) {
            continue;
        }
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
