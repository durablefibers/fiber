---
name: fiber-docs-sync
description: Auditing and fixing drift between Fiber's docs/ tree and the code it describes — routes, env vars, make targets, roles, CLI commands, pipeline fields, and rendered Mermaid diagrams. Use after a behavior change, or when asked whether the documentation is still accurate.
license: Apache-2.0
compatibility: Requires the durablefibers checkout; Mermaid export needs npx @mermaid-js/mermaid-cli.
metadata:
  author: durablefibers
  version: "1.0"
---

# Docs sync

In this repo `docs/` is the product surface. It is indexed by `docs/README.md`, linked from the root `README.md`, and expected to be complete — a route missing from `docs/api.md` is a bug, not an omission.

## Documented surface

```!
ls docs/
```

## Each doc has a source of truth — diff them mechanically

| Doc | Source of truth | Command |
|---|---|---|
| `docs/api.md` | `fiber-api/src/routes.rs` | `grep -n 'route(' crates/fiber-api/src/routes.rs` |
| `docs/configuration.md` | clap `env =` args, `env::var` calls | `grep -rn 'env = "FIBER_\|env::var("FIBER_\|env::var("OTEL_' crates/` |
| `docs/development.md` | `Makefile` | `make help` |
| `docs/cli.md` | `fiber-cli/src/main.rs` | `grep -n 'Subcommand\|#\[command' crates/fiber-cli/src/main.rs` |
| `docs/pipeline-yaml.md` | `fiber-proto`, `fiber-core::dag`, `step_if` | read the definition types |
| `docs/authz.md` | `roles.rs`, `access.rs` | `grep -rn 'require_project\|require_pipeline\|require_run' crates/fiber-api/src/` |
| `docs/triggers.md` | `schedule.rs`, `path_filter.rs`, webhook handler | read them |
| `docs/artifacts.md` | `artifacts.rs`, `artifact_util.rs` | check caps and sanitization |
| `docs/operations.md` | `retention.rs`, `otel.rs` | check defaults |
| `docs/architecture.md` | crate layout, `fiber-scheduler`, `ws.rs` | component table, run lifecycle |
| `examples/*.yml` | the validator | `cargo run -p fiber-cli -- validate examples/<f>` |

Three cross-file consistency sets drift first and are worth checking every time:

1. `.env.example` ≡ `scripts/dev-env.sh` ≡ `docs/configuration.md` ≡ the vars code actually reads.
2. `Makefile` targets ≡ `make help` output ≡ `docs/development.md`.
3. `README.md` ≡ `CLAUDE.md` ≡ `docs/` for ports, quick start, and the crate table.

## Diagrams

Mermaid sources live under `docs/diagrams` with exports in `docs/diagrams/rendered`, and some docs carry inline ```mermaid blocks. A changed diagram source needs its rendered artifact re-exported in the same change; a stale export is drift like any other. The repo has Marmalade skills for authoring, theming, review, and export — prefer them over ad-hoc edits.

## Rules

- Fix docs to match code. The exception: when code contradicts a documented *intent*, report it instead of silently redefining the product.
- Keep the register — terse, table-heavy, cross-linked, no padding. Do not turn a two-line correction into a new section.
- Keep `docs/README.md` a complete index.
- `docs/roadmap.md` "Shipped" must not claim what is not shipped, and should gain what just landed.

## Output

A drift table (doc, what it claims, what the code does), then the fixes applied. Say which drift you deliberately left and why.
