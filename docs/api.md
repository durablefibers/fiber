# HTTP & WebSocket API

Base URL default: `http://127.0.0.1:18080`. JSON bodies. User routes need `Authorization: Bearer <session>` unless noted.

## Health

| Method | Path | Auth | Notes |
|---|---|---|---|
| GET | `/health` | no | Liveness |
| GET | `/ready` | no | Postgres + Redis + supervised background loops; `503` when any is down |
| GET | `/metrics` | Prometheus exposition. Off unless `FIBER_METRICS_TOKEN` is set, then requires it as a bearer token; `404` when off, `401` when wrong |

## Auth

| Method | Path | Notes |
|---|---|---|
| POST | `/api/auth/login` | `{ username, password }` → `{ token, user, expires_at }` (`user.is_admin` = instance admin) |
| POST | `/api/auth/logout` | Invalidate session |
| GET | `/api/auth/me` | Current user |

## Projects & members

| Method | Path | Min role |
|---|---|---|
| GET/POST | `/api/projects` | session; create → owner on new project |
| GET | `/api/projects/{id}` | reader (includes `role`) |
| GET/POST | `/api/projects/{id}/members` | reader / admin |
| PUT/DELETE | `/api/projects/{id}/members/{user_id}` | admin |
| GET/POST | `/api/users` | instance admin |
| PUT | `/api/users/{id}` | instance admin — `{ is_admin }`; cannot demote the last admin |

## Pipelines & runs

| Method | Path | Min role |
|---|---|---|
| GET/POST | `/api/projects/{id}/pipelines` | reader / writer |
| GET/PUT | `/api/pipelines/{id}` | reader / writer |
| POST | `/api/pipelines/parse-yaml` | session |
| POST | `/api/pipelines/{id}/runs` | writer |
| GET | `/api/projects/{id}/runs` | reader — `?limit=` (default 50, max 200) `&before=<run_id>`; returns `{ items, next_cursor }` |
| GET | `/api/runs/{id}` | reader |
| POST | `/api/runs/{id}/cancel` | writer |
| POST | `/api/runs/{id}/retry` | writer — `{ "failed_only": bool }`; new run from the original's snapshot, `201` with `{ run, steps }` |
| GET | `/api/runs/{id}/steps` | reader |
| GET | `/api/runs/{id}/artifacts` | reader |
| GET | `/api/artifacts/{id}/download` | reader |
| GET | `/api/steps/{id}/logs` | reader — `?attempt=N` `&after_id=<id>` `&limit=` (default 1000, max 5000). Without `after_id` returns the **newest** `limit` lines |
| GET | `/api/steps/{id}/attempts` | reader |

## Secrets & webhooks

| Method | Path | Min role |
|---|---|---|
| GET/POST | `/api/projects/{id}/secrets` | admin |
| DELETE | `/api/projects/{id}/secrets/{key}` | admin |
| PUT | `/api/projects/{id}/webhooks/github` | admin — set HMAC secret |
| POST | `/api/projects/{id}/webhooks/github` | GitHub — **requires** a configured secret and a valid `X-Hub-Signature-256`; `401` otherwise |

## Agents (global)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/agents` | **Instance admin** — every agent |
| GET | `/api/agents?project_id=` | reader on project — that project's agents + globals |
| POST | `/api/agents` | Create (token returned once). Global (no `project_id`): **instance admin**; project: admin on that project |
| PUT/DELETE | `/api/agents/{id}` | Update / delete — global: instance admin; project: project admin |
| POST | `/api/agents/{id}/rotate-token` | New token once; force disconnect — same gate as update |

## Durable fibers

| Method | Path | Min role |
|---|---|---|
| GET/POST | `/api/projects/{id}/fibers` | reader / writer |
| GET | `/api/fibers/tasks` | any authenticated user — durable task names this build registered |
| GET | `/api/fibers/{id}` | reader |
| POST | `/api/fibers/{id}/cancel` | writer |

## Agent HTTP (agent token)

| Method | Path | Notes |
|---|---|---|
| PUT | `/api/agent/steps/{step_run_id}/artifacts` | Proxy upload + `X-Fiber-Artifact-Path`; step must be **running and leased to this agent** |
| POST | `/api/agent/steps/{step_run_id}/artifacts/presign` | S3 presign or `{ mode: "proxy" }`; same lease check |
| POST | `/api/agent/steps/{step_run_id}/artifacts/complete` | After presigned PUT; same lease check |
| GET | `/api/agent/artifacts/{id}/download` | Restore download. Redirects to object storage when it is configured; `?via=api` streams the bytes through the API instead, for an agent that cannot reach the storage endpoint. Only artifacts of a run in which this agent currently holds a running step; `404` otherwise |

## WebSockets

| Path | Auth | Notes |
|---|---|---|
| `/ws/agent?token=` | agent token | Hello, Offer, logs, complete. Identity is bound from the token; `agent_id` fields in messages are ignored, and log / artifact / complete messages are accepted only for steps leased to that agent |
| `/ws/runs/{id}?token=` | session token | Live run/step/log events |

Message shapes: `crates/fiber-proto`.
