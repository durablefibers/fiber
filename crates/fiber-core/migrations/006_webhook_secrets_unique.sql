-- One webhook secret per (project, provider). The old upsert was DELETE + INSERT
-- outside a transaction, so concurrent writes could leave zero or two rows.
-- Keep the newest row where duplicates exist, then enforce uniqueness so the
-- store can use INSERT ... ON CONFLICT.
DELETE FROM webhook_secrets w
USING webhook_secrets n
WHERE w.project_id = n.project_id
  AND w.provider = n.provider
  AND (w.created_at < n.created_at OR (w.created_at = n.created_at AND w.id < n.id));

CREATE UNIQUE INDEX IF NOT EXISTS uq_webhook_secrets_project_provider
    ON webhook_secrets (project_id, provider);
