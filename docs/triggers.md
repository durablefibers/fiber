# Triggers

Configured under `on:` in pipeline YAML / the Pipeline inspector.

## Push

```yaml
on:
  push:
    branches: [main, develop]   # empty = any branch
    paths: ["src/**", "Cargo.toml"]
    paths_ignore: ["**/*.md"]
```

Webhook: `POST /api/projects/{id}/webhooks/github` with header `X-GitHub-Event: push`.

Changed files come from the payload (`commits` / `head_commit` added·modified·removed). Empty `changed` + non-empty filters → do not fire.


## Pull request

```yaml
on:
  pull_request:
    branches: [main]            # base branch
    types: [opened, synchronize, reopened]  # default if omitted
    paths: ["src/**"]
    paths_ignore: ["**/*.md"]
```

PR webhooks do **not** include file lists. When path filters are set, Fiber lists files via the GitHub API:

1. Project secret `GITHUB_TOKEN` or `FIBER_GITHUB_TOKEN`, else env `FIBER_GITHUB_TOKEN` / `GITHUB_TOKEN`
2. `GET /repos/{owner}/{repo}/pulls/{n}/files` (paginated)
3. Optional `FIBER_GITHUB_API_URL` for GitHub Enterprise (default `https://api.github.com`)

Without a token (and without embedded `changed_files` in a test payload), path-filtered PR pipelines **skip**.

## Schedules

```yaml
on:
  interval_minutes: 60
  # OR
  cron: "0 0 2 * * *"   # 6 fields WITH seconds: SEC MIN HOUR DAY MONTH DOW
```

- Cron wins if both are set.
- Due times stored on `pipelines.next_due_at`; the scheduler polls Postgres every 30 s, so a schedule saved through the API or UI fires on the next tick without a restart. With several API replicas the slot is claimed with a compare-and-set, so each occurrence starts exactly one run; while a previous run of the pipeline is still active the occurrence is skipped and retried next tick.
- Trigger label looks like `schedule:60m` or cron-derived.

## Webhook security

`PUT /api/projects/{id}/webhooks/github` with `{ "secret": "…" }` stores the HMAC secret (admin+). The secret is encrypted at rest with `FIBER_SECRETS_KEY` like project secrets, and there is exactly one per project and provider.

Webhooks **fail closed**: until a secret is configured, every delivery to `POST /api/projects/{id}/webhooks/github` is rejected with `401`. Once set, requests must carry a valid `X-Hub-Signature-256` (HMAC-SHA256 of the raw body, `sha256=<hex>`), verified with a constant-time compare. Configure the same secret on the GitHub webhook, with content type **`application/json`** (form-encoded deliveries verify but are then rejected as invalid JSON). An empty secret is refused (`400`), since an empty HMAC key would be publicly computable.

## Manual runs

UI **Run** or `POST /api/pipelines/{id}/runs` with optional `{ "trigger": "manual" }` — always starts regardless of `on:`.
