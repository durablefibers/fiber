-- Offers now read the queue oldest-first by `queued_at` (bounded to a page), where they
-- used to order by `started_at NULLS FIRST` over the whole table. `started_at` is never
-- cleared, so a step requeued after a lost lease sorted behind every step that had
-- never started, however old its run. This index serves the new order exactly
-- (`ORDER BY queued_at, id` — ASC NULLS LAST, the index default) and the old one has no
-- reader left. No CONCURRENTLY: sqlx runs the file in one transaction; step_runs is small
-- enough that the SHARE lock does not matter (see docs/operations.md "Upgrades").
CREATE INDEX IF NOT EXISTS idx_step_runs_queued_at
    ON step_runs (queued_at, id) WHERE status = 'queued';
DROP INDEX IF EXISTS idx_step_runs_queued;

-- Every path that makes a step `queued` has stamped `queued_at` since migration 012, which
-- also backfilled the rows queued at the time. Repeated here for any row a path older than
-- 012 left behind: a NULL sorts last under the new order, which would starve it.
UPDATE step_runs s
   SET queued_at = r.created_at
  FROM runs r
 WHERE r.id = s.run_id AND s.status = 'queued' AND s.queued_at IS NULL;
