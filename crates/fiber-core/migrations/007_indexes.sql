-- Indexes for the hot paths that previously sequential-scanned:
--   renew_agent_leases / requeue_agent_steps (per heartbeat, per disconnect),
--   requeue_expired_leases (every reclaim tick), queued-step offers (per heartbeat),
--   the artifact restore list (per offer), retention's cascade from runs into
--   log_lines / artifacts, session purge, and the per-pipeline active-run guard.
-- Plain CREATE INDEX (not CONCURRENTLY): sqlx runs a migration file as one
-- multi-statement query, which Postgres wraps in an implicit transaction where
-- CONCURRENTLY is not allowed. Each build takes a SHARE lock on its table for the
-- duration of the build; see docs/operations.md "Upgrades".
CREATE INDEX IF NOT EXISTS idx_step_runs_agent
    ON step_runs (agent_id) WHERE agent_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_step_runs_running_lease
    ON step_runs (lease_expires_at) WHERE status = 'running';
CREATE INDEX IF NOT EXISTS idx_step_runs_queued
    ON step_runs (started_at ASC NULLS FIRST, id) WHERE status = 'queued';
CREATE INDEX IF NOT EXISTS idx_artifacts_run ON artifacts (run_id);
CREATE INDEX IF NOT EXISTS idx_artifacts_step_run ON artifacts (step_run_id);
CREATE INDEX IF NOT EXISTS idx_log_lines_run ON log_lines (run_id);
CREATE INDEX IF NOT EXISTS idx_sessions_expires ON sessions (expires_at);
CREATE INDEX IF NOT EXISTS idx_runs_pipeline_status ON runs (pipeline_id, status);
CREATE INDEX IF NOT EXISTS idx_step_attempts_open
    ON step_attempts (step_run_id) WHERE finished_at IS NULL;
