//! Extension point for alternate durability backends (Temporal, etc.).
//! The shipped implementation is Postgres [`crate::FiberStore`].

use crate::types::FiberRecord;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

/// Pluggable durability backend. v1 ships Postgres only; this trait documents the swap surface.
#[async_trait]
pub trait Durability: Send + Sync {
    async fn create(
        &self,
        project_id: Uuid,
        name: &str,
        input: Value,
        wake_at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<FiberRecord>;

    async fn get(&self, id: Uuid) -> anyhow::Result<Option<FiberRecord>>;

    async fn save(&self, record: &FiberRecord) -> anyhow::Result<()>;

    async fn list_ready(
        &self,
        now: DateTime<Utc>,
        stale_after_secs: i64,
    ) -> anyhow::Result<Vec<FiberRecord>>;
}
