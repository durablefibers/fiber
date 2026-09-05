---
name: fiber-ops-runbook
description: Operating a running Fiber deployment — health checks, retention and GC, OTel, multi-instance behavior, backups, and diagnosing stuck runs, offline agents, and lease reclaim. Use when the stack is misbehaving, when tuning retention or observability, or when planning an upgrade.
license: Apache-2.0
compatibility: Requires access to a running Fiber deployment, Docker, and psql or the API for inspection.
metadata:
  author: durablefibers
  version: "1.0"
---

# Fiber operations

## Health

- `GET /health` — process is up (liveness).
- `GET /ready` — Postgres and Redis reachable. This is what the Compose healthcheck uses. `make ready` pretty-prints it.

An API that answers `/health` but not `/ready` is running with a broken dependency: check `fiber-postgres` and `fiber-redis` first, not the application.

## Retention and GC

A background loop in `fiber-api`:

1. Purge expired sessions.
2. Delete terminal runs (`succeeded`/`failed`/`cancelled`) older than `FIBER_RETENTION_DAYS`, keeping the newest `FIBER_RETENTION_KEEP_RUNS` per pipeline.
3. Delete artifact blobs (local file or S3) **before** the DB rows, since the cascade from `runs` removes steps, logs, and attempts.

Defaults: 30 days, keep 20, batch 100, hourly. `FIBER_RETENTION_DAYS=0` disables age deletion; sessions are still purged. `003_retention.sql` provides the indexes the sweep depends on — a schema change that adds a run-referencing table without `ON DELETE CASCADE` will make retention fail on a foreign key.

## Observability

Set `OTEL_EXPORTER_OTLP_ENDPOINT` or `FIBER_OTEL_ENDPOINT` to an OTLP HTTP collector. Without one, tracing still goes to stdout under `RUST_LOG` (default `info,fiber_api=info,fiber_agent=info`).

## Multi-instance

Redis channel `fiber:events` fans run events out so several API replicas can serve UI subscriptions. **Postgres leases remain the source of truth for execution.** Each agent holds a WebSocket to exactly one replica, so agent presence is per-replica while step state is global.

## Diagnosing

| Symptom | Check |
|---|---|
| Run stuck `queued` | Any agent online with matching labels? A step's labels must all be present on the agent (`FIBER_AGENT_LABELS`). Project-scoped agents only serve their project |
| Step stuck `running` | Lease expiry and reclaim — the scheduler requeues expired leases, so a step that never moves suggests a live agent that is hung, not a dead one |
| Agent shows offline | Heartbeats: stale agents are marked offline by the scheduler. Check network path to `/ws/agent` and the token |
| Step re-ran unexpectedly | Expected: at-least-once. Lease expired or the agent disconnected mid-step. The `step_attempts` history shows the sequence |
| Logs missing for a step | Append-only `log_lines`; check retention did not sweep the run, and that the agent is streaming |
| Artifact missing | Uploaded on success only, path sanitized, size-capped (64MB HTTP / 8MB WS). Check the backend selection (`FIBER_USE_S3`) |
| Webhook did nothing | Signature must verify against the raw body; PR path filters need `GITHUB_TOKEN` to list changed files |
| Cron and interval both set | Cron wins |

## Upgrades

`fiber-api` applies migrations at boot, so rolling an image forward rolls the schema forward while the old image may still be serving. Migrations must be backward-compatible for that window.

Postgres is 17-alpine: a volume created under 16 will not start. The recovery destroys all data, so it is an operator decision, never an automated one.

## Backups

Back up Postgres (all run history, users, encrypted secrets) **and** the artifact store (local `FIBER_ARTIFACTS_DIR` or the S3 bucket) — they are separate stores and a Postgres-only backup restores rows pointing at blobs that no longer exist. `FIBER_SECRETS_KEY` must be backed up separately from the database, or encrypted project secrets are unrecoverable.

Details: `docs/operations.md`, `docs/configuration.md`.
