# Local development

Day-to-day workflow for Fiber. Product docs: [README](./README.md) index.

## One-time

```bash
# Rust stable + Docker + pnpm
docker compose -f deploy/docker-compose.yml up -d fiber-postgres fiber-redis
# optional artifacts backend:
make infra-minio
```

Copy [`.env.example`](../.env.example) into your shell, or always use:

```bash
source scripts/dev-env.sh          # local FS artifacts
FIBER_USE_S3=1 source scripts/dev-env.sh   # MinIO
```

## Make targets

```bash
make help          # list
make infra         # Postgres + Redis
make build         # api + agent + cli
make api           # fiber-api :18080
make api-s3        # fiber-api with MinIO
make ui            # UI :3100
make agent         # needs FIBER_AGENT_TOKEN
make login         # session → ~/.fiber/token
make validate      # examples/fiber.yml
make check         # THE gate: fmt --check + clippy --workspace --all-targets --locked -D warnings
make deny          # cargo deny check (advisories, licences, crates.io-only sources)
make images        # build the fiber-api / fiber-agent container images
make test          # cargo test --workspace --locked + apps/ui vitest
make smoke           # authz + artifacts + concurrency + pools (pools last: it kills every agent)
make smoke-s3        # MinIO presign (api-s3 running)
make smoke-compose   # full Compose stack incl. a real pipeline on the containerised agent
make ready         # GET /ready
```

Login defaults: **admin** / **fiber**.

## Ports

| Service | Host |
|---|---|
| API | 18080 |
| UI (dev) | 3100 |
| Postgres | 15432 |
| Redis | 16379 |
| MinIO API / console | 19000 / 19001 |

## Naming

Product prefix is **`fiber`** / `FIBER_*` — see `.claude/rules/naming.md`. Never `df` / `durablefibers` in crates, env, or Compose service names.

## Layout

| Path | Role |
|---|---|
| `crates/fiber-*` | Control plane, agent, CLI, durable runtime |
| `apps/ui` | TanStack Start UI |
| `deploy/` | Compose + Dockerfiles |
| `scripts/` | Smoke scripts + `dev-env.sh` |
| `docs/` | User + ops docs |
| `examples/` | Sample `fiber.yml` |

## Quality gate

```bash
make check   # cargo fmt --check + cargo clippy --workspace --all-targets --locked -- -D warnings
make test    # cargo test --workspace --locked, then apps/ui vitest
cd apps/ui && pnpm check && pnpm exec tsc --noEmit && pnpm build   # ui lint/format, types, build
```

**`make check` is the whole definition of the gate.** The `Makefile` spells the clippy
invocation out once; `.github/workflows/ci.yml` and `.github/workflows/release.yml` both
run `make check` and nothing else, so there is no CI-only variant to be surprised by. If
you widen the gate, widen it in the `Makefile`. (Before this, CI added `--all-targets`,
`-p fiber-proto` and `RUSTFLAGS: -Dwarnings` that the `Makefile` did not, and the release
workflow ran a third version — a warning in a `#[cfg(test)]` module passed locally and
failed in CI.)

GitHub Actions (`.github/workflows/ci.yml`) runs, on push/PR to `main`:

| Job | What it runs | Required check |
|---|---|---|
| `check` | `make check`, `make test-rust`, `make build`, and `git diff --exit-code Cargo.lock` | yes |
| `deny` | `cargo deny check` — RustSec advisories, the licence allowlist, crates.io-only sources (`deny.toml`) | no |
| `ui` | Biome (`pnpm check`), `tsc --noEmit`, vitest, `pnpm build`, and `git diff --exit-code src/routeTree.gen.ts` | yes |
| `docker` | `docker build` of `deploy/Dockerfile` (both targets) and `apps/ui/Dockerfile.ui` | yes |
| `smoke` | `scripts/smoke_compose.sh` — the whole Compose stack and one real pipeline | no |
| `smoke-host` | `make smoke` (authz + artifacts + concurrency + pools) against a host-built API and agent, with Postgres and Redis as service containers | no |

The three "required check" jobs are pinned **by name** in the `main` ruleset: renaming
one blocks every pull request until the ruleset is edited to match, which is why each
carries a `⚠` comment in the workflow.

Every `uses:` is pinned to a commit SHA with the tag in a trailing comment, and runners
are `ubuntu-24.04` rather than `ubuntu-latest` (which becomes Ubuntu 26.04 on 19 October
2026). Dependabot's `github-actions` ecosystem moves the SHAs. The Rust toolchain is
pinned by `rust-toolchain.toml` (kept in step with `deploy/Dockerfile`).

`pnpm` is pinned in one place: `packageManager` in `apps/ui/package.json`. CI's
`pnpm/action-setup` reads it, and `corepack` in `apps/ui/Dockerfile.ui` reads it, so the
lockfile is always validated by the resolver that wrote it.

### Supply chain

```bash
cargo install cargo-deny --locked
make deny        # == the `deny` CI job
```

`deny.toml` at the repo root is the config: RustSec advisories and yanked crates are a
hard failure, licences are an allowlist derived from the tree (permissive only — Fiber
ships Apache-2.0 binaries and images), and `sources` allows crates.io and nothing else,
so a git dependency cannot reach a released binary.

When `deny` goes red on an advisory, the first move is `cargo update -p <crate>` and a
lockfile-only commit. Only if there is genuinely no upgrade path does an entry go in
`[advisories] ignore`, and it needs the advisory link and a sentence saying why. It is
not part of `make check`, which has to work without the network.

UI unit tests live next to their modules as `src/**/*.test.ts(x)` and run under `vitest.config.ts` (jsdom; `src/test-setup.ts` installs an in-memory `localStorage`).

## Agent tips

Prefer `make` / `scripts/dev-env.sh` over ad-hoc env in shell one-liners. Do **not** `pkill -f fiber-agent` — that can match parent shells whose argv mentions the binary; kill by PID of `./target/debug/fiber-agent` only (see smoke scripts).
