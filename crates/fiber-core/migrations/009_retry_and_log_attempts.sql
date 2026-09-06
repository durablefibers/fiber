-- Re-running a run, and reading one attempt's logs.
--
-- retry_of records lineage so a retry can be traced back to what it re-ran; the retry
-- itself carries its own copy of the original's definition snapshot, so it executes what
-- the original executed rather than whatever the pipeline says now.
ALTER TABLE runs ADD COLUMN IF NOT EXISTS retry_of UUID REFERENCES runs(id) ON DELETE SET NULL;

-- Log lines are append-only per attempt; without this, a retried step's output from two
-- attempts interleaves with no way to tell them apart. Existing rows are attempt 0.
ALTER TABLE log_lines ADD COLUMN IF NOT EXISTS attempt INT NOT NULL DEFAULT 0;

-- Tail and follow queries: newest-first within a step, optionally one attempt.
CREATE INDEX IF NOT EXISTS idx_log_lines_step_attempt ON log_lines (step_run_id, attempt, id);
-- Keyset pagination over a project's runs.
CREATE INDEX IF NOT EXISTS idx_runs_project_created ON runs (project_id, created_at DESC, id DESC);
