# Changelog

Notable changes per release. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project uses [semantic versioning](https://semver.org/spec/v2.0.0.html) and is pre-1.0, so
minor versions may carry breaking changes.

## [Unreleased]

### Added

- **Re-run a run**: `POST /api/runs/{id}/retry`, and buttons on the run page. It builds
  from the original run's definition snapshot, so it reproduces what that run executed.
  `failed_only` carries over the steps that already succeeded — copying their artifacts
  forward so dependents can still restore them — and re-runs the rest.
- **Attempt-scoped logs.** `log_lines` records which attempt produced each line, and
  `GET /api/steps/{id}/logs?attempt=N` narrows to it. The run page's attempt selector now
  changes the log pane instead of showing every attempt interleaved.
- **Pagination.** `GET /api/projects/{id}/runs` takes `?limit=&before=` and returns
  `{ items, next_cursor }`; older runs were previously unreachable past the newest 50.
  `GET /api/steps/{id}/logs` takes `?after_id=&limit=` and returns the newest lines by
  default rather than the entire log — a step that printed millions of lines could
  previously exhaust the API's memory.

### Fixed

- Retention deletes an artifact blob only when no remaining run references its path, so a
  retry cannot lose the artifacts it inherited.

### Security

- **Steps only see what they need.** A step's environment is cleared before its own is
  applied, so repo-supplied shell can no longer read the agent's `FIBER_AGENT_TOKEN`
  (which would let it lease other projects' steps and read their secrets). Docker steps
  receive their environment through a `0600` env-file instead of `-e KEY=VALUE`, which
  put every project secret in the host's process list. Secret values are masked as `***`
  in log lines. A new `secrets:` list on a step narrows which project secrets it gets at
  all; omitted still means all of them.
- **Container limits.** Step containers run with `--security-opt no-new-privileges` and a
  512 process limit, plus configurable `--user`, `--network`, `--memory`, and `--cpus`
  (`FIBER_AGENT_DOCKER_*`). Memory and CPU limits are off by default so an upgrade cannot
  start OOM-killing existing builds; whatever applies is logged as a `system` line.
- **Environment-variable names are validated** before reaching a step. A `docker
  --env-file` line without `=` means "copy this variable from my own environment", so a
  pipeline could otherwise use a matrix axis name containing a newline to make the docker
  client hand the step the agent's token. The docker client now also starts from a cleared
  environment.

### Fixed

- **Each step gets its own workspace**, so steps of one run on the same agent no longer
  overwrite each other's build output. Steps of a run share one git clone through
  worktrees, so the second step costs a checkout rather than another fetch.
- **Workspaces are cleaned up**: a step's directory goes when it finishes (including on
  cancel, timeout, or failure), the run's tree when its last step on that agent finishes,
  and anything older than `FIBER_AGENT_WORKSPACE_TTL_HOURS` is swept at startup. They
  previously accumulated for the life of the agent.
- **Artifacts restore from dependencies only.** A step receives the artifacts of the
  steps it transitively `needs`, not every artifact in the run, so a parallel sibling
  cannot drop files into its workspace.

## [0.2.0] — 2026-09-06

Security and correctness hardening from a full platform audit, plus agent packaging.
Schema migrations `005`–`008` apply automatically on `fiber-api` boot.

### Security

- **Global agents are instance-admin only.** Any authenticated user could previously create,
  update, delete, or rotate the token of a global agent, whose token leases steps — and receives
  the injected secrets — from every project. New `users.is_admin` flag gates that, plus
  `GET /api/agents` without a project filter and all user management
  (`GET`/`POST /api/users`, new `PUT /api/users/{id}`).
- **Agent identity is bound to its token.** `Heartbeat` no longer rebinds the session from a
  client-supplied `agent_id`, so an agent can no longer impersonate another, renew its leases, or
  receive its offers. Log lines require the step to be owned by the sender; artifacts and
  completions require a live lease. Artifact restore downloads are limited to runs where the agent
  holds a running step. A socket that never sends `Hello` is offered nothing.
- **GitHub webhooks fail closed.** Deliveries are rejected with `401` until a secret is configured
  (previously unsigned deliveries were accepted, letting anyone who knew a project id start runs).
  Webhook secrets are encrypted at rest and unique per project and provider.
- **Deployment defaults.** Compose binds Postgres, Redis, and MinIO to loopback, requires a Redis
  password, sets restart policies and a MinIO healthcheck, and reads settings from `deploy/.env`
  (the committed sample `FIBER_SECRETS_KEY` is gone). The API gained a `FIBER_CORS_ORIGINS`
  allowlist, a per-username login throttle, and masked error responses; S3 credentials are required
  rather than defaulted. The CLI writes `~/.fiber/token` as `0600` and accepts `--password-stdin` /
  `--value-stdin`.

### Fixed

- **Offers are built from the run's definition snapshot only.** Editing a pipeline mid-run no
  longer changes the workspace, command, or artifacts of runs already started.
- **Step propagation is transactional** (run row locked, single read, fixpoint planner), so
  concurrent sibling completions cannot race and a failed chain no longer strands later steps as
  pending. `always()` steps now really run after an upstream failure, and `success()` is transitive
  over the whole ancestry, so a step after an `always()` cleanup does not run against a failed build.
- **Multi-replica safety.** Scheduled runs are claimed with a compare-and-set on `next_due_at`,
  durable fibers with `FOR UPDATE SKIP LOCKED`, and cancel / token revocation reach the replica
  holding the agent's socket over a new `fiber:agent_cmds` Redis channel. Revocation also fails
  closed: heartbeats re-check the token. Schedules created after boot now fire without a restart.
- **Retry backoff is real.** It is persisted on the row (`step_runs.not_before`) instead of a
  sleeping task pushing to a Redis list nobody read, and an attempt lost to a disconnect counts
  against `retries`.
- Nine indexes for the hot paths (lease renewal, expired-lease reclaim, queued offers, artifact
  restore lists, the retention cascade, session purge).
- Agent log lines use a single sequence per attempt; stdout and stderr no longer collide after
  1000 lines.

### Added

- **Step and run timeouts.** `timeout_minutes` on a step (per attempt) and on the pipeline (whole
  run). Agents enforce their own deadline and kill the process group and container; the server is a
  backstop after `FIBER_STEP_TIMEOUT_GRACE_MINUTES`. Steps without a timeout get
  `FIBER_STEP_TIMEOUT_DEFAULT_MINUTES` (60) — see the upgrade note in `docs/operations.md`.
- **Agent packaging.** A published `fiber-agent` image, an optional `fiber-agent` Compose service
  (isolated from Postgres/Redis/MinIO on its own network), a systemd unit, and
  `scripts/install-agent.sh` for attaching a second machine. Tagging `vX.Y.Z` runs the gate and
  publishes images plus agent/CLI binaries.
- `--version` on `fiber-api` and `fiber-agent`.
- Agent lifecycle hardening: SIGTERM stops steps and lets the server requeue them (a rolling
  restart no longer fails a build), exponential reconnect backoff with jitter, exit on a revoked
  token, and local enforcement of `--concurrency`.
- CI runs `cargo test`, Biome, vitest, the web build, and both container images.

### Changed

- Container images build from one `deploy/Dockerfile` with `--target fiber-api` / `fiber-agent`,
  sharing a cargo-chef dependency layer. `deploy/Dockerfile.api` and the duplicated
  `deploy/Dockerfile.web` / `deploy/nginx.conf` are gone.

## [0.1.0]

Initial release: DAG pipelines on a canvas, agents (global and project pools), artifacts
(local and S3), project roles, retention, GitHub push/PR triggers with path filters,
cron and interval schedules, matrix and `if`, durable fibers, and the CLI.
