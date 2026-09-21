use crate::dag::{CompiledDag, compile_definition, parse_pipeline_yaml};
use crate::models::*;
// sqlx 0.9 refuses a runtime-built query string unless it is asserted safe. Every
// interpolation in this file splices a column-list `const` — PIPELINE_COLS, AGENT_COLS,
// RUN_COLS, STEP_RUN_COLS — and never caller data; values are always bind parameters.
// Any new `AssertSqlSafe` here has to hold to that, or it is a SQL injection.
use crate::schedule::{has_schedule, initial_due_from_definition, next_due_from_triggers};
use crate::tokens::{generate_token, hash_token, slugify};
use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use fiber_proto::{PipelineDefinition, RunStatus, StepStatus};
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use sqlx::PgPool;
use uuid::Uuid;

const PIPELINE_COLS: &str =
    "id, project_id, name, definition, created_at, updated_at, last_scheduled_at, next_due_at";
const AGENT_COLS: &str =
    "id, project_id, name, labels, concurrency, token_hash, last_seen_at, online, created_at";

#[derive(Clone)]
pub struct Store {
    pub pool: PgPool,
}

/// Bucket boundaries in seconds, for both step duration and queue wait. Chosen for CI:
/// sub-second is noise, and anything past an hour is a stuck build rather than a slow one.
/// How far back the `/metrics` latency histograms look.
///
/// A day is what an operator watches when something is wrong now; Prometheus keeps the
/// longer history itself from these samples. Unbounded, each histogram was a sequential
/// scan of every `step_attempts` row inside the retention window, twice per scrape.
const METRICS_WINDOW_HOURS: i32 = 24;

const HISTOGRAM_BUCKETS: &[f64] = &[
    1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0,
];

/// Expand a concurrency group template into the string runs actually contend on.
///
/// `{pipeline}` is the pipeline id rather than its name: names are editable and not
/// unique, and a group that silently changes meaning when someone renames a pipeline is
/// worse than a long one. `{ref}` is the branch or PR ref, empty for a manual run — which
/// is deliberate, so two manual runs of one pipeline still contend with each other.
///
/// Unknown placeholders are left alone. A template is author-supplied text, and quietly
/// eating `{version}` would make one group out of what was meant to be many.
pub fn resolve_concurrency_group(
    template: Option<&str>,
    pipeline_id: Uuid,
    head_ref: Option<&str>,
) -> String {
    template
        .unwrap_or("{pipeline}-{ref}")
        .replace("{pipeline}", &pipeline_id.to_string())
        .replace("{ref}", head_ref.unwrap_or(""))
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
        // Project and owner land together: a project with no owner can never be deleted
        // or have an owner granted, since both need one.
        let mut tx = self.pool.begin().await?;
        let project = sqlx::query_as::<_, Project>(
            "INSERT INTO projects (id, name, slug) VALUES ($1, $2, $3)
             RETURNING id, name, slug, created_at",
        )
        .bind(id)
        .bind(&req.name)
        .bind(&slug)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO project_members (project_id, user_id, role) VALUES ($1, $2, $3)")
            .bind(project.id)
            .bind(owner_id)
            .bind(crate::roles::ProjectRole::Owner.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(project)
    }

    /// Add a member or change their role. An existing owner's role only changes when
    /// `actor_is_owner`, and never to leave the project without one — both checks are
    /// inside the statement, under a lock on the project's owner rows, so two concurrent
    /// demotions cannot each see "two owners" and together remove both.
    pub async fn add_project_member(
        &self,
        project_id: Uuid,
        user_id: Uuid,
        role: crate::roles::ProjectRole,
        actor_is_owner: bool,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        lock_project_owners_on(&mut tx, project_id).await?;
        let changed = sqlx::query(
            "INSERT INTO project_members (project_id, user_id, role)
             VALUES ($1, $2, $3)
             ON CONFLICT (project_id, user_id) DO UPDATE SET role = EXCLUDED.role
             WHERE project_members.role <> 'owner'
                OR ($4 AND (EXCLUDED.role = 'owner'
                            OR (SELECT COUNT(*) FROM project_members m
                                WHERE m.project_id = $1 AND m.role = 'owner') > 1))",
        )
        .bind(project_id)
        .bind(user_id)
        .bind(role.as_str())
        .bind(actor_is_owner)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed == 0 {
            // The decision was already made atomically above; this only names the reason.
            tx.rollback().await?;
            return Err(if actor_is_owner {
                crate::ValidationError("cannot demote the last owner".into()).into()
            } else {
                anyhow!("forbidden: only an owner can change an owner's role")
            });
        }
        tx.commit().await?;
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

    /// Remove a member. Removing an owner takes an owner (`actor_is_owner`) and never the
    /// last one; see `add_project_member` for why the guard is inside the statement.
    /// Removing someone who is not a member is a no-op.
    pub async fn remove_project_member(
        &self,
        project_id: Uuid,
        user_id: Uuid,
        actor_is_owner: bool,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        lock_project_owners_on(&mut tx, project_id).await?;
        let removed = sqlx::query(
            "DELETE FROM project_members
             WHERE project_id = $1 AND user_id = $2
               AND (role <> 'owner'
                    OR ($3 AND (SELECT COUNT(*) FROM project_members m
                                WHERE m.project_id = $1 AND m.role = 'owner') > 1))",
        )
        .bind(project_id)
        .bind(user_id)
        .bind(actor_is_owner)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if removed == 0 {
            let role: Option<String> = sqlx::query_scalar(
                "SELECT role FROM project_members WHERE project_id = $1 AND user_id = $2",
            )
            .bind(project_id)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
            tx.rollback().await?;
            return match role {
                None => Ok(()),
                Some(_) if !actor_is_owner => {
                    Err(anyhow!("forbidden: only an owner can remove an owner"))
                }
                Some(_) => {
                    Err(crate::ValidationError("cannot remove the last owner".into()).into())
                }
            };
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn find_user_by_username(&self, username: &str) -> Result<Option<PublicUser>> {
        Ok(sqlx::query_as::<_, PublicUser>(
            "SELECT id, username, is_admin FROM users WHERE username = $1",
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
             RETURNING id, username, is_admin",
        )
        .bind(id)
        .bind(username)
        .bind(hash)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Grant or revoke instance-admin. Refuses to demote the last admin; the guard is
    /// part of the UPDATE so two concurrent demotions cannot both succeed.
    pub async fn set_instance_admin(&self, user_id: Uuid, is_admin: bool) -> Result<PublicUser> {
        let updated = sqlx::query_as::<_, PublicUser>(
            "UPDATE users SET is_admin = $2
             WHERE id = $1
               AND ($2 OR EXISTS (SELECT 1 FROM users o WHERE o.is_admin AND o.id <> $1))
             RETURNING id, username, is_admin",
        )
        .bind(user_id)
        .bind(is_admin)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(u) = updated {
            return Ok(u);
        }
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE id = $1)")
            .bind(user_id)
            .fetch_one(&self.pool)
            .await?;
        if exists {
            Err(crate::ValidationError("cannot demote the last instance admin".into()).into())
        } else {
            anyhow::bail!("user not found")
        }
    }

    /// Bucket one expression into a Prometheus histogram.
    ///
    /// `width_bucket` does the counting in Postgres, so this is one row per bucket rather
    /// than one per observation. The caller's `from_sql` must produce a single `v` column
    /// and bound itself to the last `$2` hours: unbounded, each of these was a sequential
    /// scan of every attempt inside the retention window on every scrape.
    async fn histogram(&self, from_sql: &str, window_hours: i32) -> Result<Histogram> {
        let rows: Vec<(i32, i64, f64)> = sqlx::query_as(AssertSqlSafe(format!(
            "SELECT width_bucket(v, $1::float8[])::int4, COUNT(*)::int8, COALESCE(SUM(v), 0)::float8 \
             FROM ({from_sql}) t WHERE v IS NOT NULL AND v >= 0 GROUP BY 1"
        )))
        .bind(HISTOGRAM_BUCKETS)
        .bind(window_hours)
        .fetch_all(&self.pool)
        .await?;

        let mut per_bucket = vec![0i64; HISTOGRAM_BUCKETS.len() + 1];
        let mut count = 0i64;
        let mut sum = 0.0;
        for (idx, n, s) in rows {
            // width_bucket returns 0 below the first threshold and len() above the last.
            per_bucket[(idx as usize).min(HISTOGRAM_BUCKETS.len())] += n;
            count += n;
            sum += s;
        }
        // Prometheus buckets are cumulative: each `le` counts everything at or below it.
        let mut running = 0i64;
        let mut buckets = Vec::with_capacity(HISTOGRAM_BUCKETS.len());
        for (i, le) in HISTOGRAM_BUCKETS.iter().enumerate() {
            running += per_bucket[i];
            buckets.push((*le, running));
        }
        Ok(Histogram {
            buckets,
            count,
            sum,
        })
    }

    /// One database round of the numbers `/metrics` reports.
    ///
    /// Everything here is derived from the tables rather than counters held in this
    /// process, so a restart does not reset them and two API replicas report the same
    /// figures. Retention bounds every table involved.
    pub async fn metrics_snapshot(&self) -> Result<MetricsSnapshot> {
        let step_runs: Vec<(String, i64)> =
            sqlx::query_as("SELECT status, COUNT(*) FROM step_runs GROUP BY status")
                .fetch_all(&self.pool)
                .await?;
        let runs: Vec<(String, i64)> =
            sqlx::query_as("SELECT status, COUNT(*) FROM runs GROUP BY status")
                .fetch_all(&self.pool)
                .await?;
        let fibers: Vec<(String, i64)> =
            sqlx::query_as("SELECT status, COUNT(*) FROM fibers GROUP BY status")
                .fetch_all(&self.pool)
                .await?;
        let (agents_online, agents_total): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*) FILTER (WHERE online), COUNT(*) FROM agents")
                .fetch_one(&self.pool)
                .await?;
        // How long the oldest step that could be leased right now has been waiting,
        // measured from when it became leasable (`queued_at`, stamped on every path since
        // 012 and backfilled by 016) — not from its run's creation, which charged a
        // dependent step for the whole time its predecessors ran. Steps held back by a
        // backoff are excluded: they are waiting on purpose, and counting them would make
        // a healthy queue look starved.
        let oldest_queued_step_age_secs: Option<f64> = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (NOW() - MIN(queued_at)))::float8 \
             FROM step_runs \
             WHERE status = 'queued' AND (not_before IS NULL OR not_before <= NOW())",
        )
        .fetch_one(&self.pool)
        .await?;
        // Both are per attempt, not per step: a step that was retried waited twice and ran
        // twice, and averaging that away would hide exactly the runs worth looking at.
        //
        // Both are bounded to a recent window. A histogram over 30 days of attempts
        // answers a question nobody asks (Prometheus keeps its own history of these) and
        // costs a full scan of the table on every scrape.
        let step_duration = self
            .histogram(
                "SELECT EXTRACT(EPOCH FROM (finished_at - started_at))::float8 AS v \
                 FROM step_attempts \
                 WHERE finished_at IS NOT NULL \
                   AND started_at >= NOW() - make_interval(hours => $2::int4)",
                METRICS_WINDOW_HOURS,
            )
            .await?;
        let queue_wait = self
            .histogram(
                "SELECT queue_wait_seconds AS v FROM step_attempts \
                 WHERE started_at >= NOW() - make_interval(hours => $2::int4)",
                METRICS_WINDOW_HOURS,
            )
            .await?;
        Ok(MetricsSnapshot {
            step_runs,
            runs,
            fibers,
            agents_online,
            agents_total,
            oldest_queued_step_age_secs,
            step_duration,
            queue_wait,
        })
    }

    pub async fn count_instance_admins(&self) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE is_admin")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn list_users(&self) -> Result<Vec<PublicUser>> {
        Ok(sqlx::query_as::<_, PublicUser>(
            "SELECT id, username, is_admin FROM users ORDER BY created_at, id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn project_id_for_pipeline(&self, pipeline_id: Uuid) -> Result<Option<Uuid>> {
        Ok(
            sqlx::query_scalar("SELECT project_id FROM pipelines WHERE id = $1")
                .bind(pipeline_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn project_id_for_run(&self, run_id: Uuid) -> Result<Option<Uuid>> {
        Ok(
            sqlx::query_scalar("SELECT project_id FROM runs WHERE id = $1")
                .bind(run_id)
                .fetch_optional(&self.pool)
                .await?,
        )
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
        Ok(
            sqlx::query_scalar("SELECT project_id FROM fibers WHERE id = $1")
                .bind(fiber_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn get_project(&self, id: Uuid) -> Result<Option<Project>> {
        Ok(sqlx::query_as::<_, Project>(
            "SELECT id, name, slug, created_at FROM projects WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Runs of this project that an agent is currently holding a step of.
    ///
    /// Only these need cancelling before a project is deleted: cancelling is how an
    /// agent is *told* to stop, and a queued or pending run has nobody to tell. Scanning
    /// every non-terminal run instead would put an unbounded, attacker-sized loop of
    /// per-run round trips inside one HTTP request.
    pub async fn leased_run_ids_for_project(&self, project_id: Uuid) -> Result<Vec<Uuid>> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT DISTINCT r.id FROM runs r
               JOIN step_runs s ON s.run_id = r.id
              WHERE r.project_id = $1
                AND s.agent_id IS NOT NULL
                AND s.status IN ('running', 'queued')",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Agents dedicated to this project. They are cascade-deleted with it, so their live
    /// sockets must be dropped first.
    pub async fn project_agent_ids(&self, project_id: Uuid) -> Result<Vec<Uuid>> {
        Ok(
            sqlx::query_scalar::<_, Uuid>("SELECT id FROM agents WHERE project_id = $1")
                .bind(project_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// One page of this project's run ids, oldest first.
    pub async fn run_ids_for_project(&self, project_id: Uuid, limit: i64) -> Result<Vec<Uuid>> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM runs WHERE project_id = $1 ORDER BY created_at LIMIT $2",
        )
        .bind(project_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Delete these runs and return the artifact blob paths they referenced.
    ///
    /// Both halves are one transaction, and the runs are locked before their artifact
    /// rows are read. Reading the paths first and deleting afterwards leaves a window in
    /// which an upload — a presigned PUT minted up to ten minutes earlier, say — inserts
    /// an artifact row whose blob is then never seen again by anything. Holding the lock
    /// makes that insert wait for the delete, which then removes it by cascade.
    ///
    /// The caller decides which of the returned paths are safe to remove; a retry shares
    /// its predecessor's artifact rows, so a path here may still be referenced elsewhere.
    pub async fn delete_runs_returning_artifact_paths(
        &self,
        run_ids: &[Uuid],
    ) -> Result<Vec<String>> {
        if run_ids.is_empty() {
            return Ok(vec![]);
        }
        let mut tx = self.pool.begin().await?;
        // By `id`, the lock order every multi-row lock in this file follows.
        sqlx::query("SELECT id FROM runs WHERE id = ANY($1) ORDER BY id FOR UPDATE")
            .bind(run_ids)
            .execute(&mut *tx)
            .await?;
        // `path <> ''` matches retention's guard: an empty path is not a blob, and
        // handing one to the artifact backend asks it to delete the store's root.
        let paths = sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT path FROM artifacts WHERE run_id = ANY($1) AND path <> ''",
        )
        .bind(run_ids)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM runs WHERE id = ANY($1)")
            .bind(run_ids)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(paths)
    }

    /// Delete a project and whatever is left hanging off it. `false` means no such project.
    ///
    /// Callers delete the runs in batches first — this statement's cascade is otherwise
    /// unbounded. What remains here is small and fixed per project: pipelines, members,
    /// secrets, the webhook secret, durable fibers, and project-scoped agents, by
    /// `ON DELETE CASCADE` (migrations 001, 002, 004).
    pub async fn delete_project(&self, project_id: Uuid) -> Result<bool> {
        let res = sqlx::query("DELETE FROM projects WHERE id = $1")
            .bind(project_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
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
        Ok(sqlx::query_as::<_, Pipeline>(AssertSqlSafe(q))
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
        let pipeline = sqlx::query_as::<_, Pipeline>(AssertSqlSafe(q))
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
        let existing = self
            .get_pipeline(pipeline_id)
            .await?
            .ok_or_else(|| anyhow!("pipeline not found"))?;
        let scheduled = def.on.as_ref().is_some_and(has_schedule);
        // A changed cron or interval starts over from the new rule; otherwise the stored
        // due time is kept. The stored value is read *in the statement* rather than from
        // `existing`: the schedule loop claims slots with a compare-and-set on this same
        // column, and writing back a value read a moment ago would re-arm a slot it had
        // just consumed — the pipeline would fire twice.
        let existing_def = value_to_definition(&existing.definition).ok();
        let schedule_changed = crate::schedule::schedule_key(def.on.as_ref())
            != crate::schedule::schedule_key(existing_def.as_ref().and_then(|d| d.on.as_ref()));
        let initial = initial_due_from_definition(&def);
        let q = format!(
            "UPDATE pipelines
             SET name = COALESCE($2, name), definition = $3, updated_at = NOW(),
                 next_due_at = CASE
                     WHEN NOT $4 THEN NULL
                     WHEN $5 THEN $6
                     ELSE COALESCE(next_due_at, $6)
                 END
             WHERE id = $1
             RETURNING {PIPELINE_COLS}"
        );
        let pipeline = sqlx::query_as::<_, Pipeline>(AssertSqlSafe(q))
            .bind(pipeline_id)
            .bind(&req.name)
            .bind(&req.definition)
            .bind(scheduled)
            .bind(schedule_changed)
            .bind(initial)
            .fetch_one(&self.pool)
            .await?;
        Ok(pipeline)
    }

    pub async fn get_pipeline(&self, id: Uuid) -> Result<Option<Pipeline>> {
        let q = format!("SELECT {PIPELINE_COLS} FROM pipelines WHERE id = $1");
        Ok(sqlx::query_as::<_, Pipeline>(AssertSqlSafe(q))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    pub async fn list_all_pipelines(&self) -> Result<Vec<Pipeline>> {
        let q = format!("SELECT {PIPELINE_COLS} FROM pipelines");
        Ok(sqlx::query_as::<_, Pipeline>(AssertSqlSafe(q))
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
        Ok(sqlx::query_as::<_, Pipeline>(AssertSqlSafe(q))
            .bind(now)
            .fetch_all(&self.pool)
            .await?)
    }

    /// Claim one schedule slot with a compare-and-set: succeeds only if `next_due_at`
    /// is still the value the caller observed **and** the pipeline has no active run.
    /// With several API instances ticking the same schedule, exactly one wins.
    /// Returns `false` when another instance claimed it or a run is still active.
    pub async fn claim_schedule_slot(
        &self,
        pipeline_id: Uuid,
        expected_due: DateTime<Utc>,
        next_due: Option<DateTime<Utc>>,
    ) -> Result<bool> {
        let claimed: Option<Uuid> = sqlx::query_scalar(
            "UPDATE pipelines
             SET last_scheduled_at = NOW(), next_due_at = $3
             WHERE id = $1
               AND next_due_at = $2
               AND NOT EXISTS (
                   SELECT 1 FROM runs r
                   WHERE r.pipeline_id = $1 AND r.status IN ('pending', 'running'))
             RETURNING id",
        )
        .bind(pipeline_id)
        .bind(expected_due)
        .bind(next_due)
        .fetch_optional(&self.pool)
        .await?;
        Ok(claimed.is_some())
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
            sqlx::query(
                "UPDATE pipelines SET next_due_at = $2 WHERE id = $1 AND next_due_at IS NULL",
            )
            .bind(p.id)
            .bind(next)
            .execute(&self.pool)
            .await?;
            n += 1;
        }
        Ok(n)
    }

    /// Re-queue a failed step for another attempt, not offerable before `backoff_secs`.
    /// The failed attempt is closed in `step_attempts` (with its exit code / error) so
    /// the attempt history is complete and the timeout backstop, which looks at the
    /// open attempt, never sees a stale one.
    /// Returns `None` when the step is no longer running under `agent_id` — another
    /// replica's backstop or a cancel got there first — so a live attempt is never
    /// cleared by a late requeue.
    pub async fn requeue_for_retry(
        &self,
        step_run_id: Uuid,
        agent_id: Uuid,
        backoff_secs: i64,
        exit_code: Option<i32>,
        error: Option<&str>,
    ) -> Result<Option<StepRun>> {
        let mut tx = self.pool.begin().await?;
        let requeued = sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
            "UPDATE step_runs
             SET status = 'queued', agent_id = NULL, lease_expires_at = NULL,
                 error = NULL, exit_code = NULL, finished_at = NULL, queued_at = NOW(),
                 not_before = NOW() + make_interval(secs => $2)
             WHERE id = $1 AND status = 'running' AND agent_id = $3
             RETURNING {STEP_RUN_COLS}"
        )))
        .bind(step_run_id)
        .bind(backoff_secs as f64)
        .bind(agent_id)
        .fetch_optional(&mut *tx)
        .await?;
        if requeued.is_some() {
            finish_open_attempt_on(&mut tx, step_run_id, "failed", exit_code, error).await?;
        }
        tx.commit().await?;
        Ok(requeued)
    }

    pub async fn start_run(&self, pipeline_id: Uuid, trigger: &str) -> Result<StartedRun> {
        self.start_run_for_commit(pipeline_id, trigger, RunCommit::default())
            .await
    }

    /// Start a run for a specific commit. The agent checks out `head_sha` rather than
    /// whatever the branch points at by the time it clones, so a second push mid-build
    /// cannot retarget this run.
    ///
    /// When the run has a concurrency group, the insert and the search for the runs it
    /// supersedes happen under one advisory lock on `(project, group)`, so two pushes
    /// landing on two replicas at once cannot each commit, each see the other as newer,
    /// and both keep running. The caller cancels `superseded` after this commits.
    pub async fn start_run_for_commit(
        &self,
        pipeline_id: Uuid,
        trigger: &str,
        commit: RunCommit,
    ) -> Result<StartedRun> {
        let pipeline = self
            .get_pipeline(pipeline_id)
            .await?
            .ok_or_else(|| anyhow!("pipeline not found"))?;
        let def = value_to_definition(&pipeline.definition)?;
        let compiled = compile_definition(&def)?;
        let run_id = Uuid::new_v4();
        let snapshot = serde_json::to_value(&compiled)?;
        // Resolved now and stored on the row, so the rule that governed this run stays
        // readable after the pipeline is edited — the same reason the snapshot is here.
        // Only a pipeline that asked to cancel gets a group; without one the run contends
        // with nothing, which is what every existing pipeline expects.
        let concurrency_group = def
            .concurrency
            .as_ref()
            .filter(|c| c.cancel_in_progress)
            .map(|c| {
                resolve_concurrency_group(
                    c.group.as_deref(),
                    pipeline_id,
                    commit.head_ref.as_deref(),
                )
            });

        // The run row and every step row land together: a half-inserted DAG would
        // otherwise "succeed" once its partial set of steps finished.
        let mut tx = self.pool.begin().await?;
        if let Some(group) = &concurrency_group {
            lock_concurrency_group_on(&mut tx, pipeline.project_id, group).await?;
        }
        // `clock_timestamp()`, not `NOW()`: NOW() is the transaction's start, which is
        // before the group lock was taken. Two starts waiting on the lock would then be
        // ordered by who *began* rather than who *got in*, and the one that got in second
        // could carry the earlier timestamp and be cancelled by a run it had already
        // seen and cancelled itself.
        let run = sqlx::query_as::<_, Run>(AssertSqlSafe(format!(
            "INSERT INTO runs
               (id, pipeline_id, project_id, status, trigger, definition_snapshot, created_at,
                started_at, head_sha, head_ref, pr_number, repo_full_name, untrusted,
                concurrency_group)
             VALUES ($1, $2, $3, $4, $5, $6, clock_timestamp(), clock_timestamp(), $7, $8, $9,
                     $10, $11, $12)
             RETURNING {RUN_COLS}"
        )))
        .bind(run_id)
        .bind(pipeline_id)
        .bind(pipeline.project_id)
        .bind(status_str(RunStatus::Running))
        .bind(trigger)
        .bind(&snapshot)
        .bind(&commit.head_sha)
        .bind(&commit.head_ref)
        .bind(commit.pr_number)
        .bind(&commit.repo_full_name)
        .bind(commit.untrusted)
        .bind(&concurrency_group)
        .fetch_one(&mut *tx)
        .await?;
        let superseded = superseded_runs_on(&mut tx, &run).await?;

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
            sqlx::query(
                "INSERT INTO step_runs
                 (id, run_id, step_id, step_name, status, image, run_cmd, labels, needs, retries,
                  error, queued_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11,
                         CASE WHEN $5 = 'queued' THEN NOW() END)",
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
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        // If all roots skipped, propagate so dependents can resolve — and return the run
        // as it is afterwards (it may already be terminal).
        let _ = self.propagate_after_step(run_id).await?;
        let run = self.get_run(run_id).await?.unwrap_or(run);

        Ok(StartedRun {
            steps: self.list_step_runs(run_id).await?,
            run,
            dag: compiled,
            superseded,
        })
    }

    pub async fn get_run(&self, id: Uuid) -> Result<Option<Run>> {
        Ok(sqlx::query_as::<_, Run>(AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM runs WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Re-run a finished run from **its own** definition snapshot, not the pipeline as it
    /// stands now — a retry reproduces what the original executed.
    ///
    /// With `failed_only`, steps that succeeded the first time are carried over as
    /// already-succeeded (their artifacts copied so dependents can still restore them)
    /// and only the rest run again. Otherwise every step runs.
    ///
    /// Same group serialisation as `start_run_for_commit`: a retry of a `main` build is a
    /// new run in the `main` group and supersedes (or is superseded by) the others.
    pub async fn retry_run(&self, run_id: Uuid, failed_only: bool) -> Result<StartedRun> {
        let original = self
            .get_run(run_id)
            .await?
            .ok_or_else(|| anyhow!("run not found"))?;
        if !original.status_enum().is_terminal() {
            return Err(crate::ValidationError(
                "run is still active; cancel it before retrying".into(),
            )
            .into());
        }
        let compiled: CompiledDag = serde_json::from_value(original.definition_snapshot.clone())
            .map_err(|e| {
                crate::ValidationError(format!("run snapshot is not a compiled pipeline: {e}"))
            })?;
        let previous = self.list_step_runs(run_id).await?;

        let new_run_id = Uuid::new_v4();
        let mut tx = self.pool.begin().await?;
        if let Some(group) = &original.concurrency_group {
            lock_concurrency_group_on(&mut tx, original.project_id, group).await?;
        }
        let run = sqlx::query_as::<_, Run>(AssertSqlSafe(format!(
            "INSERT INTO runs
               (id, pipeline_id, project_id, status, trigger, definition_snapshot, created_at,
              started_at, retry_of, head_sha, head_ref, pr_number, repo_full_name, untrusted,
              concurrency_group)
             VALUES ($1, $2, $3, $4, $5, $6, clock_timestamp(), clock_timestamp(), $7, $8, $9,
                     $10, $11, $12, $13)
             RETURNING {RUN_COLS}"
        )))
        .bind(new_run_id)
        .bind(original.pipeline_id)
        .bind(original.project_id)
        .bind(status_str(RunStatus::Running))
        .bind(format!("retry:{run_id}"))
        .bind(&original.definition_snapshot)
        .bind(run_id)
        // A retry re-runs the same commit, so it reports against it too.
        .bind(&original.head_sha)
        .bind(&original.head_ref)
        .bind(original.pr_number)
        .bind(&original.repo_full_name)
        // Re-running a fork's pull request is still running someone else's code.
        .bind(original.untrusted)
        // Same group as the original: a retry of a `main` build contends with — and is
        // superseded by — the next push to `main`, exactly as the first run was.
        .bind(&original.concurrency_group)
        .fetch_one(&mut *tx)
        .await?;
        let superseded = superseded_runs_on(&mut tx, &run).await?;

        for step in &compiled.steps {
            let sid = Uuid::new_v4();
            let prior = previous.iter().find(|p| p.step_id == step.id);
            let carry_over =
                failed_only && prior.is_some_and(|p| p.status_enum() == StepStatus::Succeeded);
            let status = if carry_over {
                StepStatus::Succeeded
            } else if step.needs.is_empty() {
                let if_ctx = crate::step_if::IfContext {
                    needs_succeeded: true,
                    env: step.env.clone(),
                };
                if crate::step_if::eval_if(step.if_expr.as_deref(), &if_ctx) {
                    StepStatus::Queued
                } else {
                    StepStatus::Skipped
                }
            } else {
                StepStatus::Pending
            };
            let error = match status {
                StepStatus::Skipped => Some("if: condition false".to_string()),
                _ => None,
            };
            sqlx::query(
                "INSERT INTO step_runs
                 (id, run_id, step_id, step_name, status, image, run_cmd, labels, needs, retries,
                  error, exit_code, started_at, finished_at, queued_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                         CASE WHEN $5 = 'queued' THEN NOW() END)",
            )
            .bind(sid)
            .bind(new_run_id)
            .bind(&step.id)
            .bind(&step.name)
            .bind(step_status_str(status))
            .bind(&step.image)
            .bind(&step.run)
            .bind(json!(step.labels))
            .bind(json!(step.needs))
            .bind(step.retries as i32)
            .bind(error)
            .bind(
                carry_over
                    .then(|| prior.and_then(|p| p.exit_code))
                    .flatten(),
            )
            .bind(
                carry_over
                    .then(|| prior.and_then(|p| p.started_at))
                    .flatten(),
            )
            .bind(
                carry_over
                    .then(|| prior.and_then(|p| p.finished_at))
                    .flatten(),
            )
            .execute(&mut *tx)
            .await?;

            // A carried-over step produces nothing this time, so its artifacts are copied
            // forward; otherwise its dependents would have nothing to restore. The blob is
            // shared — retention only deletes one once no run references it.
            if carry_over && let Some(p) = prior {
                sqlx::query(
                    "INSERT INTO artifacts (id, run_id, step_run_id, name, path, size)
                     SELECT gen_random_uuid(), $1, $2, name, path, size
                     FROM artifacts WHERE step_run_id = $3",
                )
                .bind(new_run_id)
                .bind(sid)
                .bind(p.id)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;

        let _ = self.propagate_after_step(new_run_id).await?;
        let run = self.get_run(new_run_id).await?.unwrap_or(run);
        Ok(StartedRun {
            steps: self.list_step_runs(new_run_id).await?,
            run,
            dag: compiled,
            superseded,
        })
    }

    /// Of `paths`, those still referenced by an artifact row. Retention must not delete a
    /// blob a retry (or any other run) still points at.
    pub async fn artifact_paths_still_referenced(&self, paths: &[String]) -> Result<Vec<String>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT DISTINCT path FROM artifacts WHERE path = ANY($1)",
        )
        .bind(paths)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Newest first. `before` is a run id from a previous page (keyset on
    /// `(created_at, id)`), so pages stay stable while new runs arrive.
    pub async fn list_runs(
        &self,
        project_id: Uuid,
        limit: i64,
        before: Option<Uuid>,
    ) -> Result<Vec<Run>> {
        Ok(sqlx::query_as::<_, Run>(AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM runs
                 WHERE project_id = $1
                   AND ($3::uuid IS NULL OR (created_at, id) < (
                         SELECT created_at, id FROM runs WHERE id = $3))
                 ORDER BY created_at DESC, id DESC
                 LIMIT $2"
        )))
        .bind(project_id)
        .bind(limit)
        .bind(before)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Runs that reached a terminal status at or after `since`, oldest first.
    ///
    /// For the commit-status reporter to recover from a gap in the event bus: a run that
    /// finished while the reporter was lagged would otherwise leave a required check
    /// pending forever, because the terminal event it was waiting for is gone.
    ///
    /// `limit` cuts from the **oldest** end, not the newest: on an instance finishing
    /// more runs than the cap in one window, the ones whose events were in the gap are
    /// the newest, and an ascending `LIMIT` would return only runs already reported and
    /// drop exactly the ones this exists for. The page is reversed so callers still see
    /// oldest first.
    pub async fn list_runs_finished_since(
        &self,
        since: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Run>> {
        let mut newest = sqlx::query_as::<_, Run>(AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM runs
                 WHERE finished_at >= $1
                   AND status IN ('succeeded', 'failed', 'cancelled')
                 ORDER BY finished_at DESC
                 LIMIT $2"
        )))
        .bind(since)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        newest.reverse();
        Ok(newest)
    }

    pub async fn list_step_runs(&self, run_id: Uuid) -> Result<Vec<StepRun>> {
        let mut conn = self.pool.acquire().await?;
        list_step_runs_on(&mut conn, run_id).await
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

    /// Queued steps for the global pool: everything except untrusted runs, which need a
    /// project-dedicated agent (see `list_queued_steps_for_pool`).
    pub async fn list_queued_steps(&self, agent_labels: &[String]) -> Result<Vec<StepRun>> {
        Ok(sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
            "SELECT {STEP_RUN_COLS_S}
             FROM step_runs s
             INNER JOIN runs r ON r.id = s.run_id
             WHERE s.status = 'queued' AND NOT r.untrusted
               AND (s.not_before IS NULL OR s.not_before <= NOW())
               AND s.labels <@ $1
             ORDER BY s.queued_at, s.id
             LIMIT {QUEUE_SCAN_LIMIT}"
        )))
        .bind(json!(agent_labels))
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
        // No `step_attempts` row yet: that is written by `record_step_attempt` once the
        // offer has been built and is about to be sent. An offer that cannot be built is
        // backed out with `unlease_step`, and an attempt row for it would be a ~0 s
        // "reclaimed" attempt every time — one per heartbeat while the cause persists,
        // which both fills the table and collapses the step-duration histogram.
        Ok(sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
            "UPDATE step_runs
             SET status = 'running', agent_id = $2, lease_expires_at = $3,
                 started_at = COALESCE(started_at, NOW()), attempt = attempt + 1
             WHERE id = $1 AND status = 'queued'
               AND (not_before IS NULL OR not_before <= NOW())
             RETURNING {STEP_RUN_COLS}"
        )))
        .bind(step_run_id)
        .bind(agent_id)
        .bind(expires)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Open the `step_attempts` row for a lease whose offer is built and about to go
    /// out. `attempt` is the leased row's counter, so a late call cannot open a row
    /// against a newer lease.
    ///
    /// Between `lease_step` and this, a running step has no attempt row, and the
    /// timeout backstop (`list_timed_out_steps`, which joins on the open attempt) does
    /// not see it. That window is one offer build on one connection; an API crash
    /// inside it leaves the lease to expire (`LEASE_SECS`) and the reclaim loop to
    /// requeue the step, which is the same path any lost lease takes.
    pub async fn record_step_attempt(
        &self,
        step_run_id: Uuid,
        attempt: i32,
        agent_id: Uuid,
    ) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        insert_step_attempt_on(&mut conn, step_run_id, attempt, Some(agent_id), "running").await
    }

    /// Put a step just leased to `agent_id` back as it was before the lease, because
    /// the offer for it could not be built for a reason that may clear (a store error)
    /// and was never sent.
    ///
    /// The agent never saw the step, so nothing ran: the attempt counter goes back
    /// (`lease_step` had incremented it) and `started_at` is cleared again when this
    /// was the first lease. No attempt row exists yet (`record_step_attempt` runs after
    /// the offer is built), so there is nothing to close. `queued_at` is left alone,
    /// but `not_before` is set 30 s out: a step at the head of the queue whose offer
    /// keeps failing would otherwise be leased and backed out on every heartbeat of
    /// every agent, and nothing behind it would ever be offered. The guard binds the
    /// leased row's `attempt` as well as the agent, so a release that arrives after a
    /// long stall cannot unlease a newer lease the same agent holds (a newer lease has
    /// a higher `attempt`). It deliberately does not bind `lease_expires_at`: a heartbeat
    /// renews that between the lease and the back-out, and a release that missed on it
    /// would leave the step leased with no offer sent until the lease expired. `None`
    /// means a cancel, reclaim, or newer lease got there first.
    pub async fn unlease_step(&self, leased: &StepRun) -> Result<Option<StepRun>> {
        Ok(sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
            "UPDATE step_runs
             SET status = 'queued', agent_id = NULL, lease_expires_at = NULL,
                 attempt = attempt - 1,
                 started_at = CASE WHEN attempt = 1 THEN NULL ELSE started_at END,
                 not_before = NOW() + make_interval(secs => {OFFER_RETRY_BACKOFF_SECS})
             WHERE id = $1 AND status = 'running' AND agent_id = $2
               AND attempt = $3
             RETURNING {STEP_RUN_COLS}"
        )))
        .bind(leased.id)
        .bind(leased.agent_id)
        .bind(leased.attempt)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Steps the database shows running on `agent_id` — the authoritative count behind
    /// its concurrency slots. Served by `idx_step_runs_agent`.
    pub async fn count_running_steps_for_agent(&self, agent_id: Uuid) -> Result<i64> {
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM step_runs WHERE agent_id = $1 AND status = 'running'",
        )
        .bind(agent_id)
        .fetch_one(&self.pool)
        .await?)
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
        let mut conn = self.pool.acquire().await?;
        complete_step_on(&mut conn, step_run_id, status, exit_code, error, None)
            .await?
            .ok_or_else(|| anyhow!("step run not found"))
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
        let mut tx = self.pool.begin().await?;
        let sr = sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
            "UPDATE step_runs
             SET status = $2, exit_code = $3, error = $4, finished_at = NOW(), lease_expires_at = NULL
             WHERE id = $1 AND status = 'running' AND agent_id = $5
             RETURNING {STEP_RUN_COLS}"
        )))
        .bind(step_run_id)
        .bind(step_status_str(status))
        .bind(exit_code)
        .bind(&error)
        .bind(agent_id)
        .fetch_optional(&mut *tx)
        .await?;
        if sr.is_some() {
            finish_open_attempt_on(
                &mut tx,
                step_run_id,
                step_status_str(status),
                exit_code,
                error.as_deref(),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(sr)
    }

    /// Lines already stored for one attempt of a step. Seeds the per-session log cap
    /// when an agent reconnects mid-attempt, so the cap is per attempt, not per socket.
    pub async fn count_log_lines(&self, step_run_id: Uuid, attempt: i32) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM log_lines WHERE step_run_id = $1 AND attempt = $2",
        )
        .bind(step_run_id)
        .bind(attempt)
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
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

    /// Running steps whose current attempt has exceeded its timeout plus a grace
    /// period (the agent is expected to fail the step itself first). The timeout comes
    /// from the run's definition snapshot, falling back to `default_minutes`.
    pub async fn list_timed_out_steps(
        &self,
        default_minutes: i64,
        grace_minutes: i64,
    ) -> Result<Vec<TimedOutStep>> {
        Ok(sqlx::query_as::<_, TimedOutStep>(
            "SELECT s.id AS step_run_id, s.run_id, s.step_id, s.agent_id,
                    COALESCE(js.t, $1)::bigint AS timeout_minutes
             FROM step_runs s
             JOIN runs r ON r.id = s.run_id
             JOIN step_attempts a ON a.step_run_id = s.id AND a.attempt = s.attempt
                                 AND a.finished_at IS NULL
             LEFT JOIN LATERAL (
                 SELECT (e->>'timeout_minutes')::bigint AS t
                 FROM jsonb_array_elements(r.definition_snapshot->'steps') e
                 WHERE e->>'id' = s.step_id
                 LIMIT 1) js ON TRUE
             WHERE s.status = 'running'
               AND a.started_at < NOW() - make_interval(mins => (COALESCE(js.t, $1) + $2)::int)",
        )
        .bind(default_minutes)
        .bind(grace_minutes)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Running runs whose snapshot carries a whole-run `timeout_minutes` that has elapsed.
    pub async fn list_timed_out_runs(&self) -> Result<Vec<(Uuid, i64)>> {
        Ok(sqlx::query_as::<_, (Uuid, i64)>(
            "SELECT id, (definition_snapshot->>'timeout_minutes')::bigint
             FROM runs
             WHERE status = 'running'
               AND (definition_snapshot->>'timeout_minutes') IS NOT NULL
               AND started_at < NOW() - make_interval(mins => (definition_snapshot->>'timeout_minutes')::int)",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Reclaim running steps whose lease ran out. See [`Reclaimed`] for what comes back.
    pub async fn requeue_expired_leases(&self) -> Result<Reclaimed> {
        let mut tx = self.pool.begin().await?;
        // SKIP LOCKED: a row another replica is reclaiming, or an agent is completing,
        // is not ours this tick — waiting on it would serialise every replica's sweep.
        let candidates = sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
            "SELECT {STEP_RUN_COLS} FROM step_runs
             WHERE status = 'running' AND lease_expires_at IS NOT NULL AND lease_expires_at < NOW()
             ORDER BY id
             FOR UPDATE SKIP LOCKED"
        )))
        .fetch_all(&mut *tx)
        .await?;
        let reclaimed = reclaim_steps_on(&mut tx, &candidates, "lease expired").await?;
        tx.commit().await?;
        self.propagate_reclaim_failures(reclaimed).await
    }

    /// Reclaim every step `agent_id` was running. `reason` closes the attempts (what
    /// `step_attempts.error` says: "agent disconnected", "agent shut down"). See
    /// [`Reclaimed`].
    pub async fn requeue_agent_steps(&self, agent_id: Uuid, reason: &str) -> Result<Reclaimed> {
        let mut tx = self.pool.begin().await?;
        let candidates = sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
            "SELECT {STEP_RUN_COLS} FROM step_runs
             WHERE agent_id = $1 AND status = 'running'
             ORDER BY id
             FOR UPDATE"
        )))
        .bind(agent_id)
        .fetch_all(&mut *tx)
        .await?;
        let reclaimed = reclaim_steps_on(&mut tx, &candidates, reason).await?;
        tx.commit().await?;
        self.propagate_reclaim_failures(reclaimed).await
    }

    /// A step that failed by running out of attempts is a completed step like any other:
    /// its dependents skip and its run finalises, or the run stays `running` forever.
    async fn propagate_reclaim_failures(&self, mut reclaimed: Reclaimed) -> Result<Reclaimed> {
        let mut runs: Vec<Uuid> = reclaimed.failed.iter().map(|s| s.run_id).collect();
        runs.sort_unstable();
        runs.dedup();
        for run_id in runs {
            reclaimed
                .propagated
                .extend(self.propagate_after_step(run_id).await?);
        }
        Ok(reclaimed)
    }

    /// Running runs with no step left to run: every step is terminal, yet the run was
    /// never finalised. That is the window between a reclaim committing a step as
    /// `failed` and `propagate_after_step` running on it — a process that dies in
    /// between leaves the run `running` with nothing that would ever revisit it, since
    /// every other sweep looks for open steps. The reclaim loop feeds these back into
    /// propagation. Bounded so one tick cannot stall behind a pathological backlog.
    ///
    /// A run holding a step whose status is outside the vocabulary (a legacy row the
    /// NOT VALID check tolerates) is left out: propagation reads it as `Pending`, so it
    /// could never finalise and would be re-selected every tick, crowding out real
    /// orphans past the limit. Such rows are the operator's to fix.
    pub async fn runs_with_no_open_steps(&self) -> Result<Vec<Uuid>> {
        Ok(sqlx::query_scalar::<_, Uuid>(
            "SELECT r.id FROM runs r
             WHERE r.status = 'running'
               AND NOT EXISTS (
                   SELECT 1 FROM step_runs s
                   WHERE s.run_id = r.id AND s.status IN ('pending', 'queued', 'running'))
               AND NOT EXISTS (
                   SELECT 1 FROM step_runs s
                   WHERE s.run_id = r.id
                     AND s.status NOT IN ('pending', 'queued', 'running',
                                          'succeeded', 'failed', 'cancelled', 'skipped'))
             ORDER BY r.started_at NULLS FIRST, r.id
             LIMIT 100",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// Unlock dependents / cascade skips / finalize the run after a step changed.
    ///
    /// Runs as one transaction with the `runs` row locked (`FOR UPDATE`) so two steps
    /// of the same run completing concurrently cannot interleave their reads and
    /// writes. Transitions are computed in memory to a fixpoint, so a chain
    /// `A(failed) → B → C → D` resolves in one call regardless of step order.
    /// Returns every step whose status changed; publishing happens in the caller,
    /// after commit. A run that no longer exists (retention or a project delete got
    /// there first) is not an error: there is nothing left to finalise.
    pub async fn propagate_after_step(&self, run_id: Uuid) -> Result<Vec<StepRun>> {
        let mut tx = self.pool.begin().await?;
        let Some(run) = sqlx::query_as::<_, Run>(AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM runs WHERE id = $1 FOR UPDATE"
        )))
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tracing::debug!(%run_id, "propagate skipped: run no longer exists");
            return Ok(Vec::new());
        };
        let snapshot_steps = run
            .definition_snapshot
            .get("steps")
            .cloned()
            .unwrap_or(json!([]));

        let mut steps = list_step_runs_on(&mut tx, run_id).await?;
        let mut changed = Vec::new();
        loop {
            let plan = plan_transitions(&steps, &snapshot_steps);
            if plan.is_empty() {
                break;
            }
            let mut progressed = false;
            for (idx, transition) in plan {
                let id = steps[idx].id;
                let updated = match transition {
                    Transition::Skip(reason) => {
                        complete_step_on(
                            &mut tx,
                            id,
                            StepStatus::Skipped,
                            None,
                            Some(reason.to_string()),
                            Some(&["pending", "queued"]),
                        )
                        .await?
                    }
                    Transition::Queue => {
                        sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
                            "UPDATE step_runs SET status = 'queued', queued_at = NOW()
                             WHERE id = $1 AND status = 'pending'
                             RETURNING {STEP_RUN_COLS}"
                        )))
                        .bind(id)
                        .fetch_optional(&mut *tx)
                        .await?
                    }
                };
                if let Some(u) = updated {
                    steps[idx] = u.clone();
                    changed.push(u);
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }

        if steps.iter().all(|s| s.status_enum().is_terminal()) {
            let tolerated = snapshot_tolerated(&snapshot_steps);
            let any_failed = steps.iter().any(|s| match s.status_enum() {
                StepStatus::Cancelled => true,
                StepStatus::Failed => !tolerated.contains(&s.step_id),
                _ => false,
            });
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
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// Cancel a run. Returns the run and any steps that were `running` (for agent Cancel fan-out).
    pub async fn cancel_run(&self, run_id: Uuid) -> Result<(Run, Vec<StepRun>)> {
        self.cancel_run_with_reason(run_id, None).await
    }

    /// Cancel a run; `reason` (e.g. "run timed out") is recorded on the cancelled steps
    /// and their open attempts so the outcome is distinguishable from a manual cancel.
    ///
    /// One transaction with the run row locked, so a cancel racing a completion cannot
    /// overwrite `succeeded` or `failed`, and a step leased between the read and the
    /// write cannot be cancelled without its agent being told. A run that is already
    /// terminal is left exactly as it is and returned with an empty step list — the
    /// caller learns the outcome from the run's status, not from an error.
    pub async fn cancel_run_with_reason(
        &self,
        run_id: Uuid,
        reason: Option<&str>,
    ) -> Result<(Run, Vec<StepRun>)> {
        let mut tx = self.pool.begin().await?;
        let run = sqlx::query_as::<_, Run>(AssertSqlSafe(format!(
            "SELECT {RUN_COLS} FROM runs WHERE id = $1 FOR UPDATE"
        )))
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow!("run not found"))?;
        // Locking the open steps blocks a concurrent lease until this commits; after
        // that the lease's `status = 'queued'` guard fails on its own. `ORDER BY id` is
        // the lock order every multi-row step lock in this file uses (see the reclaims),
        // so two of them on the same run cannot deadlock.
        let open = sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
            "SELECT {STEP_RUN_COLS} FROM step_runs
             WHERE run_id = $1 AND status IN ('pending', 'queued', 'running')
             ORDER BY id
             FOR UPDATE"
        )))
        .bind(run_id)
        .fetch_all(&mut *tx)
        .await?;
        let Some(plan) = cancel_plan(run.status_enum(), &open) else {
            tx.rollback().await?;
            return Ok((run, Vec::new()));
        };

        let cancelled = sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
            "UPDATE step_runs
             SET status = 'cancelled', finished_at = NOW(), lease_expires_at = NULL,
                 error = COALESCE($2, error)
             WHERE run_id = $1 AND status IN ('pending', 'queued', 'running')
             RETURNING {STEP_RUN_COLS}"
        )))
        .bind(run_id)
        .bind(reason)
        .fetch_all(&mut *tx)
        .await?;
        for id in &plan.close_attempts {
            finish_open_attempt_on(
                &mut tx,
                *id,
                "cancelled",
                None,
                Some(reason.unwrap_or("run cancelled")),
            )
            .await?;
        }
        let updated = sqlx::query_as::<_, Run>(AssertSqlSafe(format!(
            "UPDATE runs SET status = 'cancelled', finished_at = NOW()
             WHERE id = $1 AND status IN ('pending', 'running')
             RETURNING {RUN_COLS}"
        )))
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?;
        // A status outside the vocabulary (a legacy row the NOT VALID check tolerates)
        // parses as Pending above but matches nothing here. Leave it alone rather than
        // cancel its steps under a run that never becomes `cancelled`.
        let Some(run) = updated else {
            tx.rollback().await?;
            return Ok((run, Vec::new()));
        };
        tx.commit().await?;

        // Returned rows carry their pre-cancel `agent_id`, which is what the caller
        // needs to deliver the Cancel and release the slot.
        let notify = cancelled
            .into_iter()
            .filter(|s| plan.notify.contains(&s.id))
            .collect();
        Ok((run, notify))
    }

    /// Append many lines for one attempt of a step in a single statement.
    ///
    /// The per-line version cost a round trip each, and log ingest is by a wide margin
    /// the busiest write in the system: a 100 000-line build was 100 000 inserts on the
    /// agent socket's read loop, each one blocking the next message from that agent.
    /// `UNNEST` turns a batch into one.
    ///
    /// Append-only, like every other write to this table. `log_lines` has no unique key,
    /// so a batch the agent re-sends after a reconnect is stored twice — exactly as a
    /// re-sent `LogChunk` was, and for the same reason: the alternative is a unique index
    /// on `(step_run_id, attempt, seq)` that would cost more on every insert than
    /// duplicate lines cost anyone reading them.
    ///
    /// Returns the stored rows in insertion order.
    pub async fn append_logs(
        &self,
        run_id: Uuid,
        step_run_id: Uuid,
        attempt: i32,
        lines: &[fiber_proto::LogLineWire],
    ) -> Result<Vec<LogLine>> {
        if lines.is_empty() {
            return Ok(Vec::new());
        }
        // Borrowed, not cloned: a 500-line batch is up to 64 KB of `data`, and copying
        // it into fresh `Vec<String>`s to hand to the driver doubled that for nothing.
        let streams: Vec<&str> = lines.iter().map(|l| l.stream.as_str()).collect();
        let data: Vec<&str> = lines.iter().map(|l| l.data.as_str()).collect();
        let seqs: Vec<i64> = lines.iter().map(|l| l.seq as i64).collect();
        let mut rows = sqlx::query_as::<_, LogLine>(
            "INSERT INTO log_lines (run_id, step_run_id, stream, data, seq, attempt)
             SELECT $1, $2, t.stream, t.data, t.seq, $3
               FROM UNNEST($4::text[], $5::text[], $6::int8[])
                 WITH ORDINALITY AS t(stream, data, seq, ord)
              ORDER BY t.ord
             RETURNING id, run_id, step_run_id, stream, data, seq, created_at, attempt",
        )
        .bind(run_id)
        .bind(step_run_id)
        .bind(attempt)
        .bind(&streams)
        .bind(&data)
        .bind(&seqs)
        .fetch_all(&self.pool)
        .await?;
        // `RETURNING` order is not promised by anything; `id` is, since the sequence is
        // drawn in insertion order within the statement. Readers page by `id`.
        rows.sort_unstable_by_key(|r| r.id);
        Ok(rows)
    }

    /// Log lines for a step, oldest first **by `id`**.
    ///
    /// With `after_id` this returns the lines following that id (for polling a live
    /// step); without it, the **last** `limit` lines, which is what a viewer opening a
    /// finished step wants. `attempt` narrows to one attempt — `seq` restarts per
    /// attempt, so a retried step's output would otherwise interleave.
    ///
    /// **The last element of a page is the highest `id` in it, and that is the cursor
    /// every follower resumes from** (`fiber logs --follow`, the UI's poll). Sorting a
    /// page by anything else breaks that contract silently: the cursor rewinds to the
    /// last element's lower id and the next poll reprints everything after it. The
    /// stored order is emission order because the agent assigns `seq` where a line is
    /// read and sends system lines down the same channel as piped output.
    pub async fn list_logs(
        &self,
        step_run_id: Uuid,
        attempt: Option<i32>,
        after_id: Option<i64>,
        limit: i64,
    ) -> Result<Vec<LogLine>> {
        const COLS: &str = "id, run_id, step_run_id, stream, data, seq, created_at, attempt";
        if let Some(after) = after_id {
            return Ok(sqlx::query_as::<_, LogLine>(AssertSqlSafe(format!(
                "SELECT {COLS} FROM log_lines
                 WHERE step_run_id = $1 AND id > $2
                   AND ($3::int IS NULL OR attempt = $3)
                 ORDER BY id
                 LIMIT $4"
            )))
            .bind(step_run_id)
            .bind(after)
            .bind(attempt)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?);
        }
        let mut tail = sqlx::query_as::<_, LogLine>(AssertSqlSafe(format!(
            "SELECT {COLS} FROM log_lines
             WHERE step_run_id = $1 AND ($2::int IS NULL OR attempt = $2)
             ORDER BY id DESC
             LIMIT $3"
        )))
        .bind(step_run_id)
        .bind(attempt)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        tail.reverse();
        Ok(tail)
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
        // A step is at-least-once, so the same artifact can be uploaded twice; the second
        // upload replaces the row rather than adding a duplicate a restore would then
        // fetch twice. `id` and `created_at` stay with the first row.
        Ok(sqlx::query_as::<_, Artifact>(
            "INSERT INTO artifacts (id, run_id, step_run_id, name, path, size)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (step_run_id, name) DO UPDATE
                 SET path = EXCLUDED.path, size = EXCLUDED.size
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

    /// `(count, total bytes)` of the artifacts this step has stored under names other
    /// than `name`.
    ///
    /// `name` is excluded because a re-upload replaces its row (`ON CONFLICT` on
    /// `(step_run_id, name)`): a step that is retried must not spend its cap again on the
    /// artifact it is replacing.
    pub async fn artifact_usage_for_step(
        &self,
        step_run_id: Uuid,
        name: &str,
    ) -> Result<(i64, i64)> {
        Ok(sqlx::query_as::<_, (i64, i64)>(
            "SELECT COUNT(*)::int8, COALESCE(SUM(GREATEST(size, 0)), 0)::int8
             FROM artifacts WHERE step_run_id = $1 AND name <> $2",
        )
        .bind(step_run_id)
        .bind(name)
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

    /// What an agent's artifact download is judged on: the run's definition snapshot, the
    /// step ids this agent currently holds running in that run, and the step that produced
    /// the artifact.
    ///
    /// The decision itself is the caller's (`fiber-api` owns the `needs` closure), because
    /// it has to match the one the offer made when it chose what to restore. `None` means
    /// the artifact does not exist, or this agent holds nothing running in its run — the
    /// route answers 404 either way, so existence is not disclosed.
    pub async fn agent_artifact_access(
        &self,
        agent_id: Uuid,
        artifact_id: Uuid,
    ) -> Result<Option<ArtifactAccess>> {
        // The run snapshot and the producing step, in one row. The producer join is a
        // LEFT JOIN: an artifact whose step row is gone is unreadable, not a 500.
        let Some((snapshot, producer_step_id)) =
            sqlx::query_as::<_, (serde_json::Value, Option<String>)>(
                "SELECT r.definition_snapshot, p.step_id
                 FROM artifacts a
                 JOIN runs r ON r.id = a.run_id
                 LEFT JOIN step_runs p ON p.id = a.step_run_id
                 WHERE a.id = $1",
            )
            .bind(artifact_id)
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let holder_step_ids = sqlx::query_scalar::<_, String>(
            "SELECT s.step_id FROM step_runs s
             JOIN artifacts a ON a.run_id = s.run_id
             WHERE a.id = $1 AND s.agent_id = $2 AND s.status = 'running'",
        )
        .bind(artifact_id)
        .bind(agent_id)
        .fetch_all(&self.pool)
        .await?;
        if holder_step_ids.is_empty() {
            return Ok(None);
        }
        Ok(Some(ArtifactAccess {
            snapshot,
            holder_step_ids,
            producer_step_id,
        }))
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

    /// Delete these runs' log lines in `chunk`-sized statements, returning how many rows
    /// went.
    ///
    /// `DELETE FROM runs` cascades to `log_lines`, and a batch of a hundred runs can carry
    /// millions of rows: one statement then holds a transaction (and its `xmin`, so
    /// autovacuum cannot clean up behind it) for minutes, against the hottest table in the
    /// schema. Chunking it keeps each statement short and each lock brief; the runs are
    /// terminal and past the retention cutoff, so nothing is reading these rows.
    ///
    /// This is the one path allowed to remove append-only rows (see the retention
    /// exception in the conventions).
    pub async fn delete_log_lines_for_runs(&self, run_ids: &[Uuid], chunk: i64) -> Result<u64> {
        if run_ids.is_empty() {
            return Ok(0);
        }
        let chunk = chunk.clamp(1_000, 200_000);
        let mut total = 0u64;
        // A ceiling on the statements, not on the rows: a pathological batch must not
        // hold the retention tick for ever. What is left is deleted by the cascade.
        for _ in 0..500 {
            let res = sqlx::query(
                "DELETE FROM log_lines WHERE ctid IN (
                     SELECT ctid FROM log_lines WHERE run_id = ANY($1) LIMIT $2)",
            )
            .bind(run_ids)
            .bind(chunk)
            .execute(&self.pool)
            .await?;
            let n = res.rows_affected();
            total += n;
            if n < chunk as u64 {
                break;
            }
        }
        Ok(total)
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
            let exists: Option<(Uuid,)> = sqlx::query_as("SELECT id FROM projects WHERE id = $1")
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
        let agent = sqlx::query_as::<_, Agent>(AssertSqlSafe(format!(
            "INSERT INTO agents (id, project_id, name, labels, concurrency, token_hash)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {AGENT_COLS}"
        )))
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
            Ok(sqlx::query_as::<_, Agent>(AssertSqlSafe(format!(
                "SELECT {AGENT_COLS} FROM agents
                 WHERE project_id IS NULL OR project_id = $1
                 ORDER BY project_id NULLS LAST, created_at DESC"
            )))
            .bind(pid)
            .fetch_all(&self.pool)
            .await?)
        } else {
            Ok(sqlx::query_as::<_, Agent>(AssertSqlSafe(format!(
                "SELECT {AGENT_COLS} FROM agents ORDER BY created_at DESC"
            )))
            .fetch_all(&self.pool)
            .await?)
        }
    }

    pub async fn get_agent(&self, id: Uuid) -> Result<Option<Agent>> {
        Ok(sqlx::query_as::<_, Agent>(AssertSqlSafe(format!(
            "SELECT {AGENT_COLS} FROM agents WHERE id = $1"
        )))
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
        Ok(sqlx::query_as::<_, Agent>(AssertSqlSafe(format!(
            "UPDATE agents SET name = $2, labels = $3, concurrency = $4
             WHERE id = $1
             RETURNING {AGENT_COLS}"
        )))
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
        let agent = sqlx::query_as::<_, Agent>(AssertSqlSafe(format!(
            "UPDATE agents SET token_hash = $2, online = FALSE
             WHERE id = $1
             RETURNING {AGENT_COLS}"
        )))
        .bind(id)
        .bind(&token_hash)
        .fetch_one(&self.pool)
        .await?;
        Ok(CreateAgentResponse { agent, token })
    }

    pub async fn agent_by_token(&self, token: &str) -> Result<Option<Agent>> {
        let hash = hash_token(token);
        Ok(sqlx::query_as::<_, Agent>(AssertSqlSafe(format!(
            "SELECT {AGENT_COLS} FROM agents WHERE token_hash = $1"
        )))
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Queued steps an agent may lease: global agents see all; scoped agents see one project.
    /// Queued steps this agent may take.
    ///
    /// A run marked `untrusted` is building code from outside the project (a fork's pull
    /// request). Withholding its secrets is not enough on its own: the step still executes
    /// as the agent's user, where it can read the agent's own token out of `/proc` and
    /// then lease other projects' work. So untrusted steps are offered **only** to agents
    /// bound to that project — never to the global pool.
    ///
    /// Oldest queued first, by `queued_at` — a step requeued after a lost lease keeps its
    /// original position rather than sorting behind every never-started step — and only
    /// steps whose labels the agent satisfies (`labels <@ agent labels`; an empty
    /// requirement is contained in anything), so a long run of steps for some *other*
    /// kind of agent cannot push this agent's work past the scan limit.
    pub async fn list_queued_steps_for_pool(
        &self,
        agent_project_id: Option<Uuid>,
        agent_labels: &[String],
    ) -> Result<Vec<StepRun>> {
        if let Some(pid) = agent_project_id {
            Ok(sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
                "SELECT {STEP_RUN_COLS_S}
                 FROM step_runs s
                 INNER JOIN runs r ON r.id = s.run_id
                 WHERE s.status = 'queued' AND r.project_id = $1
                   AND (s.not_before IS NULL OR s.not_before <= NOW())
                   AND s.labels <@ $2
                 ORDER BY s.queued_at, s.id
                 LIMIT {QUEUE_SCAN_LIMIT}"
            )))
            .bind(pid)
            .bind(json!(agent_labels))
            .fetch_all(&self.pool)
            .await?)
        } else {
            self.list_queued_steps(agent_labels).await
        }
    }

    pub async fn set_agent_online(&self, id: Uuid, online: bool) -> Result<()> {
        sqlx::query("UPDATE agents SET online = $2, last_seen_at = NOW() WHERE id = $1")
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
            let Ok(def) = value_to_definition(&p.definition) else {
                continue;
            };
            let Some(on) = &def.on else {
                continue;
            };
            let Some(push) = &on.push else {
                continue;
            };
            let branch_ok = push.branches.is_empty() || push.branches.iter().any(|b| b == branch);
            if !branch_ok {
                continue;
            }
            if !crate::path_filter::paths_allow(changed_files, &push.paths, &push.paths_ignore) {
                continue;
            }
            matched.push(p);
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
            let Ok(def) = value_to_definition(&p.definition) else {
                continue;
            };
            let Some(on) = &def.on else {
                continue;
            };
            let Some(pr) = &on.pull_request else {
                continue;
            };
            let branch_ok = pr.branches.is_empty() || pr.branches.iter().any(|b| b == base_branch);
            if !branch_ok {
                continue;
            }
            let default_types = ["opened", "synchronize", "reopened"];
            let types: Vec<&str> = if pr.types.is_empty() {
                default_types.to_vec()
            } else {
                pr.types.iter().map(|s| s.as_str()).collect()
            };
            if !types.contains(&action) {
                continue;
            }
            // Without a file list, path-filtered PR pipelines cannot match.
            if (!pr.paths.is_empty() || !pr.paths_ignore.is_empty())
                && !crate::path_filter::paths_allow(changed_files, &pr.paths, &pr.paths_ignore)
            {
                continue;
            }
            matched.push(p);
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
        // Encrypted at rest with FIBER_SECRETS_KEY, same as project secrets.
        let stored = crate::secrets::encrypt_secret(secret)?;
        sqlx::query(
            "INSERT INTO webhook_secrets (id, project_id, provider, secret)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (project_id, provider)
             DO UPDATE SET secret = EXCLUDED.secret, created_at = NOW()",
        )
        .bind(id)
        .bind(project_id)
        .bind(provider)
        .bind(stored)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_webhook_secret(
        &self,
        project_id: Uuid,
        provider: &str,
    ) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT secret FROM webhook_secrets WHERE project_id = $1 AND provider = $2",
        )
        .bind(project_id)
        .bind(provider)
        .fetch_optional(&self.pool)
        .await?;
        // Rows written before encryption are plaintext; decrypt_secret passes those through.
        row.map(|(v,)| crate::secrets::decrypt_secret(&v))
            .transpose()
    }

    pub async fn ensure_admin_user(&self, username: &str, password: &str) -> Result<PublicUser> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        if count > 0 {
            // Recovery path only: promote FIBER_ADMIN_USER while the instance has *no*
            // admin. Never on every boot — anyone who can create users (project admins,
            // via member invites) could otherwise squat the name and be promoted on restart.
            let admins = self.count_instance_admins().await?;
            if let Some(u) = self.find_user_by_username(username).await? {
                if !u.is_admin && admins == 0 {
                    tracing::warn!(%username, "no instance admin existed; promoting FIBER_ADMIN_USER");
                    return self.set_instance_admin(u.id, true).await;
                }
                return Ok(u);
            }
            if admins == 0 {
                tracing::error!(
                    %username,
                    "no instance admin exists and FIBER_ADMIN_USER matches no user — set it to an existing username to recover"
                );
            }
            return Ok(sqlx::query_as::<_, PublicUser>(
                "SELECT id, username, is_admin FROM users ORDER BY created_at, id LIMIT 1",
            )
            .fetch_one(&self.pool)
            .await?);
        }
        let id = Uuid::new_v4();
        let hash = crate::tokens::hash_password(password);
        let user = sqlx::query_as::<_, PublicUser>(
            "INSERT INTO users (id, username, password_hash, is_admin) VALUES ($1, $2, $3, TRUE)
             RETURNING id, username, is_admin",
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
            "SELECT id, username, password_hash, created_at, is_admin FROM users WHERE username = $1",
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
                is_admin: user.is_admin,
            },
            expires_at,
        }))
    }

    pub async fn user_by_session_token(&self, token: &str) -> Result<Option<PublicUser>> {
        let hash = crate::tokens::hash_token(token);
        Ok(sqlx::query_as::<_, PublicUser>(
            "SELECT u.id, u.username, u.is_admin
             FROM sessions s
             JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > NOW()",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Change a user's own password, verifying the current one first.
    ///
    /// Every other session is dropped: a password change is what someone does when they
    /// think a credential is compromised, and leaving the old sessions alive would make it
    /// useless for that. The caller's own session survives, so changing a password does not
    /// log you out of the page you did it from.
    pub async fn change_password(
        &self,
        user_id: Uuid,
        current: &str,
        new_password: &str,
        keep_token: &str,
    ) -> Result<bool> {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT password_hash FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(stored) = stored else {
            return Ok(false);
        };
        if !crate::tokens::verify_password(current, &stored) {
            return Ok(false);
        }
        let hash = crate::tokens::hash_password(new_password);
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
            .bind(user_id)
            .bind(&hash)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND token_hash <> $2")
            .bind(user_id)
            .bind(crate::tokens::hash_token(keep_token))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Drop every session for a user except, optionally, the one making the request.
    ///
    /// Returns how many were removed. The remedy for a leaked token: without it the only
    /// option is waiting out the expiry.
    pub async fn revoke_sessions(&self, user_id: Uuid, keep_token: Option<&str>) -> Result<u64> {
        let res = match keep_token {
            Some(t) => {
                sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND token_hash <> $2")
                    .bind(user_id)
                    .bind(crate::tokens::hash_token(t))
                    .execute(&self.pool)
                    .await?
            }
            None => {
                sqlx::query("DELETE FROM sessions WHERE user_id = $1")
                    .bind(user_id)
                    .execute(&self.pool)
                    .await?
            }
        };
        Ok(res.rows_affected())
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

    pub async fn list_secret_values(&self, project_id: Uuid) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT key, value FROM project_secrets WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for (key, value) in rows {
            // Typed, not `.context()`: the caller decides whether to retry on the
            // error's kind, and a decrypt failure is the one kind that never clears.
            let plain = crate::secrets::decrypt_secret(&value).map_err(|source| {
                anyhow::Error::from(crate::SecretDecryptError {
                    name: key.clone(),
                    source,
                })
            })?;
            out.push((key, plain));
        }
        Ok(out)
    }

    pub async fn get_secret_plain(&self, project_id: Uuid, key: &str) -> Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM project_secrets WHERE project_id = $1 AND key = $2")
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

/// Step ids the snapshot marks `continue_on_error`.
///
/// Read from the snapshot rather than a column: the snapshot is what governs this
/// execution, so editing the pipeline mid-run cannot change whether a failure is tolerated.
fn snapshot_tolerated(steps: &Value) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    if let Some(arr) = steps.as_array() {
        for s in arr {
            if s.get("continue_on_error").and_then(Value::as_bool) == Some(true)
                && let Some(id) = s.get("id").and_then(|v| v.as_str())
            {
                out.insert(id.to_string());
            }
        }
    }
    out
}

fn snapshot_step_if_env(steps: &Value, step_id: &str) -> (Option<String>, Vec<(String, String)>) {
    let Some(arr) = steps.as_array() else {
        return (None, vec![]);
    };
    for s in arr {
        if s.get("id").and_then(|v| v.as_str()) != Some(step_id) {
            continue;
        }
        let if_expr = s.get("if").and_then(|v| v.as_str()).map(|s| s.to_string());
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

const RUN_COLS: &str = "id, pipeline_id, project_id, status, trigger, definition_snapshot, \
     created_at, started_at, finished_at, retry_of, head_sha, head_ref, pr_number, \
     repo_full_name, untrusted, concurrency_group";

const STEP_RUN_COLS: &str = "id, run_id, step_id, step_name, status, image, run_cmd, labels, needs, \
     retries, attempt, agent_id, lease_expires_at, exit_code, error, started_at, finished_at";

/// `STEP_RUN_COLS` qualified with the `s` alias, for the queue queries that join `runs`.
const STEP_RUN_COLS_S: &str = "s.id, s.run_id, s.step_id, s.step_name, s.status, s.image, \
     s.run_cmd, s.labels, s.needs, s.retries, s.attempt, s.agent_id, s.lease_expires_at, \
     s.exit_code, s.error, s.started_at, s.finished_at";

/// Queued steps read per offer. An offer leases the first one it can, so the scan only
/// has to be deep enough to get past steps another replica leases in the same instant;
/// unbounded, each heartbeat of each agent transferred the whole backlog.
const QUEUE_SCAN_LIMIT: i64 = 200;

/// How long a step whose offer could not be built waits before it is leased again.
/// Longer than a heartbeat, so the retry is not the very next one; short enough that
/// a database blip costs one heartbeat's worth of delay.
const OFFER_RETRY_BACKOFF_SECS: i64 = 30;

async fn list_step_runs_on(conn: &mut sqlx::PgConnection, run_id: Uuid) -> Result<Vec<StepRun>> {
    Ok(sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
        "SELECT {STEP_RUN_COLS} FROM step_runs WHERE run_id = $1 ORDER BY step_id"
    )))
    .bind(run_id)
    .fetch_all(conn)
    .await?)
}

/// Mark a step terminal and close its open attempt. With `expected`, the write only
/// happens while the step is still in one of those statuses (returns `None` otherwise),
/// so a concurrent lease or completion is never overwritten.
async fn complete_step_on(
    conn: &mut sqlx::PgConnection,
    step_run_id: Uuid,
    status: StepStatus,
    exit_code: Option<i32>,
    error: Option<String>,
    expected: Option<&[&str]>,
) -> Result<Option<StepRun>> {
    let guard = expected.map(|e| e.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let sr = sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
        "UPDATE step_runs
         SET status = $2, exit_code = $3, error = $4, finished_at = NOW(), lease_expires_at = NULL
         WHERE id = $1 AND ($5::text[] IS NULL OR status = ANY($5))
         RETURNING {STEP_RUN_COLS}"
    )))
    .bind(step_run_id)
    .bind(step_status_str(status))
    .bind(exit_code)
    .bind(&error)
    .bind(guard)
    .fetch_optional(&mut *conn)
    .await?;
    if sr.is_some() {
        finish_open_attempt_on(
            conn,
            step_run_id,
            step_status_str(status),
            exit_code,
            error.as_deref(),
        )
        .await?;
    }
    Ok(sr)
}

async fn finish_open_attempt_on(
    conn: &mut sqlx::PgConnection,
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
    .execute(conn)
    .await?;
    Ok(())
}

/// Lock a project's owner rows for the rest of the transaction. Every owner-count check
/// runs after this, so concurrent owner changes serialise and each sees the other's
/// committed result rather than a snapshot in which both still counted two owners.
async fn lock_project_owners_on(conn: &mut sqlx::PgConnection, project_id: Uuid) -> Result<()> {
    sqlx::query(
        "SELECT 1 FROM project_members WHERE project_id = $1 AND role = 'owner'
         ORDER BY user_id FOR UPDATE",
    )
    .bind(project_id)
    .execute(conn)
    .await?;
    Ok(())
}

async fn insert_step_attempt_on(
    conn: &mut sqlx::PgConnection,
    step_run_id: Uuid,
    attempt: i32,
    agent_id: Option<Uuid>,
    status: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO step_attempts
             (id, step_run_id, attempt, agent_id, status, queue_wait_seconds)
         SELECT $1, $2, $3, $4, $5,
                EXTRACT(EPOCH FROM (NOW() - s.queued_at))::float8
           FROM step_runs s WHERE s.id = $2",
    )
    .bind(Uuid::new_v4())
    .bind(step_run_id)
    .bind(attempt)
    .bind(agent_id)
    .bind(status)
    .execute(conn)
    .await?;
    Ok(())
}

/// A run that was just created, with everything the caller needs to enqueue it and to
/// cancel what it supersedes. `superseded` was decided inside the creating transaction,
/// under the group lock, so it is exact rather than a best-effort snapshot taken later.
#[derive(Debug)]
pub struct StartedRun {
    pub run: Run,
    pub steps: Vec<StepRun>,
    pub dag: CompiledDag,
    /// Older unfinished runs of the same concurrency group. The store does not cancel
    /// them — cancelling takes the run and step locks, and doing that inside the start
    /// transaction would hold the group lock across every agent notification.
    pub superseded: Vec<Uuid>,
}

/// Serialise run starts within one concurrency group for the rest of the transaction.
///
/// A transaction-scoped advisory lock keyed on `project:group`: released on commit or
/// rollback, so a crashed start cannot wedge the group. `hashtext` folds the string to
/// the 32-bit key space; a collision between two unrelated groups only makes their
/// starts wait on each other, never miss each other.
async fn lock_concurrency_group_on(
    conn: &mut sqlx::PgConnection,
    project_id: Uuid,
    group: &str,
) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("{project_id}:{group}"))
        .execute(conn)
        .await?;
    Ok(())
}

/// The unfinished runs `run` supersedes, read on the creating connection so they are
/// exactly the runs that were committed when `run` took the group lock. The SQL narrows
/// to the group (the partial index in migration 014); [`superseded`] makes the decision.
async fn superseded_runs_on(conn: &mut sqlx::PgConnection, run: &Run) -> Result<Vec<Uuid>> {
    let Some(group) = run.concurrency_group.as_deref() else {
        return Ok(Vec::new());
    };
    let candidates = sqlx::query_as::<_, RunKey>(
        "SELECT id, project_id, concurrency_group, status, created_at FROM runs
          WHERE project_id = $1
            AND concurrency_group = $2
            AND status NOT IN ('succeeded', 'failed', 'cancelled', 'skipped')
          ORDER BY created_at, id",
    )
    .bind(run.project_id)
    .bind(group)
    .fetch_all(conn)
    .await?;
    let new = RunKey::from(run);
    Ok(candidates
        .into_iter()
        .filter(|c| superseded(&new, c))
        .map(|c| c.id)
        .collect())
}

/// The part of a run that decides whether it is superseded by another.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RunKey {
    pub id: Uuid,
    pub project_id: Uuid,
    pub concurrency_group: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

impl From<&Run> for RunKey {
    fn from(run: &Run) -> Self {
        Self {
            id: run.id,
            project_id: run.project_id,
            concurrency_group: run.concurrency_group.clone(),
            status: run.status.clone(),
            created_at: run.created_at,
        }
    }
}

/// Whether starting `new` cancels `candidate`.
///
/// Same project, same resolved group (a run with none contends with nothing), still
/// unfinished, not `new` itself, and older by `(created_at, id)`. Ordering on the pair
/// makes the decision total: when two runs carry the same timestamp exactly one of
/// them is older, so they cannot cancel each other and leave the group empty.
pub fn superseded(new: &RunKey, candidate: &RunKey) -> bool {
    let Some(group) = new.concurrency_group.as_deref() else {
        return false;
    };
    candidate.id != new.id
        && candidate.project_id == new.project_id
        && candidate.concurrency_group.as_deref() == Some(group)
        && !run_status_terminal(&candidate.status)
        && (candidate.created_at, candidate.id) < (new.created_at, new.id)
}

fn run_status_terminal(status: &str) -> bool {
    matches!(status, "succeeded" | "failed" | "cancelled" | "skipped")
}

/// What a reclaim (expired lease, agent disconnect) did to the steps it found running.
#[derive(Debug, Default)]
pub struct Reclaimed {
    /// Back in the queue for another attempt; the caller re-enqueues and publishes them.
    pub requeued: Vec<StepRun>,
    /// Out of attempts and now `failed`; their runs have already been propagated.
    pub failed: Vec<StepRun>,
    /// Every step that propagation changed for those runs (skipped dependents), in
    /// addition to `failed`. Not published by the store — the caller owns events.
    pub propagated: Vec<StepRun>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequeueOutcome {
    Requeue,
    Fail,
}

/// Whether a step whose lease was lost gets another attempt. `attempt` is the one just
/// lost (leases increment it). A lost lease counts against the budget like a reported
/// failure does, with one extra try: `retries + 2` leases in total, where `retry_plan`
/// allows `retries + 1`. The extra one is for a rolling agent restart or a network
/// blip, which a `retries: 0` step must survive once — while a step that kills its
/// agent every time still stops after two leases instead of being re-leased forever.
fn requeue_outcome(attempt: i32, retries: i32) -> RequeueOutcome {
    if attempt > retries + 1 {
        RequeueOutcome::Fail
    } else {
        RequeueOutcome::Requeue
    }
}

/// Apply [`requeue_outcome`] to `candidates` (already locked by the caller) and close
/// their open attempts, all on one connection so a crash cannot leave an attempt open
/// against a step that is back in the queue.
async fn reclaim_steps_on(
    conn: &mut sqlx::PgConnection,
    candidates: &[StepRun],
    reason: &str,
) -> Result<Reclaimed> {
    let (requeue_ids, fail_ids): (Vec<Uuid>, Vec<Uuid>) =
        candidates
            .iter()
            .fold((Vec::new(), Vec::new()), |(mut r, mut f), s| {
                match requeue_outcome(s.attempt, s.retries) {
                    RequeueOutcome::Requeue => r.push(s.id),
                    RequeueOutcome::Fail => f.push(s.id),
                }
                (r, f)
            });
    let mut out = Reclaimed::default();
    if !requeue_ids.is_empty() {
        out.requeued = sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
            "UPDATE step_runs
             SET status = 'queued', agent_id = NULL, lease_expires_at = NULL, queued_at = NOW()
             WHERE id = ANY($1) AND status = 'running'
             RETURNING {STEP_RUN_COLS}"
        )))
        .bind(&requeue_ids)
        .fetch_all(&mut *conn)
        .await?;
    }
    if !fail_ids.is_empty() {
        // `agent_id` stays: the row records which agent lost the last attempt.
        out.failed = sqlx::query_as::<_, StepRun>(AssertSqlSafe(format!(
            "UPDATE step_runs
             SET status = 'failed', lease_expires_at = NULL, finished_at = NOW(),
                 error = 'lease lost after ' || attempt
                         || CASE WHEN attempt = 1 THEN ' attempt' ELSE ' attempts' END
             WHERE id = ANY($1) AND status = 'running'
             RETURNING {STEP_RUN_COLS}"
        )))
        .bind(&fail_ids)
        .fetch_all(&mut *conn)
        .await?;
    }
    for s in out.requeued.iter().chain(out.failed.iter()) {
        finish_open_attempt_on(&mut *conn, s.id, "reclaimed", None, Some(reason)).await?;
    }
    Ok(out)
}

/// What a cancel has to do to a run, decided from the run's status and its open steps.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct CancelPlan {
    /// Steps whose open `step_attempts` row must be closed: everything that was running.
    close_attempts: Vec<Uuid>,
    /// Steps an agent is executing right now — the ones that need a Cancel delivered and
    /// a slot released. A subset of `close_attempts`.
    notify: Vec<Uuid>,
}

/// Pure cancel decision. `None` means the run is already terminal and nothing may be
/// written: a cancel that arrives after a completion must leave `succeeded` / `failed`
/// alone, or GitHub sees `success` followed by `error` for the same commit.
fn cancel_plan(run_status: RunStatus, open_steps: &[StepRun]) -> Option<CancelPlan> {
    if run_status.is_terminal() {
        return None;
    }
    let running: Vec<&StepRun> = open_steps
        .iter()
        .filter(|s| s.status_enum() == StepStatus::Running)
        .collect();
    Some(CancelPlan {
        close_attempts: running.iter().map(|s| s.id).collect(),
        notify: running
            .iter()
            .filter(|s| s.agent_id.is_some())
            .map(|s| s.id)
            .collect(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transition {
    Skip(&'static str),
    Queue,
}

/// Pure DAG bookkeeping for one pass over a run's steps: which pending/queued steps
/// must be skipped because a dependency failed or was skipped, and which pending
/// steps have all dependencies terminal and may be queued (subject to `if:`).
/// Callers apply the result and re-plan until nothing changes.
fn plan_transitions(steps: &[StepRun], snapshot_steps: &Value) -> Vec<(usize, Transition)> {
    use std::collections::HashSet;
    // `continue_on_error` means the failure is recorded but not propagated: the step's own
    // row still says failed, and everything downstream proceeds as if it had not. A cancel
    // is never tolerated — that is an operator stopping the run, not the step's own outcome.
    let tolerated = snapshot_tolerated(snapshot_steps);
    let failed: HashSet<&str> = steps
        .iter()
        .filter(|s| match s.status_enum() {
            StepStatus::Cancelled => true,
            StepStatus::Failed => !tolerated.contains(&s.step_id),
            _ => false,
        })
        .map(|s| s.step_id.as_str())
        .collect();
    let succeeded: HashSet<&str> = steps
        .iter()
        .filter(|s| match s.status_enum() {
            StepStatus::Succeeded => true,
            StepStatus::Failed => tolerated.contains(&s.step_id),
            _ => false,
        })
        .map(|s| s.step_id.as_str())
        .collect();
    let terminal: HashSet<&str> = steps
        .iter()
        .filter(|s| s.status_enum().is_terminal())
        .map(|s| s.step_id.as_str())
        .collect();
    // `success()` is transitive (GitHub Actions semantics): a failure anywhere in a
    // step's ancestry taints it, even through a succeeded `always()` step in between.
    let mut tainted: HashSet<&str> = failed.clone();
    loop {
        let before = tainted.len();
        for s in steps {
            if !tainted.contains(s.step_id.as_str())
                && s.needs_vec().iter().any(|n| tainted.contains(n.as_str()))
            {
                tainted.insert(s.step_id.as_str());
            }
        }
        if tainted.len() == before {
            break;
        }
    }

    let mut plan = Vec::new();
    for (idx, s) in steps.iter().enumerate() {
        let needs = s.needs_vec();
        match s.status_enum() {
            StepStatus::Queued => {
                // A queued step only sees a newly failed dependency after a cancel;
                // `always()` steps stay queued, as they would have when first planned.
                let (if_expr, _) = snapshot_step_if_env(snapshot_steps, &s.step_id);
                let always = if_expr.as_deref().map(str::trim) == Some("always()");
                if !always && needs.iter().any(|n| tainted.contains(n.as_str())) {
                    plan.push((idx, Transition::Skip("dependency failed")));
                }
            }
            StepStatus::Pending => {
                let (if_expr, env) = snapshot_step_if_env(snapshot_steps, &s.step_id);
                let always = if_expr.as_deref().map(str::trim) == Some("always()");
                // Fail-fast cascade — except for `always()` steps, which wait for their
                // dependencies to finish and then run regardless of the outcome.
                if !always && needs.iter().any(|n| tainted.contains(n.as_str())) {
                    plan.push((idx, Transition::Skip("dependency failed")));
                    continue;
                }
                if !needs.iter().all(|n| terminal.contains(n.as_str())) {
                    continue;
                }
                let needs_succeeded = needs
                    .iter()
                    .all(|n| succeeded.contains(n.as_str()) && !tainted.contains(n.as_str()));
                if !always && !needs_succeeded {
                    plan.push((idx, Transition::Skip("dependency skipped or failed")));
                    continue;
                }
                let ctx = crate::step_if::IfContext {
                    needs_succeeded,
                    env,
                };
                if !crate::step_if::eval_if(if_expr.as_deref(), &ctx) {
                    plan.push((idx, Transition::Skip("if: condition false")));
                } else {
                    plan.push((idx, Transition::Queue));
                }
            }
            _ => {}
        }
    }
    plan
}

#[cfg(test)]
mod propagate_tests {
    use super::*;

    fn step(id: &str, status: StepStatus, needs: &[&str]) -> StepRun {
        StepRun {
            id: Uuid::new_v4(),
            run_id: Uuid::nil(),
            step_id: id.into(),
            step_name: id.into(),
            status: step_status_str(status).into(),
            image: None,
            run_cmd: "true".into(),
            labels: json!([]),
            needs: json!(needs),
            retries: 0,
            attempt: 0,
            agent_id: None,
            lease_expires_at: None,
            exit_code: None,
            error: None,
            started_at: None,
            finished_at: None,
        }
    }

    fn apply(steps: &mut [StepRun], plan: &[(usize, Transition)]) {
        for (idx, t) in plan {
            steps[*idx].status = match t {
                Transition::Skip(_) => "skipped".into(),
                Transition::Queue => "queued".into(),
            };
        }
    }

    fn run_to_fixpoint(steps: &mut [StepRun], snap: &Value) -> usize {
        let mut passes = 0;
        loop {
            let plan = plan_transitions(steps, snap);
            if plan.is_empty() {
                return passes;
            }
            apply(steps, &plan);
            passes += 1;
        }
    }

    #[test]
    fn queues_when_all_needs_succeeded() {
        let mut steps = vec![
            step("a", StepStatus::Succeeded, &[]),
            step("b", StepStatus::Pending, &["a"]),
        ];
        let plan = plan_transitions(&steps, &json!([]));
        assert_eq!(plan, vec![(1, Transition::Queue)]);
        apply(&mut steps, &plan);
        assert!(plan_transitions(&steps, &json!([])).is_empty());
    }

    #[test]
    fn failure_cascades_through_a_chain_in_one_call() {
        // A(failed) → B → C → D: the old per-pass code could leave D pending forever.
        let mut steps = vec![
            step("a", StepStatus::Failed, &[]),
            step("b", StepStatus::Pending, &["a"]),
            step("c", StepStatus::Pending, &["b"]),
            step("d", StepStatus::Pending, &["c"]),
        ];
        run_to_fixpoint(&mut steps, &json!([]));
        assert!(
            steps[1..].iter().all(|s| s.status == "skipped"),
            "{steps:?}"
        );
    }

    #[test]
    fn always_runs_after_failure_and_success_gate_skips() {
        let snap = json!([
            {"id": "cleanup", "if": "always()"},
            {"id": "deploy"}
        ]);
        let mut steps = vec![
            step("build", StepStatus::Failed, &[]),
            step("cleanup", StepStatus::Pending, &["build"]),
            step("deploy", StepStatus::Pending, &["build"]),
        ];
        run_to_fixpoint(&mut steps, &snap);
        assert_eq!(steps[1].status, "queued");
        assert_eq!(steps[2].status, "skipped");
    }

    #[test]
    fn failure_is_transitive_through_a_succeeded_always_step() {
        // build(failed) → cleanup(always, succeeded) → deploy(default) must NOT run;
        // a further always() step after cleanup still does.
        let snap = json!([
            {"id": "cleanup", "if": "always()"},
            {"id": "deploy"},
            {"id": "notify", "if": "always()"}
        ]);
        let mut steps = vec![
            step("build", StepStatus::Failed, &[]),
            step("cleanup", StepStatus::Succeeded, &["build"]),
            step("deploy", StepStatus::Pending, &["cleanup"]),
            step("notify", StepStatus::Pending, &["cleanup"]),
        ];
        run_to_fixpoint(&mut steps, &snap);
        assert_eq!(steps[2].status, "skipped");
        assert_eq!(steps[3].status, "queued");
    }

    #[test]
    fn waits_while_a_need_is_still_running_and_skips_on_if_false() {
        let snap = json!([{"id": "gated", "if": "never()"}]);
        let mut steps = vec![
            step("a", StepStatus::Running, &[]),
            step("b", StepStatus::Pending, &["a"]),
            step("gated", StepStatus::Pending, &[]),
        ];
        let plan = plan_transitions(&steps, &snap);
        assert_eq!(plan, vec![(2, Transition::Skip("if: condition false"))]);
        apply(&mut steps, &plan);
        steps[0].status = "succeeded".into();
        assert_eq!(
            plan_transitions(&steps, &snap),
            vec![(1, Transition::Queue)]
        );
    }

    #[test]
    fn a_tolerated_failure_does_not_block_dependents() {
        let snap = json!([
            {"id": "lint", "continue_on_error": true},
            {"id": "build"}
        ]);
        let mut steps = vec![
            step("lint", StepStatus::Failed, &[]),
            step("build", StepStatus::Pending, &["lint"]),
        ];
        // Without the flag this same shape skips `build`.
        assert_eq!(
            plan_transitions(&steps, &json!([{"id": "lint"}, {"id": "build"}])),
            vec![(1, Transition::Skip("dependency failed"))]
        );
        assert_eq!(
            plan_transitions(&steps, &snap),
            vec![(1, Transition::Queue)]
        );
        run_to_fixpoint(&mut steps, &snap);
        assert_eq!(steps[1].status, "queued");
        // The failure is still on the record: tolerating it is not hiding it.
        assert_eq!(steps[0].status, "failed");
    }

    #[test]
    fn tolerance_does_not_travel_down_the_chain() {
        // `build` is tolerated; `test` fails for its own reasons and must still cascade.
        let snap = json!([
            {"id": "build", "continue_on_error": true},
            {"id": "test"},
            {"id": "deploy"}
        ]);
        let mut steps = vec![
            step("build", StepStatus::Failed, &[]),
            step("test", StepStatus::Failed, &["build"]),
            step("deploy", StepStatus::Pending, &["test"]),
        ];
        run_to_fixpoint(&mut steps, &snap);
        assert_eq!(steps[2].status, "skipped");
    }

    #[test]
    fn a_cancel_is_never_tolerated() {
        // continue_on_error is about the step's own outcome. An operator stopping the run
        // is not that, and must still stop everything downstream.
        let snap = json!([{"id": "a", "continue_on_error": true}, {"id": "b"}]);
        let steps = vec![
            step("a", StepStatus::Cancelled, &[]),
            step("b", StepStatus::Pending, &["a"]),
        ];
        assert_eq!(
            plan_transitions(&steps, &snap),
            vec![(1, Transition::Skip("dependency failed"))]
        );
    }

    #[test]
    fn a_tolerated_failure_satisfies_a_success_gate() {
        // `success()` is the default gate, and it is transitive. A tolerated failure has to
        // count as success there, or the flag would let a step queue and then skip anyway.
        let snap = json!([
            {"id": "flaky", "continue_on_error": true},
            {"id": "after", "if": "success()"}
        ]);
        let steps = vec![
            step("flaky", StepStatus::Failed, &[]),
            step("after", StepStatus::Pending, &["flaky"]),
        ];
        assert_eq!(
            plan_transitions(&steps, &snap),
            vec![(1, Transition::Queue)]
        );
    }

    #[test]
    fn queued_step_whose_dependency_failed_is_skipped() {
        let steps = vec![
            step("a", StepStatus::Cancelled, &[]),
            step("b", StepStatus::Queued, &["a"]),
        ];
        assert_eq!(
            plan_transitions(&steps, &json!([])),
            vec![(1, Transition::Skip("dependency failed"))]
        );
    }
}

#[cfg(test)]
mod snapshot_tests {
    //! The run's definition snapshot governs its execution (convention 4). These helpers
    //! are how the execution path reads it, so a bug here means a run behaves according to
    //! something other than what it snapshotted.
    use super::*;

    fn steps(v: Value) -> Value {
        v
    }

    #[test]
    fn tolerated_steps_come_from_the_snapshot() {
        let snap = steps(json!([
            {"id": "a", "continue_on_error": true},
            {"id": "b", "continue_on_error": false},
            {"id": "c"},
        ]));
        let tolerated = snapshot_tolerated(&snap);
        assert!(tolerated.contains("a"));
        assert!(!tolerated.contains("b"), "explicit false is not tolerated");
        assert!(!tolerated.contains("c"), "absent means not tolerated");
    }

    #[test]
    fn a_non_boolean_continue_on_error_does_not_tolerate() {
        // Only a real `true` tolerates a failure; a truthy-looking string must not.
        let snap = steps(json!([
            {"id": "a", "continue_on_error": "true"},
            {"id": "b", "continue_on_error": 1},
            {"id": "c", "continue_on_error": null},
        ]));
        assert!(snapshot_tolerated(&snap).is_empty());
    }

    #[test]
    fn a_malformed_snapshot_tolerates_nothing() {
        // Failing open here would let a failure pass silently through a broken snapshot.
        assert!(snapshot_tolerated(&json!({})).is_empty());
        assert!(snapshot_tolerated(&json!(null)).is_empty());
        assert!(snapshot_tolerated(&json!([{"no_id": true}])).is_empty());
    }

    #[test]
    fn a_steps_if_and_env_are_read_from_its_snapshot_entry() {
        let snap = steps(json!([
            {"id": "a", "if": "always()", "env": [["K", "v"], ["K2", "v2"]]},
            {"id": "b", "if": "never()"},
        ]));
        let (if_expr, env) = snapshot_step_if_env(&snap, "a");
        assert_eq!(if_expr.as_deref(), Some("always()"));
        assert_eq!(
            env,
            vec![
                ("K".to_string(), "v".to_string()),
                ("K2".to_string(), "v2".to_string())
            ]
        );
    }

    #[test]
    fn a_matrix_binding_is_appended_after_env_so_it_wins() {
        // compile-time precedence is pipeline < step env < matrix binding; the later
        // entry is the one that survives being applied in order.
        let snap = steps(json!([
            {"id": "a", "env": [["os", "from-env"]], "matrix": {"os": "linux"}},
        ]));
        let (_, env) = snapshot_step_if_env(&snap, "a");
        assert_eq!(
            env,
            vec![
                ("os".to_string(), "from-env".to_string()),
                ("os".to_string(), "linux".to_string()),
            ],
            "the matrix binding must come last"
        );
        assert_eq!(env.last().unwrap().1, "linux");
    }

    #[test]
    fn an_unknown_step_id_yields_no_condition_and_no_env() {
        let snap = steps(json!([{"id": "a", "if": "always()"}]));
        assert_eq!(snapshot_step_if_env(&snap, "missing"), (None, vec![]));
        assert_eq!(snapshot_step_if_env(&json!(null), "a"), (None, vec![]));
    }

    #[test]
    fn malformed_env_entries_are_skipped_not_guessed_at() {
        let snap = steps(json!([
            {"id": "a", "env": [["K", "v"], ["only-one"], ["K2", 5], "not-a-pair"]},
        ]));
        let (_, env) = snapshot_step_if_env(&snap, "a");
        assert_eq!(env, vec![("K".to_string(), "v".to_string())]);
    }

    #[test]
    fn a_non_string_matrix_value_is_skipped() {
        let snap = steps(json!([{"id": "a", "matrix": {"n": 3, "os": "linux"}}]));
        let (_, env) = snapshot_step_if_env(&snap, "a");
        assert_eq!(env, vec![("os".to_string(), "linux".to_string())]);
    }

    #[test]
    fn the_status_column_spelling_matches_the_wire_spelling() {
        // `status_str` is written by hand while the enum serialises itself via serde. If
        // they drift, the database column and the JSON the UI receives disagree.
        for s in [
            RunStatus::Pending,
            RunStatus::Running,
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            assert_eq!(
                json!(status_str(s)),
                serde_json::to_value(s).unwrap(),
                "{s:?}"
            );
        }
        for s in [
            StepStatus::Pending,
            StepStatus::Queued,
            StepStatus::Running,
            StepStatus::Succeeded,
            StepStatus::Failed,
            StepStatus::Cancelled,
            StepStatus::Skipped,
        ] {
            assert_eq!(
                json!(step_status_str(s)),
                serde_json::to_value(s).unwrap(),
                "{s:?}"
            );
        }
    }

    #[test]
    fn a_definition_is_read_from_either_stored_shape() {
        // Older rows store `{"yaml": "..."}`; newer ones store the definition as JSON.
        let from_json = value_to_definition(&json!({
            "name": "p",
            "steps": [{"id": "a", "name": "A", "run": "true"}],
        }))
        .unwrap();
        assert_eq!(from_json.name, "p");
        assert_eq!(from_json.steps.len(), 1);

        let from_yaml = value_to_definition(&json!({
            "yaml": "name: p\nsteps:\n  - id: a\n    name: A\n    run: 'true'\n",
        }))
        .unwrap();
        assert_eq!(from_yaml.name, "p");
        assert_eq!(from_yaml.steps.len(), 1);
    }

    #[test]
    fn a_definition_that_is_neither_shape_is_an_error() {
        assert!(value_to_definition(&json!({"nonsense": true})).is_err());
        assert!(value_to_definition(&json!({"yaml": "steps: [oops"})).is_err());
    }
}

#[cfg(test)]
mod concurrency_tests {
    //! The group is the whole decision: two runs contend exactly when their resolved
    //! strings match, so what the template expands to is what cancels what.

    use super::resolve_concurrency_group;
    use uuid::Uuid;

    #[test]
    fn the_default_group_separates_branches_of_one_pipeline() {
        let p = Uuid::new_v4();
        let main = resolve_concurrency_group(None, p, Some("main"));
        let feature = resolve_concurrency_group(None, p, Some("feature/x"));
        assert_ne!(
            main, feature,
            "a push to one branch must not cancel another"
        );
        assert_eq!(main, resolve_concurrency_group(None, p, Some("main")));
    }

    #[test]
    fn the_default_group_separates_pipelines_on_one_branch() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert_ne!(
            resolve_concurrency_group(None, a, Some("main")),
            resolve_concurrency_group(None, b, Some("main")),
        );
    }

    #[test]
    fn manual_runs_of_one_pipeline_contend_with_each_other() {
        // No ref, so `{ref}` is empty — which is the point: pressing Run twice should
        // supersede, not race.
        let p = Uuid::new_v4();
        assert_eq!(
            resolve_concurrency_group(None, p, None),
            resolve_concurrency_group(None, p, None),
        );
    }

    #[test]
    fn a_literal_group_makes_every_pipeline_that_names_it_contend() {
        // The escape hatch for "only one deploy at a time, whatever started it".
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(
            resolve_concurrency_group(Some("deploy"), a, Some("main")),
            resolve_concurrency_group(Some("deploy"), b, Some("release")),
        );
    }

    #[test]
    fn a_template_may_mix_placeholders_and_literals() {
        let p = Uuid::new_v4();
        let g = resolve_concurrency_group(Some("deploy-{ref}"), p, Some("main"));
        assert_eq!(g, "deploy-main");
    }

    #[test]
    fn an_unknown_placeholder_is_left_alone_rather_than_emptied() {
        // Eating it would collapse what the author meant as many groups into one, and
        // silently serialise builds that were supposed to run side by side.
        let p = Uuid::new_v4();
        let g = resolve_concurrency_group(Some("{pipeline}-{version}"), p, Some("main"));
        assert!(g.ends_with("-{version}"), "got {g}");
        assert!(g.starts_with(&p.to_string()));
    }

    // --- what a new run cancels ---------------------------------------------------------
    //
    // `superseded` is the decision `start_run` / `retry_run` apply, under the group lock,
    // to every unfinished run of the group. The SQL narrows to the group; this is what
    // says which of those go.

    use super::{RunKey, superseded};
    use chrono::{DateTime, Duration, Utc};

    fn key(project: Uuid, group: Option<&str>, status: &str, created_at: DateTime<Utc>) -> RunKey {
        RunKey {
            id: Uuid::new_v4(),
            project_id: project,
            concurrency_group: group.map(str::to_string),
            status: status.to_string(),
            created_at,
        }
    }

    #[test]
    fn an_older_unfinished_run_of_the_same_group_is_superseded() {
        let p = Uuid::new_v4();
        let t = Utc::now();
        let new = key(p, Some("main"), "running", t);
        for status in ["pending", "running"] {
            let older = key(p, Some("main"), status, t - Duration::seconds(1));
            assert!(superseded(&new, &older), "{status} must be superseded");
        }
    }

    #[test]
    fn a_finished_run_is_left_alone() {
        // Cancelling it would rewrite an outcome GitHub already saw.
        let p = Uuid::new_v4();
        let t = Utc::now();
        let new = key(p, Some("main"), "running", t);
        for status in ["succeeded", "failed", "cancelled", "skipped"] {
            let older = key(p, Some("main"), status, t - Duration::seconds(1));
            assert!(!superseded(&new, &older), "{status} must not be superseded");
        }
    }

    #[test]
    fn a_run_never_supersedes_itself() {
        let p = Uuid::new_v4();
        let new = key(p, Some("main"), "running", Utc::now());
        assert!(!superseded(&new, &new));
    }

    #[test]
    fn a_newer_run_is_not_superseded_by_an_older_one() {
        // The direction matters: a late-committing older run must not cancel the newer
        // one that already superseded it.
        let p = Uuid::new_v4();
        let t = Utc::now();
        let old = key(p, Some("main"), "running", t - Duration::seconds(1));
        let newer = key(p, Some("main"), "running", t);
        assert!(!superseded(&old, &newer));
    }

    #[test]
    fn the_same_instant_is_broken_by_id_so_exactly_one_wins() {
        let p = Uuid::new_v4();
        let t = Utc::now();
        let a = key(p, Some("main"), "running", t);
        let b = key(p, Some("main"), "running", t);
        assert_ne!(
            superseded(&a, &b),
            superseded(&b, &a),
            "two runs with one timestamp must not cancel each other, nor neither"
        );
    }

    #[test]
    fn another_group_project_or_no_group_does_not_contend() {
        let p = Uuid::new_v4();
        let t = Utc::now();
        let new = key(p, Some("main"), "running", t);
        let earlier = t - Duration::seconds(1);
        assert!(!superseded(
            &new,
            &key(p, Some("release"), "running", earlier)
        ));
        assert!(!superseded(
            &new,
            &key(Uuid::new_v4(), Some("main"), "running", earlier)
        ));
        assert!(!superseded(&new, &key(p, None, "running", earlier)));
        // A run without a group contends with nothing, whatever is older.
        let ungrouped = key(p, None, "running", t);
        assert!(!superseded(&ungrouped, &key(p, None, "running", earlier)));
        assert!(!superseded(
            &ungrouped,
            &key(p, Some("main"), "running", earlier)
        ));
    }
}

#[cfg(test)]
mod transition_tests {
    use super::*;

    fn step(status: StepStatus, agent: bool) -> StepRun {
        StepRun {
            id: Uuid::new_v4(),
            run_id: Uuid::nil(),
            step_id: "s".into(),
            step_name: "s".into(),
            status: step_status_str(status).into(),
            image: None,
            run_cmd: "true".into(),
            labels: json!([]),
            needs: json!([]),
            retries: 0,
            attempt: 1,
            agent_id: agent.then(Uuid::new_v4),
            lease_expires_at: None,
            exit_code: None,
            error: None,
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn cancel_after_the_run_finished_is_a_no_op() {
        // The step list is what a stale reader might still hold; the run's status wins.
        let open = vec![step(StepStatus::Running, true)];
        for status in [
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ] {
            assert_eq!(cancel_plan(status, &open), None, "{status:?}");
        }
    }

    #[test]
    fn cancel_of_an_active_run_notifies_only_running_steps_with_an_agent() {
        let leased = step(StepStatus::Running, true);
        let orphaned = step(StepStatus::Running, false);
        let queued = step(StepStatus::Queued, false);
        let pending = step(StepStatus::Pending, false);
        let open = vec![leased.clone(), orphaned.clone(), queued, pending];
        for status in [RunStatus::Pending, RunStatus::Running] {
            let plan = cancel_plan(status, &open).expect("active run cancels");
            assert_eq!(
                plan.close_attempts,
                vec![leased.id, orphaned.id],
                "{status:?}"
            );
            assert_eq!(plan.notify, vec![leased.id], "{status:?}");
        }
    }

    #[test]
    fn cancel_with_nothing_running_still_proceeds() {
        let open = vec![step(StepStatus::Queued, false)];
        assert_eq!(
            cancel_plan(RunStatus::Running, &open),
            Some(CancelPlan::default())
        );
    }

    #[test]
    fn a_lost_lease_gets_exactly_retries_plus_two_leases() {
        // One more than `scheduler::retry_plan` grants a reported failure: with
        // `retries: 2` lost attempts 1, 2 and 3 come back and the fourth is the last.
        assert_eq!(requeue_outcome(1, 2), RequeueOutcome::Requeue);
        assert_eq!(requeue_outcome(2, 2), RequeueOutcome::Requeue);
        assert_eq!(requeue_outcome(3, 2), RequeueOutcome::Requeue);
        assert_eq!(requeue_outcome(4, 2), RequeueOutcome::Fail);
        assert_eq!(requeue_outcome(5, 2), RequeueOutcome::Fail);
    }

    #[test]
    fn a_step_without_retries_survives_one_lost_lease_and_fails_on_the_second() {
        // One rolling agent restart is not the step's fault; a second lost lease is the
        // pattern of a step that kills its agent, and the old unconditional requeue was
        // the forever loop.
        assert_eq!(requeue_outcome(1, 0), RequeueOutcome::Requeue);
        assert_eq!(requeue_outcome(2, 0), RequeueOutcome::Fail);
    }
}
