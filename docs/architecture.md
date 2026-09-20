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
- **Offers are all or nothing.** The lease is taken first and the offer built second, from the snapshot, the project's secrets and the run's artifacts; if any of those cannot be read, nothing is sent: a store error backs the lease out (no attempt spent, a 30-second backoff so the step cannot block the queue behind it) and the pass moves on; a secret that cannot be decrypted fails the step with the reason, because that will not clear on its own. An agent is offered steps until its slots are full, oldest-queued first, and its slot count comes from `step_runs` rather than from what a replica remembers delivering.
- **Concurrency groups are serialised.** A run start (or retry) in a group takes a transaction-scoped advisory lock on `(project, group)`, inserts the run with a timestamp taken *inside* the lock, and finds the runs it supersedes on the same connection; the cancels happen after the commit and are guarded like any other. Two pushes on two replicas therefore cannot each see the other as newer and both keep running.
- **A lease outlives the session that took it.** An agent's WebSocket dropping — an API deploy, a proxy timeout, a blip — marks the agent offline and nothing else: its steps stay `running` under their leases, the agent keeps executing them and buffers their output, and its first heartbeat after reconnecting renews the leases. Every late message is judged by the row (`running` under this agent?), never by the session it came in on. An agent that cannot reconnect within the lease (300 s, less a margin) stops its steps without reporting them, and the reclaim loop requeues the expired leases. Two exits are deliberate and requeue at once: an agent stopping on SIGTERM says `Goodbye` after killing its steps, and a revoked token (rotate, delete, project delete) ends the session with a requeue.
- **Reclaim is bounded.** An expired lease (or a deliberate exit) requeues a step until it has lost more than `retries + 1` leases (one more than a reported failure gets, so a single rolling agent restart never fails a build); after that it fails and the run propagates, so a step that kills its agent cannot be leased forever. The reclaim loop also finalises any run left `running` with every step terminal — the window between a reclaim committing and its propagation running.
- **Append-only** `step_attempts` and `log_lines` for audit.
- **Durable fibers** (separate from CI DAG) persist task state in Postgres and resume after crash — see [Durable fibers](./durable-fibers.md).

## Naming

Use product prefix `fiber` everywhere (`fiber-*` crates, `FIBER_*` env). Never `df` / `durablefibers` as a package prefix. See `.claude/rules/naming.md`.
