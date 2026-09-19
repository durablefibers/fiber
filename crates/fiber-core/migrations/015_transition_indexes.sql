-- Indexes for the sweeps added after 007, one dead index, an artifact uniqueness rule,
-- and the status vocabularies as constraints. Every statement here is idempotent so a
-- partially-applied environment converges. No CONCURRENTLY: sqlx runs a migration in a
-- transaction, and none of these tables is large enough for the SHARE lock to matter
-- (log_lines only loses an index here, which is instant).

-- `list_timed_out_runs` runs every 15 s per replica and wants only running runs by
-- start time; without this it walks idx_runs_pipeline_status by its second column.
CREATE INDEX IF NOT EXISTS idx_runs_running_started
    ON runs (started_at)
    WHERE status = 'running';

-- Retention and project delete ask "which of these blob paths is still referenced".
CREATE INDEX IF NOT EXISTS idx_artifacts_path ON artifacts (path);

-- Fiber retention deletes terminal fibers oldest-first by `updated_at`.
CREATE INDEX IF NOT EXISTS idx_fibers_terminal_updated
    ON fibers (updated_at)
    WHERE status IN ('completed', 'failed', 'cancelled');

-- Nothing has ordered log lines by `seq` since migration 010 moved readers onto `id`;
-- the index only taxed every insert on the hottest table.
DROP INDEX IF EXISTS idx_log_lines_step;

-- One row per (step, artifact name). A step is at-least-once, so a re-run re-uploads
-- the same names; `create_artifact` now upserts on this key. Existing databases may
-- already hold duplicates from earlier re-uploads, so the newest row of each pair is
-- kept and the rest removed first. They point at the same blob path
-- (artifacts/{run}/{step}/{name}), so no blob is orphaned by this.
DELETE FROM artifacts a
USING artifacts b
WHERE a.step_run_id = b.step_run_id
  AND a.name = b.name
  AND (a.created_at, a.id) < (b.created_at, b.id);

CREATE UNIQUE INDEX IF NOT EXISTS artifacts_step_name_key
    ON artifacts (step_run_id, name);

-- Status columns were free text. A misspelt status could be written by a bug and would
-- then never match any transition, so the row would sit outside every state machine
-- forever. NOT VALID: enforced for every insert and update from now on, but existing
-- rows are not scanned at boot — a corrupt row on an old database would otherwise
-- refuse the whole upgrade. Validate by hand once the install is known clean:
--   ALTER TABLE runs VALIDATE CONSTRAINT runs_status_check;
--   ALTER TABLE step_runs VALIDATE CONSTRAINT step_runs_status_check;
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'runs_status_check') THEN
        ALTER TABLE runs ADD CONSTRAINT runs_status_check
            CHECK (status IN ('pending', 'running', 'succeeded', 'failed', 'cancelled'))
            NOT VALID;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = 'step_runs_status_check') THEN
        ALTER TABLE step_runs ADD CONSTRAINT step_runs_status_check
            CHECK (status IN ('pending', 'queued', 'running', 'succeeded', 'failed',
                              'cancelled', 'skipped'))
            NOT VALID;
    END IF;
END $$;
