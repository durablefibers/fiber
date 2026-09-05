# Fiber Claude Code toolkit

Repository-specific agents, skills, and hooks for working on Fiber. Everything here encodes invariants that are real in *this* codebase — at-least-once step replay, append-only migrations, hand-mirrored wire types, the `fiber-*` naming rule — rather than generic engineering advice.

```
.claude/
├── agents/      9 subagents      — isolated-context specialists
├── skills/     11 skills         — procedures, Agent Skills spec compliant
├── hooks/       6 hooks          — deterministic guardrails + formatting
├── settings.json                 — hook wiring and a read-only permission allowlist
└── validate.py                   — spec-checks the toolkit itself
```

Verify after any change here:

```bash
python3 .claude/validate.py      # frontmatter, spec compliance, hook registration
bash .claude/hooks/selftest.sh   # 24 deny/allow cases against the guard hooks
```

## Skills — the procedures

Claude loads a skill when the task matches its description; you can also invoke one by name (`/fiber-ship`). Each is written to the [Agent Skills specification](https://agentskills.io/specification) — frontmatter restricted to `name`, `description`, `license`, `compatibility`, `metadata`, `allowed-tools` — so they stay portable to any skills-compatible agent, not just Claude Code.

| Skill | Use it for |
|---|---|
| `fiber-conventions` | The nine invariants. Load before writing or reviewing anything |
| `fiber-ship` | The pre-commit gate: run what CI runs, audit the diff, confirm companion files |
| `fiber-api-endpoint` | Adding a route end-to-end: store → handler → router → `api.ts` → `docs/api.md` |
| `fiber-migration` | Schema changes; `references/patterns.md` has the worked DDL and backfill sequences |
| `fiber-pipeline-yaml` | Authoring `fiber.yml`, and diagnosing a step that never gets scheduled |
| `fiber-durable-task` | Writing `fiber-durable` tasks; step/stash/sleep and checkpoint semantics |
| `fiber-agent-protocol` | Changing `fiber-proto` without breaking deployed agents |
| `fiber-security-review` | The security checklist, tuned to a CI system that runs untrusted code by design |
| `fiber-dogfood` | Running the smokes and triaging failures to a crate |
| `fiber-docs-sync` | Auditing `docs/` against the code that is its source of truth |
| `fiber-ops-runbook` | Operating a live deployment: health, retention, OTel, stuck runs, backups |

## Agents — the specialists

Delegate work that needs its own context. Claude routes on the `description`, or name one explicitly.

| Agent | Model | Role |
|---|---|---|
| `fiber-rust-engineer` | inherit | Implements across the Rust crates |
| `fiber-web-engineer` | inherit | TanStack Start, React Flow, Biome |
| `fiber-migration-engineer` | inherit | Schema changes: migration + model + store query |
| `fiber-devops` | inherit | Compose, Dockerfiles, CI workflow, ports, deploys |
| `fiber-docs-syncer` | inherit | Fixes docs drift against the code |
| `fiber-dogfood-runner` | inherit | Runs the smokes, triages to a cause |
| `fiber-reviewer` | opus | Reviews a diff against the real invariants (read-only) |
| `fiber-security-reviewer` | opus | Auth, secrets, tokens, webhooks, artifacts (read-only) |
| `fiber-dx-reviewer` | sonnet | Onboarding friction and the five files that drift (read-only) |

The three reviewers have no `Edit`/`Write` — they report, you decide.

**How they compose.** A typical feature: `fiber-rust-engineer` implements (loading `fiber-conventions` and `fiber-api-endpoint`) → `fiber-reviewer` and `fiber-security-reviewer` run in parallel on the diff → `fiber-docs-syncer` closes doc drift → `fiber-ship` gates the commit. Skills carry the procedure; agents carry the context isolation.

## Hooks — the guardrails

Deterministic, so they do not depend on the model remembering. Wired in `settings.json`.

| Hook | Event | Does |
|---|---|---|
| `guard-bash.sh` | PreToolUse(Bash) | Denies the argv-pattern kill that also kills this session's parent shell; denies volume-destroying Compose teardown; denies writes to *committed* migrations; denies wholesale deletion of `data/workspaces` or `data/artifacts` |
| `guard-edits.sh` | PreToolUse(Edit\|Write) | Denies edits to applied migrations, `routeTree.gen.ts`, `data/`, `dist/`, `target/`, `Cargo.lock` |
| `fmt-rust.sh` | PostToolUse(Edit\|Write) | `rustfmt` on edited `.rs` using the repo's `rustfmt.toml`, keeping `cargo fmt --check` green |
| `fmt-web.sh` | PostToolUse(Edit\|Write) | Biome (not Prettier) on edited `apps/web` files |
| `naming-guard.sh` | PostToolUse(Edit\|Write) | Flags `df_` / `durablefibers` prefixes reintroduced into source |
| `session-context.sh` | SessionStart | Injects live stack state — API/web/Postgres/Redis up or down, running agents, latest migration |

Two design notes. `guard-bash.sh` strips heredoc bodies before matching, so *documenting* a hazardous command is never blocked — only running one is. The migration guards check `git ls-files`, so writing the next numbered migration stays unobstructed while editing an applied one is denied.

`settings.json` also allowlists read-only commands (`make check`, `cargo clippy`, `git diff`, `docker ps`, `pgrep`, the health endpoints) to cut permission prompts, and routes Compose teardown to an explicit ask.

## Extending

- **New skill**: `skills/<name>/SKILL.md`, `name` matching the directory, a description that says what it does *and when to use it*. Keep it under 500 lines; push detail into `references/`.
- **New agent**: `agents/<name>.md` with `name` and `description`. Reviewers should omit `Edit`/`Write`.
- **New hook**: add the script to `hooks/`, `chmod +x`, register it in `settings.json`, and add deny *and* allow cases to `selftest.sh` — a guard that blocks legitimate work is worse than no guard.

Run `python3 .claude/validate.py` before committing changes to this directory.
