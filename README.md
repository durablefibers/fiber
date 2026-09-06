# Fiber — Durable CI

Self-hosted, canvas-first Jenkins alternative. Rust control plane, TypeScript UI (TanStack Start + React Flow + shadcn).

**Docs:** [docs/](./docs/) — getting started, [development](./docs/development.md) (`make help`), architecture, pipeline YAML, agents, artifacts, authz, [CLI](./docs/cli.md), API, operations, [roadmap](./docs/roadmap.md).

## Quick DX

```bash
make help
make infra          # Postgres + Redis
make api            # :18080  (or make api-s3 after make infra-minio)
make web            # :3100
make login
```

Login **admin** / **fiber**. Env template: [`.env.example`](./.env.example).

## Agents vs Fibers

| | **Agents** | **Fibers** |
|---|---|---|
| What | Worker processes that run CI pipeline steps | Durable control-plane background tasks |
| Where | Global **Agents** nav; `fiber-agent` binary | Per-project **Fibers** page; `fiber-durable` |
| Primitives | Lease, shell/docker, logs, artifacts | `step` / `stash` / `sleep`, resume after crash |

## Durability & security

See [docs/operations.md](./docs/operations.md), [docs/authz.md](./docs/authz.md), and [docs/artifacts.md](./docs/artifacts.md) for full detail.

- **CI steps are at-least-once.** Make `run` side effects idempotent.
- **Schema** via versioned sqlx migrations in `crates/fiber-core/migrations/` (applied on API boot).
- **Schedules**: `on.interval_minutes` and/or `on.cron` (6-field with seconds). Cron wins if both set — [triggers](./docs/triggers.md).
- **GitHub webhooks**: push/PR + path filters; PR paths need `GITHUB_TOKEN` — [triggers](./docs/triggers.md).
- **Matrix / if**: [pipeline YAML](./docs/pipeline-yaml.md).
- **Project roles**: `reader` < `writer` < `admin` < `owner` — [authz](./docs/authz.md).
- **Secrets**: encrypt at rest with `FIBER_SECRETS_KEY` (64 hex chars).
- **Artifacts**: local or S3 presign — [artifacts](./docs/artifacts.md).
- **Retention / agents / OTel**: [configuration](./docs/configuration.md) + [operations](./docs/operations.md).

```bash
openssl rand -hex 32
# export FIBER_SECRETS_KEY=<that>
```

## Durable fibers

Built-ins: `ping`, `sleep_demo`, `interval_task`. Details: [docs/durable-fibers.md](./docs/durable-fibers.md).

```bash
cargo run -p fiber-cli -- login
cargo run -p fiber-cli -- fibers create $PROJECT_ID --name sleep_demo --input '{"seconds":3}'
cargo run -p fiber-cli -- members list $PROJECT_ID
cargo run -p fiber-cli -- agents rotate $AGENT_ID
```

## Quick start (local)

Full walkthrough: [docs/getting-started.md](./docs/getting-started.md).

### Dogfood smokes

```bash
python3 scripts/dogfood_authz_agents.py   # roles + agent CRUD/rotate
python3 scripts/dogfood_artifacts.py      # artifacts + path filters (needs agent)
python3 scripts/dogfood_s3_presign.py     # MinIO presign (needs MinIO + FIBER_S3_*)
```

### 1. Infrastructure

```bash
docker compose -f deploy/docker-compose.yml up -d fiber-postgres fiber-redis
```

Postgres is **17-alpine**. Upgrading from 16: `down -v` then recreate.

### 2. API

```bash
export FIBER_DATABASE_URL=postgres://fiber:fiber@127.0.0.1:15432/fiber
export FIBER_REDIS_URL=redis://:fiber@127.0.0.1:16379   # Compose Redis password (FIBER_REDIS_PASSWORD)
export FIBER_SECRETS_KEY=$(openssl rand -hex 32)   # recommended
cargo run -p fiber-api
```

API: **http://127.0.0.1:18080**. Login **admin** / **fiber**.

### 3. Web UI

```bash
cd apps/web && pnpm install && VITE_FIBER_API_URL=http://127.0.0.1:18080 pnpm dev
```

Open http://localhost:3100

### 4. CLI / agent

```bash
cargo run -p fiber-cli -- validate examples/fiber.yml
cargo run -p fiber-cli -- login
cargo run -p fiber-cli -- agent --token "$FIBER_AGENT_TOKEN"
```

## Full Compose

```bash
cp deploy/.env.example deploy/.env      # set FIBER_SECRETS_KEY (openssl rand -hex 32) and FIBER_ADMIN_PASSWORD
docker compose -f deploy/docker-compose.yml up --build
```

- UI: http://localhost:3100 · API: http://localhost:18080 (both loopback-only by default — see [operations](docs/operations.md#deployment) for TLS / exposure)
- Postgres **15432** · Redis **16379** · MinIO **19000** (loopback-only)

## Workspace

```
docs/                  # product documentation
apps/web/
crates/fiber-api/
crates/fiber-core/
crates/fiber-scheduler/
crates/fiber-agent/
crates/fiber-cli/      `fiber` binary
crates/fiber-durable/
crates/fiber-proto/
examples/              # sample fiber.yml pipelines
```

Naming: `fiber-*` / `FIBER_*` — see `.cursor/rules/naming.mdc`.
