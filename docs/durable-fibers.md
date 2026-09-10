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

### `http_request`

Call a URL, durably. Retries survive a restart, and the wait between them is a suspension
rather than a held task, so a fiber backing off for five minutes costs nothing.

```json
{
  "url": "https://hooks.example.com/build",
  "method": "POST",
  "headers": {"Authorization": "Bearer ..."},
  "body": {"run": "finished"},
  "retries": 3,
  "timeout_seconds": 30
}
```

`method` defaults to `POST`, `retries` to 3 (max 10), `timeout_seconds` to 30 (max 300). A
non-string `body` is sent as JSON.

**What is retried.** 5xx, 429, timeouts and connection errors. A 4xx is not: it will not
become a different answer by asking again, so the fiber fails immediately and says after how
many attempts. Backoff doubles from one second, capped at five minutes.

**Delivery is at-least-once.** A fiber that sends a request and crashes before checkpointing
sends it again on resume. Each attempt carries an `Idempotency-Key` header of
`<fiber id>:<attempt>`, so a receiver that honours it can make the duplicate harmless. If
yours cannot, treat the call as at-least-once and design accordingly — this is the same
guarantee CI steps have.

**Where it may not go.** This task makes the **API process** issue the request, and the API
sits on the backend network holding database credentials. Private, loopback, link-local
(including `169.254.169.254`, cloud metadata), unique-local and carrier-grade NAT addresses
are refused, every resolved address is checked rather than just the first, and redirects are
not followed — a redirect is a second destination the check never saw. Set
`FIBER_HTTP_TASK_ALLOW_PRIVATE=1` to permit them, which is a decision for whoever runs the
instance rather than whoever writes a pipeline.

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
- **A task must exist on the instance that claims the fiber.** Any API replica can pick up
  any fiber, so during a rolling upgrade a fiber naming a task only the new version has will
  fail on an old one with `no durable task handler registered`. Finish the rollout before
  creating fibers that use a newly added task  
- **A step heartbeats while it runs**, every 15 seconds, so one lasting longer than the
  60-second staleness threshold is not mistaken for a crashed fiber and executed a second
  time. This matters most for `http_request`, whose timeout can reach 300 seconds  
- **`attempts` counts failures, not claims.** Waking from a durable sleep continues the same
  attempt; only a genuine failure, or a `running` fiber found stale after a crash, spends
  one. Counting resumes meant a fiber that slept three times exhausted its retries while
  working perfectly  
- **Cancel wins.** It is terminal, so the poller does not pick the fiber up again, and a
  save from a handler that was already running is rejected rather than overwriting the
  decision. A cancelled fiber stays cancelled even if its work would have succeeded  
- Not a replacement for the CI DAG — use pipelines for builds/tests; use fibers for durable orchestration helpers  

CI step durability (leases, reclaim) is separate — see [Architecture](./architecture.md).
