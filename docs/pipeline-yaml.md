# Pipeline YAML (`fiber.yml`)

Validate locally:

```bash
cargo run -p fiber-cli -- validate examples/fiber.yml
```

Or paste into the UI Import panel / `POST /api/pipelines/parse-yaml`.

## Top level

```yaml
name: build-and-test
workspace:
  repo: https://github.com/org/repo.git
  ref: main          # branch, tag, or SHA (default main at offer time)
on:                  # optional triggers — see triggers.md
  push:
    branches: [main]
steps:
  checkout:
    run: ls -la
    labels: [os=linux]
```

| Field | Required | Notes |
|---|---|---|
| `name` | yes | Pipeline display name |
| `workspace` | no | Git clone into the step workspace |
| `on` | no | Push / PR / cron / interval |
| `steps` | yes | Map of step id → step (YAML) or list in JSON API |

In YAML, steps are usually a **map** keyed by id; the compiler fills `id` / `name` from the key when omitted.

## Step fields

| Field | Default | Notes |
|---|---|---|
| `run` | — | Shell script (required for execution) |
| `needs` | `[]` | Upstream step ids (DAG edges) |
| `labels` | `[]` | Matched against agent labels (all required labels must be present) |
| `image` | — | Optional Docker image; agent runs in container when Docker enabled |
| `retries` | `0` | Extra attempts after failure |
| `artifacts` | `[]` | Workspace-relative paths to upload after **success** |
| `matrix` | — | Axis → values; expanded at compile time |
| `if` | `success()` | Gate whether the step is queued |

Injected env (among others): `FIBER_RUN_ID`, `FIBER_STEP_ID`, project secrets as env vars, and for matrix cells `MATRIX_<AXIS>` / `FIBER_MATRIX_<AXIS>`.

## Matrix

```yaml
test:
  needs: [checkout]
  run: echo "os=$MATRIX_OS"
  labels: [os=linux]
  matrix:
    os: [linux, macos]
```

Compile produces cells like `test__os_linux`. `needs` that point at a matrixed step are rewritten to depend on all cells (or the appropriate fan-in). See `examples/matrix.yml`.

## `if` conditions

| Expression | Meaning |
|---|---|
| `success()` | Default — run when upstreams succeeded |
| `always()` | Run even if upstreams failed |
| `never()` | Skip |
| `matrix.os == 'linux'` | Compare matrix (or env) axis |

Evaluated when a step becomes ready to queue (dependencies terminal), not at run create — except root steps, which are evaluated immediately.


## Examples in-repo

| File | Focus |
|---|---|
| `examples/fiber.yml` | Linear build + artifact |
| `examples/diamond-ci.yml` | Fan-in |
| `examples/fan-out-tests.yml` | Parallel branches |
| `examples/release-with-artifacts.yml` | Multi-step artifacts |
| `examples/matrix.yml` | Matrix + `if` |
| `examples/cron.yml` | Cron schedule |
| `examples/paths-filtered.yml` | Path filters |

## Semantics

- Cycles are rejected at compile / save.
- On upstream failure, dependents are typically **skipped** (fail-fast).
- Steps are **at-least-once**; prefer idempotent `run` scripts.
