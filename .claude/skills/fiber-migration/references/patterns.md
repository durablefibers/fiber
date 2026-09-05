# Migration patterns

## Add a nullable column (safe, one step)

```sql
-- 005_step_runs_queue_reason.sql
ALTER TABLE step_runs ADD COLUMN queue_reason TEXT;
```

Model: `pub queue_reason: Option<String>`.

## Add a required column (four steps, across releases)

Never do this in one migration on a populated table.

```sql
-- Release N:   add nullable + default
ALTER TABLE agents ADD COLUMN pool TEXT DEFAULT 'default';
-- Release N:   backfill existing rows
UPDATE agents SET pool = 'default' WHERE pool IS NULL;
-- Release N+1: enforce, once all writers set it
ALTER TABLE agents ALTER COLUMN pool SET NOT NULL;
```

Between N and N+1 the old image, which does not know the column, must still be able to INSERT — which is exactly what the `DEFAULT` buys.

## Rename a column (never in place)

```sql
-- Release N:   add the new name, write both from code
ALTER TABLE runs ADD COLUMN trigger_label TEXT;
-- Release N+1: read the new one only
-- Release N+2: drop the old one
ALTER TABLE runs DROP COLUMN trigger;
```

## Index for a hot query

```sql
CREATE INDEX IF NOT EXISTS idx_step_runs_status_lease
  ON step_runs (status, lease_expires_at);
```

Use `IF NOT EXISTS` so a partially-applied environment converges. For a large table in production, run `CONCURRENTLY` — and note sqlx wraps each migration in a transaction by default, so a `CONCURRENTLY` index needs its own migration file with the transaction disabled, or an out-of-band operator step documented in `docs/operations.md`.

## A table that references a run

```sql
CREATE TABLE step_annotations (
  id          UUID PRIMARY KEY,
  run_id      UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  step_run_id UUID NOT NULL REFERENCES step_runs(id) ON DELETE CASCADE,
  body        TEXT NOT NULL,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_step_annotations_run ON step_annotations (run_id);
```

`ON DELETE CASCADE` is what keeps retention working: `delete_runs` removes runs and expects children to follow. Without it, retention leaves orphans and eventually fails on the foreign key.

## Checking what actually applied

```sql
SELECT version, description, installed_on, success, checksum
FROM _sqlx_migrations ORDER BY version;
```

A checksum mismatch here is what an edited migration looks like from the database's side. The fix is never to edit further — restore the file to its committed content and add a new migration.
