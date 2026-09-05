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

## API / CLI

```bash
cargo run -p fiber-cli -- login
cargo run -p fiber-cli -- fibers list $PROJECT_ID
cargo run -p fiber-cli -- fibers create $PROJECT_ID --name sleep_demo --input '{"seconds":3}'
cargo run -p fiber-cli -- fibers get $FIBER_ID
cargo run -p fiber-cli -- fibers cancel $FIBER_ID
```

HTTP: `GET/POST /api/projects/{id}/fibers`, `GET /api/fibers/{id}`, `POST /api/fibers/{id}/cancel` (writer to mutate).

Optional `wake_at` (RFC3339) on create starts the fiber suspended until due.

## Semantics

- State checkpointed in Postgres (`fibers` table)  
- Stale `running` heartbeats are reclaimed by the fiber poller  
- Not a replacement for the CI DAG — use pipelines for builds/tests; use fibers for durable orchestration helpers  

CI step durability (leases, reclaim) is separate — see [Architecture](./architecture.md).
