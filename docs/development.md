# Local development

Day-to-day workflow for Fiber. Product docs: [README](./README.md) index.

## One-time

```bash
# Rust stable + Docker + pnpm
docker compose -f deploy/docker-compose.yml up -d fiber-postgres fiber-redis
# optional artifacts backend:
make infra-minio
```

Copy [`.env.example`](../.env.example) into your shell, or always use:

```bash
source scripts/dev-env.sh          # local FS artifacts
FIBER_USE_S3=1 source scripts/dev-env.sh   # MinIO
```

## Make targets

```bash
make help          # list
make infra         # Postgres + Redis
make build         # api + agent + cli
make api           # fiber-api :18080
make api-s3        # fiber-api with MinIO
make ui            # UI :3100
make agent         # needs FIBER_AGENT_TOKEN
make login         # session → ~/.fiber/token
make validate      # examples/fiber.yml
make check         # fmt --check + clippy -D warnings
make images        # build the fiber-api / fiber-agent container images
make test          # cargo test --workspace + apps/ui vitest
make smoke           # authz + artifacts + pools (pools last: it kills every agent)
make smoke-s3        # MinIO presign (api-s3 running)
make smoke-compose   # full Compose stack incl. a real pipeline on the containerised agent
make ready         # GET /ready
```

Login defaults: **admin** / **fiber**.

## Ports

| Service | Host |
|---|---|
| API | 18080 |
| UI (dev) | 3100 |
| Postgres | 15432 |
| Redis | 16379 |
| MinIO API / console | 19000 / 19001 |

## Naming

Product prefix is **`fiber`** / `FIBER_*` — see `.claude/rules/naming.md`. Never `df` / `durablefibers` in crates, env, or Compose service names.

## Layout

| Path | Role |
|---|---|
| `crates/fiber-*` | Control plane, agent, CLI, durable runtime |
| `apps/ui` | TanStack Start UI |
| `deploy/` | Compose + Dockerfiles |
| `scripts/` | Smoke scripts + `dev-env.sh` |
| `docs/` | User + ops docs |
| `examples/` | Sample `fiber.yml` |

## Quality gate

```bash
make check   # cargo fmt --check + clippy -D warnings
make test    # cargo test --workspace, then apps/ui vitest
cd apps/ui && pnpm check && pnpm exec tsc --noEmit && pnpm build   # ui lint/format, types, build
```

GitHub Actions (`.github/workflows/ci.yml`) runs, on push/PR to `main`: Rust fmt, clippy (`-D warnings`), `cargo test --workspace`, build; `apps/ui` Biome (`pnpm check`), `tsc --noEmit`, vitest, `pnpm build`; and a `docker build` of `deploy/Dockerfile` and `apps/ui/Dockerfile.ui`. The Rust toolchain is pinned by `rust-toolchain.toml` (kept in step with `deploy/Dockerfile`).

UI unit tests live next to their modules as `src/**/*.test.ts(x)` and run under `vitest.config.ts` (jsdom; `src/test-setup.ts` installs an in-memory `localStorage`).

## Agent tips

Prefer `make` / `scripts/dev-env.sh` over ad-hoc env in shell one-liners. Do **not** `pkill -f fiber-agent` — that can match parent shells whose argv mentions the binary; kill by PID of `./target/debug/fiber-agent` only (see smoke scripts).
