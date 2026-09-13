-- `runs.retry_of` is a self-reference with ON DELETE SET NULL (migration 009) and had no
-- index. Postgres enforces that with a per-deleted-row trigger — `UPDATE runs SET
-- retry_of = NULL WHERE retry_of = $1` — so every deleted run sequentially scanned the
-- whole runs table. Deleting 10k runs on a 200k-run instance is a quadratic amount of
-- work inside one transaction, which retention paid a little at a time and project
-- deletion would pay all at once.
--
-- Plain CREATE INDEX, not CONCURRENTLY: sqlx runs a migration file as one multi-statement
-- simple query inside an implicit transaction, where CONCURRENTLY is not allowed.
CREATE INDEX IF NOT EXISTS idx_runs_retry_of ON runs (retry_of);
