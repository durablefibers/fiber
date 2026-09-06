-- What a run was actually triggered by.
--
-- Without this a webhook-triggered run only knew the branch, so the agent checked out
-- whatever the branch tip was when it cloned — not the commit under test — and a second
-- push during a build silently retargeted it. It also had nothing to report a status
-- against, and fork pull requests (whose head lives in another repository) could not be
-- built at all.
ALTER TABLE runs ADD COLUMN IF NOT EXISTS head_sha TEXT;
-- Branch name, or `refs/pull/<n>/head` for a pull request (fetchable from the base repo).
ALTER TABLE runs ADD COLUMN IF NOT EXISTS head_ref TEXT;
ALTER TABLE runs ADD COLUMN IF NOT EXISTS pr_number INT;
-- `owner/repo`, so posting a commit status needs nothing but the run.
ALTER TABLE runs ADD COLUMN IF NOT EXISTS repo_full_name TEXT;
