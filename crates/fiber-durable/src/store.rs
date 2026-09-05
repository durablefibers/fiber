use crate::durability::Durability;
use crate::types::{FiberRecord, FiberState, FiberStatus};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use fiber_core::DueIndex;
use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub struct FiberStore {
    pool: PgPool,
    due: Arc<DueIndex<Uuid>>,
}

impl FiberStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            due: Arc::new(DueIndex::new()),
        }
    }

    pub fn due_index(&self) -> Arc<DueIndex<Uuid>> {
        Arc::clone(&self.due)
    }

    fn record_due(&self, record: &FiberRecord) {
        let due = match record.status {
            FiberStatus::Pending => Some(Utc::now()),
            FiberStatus::Suspended => Some(record.wake_at.unwrap_or_else(Utc::now)),
            FiberStatus::Running => Some(record.heartbeat_at.unwrap_or_else(Utc::now)),
            FiberStatus::Completed | FiberStatus::Failed => None,
        };
        match due {
            Some(d) => self.due.record(record.project_id, d),
            None => {
                // leave index; authoritative refresh after sweep
            }
        }
    }

    pub async fn create(
        &self,
        project_id: Uuid,
        name: &str,
        input: Value,
        wake_at: Option<DateTime<Utc>>,
    ) -> Result<FiberRecord> {
        let id = Uuid::new_v4();
        let now = Utc::now();
        let status = if wake_at.map(|w| w > now).unwrap_or(false) {
            FiberStatus::Suspended
        } else {
            FiberStatus::Pending
        };
        let state = FiberState::default();
        let state_json = serde_json::to_value(&FiberState {
            steps: Default::default(),
            data: state.data.clone(),
            sleeps_done: state.sleeps_done,
        })?;

        sqlx::query(
            "INSERT INTO fibers (id, project_id, name, status, input, state, wake_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(project_id)
        .bind(name)
        .bind(status.as_str())
        .bind(&input)
        .bind(&state_json)
        .bind(wake_at)
        .execute(&self.pool)
        .await?;

        let record = FiberRecord {
            id,
            project_id,
            name: name.to_string(),
            status,
            input,
            state,
            result: None,
            error: None,
            attempts: 0,
            wake_at,
            heartbeat_at: None,
            created_at: now,
            updated_at: now,
        };
        self.record_due(&record);
        Ok(record)
    }

    pub async fn get(&self, id: Uuid) -> Result<Option<FiberRecord>> {
        let row = sqlx::query_as::<_, FiberRow>(
            "SELECT id, project_id, name, status, input, state, result, error, attempts,
                    wake_at, heartbeat_at, created_at, updated_at
             FROM fibers WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some(r) => Ok(Some(self.hydrate(r).await?)),
            None => Ok(None),
        }
    }

    pub async fn list_by_project(&self, project_id: Uuid) -> Result<Vec<FiberRecord>> {
        let rows = sqlx::query_as::<_, FiberRow>(
            "SELECT id, project_id, name, status, input, state, result, error, attempts,
                    wake_at, heartbeat_at, created_at, updated_at
             FROM fibers WHERE project_id = $1 ORDER BY created_at DESC LIMIT 100",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(self.hydrate(r).await?);
        }
        Ok(out)
    }

    async fn hydrate(&self, row: FiberRow) -> Result<FiberRecord> {
        let mut state: FiberState = serde_json::from_value(row.state).unwrap_or_default();
        let steps =
            sqlx::query_as::<_, StepRow>("SELECT key, value FROM fiber_steps WHERE fiber_id = $1")
                .bind(row.id)
                .fetch_all(&self.pool)
                .await?;
        for s in steps {
            state.steps.insert(s.key, s.value);
        }
        Ok(FiberRecord {
            id: row.id,
            project_id: row.project_id,
            name: row.name,
            status: FiberStatus::parse(&row.status),
            input: row.input,
            state,
            result: row.result,
            error: row.error,
            attempts: row.attempts,
            wake_at: row.wake_at,
            heartbeat_at: row.heartbeat_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    pub async fn save(&self, record: &FiberRecord) -> Result<()> {
        let state_json = serde_json::to_value(&FiberState {
            steps: Default::default(),
            data: record.state.data.clone(),
            sleeps_done: record.state.sleeps_done,
        })?;
        sqlx::query(
            "UPDATE fibers SET status = $2, state = $3, result = $4, error = $5,
                 attempts = $6, wake_at = $7, heartbeat_at = $8, updated_at = NOW()
             WHERE id = $1",
        )
        .bind(record.id)
        .bind(record.status.as_str())
        .bind(&state_json)
        .bind(&record.result)
        .bind(&record.error)
        .bind(record.attempts)
        .bind(record.wake_at)
        .bind(record.heartbeat_at)
        .execute(&self.pool)
        .await?;
        self.record_due(record);
        Ok(())
    }

    pub async fn save_checkpoint(&self, record: &FiberRecord) -> Result<()> {
        self.save(record).await
    }

    pub async fn append_step(
        &self,
        fiber_id: Uuid,
        key: &str,
        value: &Value,
        heartbeat_at: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO fiber_steps (fiber_id, key, value)
             VALUES ($1, $2, $3)
             ON CONFLICT (fiber_id, key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(fiber_id)
        .bind(key)
        .bind(value)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE fibers SET heartbeat_at = $2, updated_at = NOW() WHERE id = $1")
            .bind(fiber_id)
            .bind(heartbeat_at)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Ready: pending, suspended with wake_at <= now, or running with stale heartbeat.
    pub async fn list_ready(
        &self,
        now: DateTime<Utc>,
        stale_after_secs: i64,
    ) -> Result<Vec<FiberRecord>> {
        let stale_before = now - chrono::Duration::seconds(stale_after_secs);
        let rows = sqlx::query_as::<_, FiberRow>(
            "SELECT id, project_id, name, status, input, state, result, error, attempts,
                    wake_at, heartbeat_at, created_at, updated_at
             FROM fibers
             WHERE status = 'pending'
                OR (status = 'suspended' AND (wake_at IS NULL OR wake_at <= $1))
                OR (status = 'running' AND (heartbeat_at IS NULL OR heartbeat_at < $2))
             ORDER BY created_at ASC
             LIMIT 50",
        )
        .bind(now)
        .bind(stale_before)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            out.push(self.hydrate(r).await?);
        }
        Ok(out)
    }

    pub async fn cancel(&self, id: Uuid) -> Result<Option<FiberRecord>> {
        let Some(mut record) = self.get(id).await? else {
            return Ok(None);
        };
        if record.status.terminal() {
            return Ok(Some(record));
        }
        record.status = FiberStatus::Failed;
        record.error = Some("cancelled".into());
        record.wake_at = None;
        record.heartbeat_at = None;
        self.save(&record).await?;
        Ok(Some(record))
    }

    /// Seed due-index from all non-terminal fibers (project_id → earliest due).
    pub async fn seed_due_index(&self, stale_after_secs: i64) -> Result<()> {
        self.due.clear();
        let now = Utc::now();
        let rows =
            sqlx::query_as::<_, (Uuid, String, Option<DateTime<Utc>>, Option<DateTime<Utc>>)>(
                "SELECT project_id, status, wake_at, heartbeat_at FROM fibers
             WHERE status IN ('pending', 'suspended', 'running')",
            )
            .fetch_all(&self.pool)
            .await?;
        for (project_id, status, wake_at, heartbeat_at) in rows {
            let due = match status.as_str() {
                "pending" => now,
                "suspended" => wake_at.unwrap_or(now),
                "running" => heartbeat_at
                    .map(|h| h + chrono::Duration::seconds(stale_after_secs))
                    .unwrap_or(now),
                _ => continue,
            };
            self.due.record(project_id, due);
        }
        Ok(())
    }

    /// After processing a project, refresh authoritative next due (or clear).
    pub async fn refresh_project_due(
        &self,
        project_id: Uuid,
        now: DateTime<Utc>,
        stale_after_secs: i64,
    ) -> Result<()> {
        let row = sqlx::query_as::<_, (Option<DateTime<Utc>>,)>(
            "SELECT MIN(d) FROM (
                SELECT $2::timestamptz AS d FROM fibers
                  WHERE project_id = $1 AND status = 'pending'
                UNION ALL
                SELECT COALESCE(wake_at, $2) FROM fibers
                  WHERE project_id = $1 AND status = 'suspended'
                UNION ALL
                SELECT COALESCE(heartbeat_at + make_interval(secs => $3::int), $2) FROM fibers
                  WHERE project_id = $1 AND status = 'running'
             ) t",
        )
        .bind(project_id)
        .bind(now)
        .bind(stale_after_secs as i32)
        .fetch_one(&self.pool)
        .await?;
        self.due.set(project_id, row.0);
        Ok(())
    }
}

#[async_trait]
impl Durability for FiberStore {
    async fn create(
        &self,
        project_id: Uuid,
        name: &str,
        input: Value,
        wake_at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<FiberRecord> {
        FiberStore::create(self, project_id, name, input, wake_at).await
    }

    async fn get(&self, id: Uuid) -> anyhow::Result<Option<FiberRecord>> {
        FiberStore::get(self, id).await
    }

    async fn save(&self, record: &FiberRecord) -> anyhow::Result<()> {
        FiberStore::save(self, record).await
    }

    async fn list_ready(
        &self,
        now: DateTime<Utc>,
        stale_after_secs: i64,
    ) -> anyhow::Result<Vec<FiberRecord>> {
        FiberStore::list_ready(self, now, stale_after_secs).await
    }
}

#[derive(Debug, sqlx::FromRow)]
struct FiberRow {
    id: Uuid,
    project_id: Uuid,
    name: String,
    status: String,
    input: Value,
    state: Value,
    result: Option<Value>,
    error: Option<String>,
    attempts: i32,
    wake_at: Option<DateTime<Utc>>,
    heartbeat_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
struct StepRow {
    key: String,
    value: Value,
}
