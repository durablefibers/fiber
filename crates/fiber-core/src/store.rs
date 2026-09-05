use crate::dag::{compile_definition, parse_pipeline_yaml, CompiledDag};
use crate::models::*;
use crate::schedule::{has_schedule, initial_due_from_definition, next_due_from_triggers};
use crate::tokens::{generate_token, hash_token, slugify};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use fiber_proto::{PipelineDefinition, RunStatus, StepStatus};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const PIPELINE_COLS: &str = "id, project_id, name, definition, created_at, updated_at, last_scheduled_at, next_due_at";
const AGENT_COLS: &str =
    "id, project_id, name, labels, concurrency, token_hash, last_seen_at, online, created_at";

#[derive(Clone)]
pub struct Store {
    pub pool: PgPool,
}

impl Store {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, slug, created_at FROM projects ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn list_projects_for_user(&self, user_id: Uuid) -> Result<Vec<Project>> {
        Ok(sqlx::query_as::<_, Project>(
            "SELECT p.id, p.name, p.slug, p.created_at
             FROM projects p
             INNER JOIN project_members m ON m.project_id = p.id
             WHERE m.user_id = $1
             ORDER BY p.created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_project(
        &self,
        owner_id: Uuid,
        req: CreateProjectRequest,
    ) -> Result<Project> {
        let id = Uuid::new_v4();
        let slug = req.slug.unwrap_or_else(|| slugify(&req.name));
        let project = sqlx::query_as::<_, Project>(
            "INSERT INTO projects (id, name, slug) VALUES ($1, $2, $3)
             RETURNING id, name, slug, created_at",
        )
        .bind(id)
        .bind(&req.name)
        .bind(&slug)
        .fetch_one(&self.pool)
        .await?;
        self.add_project_member(project.id, owner_id, crate::roles::ProjectRole::Owner)
            .await?;
        Ok(project)
    }

    pub async fn add_project_member(
        &self,
        project_id: Uuid,
        user_id: Uuid,
        role: crate::roles::ProjectRole,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO project_members (project_id, user_id, role)
             VALUES ($1, $2, $3)
             ON CONFLICT (project_id, user_id) DO UPDATE SET role = EXCLUDED.role",
        )
        .bind(project_id)
        .bind(user_id)
        .bind(role.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn member_role(
        &self,
        project_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<crate::roles::ProjectRole>> {
        let role: Option<String> = sqlx::query_scalar(
            "SELECT role FROM project_members WHERE project_id = $1 AND user_id = $2",
        )
        .bind(project_id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(role.and_then(|r| crate::roles::ProjectRole::parse(&r)))
    }

    pub async fn require_role(
        &self,
        project_id: Uuid,
        user_id: Uuid,
        min: crate::roles::ProjectRole,
    ) -> Result<crate::roles::ProjectRole> {
        let Some(role) = self.member_role(project_id, user_id).await? else {
            return Err(anyhow!("forbidden"));
        };
        if !role.at_least(min) {
            return Err(anyhow!("forbidden"));
        }
        Ok(role)
    }

    pub async fn list_project_members(&self, project_id: Uuid) -> Result<Vec<ProjectMember>> {
        Ok(sqlx::query_as::<_, ProjectMember>(
            "SELECT m.project_id, m.user_id, m.role, u.username, m.created_at
             FROM project_members m
             JOIN users u ON u.id = m.user_id
             WHERE m.project_id = $1
             ORDER BY m.role DESC, u.username",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn remove_project_member(&self, project_id: Uuid, user_id: Uuid) -> Result<()> {
        let role = self.member_role(project_id, user_id).await?;
        if role == Some(crate::roles::ProjectRole::Owner) {
            let owners: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM project_members WHERE project_id = $1 AND role = 'owner'",
            )
            .bind(project_id)
            .fetch_one(&self.pool)
            .await?;
            if owners <= 1 {
                return Err(anyhow!("cannot remove the last owner"));
            }
        }
        sqlx::query("DELETE FROM project_members WHERE project_id = $1 AND user_id = $2")
            .bind(project_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn find_user_by_username(&self, username: &str) -> Result<Option<PublicUser>> {
        Ok(sqlx::query_as::<_, PublicUser>(
            "SELECT id, username FROM users WHERE username = $1",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn create_user(&self, username: &str, password: &str) -> Result<PublicUser> {
        let id = Uuid::new_v4();
        let hash = crate::tokens::hash_password(password);
        Ok(sqlx::query_as::<_, PublicUser>(
            "INSERT INTO users (id, username, password_hash) VALUES ($1, $2, $3)
             RETURNING id, username",
        )
        .bind(id)
        .bind(username)
        .bind(hash)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn project_id_for_pipeline(&self, pipeline_id: Uuid) -> Result<Option<Uuid>> {
        Ok(sqlx::query_scalar("SELECT project_id FROM pipelines WHERE id = $1")
            .bind(pipeline_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn project_id_for_run(&self, run_id: Uuid) -> Result<Option<Uuid>> {
        Ok(sqlx::query_scalar("SELECT project_id FROM runs WHERE id = $1")
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn project_id_for_step(&self, step_run_id: Uuid) -> Result<Option<Uuid>> {
        Ok(sqlx::query_scalar(
            "SELECT r.project_id FROM step_runs s JOIN runs r ON r.id = s.run_id WHERE s.id = $1",
        )
        .bind(step_run_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn project_id_for_artifact(&self, artifact_id: Uuid) -> Result<Option<Uuid>> {
        Ok(sqlx::query_scalar(
            "SELECT r.project_id FROM artifacts a JOIN runs r ON r.id = a.run_id WHERE a.id = $1",
        )
        .bind(artifact_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn project_id_for_fiber(&self, fiber_id: Uuid) -> Result<Option<Uuid>> {
        Ok(sqlx::query_scalar("SELECT project_id FROM fibers WHERE id = $1")
            .bind(fiber_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn get_project(&self, id: Uuid) -> Result<Option<Project>> {
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, slug, created_at FROM projects WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn get_project_by_slug(&self, slug: &str) -> Result<Option<Project>> {
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, slug, created_at FROM projects WHERE slug = $1",
        )
        .bind(slug)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn list_pipelines(&self, project_id: Uuid) -> Result<Vec<Pipeline>> {
        let q = format!(
            "SELECT {PIPELINE_COLS} FROM pipelines WHERE project_id = $1 ORDER BY updated_at DESC"
        );
        Ok(sqlx::query_as::<_, Pipeline>(&q)
            .bind(project_id)
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn create_pipeline(
        &self,
        project_id: Uuid,
        req: CreatePipelineRequest,
    ) -> Result<Pipeline> {
        let def = value_to_definition(&req.definition)?;
        compile_definition(&def).context("invalid pipeline definition")?;
        if let Some(on) = &def.on {
            crate::schedule::validate_triggers(on).map_err(|e| anyhow!(e))?;
        }
        let id = Uuid::new_v4();
        let next_due = initial_due_from_definition(&def);
        let q = format!(
            "INSERT INTO pipelines (id, project_id, name, definition, next_due_at)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {PIPELINE_COLS}"
        );
        let pipeline = sqlx::query_as::<_, Pipeline>(&q)
            .bind(id)
            .bind(project_id)
            .bind(&req.name)
            .bind(&req.definition)
            .bind(next_due)
            .fetch_one(&self.pool)
            .await?;
        Ok(pipeline)
    }

    pub async fn update_pipeline(
        &self,
        pipeline_id: Uuid,
        req: UpdatePipelineRequest,
    ) -> Result<Pipeline> {
        let def = value_to_definition(&req.definition)?;
        compile_definition(&def).context("invalid pipeline definition")?;
        if let Some(on) = &def.on {
            crate::schedule::validate_triggers(on).map_err(|e| anyhow!(e))?;
        }
        let name = if let Some(n) = req.name {
            n
        } else {
            sqlx::query_scalar::<_, String>("SELECT name FROM pipelines WHERE id = $1")
                .bind(pipeline_id)
                .fetch_one(&self.pool)
                .await?
        };
        let existing = self
            .get_pipeline(pipeline_id)
            .await?
            .ok_or_else(|| anyhow!("pipeline not found"))?;
        let next_due = match def.on.as_ref().filter(|o| has_schedule(o)) {
            None => None,
            Some(_) => existing
                .next_due_at
                .or_else(|| initial_due_from_definition(&def)),
        };
        let q = format!(
            "UPDATE pipelines SET name = $2, definition = $3, updated_at = NOW(), next_due_at = $4
             WHERE id = $1
             RETURNING {PIPELINE_COLS}"
        );
        let pipeline = sqlx::query_as::<_, Pipeline>(&q)
            .bind(pipeline_id)
            .bind(&name)
            .bind(&req.definition)
            .bind(next_due)
            .fetch_one(&self.pool)
            .await?;
        Ok(pipeline)
    }

    pub async fn get_pipeline(&self, id: Uuid) -> Result<Option<Pipeline>> {
        let q = format!("SELECT {PIPELINE_COLS} FROM pipelines WHERE id = $1");
        Ok(sqlx::query_as::<_, Pipeline>(&q)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn list_all_pipelines(&self) -> Result<Vec<Pipeline>> {
        let q = format!("SELECT {PIPELINE_COLS} FROM pipelines");
        Ok(sqlx::query_as::<_, Pipeline>(&q)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Pipelines whose interval schedule is due now.
    pub async fn list_due_pipelines(&self, now: DateTime<Utc>) -> Result<Vec<Pipeline>> {
        let q = format!(
            "SELECT {PIPELINE_COLS} FROM pipelines
             WHERE next_due_at IS NOT NULL AND next_due_at <= $1
             ORDER BY next_due_at ASC"
        );
        Ok(sqlx::query_as::<_, Pipeline>(&q)
            .bind(now)
            .fetch_all(&self.pool)
            .await?)
    }

    /// After a schedule fire: stamp `last_scheduled_at` and set the next wake.
    pub async fn mark_scheduled(
        &self,
        pipeline_id: Uuid,
        next_due: Option<DateTime<Utc>>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE pipelines
             SET last_scheduled_at = NOW(), next_due_at = $2
             WHERE id = $1",
        )
        .bind(pipeline_id)
        .bind(next_due)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Clear schedule wake time (no schedule configured).
    pub async fn clear_schedule_due(&self, pipeline_id: Uuid) -> Result<()> {
        sqlx::query("UPDATE pipelines SET next_due_at = NULL WHERE id = $1")
            .bind(pipeline_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Backfill `next_due_at` for pipelines that have a schedule but no due time yet.
    pub async fn backfill_schedule_dues(&self) -> Result<u64> {
        let pipelines = self.list_all_pipelines().await?;
        let mut n = 0u64;
        for p in pipelines {
            if p.next_due_at.is_some() {
                continue;
            }
            let Ok(def) = value_to_definition(&p.definition) else {
                continue;
            };
            let Some(on) = def.on.as_ref().filter(|o| has_schedule(o)) else {
                continue;
            };
            let next = match p.last_scheduled_at {
                Some(last) => next_due_from_triggers(on, last).unwrap_or_else(Utc::now),
                None => initial_due_from_definition(&def).unwrap_or_else(Utc::now),
            };
            sqlx::query("UPDATE pipelines SET next_due_at = $2 WHERE id = $1 AND next_due_at IS NULL")
                .bind(p.id)
                .bind(next)
                .execute(&self.pool)
                .await?;
            n += 1;
        }
        Ok(n)
    }

    pub async fn requeue_for_retry(&self, step_run_id: Uuid) -> Result<StepRun> {
        Ok(sqlx::query_as::<_, StepRun>(
            "UPDATE step_runs
             SET status = 'queued', agent_id = NULL, lease_expires_at = NULL,
                 error = NULL, exit_code = NULL, finished_at = NULL
             WHERE id = $1
             RETURNING id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                       retries, attempt, agent_id, lease_expires_at, exit_code, error,
                       started_at, finished_at",
        )
        .bind(step_run_id)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn start_run(
        &self,
        pipeline_id: Uuid,
        trigger: &str,
    ) -> Result<(Run, Vec<StepRun>, CompiledDag)> {
        let pipeline = self
            .get_pipeline(pipeline_id)
            .await?
            .ok_or_else(|| anyhow!("pipeline not found"))?;
        let def = value_to_definition(&pipeline.definition)?;
        let compiled = compile_definition(&def)?;
        let run_id = Uuid::new_v4();
        let snapshot = serde_json::to_value(&compiled)?;

        let run = sqlx::query_as::<_, Run>(
            "INSERT INTO runs (id, pipeline_id, project_id, status, trigger, definition_snapshot, started_at)
             VALUES ($1, $2, $3, $4, $5, $6, NOW())
             RETURNING id, pipeline_id, project_id, status, trigger, definition_snapshot,
                       created_at, started_at, finished_at",
        )
        .bind(run_id)
        .bind(pipeline_id)
        .bind(pipeline.project_id)
        .bind(status_str(RunStatus::Running))
        .bind(trigger)
        .bind(&snapshot)
        .fetch_one(&self.pool)
        .await?;

        let mut step_runs = Vec::new();
        for step in &compiled.steps {
            let sid = Uuid::new_v4();
            // Only evaluate `if:` for roots here. Dependent steps stay Pending until
            // unlock time — otherwise `success()` would skip them before needs run.
            let status = if step.needs.is_empty() {
                let if_ctx = crate::step_if::IfContext {
                    needs_succeeded: true,
                    env: step.env.clone(),
                };
                if !crate::step_if::eval_if(step.if_expr.as_deref(), &if_ctx) {
                    StepStatus::Skipped
                } else {
                    StepStatus::Queued
                }
            } else {
                StepStatus::Pending
            };
            let error = if status == StepStatus::Skipped {
                Some("if: condition false".to_string())
            } else {
                None
            };
            let sr = sqlx::query_as::<_, StepRun>(
                "INSERT INTO step_runs
                 (id, run_id, step_id, step_name, status, image, run_cmd, labels, needs, retries, error)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                 RETURNING id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                           retries, attempt, agent_id, lease_expires_at, exit_code, error,
                           started_at, finished_at",
            )
            .bind(sid)
            .bind(run_id)
            .bind(&step.id)
            .bind(&step.name)
            .bind(step_status_str(status))
            .bind(&step.image)
            .bind(&step.run)
            .bind(json!(step.labels))
            .bind(json!(step.needs))
            .bind(step.retries as i32)
            .bind(error)
            .fetch_one(&self.pool)
            .await?;
            step_runs.push(sr);
        }

        // If all roots skipped, propagate so dependents can resolve.
        let _ = self.propagate_after_step(run_id).await?;

        Ok((run, self.list_step_runs(run_id).await?, compiled))
    }

    pub async fn get_run(&self, id: Uuid) -> Result<Option<Run>> {
        Ok(sqlx::query_as::<_, Run>(
            "SELECT id, pipeline_id, project_id, status, trigger, definition_snapshot,
                    created_at, started_at, finished_at
             FROM runs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn list_runs(&self, project_id: Uuid, limit: i64) -> Result<Vec<Run>> {
        Ok(sqlx::query_as::<_, Run>(
            "SELECT id, pipeline_id, project_id, status, trigger, definition_snapshot,
                    created_at, started_at, finished_at
             FROM runs WHERE project_id = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(project_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn list_step_runs(&self, run_id: Uuid) -> Result<Vec<StepRun>> {
        Ok(sqlx::query_as::<_, StepRun>(
            "SELECT id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                    retries, attempt, agent_id, lease_expires_at, exit_code, error,
                    started_at, finished_at
             FROM step_runs WHERE run_id = $1 ORDER BY step_id",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn get_step_run(&self, id: Uuid) -> Result<Option<StepRun>> {
        Ok(sqlx::query_as::<_, StepRun>(
            "SELECT id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                    retries, attempt, agent_id, lease_expires_at, exit_code, error,
                    started_at, finished_at
             FROM step_runs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn list_queued_steps(&self) -> Result<Vec<StepRun>> {
        Ok(sqlx::query_as::<_, StepRun>(
            "SELECT id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                    retries, attempt, agent_id, lease_expires_at, exit_code, error,
                    started_at, finished_at
             FROM step_runs WHERE status = 'queued'
             ORDER BY started_at NULLS FIRST, id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn lease_step(
        &self,
        step_run_id: Uuid,
        agent_id: Uuid,
        lease_secs: i64,
    ) -> Result<Option<StepRun>> {
        let expires = Utc::now() + chrono::Duration::seconds(lease_secs);
        let sr = sqlx::query_as::<_, StepRun>(
            "UPDATE step_runs
             SET status = 'running', agent_id = $2, lease_expires_at = $3,
                 started_at = COALESCE(started_at, NOW()), attempt = attempt + 1
             WHERE id = $1 AND status = 'queued'
             RETURNING id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                       retries, attempt, agent_id, lease_expires_at, exit_code, error,
                       started_at, finished_at",
        )
        .bind(step_run_id)
        .bind(agent_id)
        .bind(expires)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(ref leased) = sr {
            self.insert_step_attempt(leased.id, leased.attempt, Some(agent_id), "running")
                .await?;
        }
        Ok(sr)
    }

    async fn insert_step_attempt(
        &self,
        step_run_id: Uuid,
        attempt: i32,
        agent_id: Option<Uuid>,
        status: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO step_attempts (id, step_run_id, attempt, agent_id, status)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(step_run_id)
        .bind(attempt)
        .bind(agent_id)
        .bind(status)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn finish_open_attempt(
        &self,
        step_run_id: Uuid,
        status: &str,
        exit_code: Option<i32>,
        error: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE step_attempts
             SET finished_at = NOW(), status = $2, exit_code = $3, error = $4
             WHERE id = (
                 SELECT id FROM step_attempts
                 WHERE step_run_id = $1 AND finished_at IS NULL
                 ORDER BY started_at DESC
                 LIMIT 1
             )",
        )
        .bind(step_run_id)
        .bind(status)
        .bind(exit_code)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_step_attempts(&self, step_run_id: Uuid) -> Result<Vec<StepAttempt>> {
        Ok(sqlx::query_as::<_, StepAttempt>(
            "SELECT id, step_run_id, attempt, agent_id, started_at, finished_at, status, exit_code, error
             FROM step_attempts WHERE step_run_id = $1
             ORDER BY attempt ASC, started_at ASC",
        )
        .bind(step_run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn complete_step(
        &self,
        step_run_id: Uuid,
        status: StepStatus,
        exit_code: Option<i32>,
        error: Option<String>,
    ) -> Result<StepRun> {
        let sr = sqlx::query_as::<_, StepRun>(
            "UPDATE step_runs
             SET status = $2, exit_code = $3, error = $4, finished_at = NOW(), lease_expires_at = NULL
             WHERE id = $1
             RETURNING id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                       retries, attempt, agent_id, lease_expires_at, exit_code, error,
                       started_at, finished_at",
        )
        .bind(step_run_id)
        .bind(step_status_str(status))
        .bind(exit_code)
        .bind(&error)
        .fetch_one(&self.pool)
        .await?;
        self.finish_open_attempt(
            step_run_id,
            step_status_str(status),
            exit_code,
            error.as_deref(),
        )
        .await?;
        Ok(sr)
    }

    /// Complete only if still running for this agent (ignores late completes after cancel/reclaim).
    pub async fn complete_running_step(
        &self,
        step_run_id: Uuid,
        agent_id: Uuid,
        status: StepStatus,
        exit_code: Option<i32>,
        error: Option<String>,
    ) -> Result<Option<StepRun>> {
        let sr = sqlx::query_as::<_, StepRun>(
            "UPDATE step_runs
             SET status = $2, exit_code = $3, error = $4, finished_at = NOW(), lease_expires_at = NULL
             WHERE id = $1 AND status = 'running' AND agent_id = $5
             RETURNING id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                       retries, attempt, agent_id, lease_expires_at, exit_code, error,
                       started_at, finished_at",
        )
        .bind(step_run_id)
        .bind(step_status_str(status))
        .bind(exit_code)
        .bind(&error)
        .bind(agent_id)
        .fetch_optional(&self.pool)
        .await?;
        if sr.is_some() {
            self.finish_open_attempt(
                step_run_id,
                step_status_str(status),
                exit_code,
                error.as_deref(),
            )
            .await?;
        }
        Ok(sr)
    }

    pub async fn renew_agent_leases(&self, agent_id: Uuid, lease_secs: i64) -> Result<u64> {
        let expires = Utc::now() + chrono::Duration::seconds(lease_secs);
        let res = sqlx::query(
            "UPDATE step_runs SET lease_expires_at = $2
             WHERE agent_id = $1 AND status = 'running'",
        )
        .bind(agent_id)
        .bind(expires)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    pub async fn requeue_expired_leases(&self) -> Result<Vec<StepRun>> {
        let requeued = sqlx::query_as::<_, StepRun>(
            "UPDATE step_runs SET status = 'queued', agent_id = NULL, lease_expires_at = NULL
             WHERE status = 'running' AND lease_expires_at IS NOT NULL AND lease_expires_at < NOW()
             RETURNING id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                       retries, attempt, agent_id, lease_expires_at, exit_code, error,
                       started_at, finished_at",
        )
        .fetch_all(&self.pool)
        .await?;
        for s in &requeued {
            self.finish_open_attempt(s.id, "reclaimed", None, Some("lease expired"))
                .await?;
        }
        Ok(requeued)
    }

    pub async fn requeue_agent_steps(&self, agent_id: Uuid) -> Result<Vec<StepRun>> {
        let requeued = sqlx::query_as::<_, StepRun>(
            "UPDATE step_runs SET status = 'queued', agent_id = NULL, lease_expires_at = NULL,
                 started_at = NULL, attempt = GREATEST(attempt - 1, 0)
             WHERE agent_id = $1 AND status = 'running'
             RETURNING id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                       retries, attempt, agent_id, lease_expires_at, exit_code, error,
                       started_at, finished_at",
        )
        .bind(agent_id)
        .fetch_all(&self.pool)
        .await?;
        for s in &requeued {
            self.finish_open_attempt(s.id, "reclaimed", None, Some("agent disconnected"))
                .await?;
        }
        Ok(requeued)
    }

    /// After a step completes, unlock dependents or skip cascade on failure.
    pub async fn propagate_after_step(&self, run_id: Uuid) -> Result<Vec<StepRun>> {
        let run = self
            .get_run(run_id)
            .await?
            .ok_or_else(|| anyhow!("run not found"))?;
        let snapshot_steps = run
            .definition_snapshot
            .get("steps")
            .cloned()
            .unwrap_or(json!([]));

        let steps = self.list_step_runs(run_id).await?;

        let failed: std::collections::HashSet<String> = steps
            .iter()
            .filter(|s| matches!(s.status_enum(), StepStatus::Failed | StepStatus::Cancelled))
            .map(|s| s.step_id.clone())
            .collect();

        let mut changed = Vec::new();

        // Skip cascade: any pending/queued step that depends on failed
        for s in &steps {
            if !matches!(s.status_enum(), StepStatus::Pending | StepStatus::Queued) {
                continue;
            }
            let needs = s.needs_vec();
            if needs.iter().any(|n| failed.contains(n)) {
                let updated = self
                    .complete_step(s.id, StepStatus::Skipped, None, Some("dependency failed".into()))
                    .await?;
                changed.push(updated);
            }
        }

        // Refresh after failure skips
        let steps = self.list_step_runs(run_id).await?;
        let succeeded: std::collections::HashSet<String> = steps
            .iter()
            .filter(|s| s.status_enum() == StepStatus::Succeeded)
            .map(|s| s.step_id.clone())
            .collect();
        let terminal: std::collections::HashSet<String> = steps
            .iter()
            .filter(|s| {
                matches!(
                    s.status_enum(),
                    StepStatus::Succeeded
                        | StepStatus::Failed
                        | StepStatus::Cancelled
                        | StepStatus::Skipped
                )
            })
            .map(|s| s.step_id.clone())
            .collect();

        for s in &steps {
            if s.status_enum() != StepStatus::Pending {
                continue;
            }
            let needs = s.needs_vec();
            if !needs.iter().all(|n| terminal.contains(n)) {
                continue;
            }

            let (if_expr, env) = snapshot_step_if_env(&snapshot_steps, &s.step_id);
            let always = if_expr.as_deref().map(str::trim) == Some("always()");
            // success()/default: all needs succeeded. always(): run even if deps failed/skipped.
            let needs_ok = if always {
                true
            } else {
                needs.iter().all(|n| succeeded.contains(n))
            };

            if !needs_ok {
                let updated = self
                    .complete_step(
                        s.id,
                        StepStatus::Skipped,
                        None,
                        Some("dependency skipped or failed".into()),
                    )
                    .await?;
                changed.push(updated);
                continue;
            }

            let ctx = crate::step_if::IfContext {
                needs_succeeded: needs.iter().all(|n| succeeded.contains(n)),
                env,
            };
            if !crate::step_if::eval_if(if_expr.as_deref(), &ctx) {
                let updated = self
                    .complete_step(
                        s.id,
                        StepStatus::Skipped,
                        None,
                        Some("if: condition false".into()),
                    )
                    .await?;
                changed.push(updated);
                continue;
            }

            let updated = sqlx::query_as::<_, StepRun>(
                "UPDATE step_runs SET status = 'queued' WHERE id = $1 AND status = 'pending'
                 RETURNING id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                           retries, attempt, agent_id, lease_expires_at, exit_code, error,
                           started_at, finished_at",
            )
            .bind(s.id)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(u) = updated {
                changed.push(u);
            }
        }

        // Finalize run status
        let steps = self.list_step_runs(run_id).await?;
        let all_terminal = steps.iter().all(|s| {
            matches!(
                s.status_enum(),
                StepStatus::Succeeded
                    | StepStatus::Failed
                    | StepStatus::Cancelled
                    | StepStatus::Skipped
            )
        });
        if all_terminal {
            let any_failed = steps
                .iter()
                .any(|s| matches!(s.status_enum(), StepStatus::Failed | StepStatus::Cancelled));
            let status = if any_failed {
                RunStatus::Failed
            } else {
                RunStatus::Succeeded
            };
            sqlx::query(
                "UPDATE runs SET status = $2, finished_at = NOW() WHERE id = $1 AND status = 'running'",
            )
            .bind(run_id)
            .bind(status_str(status))
            .execute(&self.pool)
            .await?;
        }

        Ok(changed)
    }

    /// Cancel a run. Returns the run and any steps that were `running` (for agent Cancel fan-out).
    pub async fn cancel_run(&self, run_id: Uuid) -> Result<(Run, Vec<StepRun>)> {
        let running = sqlx::query_as::<_, StepRun>(
            "SELECT id, run_id, step_id, step_name, status, image, run_cmd, labels, needs,
                    retries, attempt, agent_id, lease_expires_at, exit_code, error,
                    started_at, finished_at
             FROM step_runs WHERE run_id = $1 AND status = 'running'",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;

        sqlx::query(
            "UPDATE step_runs SET status = 'cancelled', finished_at = NOW(), lease_expires_at = NULL
             WHERE run_id = $1 AND status IN ('pending', 'queued', 'running')",
        )
        .bind(run_id)
        .execute(&self.pool)
        .await?;
        for s in &running {
            self.finish_open_attempt(s.id, "cancelled", None, Some("run cancelled"))
                .await?;
        }
        let run = sqlx::query_as::<_, Run>(
            "UPDATE runs SET status = 'cancelled', finished_at = NOW() WHERE id = $1
             RETURNING id, pipeline_id, project_id, status, trigger, definition_snapshot,
                       created_at, started_at, finished_at",
        )
        .bind(run_id)
        .fetch_one(&self.pool)
        .await?;
        Ok((run, running))
    }

    pub async fn append_log(
        &self,
        run_id: Uuid,
        step_run_id: Uuid,
        stream: &str,
        data: &str,
        seq: u64,
    ) -> Result<LogLine> {
        Ok(sqlx::query_as::<_, LogLine>(
            "INSERT INTO log_lines (run_id, step_run_id, stream, data, seq)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id, run_id, step_run_id, stream, data, seq, created_at",
        )
        .bind(run_id)
        .bind(step_run_id)
        .bind(stream)
        .bind(data)
        .bind(seq as i64)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn list_logs(&self, step_run_id: Uuid) -> Result<Vec<LogLine>> {
        Ok(sqlx::query_as::<_, LogLine>(
            "SELECT id, run_id, step_run_id, stream, data, seq, created_at
             FROM log_lines WHERE step_run_id = $1 ORDER BY seq",
        )
        .bind(step_run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn create_artifact(
        &self,
        run_id: Uuid,
        step_run_id: Uuid,
        name: &str,
        path: &str,
        size: i64,
    ) -> Result<Artifact> {
        let id = Uuid::new_v4();
        Ok(sqlx::query_as::<_, Artifact>(
            "INSERT INTO artifacts (id, run_id, step_run_id, name, path, size)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING id, run_id, step_run_id, name, path, size, created_at",
        )
        .bind(id)
        .bind(run_id)
        .bind(step_run_id)
        .bind(name)
        .bind(path)
        .bind(size)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn list_artifacts(&self, run_id: Uuid) -> Result<Vec<Artifact>> {
        Ok(sqlx::query_as::<_, Artifact>(
            "SELECT id, run_id, step_run_id, name, path, size, created_at
             FROM artifacts WHERE run_id = $1 ORDER BY created_at",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn get_artifact(&self, id: Uuid) -> Result<Option<Artifact>> {
        Ok(sqlx::query_as::<_, Artifact>(
            "SELECT id, run_id, step_run_id, name, path, size, created_at
             FROM artifacts WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Terminal runs older than `cutoff`, optionally keeping the newest `keep_per_pipeline`
    /// per pipeline. Returns run ids and artifact storage paths to delete.
    pub async fn list_runs_for_retention(
        &self,
        cutoff: DateTime<Utc>,
        keep_per_pipeline: i64,
        limit: i64,
    ) -> Result<Vec<(Uuid, Option<String>)>> {
        // Rows: (run_id, artifact_path). Multiple rows per run when many artifacts.
        let rows: Vec<(Uuid, Option<String>)> = if keep_per_pipeline > 0 {
            sqlx::query_as(
                r#"
                WITH ranked AS (
                    SELECT id, pipeline_id,
                           ROW_NUMBER() OVER (
                               PARTITION BY pipeline_id
                               ORDER BY COALESCE(finished_at, created_at) DESC
                           ) AS rn,
                           COALESCE(finished_at, created_at) AS age_at
                    FROM runs
                    WHERE status IN ('succeeded', 'failed', 'cancelled')
                ),
                doomed AS (
                    SELECT id FROM ranked
                    WHERE rn > $1 AND age_at < $2
                    ORDER BY age_at ASC
                    LIMIT $3
                )
                SELECT d.id, a.path
                FROM doomed d
                LEFT JOIN artifacts a ON a.run_id = d.id
                "#,
            )
            .bind(keep_per_pipeline)
            .bind(cutoff)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as(
                r#"
                WITH doomed AS (
                    SELECT id
                    FROM runs
                    WHERE status IN ('succeeded', 'failed', 'cancelled')
                      AND COALESCE(finished_at, created_at) < $1
                    ORDER BY COALESCE(finished_at, created_at) ASC
                    LIMIT $2
                )
                SELECT d.id, a.path
                FROM doomed d
                LEFT JOIN artifacts a ON a.run_id = d.id
                "#,
            )
            .bind(cutoff)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows)
    }

    pub async fn delete_runs(&self, run_ids: &[Uuid]) -> Result<u64> {
        if run_ids.is_empty() {
            return Ok(0);
        }
        let res = sqlx::query("DELETE FROM runs WHERE id = ANY($1)")
            .bind(run_ids)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    pub async fn purge_expired_sessions(&self) -> Result<u64> {
        let res = sqlx::query("DELETE FROM sessions WHERE expires_at < NOW()")
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }

    pub async fn create_agent(&self, req: CreateAgentRequest) -> Result<CreateAgentResponse> {
        if let Some(pid) = req.project_id {
            let exists: Option<(Uuid,)> =
                sqlx::query_as("SELECT id FROM projects WHERE id = $1")
                    .bind(pid)
                    .fetch_optional(&self.pool)
                    .await?;
            if exists.is_none() {
                return Err(anyhow!("project not found"));
            }
        }
        let id = Uuid::new_v4();
        let token = generate_token();
        let token_hash = hash_token(&token);
        let concurrency = req.concurrency.unwrap_or(1) as i32;
        let agent = sqlx::query_as::<_, Agent>(&format!(
            "INSERT INTO agents (id, project_id, name, labels, concurrency, token_hash)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {AGENT_COLS}"
        ))
        .bind(id)
        .bind(req.project_id)
        .bind(&req.name)
        .bind(json!(req.labels))
        .bind(concurrency)
        .bind(&token_hash)
        .fetch_one(&self.pool)
        .await?;
        Ok(CreateAgentResponse { agent, token })
    }

    /// List agents. When `project_filter` is set, returns that project's agents plus globals.
    pub async fn list_agents(&self, project_filter: Option<Uuid>) -> Result<Vec<Agent>> {
        if let Some(pid) = project_filter {
            Ok(sqlx::query_as::<_, Agent>(&format!(
                "SELECT {AGENT_COLS} FROM agents
                 WHERE project_id IS NULL OR project_id = $1
                 ORDER BY project_id NULLS LAST, created_at DESC"
            ))
            .bind(pid)
            .fetch_all(&self.pool)
            .await?)
        } else {
            Ok(sqlx::query_as::<_, Agent>(&format!(
                "SELECT {AGENT_COLS} FROM agents ORDER BY created_at DESC"
            ))
            .fetch_all(&self.pool)
            .await?)
        }
    }

    pub async fn get_agent(&self, id: Uuid) -> Result<Option<Agent>> {
        Ok(sqlx::query_as::<_, Agent>(&format!(
            "SELECT {AGENT_COLS} FROM agents WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn update_agent(&self, id: Uuid, req: UpdateAgentRequest) -> Result<Agent> {
        let Some(existing) = self.get_agent(id).await? else {
            return Err(anyhow!("agent not found"));
        };
        let name = req.name.unwrap_or(existing.name);
        let labels = req.labels.unwrap_or_else(|| {
            existing
                .labels
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        });
        let concurrency = req
            .concurrency
            .map(|c| c.max(1) as i32)
            .unwrap_or(existing.concurrency);
        Ok(sqlx::query_as::<_, Agent>(&format!(
            "UPDATE agents SET name = $2, labels = $3, concurrency = $4
             WHERE id = $1
             RETURNING {AGENT_COLS}"
        ))
        .bind(id)
        .bind(&name)
        .bind(json!(labels))
        .bind(concurrency)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn delete_agent(&self, id: Uuid) -> Result<bool> {
        let res = sqlx::query("DELETE FROM agents WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Issue a new token; invalidates the previous one. Returns plaintext once.
    pub async fn rotate_agent_token(&self, id: Uuid) -> Result<CreateAgentResponse> {
        let Some(_) = self.get_agent(id).await? else {
            return Err(anyhow!("agent not found"));
        };
        let token = generate_token();
        let token_hash = hash_token(&token);
        let agent = sqlx::query_as::<_, Agent>(&format!(
            "UPDATE agents SET token_hash = $2, online = FALSE
             WHERE id = $1
             RETURNING {AGENT_COLS}"
        ))
        .bind(id)
        .bind(&token_hash)
        .fetch_one(&self.pool)
        .await?;
        Ok(CreateAgentResponse { agent, token })
    }

    pub async fn agent_by_token(&self, token: &str) -> Result<Option<Agent>> {
        let hash = hash_token(token);
        Ok(sqlx::query_as::<_, Agent>(&format!(
            "SELECT {AGENT_COLS} FROM agents WHERE token_hash = $1"
        ))
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Queued steps an agent may lease: global agents see all; scoped agents see one project.
    pub async fn list_queued_steps_for_pool(
        &self,
        agent_project_id: Option<Uuid>,
    ) -> Result<Vec<StepRun>> {
        if let Some(pid) = agent_project_id {
            Ok(sqlx::query_as::<_, StepRun>(
                "SELECT s.id, s.run_id, s.step_id, s.step_name, s.status, s.image, s.run_cmd,
                        s.labels, s.needs, s.retries, s.attempt, s.agent_id, s.lease_expires_at,
                        s.exit_code, s.error, s.started_at, s.finished_at
                 FROM step_runs s
                 INNER JOIN runs r ON r.id = s.run_id
                 WHERE s.status = 'queued' AND r.project_id = $1
                 ORDER BY s.started_at NULLS FIRST, s.id",
            )
            .bind(pid)
            .fetch_all(&self.pool)
            .await?)
        } else {
            self.list_queued_steps().await
        }
    }

    pub async fn set_agent_online(&self, id: Uuid, online: bool) -> Result<()> {
        sqlx::query(
            "UPDATE agents SET online = $2, last_seen_at = NOW() WHERE id = $1",
        )
        .bind(id)
        .bind(online)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn touch_agent(&self, id: Uuid) -> Result<()> {
        sqlx::query("UPDATE agents SET last_seen_at = NOW(), online = TRUE WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Agents that claim online but have not heartbeated within `stale_secs`.
    /// Marks them offline and returns their ids for disconnect cleanup.
    pub async fn mark_stale_agents_offline(&self, stale_secs: i64) -> Result<Vec<Uuid>> {
        let stale_secs = stale_secs.max(15);
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            r#"
            UPDATE agents
            SET online = FALSE
            WHERE online = TRUE
              AND (
                last_seen_at IS NULL
                OR last_seen_at < NOW() - make_interval(secs => $1::int)
              )
            RETURNING id
            "#,
        )
        .bind(stale_secs as i32)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    pub async fn find_pipelines_for_push(
        &self,
        project_id: Uuid,
        branch: &str,
        changed_files: &[String],
    ) -> Result<Vec<Pipeline>> {
        let pipelines = self.list_pipelines(project_id).await?;
        let mut matched = Vec::new();
        for p in pipelines {
            if let Ok(def) = value_to_definition(&p.definition) {
                if let Some(on) = &def.on {
                    if let Some(push) = &on.push {
                        let branch_ok = push.branches.is_empty()
                            || push.branches.iter().any(|b| b == branch);
                        if !branch_ok {
                            continue;
                        }
                        if !crate::path_filter::paths_allow(
                            changed_files,
                            &push.paths,
                            &push.paths_ignore,
                        ) {
                            continue;
                        }
                        matched.push(p);
                    }
                }
            }
        }
        Ok(matched)
    }

    pub async fn find_pipelines_for_pull_request(
        &self,
        project_id: Uuid,
        base_branch: &str,
        action: &str,
        changed_files: &[String],
    ) -> Result<Vec<Pipeline>> {
        let pipelines = self.list_pipelines(project_id).await?;
        let mut matched = Vec::new();
        for p in pipelines {
            if let Ok(def) = value_to_definition(&p.definition) {
                if let Some(on) = &def.on {
                    if let Some(pr) = &on.pull_request {
                        let branch_ok = pr.branches.is_empty()
                            || pr.branches.iter().any(|b| b == base_branch);
                        if !branch_ok {
                            continue;
                        }
                        let default_types = ["opened", "synchronize", "reopened"];
                        let types: Vec<&str> = if pr.types.is_empty() {
                            default_types.to_vec()
                        } else {
                            pr.types.iter().map(|s| s.as_str()).collect()
                        };
                        if !types.iter().any(|t| *t == action) {
                            continue;
                        }
                        // Without a file list, path-filtered PR pipelines cannot match.
                        if !pr.paths.is_empty() || !pr.paths_ignore.is_empty() {
                            if !crate::path_filter::paths_allow(
                                changed_files,
                                &pr.paths,
                                &pr.paths_ignore,
                            ) {
                                continue;
                            }
                        }
                        matched.push(p);
                    }
                }
            }
        }
        Ok(matched)
    }

    pub async fn upsert_webhook_secret(
        &self,
        project_id: Uuid,
        provider: &str,
        secret: &str,
    ) -> Result<()> {
        let id = Uuid::new_v4();
        sqlx::query(
            "DELETE FROM webhook_secrets WHERE project_id = $1 AND provider = $2",
        )
        .bind(project_id)
        .bind(provider)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "INSERT INTO webhook_secrets (id, project_id, provider, secret) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(project_id)
        .bind(provider)
        .bind(secret)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_webhook_secret(
        &self,
        project_id: Uuid,
        provider: &str,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT secret FROM webhook_secrets WHERE project_id = $1 AND provider = $2",
        )
        .bind(project_id)
        .bind(provider)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn ensure_admin_user(&self, username: &str, password: &str) -> Result<PublicUser> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        if count > 0 {
            if let Some(u) = self.find_user_by_username(username).await? {
                return Ok(u);
            }
            return Ok(sqlx::query_as::<_, PublicUser>(
                "SELECT id, username FROM users ORDER BY created_at LIMIT 1",
            )
            .fetch_one(&self.pool)
            .await?);
        }
        let id = Uuid::new_v4();
        let hash = crate::tokens::hash_password(password);
        let user = sqlx::query_as::<_, PublicUser>(
            "INSERT INTO users (id, username, password_hash) VALUES ($1, $2, $3)
             RETURNING id, username",
        )
        .bind(id)
        .bind(username)
        .bind(hash)
        .fetch_one(&self.pool)
        .await?;
        tracing::info!(%username, "bootstrapped admin user");
        Ok(user)
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<Option<LoginResponse>> {
        let user = sqlx::query_as::<_, User>(
            "SELECT id, username, password_hash, created_at FROM users WHERE username = $1",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        let Some(user) = user else {
            return Ok(None);
        };
        if !crate::tokens::verify_password(password, &user.password_hash) {
            return Ok(None);
        }
        // Upgrade legacy SHA-256 password hashes to argon2id on successful login.
        if crate::tokens::password_needs_rehash(&user.password_hash) {
            let new_hash = crate::tokens::hash_password(password);
            let _ = sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
                .bind(user.id)
                .bind(&new_hash)
                .execute(&self.pool)
                .await;
            tracing::info!(user_id = %user.id, "upgraded password hash to argon2id");
        }
        let token = crate::tokens::generate_session_token();
        let token_hash = crate::tokens::hash_token(&token);
        let id = Uuid::new_v4();
        let expires_at = Utc::now() + chrono::Duration::days(14);
        sqlx::query(
            "INSERT INTO sessions (id, user_id, token_hash, expires_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(user.id)
        .bind(token_hash)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(Some(LoginResponse {
            token,
            user: PublicUser {
                id: user.id,
                username: user.username,
            },
            expires_at,
        }))
    }

    pub async fn user_by_session_token(&self, token: &str) -> Result<Option<PublicUser>> {
        let hash = crate::tokens::hash_token(token);
        Ok(sqlx::query_as::<_, PublicUser>(
            "SELECT u.id, u.username
             FROM sessions s
             JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > NOW()",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?)
    }

    pub async fn logout(&self, token: &str) -> Result<()> {
        let hash = crate::tokens::hash_token(token);
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_secret_keys(&self, project_id: Uuid) -> Result<Vec<ProjectSecretMeta>> {
        Ok(sqlx::query_as::<_, ProjectSecretMeta>(
            "SELECT id, project_id, key, created_at, updated_at
             FROM project_secrets WHERE project_id = $1 ORDER BY key",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn upsert_secret(
        &self,
        project_id: Uuid,
        key: &str,
        value: &str,
    ) -> Result<ProjectSecretMeta> {
        let id = Uuid::new_v4();
        let stored = crate::secrets::encrypt_secret(value)?;
        Ok(sqlx::query_as::<_, ProjectSecretMeta>(
            "INSERT INTO project_secrets (id, project_id, key, value)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (project_id, key) DO UPDATE
               SET value = EXCLUDED.value, updated_at = NOW()
             RETURNING id, project_id, key, created_at, updated_at",
        )
        .bind(id)
        .bind(project_id)
        .bind(key)
        .bind(stored)
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn delete_secret(&self, project_id: Uuid, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM project_secrets WHERE project_id = $1 AND key = $2")
            .bind(project_id)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_secret_values(
        &self,
        project_id: Uuid,
    ) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT key, value FROM project_secrets WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (key, value) in rows {
            let plain = crate::secrets::decrypt_secret(&value)
                .with_context(|| format!("decrypt secret {key}"))?;
            out.push((key, plain));
        }
        Ok(out)
    }

    pub async fn get_secret_plain(
        &self,
        project_id: Uuid,
        key: &str,
    ) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT value FROM project_secrets WHERE project_id = $1 AND key = $2",
        )
        .bind(project_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some((value,)) => Ok(Some(crate::secrets::decrypt_secret(&value)?)),
            None => Ok(None),
        }
    }
}

pub fn value_to_definition(value: &Value) -> Result<PipelineDefinition> {
    if let Some(yaml) = value.get("yaml").and_then(|v| v.as_str()) {
        return Ok(parse_pipeline_yaml(yaml)?);
    }
    Ok(serde_json::from_value(value.clone())?)
}

fn snapshot_step_if_env(steps: &Value, step_id: &str) -> (Option<String>, Vec<(String, String)>) {
    let Some(arr) = steps.as_array() else {
        return (None, vec![]);
    };
    for s in arr {
        if s.get("id").and_then(|v| v.as_str()) != Some(step_id) {
            continue;
        }
        let if_expr = s
            .get("if")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let mut env = Vec::new();
        if let Some(pairs) = s.get("env").and_then(|v| v.as_array()) {
            for p in pairs {
                if let (Some(k), Some(v)) = (
                    p.get(0).and_then(|x| x.as_str()),
                    p.get(1).and_then(|x| x.as_str()),
                ) {
                    env.push((k.to_string(), v.to_string()));
                }
            }
        }
        if let Some(obj) = s.get("matrix").and_then(|v| v.as_object()) {
            for (k, v) in obj {
                if let Some(val) = v.as_str() {
                    env.push((k.clone(), val.to_string()));
                }
            }
        }
        return (if_expr, env);
    }
    (None, vec![])
}

fn status_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

fn step_status_str(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Pending => "pending",
        StepStatus::Queued => "queued",
        StepStatus::Running => "running",
        StepStatus::Succeeded => "succeeded",
        StepStatus::Failed => "failed",
        StepStatus::Cancelled => "cancelled",
        StepStatus::Skipped => "skipped",
    }
}
