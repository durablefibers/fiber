# Changelog

Notable changes per release. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project uses [semantic versioning](https://semver.org/spec/v2.0.0.html) and is pre-1.0, so
minor versions may carry breaking changes.

## [Unreleased]

### Added

- **`GET /metrics`**, Prometheus exposition of the queue, runs, agents, and durable fibers.
  Off until `FIBER_METRICS_TOKEN` is set, then requires it as a bearer token — the API is
  internet-facing in a normal deployment and these figures describe your build volume, so it
  fails closed. Values are read from the database on scrape rather than counted in the
  process, so a restart resets nothing and two replicas agree.
  `fiber_oldest_queued_step_age_seconds` is the one to alert on: it climbs when no agent
  matches a step's labels, which was previously invisible until someone noticed a run
  sitting still.

## [0.2.3] — 2026-09-07

Artifacts work from a containerised agent, and Compose runs on the published images.

### Fixed

- **A containerised agent can produce and consume artifacts again.** The Compose agent runs
  on its own network, away from Postgres, Redis, and MinIO, so it could not reach the
  presigned URL the API handed it — that URL names the storage endpoint as the host reaches
  it, which inside a container is the container. Every artifact upload from it failed, and
  restores failed on the matching redirect. The agent now falls back to transferring through
  the API, which it can reach by definition. Direct transfer is still tried first, and the
  fallback logs why it engaged. `GET /api/agent/artifacts/{id}/download` accepts `?via=api`
  to stream bytes instead of redirecting.

### Changed

- **Compose runs the published images.** `deploy/docker-compose.yml` pulls
  `ghcr.io/durablefibers/fiber-api` and `fiber-agent` instead of building from the working
  tree, so a deployment needs no Rust toolchain. `FIBER_VERSION` in `deploy/.env` picks the
  tag and defaults to `latest`; `docker compose up --build` still builds locally.

## [0.2.2] — 2026-09-07

### Fixed

- **The published images are multi-architecture.** `ghcr.io/durablefibers/fiber-api` and
  `fiber-agent` carried only `linux/amd64`, so `docker run ghcr.io/durablefibers/fiber-agent`
  failed outright on Apple Silicon, arm64 Linux, and Graviton with `no matching manifest for
  linux/arm64`. Both now ship `linux/amd64` and `linux/arm64`, built on native runners rather
  than under emulation, and joined into one manifest list per tag. The release binaries
  already covered arm64; the images did not.

## [0.2.1] — 2026-09-07

Two correctness fixes found by running Fiber's own pipeline on Fiber. Anyone on `0.2.0`
whose steps use `image:` wants this one.

### Fixed

- **Docker steps no longer lose the image's `PATH`.** Step commands ran under `sh -lc`, and
  a login shell sources `/etc/profile`, which on Debian resets `PATH` to a fixed default.
  Anything the image put there was discarded, so `cargo` in `rust:*` (which lives on
  `/usr/local/cargo/bin`) was simply not found and a plain `cargo fmt` step failed with
  `sh: 1: cargo: not found`. Steps now run under `sh -c`, on the host as well, so the
  environment the agent assembles is the environment the command sees.
- **A failed artifact upload now fails the step.** A declared artifact that existed but
  could not be stored — unreadable, over the 64 MiB cap, an unsafe path, or a failed
  transfer — was logged and then ignored, so the step reported success and a later step
  that `needs` it failed with a missing file instead. The step now fails with the real
  reason and its dependents are skipped. A declared path that does not exist stays a
  warning, so a step may still declare an artifact it only sometimes produces.

## [0.2.0] — 2026-09-06

The first tagged release. Security and correctness hardening from a full platform audit,
agent packaging, run operations, GitHub commit statuses, and the open-source licensing.
Schema migrations `005`–`011` apply automatically on `fiber-api` boot. Steps without a
timeout now inherit `FIBER_STEP_TIMEOUT_DEFAULT_MINUTES` (60) — see the upgrade note in
`docs/operations.md`.

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
- **Pull requests from forks are contained.** Such a run is marked untrusted: it receives
  no project secrets whatever its `secrets:` says, and its steps are offered only to
  agents bound to that project, never to the global pool. Unknown provenance counts as
  untrusted and a retry stays untrusted. This limits the blast radius rather than
  sandboxing the code — the docs say plainly that these runs belong on a disposable,
  project-dedicated agent.
- Commit statuses are posted only with a **project** token (never the instance-wide
  environment one) and only to the repository the pipeline's workspace points at, so a
  project cannot aim the instance's credentials at someone else's repository. Webhook
  `head_sha` / `head_ref` are validated before reaching `git`, which is also invoked with
  `--`; a commit outside the shallow window is fetched in bounded steps and a run that
  cannot check out its commit fails rather than building a different one.

### Added

- **The project is open source under Apache 2.0.** `LICENSE`, `NOTICE`,
  `CONTRIBUTING.md`, `SECURITY.md` (with the trust model spelled out), and a
  Contributor Covenant `CODE_OF_CONDUCT.md`, plus issue and pull-request templates
  and Dependabot for Cargo, npm, Actions, and Docker.
- **Step and run timeouts.** `timeout_minutes` on a step (per attempt) and on the pipeline (whole
  run). Agents enforce their own deadline and kill the process group and container; the server is a
  backstop after `FIBER_STEP_TIMEOUT_GRACE_MINUTES`.
- **Agent packaging.** A published `fiber-agent` image, an optional `fiber-agent` Compose service
  (isolated from Postgres/Redis/MinIO on its own network), a systemd unit, and
  `scripts/install-agent.sh` for attaching a second machine. Tagging `vX.Y.Z` runs the gate and
  publishes images plus agent/CLI binaries.
- **Runs record the commit they are for** (`head_sha`, `head_ref`, `pr_number`,
  `repo_full_name`). The agent checks out that commit exactly, so a second push while a
  run is queued no longer retargets it, and pull requests are fetched as
  `refs/pull/<n>/head` from the base repository — which is what makes a **fork's pull
  request** build at all. A retry re-runs, and reports against, the same commit.
- **GitHub commit statuses.** A webhook-triggered run reports `pending` when it starts and
  `success` / `failure` / `error` when it finishes, under the context
  `fiber/<pipeline name>`, so a pull request can require it as a check. Needs a token with
  `repo:status`; without one nothing changes, and a status that cannot be posted never
  fails a run. `FIBER_PUBLIC_URL` makes the status link to the run page.
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
- **CLI parity with the API.** `pipelines apply` pushes a `fiber.yml` (create or update,
  compiled locally first); `run --wait` / `--follow` block and exit with the run's outcome
  (0 succeeded, 1 failed, 3 timed out) so another CI system or a git hook can gate on it;
  plus `projects`, `runs list/get/cancel/retry`, `logs` (with `--attempt` and `--follow`),
  `artifacts list/download`, `logout`, `completions`, and a global `--json`.
- `fiber.yml` at the repository root: Fiber's own gate, for dogfooding.
- `--version` on `fiber-api` and `fiber-agent`.
- Agent lifecycle hardening: SIGTERM stops steps and lets the server requeue them (a rolling
  restart no longer fails a build), exponential reconnect backoff with jitter, exit on a revoked
  token, and local enforcement of `--concurrency`.
- CI runs `cargo test`, Biome, vitest, the web build, and both container images.

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
- **Each step gets its own workspace**, so steps of one run on the same agent no longer
  overwrite each other's build output. Steps of a run share one git clone, so the second
  step costs a checkout rather than another fetch.
- **Workspaces are cleaned up**: a step's directory goes when it finishes (including on
  cancel, timeout, or failure), the run's tree when its last step on that agent finishes,
  and anything older than `FIBER_AGENT_WORKSPACE_TTL_HOURS` is swept at startup. They
  previously accumulated for the life of the agent.
- **Artifacts restore from dependencies only.** A step receives the artifacts of the
  steps it transitively `needs`, not every artifact in the run, so a parallel sibling
  cannot drop files into its workspace.
- Retention deletes an artifact blob only when no remaining run references its path, so a
  retry cannot lose the artifacts it inherited.
- Nine indexes for the hot paths (lease renewal, expired-lease reclaim, queued offers, artifact
  restore lists, the retention cascade, session purge).
- Agent log lines use a single sequence per attempt; stdout and stderr no longer collide after
  1000 lines.
- **The released binaries no longer link OpenSSL.** The agent's WebSocket client used
  `native-tls`, so the published `fiber-agent` / `fiber` tarballs failed to start on any host
  without `libssl3` — including `debian:bookworm-slim`. Both the WebSocket and the HTTP
  client now use rustls with the host's certificate store, which also means a self-hosted
  instance behind a private CA works: previously the WebSocket trusted the system store
  while artifact upload and restore trusted a bundled root list, so steps ran but their
  artifacts silently failed to transfer. `SSL_CERT_FILE` and `SSL_CERT_DIR` are honoured.
- The agent logs the whole error chain when a session fails, instead of just
  `connect websocket` with the cause dropped.

### Changed

- Container images build from one `deploy/Dockerfile` with `--target fiber-api` / `fiber-agent`,
  sharing a cargo-chef dependency layer. `deploy/Dockerfile.api` and the duplicated
  `deploy/Dockerfile.web` / `deploy/nginx.conf` are gone.

## [0.1.0]

Initial release: DAG pipelines on a canvas, agents (global and project pools), artifacts
(local and S3), project roles, retention, GitHub push/PR triggers with path filters,
cron and interval schedules, matrix and `if`, durable fibers, and the CLI.
