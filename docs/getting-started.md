# Getting started

## Prerequisites

- Rust (stable, edition **2024**), Docker, Node/pnpm (for the UI)
- Or use Make: `make help` — see [development](./development.md)

## 1. Infrastructure

```bash
make infra
# optional MinIO for S3 artifacts:
make infra-minio
```

Ports: Postgres **15432**, Redis **16379**, MinIO **19000** / **19001**.

## 2. API

```bash
make api
# or with MinIO:
make api-s3
```

Equivalent manual env: `source scripts/dev-env.sh` then `cargo run -p fiber-api`.
- API: http://127.0.0.1:18080
- Login: **admin** / **fiber** (override with `FIBER_ADMIN_USER` / `FIBER_ADMIN_PASSWORD`)
- Migrations apply on boot; a **Showcase** project is seeded for the admin owner

## 3. Web UI

```bash
cd apps/web && pnpm install
VITE_FIBER_API_URL=http://127.0.0.1:18080 pnpm dev
```

Open http://localhost:3100 — create or open a pipeline on the canvas, **Save**, **Run**.

## 4. Agent

Register an agent in the UI (**Agents**) or:

```bash
curl -s -X POST http://127.0.0.1:18080/api/agents \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"name":"local","labels":["os=linux"],"concurrency":1}'
```

Run the worker (token shown once at create/rotate):

```bash
export FIBER_AGENT_TOKEN=…          # from register response
export FIBER_API_URL=ws://127.0.0.1:18080
export FIBER_AGENT_USE_DOCKER=false # shell executor for local dogfood
cargo run -p fiber-agent
```

Steps match agents by **labels** (e.g. `os=linux` on both the step and the agent).

## 5. CLI

```bash
cargo run -p fiber-cli -- validate examples/fiber.yml
cargo run -p fiber-cli -- login
cargo run -p fiber-cli -- run $PIPELINE_ID
cargo run -p fiber-cli -- members list $PROJECT_ID
cargo run -p fiber-cli -- agents create --name local --labels os=linux
```

More: [CLI](./cli.md).

## Full Compose

```bash
cp deploy/.env.example deploy/.env
sed -i'' -e "s/^FIBER_SECRETS_KEY=.*/FIBER_SECRETS_KEY=$(openssl rand -hex 32)/" deploy/.env   # keep a backup of this key
docker compose -f deploy/docker-compose.yml up --build
```

Everything binds to `127.0.0.1` by default; see [operations → Deployment](./operations.md#deployment) for the reverse-proxy / TLS setup and how to expose it. MinIO artifacts are on by default in Compose. See [Artifacts](./artifacts.md).

To run pipelines on the Compose stack, add a worker: create an agent (Agents page or
`fiber agents create --name compose --labels os=linux`), put its token in `deploy/.env` as
`FIBER_AGENT_TOKEN`, then:

```bash
docker compose -f deploy/docker-compose.yml --profile agent up -d fiber-agent
```

For a separate build machine, see [agents → Install](./agents.md#install).

## Next

- [Pipeline YAML](./pipeline-yaml.md)
- [Agents](./agents.md)
- [Configuration](./configuration.md)
