# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Fiber — self-hosted, canvas-first DAG CI ("Jenkins alternative"). Rust control plane + TypeScript UI. Repo folder is `durablefibers`; **all code, crates, binaries, env vars, and Docker services use the `fiber` / `FIBER_*` prefix** — never `df`, `durablefibers`, or `durable_fibers` (`.cursor/rules/naming.mdc`).

## Commands

Prefer `make` targets and `scripts/dev-env.sh` over ad-hoc env in shell one-liners (`.cursor/rules/dx.mdc`). `make help` lists everything.

```bash
make infra          # Postgres :15432 + Redis :16379 (docker compose, deploy/docker-compose.yml)
make infra-minio    # + MinIO :19000 / console :19001 (for S3 artifact path)
make api            # fiber-api on :18080 (sources scripts/dev-env.sh)
make api-s3         # same with FIBER_USE_S3=1
make web            # pnpm dev in apps/web on :3100
make agent          # fiber-agent (requires FIBER_AGENT_TOKEN; Make rewrites http→ws for FIBER_API_URL)
make check          # THE gate: cargo fmt --check + clippy -D warnings (same as CI)
make ready          # GET /ready
```

Login defaults: **admin / fiber**.

### Tests

Rust tests are pure unit tests (no DB/Redis needed) living in `#[cfg(test)]` modules in `fiber-core` (`dag`, `due_index`, `path_filter`, `schedule`, `secrets`, `step_if`, `tokens`) and `fiber-durable/src/tests.rs`.

```bash
cargo test -p fiber-core -p fiber-durable
cargo test -p fiber-core dag::            # one module
cargo test -p fiber-durable due_index_earliest_and_authoritative   # one test
cd apps/web && pnpm test                  # vitest
cd apps/web && pnpm exec tsc --noEmit     # what CI runs for the web app
```

End-to-end coverage is the **dogfood smokes**, not integration tests — they drive the live API and print `DOGFOOD_OK`:

```bash
make dogfood                # authz + agent pools + artifacts (needs infra + api running)
make dogfood-s3             # needs infra-minio + api-s3 + a built fiber-agent
make dogfood-compose        # full `compose up --build` smoke
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy (`-D warnings`), `cargo build -p fiber-api -p fiber-agent -p fiber-cli`, and `apps/web` `tsc --noEmit`.

## Architecture

Two independent durable systems share one Postgres and one API process — keep them straight:

| | **Agents** (CI) | **Fibers** (durable tasks) |
|---|---|---|
| Runs | Pipeline shell/Docker steps | Control-plane background work |
| Where | `fiber-agent` process, outbound WS to `/ws/agent`; global **Agents** nav | In-process in `fiber-api` via `FiberScheduler`; per-project **Fibers** page |
| Primitives | lease / offer / log stream / artifact upload | `step` / `stash` / `sleep`, resume after crash |

### Crates

- **`fiber-core`** — everything DB and domain: `store.rs` (one large `Store` impl, all SQL via runtime `sqlx::query_as` — **no `query!` macros**, so no `DATABASE_URL` needed at build time), `dag.rs` (definition → compiled DAG, matrix expansion), `models.rs`, `migrations/` (versioned sqlx migrations applied on API boot), plus `path_filter`, `schedule`, `step_if`, `secrets`, `tokens`, `roles`, `due_index`, `seed`.
- **`fiber-api`** — axum. `routes.rs` is the whole HTTP surface; `ws.rs` holds both `/ws/agent` (agent protocol) and `/ws/runs/{id}` (UI event stream); `access.rs` is the role-gate helper layer; `auth.rs` has the `AuthUser` (bearer session) and `AuthAgent` (agent token) extractors; also `artifacts.rs` (local FS or S3 presign backend), `retention.rs`, `github.rs`, `otel.rs`.
- **`fiber-scheduler`** — lease queue, agent registry/connections, offers, lease renewal, expired-lease reclaim, schedule loop, Redis `fiber:events` fan-out.
- **`fiber-agent`** — single-binary worker: connects out over WS, claims offers, prepares git workspace, restores artifacts, runs shell or Docker, streams `LogChunk`, uploads artifacts.
- **`fiber-durable`** — separate memoturn-style runtime: `engine.rs` (run/resume one fiber), `context.rs` (`FiberSuspended`), `store.rs`, `scheduler.rs` (poller + stale-heartbeat reclaim), `registry.rs`, `tasks.rs` (built-ins `ping`, `sleep_demo`, `interval_task`).
- **`fiber-proto`** — wire + YAML-facing types shared by API, agent, and CLI (`AgentMessage`, `ServerMessage`, `RunEvent`, `PipelineDefinition`, `StepDefinition`).
- **`fiber-cli`** — the `fiber` binary: `validate`, `login`, run, members, secrets, agents, fibers, and spawning an agent.

### Run lifecycle

Start run snapshots the pipeline definition onto the run (immutable for that execution) and creates `runs` + `step_runs` from the compiled DAG → scheduler queues root-ready steps and matches agent labels → connected agent gets an `Offer` (workspace, env, artifact restore list) → agent executes and streams logs → completion unlocks dependents or skips them (fail-fast) → events publish on Redis `fiber:events` and to `/ws/runs/{id}`.

**CI steps are at-least-once.** Leases expire, stale agents are reclaimed, steps re-run — anything written into a step's `run` must be idempotent. `step_attempts` and `log_lines` are append-only. Durable fibers are also at-least-once (a step that finishes but crashes before checkpoint re-runs).

### Web

`apps/web` is TanStack Start + React Router (file-based routes in `src/routes`, generated `routeTree.gen.ts`) + React Flow canvas (`components/dag-canvas.tsx`, `step-node.tsx`) + shadcn/Tailwind v4. `src/lib/api.ts` is the single typed API client and mirrors the Rust types by hand — update both sides together. Formatting/linting is **Biome** (`pnpm check`), not ESLint/Prettier.

## Conventions & gotchas

- Adding schema: new numbered file in `crates/fiber-core/migrations/` (never edit an applied one); it runs on `fiber-api` boot.
- `collapsible_if` is workspace-allowed in clippy — edition 2024 let-chains make it noisy on protocol code.
- Never `pkill -f fiber-agent`: it matches parent shells whose argv mentions the binary path. Kill by PID of `./target/debug/fiber-agent` (see the dogfood scripts).
- Postgres in Compose is 17-alpine; upgrading from a 16 volume requires `down -v`.
- `data/` holds local dev artifacts and agent git workspaces — generated, not source.
- Secrets are encrypted at rest only when `FIBER_SECRETS_KEY` (64 hex chars) is set: `openssl rand -hex 32`.

## Toolkit

`.claude/` carries repo-specific agents, skills, and hooks — see [.claude/README.md](.claude/README.md). Load the `fiber-conventions` skill before writing code and `fiber-ship` before committing; delegate reviews to `fiber-reviewer` and `fiber-security-reviewer`. Hooks enforce the hazards above deterministically (the argv-pattern agent kill, edits to applied migrations, generated files) and run `rustfmt`/Biome on edits. After changing anything under `.claude/`, run `python3 .claude/validate.py` and `bash .claude/hooks/selftest.sh`.

## Docs

`docs/README.md` indexes everything. Most useful when changing behavior: [architecture](docs/architecture.md), [pipeline-yaml](docs/pipeline-yaml.md) (`fiber.yml` schema, matrix, `if`), [triggers](docs/triggers.md), [authz](docs/authz.md) (`reader` < `writer` < `admin` < `owner`), [artifacts](docs/artifacts.md), [configuration](docs/configuration.md) (all `FIBER_*` vars), [operations](docs/operations.md), [development](docs/development.md), [roadmap](docs/roadmap.md). Keep docs in step with behavior changes — they are the product surface here, and `examples/*.yml` are the worked pipeline samples.
