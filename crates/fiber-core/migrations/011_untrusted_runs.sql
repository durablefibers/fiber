-- A run whose code came from outside the project: a pull request opened from a fork.
--
-- The pipeline and its secrets belong to the base repository, but the code being built
-- was written by whoever opened the pull request. Injecting project secrets into it would
-- hand any outside contributor every credential the project owns, so these runs get none.
ALTER TABLE runs ADD COLUMN IF NOT EXISTS untrusted BOOLEAN NOT NULL DEFAULT FALSE;
