-- Retry backoff: a step re-queued after a failure is not offered before this time.
-- Previously the backoff was a sleeping task pushing to a Redis list nobody read,
-- so retries were re-offered on the next heartbeat and the delay was lost on restart.
ALTER TABLE step_runs ADD COLUMN IF NOT EXISTS not_before TIMESTAMPTZ;
