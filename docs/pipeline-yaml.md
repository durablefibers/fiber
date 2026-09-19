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
| `env` | no | Environment for every step; see below |
| `workspace` | no | Git clone into the step workspace. `repo` must be an `http(s)://`, `ssh://`, `git://`, or `file://` URL, an scp-like `user@host:path`, or a path — rejected when the pipeline compiles otherwise, and again by the agent, which also pins `GIT_ALLOW_PROTOCOL`. Git's `ext::` transport runs a command, and the repo is added on the agent host before any container exists |
| `on` | no | Push / PR / cron / interval |
| `steps` | yes | Map of step id → step (YAML) or list in JSON API |
| `timeout_minutes` | no | Whole-run wall-clock limit from run start; the run is cancelled with reason `run timed out` |
| `concurrency` | no | Keep one run per group in flight; see below |

In YAML, steps are usually a **map** keyed by id; the compiler fills `id` / `name` from the key when omitted.

### `concurrency`

Runs whose resolved group matches contend, and a new one cancels the older:

```yaml
concurrency:
  group: "{pipeline}-{ref}"   # default
  cancel_in_progress: true
```

`{pipeline}` expands to the pipeline id — not its name, which is editable and not unique —
and `{ref}` to the branch or pull-request ref, empty for a manual run. Anything else is
literal, including an unrecognised `{placeholder}`, which is left as written rather than
emptied: collapsing it would merge groups the author meant to keep apart.

The default therefore cancels a branch's previous build on a new push without touching any
other branch, and makes two manual runs of one pipeline supersede rather than race. A
literal group is the escape hatch for "one deploy at a time, whatever started it":

```yaml
concurrency:
  group: deploy
  cancel_in_progress: true
```

Groups are matched **within a project**, so a literal name cannot collide across projects.

Omitting `concurrency` means no limit, and so does `cancel_in_progress: false` — queueing
instead of cancelling is a different behaviour, and Fiber would rather do nothing than
guess which one was meant. The resolved group is stored on the run (`concurrency_group`),
so the rule that governed a run stays readable after the pipeline is edited. A superseded
run ends as `cancelled` with reason `superseded by a newer run`.

## Step fields

| Field | Default | Notes |
|---|---|---|
| `run` | — | Shell script (required for execution) |
| `needs` | `[]` | Upstream step ids (DAG edges) |
| `labels` | `[]` | Matched against agent labels (all required labels must be present) |
| `image` | — | Optional Docker image; agent runs in container when Docker enabled. A docker image reference only (`name[:tag][@digest]`, optionally registry-qualified): the value becomes a `docker run` argument, so anything docker could read as a flag (`-v/:/host`, `--privileged`) is rejected when the pipeline compiles and again by the agent, which also passes it after `--` |
| `retries` | `0` | Extra attempts after failure (exponential backoff `2^attempt` s, max 60 s, persisted on the row). An attempt lost to an agent disconnect or lease expiry also counts, and gets one extra try: a step is failed once it has lost more than `retries + 1` leases |
| `timeout_minutes` | server default (60) | Per-attempt wall-clock limit incl. workspace prep and restores. The agent kills the step and fails the attempt (retries still apply); the server independently fails it `FIBER_STEP_TIMEOUT_GRACE_MINUTES` later if the agent did not |
| `secrets` | all | Project secrets to inject, by name. Omit for every secret (the default); `secrets: []` for none. Naming them keeps credentials out of steps that have no use for them, which matters most for a step running a third-party `image:` |
| `env` | `{}` | Environment for this step; overrides the pipeline's for the same name |
| `working_directory` | workspace root | Run in this subdirectory. Must stay inside the workspace — absolute paths and `..` are rejected when the pipeline compiles. **`artifacts` paths stay relative to the workspace root**, so moving a step's `working_directory` does not silently change what it publishes |
| `shell` | `sh` | Interpreter for `run`, invoked as `<shell> -c`. A bare program name only: `bash` yes, `/bin/bash` or `bash -e` no. It has to exist in the `image`, or on the host for a shell step |
| `continue_on_error` | `false` | The step's failure is recorded but does not fail the run, and its dependents still run. See below |
| `artifacts` | `[]` | Workspace-relative paths to upload after **success** |
| `matrix` | — | Axis → values; expanded at compile time |
| `if` | `success()` | Gate whether the step is queued |

Injected env (among others): `FIBER_RUN_ID`, `FIBER_STEP_ID`, project secrets as env vars (see `secrets:` to narrow them), and for matrix cells `MATRIX_<AXIS>` / `FIBER_MATRIX_<AXIS>`. Secret **values** are masked as `***` in step logs (a substring match, so it will not catch a value the step re-encodes first). Nothing else from the agent's own environment reaches a step.

```yaml
publish:
  needs: [build]
  run: npm publish
  secrets: [NPM_TOKEN]     # only this one; other project secrets stay out
```

### `env`

Set it on the pipeline for every step, on a step for that step, or both:

```yaml
name: build
env:
  CARGO_TERM_COLOR: always
  RUST_LOG: info
steps:
  test:
    run: cargo test
    env:
      RUST_LOG: debug        # wins over the pipeline's for this step only
```

**Precedence, least specific first:** pipeline `env`, then step `env`, then matrix bindings.
A matrix binding wins because it is what says which cell is running — a step able to shadow
`os` would make its own logs lie about what it built.

Names must be usable as environment variables: letters, digits and underscore, not starting
with a digit. `FIBER_*` is reserved, since the server sets `FIBER_RUN_ID` and friends there.
Both rules are checked when the pipeline compiles, so a bad name is a validation error
rather than a variable that silently never arrives.

Values are not secret. They are stored in the pipeline definition, snapshotted onto every
run, and visible to anyone who can read the project. Use project `secrets:` for anything
that should not be.

### `continue_on_error`

```yaml
lint:
  run: cargo clippy
  continue_on_error: true     # advisory: report it, do not block the build
build:
  needs: [lint]
  run: cargo build            # runs even when lint failed
```

The step is **still recorded as failed** — the run page shows it red and the attempt keeps
its exit code and logs. What changes is what that failure does to everything else: it does
not fail the run, and dependents are queued rather than skipped. So a green run can contain
a red step, which is the point.

Two limits worth knowing:

- **Tolerance does not travel.** If a tolerated step's dependent fails on its own, that
  failure cascades normally. The flag covers one step's outcome, not the chain below it.
- **A cancel is never tolerated.** `continue_on_error` is about the step's own result; an
  operator stopping the run is not that, and still stops everything downstream.

For gating, a tolerated failure counts as success, so a dependent's default `success()`
passes. Anything else would let the step queue and then skip anyway.

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
| `always()` | Run once upstreams are terminal, even if they failed. Steps *after* an `always()` step still see the failure: `success()` is transitive over the whole ancestry, so `build → cleanup (always) → deploy` skips `deploy` when `build` failed |
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
| `examples/scoped-secrets.yml` | Per-step `secrets:` allowlist |
| `examples/concurrency.yml` | One run per branch, cancelling the previous |

## Semantics

- Cycles are rejected at compile / save.
- On upstream failure, dependents are typically **skipped** (fail-fast).
- Steps are **at-least-once**; prefer idempotent `run` scripts.
- Each step gets its **own workspace**, so parallel steps cannot overwrite each other. Files reach a later step as **artifacts**, and a step is given only the artifacts produced by the steps it (transitively) `needs`.
- What a run executes is frozen when it starts (`workspace`, `run`, `image`, `artifacts`, matrix env). Saving the pipeline afterwards affects only future runs.
