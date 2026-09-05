---
name: fiber-durable-task
description: Writing durable fiber tasks in fiber-durable — the step/stash/sleep primitives, checkpointing, suspend and resume, registration, and the at-least-once semantics that make handlers need to be idempotent. Use when adding control-plane background work, or when a fiber is stuck, retrying, or not resuming after a restart.
license: Apache-2.0
compatibility: Requires the durablefibers repository checkout, Rust stable, and Postgres for persistence.
metadata:
  author: durablefibers
  version: "1.0"
---

# Durable fibers

**Fibers are not CI steps.** CI steps run on `fiber-agent` processes over a WebSocket. Fibers are control-plane background work running **in-process inside `fiber-api`** via `FiberScheduler`, persisted in Postgres, resuming after a crash or restart. Use pipelines for builds and tests; use fibers for durable orchestration that must survive an API restart.

## The runtime

| File | Role |
|---|---|
| `engine.rs` | `run_fiber` — executes or resumes one fiber, persists the outcome (`Completed` / `Suspended` / `Failed` / `Retry`) |
| `context.rs` | `FiberContext` and `FiberSuspended` — the suspend signal, carried as an `anyhow` error and downcast by the engine |
| `store.rs` | Postgres persistence of `FiberRecord` / `FiberState` |
| `scheduler.rs` | The poller: wakes due fibers, reclaims stale `running` heartbeats |
| `registry.rs` | Name → handler map |
| `tasks.rs` | Built-ins: `ping`, `sleep_demo`, `interval_task` |

## Semantics you must design around

- **At-least-once.** A step that finishes its work but crashes before its checkpoint is written will run again on resume. Every handler step must be idempotent or guarded by a stash value it checks first.
- **Memoized steps.** Completed steps are replayed from persisted state rather than re-executed, so a handler is re-entered from the top on resume — write handlers as deterministic replays whose side effects live inside `step`.
- **Sleep suspends, it does not block.** Sleeping returns control and sets `wake_at`; the fiber is not holding a thread or a connection. Never `tokio::time::sleep` inside a handler for anything longer than an instant.
- **Retries** back off (`RETRY_BACKOFF_SECS` in `engine.rs`) and stop at `max_attempts`, after which the fiber is `Failed` with its error recorded.
- **Stale heartbeats** are reclaimed by the poller — a fiber whose API process died mid-run is picked up again, which is another path to replay.

## Adding a task

1. Write the handler alongside the built-ins in `tasks.rs`, following their shape.
2. Register it in the registry at API boot — an unregistered name fails the fiber immediately with `no durable task handler registered for '<name>'`, which is the first thing to check when a new task fails instantly.
3. Put every side effect inside a `step` so it is checkpointed, and keep the handler's non-step code pure.
4. Define the input as a serde type and validate it at the top; input arrives as arbitrary JSON from the API.

## Exercising it

```bash
cargo run -p fiber-cli -- login
cargo run -p fiber-cli -- fibers create $PROJECT_ID --name sleep_demo --input '{"seconds":3}'
cargo run -p fiber-cli -- fibers get $FIBER_ID
cargo run -p fiber-cli -- fibers cancel $FIBER_ID
```

HTTP: `GET/POST /api/projects/{id}/fibers`, `GET /api/fibers/{id}`, `POST /api/fibers/{id}/cancel` (writer to mutate). An optional `wake_at` (RFC3339) on create starts the fiber suspended until due.

Unit tests for fiber logic live in `fiber-durable/src/tests.rs` and must stay DB-free — test the registry, the suspend/downcast path, and due-index ordering rather than the store.

## Debugging

- **Fails instantly** → handler not registered, or input deserialization failed.
- **Never wakes** → check `wake_at` and the poller's due index; `DueIndex` keeps the earliest due time and is authoritative.
- **Runs repeatedly** → a step's effect is outside a checkpoint, or the process dies before the checkpoint each time; look for work done before the first `step`.
- **Stuck `running`** → the owning process died; the poller reclaims it after the heartbeat goes stale.

Docs: `docs/durable-fibers.md`.
