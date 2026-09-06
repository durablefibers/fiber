# Configuration

All product env vars use the `FIBER_*` prefix (plus standard OTEL names).

## fiber-api

| Variable | Default | Purpose |
|---|---|---|
| `FIBER_DATABASE_URL` | `postgres://fiber:fiber@localhost:15432/fiber` | Postgres |
| `FIBER_REDIS_URL` | `redis://localhost:16379` | Redis (events + coordination). The Compose Redis requires a password: `redis://:fiber@127.0.0.1:16379` (`scripts/dev-env.sh` sets this) |
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
| `FIBER_GITHUB_TOKEN` | unset | Fallback for PR file API |
| `FIBER_GITHUB_API_URL` | `https://api.github.com` | GitHub / GHE API base |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | unset | Enable OTLP HTTP traces/metrics |
| `FIBER_OTEL_ENDPOINT` | unset | Alias for OTEL endpoint |
| `RUST_LOG` | — | Tracing filter |

Login is throttled per username: 10 failures within 10 minutes lock that username for 60 s (`429` with `Retry-After`); after a lockout the count restarts, so the sustained ceiling is about 10 guesses per minute per username per API instance. Because the key is the username, anyone can keep a known username (including `admin`) locked — an accepted trade-off over IP keying, which is spoofable behind a proxy. Error responses never include database or Redis error text; details go to the API log.

## Compose (`deploy/.env`)

| Variable | Default | Purpose |
|---|---|---|
| `FIBER_SECRETS_KEY` | empty (plaintext, warned) | Passed through to `fiber-api` |
| `FIBER_ADMIN_USER` / `FIBER_ADMIN_PASSWORD` | `admin` / `fiber` | Bootstrap admin — applied only on the first boot of an empty database; rotate an existing admin through the API/UI |
| `FIBER_POSTGRES_PASSWORD` | `fiber` | Postgres password (loopback-bound port `15432`); applied when the volume is first initialised |
| `FIBER_REDIS_PASSWORD` | `fiber` | Redis `requirepass` (loopback-bound port `16379`) |
| `FIBER_S3_ACCESS_KEY` / `FIBER_S3_SECRET_KEY` | `fiber` / `fiberfiber` | MinIO root + API credentials |
| `FIBER_S3_PUBLIC_ENDPOINT` | `http://127.0.0.1:19000` | Presign host for agents/browsers |
| `FIBER_CORS_ORIGINS` | `http://localhost:3100,http://127.0.0.1:3100` | See above |
| `FIBER_API_BIND` / `FIBER_WEB_BIND` | `127.0.0.1` | Host interface for `18080` / `3100`; set `0.0.0.0` only without a reverse proxy |
| `VITE_FIBER_API_URL` | `http://localhost:18080` | Baked into the web bundle |
| `RUST_LOG` | `info,fiber_api=info` | API log filter |

## fiber-agent

| Variable | Default | Purpose |
|---|---|---|
| `FIBER_API_URL` | `ws://127.0.0.1:18080` | API base (ws or http; agent derives HTTP) |
| `FIBER_AGENT_TOKEN` | required | Agent token |
| `FIBER_AGENT_NAME` | `local` | Display name in Hello |
| `FIBER_AGENT_LABELS` | `os=linux,docker=true` | Comma-separated |
| `FIBER_AGENT_CONCURRENCY` | `1` | Max parallel steps |
| `FIBER_AGENT_USE_DOCKER` | `true` | Docker vs host shell |
| `FIBER_AGENT_WORKSPACE_DIR` | `./data/workspaces` | Per-run work dirs |

Installed by `scripts/install-agent.sh` into `/etc/fiber/agent.env` (root-owned, mode `0640`);
in Compose they come from `deploy/.env` (`FIBER_AGENT_TOKEN`, `FIBER_AGENT_NAME`,
`FIBER_AGENT_LABELS`, `FIBER_AGENT_CONCURRENCY`, `FIBER_AGENT_USE_DOCKER`).

## fiber-cli / web

| Variable | Default | Purpose |
|---|---|---|
| `FIBER_API_URL` | `http://127.0.0.1:18080` | CLI API base |
| `FIBER_TOKEN` | ~/.fiber/token | Session token |
| `VITE_FIBER_API_URL` | `http://127.0.0.1:18080` | Web UI API base at build/dev time |
