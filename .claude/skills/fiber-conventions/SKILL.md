---
name: fiber-conventions
description: The non-negotiable invariants of the Fiber CI codebase — fiber-*/FIBER_* naming, the make check gate, at-least-once step semantics, append-only tables, run definition snapshots, runtime sqlx (never query! macros), and role gating. Use before writing or reviewing any code in this repository, and whenever deciding whether a change is safe.
license: Apache-2.0
compatibility: Requires the durablefibers repository checkout, Rust stable, Docker, and pnpm.
metadata:
  author: durablefibers
  version: "1.0"
---

# Fiber conventions

Nine rules. Violating any of them produces a defect that compiles, passes review by inspection, and fails in production or on a second machine.

## 1. Naming: `fiber` / `FIBER_*`, never `df` or `durablefibers`

The repo directory is `durablefibers`; nothing inside it is. Crates `fiber-*`, binaries `fiber` / `fiber-agent`, env `FIBER_*`, Compose services `fiber-*`, config file `fiber.yml`. Source: `.claude/rules/naming.md`.

## 2. `make check` is the gate

`cargo fmt --check` + `cargo clippy -D warnings` across all six crates — byte-identical to CI. For `apps/web`, `pnpm exec tsc --noEmit` is the CI gate and `pnpm check` (Biome) is the formatter. There is no ESLint or Prettier here.

`collapsible_if` is allowed workspace-wide because edition 2024 let-chains make it fire constantly on protocol code. Do not "fix" those.

## 3. CI steps are at-least-once

Leases expire, stale agents are reclaimed, steps re-run. Anything you add to the execution path must survive replay. The same holds for durable fibers: a step that completes but crashes before its checkpoint runs again. When you add an effect, ask what a second execution does — and if the answer is "double-writes", fix it before writing the tests.

## 4. The run's definition snapshot is immutable

`start_run` snapshots the pipeline definition onto the run. That snapshot, not the live pipeline row, governs the execution. Reading live pipeline state from the execution path is a bug even when it appears to work.

## 5. `step_attempts` and `log_lines` are append-only

They are the audit trail. The only path that removes rows is retention (`fiber-api/src/retention.rs`, cascading from `runs`).

## 6. Migrations are append-only and apply at boot

`crates/fiber-core/migrations/` is run by `sqlx::migrate!` when `fiber-api` starts, and each file is checksummed. Never edit a committed migration — add the next numbered file. See the `fiber-migration` skill.

## 7. All SQL is runtime `sqlx`, never the `query!` macros

Every query is `sqlx::query_as::<_, T>` or `sqlx::query` with bind parameters. The compile-time macros would require a live `DATABASE_URL` during `cargo build`, which CI does not have. The trade-off is that a column/struct mismatch is a **runtime** error — so verify schema-touching changes against a real database, not just a green build.

## 8. Every project-scoped handler goes through `access.rs`

`require_project` / `require_pipeline` / `require_run` with the right minimum of `reader` < `writer` < `admin` < `owner`. Mutations are `writer` or above. A handler that takes an id, resolves its project, and proceeds without a role check is a security bug, not a style issue.

## 9. Serialized shapes are three-sided

`fiber-proto` types are consumed by `fiber-api`, `fiber-agent`, `fiber-cli`, **and** `apps/web/src/lib/api.ts`, which mirrors them by hand with no codegen. Change one side and the drift is silent until runtime. Grep all four when a wire type moves.

## Environment variables

A new `FIBER_*` variable lands in four places in the same change: `.env.example`, `scripts/dev-env.sh`, `deploy/docker-compose.yml` (if a container needs it), and `docs/configuration.md`.

## Hazards

- Never kill agents by argv pattern — the pattern matches shells whose command line contains the binary path, including this session's parent. Use `pgrep -x fiber-agent` and `kill <pid>`.
- Never destroy the Compose volumes to "reset" — that deletes every run, artifact, user, and secret. `make down` stops containers and keeps data.
- `data/` (workspaces, artifacts), `apps/web/dist/`, `target/`, and `apps/web/src/routeTree.gen.ts` are generated. Do not hand-edit them.

## Ports

API 18080 · web 3100 · Postgres 15432 · Redis 16379 · MinIO 19000/19001. Deliberately non-default; do not normalize them.
