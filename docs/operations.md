# Operations

## Health checks

- `GET /health` — process up  
- `GET /ready` — Postgres + Redis reachable (Compose healthcheck uses this)

## Retention / GC

Background loop in `fiber-api`:

1. Purge expired sessions  
2. Delete terminal runs (`succeeded` / `failed` / `cancelled`) older than `FIBER_RETENTION_DAYS`, while keeping the newest `FIBER_RETENTION_KEEP_RUNS` per pipeline  
3. Delete artifact blobs (local file or S3) before removing DB rows (cascade removes steps, logs, attempts)

Defaults: 30 days, keep 20, batch 100, interval 1h. Set `FIBER_RETENTION_DAYS=0` to disable age deletion (sessions still purged).

Migration `003_retention.sql` adds indexes used by the query.

## Observability

Set `OTEL_EXPORTER_OTLP_ENDPOINT` or `FIBER_OTEL_ENDPOINT` to an OTLP HTTP collector. Without it, tracing still goes to stdout via `RUST_LOG`.

## Multi-instance

Redis channel `fiber:events` fans out run events so multiple API processes can share UI subscriptions. Leases and DB remain the source of truth for execution.

## Dogfood scripts

```bash
python3 scripts/dogfood_authz_agents.py   # roles + agent CRUD/rotate
python3 scripts/dogfood_agent_pools.py    # project-scoped vs global agents
python3 scripts/dogfood_artifacts.py      # artifacts + path filters
python3 scripts/dogfood_s3_presign.py     # MinIO presign upload/restore/download

python3 scripts/dogfood_artifacts.py      # needs a connected agent
```

Expect `DOGFOOD_OK` on success.

## Backups

Backup Postgres (runs, definitions, secrets ciphertext, memberships). Separately backup `FIBER_ARTIFACTS_DIR` or the S3 bucket. Keep `FIBER_SECRETS_KEY` safe — without it encrypted secrets cannot be decrypted.

## Upgrades

- Schema: sqlx migrations under `crates/fiber-core/migrations/` run on API boot  
- Postgres major bumps (e.g. 16 → 17): Compose volume recreate (`down -v`) if needed  
- Agent tokens are hashes only — rotating requires distributing a new plaintext token
