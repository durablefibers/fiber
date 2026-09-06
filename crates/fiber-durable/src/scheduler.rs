use crate::engine::run_fiber;
use crate::registry::FiberRegistry;
use crate::store::FiberStore;
use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

const POLL_SECS: u64 = 2;
const STALE_AFTER_SECS: i64 = 60;
const MAX_ATTEMPTS: i32 = 3;
/// Fibers claimed per sweep. They run concurrently, so a slow one cannot let the
/// others' claims go stale (and be re-claimed elsewhere) while they wait their turn.
const CLAIM_BATCH: i64 = 20;

#[derive(Clone)]
pub struct FiberScheduler {
    store: FiberStore,
    registry: FiberRegistry,
}

impl FiberScheduler {
    pub fn new(store: FiberStore, registry: FiberRegistry) -> Self {
        Self { store, registry }
    }

    pub fn store(&self) -> &FiberStore {
        &self.store
    }

    pub fn registry(&self) -> &FiberRegistry {
        &self.registry
    }

    pub async fn seed(&self) -> anyhow::Result<()> {
        self.store.seed_due_index(STALE_AFTER_SECS).await
    }

    pub async fn run_loop(self: Arc<Self>) {
        if let Err(e) = self.seed().await {
            warn!(error = %e, "fiber due-index seed failed");
        }
        loop {
            if let Err(e) = self.sweep_once().await {
                warn!(error = %e, "fiber sweep failed");
            }
            tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
        }
    }

    #[tracing::instrument(skip(self), level = "debug")]
    pub async fn sweep_once(&self) -> anyhow::Result<usize> {
        let now = Utc::now();
        let due_index = self.store.due_index();

        // If index empty or has due projects, run ready query (DB is source of truth).
        // Index avoids work when earliest due is in the future.
        if let Some(earliest) = due_index.earliest() {
            if earliest > now && due_index.due_keys(now).is_empty() {
                debug!("no fibers due yet");
                return Ok(0);
            }
        }

        let ready = self
            .store
            .claim_ready(STALE_AFTER_SECS, CLAIM_BATCH)
            .await?;
        let mut n = 0;
        let mut touched = std::collections::HashSet::new();
        let mut tasks = tokio::task::JoinSet::new();
        for record in ready {
            touched.insert(record.project_id);
            let store = self.store.clone();
            let registry = self.registry.clone();
            tasks.spawn(async move {
                let name = record.name.clone();
                let id = record.id;
                let result = run_fiber(&store, &registry, record, MAX_ATTEMPTS).await;
                (id, name, result)
            });
        }
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((id, name, Ok(outcome))) => {
                    info!(%id, %name, ?outcome, "fiber ran");
                    n += 1;
                }
                Ok((id, name, Err(e))) => warn!(%id, %name, error = %e, "fiber run error"),
                Err(e) => warn!(error = %e, "fiber task panicked"),
            }
        }
        for project_id in touched {
            let _ = self
                .store
                .refresh_project_due(project_id, Utc::now(), STALE_AFTER_SECS)
                .await;
        }
        Ok(n)
    }
}
