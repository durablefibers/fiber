# Roadmap

Shipped: MVP durable CI, agents (global + project pools), artifacts (local + S3 presign), authz, retention, GitHub push/PR + path filters, cron/interval, matrix/`if`, durable fibers, CLI, docs, Compose dogfood, CI.

Hardening (audit, September 2026): CI now runs the test suites; instance-admin gate on global agents and user management; agent identity bound to the token with per-step ownership checks; webhooks fail closed with encrypted secrets; loopback-bound Compose with a CORS allowlist, login throttle, and masked errors; offers built from the run snapshot only; transactional propagation with `always()` / transitive `success()` fixed; hot-path indexes; multi-replica safety (schedule CAS, fiber claim, cross-replica cancel / revocation); step and run `timeout_minutes`, persisted retry backoff, agent SIGTERM handling and reconnect backoff.

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

| # | Item | Goal |
|---|---|---|
| 1 | **Dogfood on this repo** | Point a public Fiber at `durablefibers/fiber` with a webhook secret set, build `fiber.yml` on a labeled agent, require the commit status on pull requests, and fix what breaks under real push/PR traffic. Needs a public URL for the instance. |

## Later

| Area | Ideas |
|---|---|
| **Pipeline YAML** | Artifact globs, richer `if` (`failure()`, `&&`, `\|\|`), branch globs |
| **Scheduling** | Per-project / per-pipeline concurrency with cancel-in-progress, FIFO by `created_at`, single-step cancel, batched log inserts with a per-step cap |
| **Durable fibers** | A user-definable task type (shell on an agent), task list endpoint, cooperative cancel + `cancelled` status, resumes not counted as attempts, events on `fiber:events`, retention |
| **Observability** | JSON logs, request ids, supervised background loops surfaced in `/ready` |
| **Packaging** | Run `fiber-api` as a non-root user (needs a chown path for existing artifact volumes); a runtime-configurable web image so it can be published; build attestations for release assets |
| **Sessions** | Password change, revoke-all, sliding expiry, tokens off the WS query string |
| **SCM** | GitLab / Bitbucket webhooks; multibranch indexing |
| **Secrets** | Vault / OIDC / external secret stores (beyond encrypted project secrets) |
| **Agents** | Dynamic cloud agents; autoscaling pools |
| **Extensibility** | Plugin / WASM step SDK |
| **Product** | Preview envs, BYOC, managed DBs, GPU (Northflank-class — out of scope for self-hosted CI core) |

Revisit **Later** after the **Next** vertical is in production use.
