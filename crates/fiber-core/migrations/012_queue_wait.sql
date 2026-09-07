-- Queue wait needs two facts nothing recorded: when a step became leasable, and how long
-- the attempt that picked it up had been waiting.
--
-- The wait is stored on the attempt rather than derived later from step_runs, because a
-- requeue overwrites queued_at and would otherwise rewrite the history of earlier attempts.
ALTER TABLE step_runs ADD COLUMN IF NOT EXISTS queued_at TIMESTAMPTZ;
ALTER TABLE step_attempts ADD COLUMN IF NOT EXISTS queue_wait_seconds DOUBLE PRECISION;

-- Steps already queued when this applied have no queued_at, and would otherwise never
-- record a wait. The run's creation is the closest truth available.
UPDATE step_runs s
   SET queued_at = r.created_at
  FROM runs r
 WHERE r.id = s.run_id AND s.status = 'queued' AND s.queued_at IS NULL;
