# Operations

## Deployment

`deploy/docker-compose.yml` is the reference deployment. Its defaults are chosen so that `docker compose up` on a shared host does not expose anything by accident:

- Postgres, Redis, and MinIO publish only on **127.0.0.1**; Redis requires a password (`FIBER_REDIS_PASSWORD`).
- `fiber-api` (`18080`) and `fiber-web` (`3100`) also bind to `127.0.0.1` by default — terminate TLS with a reverse proxy and forward to them. Set `FIBER_API_BIND=0.0.0.0` / `FIBER_WEB_BIND=0.0.0.0` only for a trusted network.
- Every service has `restart: unless-stopped`; the API waits for Postgres, Redis, **and** MinIO health.
- Settings live in `deploy/.env` (copy `deploy/.env.example`). Generate `FIBER_SECRETS_KEY` with `openssl rand -hex 32` before storing any real secret; without it, secrets are stored in plaintext and the API warns at boot.
- Set `FIBER_ADMIN_PASSWORD` **before the first boot**: it is applied only when the users table is empty. For an existing instance, change the admin password through the API/UI instead. The API warns at boot while the configured value is the default `fiber`.

Minimal Caddy front end (automatic TLS):

```
ci.example.com {
    reverse_proxy /api/* 127.0.0.1:18080
    reverse_proxy /ws/*  127.0.0.1:18080
    reverse_proxy       127.0.0.1:3100
}
```

Build the web image with `VITE_FIBER_API_URL=https://ci.example.com` and set `FIBER_CORS_ORIGINS=https://ci.example.com` so the browser is allowed to call the API. Session and agent tokens are bearer credentials and must only travel over TLS.

## Health checks

- `GET /health` — process up  
- `GET /ready` — Postgres + Redis reachable (Compose healthcheck uses this). Failing checks report `"error"` only; the cause is in the API log.

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

`fiber-api` can run as several replicas against one Postgres and one Redis:

- **Leases** — `lease_step`, lease renewal, expired-lease reclaim and stale-agent marking are single conditional `UPDATE ... RETURNING` statements; two replicas cannot lease the same step.
- **Propagation** — unlocking dependents / cascading skips / finishing a run happens in one transaction with the run row locked.
- **Schedules** — a cron/interval slot is claimed with a compare-and-set on `pipelines.next_due_at` (plus "no active run"), so exactly one replica starts each scheduled run.
- **Durable fibers** — ready fibers are claimed with `UPDATE ... FOR UPDATE SKIP LOCKED`; a fiber runs on one replica per attempt.
- **Redis `fiber:events`** — run/step/log events fan out so `/ws/runs/{id}` subscribers on any replica see them.
- **Redis `fiber:agent_cmds`** — agent-directed messages (run cancel, token rotation / delete disconnects) fan out so the replica holding the agent's socket delivers them.

Agent presence (labels, concurrency, in-flight counts) is per replica: an agent is offered steps by the replica it is connected to. Redis is not a queue — queued steps live in Postgres and are pulled on each agent heartbeat.

## Dogfood scripts

```bash
python3 scripts/dogfood_authz_agents.py   # roles + agent CRUD/rotate
python3 scripts/dogfood_agent_pools.py    # project-scoped vs global agents
python3 scripts/dogfood_artifacts.py      # artifacts + path filters
python3 scripts/dogfood_s3_presign.py     # MinIO presign upload/restore/download
bash scripts/dogfood_compose.sh           # full compose up --build smoke
```

Expect `DOGFOOD_OK` on success.

## Backups

### Postgres

Compose volume: `fiber_pg` (service `fiber-postgres`).

```bash
# Logical dump (preferred)
docker compose -f deploy/docker-compose.yml exec -T fiber-postgres \
  pg_dump -U fiber fiber > fiber-$(date +%Y%m%d).sql

# Restore into a running empty DB
docker compose -f deploy/docker-compose.yml exec -T fiber-postgres \
  psql -U fiber fiber < fiber-YYYYMMDD.sql

# Volume snapshot (stop writers first)
docker compose -f deploy/docker-compose.yml stop fiber-api
docker run --rm -v fiber_fiber_pg:/data -v "$PWD":/backup alpine \
  tar czf /backup/fiber-pg.tgz -C /data .
```

Volume name may be prefixed by the Compose project (`fiber_fiber_pg` when using `name: fiber` in `deploy/docker-compose.yml`). Confirm with `docker volume ls | grep fiber`.

### Artifacts & secrets key

- Backup `FIBER_ARTIFACTS_DIR` **or** the S3/MinIO bucket (`fiber-artifacts`).
- Keep **`FIBER_SECRETS_KEY`** offline and backed up separately — without it, encrypted project and webhook secrets cannot be decrypted. Compose reads it from `deploy/.env`; it is intentionally not committed anywhere.

### What to include

| Data | Where |
|---|---|
| Runs, pipelines, memberships, sessions | Postgres |
| Secret ciphertext | Postgres (`project_secrets`, `webhook_secrets`) — needs `FIBER_SECRETS_KEY`. If the key is lost, re-enter project secrets and re-`PUT` webhook secrets (deliveries are rejected with 401 until then) |
| Artifact blobs | Local dir or S3 |
| Agent tokens | Not recoverable from DB (hashes only) — re-issue after restore |

## Upgrades

- Schema: sqlx migrations under `crates/fiber-core/migrations/` run on API boot  
- Redis no longer carries a step queue; after upgrading, `DEL fiber:ready_steps` removes the orphaned list left by older versions  
- Migration `007_indexes.sql` adds nine indexes (`step_runs`, `artifacts`, `log_lines`, `sessions`, `runs`, `step_attempts`). It runs at the first boot of the new version and holds a `SHARE` lock on each table while that index builds — writes to `log_lines` pause for the duration, which is seconds on a typical install. On a very large `log_lines` table, run retention first or apply the statements by hand with `CREATE INDEX CONCURRENTLY` before upgrading (the migration's `IF NOT EXISTS` then skips them).

- Postgres major bumps (e.g. 16 → 17): Compose volume recreate (`down -v`) if needed  
- Agent tokens are hashes only — rotating requires distributing a new plaintext token
