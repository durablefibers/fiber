# Durable fibers

**Fibers** here are control-plane durable tasks (`fiber-durable`), not CI agents. They are per-project background work with `step` / `stash` / `sleep` that survive API restarts.

| | CI Agents | Durable fibers |
|---|---|---|
| Purpose | Run pipeline shell/Docker steps | Long-running / resumable control tasks |
| UI | Global Agents | Project → Fibers |
| Runtime | `fiber-agent` | In-process in `fiber-api` via `FiberScheduler` |

## Built-in tasks

Registered at API boot (see `fiber-durable` tasks module):

- `ping` — trivial success  
- `sleep_demo` — demonstrates `sleep` / resume (`input.seconds`)  
- `interval_task` — recurring-style demo  

`GET /api/fibers/tasks` returns what this build actually registered, and the Fibers page
asks for that rather than carrying its own copy of the list. Tasks are compiled in: adding
one means implementing `FiberHandler` and registering it, not editing configuration.

## API / CLI

```bash
cargo run -p fiber-cli -- login
cargo run -p fiber-cli -- fibers list $PROJECT_ID
cargo run -p fiber-cli -- fibers create $PROJECT_ID --name sleep_demo --input '{"seconds":3}'
cargo run -p fiber-cli -- fibers get $FIBER_ID
cargo run -p fiber-cli -- fibers cancel $FIBER_ID
```

HTTP: `GET/POST /api/projects/{id}/fibers`, `GET /api/fibers/{id}`, `POST /api/fibers/{id}/cancel` (writer to mutate), `GET /api/fibers/tasks` (any authenticated user).

Optional `wake_at` (RFC3339) on create starts the fiber suspended until due.

## Semantics

- State checkpointed in Postgres (`fibers` table)  
- Stale `running` heartbeats are reclaimed by the fiber poller  
- **Statuses**: `pending`, `running`, `suspended`, `completed`, `failed`, `cancelled`.
  `cancelled` is separate from `failed` on purpose — one is a person stopping the work, the
  other is the task's own outcome, and a dashboard that conflates them cannot answer whether
  anything is actually broken  
- **Cancel wins.** It is terminal, so the poller does not pick the fiber up again, and a
  save from a handler that was already running is rejected rather than overwriting the
  decision. A cancelled fiber stays cancelled even if its work would have succeeded  
- Not a replacement for the CI DAG — use pipelines for builds/tests; use fibers for durable orchestration helpers  

CI step durability (leases, reclaim) is separate — see [Architecture](./architecture.md).
