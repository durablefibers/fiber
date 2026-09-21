-- `/metrics` now bounds both latency histograms to the last 24 hours instead of scanning
-- every attempt inside the retention window on each scrape. This index is what makes the
-- bound cheap: without it the window is still a sequential scan, just with a filter.
--
-- No CONCURRENTLY: sqlx runs a migration in one transaction, and `step_attempts` is
-- bounded by retention (see docs/operations.md "Upgrades").
CREATE INDEX IF NOT EXISTS idx_step_attempts_started
    ON step_attempts (started_at);
