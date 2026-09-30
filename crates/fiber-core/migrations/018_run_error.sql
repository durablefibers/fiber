-- Why a run failed before any step existed: the stored definition no longer compiled when
-- a webhook or schedule tried to start it. NULL for every other run. Additive and nullable,
-- so a previous version reading this table is unaffected.
ALTER TABLE runs ADD COLUMN IF NOT EXISTS error TEXT;
