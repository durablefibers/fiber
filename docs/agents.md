# Agents

Agents are outbound WebSocket workers that execute CI steps.

An agent's identity is the token it connected with. The server binds `agent_id` from that token and ignores the `agent_id` carried in messages (a mismatch is logged). Log lines are accepted only for a step **last leased to that agent** (so the tail of a cancelled or reclaimed step is still recorded); artifacts and completions additionally require the lease to be **live** — anything after a reclaim or cancel is dropped, which is the at-least-once contract: the re-leased attempt reports its own result. Artifact restore downloads are limited to runs in which the agent holds a running step. A socket that never sends `Hello` is offered nothing, and pool scope always comes from the agent's database row, never from the connection.

## Pools

| Scope | `project_id` | Who they serve |
|---|---|---|
| **Global** | `null` | Any project's queued steps (label match still applies) |
| **Project** | UUID | Only that project's steps |

Create a global agent from **Agents** (`/agents`) — **instance admins only**, because a global agent's token leases steps (and receives secrets) from every project. Create a project agent from `/p/{project}/agents`; that requires **admin** on the project. See [authz](./authz.md#instance-admin).

## Register

```http
POST /api/agents
{ "name": "local", "labels": ["os=linux", "docker=true"], "concurrency": 1 }
```

Project-scoped:

```http
POST /api/agents
{ "name": "team-a", "labels": ["os=linux"], "concurrency": 1, "project_id": "<uuid>" }
```

List (optional filter includes globals + that project's agents):

```http
GET /api/agents
GET /api/agents?project_id=<uuid>
```

Response includes a **plaintext token once**. Store it as `FIBER_AGENT_TOKEN`.

CLI:

```bash
cargo run -p fiber-cli -- agents create --name local --labels os=linux,docker=true
cargo run -p fiber-cli -- agents rotate $AGENT_ID
cargo run -p fiber-cli -- agents list --project-id $PROJECT_ID
```

See [cli.md](./cli.md).

## Run

```bash
export FIBER_AGENT_TOKEN=…
export FIBER_API_URL=ws://127.0.0.1:18080
export FIBER_AGENT_NAME=local
export FIBER_AGENT_LABELS=os=linux,docker=true
export FIBER_AGENT_CONCURRENCY=1
export FIBER_AGENT_USE_DOCKER=true   # false = host shell
export FIBER_AGENT_WORKSPACE_DIR=./data/workspaces
cargo run -p fiber-agent
# or: cargo run -p fiber-cli -- agent --token "$FIBER_AGENT_TOKEN"
```

Connects to `/ws/agent?token=…`, sends `Hello`, then heartbeats every **10s**. Pool scope comes from the token's agent row (not from the client).

## Label matching

A step is offered only if:

1. The agent is **global** or bound to the step's **project**, and  
2. **Every** step label appears on the agent (empty step labels match any agent).

## Lifecycle

| Event | Behavior |
|---|---|
| Heartbeat | Touches `last_seen_at`, renews leases, may receive new offers. Retried steps are not offered before their backoff (`not_before`) |
| Step timeout | Every offer carries `timeout_minutes`; the agent kills the process group at the deadline and reports `failed` (`timed out after N min`). The server fails it itself after a grace period if the agent does not |
| SIGTERM / SIGINT | In-flight step processes are stopped and the socket is closed **without** reporting a result, so the server requeues those steps to another agent (a rolling agent restart does not fail a build). The agent exits once its steps are stopped (≤ 10 s) |
| Reconnect | Exponential backoff 1 s → 30 s with jitter; a `401` (revoked token) exits the process with status 2 instead of retrying forever |
| Concurrency | `--concurrency` is enforced locally with a process-wide semaphore as well as by the server; a step parked on it still counts against its `timeout_minutes`, which start when the offer is received |
| Disconnect / WS close | Agent marked offline; in-flight steps requeued (the bounced attempt counts against `retries`) |
| Stale | No heartbeat for `FIBER_AGENT_STALE_SECS` (default **45**) → offline + requeue |
| Token rotate | `POST /api/agents/{id}/rotate-token` — new token once; force-disconnect; old session cannot keep leasing |
| Update | `PUT /api/agents/{id}` — name / labels / concurrency (inflight preserved; pool unchanged) |
| Delete | `DELETE /api/agents/{id}` — disconnect cleanup then delete |

## Presence in UI

- **online** — DB online and last seen within ~45s  
- **stale** — flagged online but heartbeat is old (until reclaim marks offline)  
- **offline** — disconnected or never seen  

## Executor

- **Shell** (`FIBER_AGENT_USE_DOCKER=false`): runs `run` in the workspace directory.
- **Docker**: if `image` is set (or docker mode on), runs inside the image with the workspace mounted, as a named container (`fiber-step-<uuid>`) so cancel and timeout `docker kill` it rather than only the client.

Process groups: cancel kills the step’s process group so grandchildren die too.
