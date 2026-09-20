# Operations

## Deployment

`deploy/docker-compose.yml` is the reference deployment. Its defaults are chosen so that `docker compose up` on a shared host does not expose anything by accident:

- **Four credentials have no default and must be set in `deploy/.env` before anything starts**: `FIBER_POSTGRES_PASSWORD`, `FIBER_REDIS_PASSWORD`, `FIBER_S3_ACCESS_KEY` / `FIBER_S3_SECRET_KEY`, and `FIBER_ADMIN_PASSWORD`. `docker compose config` (and therefore `up`, `pull`, `logs`) fails with the name of the first one missing until they are. A default that works is a default nobody changes, and `FIBER_API_BIND=0.0.0.0` plus a published admin password is a public instance with a known login. The S3 pair is required even when MinIO is off — Compose interpolates the whole file before it filters by profile — so give it any non-empty placeholder if you do not use the object store.
- Postgres, Redis, and MinIO publish only on **127.0.0.1**; Redis requires a password, which reaches it through a Compose `configs:` entry rather than `--requirepass` on the command line, where `ps` and `docker inspect` would show it.
- `fiber-api` (`18080`) and `fiber-ui` (`3100`) also bind to `127.0.0.1` by default — terminate TLS with a reverse proxy and forward to them. Set `FIBER_API_BIND=0.0.0.0` / `FIBER_UI_BIND=0.0.0.0` only for a trusted network.
- **Artifacts go to the local filesystem by default** (the `fiber_artifacts` volume). MinIO is optional and sits behind a Compose profile: set `FIBER_S3_BUCKET=fiber-artifacts` and start it with `--profile minio`. See [artifacts](./artifacts.md).
- **`fiber-api` and `fiber-ui` run unprivileged**: uid 10001 and uid 101, `read_only: true` rootfs with a tmpfs for the few scratch paths they need, `cap_drop: [ALL]`, `no-new-privileges`. The API writes nothing outside `FIBER_ARTIFACTS_DIR`; the UI image is `nginxinc/nginx-unprivileged` and listens on 8080 inside the container.
- **Container logs are capped** at `max-size: 10m` × `max-file: 5` per service (~50 MB each). Docker's default keeps every line for ever, and a full host disk stops Postgres. Point the daemon at a real log system if you need more history.
- Every service has `restart: unless-stopped`; the API waits for Postgres and Redis health, and for MinIO too when the `minio` profile is active.
- Both images carry a `HEALTHCHECK` (API: `GET /ready`; UI: `GET /`), so `docker run` and non-Compose orchestrators get the same probe Compose uses.
- `fiber-api` runs with `init: true` and `stop_grace_period: 30s`, so `docker stop` and a `compose up` of a new image deliver SIGTERM and the API drains (see [Shutdown and deploys](#shutdown-and-deploys)) instead of being killed after Docker's default 10 s.
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

Build the UI image with `VITE_FIBER_API_URL=https://ci.example.com` and set `FIBER_CORS_ORIGINS=https://ci.example.com` so the browser is allowed to call the API. Session and agent tokens are bearer credentials and must only travel over TLS.

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

## Shutdown and deploys

On SIGTERM or SIGINT, `fiber-api` stops accepting connections, lets requests already in
flight finish, and ends every open WebSocket session with a Close frame (code `1012`,
"server shutting down") — the process waits for those sessions to finish, not just for
HTTP. It then flushes the OpenTelemetry batch and exits. The drain is bounded at
**20 s**: a request or session still open then is dropped and the process leaves anyway,
which with the ≤ 5 s telemetry flush stays inside the 30 s `stop_grace_period`. Each
phase is logged under `shutdown:`.

The Close frame is what lets an agent reconnect at once instead of discovering a dead
socket by TCP timeout, particularly behind a proxy that would otherwise hold its side
half-open. The UI already reconnects 2 s after any close, so for browsers the frame is
tidiness rather than a change in behaviour.

Restarting `fiber-api` does not restart the builds in flight. A step's lease belongs to
the agent, not to the WebSocket session: when the socket drops, the agent keeps running
the step, buffers its output, and reconnects with backoff (1 s → 30 s, jittered); its
first heartbeat back renews the leases and the buffered lines and completions are flushed
in order. The server accepts them as long as the step is still `running` under that agent.
Nothing is requeued on disconnect — the agent is only marked offline — so a deploy costs
the builds nothing but the reconnect delay. Measured on a debug build: the API killed and
restarted under a 30 s step, the step finished on attempt 1 with every line present.

The limit is the lease (**300 s**). An agent that has not reconnected 270 s after the
drop stops its steps without reporting them; the reclaim loop requeues the expired leases
on the server, within the same `retries + 1` budget a reported failure gets. Keep an API
outage under that and no build notices; past it, the steps that were running are
re-leased once the API is back. Two things still requeue at once, on purpose: an agent
that exits on SIGTERM says `Goodbye` after stopping its steps (a rolling *agent* restart
hands the work over within seconds), and a token rotation, agent delete, or project
delete ends the session with an immediate requeue, since the agent's results could not
be accepted anyway.

During a mixed-version rolling upgrade of the API, a replica *older* than this behaviour
still requeues on disconnect and on its stale-agent sweep. Upgrade every replica before
counting on it. Agents older than protocol revision 1 (no `protocol_version` in `Hello`)
cancel their steps on any close, and the server requeues their steps at once as before —
upgrade agents to get the new behaviour on their side.

Background loops are not drained. A scheduled run or a durable-fiber step interrupted
mid-way is picked up by the replica that comes up next — that is what at-least-once
means here.

## Request limits

- Ordinary API requests are answered `408 Request Timeout` after **30 s** and the
  handler is dropped, so a slow client or a stuck query cannot hold a server task
  indefinitely. WebSocket upgrades (`/ws/*`), artifact transfers
  (`/api/artifacts/{id}/download`, `/api/agent/steps/{id}/artifacts`,
  `/api/agent/artifacts/{id}/download`) and `DELETE /api/projects/{id}` are exempt: a
  session lives as long as the agent, an artifact moves at link speed up to its size
  cap, and a project delete cascades through everything the project ever ran.
- The GitHub webhook accepts deliveries up to **25 MiB**, GitHub's own maximum, with at
  most **8** deliveries buffered at once (the endpoint is unauthenticated until the body
  is read and its signature checked); further deliveries wait for a slot and fail with
  `408` if none frees up in time. GitHub does **not** retry a failed delivery: it is listed
  under the webhook's *Recent Deliveries* and must be redelivered from there, so a `408`
  is a push that did not build until someone does. Other JSON bodies keep the 2 MiB default.
- The server pings every agent socket every **15 s** and closes it after **45 s** without
  a frame of any kind (two pongs missed; the agent's own 10 s heartbeat normally answers
  long before). The close takes the normal disconnect path, so an agent whose host
  vanished silently goes offline within a minute instead of when the kernel gives up on
  the TCP connection.

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
only once no remaining run references its path. If that reference check itself fails — a
database hiccup mid-tick — retention keeps every blob for that tick and logs a warning,
rather than assuming nothing references them. The cost is a blob that no run points at any
more surviving on disk; the alternative was deleting one that a surviving retry still
needs.

## Reading logs

`GET /api/steps/{id}/logs` returns the newest 1000 lines by default. `?attempt=N` narrows
to one attempt — `seq` restarts per attempt, so a retried step's output interleaves
otherwise — and `?after_id=<id>` returns what followed a line you already have, which is
how you tail a live step. Pages are ordered by `id` and nothing else, so the last line of
a page is the cursor to pass as the next `after_id`; the agent assigns `seq` in emission
order and sends its own notes down the same channel as the step's output, so `id` order
is emission order too.

## Stuck runs

A run that stays `running` is one of:

- **Nothing to lease it** — its steps are `queued` and no online agent matches the labels / project pool. Check **Agents** for an online agent with every required label.
- **Waiting on a retry backoff** — a failed step with `retries` is `queued` with `not_before` in the future (max 60 s).
- **Hung step** — the attempt exceeds its `timeout_minutes` (default `FIBER_STEP_TIMEOUT_DEFAULT_MINUTES`): the agent fails it at the deadline, and the server fails it `FIBER_STEP_TIMEOUT_GRACE_MINUTES` later if the agent did not. A whole-run `timeout_minutes` cancels the run with reason `run timed out`.
- **Agent gone** — leases expire after 5 minutes without heartbeats and the step is re-queued (`step_attempts` shows `reclaimed`). A disconnect alone does not requeue: the agent may be reconnecting, still running the step, and it renews the lease when it is back — so a step whose agent shows **offline** is not stuck until its lease has expired. A lost lease counts against the step's `retries` like a reported failure does, with one extra try: once the step has lost more than `retries + 1` leases it fails with `lease lost after N attempts` and the run finalises. A `retries: 0` step therefore survives one agent crash, while a step that reliably kills its agent (OOM, a Docker hang past the lease) stops after two leases instead of being re-leased forever. The same loop finalises any run still `running` whose every step is terminal.

`GET /api/steps/{id}/attempts` lists every attempt with its agent, status, and error.

## Retention

One loop purges expired sessions, terminal runs with their artifact blobs, and terminal
durable fibers. Each part has its own switch and `0` turns that part off; the loop still
ticks, so turning one off does not stop the others.

| Setting | Default | Removes |
|---|---|---|
| `FIBER_RETENTION_DAYS` | `30` | Terminal runs, keeping the newest `FIBER_RETENTION_KEEP_RUNS` per pipeline |
| `FIBER_RETENTION_FIBER_DAYS` | `7` | Terminal durable fibers, and their memoized steps by cascade |

Fibers have a shorter default than runs because they are usually a notification that either
worked or did not, where a run is a build someone may want to look back at. **A suspended
fiber is never deleted, however old its row looks** — one sleeping for a month is waiting,
not stale, and removing it would silently cancel work someone scheduled.

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
{"checks": {"loops": ["schedules"], "postgres": "ok", "redis": "ok"}, "degraded": false, "ok": false}
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
cp deploy/.env.example deploy/.env      # fill the four credentials and FIBER_SECRETS_KEY
docker compose -f deploy/docker-compose.yml pull
docker compose -f deploy/docker-compose.yml up -d
```

Every one of those commands reads the Compose file, so all four credentials must be in
`deploy/.env` before the first `pull`.

`FIBER_VERSION` in `deploy/.env` picks the tag. It defaults to `latest`; pin an exact
version for a reproducible deployment. To build from the working tree instead, add
`--build`, which overrides the published image with a local one.

`fiber-ui` is still built locally, deliberately: its API URL is baked in at build time, so
a generic image would only work for whoever's URL was compiled in.

Before the first tag:

- **The repository must be public** for the documented install paths to work. Release assets and
  `raw.githubusercontent.com` URLs 404 for anonymous callers on a private repo, and GitHub-hosted
  arm64 runners are free only on public repos — a private repo would queue that leg until it times out.
- **Make the GHCR packages public once.** The first push creates each package private, whatever the
  repository's visibility; flip it in the package settings or `docker pull` needs a token.

The **UI image is not published**: `VITE_FIBER_API_URL` is baked in at build time, so a generic
image would only work for whoever's URL was compiled in. Build it per deployment:

```bash
docker build -f apps/ui/Dockerfile.ui --build-arg VITE_FIBER_API_URL=https://ci.example.com \
  -t fiber-ui apps/ui
```

Container images build from one `deploy/Dockerfile`:

```bash
docker build -f deploy/Dockerfile --target fiber-api   -t fiber-api   .
docker build -f deploy/Dockerfile --target fiber-agent -t fiber-agent .
```

Both share a cargo-chef dependency layer, so a source-only change does not rebuild the whole
dependency graph. `CHANGELOG.md` records what each version contains.

## Health checks

- `GET /health` — process up.
- `GET /ready` — what the Compose healthcheck and a load balancer should use. `200` when
  Postgres answers and every supervised loop is running; `503` otherwise. Each dependency
  probe is bounded at **2 s**, so a hung Postgres shows up as `"postgres": "error"` inside
  the orchestrator's own probe window instead of as an unexplained probe timeout. Failing
  checks report `"error"` only; the cause is in the API log.

Redis is reported but does not fail the probe. Leases, scheduling, the queue, and lease
reclaim all live in Postgres, so a replica without Redis still runs builds. `/ready`
answers `200` with `"redis": "degraded"` and a top-level `"degraded": true`:

```json
{"checks": {"loops": "ok", "postgres": "ok", "redis": "degraded"}, "degraded": true, "ok": true}
```

Alert on `degraded`; do not pull the replica for it. While Redis is down:

- **Live run views stall across replicas.** `/ws/runs/{id}` still receives events raised
  on the replica it is connected to (they go through an in-process broadcast first), but
  nothing from the others. Reloading the page catches up from Postgres.
- **Agent commands do not fan out.** Run cancel, token rotation and agent deletion reach
  an agent only when it is connected to the replica that handled the request; otherwise
  they take effect when the agent reconnects or its lease expires.
- **Status updates land a little later.** Each event publish waits out one reconnect
  attempt with a 1 s connect timeout (about two seconds in all) before giving up on
  that event. Handlers that publish sit under the 30 s request timeout, which is why the
  client is bounded this tightly rather than left at its 13 s default.
- **External `fiber:events` consumers see a gap.**

Everything reconnects on its own when Redis is back: the client-side manager retries per
command, and the `events` and `agent_cmds` subscriber loops retry every 2 s. The same
holds at boot — an unreachable Redis no longer makes `fiber-api` crash-loop; it logs
`redis unreachable at boot; starting degraded` and comes up. If a broken live view
matters more to you than build throughput, have the load balancer also require
`"degraded": false` in the body.

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
- **Concurrency groups** — a run start or retry in a group takes `pg_advisory_xact_lock` on `(project, group)` for its transaction and decides what it supersedes inside it, so two replicas starting runs in one group cannot both keep running.
- **Durable fibers** — ready fibers are claimed with `UPDATE ... FOR UPDATE SKIP LOCKED`; a fiber runs on one replica per attempt.
- **Redis `fiber:events`** — run/step/log events fan out so `/ws/runs/{id}` subscribers on any replica see them.
- **Redis `fiber:agent_cmds`** — agent-directed messages (run cancel, token rotation / delete disconnects) fan out so the replica holding the agent's socket delivers them.

Agent presence (labels, concurrency) is per replica: an agent is offered steps by the replica it is connected to. Its in-flight count is not — it is `SELECT COUNT(*) … WHERE agent_id = … AND status = 'running'` at offer time, so a cancel or completion that only another replica saw still frees the slot. Redis is not a queue — queued steps live in Postgres and are pulled, oldest-queued first, on each agent heartbeat until the agent is full, which is why a replica keeps building with Redis down ([Health checks](#health-checks) lists what it loses).

## Smoke scripts

```bash
python3 scripts/smoke_authz_agents.py   # roles + agent CRUD/rotate
python3 scripts/smoke_agent_pools.py    # project-scoped vs global agents
python3 scripts/smoke_artifacts.py      # artifacts + path filters
python3 scripts/smoke_s3_presign.py     # MinIO presign upload/restore/download
bash scripts/smoke_compose.sh           # full compose up --build smoke
```

Expect `SMOKE_OK` on success.

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
- **Since 0.6.1 a pipeline compiles only if every `image` is a docker image reference and `workspace.repo` is an `http(s)`, `ssh`, `git`, or `file` URL, an scp-like `user@host:path`, or a path.** A stored pipeline that fails this stops producing runs — a manual or webhook start returns the error; a cron or interval pipeline skips its occurrence with a server-side warning only. Before upgrading, run `fiber validate` on each `fiber.yml` (or re-save each pipeline in the UI afterwards) so a legacy value is found before a schedule is missed  
- Redis no longer carries a step queue; after upgrading, `DEL fiber:ready_steps` removes the orphaned list left by older versions  
- **Step timeouts apply to runs already in flight at upgrade.** Steps without `timeout_minutes` get `FIBER_STEP_TIMEOUT_DEFAULT_MINUTES` (60); a build that legitimately runs longer must set `timeout_minutes` on the step (or raise the default) *before* upgrading, or its in-flight attempt will be failed by the backstop  
- Migration `007_indexes.sql` adds nine indexes (`step_runs`, `artifacts`, `log_lines`, `sessions`, `runs`, `step_attempts`). It runs at the first boot of the new version and holds a `SHARE` lock on each table while that index builds — writes to `log_lines` pause for the duration, which is seconds on a typical install. On a very large `log_lines` table, run retention first or apply the statements by hand with `CREATE INDEX CONCURRENTLY` before upgrading (the migration's `IF NOT EXISTS` then skips them).

- Migration `015_transition_indexes.sql` does four things at the first boot of the new version, all inside one transaction:
  - **Deletes duplicate artifact rows** before adding a unique index on `artifacts (step_run_id, name)`. Duplicates came from at-least-once re-uploads of the same artifact; the newest row of each pair is kept, and since both pointed at the same blob path no blob is orphaned. Nothing else is deleted. The one exception: an install that switched artifact backends (local ↔ S3) between two attempts of the same step has rows pointing at two different paths, and the older blob is left unreferenced — a cosmetic disk leak, not a data loss.
  - Adds `CHECK` constraints on `runs.status` and `step_runs.status` as **`NOT VALID`**: every insert and update from then on is checked, but existing rows are not scanned, so an old database with a stray status string still boots. Once the install is known clean, validate by hand — `ALTER TABLE runs VALIDATE CONSTRAINT runs_status_check; ALTER TABLE step_runs VALIDATE CONSTRAINT step_runs_status_check;` — which takes a `SHARE UPDATE EXCLUSIVE` lock and does not block writes.
  - Adds three indexes (`runs`, `artifacts`, `fibers`) and drops `idx_log_lines_step`, which nothing has read since 010. The builds hold a `SHARE` lock on their table for seconds on a typical install; `log_lines`, the only large table, is not indexed here.
  - The migration is idempotent (`IF NOT EXISTS` throughout), so a partially-applied environment converges on the next boot.
- After 015, **a lost lease counts against `retries`** (see *Stuck runs*): a step fails once it has lost more than `retries + 1` leases. A step that was being re-leased forever before the upgrade fails on its next reclaim and its run finalises; nothing else about in-flight runs changes.
- **A disconnect no longer requeues** (see [Shutdown and deploys](#shutdown-and-deploys)). Upgrade the API before the agents: a new agent against an old server behaves as before (it stops its steps on any close, because the old server sends no `lease_secs`), and an old agent against a new server is requeued on disconnect as before. Only new-on-new keeps a step running through a reconnect. While some API replicas are still old, their disconnect and stale-sweep paths still requeue.
- Migration `016_queue_order.sql` adds one partial index, `step_runs (queued_at, id) WHERE status = 'queued'`, drops `idx_step_runs_queued` (no reader left), and backfills `queued_at` on any queued row a pre-012 path left without one. Seconds on any install; idempotent.
- After 016, offers are made **oldest-queued first**, and an agent is offered steps until it is full on every heartbeat. A backlog that had been draining newest-run-first drains in queue order from the first heartbeat after the upgrade; nothing about in-flight steps changes.
- **`fiber-api` now runs as uid 10001, not root.** New deployments need nothing. An existing deployment whose artifact directory was created by a root container has to hand it over once, or every artifact upload fails with `Permission denied`:

  ```bash
  # Compose, local-filesystem artifacts. A plain `docker run`, not `compose run`:
  # the fiber-api service drops every capability, so even --user 0 inside it gets
  # EPERM from chown. The volume is <compose project>_fiber_artifacts, and the
  # project is named `fiber` in the Compose file.
  docker run --rm -v fiber_fiber_artifacts:/data/artifacts alpine \
    chown -R 10001:10001 /data/artifacts
  # A bind mount or a host-run API instead: chown the directory itself
  sudo chown -R 10001:10001 /srv/fiber/artifacts
  ```

  Whether this affects a Compose install depends on which backend it used. Until this version Compose set `FIBER_S3_BUCKET` unconditionally, so blobs went to MinIO and the `fiber_artifacts` volume held nothing — but the same upgrade **makes the local filesystem the default**, so an install that says nothing in `deploy/.env` switches backends and starts writing to that root-owned volume. Either run the `chown` above, or keep MinIO by setting `FIBER_S3_BUCKET=fiber-artifacts` in `deploy/.env` and bringing the stack up with `--profile minio`. Artifacts already in MinIO are not copied to the local disk (or the other way round); the rows keep pointing at the backend that stored them, so switching leaves older artifacts undownloadable until you switch back.
- **The four Compose credentials are now required.** `FIBER_POSTGRES_PASSWORD`, `FIBER_REDIS_PASSWORD`, `FIBER_S3_ACCESS_KEY`/`FIBER_S3_SECRET_KEY` and `FIBER_ADMIN_PASSWORD` no longer default to `fiber` / `fiberfiber`; `docker compose` refuses to read the file until `deploy/.env` sets them. An existing deployment that relied on the defaults must write those same values into `deploy/.env` before the upgrade — Postgres keeps the password its volume was initialised with, and Redis keeps whatever the new config file says, so putting the old values back is the no-downtime path. Changing the Postgres password needs `ALTER ROLE fiber PASSWORD …` inside the container as well as the `.env` edit; `FIBER_ADMIN_PASSWORD` is read only on the first boot of an empty database, so for an existing instance any placeholder is fine.
- Postgres major bumps (e.g. 16 → 17): Compose volume recreate (`down -v`) if needed  
- Agent tokens are hashes only — rotating requires distributing a new plaintext token
