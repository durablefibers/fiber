//! The narrow slice of the store that [`crate::context::FiberContext`] depends on.
//!
//! The context needs four operations, not the whole of [`FiberStore`]. Naming them as a
//! trait lets the durability primitives — step memoization, checkpointing, the sleep
//! ordinal — be exercised against an in-memory double, which is the only way to assert
//! that a resumed fiber does *not* re-run a completed step. `FiberStore` is the
//! production implementation and the only one outside tests.

use crate::store::FiberStore;
use crate::types::FiberRecord;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

#[async_trait]
pub trait FiberPersistence: Send + Sync + 'static {
    /// Persist `record`'s state blob mid-run (stash, and before suspending to sleep).
    async fn save_checkpoint(&self, record: &FiberRecord) -> Result<()>;

    /// Record one completed step's memoized result, upserting by `(fiber_id, key)`.
    async fn append_step(
        &self,
        fiber_id: Uuid,
        key: &str,
        value: &Value,
        heartbeat_at: Option<DateTime<Utc>>,
    ) -> Result<()>;

    /// Keep a long-running step from looking like a crashed fiber to the reclaim sweep.
    async fn touch_heartbeat(&self, id: Uuid) -> Result<()>;

    /// Create a follow-up fiber (the self-rescheduling chain in `interval_task`).
    async fn create(
        &self,
        project_id: Uuid,
        name: &str,
        input: Value,
        wake_at: Option<DateTime<Utc>>,
    ) -> Result<FiberRecord>;

    /// How many *other* unfinished fibers of this task exist in the project. A
    /// self-rescheduling chain asks before extending itself, which is the only thing
    /// standing between a project writer and an unbounded number of perpetual fibers.
    async fn count_live_siblings(&self, project_id: Uuid, name: &str, exclude: Uuid)
    -> Result<i64>;
}

#[async_trait]
impl FiberPersistence for FiberStore {
    async fn save_checkpoint(&self, record: &FiberRecord) -> Result<()> {
        // Spelled out rather than `self.save_checkpoint(..)`: the inherent method wins
        // that lookup today, but a future signature change would turn it into recursion.
        FiberStore::save_checkpoint(self, record).await
    }

    async fn append_step(
        &self,
        fiber_id: Uuid,
        key: &str,
        value: &Value,
        heartbeat_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        FiberStore::append_step(self, fiber_id, key, value, heartbeat_at).await
    }

    async fn touch_heartbeat(&self, id: Uuid) -> Result<()> {
        FiberStore::touch_heartbeat(self, id).await
    }

    async fn create(
        &self,
        project_id: Uuid,
        name: &str,
        input: Value,
        wake_at: Option<DateTime<Utc>>,
    ) -> Result<FiberRecord> {
        FiberStore::create(self, project_id, name, input, wake_at).await
    }

    async fn count_live_siblings(
        &self,
        project_id: Uuid,
        name: &str,
        exclude: Uuid,
    ) -> Result<i64> {
        FiberStore::count_live_siblings(self, project_id, name, exclude).await
    }
}
