# Configuration

All product env vars use the `FIBER_*` prefix (plus standard OTEL names).

## fiber-api

| Variable | Default | Purpose |
|---|---|---|
| `FIBER_DATABASE_URL` | `postgres://fiber:fiber@localhost:15432/fiber` | Postgres |
| `FIBER_REDIS_URL` | `redis://localhost:16379` | Redis (events + coordination). The Compose Redis requires a password (`FIBER_REDIS_PASSWORD`, no default — `make infra` passes `fiber` on a dev box): `redis://:fiber@127.0.0.1:16379`, which is what `scripts/dev-env.sh` builds |
| `FIBER_LISTEN` | `0.0.0.0:18080` | Bind address |
| `FIBER_ARTIFACTS_DIR` | `./data/artifacts` | Local artifact root |
| `FIBER_ADMIN_USER` | `admin` | Bootstrap admin username |
| `FIBER_ADMIN_PASSWORD` | `fiber` | Bootstrap admin password (API warns at boot while it is the default) |
| `FIBER_CORS_ORIGINS` | `http://127.0.0.1:3100,http://localhost:3100` | Comma-separated browser origins allowed to call the API; `*` = any origin |
| `FIBER_SECRETS_KEY` | unset | 64 hex chars → encrypt project **and webhook** secrets at rest |
| `FIBER_S3_BUCKET` | unset | If set, use S3/MinIO backend |
| `FIBER_S3_ENDPOINT` | `http://127.0.0.1:19000` | S3 API endpoint |
| `FIBER_S3_PUBLIC_ENDPOINT` | = endpoint | Presign host agents can reach |
| `FIBER_S3_REGION` | `us-east-1` | Region |
| `FIBER_S3_ACCESS_KEY` | required with bucket | Access key (no default; boot fails if missing) |
| `FIBER_S3_SECRET_KEY` | required with bucket | Secret key (no default; boot fails if missing) |
| `FIBER_RETENTION_DAYS` | `30` | `0` disables age GC |
| `FIBER_RETENTION_KEEP_RUNS` | `20` | Newest terminal runs kept per pipeline |
| `FIBER_RETENTION_BATCH` | `100` | Max runs deleted per tick |
| `FIBER_RETENTION_INTERVAL_SECS` | `3600` | GC loop period (min 60) |
| `FIBER_AGENT_STALE_SECS` | `45` | Offline threshold for heartbeats |
| `FIBER_STEP_TIMEOUT_DEFAULT_MINUTES` | `60` | Per-attempt limit for steps without `timeout_minutes` |
| `FIBER_STEP_TIMEOUT_GRACE_MINUTES` | `5` | Extra minutes the server waits past a step's limit before failing it itself (backstop for hung/old agents) |
| `FIBER_GITHUB_TOKEN` | unset | Fallback for the PR file API and commit statuses (needs `repo:status`) |
| `FIBER_PUBLIC_URL` | unset | Public base URL of this Fiber; used to link commit statuses to the run page |
| `FIBER_GITHUB_API_URL` | `https://api.github.com` | GitHub / GHE API base |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | unset | Collector **base** URL; `/v1/traces` and `/v1/metrics` are appended |
| `FIBER_OTEL_ENDPOINT` | unset | Alias for OTEL endpoint |
| `FIBER_METRICS_TOKEN` | unset | Bearer token for `GET /metrics`. Unset = the endpoint returns `404` |
| `FIBER_HTTP_TASK_ALLOW_PRIVATE` | `0` | Let the `http_request` fiber task reach private, loopback and link-local addresses. See [durable fibers](./durable-fibers.md) |
| `FIBER_STEP_LOG_MAX_LINES` | `50000` | Lines stored per step attempt; the rest are dropped with one line saying so. `0` disables the cap |
| `FIBER_RETENTION_FIBER_DAYS` | `7` | Delete terminal durable fibers older than this. `0` disables. Suspended fibers are never touched |
| `RUST_LOG` | — | Tracing filter |

Login is throttled per username: 10 failures within 10 minutes lock that username for 60 s (`429` with `Retry-After`); after a lockout the count restarts, so the sustained ceiling is about 10 guesses per minute per username per API instance. Because the key is the username, anyone can keep a known username (including `admin`) locked — an accepted trade-off over IP keying, which is spoofable behind a proxy. Error responses never include database or Redis error text; details go to the API log.

## Compose (`deploy/.env`)

`deploy/.env.example` is the copy-and-fill version of this table; every variable below is
in it, and nothing reaches a container that is not in `deploy/docker-compose.yml`'s env
block. An empty value means "the API's own default" from the table above — the Compose
file passes the variable through as an empty string and each reader falls back.

**Required — no default.** `docker compose config`, and therefore `up`, `pull` and
`logs`, fails until `deploy/.env` gives each of these a value.

| Variable | Purpose |
|---|---|
| `FIBER_POSTGRES_PASSWORD` | Postgres password (loopback-bound port `15432`); applied when the volume is first initialised, so changing it later also needs `ALTER ROLE` |
| `FIBER_REDIS_PASSWORD` | Redis `requirepass` (loopback-bound port `16379`), delivered through a Compose `configs:` file, not argv |
| `FIBER_S3_ACCESS_KEY` / `FIBER_S3_SECRET_KEY` | MinIO root credentials and what `fiber-api` signs S3 requests with. Required even with the `minio` profile off: Compose interpolates the whole file before it filters by profile. Any placeholder does when `FIBER_S3_BUCKET` is empty |
| `FIBER_ADMIN_PASSWORD` | Bootstrap admin password — applied only on the first boot of an empty database; rotate an existing admin through the API/UI |

**Optional.**

| Variable | Default | Purpose |
|---|---|---|
| `FIBER_VERSION` | `latest` | Tag for the `ghcr.io/durablefibers/fiber-api` and `fiber-agent` images |
| `FIBER_SECRETS_KEY` | empty (plaintext, warned) | Passed through to `fiber-api` |
| `FIBER_ADMIN_USER` | `admin` | Bootstrap admin username |
| `FIBER_S3_BUCKET` | empty | Empty = local filesystem on the `fiber_artifacts` volume. Set it (and run `--profile minio`, or point the endpoint at real S3) to use object storage |
| `FIBER_S3_ENDPOINT` | `http://fiber-minio:9000` | S3 API endpoint as the API reaches it |
| `FIBER_S3_PUBLIC_ENDPOINT` | `http://127.0.0.1:19000` | Presign host for agents/browsers; the loopback default only works for an agent on this host |
| `FIBER_S3_REGION` | `us-east-1` | Region |
| `FIBER_CORS_ORIGINS` | `http://localhost:3100,http://127.0.0.1:3100` | See above |
| `FIBER_API_BIND` / `FIBER_UI_BIND` | `127.0.0.1` | Host interface for `18080` / `3100`; set `0.0.0.0` only without a reverse proxy |
| `VITE_FIBER_API_URL` | `http://localhost:18080` | Baked into the UI bundle |
| `FIBER_PUBLIC_URL` | empty | Public base URL; commit statuses link back to it |
| `FIBER_GITHUB_TOKEN` | empty | Instance-wide fallback for the PR file API and commit statuses |
| `FIBER_GITHUB_API_URL` | empty (`https://api.github.com`) | GitHub Enterprise API base |
| `FIBER_RETENTION_DAYS` / `_KEEP_RUNS` / `_BATCH` / `_INTERVAL_SECS` | empty (`30` / `20` / `100` / `3600`) | Run GC; see the API table |
| `FIBER_RETENTION_FIBER_DAYS` | `7` | Durable-fiber GC |
| `FIBER_AGENT_STALE_SECS` | empty (`45`) | Offline threshold for heartbeats |
| `FIBER_STEP_TIMEOUT_DEFAULT_MINUTES` / `_GRACE_MINUTES` | empty (`60` / `5`) | Step timeout and the server-side backstop |
| `FIBER_STEP_LOG_MAX_LINES` | `50000` | Lines stored per step attempt |
| `FIBER_HTTP_TASK_ALLOW_PRIVATE` | `0` | `http_request` fiber task reach |
| `FIBER_METRICS_TOKEN` | empty | Bearer token for `/metrics`; empty = off |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | empty | OTLP collector base URL. Use this name in Compose, not `FIBER_OTEL_ENDPOINT`: the API reads the alias only when this one is *absent*, and Compose always defines what it passes |
| `RUST_LOG` | `info,fiber_api=info` | API log filter |
| `FIBER_AGENT_TOKEN` / `_NAME` / `_LABELS` / `_CONCURRENCY` | — / `compose` / `os=linux` / `2` | The optional `agent` profile |

## fiber-agent

| Variable | Default | Purpose |
|---|---|---|
| `FIBER_API_URL` | `ws://127.0.0.1:18080` | API base (ws or http; agent derives HTTP) |
| `FIBER_AGENT_TOKEN` | required | Agent token |
| `FIBER_AGENT_NAME` | `local` | Display name in Hello |
| `FIBER_AGENT_LABELS` | `os=linux,docker=true` | Comma-separated |
| `FIBER_AGENT_CONCURRENCY` | `1` | Max parallel steps |
| `FIBER_AGENT_USE_DOCKER` | `true` | Docker vs host shell |
| `FIBER_AGENT_WORKSPACE_DIR` | `./data/workspaces` | Root for per-step work dirs |
| `FIBER_AGENT_ENV_PASSTHROUGH` | empty | Comma-separated names to pass from the agent's environment into steps, on top of the built-in allowlist (shell basics, proxy and CA settings). Everything else is cleared — use this for `SSH_AUTH_SOCK`, `CARGO_HOME`, `JAVA_HOME`, `NVM_DIR` and similar. **Under systemd, `SSH_AUTH_SOCK` needs a drop-in** — see below |
| `FIBER_AGENT_WORKSPACE_TTL_HOURS` | `24` | Sweep run workspaces older than this at startup; `0` disables |
| `FIBER_AGENT_DOCKER_USER` | image default | `--user` for step containers |
| `FIBER_AGENT_DOCKER_NETWORK` | `bridge` | `--network`; `none` isolates steps from the network |
| `FIBER_AGENT_DOCKER_MEMORY` | unlimited | `--memory` for step containers, e.g. `2g`. Off by default so an upgrade cannot start OOM-killing existing builds |
| `FIBER_AGENT_DOCKER_CPUS` | unlimited | `--cpus` for step containers, e.g. `2` |
| `FIBER_AGENT_DOCKER_PIDS_LIMIT` | `512` | `--pids-limit`; `0` = unlimited |
| `GIT_ALLOW_PROTOCOL` | `file:git:http:https:ssh` | Transports git may use for the workspace fetch. Set by the agent when absent so the `ext::` transport (which runs a command) is unreachable; an operator's own value is kept as is |

Installed by `scripts/install-agent.sh` into `/etc/fiber/agent.env` (root-owned, mode `0640`);
in Compose they come from `deploy/.env` (`FIBER_AGENT_TOKEN`, `FIBER_AGENT_NAME`,
`FIBER_AGENT_LABELS`, `FIBER_AGENT_CONCURRENCY`, `FIBER_AGENT_USE_DOCKER`). The installer also writes
`FIBER_AGENT_INSTALLED_VERSION` and `FIBER_AGENT_INSTALLED_SHA256` there so a re-run can say
what it is replacing; no crate reads them, and they are not deployment configuration.

### `SSH_AUTH_SOCK` under the systemd unit

`deploy/fiber-agent.service` sets `ProtectHome=true`, which makes `/home`, `/root` and
`/run/user/*` inaccessible to the agent and to every step it runs. A login session's SSH
agent socket lives under `/run/user/<uid>`, so

```
FIBER_AGENT_ENV_PASSTHROUGH=SSH_AUTH_SOCK
```

passes the *name* of a socket the step cannot open, and git fails with a confusing
"Permission denied (publickey)" rather than a missing-file error.

This is deliberate — a build host reaching into a human's login session is what
`ProtectHome=` exists to prevent — so the unit keeps it and you opt out per host. Re-expose
exactly the socket in a drop-in, never the whole directory:

```ini
# /etc/systemd/system/fiber-agent.service.d/ssh-agent.conf
[Service]
BindPaths=/run/user/1000/keyring/ssh
Environment=FIBER_AGENT_ENV_PASSTHROUGH=SSH_AUTH_SOCK
```

`systemctl daemon-reload && systemctl restart fiber-agent`, then prove it with a one-line
step (`ssh-add -l`) before relying on it: the socket path differs between distributions and
desktop environments, and it changes when that user logs out. A deploy key in
`/var/lib/fiber/.ssh` with a `core.sshCommand` in the pipeline is the more durable answer
for an unattended build host. A drop-in also survives the next `install-agent.sh` run;
edits to the unit file itself do not.

## fiber-cli / ui

| Variable | Default | Purpose |
|---|---|---|
| `FIBER_API_URL` | `http://127.0.0.1:18080` | CLI API base |
| `FIBER_TOKEN` | ~/.fiber/token | Session token |
| `VITE_FIBER_API_URL` | `http://127.0.0.1:18080` | UI API base at build/dev time |
