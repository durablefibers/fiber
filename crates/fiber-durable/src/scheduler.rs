use crate::engine::{fail_fiber, run_fiber};
use crate::registry::FiberRegistry;
use crate::store::FiberStore;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const POLL_SECS: u64 = 2;
const STALE_AFTER_SECS: i64 = 60;
const MAX_ATTEMPTS: i32 = 3;
/// Fibers claimed per sweep. They run concurrently, so a slow one cannot let the
/// others' claims go stale (and be re-claimed elsewhere) while they wait their turn.
const CLAIM_BATCH: i64 = 20;
/// Longest the in-memory due index may keep a replica from asking the database. The
/// index only knows about fibers *this* replica created or ran; one created on a replica
/// that then died is invisible to it, and without a ceiling it would wait until the next
/// local event — hours, on a quiet install — before anyone claimed it.
const MAX_SKIP: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct FiberScheduler {
    store: FiberStore,
    registry: FiberRegistry,
    /// When the ready query last ran, whatever the index said.
    last_query: Arc<Mutex<Option<Instant>>>,
}

impl FiberScheduler {
    pub fn new(store: FiberStore, registry: FiberRegistry) -> Self {
        Self {
            store,
            registry,
            last_query: Arc::new(Mutex::new(None)),
        }
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

        // The database is the source of truth; the index only lets an idle replica skip
        // the query — for at most MAX_SKIP, because it cannot see what other replicas did.
        let since_last = self
            .last_query
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .map(|t| t.elapsed());
        let index_says_idle =
            due_index.earliest().is_some_and(|e| e > now) && due_index.due_keys(now).is_empty();
        if !sweep_is_due(index_says_idle, since_last) {
            debug!("no fibers due yet");
            return Ok(0);
        }
        *self.last_query.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());

        let ready = self
            .store
            .claim_ready(STALE_AFTER_SECS, CLAIM_BATCH)
            .await?;
        let mut n = 0;
        let mut touched = std::collections::HashSet::new();
        let mut tasks = tokio::task::JoinSet::new();
        // Which fiber each task is, for the case where the task itself is all that comes
        // back: a panic yields a `JoinError` with no payload.
        let mut by_task: HashMap<tokio::task::Id, (uuid::Uuid, String)> = HashMap::new();
        for record in ready {
            touched.insert(record.project_id);
            let store = self.store.clone();
            let registry = self.registry.clone();
            let (id, name) = (record.id, record.name.clone());
            let handle = tasks.spawn(async move {
                let name = record.name.clone();
                let id = record.id;
                let result = run_fiber(&store, &registry, record, MAX_ATTEMPTS).await;
                (id, name, result)
            });
            by_task.insert(handle.id(), (id, name));
        }
        while let Some(joined) = tasks.join_next_with_id().await {
            match joined {
                Ok((_, (id, name, Ok(outcome)))) => {
                    info!(%id, %name, ?outcome, "fiber ran");
                    n += 1;
                }
                Ok((_, (id, name, Err(e)))) => warn!(%id, %name, error = %e, "fiber run error"),
                Err(e) => {
                    // The heartbeat died with the task (it is aborted on drop), so the
                    // row would be reclaimed as stale in a minute — and run again, and
                    // panic again, with `attempts` never spent because a stale reclaim
                    // does not count. Charge it now, exactly as a returned error is.
                    let Some((id, name)) = by_task.get(&e.id()) else {
                        warn!(error = %e, "fiber task panicked");
                        continue;
                    };
                    warn!(%id, %name, error = %e, "fiber task panicked");
                    self.fail_panicked(*id, &e.to_string()).await;
                }
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

    /// Spend an attempt on a fiber whose handler panicked. Re-read first: the record
    /// the poller claimed predates every `stash` the handler checkpointed before it
    /// died, and saving it back would erase them.
    async fn fail_panicked(&self, id: uuid::Uuid, error: &str) {
        let record = match self.store.get(id).await {
            Ok(Some(r)) => r,
            Ok(None) => return,
            Err(e) => {
                warn!(%id, error = %e, "could not load panicked fiber to fail it");
                return;
            }
        };
        if let Err(e) = fail_fiber(
            &self.store,
            record,
            format!("fiber task panicked: {error}"),
            MAX_ATTEMPTS,
        )
        .await
        {
            warn!(%id, error = %e, "could not record panicked fiber's failure");
        }
    }
}

/// Whether this tick asks the database for ready fibers.
///
/// Always when the index has something due or knows nothing; when it says everything is
/// in the future, still at least every `MAX_SKIP`, because the index is per replica and a
/// fiber created elsewhere is not in it. Pure so the ceiling can be pinned without a store.
pub(crate) fn sweep_is_due(index_says_idle: bool, since_last_query: Option<Duration>) -> bool {
    if !index_says_idle {
        return true;
    }
    match since_last_query {
        None => true,
        Some(elapsed) => elapsed >= MAX_SKIP,
    }
}
