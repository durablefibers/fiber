# Architecture

## Components

```
┌─────────────┐     REST / WS      ┌──────────────┐
│  apps/ui    │◄──────────────────►│  fiber-api   │
│  fiber-cli  │                    │  (axum)      │
└─────────────┘                    └──────┬───────┘
                                          │
                    ┌─────────────────────┼─────────────────────┐
                    ▼                     ▼                     ▼
              ┌──────────┐         ┌──────────┐          ┌────────────┐
              │ Postgres │         │  Redis   │          │ Artifacts  │
              │ schema + │         │ events   │          │ local / S3 │
              │ runs     │         │ fiber:…  │          └────────────┘
              └──────────┘         └──────────┘
                                          ▲
                                          │ WS /ws/agent
                                   ┌──────┴───────┐
                                   │ fiber-agent  │
                                   │ shell/docker │
                                   └──────────────┘
```

| Crate / app | Role |
|---|---|
| `fiber-core` | Models, DAG compile, store, migrations, path filters, roles |
| `fiber-api` | HTTP API, agent WS, run-event WS, retention, GitHub, artifacts |
| `fiber-scheduler` | Lease queue, offers, reclaim, schedules, Redis fan-out |
| `fiber-agent` | Outbound worker: claim steps, stream logs, upload artifacts |
| `fiber-durable` | Separate durable task runtime (`step`/`stash`/`sleep`) |
| `fiber-proto` | Shared message / YAML-facing types |
| `fiber-cli` | Validate, login, run, members, secrets, agents, fibers, spawn agent |
| `apps/ui` | TanStack Start UI + React Flow canvas |

## Run lifecycle

1. **Start run** — API snapshots the pipeline definition, creates `runs` + `step_runs` (compiled DAG, including matrix expansion).
2. **Enqueue** — Scheduler marks root-ready steps `queued` and matches agent labels.
3. **Offer / lease** — Connected agent receives `Offer` over WS (workspace, env, artifact restore list).
4. **Execute** — Agent prepares git workspace, restores prior artifacts, runs shell or Docker, streams `LogChunk`.
5. **Complete** — Agent reports status; scheduler unlocks dependents or skips on failure (fail-fast).
6. **Events** — Run/step/log updates publish on Redis `fiber:events` and to `/ws/runs/{id}` subscribers; agent-directed messages (cancel, disconnect) fan out on `fiber:agent_cmds` to whichever instance holds the agent's socket.

## Durability model

- **CI steps are at-least-once.** Leases expire; stale agents are reclaimed; steps may re-run. Make `run` idempotent.
- **Definition snapshot** on the run is immutable for that execution. Every offer an agent receives — workspace, command, image, artifacts, matrix env — is built from that snapshot; the live pipeline row is never consulted on the execution path, so editing a pipeline mid-run changes nothing for runs already started.
- **Propagation is transactional.** Unlocking dependents, cascading skips, and finalising the run happen in one transaction with the run row locked, computed to a fixpoint — concurrent completions of sibling steps cannot interleave, and a failure at the top of a chain skips the whole chain in one pass.
- **Cancel is guarded the same way.** It takes the run row lock, and a run that is already `succeeded` / `failed` / `cancelled` is returned untouched — a cancel that arrives after the last completion (a user, a run timeout, a superseding push, a project delete) never rewrites the outcome. Open steps are locked before they are cancelled, so a step leased in the same instant is cancelled *with* its agent told and its slot released, and every step-status transition on the lease, complete, and reclaim paths writes the step and its `step_attempts` row in one transaction.
- **Reclaim is bounded.** An expired lease or a disconnect requeues a step until it has lost more than `retries + 1` leases (one more than a reported failure gets, so a single rolling agent restart never fails a build); after that it fails and the run propagates, so a step that kills its agent cannot be leased forever. The reclaim loop also finalises any run left `running` with every step terminal — the window between a reclaim committing and its propagation running.
- **Append-only** `step_attempts` and `log_lines` for audit.
- **Durable fibers** (separate from CI DAG) persist task state in Postgres and resume after crash — see [Durable fibers](./durable-fibers.md).

## Naming

Use product prefix `fiber` everywhere (`fiber-*` crates, `FIBER_*` env). Never `df` / `durablefibers` as a package prefix. See `.claude/rules/naming.md`.
