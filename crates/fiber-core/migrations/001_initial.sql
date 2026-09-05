-- Initial Fiber schema (idempotent for existing IF NOT EXISTS installs).

CREATE TABLE IF NOT EXISTS projects (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    slug TEXT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS pipelines (
    id UUID PRIMARY KEY,
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    definition JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS runs (
    id UUID PRIMARY KEY,
    pipeline_id UUID NOT NULL REFERENCES pipelines(id) ON DELETE CASCADE,
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    status TEXT NOT NULL,
    trigger TEXT NOT NULL,
    definition_snapshot JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS step_runs (
    id UUID PRIMARY KEY,
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    step_id TEXT NOT NULL,
    step_name TEXT NOT NULL,
    status TEXT NOT NULL,
    image TEXT,
    run_cmd TEXT NOT NULL,
    labels JSONB NOT NULL DEFAULT '[]',
    needs JSONB NOT NULL DEFAULT '[]',
    retries INT NOT NULL DEFAULT 0,
    attempt INT NOT NULL DEFAULT 0,
    agent_id UUID,
    lease_expires_at TIMESTAMPTZ,
    exit_code INT,
    error TEXT,
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_step_runs_status ON step_runs(status);
CREATE INDEX IF NOT EXISTS idx_step_runs_run ON step_runs(run_id);

CREATE TABLE IF NOT EXISTS agents (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    labels JSONB NOT NULL DEFAULT '[]',
    concurrency INT NOT NULL DEFAULT 1,
    token_hash TEXT NOT NULL UNIQUE,
    last_seen_at TIMESTAMPTZ,
    online BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS log_lines (
    id BIGSERIAL PRIMARY KEY,
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    step_run_id UUID NOT NULL REFERENCES step_runs(id) ON DELETE CASCADE,
    stream TEXT NOT NULL,
    data TEXT NOT NULL,
    seq BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_log_lines_step ON log_lines(step_run_id, seq);

CREATE TABLE IF NOT EXISTS artifacts (
    id UUID PRIMARY KEY,
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    step_run_id UUID NOT NULL REFERENCES step_runs(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    path TEXT NOT NULL,
    size BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS webhook_secrets (
    id UUID PRIMARY KEY,
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    secret TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

ALTER TABLE pipelines ADD COLUMN IF NOT EXISTS last_scheduled_at TIMESTAMPTZ;
ALTER TABLE pipelines ADD COLUMN IF NOT EXISTS next_due_at TIMESTAMPTZ;
CREATE INDEX IF NOT EXISTS idx_pipelines_next_due
    ON pipelines(next_due_at) WHERE next_due_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS step_attempts (
    id UUID PRIMARY KEY,
    step_run_id UUID NOT NULL REFERENCES step_runs(id) ON DELETE CASCADE,
    attempt INT NOT NULL,
    agent_id UUID,
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at TIMESTAMPTZ,
    status TEXT NOT NULL,
    exit_code INT,
    error TEXT
);
CREATE INDEX IF NOT EXISTS idx_step_attempts_step
    ON step_attempts(step_run_id, attempt);

CREATE TABLE IF NOT EXISTS users (
    id UUID PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS sessions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash TEXT NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS project_secrets (
    id UUID PRIMARY KEY,
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (project_id, key)
);

CREATE TABLE IF NOT EXISTS fibers (
    id UUID PRIMARY KEY,
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    status TEXT NOT NULL,
    input JSONB NOT NULL DEFAULT 'null',
    state JSONB NOT NULL DEFAULT '{}',
    result JSONB,
    error TEXT,
    attempts INT NOT NULL DEFAULT 0,
    wake_at TIMESTAMPTZ,
    heartbeat_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX IF NOT EXISTS idx_fibers_project ON fibers(project_id);
CREATE INDEX IF NOT EXISTS idx_fibers_status_wake ON fibers(status, wake_at);
CREATE INDEX IF NOT EXISTS idx_fibers_running_hb ON fibers(status, heartbeat_at);

CREATE TABLE IF NOT EXISTS fiber_steps (
    fiber_id UUID NOT NULL REFERENCES fibers(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    value JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (fiber_id, key)
);
