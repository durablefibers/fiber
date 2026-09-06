# Changelog

Notable changes per release. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project uses [semantic versioning](https://semver.org/spec/v2.0.0.html) and is pre-1.0, so
minor versions may carry breaking changes.

## [Unreleased]

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
