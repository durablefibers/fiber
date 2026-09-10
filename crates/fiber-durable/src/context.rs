use crate::store::FiberStore;
use crate::types::{FiberRecord, FiberState};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::future::Future;
use thiserror::Error;

/// Well inside the poller's staleness threshold, so a tick can be missed without the fiber
/// looking dead.
const HEARTBEAT_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Error)]
#[error("fiber suspended until {wake_at}")]
pub struct FiberSuspended {
    pub wake_at: DateTime<Utc>,
}

/// Durability primitives for a single fiber run.
pub struct FiberContext {
    pub record: FiberRecord,
    pub input: Value,
    store: FiberStore,
    state: FiberState,
    sleep_i: i32,
}

impl FiberContext {
    pub fn new(record: FiberRecord, store: FiberStore) -> Self {
        let input = record.input.clone();
        let state = record.state.clone();
        Self {
            record,
            input,
            store,
            state,
            sleep_i: 0,
        }
    }

    pub fn state(&self) -> &FiberState {
        &self.state
    }

    /// Run `f` once, memoizing under `key`. Skipped on resume when already done.
    pub async fn step<F, Fut>(&mut self, key: &str, f: F) -> anyhow::Result<Value>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<Value>>,
    {
        if let Some(existing) = self.state.steps.get(key) {
            return Ok(existing.clone());
        }
        // A step is a single opaque await. Nothing inside it touches the heartbeat, so a
        // step lasting longer than the staleness threshold used to look like a crashed
        // fiber and get claimed and run a second time by another sweep. An `http_request`
        // with a 300-second timeout reaches that in one call.
        let beat = tokio::spawn({
            let store = self.store.clone();
            let id = self.record.id;
            async move {
                loop {
                    tokio::time::sleep(HEARTBEAT_EVERY).await;
                    if let Err(e) = store.touch_heartbeat(id).await {
                        tracing::warn!(fiber_id = %id, error = %e, "fiber heartbeat failed");
                    }
                }
            }
        });
        let result = f().await;
        beat.abort();
        let result = result?;
        self.state.steps.insert(key.to_string(), result.clone());
        self.record.heartbeat_at = Some(Utc::now());
        self.store
            .append_step(self.record.id, key, &result, self.record.heartbeat_at)
            .await?;
        Ok(result)
    }

    /// Persist an arbitrary checkpoint value.
    pub async fn stash(&mut self, key: &str, value: Value) -> anyhow::Result<()> {
        self.state.data.insert(key.to_string(), value);
        self.checkpoint().await
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.state.data.get(key)
    }

    /// Durably sleep until `wake_at`. Returns immediately if already elapsed on resume.
    pub async fn sleep_until(&mut self, wake_at: DateTime<Utc>) -> Result<(), FiberSuspended> {
        self.sleep_i += 1;
        if self.sleep_i <= self.state.sleeps_done {
            return Ok(());
        }
        self.state.sleeps_done = self.sleep_i;
        if let Err(e) = self.checkpoint().await {
            tracing::error!(error = %e, "checkpoint before sleep failed");
        }
        Err(FiberSuspended { wake_at })
    }

    pub async fn sleep(&mut self, seconds: i64) -> Result<(), FiberSuspended> {
        self.sleep_until(Utc::now() + chrono::Duration::seconds(seconds))
            .await
    }

    /// Create a follow-up fiber (cron-chain / interval_task). Prefer calling inside `step`
    /// so creation is memoized and not duplicated on resume.
    pub async fn spawn_fiber(
        &self,
        name: &str,
        input: Value,
        wake_at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<uuid::Uuid> {
        let record = self
            .store
            .create(self.record.project_id, name, input, wake_at)
            .await?;
        Ok(record.id)
    }

    pub fn store(&self) -> &FiberStore {
        &self.store
    }

    async fn checkpoint(&mut self) -> anyhow::Result<()> {
        self.record.state = FiberState {
            steps: std::collections::HashMap::new(),
            data: self.state.data.clone(),
            sleeps_done: self.state.sleeps_done,
        };
        self.store.save_checkpoint(&self.record).await
    }

    pub(crate) fn take_state(&mut self) -> FiberState {
        std::mem::take(&mut self.state)
    }
}
