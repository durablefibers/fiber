//! Background retention / GC for finished runs, artifact blobs, and sessions.

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
        Self {
            days,
            keep_per_pipeline,
            batch: batch.max(1),
            interval: StdDuration::from_secs(interval_secs.max(60)),
        }
    }

    pub fn enabled(&self) -> bool {
        self.days > 0
    }
}

pub async fn run_once(store: &Store, artifacts: &ArtifactBackend, cfg: &RetentionConfig) -> Result<u64> {
    let sessions = store.purge_expired_sessions().await.unwrap_or(0);
    if sessions > 0 {
        info!(sessions, "purged expired sessions");
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

    for path in &paths {
        if let Err(e) = artifacts.delete(path).await {
            warn!(%path, error = %e, "artifact blob delete failed (continuing)");
        }
    }

    let ids: Vec<Uuid> = run_ids.into_iter().collect();
    let deleted = store.delete_runs(&ids).await?;
    info!(
        deleted,
        blobs = paths.len(),
        days = cfg.days,
        keep = cfg.keep_per_pipeline,
        "retention purged terminal runs"
    );
    Ok(deleted)
}

pub async fn retention_loop(store: Store, artifacts: ArtifactBackend, cfg: RetentionConfig) {
    if !cfg.enabled() {
        info!("retention disabled (FIBER_RETENTION_DAYS=0)");
        // Still purge sessions occasionally.
        loop {
            let _ = store.purge_expired_sessions().await;
            tokio::time::sleep(cfg.interval).await;
        }
    } else {
        info!(
            days = cfg.days,
            keep = cfg.keep_per_pipeline,
            batch = cfg.batch,
            interval_secs = cfg.interval.as_secs(),
            "retention GC enabled"
        );
    }

    loop {
        if let Err(e) = run_once(&store, &artifacts, &cfg).await {
            warn!(error = %e, "retention tick failed");
        }
        tokio::time::sleep(cfg.interval).await;
    }
}
