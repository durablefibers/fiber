# Configuration

All product env vars use the `FIBER_*` prefix (plus standard OTEL names).

## fiber-api

| Variable | Default | Purpose |
|---|---|---|
| `FIBER_DATABASE_URL` | `postgres://fiber:fiber@localhost:15432/fiber` | Postgres |
| `FIBER_REDIS_URL` | `redis://localhost:16379` | Redis (events + coordination) |
| `FIBER_LISTEN` | `0.0.0.0:18080` | Bind address |
| `FIBER_ARTIFACTS_DIR` | `./data/artifacts` | Local artifact root |
| `FIBER_ADMIN_USER` | `admin` | Bootstrap admin username |
| `FIBER_ADMIN_PASSWORD` | `fiber` | Bootstrap admin password |
| `FIBER_SECRETS_KEY` | unset | 64 hex chars → encrypt project secrets |
| `FIBER_S3_BUCKET` | unset | If set, use S3/MinIO backend |
| `FIBER_S3_ENDPOINT` | `http://127.0.0.1:19000` | S3 API endpoint |
| `FIBER_S3_PUBLIC_ENDPOINT` | = endpoint | Presign host agents can reach |
| `FIBER_S3_REGION` | `us-east-1` | Region |
| `FIBER_S3_ACCESS_KEY` | `fiber` | Access key |
| `FIBER_S3_SECRET_KEY` | `fiberfiber` | Secret key |
| `FIBER_RETENTION_DAYS` | `30` | `0` disables age GC |
| `FIBER_RETENTION_KEEP_RUNS` | `20` | Newest terminal runs kept per pipeline |
| `FIBER_RETENTION_BATCH` | `100` | Max runs deleted per tick |
| `FIBER_RETENTION_INTERVAL_SECS` | `3600` | GC loop period (min 60) |
| `FIBER_AGENT_STALE_SECS` | `45` | Offline threshold for heartbeats |
| `FIBER_GITHUB_TOKEN` | unset | Fallback for PR file API |
| `FIBER_GITHUB_API_URL` | `https://api.github.com` | GitHub / GHE API base |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | unset | Enable OTLP HTTP traces/metrics |
| `FIBER_OTEL_ENDPOINT` | unset | Alias for OTEL endpoint |
| `RUST_LOG` | — | Tracing filter |

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

## fiber-cli / web

| Variable | Default | Purpose |
|---|---|---|
| `FIBER_API_URL` | `http://127.0.0.1:18080` | CLI API base |
| `FIBER_TOKEN` | ~/.fiber/token | Session token |
| `VITE_FIBER_API_URL` | `http://127.0.0.1:18080` | Web UI API base at build/dev time |
