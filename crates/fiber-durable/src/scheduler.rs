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

        let ready = self.store.list_ready(now, STALE_AFTER_SECS).await?;
        let mut n = 0;
        let mut touched = std::collections::HashSet::new();
        for record in ready {
            touched.insert(record.project_id);
            let name = record.name.clone();
            let id = record.id;
            match run_fiber(&self.store, &self.registry, record, MAX_ATTEMPTS).await {
                Ok(outcome) => {
                    info!(%id, %name, ?outcome, "fiber ran");
                    n += 1;
                }
                Err(e) => warn!(%id, %name, error = %e, "fiber run error"),
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
