-- Per-pipeline concurrency: runs sharing a resolved group contend, and a new one cancels
-- the older ones. The group is resolved at run start from the pipeline's `concurrency:`
-- and stored here, so the rule that governed a run stays readable afterwards — the same
-- reason the definition snapshot lives on the row.
--
-- Nullable: every existing run, and every pipeline that declares no concurrency, has none.
ALTER TABLE runs ADD COLUMN IF NOT EXISTS concurrency_group TEXT;

-- The lookup is "other unfinished runs of this project in this group". Partial, because
-- finished runs are the overwhelming majority and are never the answer.
CREATE INDEX IF NOT EXISTS idx_runs_concurrency_group
    ON runs (project_id, concurrency_group, created_at)
    WHERE concurrency_group IS NOT NULL
      AND status NOT IN ('succeeded', 'failed', 'cancelled', 'skipped');
