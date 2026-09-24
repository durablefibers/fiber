# Roadmap

Shipped: MVP durable CI, agents (global + project pools), artifacts (local + S3 presign), authz, retention, GitHub push/PR + path filters, cron/interval, matrix/`if`, durable fibers, CLI, docs, Compose smoke, CI.

Hardening (audit, September 2026): CI now runs the test suites; instance-admin gate on global agents and user management; agent identity bound to the token with per-step ownership checks; webhooks fail closed with encrypted secrets; loopback-bound Compose with a CORS allowlist, login throttle, and masked errors; offers built from the run snapshot only; transactional propagation with `always()` / transitive `success()` fixed; hot-path indexes; multi-replica safety (schedule CAS, fiber claim, cross-replica cancel / revocation); step and run `timeout_minutes`, persisted retry backoff, agent SIGTERM handling and reconnect backoff.

Coverage (September 2026): unit tests in every crate, with the decisions that need Postgres either extracted into pure functions (lease and retry semantics, retention's blob selection, the snapshot readers, the secret cipher) or driven through a trait double (durable step memoization, sleep ordinals, checkpointing); source audits that fail when a route loses its `access.rs` gate or the hand-mirrored TypeScript drifts from `fiber-proto`; and the Compose smoke running in CI.

UI (September 2026): the canvas draws a run from its compiled step runs, so matrix cells appear as the nodes that actually ran; the pipeline editor covers the whole step schema; project settings, a paged runs list, and account and user management are reachable pages. See [ui.md](./ui.md).

Open source under Apache 2.0 since September 2026 — see `CONTRIBUTING.md` and `SECURITY.md`.

## Next

Production hardening via real use — before GitLab / Vault / cloud agents.

`v0.2.0` shipped the first three items of this vertical: agent install packaging (image,
Compose service, systemd unit, install script, and a release workflow publishing images and
binaries on tags), agent hygiene (cleared environments, `--env-file`, container limits, log
redaction, per-step `secrets:`, per-step workspaces with GC, `needs`-derived artifact
restore), and run ops (retry from UI/CLI/API, attempt-scoped logs, cursor pagination, lease
and next-run visibility, WS reconnect, and CLI parity). GitHub commit statuses, exact-commit
checkout, and fork-PR containment landed with it.

`v0.2.2` published the images for `linux/amd64` and `linux/arm64`, made them public, and
pointed Compose at them, so a deployment no longer needs a build toolchain. `v0.2.3` let an
agent that cannot reach object storage transfer artifacts through the API instead, which is
what the containerised agent needs, and added `/metrics`.

`v0.6.0`–`v0.6.4` shipped the September hardening audit: per-pipeline concurrency groups
with cancel-in-progress, FIFO by enqueue time, a step-validation boundary on `image:` and
`workspace.repo`, guarded status transitions with an attempt cap, leases that survive an
agent or API restart, batched log ingest with a per-step cap and a `resync` frame for
viewers that fall behind, cooperative fiber cancel with a `cancelled` status, the task-list
endpoint, fiber retention, supervised background loops surfaced in `/ready`, a non-root
`fiber-api` image, Compose that refuses defaulted credentials, `cargo-deny` and SHA-pinned
actions in CI, and build provenance on every release asset and image.

| # | Item | Goal |
|---|---|---|
| 1 | **Dogfood on this repo** | Point a public Fiber at `durablefibers/fiber` with a webhook secret set, build `fiber.yml` on a labeled agent, require the commit status on pull requests, and fix what breaks under real push/PR traffic. Needs a public URL for the instance. |
| 2 | **One step-validation module** | `fiber_proto::validate` is called by compile, offer and agent for `image:` and `workspace.repo`; extend it to every field a step carries, with the agent treating the server as untrusted, so a new YAML field cannot bypass the boundary. |
| 3 | **Status transitions as a state machine** | `StepTransition` / `RunTransition` enums, one guarded `UPDATE … WHERE status IN (…) RETURNING` each, `inflight` derived from the database; split `store.rs` by aggregate along the `*_on(&mut PgConnection)` seam. |
| 4 | **Secrets key rotation and admin recovery** | Key id in the ciphertext prefix, AAD bound to `(project_id, name)`, `FIBER_SECRETS_KEY_PREVIOUS` decrypt-only, `fiber secrets rekey`; `fiber users reset-password` for a locked-out admin. Neither can be done today without raw SQL. |
| 5 | **Control-plane work on `fiber-durable`** | Commit statuses, PR file listing, retention and rekey as registered tasks with claims and retries — webhook idempotency and status-report retry fall out of it, and it dogfoods the runtime the product advertises. |
| 6 | **Throughput tier** | Persistent per-repo git mirror, streaming artifact upload/download, push-based offers over a Redis wake, label matching in SQL, leader-elected sweeps with jitter. Needed before autoscaling pools make sense. |
| 7 | **Rolling-deploy contract** | An N-1 job in CI that boots the previous image against the PR-migrated database, an expand/contract rule in [operations](./operations.md), `PROTOCOL_VERSION` gating for agents. Prerequisite for any multi-replica claim. |
| 8 | **End-to-end coverage** | Smoke scenarios for every feature since 0.3 (retry, cancel, groups, statuses, fibers, untrusted PRs, timeouts, retention) and a Playwright login → run flow inside the compose job. |
| 9 | **Operator kit** | `alerts.yml`, the disaster matrix and backup ordering, JSON logs and request ids, a Postgres major-upgrade runbook, and a `deploy/.env` that can drive every documented knob. |

Three follow-ups the audit deferred and the changelog records: a visible *failed* run when a
stored pipeline no longer compiles (needs a `runs.error` column), artifact caps enforced by
a conditional insert rather than advisory, and pipeline `env` shared in the run snapshot
rather than repeated per step (a wire change).

## Later

| Area | Ideas |
|---|---|
| **Pipeline YAML** | Artifact globs, richer `if` (`failure()`, `&&`, `\|\|` — today they are rejected at save time rather than silently false), branch globs |
| **Scheduling** | Single-step cancel |
| **Durable fibers** | A user-definable task type (shell on an agent), resumes not counted as attempts, events on `fiber:events` |
| **Observability** | JSON logs, request ids (also under **Next** 9) |
| **Packaging** | A runtime-configurable UI image so it can be published (`VITE_FIBER_API_URL` is baked at build time) |
| **Sessions** | Sliding expiry |
| **SCM** | GitLab / Bitbucket webhooks; multibranch indexing |
| **Secrets** | Vault / OIDC / external secret stores (beyond encrypted project secrets) |
| **Agents** | Dynamic cloud agents; autoscaling pools |
| **Extensibility** | Plugin / WASM step SDK |
| **Product** | Preview envs, BYOC, managed DBs, GPU (Northflank-class — out of scope for self-hosted CI core) |

Revisit **Later** after the **Next** vertical is in production use.
