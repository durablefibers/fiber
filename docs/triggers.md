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
- Due times stored on `pipelines.next_due_at`; scheduler self-reschedules after fire.
- Trigger label looks like `schedule:60m` or cron-derived.

## Webhook security

`PUT /api/projects/{id}/webhooks/github` with `{ "secret": "…" }` stores the HMAC secret (admin+). When set, requests must include valid `X-Hub-Signature-256`.

## Manual runs

UI **Run** or `POST /api/pipelines/{id}/runs` with optional `{ "trigger": "manual" }` — always starts regardless of `on:`.
