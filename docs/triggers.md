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

## Commit statuses

A run started by a webhook records the commit that triggered it (`head_sha`, `head_ref`,
`pr_number`, `repo_full_name`), and reports back to GitHub as a commit status:

- **pending** when the run starts, **success** / **failure** / **error** when it finishes.
- The context is `fiber/<pipeline name>`, so several pipelines on one repository report
  separately and can each be marked required.
- The status links to the run page when `FIBER_PUBLIC_URL` is set.

Posting needs a token with `repo:status` — a project secret `GITHUB_TOKEN` /
`FIBER_GITHUB_TOKEN`, or the same environment variables on `fiber-api`. Without one,
runs still work and the status is simply not posted. A failure to post never fails a run.

## What gets built

The agent checks out `head_sha` exactly, not the branch tip, so a second push while a run
is queued cannot retarget it. Pull requests are fetched as `refs/pull/<n>/head` from the
**base** repository, which is why a pull request from a fork builds without granting any
access to the fork.

## Pull requests from forks

A fork's pull request is code written by someone outside the project, while the pipeline
and its secrets belong to the base repository. Fiber marks such a run **untrusted**
(`runs.untrusted`) and applies two controls:

1. **No project secrets.** No step of the run receives any, whatever its `secrets:` says.
   Provenance that cannot be determined from the payload counts as untrusted, and a retry
   of an untrusted run stays untrusted.
2. **Project-dedicated agents only.** Untrusted steps are never offered to the global
   pool — only to an agent bound to that project. If the project has no such agent, the
   steps stay queued.

**These reduce the blast radius; they do not sandbox the code.** A step still executes as
the agent's user, so on a shared agent it could read that agent's token out of `/proc` and
reach other projects' work. Run untrusted pull requests only on an agent you are willing
to treat as compromised: dedicated to the project, disposable, and ideally running every
step in a container (`image:`), which is what keeps the agent's own environment out of
reach.

A step that needs a secret will fail on a fork's pull request. That is the intended
outcome — do the privileged part on `push` to a branch you control, after review.

## Dogfooding this repo

`fiber.yml` at the repository root runs the same gate CI does. To wire it up:

```bash
fiber projects create "Fiber"
fiber pipelines apply fiber.yml --project-id $PROJECT_ID
fiber secrets set $PROJECT_ID GITHUB_TOKEN --value-stdin   # repo:status, for the check
```

Then add a webhook in the repository settings pointing at
`https://<your fiber>/api/projects/$PROJECT_ID/webhooks/github`, content type
`application/json`, with a secret matching
`PUT /api/projects/{id}/webhooks/github`. The agent needs a Rust toolchain, `pnpm`, and
Docker, and the label `os=linux`.

## Manual runs

UI **Run** or `POST /api/pipelines/{id}/runs` with optional `{ "trigger": "manual" }` — always starts regardless of `on:`.
