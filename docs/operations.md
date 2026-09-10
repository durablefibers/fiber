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

### Agent TLS

Point agents at `wss://ci.example.com` and they use the host's own certificate store for
both the WebSocket and the artifact HTTP calls, so a private or corporate CA works as soon
as it is installed on the host. On Unix, `SSL_CERT_FILE` and `SSL_CERT_DIR` override that
if you would rather point at one bundle:

```
Environment=SSL_CERT_FILE=/etc/fiber/internal-ca.pem
```

The binaries link no OpenSSL, so a host needs no `libssl` — only a CA bundle
(`ca-certificates` on Debian and Ubuntu). Without one, nothing is trusted and every TLS
connection fails.

## Re-running a run

`POST /api/runs/{id}/retry` creates a new run from the original's **definition snapshot**,
so it re-runs what that run actually executed, not the pipeline as it stands now. The new
run records `retry_of`, and its trigger is `retry:<original id>`.

- `{ "failed_only": true }` carries over the steps that already succeeded — marked
  succeeded, never re-executed, with their artifacts copied forward so dependents can
  still restore them — and re-runs everything else. Use it when a long build succeeded and
  only a flaky test needs another go.
- Without it, every step runs again.

Because a retry shares artifact blobs with the run it came from, retention deletes a blob
only once no remaining run references its path.

## Reading logs

`GET /api/steps/{id}/logs` returns the newest 1000 lines by default. `?attempt=N` narrows
to one attempt — `seq` restarts per attempt, so a retried step's output interleaves
otherwise — and `?after_id=<id>` returns what followed a line you already have, which is
how you tail a live step.

## Stuck runs

A run that stays `running` is one of:

- **Nothing to lease it** — its steps are `queued` and no online agent matches the labels / project pool. Check **Agents** for an online agent with every required label.
- **Waiting on a retry backoff** — a failed step with `retries` is `queued` with `not_before` in the future (max 60 s).
- **Hung step** — the attempt exceeds its `timeout_minutes` (default `FIBER_STEP_TIMEOUT_DEFAULT_MINUTES`): the agent fails it at the deadline, and the server fails it `FIBER_STEP_TIMEOUT_GRACE_MINUTES` later if the agent did not. A whole-run `timeout_minutes` cancels the run with reason `run timed out`.
- **Agent gone** — leases expire after 5 minutes without heartbeats and the step is re-queued (`step_attempts` shows `reclaimed`).

`GET /api/steps/{id}/attempts` lists every attempt with its agent, status, and error.

## Step logs

A step's output is stored line by line, capped at `FIBER_STEP_LOG_MAX_LINES` (50,000) per
**attempt**. Past the cap the lines are dropped and one `system` line says so, because
silence looks like a step that stopped producing output. A retry is a new attempt with its
own budget. `0` disables the cap, for whoever would rather risk the disk than lose output.

Truncation never fails a step: losing the tail of a log is not a build failure.

Reading is already bounded — `GET /api/steps/{id}/logs` returns the newest lines by default
and pages with `?after_id=`. Writing is still one insert per line, which the cap bounds
rather than removes; batching them is worth doing when a real instance shows it matters.

## Metrics

`GET /metrics` serves Prometheus exposition. It is **off** until `FIBER_METRICS_TOKEN` is
set, and then wants that token as a bearer credential: `404` when off, `401` when wrong.
The API is reachable from the internet in a normal deployment and these figures describe
your build volume, so it fails closed rather than defaulting to open the way an exporter on
a private network would.

```yaml
scrape_configs:
  - job_name: fiber
    authorization:
      credentials: <FIBER_METRICS_TOKEN>
    static_configs:
      - targets: ["ci.example.com"]
```

| Metric | Meaning |
|---|---|
| `fiber_build_info{version}` | The running version, always `1` |
| `fiber_step_runs{status}` | Step runs by status — `queued` is the backlog |
| `fiber_runs{status}` | Runs by status |
| `fiber_fibers{status}` | Durable fibers by status |
| `fiber_agents{state}` | Agents `online` / `offline` |
| `fiber_oldest_queued_step_age_seconds` | How long the oldest leasable step has waited; `0` when the queue is empty |
| `fiber_step_queue_wait_seconds` | Histogram: how long each attempt waited to be leased |
| `fiber_step_duration_seconds` | Histogram: how long each attempt spent running |

Every value is read from the database on scrape, not counted in the process, so a restart
does not reset anything and two API replicas report the same figures. The one to alert on
is `fiber_oldest_queued_step_age_seconds`: it climbs when no agent matches a step's labels,
which is otherwise invisible until someone notices a run sitting still. Steps held back by
retry backoff are excluded, since they are waiting deliberately.

The two histograms answer the question a gauge cannot, which is whether things are getting
worse:

```promql
histogram_quantile(0.95, sum by (le) (rate(fiber_step_queue_wait_seconds_bucket[1h])))
```

Both are per **attempt**, not per step. A step that was retried waited twice and ran twice,
and folding those together would hide exactly the runs worth looking at. Buckets run from
one second to an hour; past that a build is stuck rather than slow, which is what
`fiber_oldest_queued_step_age_seconds` is for.

Queue wait is recorded on the attempt when it is leased, rather than derived later, because
a requeue overwrites the step's `queued_at` and would otherwise rewrite the history of
earlier attempts. Attempts from before this shipped have no wait recorded and are absent
from the histogram rather than counted as zero.

## Background loops

Seven loops do the work between requests: `reclaim`, `schedules`, `events`, `agent_cmds`,
`fibers`, `github_status`, and `retention`. Each is supervised — a panic restarts it with
backoff rather than killing that task silently while the process stays up.

`/ready` reports them. `"loops": "ok"` when all are running; otherwise it lists the ones
that are down and the endpoint returns `503`, so a load balancer takes the instance out
rather than leaving it accepting traffic it cannot act on:

```json
{"checks": {"loops": ["schedules"], "postgres": "ok", "redis": "ok"}, "ok": false}
```

`/metrics` carries `fiber_background_loop_up{loop=...}` and
`fiber_background_loop_restarts_total{loop=...}`. **The restart counter is the one to
alert on.** A loop that keeps coming back is failing repeatedly, and because it recovers,
nothing else will tell you.

## OpenTelemetry

Set `OTEL_EXPORTER_OTLP_ENDPOINT` (or `FIBER_OTEL_ENDPOINT`) to a collector's **base** URL
and both `fiber-api` and `fiber-agent` export traces and metrics over OTLP HTTP. The signal
path is appended, so `http://collector:4318` becomes `/v1/traces` and `/v1/metrics`. A full
signal URL is left as given.

The agent is where steps actually run, so it is where the interesting numbers come from:

| Signal | Name | Notes |
|---|---|---|
| Span | `fiber.step` | One per step execution, with `run_id`, `step_run_id`, `kind` (`docker` / `shell`) and `outcome` |
| Counter | `fiber.agent.steps` | Steps finished, by `outcome` and `kind` |
| Histogram | `fiber.agent.step.duration` | Seconds from offer to completion, workspace preparation and artifact transfer included |

`outcome` is `succeeded`, `failed`, `cancelled`, `timed_out`, or `error`, matching what the
run page shows. Every agent reports the same `service.name`, so `service.instance.id` is set
from `--name`: "which worker is slow" is the question this data gets asked.

The offer carries a W3C `traceparent`, so an agent's `fiber.step` is a child of the API's
`fiber.offer` and a step reads as one trace across both processes. An agent that receives no
trace context, because the server does not export, starts a root span as before.

`RUST_LOG` raises or lowers this; `RUST_LOG=fiber_api=debug` logs the trace context of every
offer, which is where to look when spans do not join up.

## Images and releases

Tagging `vX.Y.Z` runs the gate, then publishes `ghcr.io/durablefibers/fiber-api` and
`fiber-agent` (tagged `X.Y.Z`, `X.Y`, and `latest`) and attaches agent + CLI binaries
(`fiber-agent-<target>.tar.gz` with a `.sha256`) for linux x86_64/arm64 and macOS arm64 to the
GitHub release. `scripts/install-agent.sh` consumes those assets. The tag must match the
workspace version in `Cargo.toml` or the release fails before publishing anything.

Images cover `linux/amd64` and `linux/arm64`. Each architecture is built on a runner of that
architecture and the two are joined into one manifest list, so `docker pull` picks the right
one and no build runs under emulation. The manifest step prints the platforms it published,
which is where to look if an image ever goes single-architecture again.

### Running Compose on the published images

`deploy/docker-compose.yml` uses `ghcr.io/durablefibers/fiber-api` and `fiber-agent`, so a
deployment needs no build toolchain:

```bash
cp deploy/.env.example deploy/.env      # set FIBER_SECRETS_KEY and FIBER_ADMIN_PASSWORD
docker compose -f deploy/docker-compose.yml pull
docker compose -f deploy/docker-compose.yml up -d
```

`FIBER_VERSION` in `deploy/.env` picks the tag. It defaults to `latest`; pin an exact
version for a reproducible deployment. To build from the working tree instead, add
`--build`, which overrides the published image with a local one.

`fiber-web` is still built locally, deliberately: its API URL is baked in at build time, so
a generic image would only work for whoever's URL was compiled in.

Before the first tag:

- **The repository must be public** for the documented install paths to work. Release assets and
  `raw.githubusercontent.com` URLs 404 for anonymous callers on a private repo, and GitHub-hosted
  arm64 runners are free only on public repos — a private repo would queue that leg until it times out.
- **Make the GHCR packages public once.** The first push creates each package private, whatever the
  repository's visibility; flip it in the package settings or `docker pull` needs a token.

The **web image is not published**: `VITE_FIBER_API_URL` is baked in at build time, so a generic
image would only work for whoever's URL was compiled in. Build it per deployment:

```bash
docker build -f apps/web/Dockerfile.web --build-arg VITE_FIBER_API_URL=https://ci.example.com \
  -t fiber-web apps/web
```

Container images build from one `deploy/Dockerfile`:

```bash
docker build -f deploy/Dockerfile --target fiber-api   -t fiber-api   .
docker build -f deploy/Dockerfile --target fiber-agent -t fiber-agent .
```

Both share a cargo-chef dependency layer, so a source-only change does not rebuild the whole
dependency graph. `CHANGELOG.md` records what each version contains.

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
- **Step timeouts apply to runs already in flight at upgrade.** Steps without `timeout_minutes` get `FIBER_STEP_TIMEOUT_DEFAULT_MINUTES` (60); a build that legitimately runs longer must set `timeout_minutes` on the step (or raise the default) *before* upgrading, or its in-flight attempt will be failed by the backstop  
- Migration `007_indexes.sql` adds nine indexes (`step_runs`, `artifacts`, `log_lines`, `sessions`, `runs`, `step_attempts`). It runs at the first boot of the new version and holds a `SHARE` lock on each table while that index builds — writes to `log_lines` pause for the duration, which is seconds on a typical install. On a very large `log_lines` table, run retention first or apply the statements by hand with `CREATE INDEX CONCURRENTLY` before upgrading (the migration's `IF NOT EXISTS` then skips them).

- Postgres major bumps (e.g. 16 → 17): Compose volume recreate (`down -v`) if needed  
- Agent tokens are hashes only — rotating requires distributing a new plaintext token
