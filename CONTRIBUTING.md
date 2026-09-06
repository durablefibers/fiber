# Contributing

Thanks for looking. Fiber is a self-hosted durable CI system: a Rust control plane, a
TypeScript UI, and an agent that executes pipeline steps.

## Getting set up

```bash
make infra     # Postgres + Redis in Docker
make api       # fiber-api on :18080  (admin / fiber)
make web       # the UI on :3100
```

[docs/getting-started.md](docs/getting-started.md) covers the rest, including running an
agent so pipelines can actually execute. `make help` lists every target.

## The gate

Run what CI runs, before you push:

```bash
make check     # cargo fmt --check + clippy -D warnings
make test      # cargo test --workspace + the web vitest suite
cd apps/web && pnpm check && pnpm exec tsc --noEmit && pnpm build
```

Changes to the API, scheduler, agent, or artifact path are not covered by unit tests. Run
the end-to-end smokes against a live stack instead:

```bash
make dogfood            # authz, agent pools, artifacts
make dogfood-compose    # the whole stack, including a pipeline on a containerised agent
```

They print `DOGFOOD_OK`. A change that touches execution and has not been through one of
them is not finished.

## Things that will get a patch sent back

Fiber has a few invariants that are easy to break by accident, and expensive to debug
afterwards. [docs/architecture.md](docs/architecture.md) explains the reasoning.

- **Steps are at-least-once.** Leases expire, agents are reclaimed, steps re-run. Any
  effect you add to the execution path has to survive running twice.
- **A run's definition snapshot is immutable.** The execution path reads the snapshot
  stored on the run, never the live pipeline row — editing a pipeline must not change a
  run already in flight.
- **`step_attempts` and `log_lines` are append-only.** Only retention deletes from them.
- **All SQL is runtime `sqlx`** (`query_as` with bind parameters), never the `query!`
  macros, which would need a live database at build time.
- **Every project-scoped handler gates through `access.rs`** at a defensible minimum role.
- **Migrations are append-only**: add the next numbered file in
  `crates/fiber-core/migrations/`, never edit one that has shipped.
- **Wire types are three-sided.** `fiber-proto` is consumed by the API, the agent, the CLI
  *and* `apps/web/src/lib/api.ts`, which mirrors it by hand. Change one, check all four.

## Pull requests

- One concern per pull request, with a description of what changed and why.
- Say how you verified it. "Tests pass" is fine for a pure refactor; anything touching
  execution should say which smoke you ran.
- Update the docs in the same change. They are the product surface here, not an appendix.
- Add a `CHANGELOG.md` entry under `## [Unreleased]` for anything user-visible.

## Security

Please do not open a public issue for a vulnerability. [SECURITY.md](SECURITY.md) explains
how to report one, and describes the trust model — some things that look like bugs are
documented behaviour of a system that runs repository-supplied code.

## License

By contributing you agree that your contributions are licensed under the Apache License
2.0, the same as the project.
