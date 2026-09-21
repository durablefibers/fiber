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
            FiberStatus::Completed | FiberStatus::Failed | FiberStatus::Cancelled => None,
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

    /// Unfinished fibers of one task in one project, not counting `exclude`.
    ///
    /// The caller is usually a fiber asking about itself, and counting itself would make
    /// a cap of one stop the only chain there is.
    pub async fn count_live_siblings(
        &self,
        project_id: Uuid,
        name: &str,
        exclude: Uuid,
    ) -> Result<i64> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM fibers
             WHERE project_id = $1 AND name = $2 AND id <> $3
               AND status IN ('pending', 'running', 'suspended')",
        )
        .bind(project_id)
        .bind(name)
        .bind(exclude)
        .fetch_one(&self.pool)
        .await?)
    }

    /// The project's fibers for the list view, **without** their memoized step results.
    ///
    /// The Fibers page polls this every two seconds and shows status, timing and the
    /// result — never `state.steps`. Hydrating each row cost one `fiber_steps` query per
    /// fiber (101 per poll at the limit), so the steps are left empty here and filled in
    /// by [`Self::get`], which is what the detail view calls.
    pub async fn list_by_project(&self, project_id: Uuid) -> Result<Vec<FiberRecord>> {
        let rows = sqlx::query_as::<_, FiberRow>(
            "SELECT id, project_id, name, status, input, state, result, error, attempts,
                    wake_at, heartbeat_at, created_at, updated_at
             FROM fibers WHERE project_id = $1 ORDER BY created_at DESC LIMIT 100",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(record_from_row).collect())
    }

    async fn hydrate(&self, row: FiberRow) -> Result<FiberRecord> {
        let steps =
            sqlx::query_as::<_, StepRow>("SELECT key, value FROM fiber_steps WHERE fiber_id = $1")
                .bind(row.id)
                .fetch_all(&self.pool)
                .await?;
        let mut record = record_from_row(row);
        for s in steps {
            record.state.steps.insert(s.key, s.value);
        }
        Ok(record)
    }

    /// Hydrate many rows with one `fiber_steps` query instead of one per fiber.
    async fn hydrate_all(&self, rows: Vec<FiberRow>) -> Result<Vec<FiberRecord>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
        let steps = sqlx::query_as::<_, (Uuid, String, serde_json::Value)>(
            "SELECT fiber_id, key, value FROM fiber_steps WHERE fiber_id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(&self.pool)
        .await?;
        let mut out: Vec<FiberRecord> = rows.into_iter().map(record_from_row).collect();
        attach_steps(&mut out, steps);
        Ok(out)
    }
}

/// File each `(fiber_id, key, value)` under its own fiber.
///
/// The batched read returns every claimed fiber's steps in one result set, so the rows
/// have to be matched back by id — putting one fiber's memoized results on another would
/// make the engine skip a step that never ran.
fn attach_steps(records: &mut [FiberRecord], steps: Vec<(Uuid, String, serde_json::Value)>) {
    let index: std::collections::HashMap<Uuid, usize> =
        records.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
    for (fiber_id, key, value) in steps {
        if let Some(i) = index.get(&fiber_id) {
            records[*i].state.steps.insert(key, value);
        }
    }
}

/// A row as a record, with no memoized steps attached. Callers that need them add them.
fn record_from_row(row: FiberRow) -> FiberRecord {
    let state: FiberState = serde_json::from_value(row.state).unwrap_or_default();
    FiberRecord {
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
    }
}

impl FiberStore {
    /// Persist a fiber, unless a person has cancelled it in the meantime.
    ///
    /// Returns whether the write applied. The engine holds a record from before the handler
    /// ran, so an unguarded save would resurrect a fiber someone stopped mid-flight and
    /// report it completed — the cancel would appear to do nothing.
    pub async fn save(&self, record: &FiberRecord) -> Result<bool> {
        let state_json = serde_json::to_value(&FiberState {
            steps: Default::default(),
            data: record.state.data.clone(),
            sleeps_done: record.state.sleeps_done,
        })?;
        let res = sqlx::query(
            "UPDATE fibers SET status = $2, state = $3, result = $4, error = $5,
                 attempts = $6, wake_at = $7, heartbeat_at = $8, updated_at = NOW()
             WHERE id = $1 AND status <> 'cancelled'",
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
        let applied = res.rows_affected() > 0;
        if applied {
            self.record_due(record);
        }
        Ok(applied)
    }

    pub async fn save_checkpoint(&self, record: &FiberRecord) -> Result<()> {
        self.save(record).await?;
        Ok(())
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

    /// Delete terminal fibers finished before `cutoff`, newest-first up to `limit`.
    ///
    /// Returns how many went. `fiber_steps` cascades from `fibers`, so the memoized step
    /// results go with them — those are the bulk, one row per step of every fiber ever run.
    ///
    /// Only terminal statuses: a suspended fiber sleeping for a month is not old, it is
    /// waiting, and deleting it would silently cancel work someone scheduled.
    pub async fn delete_terminal_before(&self, cutoff: DateTime<Utc>, limit: i64) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM fibers WHERE id IN (
                 SELECT id FROM fibers
                 WHERE status IN ('completed', 'failed', 'cancelled')
                   AND updated_at < $1
                 ORDER BY updated_at ASC
                 LIMIT $2)",
        )
        .bind(cutoff)
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Mark a running fiber alive.
    ///
    /// Guarded on `running` so it cannot revive a fiber someone cancelled, and so a late
    /// tick from a finished step does not stamp a terminal row.
    pub async fn touch_heartbeat(&self, id: Uuid) -> Result<()> {
        sqlx::query("UPDATE fibers SET heartbeat_at = NOW() WHERE id = $1 AND status = 'running'")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Ready: pending, suspended with wake_at <= now, or running with stale heartbeat.
    /// Atomically claim up to 50 ready fibers: pending, suspended-and-due, or running
    /// with a stale heartbeat. The claim flips them to `running`, bumps `attempts`, and
    /// stamps the heartbeat inside the same statement (`FOR UPDATE SKIP LOCKED`), so two
    /// API instances sweeping concurrently never execute the same fiber twice.
    /// Atomically claim up to `limit` ready fibers: pending, suspended-and-due, or
    /// running with a stale heartbeat. The claim flips them to `running`, bumps
    /// `attempts`, and stamps the heartbeat inside the same statement
    /// (`FOR UPDATE SKIP LOCKED`), so two API instances sweeping concurrently never
    /// execute the same fiber twice. All timestamps come from the database clock so
    /// replicas with skewed clocks do not see each other's fresh claims as stale.
    pub async fn claim_ready(&self, stale_after_secs: i64, limit: i64) -> Result<Vec<FiberRecord>> {
        let rows = sqlx::query_as::<_, FiberRow>(
            "UPDATE fibers
             SET status = 'running',
                 -- Only a stale `running` row counts: that is an execution that vanished
                 -- without reporting, so nobody else will count it. A `pending` first run
                 -- has not failed yet, and a `suspended` row waking from a durable sleep is
                 -- the same attempt continuing — counting those made a fiber that sleeps
                 -- three times exhaust its retries while working perfectly.
                 attempts = attempts + CASE WHEN status = 'running' THEN 1 ELSE 0 END,
                 heartbeat_at = NOW(),
                 wake_at = NULL, updated_at = NOW()
             WHERE id IN (
                 SELECT id FROM fibers
                 WHERE status = 'pending'
                    OR (status = 'suspended' AND (wake_at IS NULL OR wake_at <= NOW()))
                    OR (status = 'running'
                        AND (heartbeat_at IS NULL
                             OR heartbeat_at < NOW() - make_interval(secs => $1)))
                 ORDER BY created_at ASC
                 LIMIT $2
                 FOR UPDATE SKIP LOCKED)
             RETURNING id, project_id, name, status, input, state, result, error, attempts,
                       wake_at, heartbeat_at, created_at, updated_at",
        )
        .bind(stale_after_secs as f64)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        // One query for every claimed fiber's memoized steps, not one each: this runs on
        // the poller's tick, and the engine needs the steps to skip what is already done.
        self.hydrate_all(rows).await
    }

    pub async fn cancel(&self, id: Uuid) -> Result<Option<FiberRecord>> {
        let Some(mut record) = self.get(id).await? else {
            return Ok(None);
        };
        if record.status.terminal() {
            return Ok(Some(record));
        }
        record.status = FiberStatus::Cancelled;
        record.error = None;
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
        FiberStore::save(self, record).await?;
        Ok(())
    }

    async fn claim_ready(
        &self,
        stale_after_secs: i64,
        limit: i64,
    ) -> anyhow::Result<Vec<FiberRecord>> {
        FiberStore::claim_ready(self, stale_after_secs, limit).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(id: Uuid) -> FiberRecord {
        let now = Utc::now();
        FiberRecord {
            id,
            project_id: Uuid::new_v4(),
            name: "t".into(),
            status: FiberStatus::Running,
            input: json!({}),
            state: FiberState::default(),
            result: None,
            error: None,
            attempts: 0,
            wake_at: None,
            heartbeat_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn batched_steps_land_on_their_own_fiber() {
        // One query returns every claimed fiber's steps together. Filing one fiber's
        // memoized result under another would make the engine skip a step that never ran.
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut records = vec![rec(a), rec(b)];
        attach_steps(
            &mut records,
            vec![
                (a, "one".into(), json!(1)),
                (b, "two".into(), json!(2)),
                (a, "three".into(), json!(3)),
                // A fiber that is not in this batch must not panic or land anywhere.
                (Uuid::new_v4(), "stray".into(), json!(9)),
            ],
        );
        assert_eq!(records[0].state.steps.len(), 2);
        assert_eq!(records[0].state.steps["one"], json!(1));
        assert_eq!(records[0].state.steps["three"], json!(3));
        assert_eq!(records[1].state.steps.len(), 1);
        assert_eq!(records[1].state.steps["two"], json!(2));
    }

    #[test]
    fn a_batch_with_no_steps_leaves_every_record_alone() {
        let mut records = vec![rec(Uuid::new_v4())];
        attach_steps(&mut records, vec![]);
        assert!(records[0].state.steps.is_empty());
    }
}
