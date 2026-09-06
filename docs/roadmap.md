# Roadmap

Shipped: MVP DAG CI, agents (global + project pools), artifacts (local + S3 presign), authz, retention, GitHub push/PR + path filters, cron/interval, matrix/`if`, durable fibers, CLI, docs, Compose dogfood, CI.

Hardening (audit, September 2026): CI now runs the test suites; instance-admin gate on global agents and user management; agent identity bound to the token with per-step ownership checks; webhooks fail closed with encrypted secrets; loopback-bound Compose with a CORS allowlist, login throttle, and masked errors; offers built from the run snapshot only; transactional propagation with `always()` / transitive `success()` fixed; hot-path indexes; multi-replica safety (schedule CAS, fiber claim, cross-replica cancel / revocation); step and run `timeout_minutes`, persisted retry backoff, agent SIGTERM handling and reconnect backoff.

## Next

Production hardening via real use — before GitLab / Vault / cloud agents. Ordered so that each step is safe to run on a public endpoint.

| # | Item | Goal |
|---|---|---|
| 1 | **Agent install packaging** | `Dockerfile.agent`, agent service in Compose (so `make dogfood-compose` runs a real pipeline), systemd unit + install script, images on `ghcr.io` and binaries on tags; `CHANGELOG.md` and a release version |
| 2 | **Agent hygiene** | `env_clear` + allowlist for host shell; `--env-file` instead of `-e K=V`; `--user` / `--network` / memory / cpu / pids limits; secret redaction in logs; per-step `secrets:` allowlist; workspace GC and per-step workspaces; restore lists derived from `needs` |
| 3 | **Run ops polish** | Retry / re-run from the UI, CLI and API (cancel ships); attempt-scoped logs and cursor pagination for runs and logs; lease / agent / next-run visibility; Agents entry in the project nav; WS reconnect with refetch; CLI `pipelines apply`, `run --wait`, `runs` / `logs` / `artifacts` verbs |
| 4 | **Dogfood on this repo** | GitHub webhook (secret set) → build/test pipeline on a labeled agent for `durablefibers/fiber`; capture the commit SHA and report commit statuses; fix what breaks under real push/PR traffic |

## Later

| Area | Ideas |
|---|---|
| **Pipeline YAML** | `env:`, `continue_on_error`, `working_directory`, `shell`, artifact globs, richer `if` (`failure()`, `&&`, `\|\|`), branch globs |
| **Scheduling** | Per-project / per-pipeline concurrency with cancel-in-progress, FIFO by `created_at`, single-step cancel, batched log inserts with a per-step cap |
| **Durable fibers** | A user-definable task type (shell on an agent), task list endpoint, cooperative cancel + `cancelled` status, resumes not counted as attempts, events on `fiber:events`, retention |
| **Observability** | `/metrics`, OTel in the agent, JSON logs, request ids, supervised background loops surfaced in `/ready` |
| **Sessions** | Password change, revoke-all, sliding expiry, tokens off the WS query string |
| **SCM** | GitLab / Bitbucket webhooks; multibranch indexing |
| **Secrets** | Vault / OIDC / external secret stores (beyond encrypted project secrets) |
| **Agents** | Dynamic cloud agents; autoscaling pools |
| **Extensibility** | Plugin / WASM step SDK |
| **Product** | Preview envs, BYOC, managed DBs, GPU (Northflank-class — out of scope for self-hosted CI core) |

Revisit **Later** after the **Next** vertical is in production use.
