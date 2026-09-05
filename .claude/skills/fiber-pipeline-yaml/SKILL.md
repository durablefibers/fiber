---
name: fiber-pipeline-yaml
description: Authoring, validating, and debugging fiber.yml pipeline definitions — steps, needs, labels, matrix, if conditions, artifacts, triggers, and path filters. Use when writing or fixing a pipeline, when a step never gets scheduled, or when adding a field to the pipeline schema.
license: Apache-2.0
compatibility: Requires the durablefibers repository checkout and Rust stable for the validator.
metadata:
  author: durablefibers
  version: "1.0"
---

# fiber.yml

## Validate first, always

```bash
cargo run -p fiber-cli -- validate examples/fiber.yml
```

Also available as `POST /api/pipelines/parse-yaml` and the UI Import panel. Validation catches DAG cycles, unknown `needs` targets, and matrix expansion errors before a run exists.

## Existing examples

```!
ls examples/
```

Read the closest one before writing new YAML — `diamond-ci.yml` for fan-in, `fan-out-tests.yml` for parallelism, `matrix.yml` for axes, `release-with-artifacts.yml` for artifact flow, `paths-filtered.yml` for path filters, `cron.yml` for schedules.

## Shape

```yaml
name: build-and-test
workspace:
  repo: https://github.com/org/repo.git
  ref: main                 # branch, tag, or SHA
on:
  push:
    branches: [main]
steps:
  checkout:
    run: ls -la
    labels: [os=linux]
  build:
    needs: [checkout]
    run: make build
    labels: [os=linux]
    artifacts: [out/app.tar]
```

Steps are a **map keyed by step id** in YAML; the compiler fills `id`/`name` from the key. Step fields: `run` (shell), `needs` (DAG edges), `labels` (all must be present on the agent), `image` (Docker, when the agent has Docker enabled), `retries` (extra attempts, default 0), `artifacts` (workspace-relative paths uploaded **on success only**), `matrix` (axis → values, expanded at compile time), `if` (default `success()`).

Injected env includes `FIBER_RUN_ID`, `FIBER_STEP_ID`, the project's secrets as env vars, and for matrix cells `MATRIX_<AXIS>` / `FIBER_MATRIX_<AXIS>`.

## Write `run` scripts to be replayable

Steps are **at-least-once**. A step can re-run after a lease expires or an agent dies mid-execution. Scripts that append to a shared store, publish a release, or increment a counter must be idempotent or guarded — this is the single most common pipeline authoring mistake in this system.

## When a step never runs

Diagnose in this order:

1. **Labels.** Every label on the step must be present on some connected agent (`FIBER_AGENT_LABELS`, default `os=linux`). A single typo leaves the step `queued` forever with no error.
2. **No connected agent.** `pgrep -x fiber-agent`, and check the Agents page — the agent must be online, and a project-scoped agent only serves its project.
3. **`if` gate.** Default is `success()`; a step whose upstream failed is skipped, not queued.
4. **`needs` unsatisfied.** An upstream that was skipped does not unlock dependents under fail-fast.
5. **Trigger never fired.** Cron wins over `interval_minutes` when both are set. PR path filters require a `GITHUB_TOKEN` (project secret or env) to list changed files; without it the filter cannot evaluate.

## Changing the schema

Adding a field is three-sided: `fiber-proto` (`StepDefinition` / `PipelineDefinition`), the compiler in `fiber-core/src/dag.rs` (and `step_if.rs` for conditions), and `apps/web/src/lib/api.ts`, which mirrors these types by hand. Then document it in `docs/pipeline-yaml.md` and add an example to `examples/` that `fiber-cli validate` accepts.
