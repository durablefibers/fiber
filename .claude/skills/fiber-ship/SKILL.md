---
name: fiber-ship
description: The pre-commit and pre-PR gate for Fiber — run the exact checks CI runs, audit the change against the repo's invariants, and confirm docs and config landed with the code. Use before committing, before opening a PR, or whenever asked whether a change is ready.
license: Apache-2.0
compatibility: Requires the durablefibers checkout, Rust stable, and pnpm for the web gate.
metadata:
  author: durablefibers
  version: "1.0"
---

# Shipping a change

## What changed

```!
git status --short
```

## 1. Run the gates CI runs

```bash
make check                                    # cargo fmt --check + clippy -D warnings, all six crates
cargo test -p fiber-core -p fiber-durable     # pure unit tests, no DB needed
```

If `apps/web` changed:

```bash
cd apps/web && pnpm exec tsc --noEmit && pnpm check && pnpm test
```

`make check` and `pnpm exec tsc --noEmit` are byte-identical to `.github/workflows/ci.yml`. A change that needs a "just this once" exception to either is not ready.

Report the real output. If something fails, fix it — do not describe it as a pre-existing condition without checking `git stash` first.

## 2. Audit the diff against the invariants

Read `git diff` and answer each, out loud:

- **Replay.** Does any new effect on the execution path survive running twice? Steps and fibers are at-least-once.
- **Wire drift.** Did a `fiber-proto` type change? Then `fiber-api`, `fiber-agent`, `fiber-cli`, and `apps/web/src/lib/api.ts` all need checking — the last one mirrors Rust types by hand.
- **Authorization.** Does every new project-scoped handler gate through `access.rs` at a defensible minimum role?
- **Migrations.** Is the schema change a *new* numbered file? Are new columns nullable or defaulted?
- **Snapshot.** Does the execution path read the run's stored definition rather than live pipeline rows?
- **Naming.** `fiber-*` / `FIBER_*` throughout, no `df` / `durablefibers`.

## 3. Confirm the companion files landed

A change is incomplete without these:

| If you changed… | These must change too |
|---|---|
| A route | `docs/api.md`, and `apps/web/src/lib/api.ts` if the UI consumes it |
| An env var | `.env.example`, `scripts/dev-env.sh`, `docs/configuration.md`, `deploy/docker-compose.yml` if containerized |
| A `make` target | `make help` text, `docs/development.md` |
| Pipeline YAML schema | `docs/pipeline-yaml.md`, an `examples/*.yml` that validates |
| A CLI subcommand | `docs/cli.md` |
| Roles or gating | `docs/authz.md` |
| Anything user-visible | `docs/roadmap.md` "Shipped" line if it closes a roadmap item |

## 4. End-to-end when the change warrants it

Unit tests do not cover the API, scheduler, agent, or artifact path. For changes there, run the relevant dogfood smoke and confirm `DOGFOOD_OK` — see the `fiber-dogfood` skill.

## 5. Commit

Only when asked. Branch first if on `main`. Message: what changed and why, in the imperative, matching the existing log style (`Ship Compose dogfood, run attempt UX, and ops backup notes.`).

## Report honestly

State which gates ran and their real results, which checks you skipped and why, and anything you found but did not fix. A summary claiming green when a check was not run is worse than no summary.
