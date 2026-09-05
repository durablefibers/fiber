-- Retention / GC helpers.
CREATE INDEX IF NOT EXISTS idx_runs_finished_at
    ON runs (finished_at)
    WHERE finished_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_runs_terminal_created
    ON runs (created_at)
    WHERE status IN ('succeeded', 'failed', 'cancelled');
