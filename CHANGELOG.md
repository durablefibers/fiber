# Changelog

Notable changes per release. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
this project uses [semantic versioning](https://semver.org/spec/v2.0.0.html) and is pre-1.0, so
minor versions may carry breaking changes.

## [Unreleased]

### Upgrading

- **This release rejects definitions that earlier versions accepted** (see *Changed*).
  A pipeline that was stored before the upgrade and no longer compiles cannot start:
  `fiber-api` now **audits every stored pipeline at boot** and logs one `ERROR` per
  broken one with its project id, pipeline id and reason, plus a count — read the boot
  log after upgrading. A scheduled pipeline in that state keeps its due time instead of
  silently burning the occurrence, so it starts firing again as soon as the definition is
  fixed, with no re-arming. A webhook delivery starts every other pipeline it matched and
  reports the failures in its response. Nothing needs a migration.
- Creating a *failed run* for a definition that will not compile, so the breakage is
  visible in the UI rather than only in the log, is a follow-up: it needs a `runs.error`
  column, a snapshot the canvas tolerates that is not a `CompiledDag`, care that a
  zero-step run does not finalise as *succeeded*, and a policy split so a manual start
  still gets its `400`.

### Security

- **An unprivileged caller could OOM the API with one request.** The step cap was tested
  *after* the matrix expansion that materialised every cell, each one a clone of its step
  definition. A 2 MB definition — inside axum's default body limit — of 16 900 steps × 64
  cells expanded to 1 081 600 cells, 3.2 GB resident and 9.8 s before being refused, and
  `POST /api/pipelines/parse-yaml` takes only `AuthUser`, so any logged-in account with
  no project membership could kill the process and take the scheduler, every agent socket
  and every in-flight durable fiber with it. The cap is now checked against the declared
  step count before any expansion (every step yields at least one cell) and again as a
  running total during it, so the peak is the cap plus one step's matrix. The matrix
  product is likewise bounded before it is built, rather than after — one axis of a
  million values used to allocate a million maps to then reject them.
- **`showcase` is a reserved project slug, and the demo project is seeded once.** A
  caller-supplied `slug` on `POST /api/projects` skipped `slugify` entirely and landed
  verbatim, so anyone who could create a project could claim `showcase` — and the boot
  seeder adopts whatever project holds that slug and adds the bootstrap admin to it as
  **owner**. Supplied slugs are now `slugify`d like derived ones, and `showcase` is
  refused with a `400`. Separately, the seed ran on every boot, so a demo project you
  deleted came back on the next restart; it now runs only when the users table was empty
  at boot — the same condition the admin bootstrap uses — or when `FIBER_SEED_SHOWCASE=1`
  asks for it explicitly.

### Added

- `FIBER_SEED_SHOWCASE` (`auto` / `1` / `0`) — whether to seed the Showcase demo project
  this boot. `auto` means "only on a fresh instance". An unreadable value fails the boot
  rather than guessing. See [configuration](docs/configuration.md).
- `FIBER_MAX_STEPS` — raise the 500-step cap for a generated fan-out. `0` or a
  non-number fails the boot. The cap is enforced incrementally, so raising it raises the
  ceiling but not the shape of the allocation.
- **A boot-time audit of every stored pipeline**, logging the ones that would no longer
  compile with project, pipeline, name and reason.

### Changed

- **A pipeline that is wrong now says so instead of quietly doing nothing.** Five silent
  misconfigurations became errors where the author can still see them:
  - An `if:` this server cannot evaluate fails the compile, naming the expression and what
    was wrong with it. `!=`, `&&`, `||`, `failure()`, `cancelled()`, a `${{ … }}` wrapper
    and contexts other than `matrix.` / `env.` all used to evaluate to `false` **forever**,
    so the step was skipped on every run with no diagnosis. They are rejected, not
    implemented — richer conditions remain a roadmap item.
  - A `paths` / `paths_ignore` glob that does not compile is a `400`. It used to be
    dropped, leaving an empty filter set, which matches no file — so the trigger never
    fired.
  - A cron that parses but can never occur (`0 0 0 31 2 *`) is a `400`; it used to leave
    `next_due_at` `NULL` forever.
  - An empty step id is rejected. `needs`, the snapshot and the UI all refer to a step
    by id.
  - An `if:` that reads a name the step does not have — `matrix.osx` where the axis is
    `os` — is rejected with the names it does have. What a condition can read is fully
    known when the pipeline compiles.
  - **Unknown YAML and JSON fields are rejected**, naming the field and the nearest valid
    one. `continue-on-error`, `working-directory`, `need:`, `artifact:` and every other
    typo parsed clean and produced a step that ran without the field. This covers the
    **save API** as well as `fiber validate` / `parse-yaml` / the Import panel: the JSON
    definition on `POST`/`PUT /api/projects/{id}/pipelines` went through serde directly,
    so `"continue-on-error": true` returned `201` and ran the step with the flag off.
    Top-level anchor holders (`_common: &labels [...]`, `x-…`) are allowed, since that is
    how a YAML file is written DRY; YAML merge keys (`<<`) are rejected with a message
    saying the merged fields would be dropped. A pipeline **already stored** is still read
    leniently, so existing rows and run snapshots keep executing. All 10 `examples/*.yml`
    and the repo's own `fiber.yml` compile unchanged.
- **A definition is capped at 500 expanded steps** (`DagError::TooManySteps`). Only the
  per-step matrix (64 cells) was capped before, so 80 steps of 64 cells was a legal
  5 120-step pipeline that opened a transaction and did one `INSERT` per step.

### Fixed

- **One uncompilable pipeline no longer blocks every other pipeline on the same push.**
  The webhook loop `?`-ed on the first failure, so with pipelines A, B and C matching a
  branch and B carrying a definition that no longer compiles, A started, B failed the
  delivery and C never ran — then GitHub redelivered and started a *second* run of A for
  the same commit. Deterministic, so it repeated on every push. Both webhook branches now
  collect per-pipeline failures and answer `{ "started": [...], "failed": [...] }`.
- **A scheduled pipeline that does not compile no longer stops firing for good.** The
  occurrence was claimed before the run was started and the failure was only a warning,
  so a nightly pipeline consumed its slot every night and never ran again. It is compiled
  first; the due time stays put and the schedule resumes on its own once fixed.
- **Store errors are classified by type, not by substring.** `access.rs` and seven
  handlers matched on `contains("forbidden")` and `ends_with("not found")`: rewording a store
  message silently turned a `404` into a `500`, and any error from any layer whose text
  happened to contain "forbidden" became a `403`. A typed `fiber_core::StoreError`
  (`Forbidden` / `NotFound` / `Validation` / `Other`) now carries the classification, with
  one `From` impl mapping it to a status. Every HTTP status is unchanged; `Internal` still
  masks its body and logs the full cause chain. `ValidationError` is replaced by
  `StoreError::Validation`. A project slug collision is now a `400` naming the slug
  rather than a `500`: slugifying supplied slugs makes a clash an ordinary thing for a
  caller to do.
- **Starting a run is one insert, and propagation no longer scans the snapshot per step.**
  `start_run` and `retry_run` issued one `INSERT` per step inside the run's transaction
  (500 round-trips for a 500-step DAG) and now use a single `UNNEST` insert;
  `propagate_after_step` indexes the definition snapshot by id once instead of scanning
  the whole JSON array for every pending step on every planning pass, under the run lock.
  Measured on a synthetic 500-step run, one planning pass went from **12.9 ms to 1.1 ms**
  (debug build). A `failed_only` retry likewise copies carried-over artifacts in one
  statement.

## [0.6.3] — 2026-09-20

A big release: the September hardening audit's remaining phases, and the first one whose
own artifacts can be verified.

**Upgrade the API before the agents.** This release adds no migration, but it changes the
agent wire protocol (`PROTOCOL_VERSION` 2) and the reference deployment's defaults — the
Compose file now refuses to start without credentials of your own, and artifacts default
to the local filesystem rather than MinIO. Read
[operations](docs/operations.md#upgrades) before upgrading an existing install; a
root-owned artifact volume needs one `chown`, and the API now refuses to boot rather than
run green while losing every artifact it is handed.

Security: `scripts/install-agent.sh` could be made to install an attacker's systemd unit
as root. Nothing published shipped it. Details in the Security section below.

### Security

- **The installer could be made to install an attacker's systemd unit as root.** Under the
  documented `curl … | sudo bash -s --`, `$0` is the literal string `bash`, so the
  local-checkout test `[[ -f "$0" ]]` resolved against the *current directory*: a local
  user or a CI step that planted `./bash` and `../deploy/fiber-agent.service` got their
  unit — with its own `ExecStart=` and `User=` — installed and started the next time an
  administrator ran the documented command from that directory. The test now uses
  `BASH_SOURCE[0]`, which is unset for a script read from stdin, and additionally requires
  the basename to be `install-agent.sh`. Introduced in this release cycle; no published
  release shipped it.

### Added

- **Build provenance on everything a release publishes.** The release workflow mints
  [GitHub build attestations](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations)
  with `actions/attest-build-provenance` for each `fiber-agent-<target>.tar.gz` (in the
  job that ran `cargo build`), for the new `SHA256SUMS` (in the publishing job), and for
  the container-image index digest that `ghcr.io/durablefibers/fiber-{api,agent}:X.Y.Z`
  resolves to (in the job that assembles the manifest list). A `.sha256` next to a
  tarball proves the two files agree; an attestation proves who built them, which is the
  guarantee a compromised release page cannot forge. Verify with:

  ```
  gh attestation verify <file> --repo durablefibers/fiber \
    --signer-workflow durablefibers/fiber/.github/workflows/release.yml \
    --source-ref refs/tags/vX.Y.Z --deny-self-hosted-runners
  ```

  `id-token: write` and `attestations: write` are granted to those three jobs only, so a
  step added to `verify` or `images` later cannot quietly mint an attestation. Provenance
  is not a defence against repository write — anyone who can push a `v*` tag gets a valid
  attestation for it; it makes substitution and repackaging detectable.
- **One `SHA256SUMS` per release**, covering every published asset, assembled in the
  publishing job where the whole set is visible for the first time. The per-asset
  `.sha256` files stay: installers already on hosts fetch those, and an older installer
  treats a missing checksum as a warning, so removing them would quietly leave those
  hosts verifying nothing.
- **`deploy/fiber-agent.service` is now a release asset**, so its hash is in `SHA256SUMS`
  and the installer verifies it. It was the one file the installer fetched unchecked, and
  it decides which binary runs as which user. Because the unit has no attestation of its
  own, the installer first verifies `SHA256SUMS` *itself* — same four `gh` flags — and
  only then lets that manifest vouch for the unit. Otherwise someone able to edit an
  already-published release's assets could keep the genuine tarball and its genuine
  provenance, rewrite `SHA256SUMS` around a unit of their own, and have every check pass.
  When the manifest is not attested the unit comes from git at the tag instead, which
  needs repository write to tamper with.
- **`install-agent.sh --check-only`** resolves, downloads and verifies a release and its
  unit, prints the tag, both digests and where the unit came from, and exits without
  touching the host. No
  root needed, and it runs on macOS. Its exit code is the result — `0` both guarantees,
  `1` a check ran and came back wrong (including one silenced by `--insecure-skip-*`,
  which changes whether the installer stops, not whether the check failed), `2` a check
  could not be run at all — so it can gate a deployment script. It resolves the unit too,
  so it exercises the whole trust chain. This is also the documented way to verify a
  download by hand.
- **`install-agent.sh --require-attestation`** refuses to install unless provenance
  actually verified, for hosts where "we could not check" is not an acceptable outcome.
  It is one gate immediately before the install, so it also catches the paths that do not
  go through the "could not check" branch — `--insecure-skip-attestation` after a real
  failure, and `--tarball`.


- **A `resync` frame on `/ws/runs/{id}`.** When a viewer falls behind the run's event
  channel, the server now says so and keeps the socket instead of closing it. The run
  page answers by refetching the selected step's log from the last line id it holds
  (`after_id`) **and** re-reading the run, its steps and its artifacts — the same channel
  carries the status transitions, so a gap could otherwise leave a finished run showing
  as running until someone reloaded. It does the same after a reconnect. A client that
  does not know the frame ignores it. See [api](docs/api.md#websockets).

### Changed

- **`scripts/install-agent.sh` verifies instead of trusting.** `--version latest` is now
  resolved to a concrete tag by following the `/releases/latest` redirect, and the
  tarball, the checksums *and* the systemd unit all come from that one tag — previously
  the unit was fetched from `main`, so a host could end up running a released binary
  under an unreleased unit. If the unit cannot be obtained for that tag the install now
  aborts rather than pairing a new binary with the unit already on disk. A missing or
  mismatched checksum is fatal rather than a warning.
- **Provenance verification is pinned to the version being installed.** `gh attestation
  verify` is called with `--source-ref refs/tags/<tag>` and `--deny-self-hosted-runners`
  as well as `--signer-workflow`. Without the ref pin, an attacker who can write to the
  release page can serve a genuine, still-validly-attested tarball and `SHA256SUMS` from
  an *older, vulnerable* tag and pass every check. A consequence: a re-publish must be
  dispatched against the tag — `gh workflow run release.yml --ref vX.Y.Z -f tag=vX.Y.Z`
  — which also pins `release.yml` itself to the tag.
- **"We did not check" is now said before the install, not after it.** When `gh` is
  missing or unauthenticated the script warns immediately and names the cause; under
  `sudo` that is the *normal* case, because `env_reset` drops `GH_TOKEN` and points
  `HOME` at `/root`. The documented command now uses
  `sudo --preserve-env=FIBER_AGENT_TOKEN,GH_TOKEN`, which also keeps the agent token out
  of sudo's argv (`sudo VAR=… bash` did not). `gh` is still not required to install.
- **The installer bootstraps from a tag.** `docs/agents.md` now resolves the latest tag
  and fetches `scripts/install-agent.sh` from it rather than from `main`: the script
  performs every check above, so pinning the binary while taking the verifier from a
  moving branch only moved the problem.
- **Upgrades are reversible.** The installer keeps the binary it replaces as
  `/usr/local/bin/fiber-agent.prev` (and `fiber.prev`) and records
  `FIBER_AGENT_INSTALLED_VERSION` / `FIBER_AGENT_INSTALLED_SHA256` in
  `/etc/fiber/agent.env`, so a re-run prints what it is upgrading from and a bad rollout
  is one `cp` away from undone.
- **`deploy/fiber-agent.service` is hardened further**: `ProtectKernelModules`,
  `ProtectKernelLogs`, `ProtectClock`, `RestrictRealtime`, `RestrictSUIDSGID`,
  `LockPersonality`, `SystemCallArchitectures=native`, an empty `CapabilityBoundingSet=`,
  `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK` and `UMask=0027`. Steps
  are repo-supplied shell, so `MemoryDenyWriteExecute` (breaks every JIT) and
  `RestrictNamespaces` (breaks `unshare` and rootless container tooling) are deliberately
  left off, along with `PrivateDevices`, `ProcSubset=pid`,
  `SystemCallFilter=@system-service` and `ProtectSystem=strict`; the unit file records why
  for each. Two that *are* set can still bite a build and are documented in
  [agents](docs/agents.md): `SystemCallArchitectures=native` stops 32-bit binaries, and
  `RestrictSUIDSGID` stops `dpkg-deb`/`rpmbuild` producing packages that contain a setuid
  file. Relax any of it in a drop-in under
  `/etc/systemd/system/fiber-agent.service.d/`, which also survives the next installer run.
- `UMask` is `0027`, not `0077`. It costs nothing — `/var/lib/fiber` is already
  `0750 fiber:fiber` — and it is a precondition for a step container reading the
  bind-mounted workspace, but on its own it does not fix the Docker case: the agent
  issues `docker run` without `--group-add`, so a container running as some other uid/gid
  still gets `EACCES` on `cd /workspace`. The working configurations are
  `FIBER_AGENT_DOCKER_USER=<uid>:<fiber-gid>` or `UMask=0022` in a drop-in, both
  documented next to `FIBER_AGENT_DOCKER_USER` in [configuration](docs/configuration.md).
- `ProtectHome=true` stays, so `FIBER_AGENT_ENV_PASSTHROUGH=SSH_AUTH_SOCK` cannot reach a
  socket under `/run/user/<uid>` as shipped. [configuration](docs/configuration.md) now
  documents the caveat and a `BindPaths=` drop-in next to the variable.


- **The run-event fan-out is per run.** Every `/ws/runs/{id}` subscriber used to sit on
  one process-wide 1024-slot broadcast, JSON-parse every event of every run to compare
  `run_id`, and treat falling behind as fatal. One chatty build could therefore close the
  socket of a viewer watching a completely different run, and the lines published in the
  gap were never shown. Events are now parsed once, for their run id, and delivered to a
  channel that exists only while that run has viewers; log output no longer travels on
  the instance-wide bus at all.
- **The commit-status reporter recovers from a gap.** A lag used to mean the reporter
  missed the terminal event it was waiting for, and a required check stayed pending
  forever. It now re-scans for runs that reached a terminal status in the window the gap
  covers (rate-limited, and de-duplicated against what it has already posted).
- **The run page's log view survives a fast build.** Lines are batched into one commit
  per animation frame instead of one `setLogs` per event, rows are keyed by log id and
  virtualised, and the log-fetch effect no longer depends on the `steps` array — a
  50-cell matrix used to refetch the selected step's log on every `step_updated`. Adds
  `@tanstack/react-virtual` (and its one dependency, `@tanstack/virtual-core`).

### Fixed

- `wire-drift.test.ts` now also reads `fiber-core/src/models.rs` and
  `fiber-durable/src/types.rs`, and covers `ConcurrencyConfig` (which had already
  drifted), the `RunEvent` tags the run page matches as string literals, and
  `FiberStatus`. Fields the UI deliberately does not mirror are listed with a reason and
  checked for staleness.

## [0.6.2] — 2026-09-19

Four phases of the September hardening audit, and the first dependency advisory the new
supply-chain gate caught.

**Upgrade the API before the agents**, and read
[operations](docs/operations.md#upgrades) first on a large install: this release adds
migrations 015 and 016, changes what a lost lease costs a step, and makes a disconnect
stop requeueing.

Security: rustls moves to 0.23.45 for
[RUSTSEC-2026-0285](https://rustsec.org/advisories/RUSTSEC-2026-0285), which accepted TLS
1.3 handshake messages across encryption-level boundaries. It sits under reqwest, the
S3 client and both WebSocket stacks, so it is on every connection the API, the agent and
the CLI make.

Adds migration 015: three indexes, one dropped index, a unique index on artifacts (after
removing duplicate rows), and `CHECK` constraints on the two status columns, added
`NOT VALID` so an old database with a stray status string still boots. Adds migration
016: one partial index on `step_runs (queued_at, id)` for the new queue order, replacing
`idx_step_runs_queued`. See [operations](docs/operations.md#upgrades) before upgrading a
large install.

### Added

- **A supply-chain gate.** `deny.toml` at the repo root and a `deny` job in CI
  (`EmbarkStudios/cargo-deny-action`) fail the build on a RustSec advisory or a yanked
  crate, hold licences to an allowlist derived from the tree (permissive only — Fiber
  ships Apache-2.0 binaries and images, and a copyleft transitive would change what an
  operator may do with them), and allow crates.io as the only source, so a git dependency
  cannot reach a released binary. `make deny` runs the same thing locally. It found one
  real advisory on the way in: **RUSTSEC-2026-0285**, TLS 1.3 handshake messages accepted
  across encryption-level boundaries in `rustls` 0.23.43, reached through `reqwest`,
  `aws-sdk-s3`, `axum`'s WebSocket stack and `tokio-tungstenite` — i.e. every TLS
  connection the API, the agent and the CLI make. Fixed by `rustls` 0.23.45 in
  `Cargo.lock`; the `ignore` list is empty.
- **A host smoke job in CI.** `smoke-host` brings up Postgres 17 and Redis 7 as service
  containers on the same ports as `deploy/docker-compose.yml`, builds and runs
  `fiber-api` and an agent, and runs `make smoke` — authz, artifacts, concurrency groups
  and agent pools. Until now the only end-to-end coverage that ran automatically was the
  Compose smoke, which starts one pipeline and never touches the role gate or the
  project-scoped pools. It is not a required status check yet; the `main` ruleset pins
  job names and has to be edited separately.
- **Generated files are checked in CI.** `apps/ui/src/routeTree.gen.ts` after the UI
  build and `Cargo.lock` after a `--locked` build must both be unchanged. A stale
  committed route tree means the routes CI tested are not the routes in the tree.

### Fixed

- **An artifact the server cannot store fails its step instead of vanishing.** The
  WebSocket upload path logged a `warn!` and carried on, and the row insert that follows
  it was not checked at all, so a full disk, an unreachable bucket or an artifact
  directory the API cannot write lost every artifact into the API's own log while every
  build stayed green and dependent steps restored nothing. The step now fails through
  the ordinary completion path with `artifact <name> could not be stored: <cause>` on the
  step and its attempt, so `retries` apply and the failure is where the pipeline author
  will look. The HTTP upload path already failed loudly.
- **`fiber-api` refuses to start when its artifact directory is not writable.**
  `create_dir_all` succeeds on a directory that already exists whatever it belongs to, so
  an install upgrading to the non-root image booted green against its old root-owned
  volume and only discovered the problem on the first upload. Boot now writes and removes
  one probe file and, on failure, exits naming the directory and the `chown -R
  10001:10001` that fixes it. Only the local backend is probed; the S3 backend already
  fails closed on an unreachable bucket.

- **The artifacts smoke wiped a workspace a step was using.** It removed
  `data/workspaces/<run_id>` — including `.repo`, the reference clone every step of the
  run fetches from and the working directory of whichever step is preparing — as soon as
  the producing step reported success. A dependent step is routinely already `running` by
  then, and it died with `fatal: unable to get current working directory`. It passed only
  by winning a race against the consumer's `git fetch`, and anything that shortens the
  gap between a step finishing and the next one starting makes it fail on a control plane
  that is working correctly. It now removes the producer's own (terminal) step directory,
  which cannot race anything — except that the agent already removes each step's
  workspace as that step finishes, so there was never anything there to wipe and the
  external wipe only ever raced the agent's own GC. The smoke now asserts the two facts
  that are true by the end of the run instead: no workspace survives it, and a consumer's
  log says it restored its inputs.

### Changed

- **Log ingest is batched end to end.** A line cost two Postgres round trips and an
  awaited Redis publish, serially, inside the agent socket's read loop: a 100 000-line
  build was 200 000 statements, and a single socket topped out at a few hundred lines a
  second. The agent now coalesces output into a new `log_batch` message — flushed at
  50 ms, 64 KB or 500 lines, whichever comes first — and the server stores a batch with
  one `INSERT ... SELECT FROM UNNEST(...)` and publishes one `fiber:events` message for
  it. The `owned_step` lookup and the per-attempt line counter are cached per
  `(socket, step_run_id, attempt)` and dropped when the step completes or is cancelled;
  the counter map used to grow for the life of the socket. Measured on a throwaway
  database with a 50 000-line step: **122–609 lines/s before, 6 000–32 000 after**, and
  **2.0 Postgres transactions per line before, 0.005–0.011 after**. `log_chunk` is still
  accepted and takes the same path, so an agent that has not been upgraded is unaffected;
  `PROTOCOL_VERSION` is now `2`.
- **A byte that is not UTF-8 no longer ends a step's log.** The agent read output with a
  line reader that returned `InvalidData` on the first such byte, and the loop around it
  treated that as end-of-stream: one Latin-1 filename in an `ls` listing dropped the
  pipe, the child got SIGPIPE on its next write, and the step ended at exit 141 with an
  empty log. Output is now read as bytes and decoded lossily, and a line longer than
  64 KiB is cut with a visible `…[truncated]` marker instead of becoming a WebSocket
  frame past tungstenite's 64 MiB limit — which closed the socket and took every other
  step on that agent with it. The server caps each line at 64 KiB as well, before the
  insert and before the publish, since it cannot assume the agent did.
- **A step that outruns the control plane is made to wait, not truncated.** Output now
  crosses a bounded channel between the pipes and the batcher, and the batcher waits for
  room in the outbox while a session is up, so back-pressure reaches the step's own
  `write`. Before, a 50 000-line step reached the database as 10 008 lines with no
  indication the other 40 000 had existed; it now arrives whole. With the socket down
  nothing has changed in kind — the outbox still drops its oldest lines, now bounded by
  bytes (8 MB) as well as lines (10 000), and still says how many went.
  A step that exited waits up to 60 s for its buffered output to reach the server (10 s
  after a cancel or timeout, which must be reported promptly) and says so if it gives up
  with output still queued, rather than holding its concurrency slot for as long as a
  stalled socket takes. A batch is capped at 2 000 lines server-side (the agent flushes at 500), since a
  hostile agent could otherwise put a 64 MiB frame on the wire that parses to a million
  of them; the excess is dropped with a notice. A batch the database refuses is reported
  on the next one that lands, rather than leaving a silent hole.
  `log_lines` still has no unique key: a message re-sent after a session ended mid-flush
  is stored twice, as before — at most one message per interrupted session, but now up to
  500 lines of it rather than one. The lines keep their original `seq`.
- **The run event stream carries `log_batch`.** One event per batch rather than per line,
  which is also one slot of the 1024-slot broadcast rather than five hundred. `log` is
  still forwarded, so a replica running the older code during a rolling deploy keeps
  feeding open run pages, and `RunEvent::Log` now carries the `attempt` it belongs to.
  **A run page opened before the upgrade stops tailing live** until it is reloaded or the
  step's log is refetched: the old bundle only knows `log`. Nothing is lost — the lines
  are stored and appear on reload.
- **A step's own notes travel with its output.** Batching had made storage order stop
  meaning emission order — the agent's `system` notes went out immediately while piped
  output waited behind a flush interval, so a cancel's "killing step process" was stored
  above the output that preceded it. While a step is running those notes now go down the
  same channel as its pipes, so `id` order is emission order again and
  `GET /api/steps/{id}/logs` keeps ordering by `id` alone — which it must, because the
  last line of a page is the cursor `fiber logs --follow` and the UI resume from. Notices
  inserted after the fact carry the `seq` of the gap they report.
- **An agent newer than the server unpacks its batches.** `Welcome` now carries the
  server's `protocol_version`; absent means "older than `log_batch`", and the agent sends
  that server one `log_chunk` per line, each keeping its `seq`. Without it a rev-2 agent
  against a pre-batch API lost every line of every build silently — the server answered
  `Error { "invalid message" }` and the agent had already dropped the message, because
  the outbox pops on a successful send, not on an acknowledgement. The expansion happens
  in the writer, not where the batch is built, so a batch queued against a new replica
  and re-sent to an old one mid-deploy is also covered, and that `Error` is now logged at
  `error!` naming the likely cause.

- **An agent fills every free slot in one heartbeat.** Offers were made one per
  heartbeat, so a concurrency-4 agent took forty seconds to fill and a new step waited up
  to ten with idle capacity beside it. Every offer point (`Hello`, heartbeat, step
  completion) now offers until the agent is full or nothing matches.
- **The queue is oldest-first by `queued_at`, bounded, and label-filtered in SQL.** It was
  ordered by `started_at NULLS FIRST` over the whole table, which `started_at` — never
  cleared — turned into "every never-started step before any requeued one, however old
  its run"; and every heartbeat of every agent read the entire backlog. A requeued step
  now keeps its place, a heartbeat reads at most 200 steps the agent can actually take,
  and `fiber_oldest_queued_step_age_seconds` measures what it says: it now reads the
  oldest `queued_at` among leasable steps, where it read the oldest run's `created_at`
  and charged a dependent step for the whole time its predecessors ran.
- **A secret that cannot be decrypted fails the step, with the reason.** This reverses
  0.6.1, which logged the failure and let the step run without its secrets — a build
  that then failed looked like the build's fault. The store now types the failure
  (`SecretDecryptError`), and an offer that hits it is not sent: the step is failed
  through the ordinary completion path with `cannot decrypt project secret NAME (is
  FIBER_SECRETS_KEY right?)` on the step and its attempt, `retries` apply, dependents
  skip, the run finalises, and the queue drains past it. A wrong or rotated key now shows
  up on the run page instead of in a log line, and nothing runs without its secrets.
- **An agent's slots are counted from the database**, not from what the replica remembers
  delivering. A `Cancel` that never reached the agent (a Redis gap between replicas) or a
  completion another replica handled used to leave a slot taken until the agent
  reconnected. The in-memory count is now only a reservation held while an offer is being
  leased. `concurrency: 0` from an agent is treated as 1 on both sides.
- **The durable poller asks the database at least every 30 seconds.** Its due index is
  per replica, so a fiber created on a replica that then died could wait hours to be
  claimed elsewhere.

- **`/ready` no longer fails on Redis.** Leases, scheduling, and the queue live in
  Postgres, so a replica without Redis still runs builds; reporting `503` for it made a
  Redis blip a full API outage from the load balancer's point of view. The probe now
  answers `200` with `"redis": "degraded"` and a top-level `"degraded": true`, and
  `docs/operations.md` lists what is actually lost (cross-replica live run streams and
  agent-command fan-out). Postgres down or a supervised loop down is still `503`. For the
  same reason an unreachable Redis at boot no longer crash-loops `fiber-api`: the client
  connects lazily, the process logs that it is starting degraded, and it heals when Redis
  is back.

- **Ordinary API requests time out after 30 s** with `408`, so a slow client or a stuck
  query cannot hold a server task open indefinitely. WebSocket upgrades, artifact
  uploads/downloads and project deletion are exempt. The GitHub webhook now accepts
  deliveries up to 25 MiB (GitHub's maximum) — the 2 MiB default turned a large push
  into `413` and no run — with at most eight deliveries buffered at once, since the
  endpoint is unauthenticated until the signature is checked. With Redis down, an event
  publish now fails in about two seconds instead of the client's 13 s default, so a
  cancel or completion cannot spend its request budget on one publish.

- **A disconnect no longer requeues; a lease outlives its session.** Until now any drop
  of the agent's WebSocket — an API deploy, a proxy idle timeout, a NAT hiccup — ended
  every step in flight on both sides: the agent killed its steps and the server requeued
  them at once, so a rolling API deploy restarted every running build and spent a retry
  on each. Now the server only marks the agent offline; the steps stay `running` under
  their leases, and the agent keeps executing them, buffers their output (10 000
  messages; past that the oldest log lines go, never a completion or an artifact, with a
  system line saying how many), reconnects with its usual backoff, renews the leases on
  its first heartbeat back, and flushes the buffer in order. The server accepts the late
  lines and completions as long as the row is still `running` under that agent, and
  counts the steps it still holds before offering it more. An agent that has not
  reconnected within the lease (300 s, less a 30 s margin) stops its steps without
  reporting them, and the reclaim loop requeues the expired leases within the existing
  `retries + 1` budget. See [operations](docs/operations.md#shutdown-and-deploys).

  Two exits still requeue immediately, on purpose: an agent stopping on SIGTERM now sends
  a new `Goodbye` message once its steps are gone, so a rolling *agent* restart hands the
  work to another agent within seconds instead of after the lease; and a token rotation,
  agent delete, or project delete ends the session with a requeue as before. The
  stale-agent sweep (45 s without a heartbeat) now only marks the agent offline and closes
  its socket; its leases expire on their own.

  Wire: `Hello` gains `protocol_version` (`fiber_proto::PROTOCOL_VERSION` = 1; absent
  from older agents and read as 0), `Welcome` gains `lease_secs`, `AgentMessage` gains
  `Goodbye`, and `Offer`, `LogChunk`, `Artifact` and `StepComplete` gain `attempt` — the
  server drops a message whose attempt is not the row's, so output an agent held through
  a reclaim cannot land on, or close, the attempt that replaced it — all backward
  compatible. The same attempt travels on the HTTP artifact routes as `X-Fiber-Attempt`
  (upload) and an `attempt` body field (presign, complete). An agent with no `protocol_version` cancels its
  steps on any close, so the server requeues its steps on disconnect exactly as before;
  a new agent against a server that sends no `lease_secs` stops its steps on close as
  before. Only new-on-new keeps a step running through a reconnect — upgrade the API
  first, then the agents. `protocol_version` is logged on `Hello` and not yet stored.

- **`make check` is the only definition of the gate.** The `Makefile` ran clippy without
  `--all-targets` and without `fiber-proto`, CI added both plus `RUSTFLAGS: -Dwarnings`,
  and the release workflow ran a third variant without `--locked` — so a warning in a
  `#[cfg(test)]` module passed locally and failed in CI, and a `Cargo.toml` edit with a
  stale lockfile passed `check` and failed two jobs later. One definition now
  (`cargo clippy --workspace --all-targets --locked -- -D warnings`), and both workflows
  call `make check`; `--locked` is on the test and build targets too.
- **Every GitHub Actions `uses:` is pinned to a commit SHA**, with the tag in a trailing
  comment that Dependabot maintains. The worst case was `dtolnay/rust-toolchain@master` —
  a *branch* — in the release job that pushes GHCR images and release binaries with
  `packages: write` / `contents: write`. Both workflows now also declare
  `permissions: contents: read` at the top, so a job that needs more has to say so
  (`images` and `manifest` take `packages: write`, `publish` takes `contents: write`, and
  nothing else widens anything), and both have `concurrency:` groups — CI cancels the run
  a force-push superseded, release deliberately does not, because a half-published
  release is worse than a queued one.
- **Runners are `ubuntu-24.04`, not `ubuntu-latest`.** `ubuntu-latest` migrates to Ubuntu
  26.04 on 19 October 2026; a runner image that changes under a release build is the same
  class of problem as a floating action tag. The release `binaries` matrix keeps
  `ubuntu-22.04` on purpose — the agent has to run on hosts older than the runner.
- **One pnpm version.** `"packageManager": "pnpm@10.34.5"` in `apps/ui/package.json` is
  now the single source: CI's `pnpm/action-setup` reads it (the pinned `version: 9` is
  gone) and `corepack` in `apps/ui/Dockerfile.ui` reads it. CI was validating the
  lockfile with pnpm 9 while the image built it with a 10.16+ resolver.
- **MinIO is pinned** to `quay.io/minio/minio:RELEASE.2025-09-07T16-13-09Z` instead of
  `:latest`, so a fresh host brings up the MinIO the smoke tested, and Dependabot watches
  `/apps/ui` as well as `/deploy` — `node:22-alpine` and `nginx:alpine` in the image that
  serves the UI were the only base images nothing tracked.
- **The CI smoke reuses the images the `docker` job built.** It ran in parallel with
  `docker` and rebuilt the entire release graph from scratch under a 25-minute timeout;
  it now runs after it and imports the same `type=gha` layer cache (one scope per image —
  the three builds previously shared one scope and evicted each other).
- **The pool and S3 smokes derive the agent WebSocket URL from `FIBER_API_URL` and the
  DSN from `FIBER_DATABASE_URL`.** Both hardcoded `ws://127.0.0.1:18080` and the dev
  connection string, so against a second API on another port the assertions and the agent
  talked to different servers and the smoke passed anyway. Their fixed `sleep`s before
  negative assertions are polls: "the project-scoped agent does not take project B's
  step" is now checked continuously across a full offer cycle and fails the moment it is
  violated, rather than being sampled once after four seconds.
- **Compose ships no working credentials.** `FIBER_POSTGRES_PASSWORD`,
  `FIBER_REDIS_PASSWORD`, `FIBER_S3_ACCESS_KEY`/`FIBER_S3_SECRET_KEY` and
  `FIBER_ADMIN_PASSWORD` defaulted to `fiber` / `fiberfiber`, so a stack brought up with
  `FIBER_API_BIND=0.0.0.0` was a public instance with a published admin login. They are
  now `${VAR:?}` — `docker compose config`, `up`, `pull` and `logs` all fail, naming the
  first variable that is missing, until `deploy/.env` sets them.
  **Migration: before upgrading, write the values you are already using into
  `deploy/.env`** (`cp deploy/.env.example deploy/.env` and fill them in) — Postgres
  keeps the password its volume was initialised with, so putting the old value back is
  the no-downtime path. The S3 pair is required even when MinIO is off, because Compose
  interpolates the whole file before it filters by profile; any non-empty placeholder
  does. `make infra` and the smokes supply the old dev values themselves, so a
  development checkout needs no `deploy/.env`.
- **MinIO is optional; artifacts default to the local filesystem.** Compose set
  `FIBER_S3_BUCKET` unconditionally, so the S3 backend was the only one a Compose
  deployment could have, the `fiber_artifacts` volume was dead weight, and the API could
  not become ready without MinIO. MinIO now sits behind the `minio` profile and
  `FIBER_S3_BUCKET` is empty by default. **Migration: an install that wants to keep MinIO
  must set `FIBER_S3_BUCKET=fiber-artifacts` in `deploy/.env` and bring the stack up with
  `--profile minio`** — switching backends does not move existing blobs, and artifacts
  written under the other backend stop downloading until you switch back.
- **`fiber-api` runs as uid 10001 with a read-only rootfs.** The image ran as root while
  the agent image already did not, so any RCE or SSRF in the API started from uid 0.
  `fiber-ui` moves to `nginxinc/nginx-unprivileged` (uid 101, listening on 8080 in the
  container; the published port is still 3100), and both get `read_only: true` with a
  tmpfs for their scratch paths, `cap_drop: [ALL]` and `no-new-privileges`.
  **Migration: an existing deployment whose artifact directory was created by the root
  image needs a one-shot `chown -R 10001:10001`** — see
  [operations](docs/operations.md#upgrades) for the exact command. It matters to Compose
  now precisely because the same release makes the local filesystem the default backend.
- **Compose plumbs the operator knobs it documented.** The env block is an allowlist and
  silently dropped twelve variables `docs/configuration.md` told operators to set:
  `FIBER_PUBLIC_URL` (so commit statuses linked nowhere), `FIBER_GITHUB_TOKEN`,
  `FIBER_GITHUB_API_URL`, the four retention knobs, `FIBER_AGENT_STALE_SECS`, the two
  step-timeout knobs and the OTLP endpoint. All are passed through, `deploy/.env.example`
  lists every one of them, and the Compose table in `docs/configuration.md` now matches
  both. The OTLP endpoint is plumbed only as `OTEL_EXPORTER_OTLP_ENDPOINT`: the API reads
  `FIBER_OTEL_ENDPOINT` only when the standard name is *absent*, which a Compose
  passthrough never is.
- **Container logs are capped and the Redis password is off the command line.** Every
  service gets `max-size: 10m` / `max-file: 5`, where the json-file default keeps every
  line until the host disk fills and Postgres stops; `requirepass` reaches Redis through
  a Compose `configs:` file instead of argv, where any process in the host's pid
  namespace could read it out of `ps`. Redis's healthcheck is unauthenticated (`NOAUTH`
  is an answer from a live server) so that the password does not return to the service's
  environment just to satisfy a probe. It is still not hidden from the Docker daemon:
  `fiber-api`'s environment carries both passwords inside `FIBER_DATABASE_URL` and
  `FIBER_REDIS_URL`. Both images gained a `HEALTHCHECK`, so a plain `docker run` gets
  the probe Compose had.
- **The `.dockerignore` files are allowlists.** The root one was a denylist, so `deploy/`
  — including a `deploy/.env` holding `FIBER_SECRETS_KEY` — entered the build context.
  Nothing copies it today, but `Dockerfile.ui` already ends in `COPY . .`, and one such
  line in `deploy/Dockerfile` would bake every credential into a published layer. Both
  files now start from `*` and add back only what the builds read.

### Fixed

- **A step given up after a long outage no longer hangs its run.** The agent killed such
  a step and said nothing, so the row stayed `running` under it and the first heartbeat
  after reconnecting renewed the lease again — and every one after that. Nothing reported
  it, no lease ever expired, and the run hung until the 60-minute step-timeout backstop
  while the agent kept the concurrency slot. It now reports the attempt `failed` with
  `lease lost while the agent was disconnected`; the server takes the report only if it
  has not already reclaimed the step, and the step retries under its existing budget.
- **An artifact upload can no longer overwrite a later attempt's.** The three HTTP agent
  artifact routes checked the lease but not the attempt, and an upload retrying through
  an outage could land after the step was re-leased — `artifacts` is unique on
  `(step_run_id, name)` and the object key is deterministic, so the abandoned attempt's
  bytes replaced the live one's for a dependent step to restore. They now carry the
  attempt (`X-Fiber-Attempt`, or an `attempt` field) and refuse a stale one with `401`.
- **A rotated token now requeues on the socket's own replica.** The heartbeat that finds
  the token invalid ends the session with an immediate requeue, so a rotation or delete
  no longer depends on the Redis fan-out reaching the replica that holds the socket.
- **The per-attempt log cap is per attempt, not per socket.** The counter behind
  `FIBER_STEP_LOG_MAX_LINES` was kept per session, so an agent that reconnected
  mid-attempt started a fresh budget each time; it is now seeded from the lines already
  stored for that attempt.
- **An offer that cannot be built is not sent.** The lease was taken first and the offer
  built second — from the run's snapshot, the project's secrets and its artifacts — and a
  failure reading any of them was swallowed: a database blip during a heartbeat sent an
  offer with no workspace, env, secrets or restores, the step ran in an empty directory,
  failed, and spent a retry, with nothing in the log. The lease is now backed out (the
  step returns to the queue with its attempt counter restored and a **30-second
  backoff** on `not_before`, so a step whose offer keeps failing is not the very next
  thing every agent tries and cannot block the queue behind it), nothing is sent, the
  failure is logged at `error` with the run and step ids, and the fill pass moves on to
  the next candidate — skipping what it already backed out and giving up after five
  failures, so a database that is down costs one heartbeat a bounded amount of work.
  The `step_attempts` row is now opened only once an offer is built and about to be
  sent, so a backed-out offer leaves no near-zero-length attempt behind to fill the
  table and collapse the step-duration histogram.
- **Two pushes on two replicas can no longer both run in a `cancel_in_progress` group.**
  The superseded runs were looked up after the new run committed, and `created_at` was
  the transaction's start time, so two starts could each commit and each see the other
  as newer. A start (or retry) in a group now takes a transaction-scoped advisory lock on
  `(project, group)`, stamps the run with a timestamp taken inside the lock, and finds
  what it supersedes on the same connection; the cancels follow the commit and are
  guarded like any other. The decision (`superseded`) is a pure function with tests.
- **A retry now supersedes the older runs of its group.** `POST /runs/{id}/retry` reached
  the store directly, so a retry of a `main` build — which 0.6.0's fix gave the group —
  still did not cancel the run it replaced. It goes through the scheduler with every
  other start, and the source audit that enforces that now covers it.
- **A failed save after a durable `sleep` no longer skips the sleep.** `sleep` checkpointed
  its ordinal before the engine persisted `status = suspended` and `wake_at`; if that save
  failed, the row stayed `running` with no wake time, was reclaimed as stale, and resumed
  past the sleep — a 24-hour sleep became a 60-second one. The ordinal now reaches the
  database only in the engine's save, in the same `UPDATE` as the status and wake time.
- **A panicking fiber handler no longer keeps its row alive forever.** The step heartbeat
  outlived a panic, so the row was never stale, never reclaimed, never failed, and its
  attempts never spent. The heartbeat is aborted when the step is dropped — by return,
  `?`, or unwinding — and the poller records `fiber task panicked: …` and spends an
  attempt exactly as a returned error does.
- **`fiber-api` shuts down cleanly.** There was no signal handler: SIGTERM ended the
  process mid-request and mid-upload, and as PID 1 in Compose without an init it was not
  even delivered — the container was killed after 10 s. Now the listener closes, in-flight
  requests finish, every WebSocket session sends a Close frame (`1012`) and ends — the
  process waits for the sessions, not only for HTTP — the OpenTelemetry batch flushes,
  and the process exits, all within a 20 s bound. Compose runs it with `init: true` and
  `stop_grace_period: 30s`.

- **An agent whose host vanished no longer stays `online` for hours.** The server pings
  each agent socket every 15 s and closes it after 45 s without any frame, which takes
  the normal disconnect path; before, a half-open connection lingered until the kernel
  gave up on it while the agent kept being offered steps.

- **Readiness probes are bounded at 2 s.** A hung Postgres made `/ready` hang with it,
  so the orchestrator killed the pod for "probe timeout" with nothing in the log to say
  why; a dependency that does not answer in time is now reported down.

- **Supervisor backoff resets after a healthy run.** A loop that crashed once a day
  was, from day seven on, waiting the full 60 s cap before every restart with `/ready`
  at `503` the whole time. A loop that ran at least 60 s before dying starts over at 1 s.

- **A reclaimed step now reports what happened to it.** When an expired lease or a
  disconnect requeued or failed a step, the row changed but no event was published: the
  run page showed the step `running` until a reload, and a run that ended on a lost lease
  never sent its commit status to GitHub. Reclaim now publishes the same step and run
  events a reported completion does.
- **Cancelling a run whose row carries a legacy status no longer fails with a 500.** A
  status outside the vocabulary (tolerated by the new `NOT VALID` check) matched neither
  the terminal guard nor the cancel update; the run is now returned untouched, and the
  reclaim loop's orphan-run sweep leaves such runs alone for the operator instead of
  retrying them every tick.
- **A cancel can no longer overwrite a finished run.** `cancel_run` was three statements
  on three connections with no status guard on the run, so a cancel that landed after the
  last step completed turned `succeeded` into `cancelled` — and GitHub saw `success`
  followed by `error` for the same commit. It is now one transaction with the run row
  locked: a run that is already terminal is left exactly as it is, and a step leased
  between the read and the write is cancelled *and* its agent told, where before the row
  flipped and the agent kept running with a slot that was never released.
- **A step that keeps killing its agent now fails instead of running forever.** Reclaim
  (expired lease, agent disconnect) requeued unconditionally; only a failure the agent
  *reported* went through the retry budget. A step that OOM-killed its agent was leased,
  lost, and leased again indefinitely, its run never terminal and its `step_attempts`
  growing every five minutes. A lost lease now counts against `retries` like a reported
  failure does, with one extra try: a step is failed with `lease lost after N attempts`
  once it has lost more than `retries + 1` leases, and its run propagates like any other
  failure. The extra try is deliberate — a `retries: 0` step survives one rolling agent
  restart or one network blip, while a step that kills its agent every time still stops
  after two leases. The reclaim loop also finalises any run left `running` with no open
  step (the window between a reclaim committing and its propagation running), and a run
  deleted underneath a reclaim no longer aborts the sweep.
- **Lease, complete, and reclaim write the step and its attempt in one transaction.**
  Each used to be a `step_runs` update followed by a `step_attempts` write on a separate
  connection. A crash between them left an attempt open against a step that was back in
  the queue, and the timeout backstop then failed every later lease of that step within
  seconds as "timed out". The backstop now also matches the attempt to the step's current
  attempt number, so a stale open attempt can never be mistaken for the live one.
- **A retry keeps its concurrency group.** `retry_run` dropped `concurrency_group`, so a
  retried `main` build neither superseded nor was superseded by the next push.
- **Editing a pipeline no longer re-arms a schedule slot the loop just consumed.**
  `update_pipeline` wrote back a `next_due_at` it had read a moment earlier, racing the
  scheduler's compare-and-set and firing the pipeline twice. The due time is now decided
  inside the statement: kept when the cron or interval is unchanged, recomputed from the
  new rule when it changed (a daily → hourly switch no longer waits for the old daily
  time), cleared when the schedule is removed.
- **An admin can no longer demote or remove an owner.** `PUT`/`DELETE` on a member only
  required `admin`, and the last-owner guard existed only on `DELETE` and counted owners
  in a separate statement — an admin could strip every owner, or two concurrent removals
  could each see "two owners" and leave none. Changing or removing an owner now requires
  the actor to be an owner, and the last-owner rule is evaluated inside the `UPDATE` /
  `DELETE` under a lock on the project's owner rows. Refusals are `403` when the actor is
  not an owner and `400` when the target is the last owner.
- **Project creation is one transaction**, so a crash between the project and its first
  member cannot leave a project nobody owns.
- **Re-uploading an artifact replaces its row.** Steps are at-least-once, and a re-run
  inserted a second `(step, name)` row a dependent's restore then fetched twice.
  `artifacts` is now unique on `(step_run_id, name)`; migration 015 removes existing
  duplicates (keeping the newest; both rows pointed at the same blob) before adding it.
- **Missing indexes for three sweeps** that arrived after migration 007: the run-timeout
  backstop (`runs (started_at) WHERE status = 'running'`), retention and project delete
  (`artifacts (path)`), and fiber retention (`fibers (updated_at)` for terminal rows).
  `idx_log_lines_step (step_run_id, seq)` is dropped — nothing has ordered by `seq` since
  010, and it taxed every log insert.

## [0.6.1] — 2026-09-19

A security release. Two pipeline fields set by a project writer reached a process on the
agent host unchecked, and either one led to the agent's own token — on a global agent, to
every project's secrets. No migration and no schema change. Upgrade the API first, then
the agents: an agent older than this release still executes whatever an older snapshot
holds, and a pipeline stored with a now-invalid `image` or `workspace.repo` stops
producing runs until it is corrected (see the note below).

### Security

- **A step's `image:` and a pipeline's `workspace.repo` are now validated before they reach
  a process on the agent host.** Both are set by anyone who can edit a pipeline, which is a
  project writer. Docker accepted `image: "-v/:/host"` as a flag and mounted the host root
  into the step; git accepted `repo: "ext::<command>"` as a transport and ran the command
  during workspace preparation, before any container existed. Either path reached the
  agent's own token, and on a global agent that token receives every project's secrets.
  The pipeline now fails to compile (HTTP 400, `fiber validate`) unless `image` is a docker
  image reference and `repo` is an `http(s)`, `ssh`, `git`, or `file` URL, an scp-like
  `user@host:path`, or a path; the agent checks both again before use, passes the image
  after `--`, and pins `GIT_ALLOW_PROTOCOL`. A run whose snapshot predates this check fails
  its step with the reason rather than executing it. **A stored pipeline that no longer
  compiles stops producing runs**: a manual or webhook start returns the error, and a
  cron or interval pipeline skips its occurrence with only a server-side warning. After
  upgrading, run `fiber validate` on each `fiber.yml`, or re-save each pipeline, so a
  legacy `image` (say, one with a space in it) is found before its schedule is missed.
- **`/ws/runs/{id}` no longer accepts a session token in the query string.** The UI has
  sent it in the WebSocket subprotocol since 0.2; the fallback only put a two-week
  credential into access logs. `/ws/agent` keeps `?token=` for older agents.
- The pull-request file listing validates the repository owner, name, and number from the
  webhook body before building the GitHub API URL, as the commit-status path already did.
  A project admin, who sets their own webhook secret, could otherwise aim the instance's
  `GITHUB_TOKEN` at an arbitrary API path.
- A failure to decrypt a project's secrets (a wrong or rotated `FIBER_SECRETS_KEY`) is now
  logged at error level when an offer is built. It used to silently strip every secret
  from the step.

### Changed

- Steps and git on the agent run with no stdin and `GIT_TERMINAL_PROMPT=0`, so a command
  that waits on a terminal fails at once instead of at the step timeout. A credential in
  the `workspace.repo` URL is masked in the step log.

## [0.6.0] — 2026-09-19

Pipelines can keep one run per group, cancelling the older ones on a new push.
Adds migration 014 (a nullable column and an index; no data change) and no breaking
change: a pipeline that declares no `concurrency:` behaves exactly as before.

### Added

- **`concurrency:` in a pipeline, with `cancel_in_progress`.** A second push to a branch
  cancels that branch's previous build instead of racing it. Runs sharing a resolved group
  contend; the default `{pipeline}-{ref}` keeps branches independent, and a literal group
  (`deploy`) serialises across pipelines. `{pipeline}` is the pipeline id rather than its
  name, which is editable and not unique. An unrecognised placeholder is left as written:
  emptying it would merge groups the author meant to keep apart. Matching is scoped to the
  project, the resolved group is stored on the run so the rule stays readable after the
  pipeline changes, and a superseded run ends `cancelled` with reason
  `superseded by a newer run`. Omitting the block, or `cancel_in_progress: false`, means no
  limit — queueing is a separate behaviour and Fiber will not guess which was meant.
  Migration 014. Every path that starts a run now goes through the scheduler, and a source
  audit fails the build if a new one bypasses it.

- **A `matrix-build` showcase pipeline**, and tests that every seeded pipeline compiles.
  The seed had avoided `matrix:` and imitated it with hand-written parallel lanes — the
  canvas could not draw a matrix step until 0.5.0, so the one feature that needs a picture
  had none. It fans out on two axes, carries a per-cell `if` so one cell is visibly
  skipped, and ends on a step whose `needs` the compiler rewrites onto every cell. The
  axes are `rust` and `features` rather than `os`: a `macos` cell would sit queued forever
  on the single agent a demo instance has.
- **`seed::tests`** — the seeded pipelines were raw JSON that nothing ever compiled, so a
  malformed one was not a failing test but a pipeline that shows up in the demo project
  and cannot run. One test now compiles all of them; others pin the matrix expansion, and
  one refuses a demo whose terminal step hangs off a conditional, since `success()` is
  transitive and the last node would always be grey.

### Fixed

- **`fiber.yml` built this repo against a Rust it does not use.** It pinned
  `rust:1.97-bookworm` while `rust-toolchain.toml` and `deploy/Dockerfile` had moved to
  1.98, and `.github/workflows/ci.yml` declared 1.97 as well. The pin wins, so nothing
  failed — `rustup` downloaded a full 1.98 toolchain inside the container instead, on
  `fmt`, `clippy`, `test` and `build`, on every push and pull request, and in CI. All four
  files name 1.98 now, and `toolchain_audit` reads them and fails when they disagree.

## [0.5.0] — 2026-09-16

The web app is `apps/ui`, projects can be deleted, and the canvas tells the truth about a
run. Adds migration 013 (an index; no data change) and carries a **breaking rename**: the
Compose service `fiber-web` is now `fiber-ui`, `FIBER_WEB_BIND` is now `FIBER_UI_BIND`, and
the UI image builds from `apps/ui/Dockerfile.ui`. A deployment pinned to either name needs
updating; nothing about the running system changed. Upgrade the server before the agents.

### Added

- **A full-screen canvas** on the pipeline editor and the run view — the toolbar button,
  or `F` once the keyboard is in the canvas; `Escape` or **Exit** comes back. It expands
  the whole work area rather than the canvas alone: the graph takes the room the artifact
  list was using, and the step inspector and log stream stay docked beside it, so clicking
  a node still lands somewhere. Deliberately not the browser's Fullscreen API, which can
  only take one element — the canvas would have gone up alone and a selected step would
  have led nowhere. The graph re-fits to whatever box it ends up with, keyed off the
  element actually resizing rather than a timer, since React Flow can only fit to
  dimensions it has already measured.
- **`DELETE /api/projects/{id}`**, owner only. Nothing could remove a project, so smoke
  runs and abandoned experiments accumulated forever — one path-filter webhook in a
  well-used instance now matches dozens of leftover pipelines. Runs still in flight are
  cancelled first, so agents are told to stop while their rows still exist; then the
  cascade takes pipelines, runs, step runs, attempts, log lines, artifact rows, members,
  secrets, the webhook secret, durable fibers, and project-scoped agents. Artifact
  **blobs** are collected before the delete and removed afterwards if nothing else
  references them: retention only ever considers blobs belonging to runs it deletes
  itself, so a blob whose last row vanished would otherwise be leaked for good. Returns
  `{ok, cancelled_runs, runs_deleted, blobs_deleted}`. The UI exposes it in project
  settings behind typing the project name. Runs are deleted 100 at a time rather than in
  one statement — anyone can create a project and fill it with runs, and an unbounded
  cascade would hold a write transaction and a pool connection for as long as the project
  is large. Deleting the seeded **showcase** project is not permanent —
  `ensure_showcase` recreates it on the next `fiber-api` boot.
- **An index on `runs.retry_of`** (migration 013). The self-reference added in 009 is
  `ON DELETE SET NULL` with no index, so Postgres enforced it with a per-deleted-row
  `UPDATE runs SET retry_of = NULL WHERE retry_of = $1` that scanned the whole table.
  Deleting runs was quadratic in the size of the instance; retention paid it a little at
  a time, and project deletion would have paid it all at once.
- **Account and user management in the UI.** `POST /api/auth/password`,
  `DELETE /api/auth/sessions` and `POST /api/users` existed on the server but nothing in the
  UI reached them. Settings now changes your own password, signs out your other sessions,
  and — for instance admins — lists, creates, and promotes users.
- **A project Runs page** at `/p/{project}/runs`: every run, newest first, filterable by
  status and paged through `next_cursor` with **Load more**. The cursor pagination shipped
  with run ops; only the twelve most recent runs were ever reachable.
- **The pipeline editor covers the whole step schema.** `env`, `shell`, `working_directory`,
  `timeout_minutes`, `secrets` (as the three-state All / None / Pick the YAML actually has)
  and `continue_on_error` on a step, plus pipeline-wide `env`, run `timeout_minutes`, and
  `pull_request.types`. All of these could be written in `fiber.yml` and exported from the
  canvas, but not edited on it.
- **[docs/ui.md](./docs/ui.md)** — the pages, the canvas, the editor, and the run page.
- **Terminal durable fibers are cleaned up**, `FIBER_RETENTION_FIBER_DAYS`, default 7.
  Retention covered runs, artifacts and sessions; the `fibers` table grew forever. Memoized
  steps go with them by cascade. A **suspended** fiber is never touched however old its row
  looks: one sleeping for a month is waiting, not stale, and deleting it would silently
  cancel scheduled work.

### Changed

- **`compile_definition` stops checking for cycles twice.** It ran
  `is_cyclic_directed` and then `toposort`, which is the same question asked two ways —
  `toposort` can only order an acyclic graph and reports `Err` for anything else,
  including a step that needs itself. The guard is gone and the `map_err` that was
  already there carries it. The real gain is in the tests: with two checks in front of
  it, the cycle tests passed even when one was disabled, so neither was pinned. All
  three now fail if the remaining check breaks, and a fourth covers the self-need typo.
- **Web dependencies: React 19.2 to 19.3** (with `react-dom` and both `@types` in step),
  `lucide-react` 1.41 to 1.46, and `cn` 0.2.4 to 0.2.6. The `^` floors move with them, so
  `package.json` records the versions actually tested rather than the oldest that would
  still resolve. Beyond the UI suite, the canvas was exercised in a browser on React 19.3
  — step nodes, their accessible names, the token palette, and full screen in and out —
  because `tsc` and vitest would not notice a rendering regression.
- **`uuid` 1.26.1 and `aws-sdk-s3` 1.146.1**, both inside the ranges already declared,
  so this is a lockfile change only. uuid's release fixes v7 timestamp handling, which
  this codebase does not reach — it builds ids with `new_v4` and the `v4` feature alone.
  `aws-sdk-s3` 1.147.0 was available and deliberately skipped: it pulls a smithy cascade
  (`aws-smithy-types` 1.6 to 1.7, `aws-smithy-xml` 0.62 to 0.63, a new
  `aws-smithy-schema`) that deserves its own change rather than riding along with a
  patch.
- **`petgraph` 0.7 to 0.8**, with no code change — `DiGraph`, `toposort` and
  `is_cyclic_directed` are unchanged where `dag.rs` uses them. The compiled step list
  and the level map are built from the definition's own order rather than the graph
  walk, so petgraph making no promise about which valid topological order it returns
  cannot move a step or a level. Three tests now pin that, because the two the DAG had
  were a three-node fan-out and a two-node cycle: a level is the longest path from a
  root and not the shortest, declaration order does not change the compiled levels, and
  a cycle longer than two steps is still caught.
- **`tokio-tungstenite` 0.26 to 0.29**, which needed no code change and removes a
  duplicate rather than adding one: `axum` 0.8.9 already depends on 0.29, so the agent
  sitting on 0.26 meant the workspace carried two copies of the WebSocket stack — the
  one the server speaks and the one the agent speaks. They are now the same crate. The
  agent protocol is not covered by unit tests, so this was verified by building the
  agent and running the pools smoke against a live API: offers, claims, log streaming
  and completion across a scoped and a global pool, `SMOKE_OK`.
- **`redis` 0.29 to 1.7 and `aes-gcm` 0.10 to 0.11.** Redis needed no code change — the
  surface the scheduler uses (`ConnectionManager`, `publish`, `get_async_pubsub`,
  `on_message`) is unchanged across the 1.0 boundary — but nothing in the unit suite
  touches Redis, so the pub/sub round trip was exercised against a live server rather
  than inferred from a clean build. `aes-gcm` 0.11 deprecates `Nonce::from_slice` in
  favour of `TryFrom`, which is a hard error under clippy `-D warnings`; the cipher now
  builds its nonce that way and passes it by reference. **Stored secrets are unaffected**
  — a ciphertext written by 0.10 decrypts under 0.11, the `enc:v1:` layout is identical,
  and nothing needs re-encrypting. That is now pinned by a test carrying a literal
  0.10-era ciphertext, so a future bump that moves the nonce or the tag fails in CI
  instead of in production, where it would read as every secret being corrupt.
- **The DAG canvas passes an accessibility and typography audit it previously did not.**
  Every step node is a tab stop React Flow gives `role="group"` to, so the whole graph
  announced as "group, node" repeated once per step — nodes now carry a name built from
  the step, its status, and its dependency count (`nodeAriaLabel`), and the canvas region
  itself is labelled rather than being an unnamed `role="application"`. Two text colours
  were below WCAG AA on the node surface — the step id at **2.67:1** and the status line
  at **3.82:1** — and are now 4.5:1 and 6.2:1. The node's type ramp was six roles bunched
  at 9–10px distinguished only by colour; it is now three sizes (13 / 11 / 10px) with the
  9px label chips gone. The running-step pulse is `motion-safe:` and every viewport
  animation passes through `motionDuration`, so `prefers-reduced-motion` is honoured
  without losing the state change — the status dot and the status word still carry it.
- **The canvas stops re-rendering every node on every poll.** A status tick rebuilt each
  node's `data` object, so all of them got a fresh identity every few seconds and the
  `memo` on `StepNode` never skipped anything. `sameNodeData` compares first and keeps the
  object that is already there. The pipeline editor's keyboard handler likewise stopped
  re-registering its `window` listener on every render.
- **The graph re-fits when its box changes size**, including an ordinary window resize,
  which previously left it drifting off-centre with no recovery but the Fit View control.
  A viewport the viewer panned or zoomed themselves is left alone; expanding or tidying
  is an explicit request for a new view and fits regardless.
- **The canvas has a palette instead of a pile of literals.** Every colour it drew was
  hard-coded at the point of use — `oklch()` literals for surfaces, `rgba()` for edges
  and the minimap, and long chains of `white/N` over whatever happened to be underneath.
  It is now a semantic scale (`--canvas`, `--canvas-node`, `--canvas-fg-muted`,
  `--canvas-edge`, `--canvas-artifact`, `--canvas-matrix`, …) defined for both themes,
  and the dark values are the ones it already shipped — each alpha stack resolved to the
  solid colour it was compositing to, so the rendering is unchanged and contrast no
  longer depends on what is behind the text. A light theme is composed rather than
  inverted: paper-white nodes on a faintly tinted ground, a darker sky for edges and
  accents, and every text pair verified at 4.5:1 or better (icons at 3:1). `statusColor`
  stays a function — status is the one role that must not be remapped by a theme.
- **React Flow's attribution is hidden by the library's own `proOptions` prop** rather
  than by a `!important` CSS override contradicting `hideAttribution: false`. Same result,
  one mechanism. @xyflow/react is MIT and we run no Pro licence.
- **The sidebar starts as the icon rail.** Fiber is canvas-first and 16rem of chrome is
  16rem the DAG does not get. Collapsing it was already remembered; now that the rail is
  where you start, it had to earn the job: the global agent pool takes a distinct glyph
  from a project's own agents (they shared one, and collapsed there are no labels to tell
  them apart), a rule stands in for the group labels that fade out in icon mode, and the
  active item tints its own mark, because the neutral highlight is invisible at 16px on
  near-black. Someone who prefers the wide sidebar still gets it, restored on load.
- **The smokes clean up after themselves.** `smoke-pools` and `smoke-s3` delete the
  projects they create, and the path-filter half of `smoke-artifacts` now runs in a
  project of its own instead of adding a `paths-test` pipeline to the seeded showcase on
  every run — one instance had reached 31 of them, so a single webhook started 31 runs
  and the assertions had to settle for "ours is somewhere in the set". They are now
  equalities, and the smoke no longer leaves a webhook secret on showcase. A *failing*
  smoke keeps its project: the pipelines, runs and logs inside are the only record of
  what went wrong.
- **`make smoke` runs `smoke-pools` last.** It terminates every `fiber-agent` on the host
  before starting its own, so running it second left `smoke-artifacts` with no agent and
  the combined target failed every time. `smoke-artifacts` now also checks for an online
  agent up front and says so, rather than waiting ninety seconds to report
  `run failed: running`.
- **`apps/web` is now `apps/ui`.** The directory, the package, the Docker image and its
  `Dockerfile.ui`, the `fiber-ui` Compose service, the `FIBER_UI_BIND` variable, the CI job,
  and `make ui` (was `make web`). A deployment pinned to `FIBER_WEB_BIND` or driving the
  `fiber-web` service needs updating; nothing about the running system changed.
- **Project settings moved off the project overview.** Members, secrets, and the GitHub
  webhook are at `/p/{project}/settings`; the overview is pipelines and recent runs. The
  global Settings page no longer asks you to paste a project UUID to set a webhook secret.
- **The sidebar reaches every project page.** Runs, Agents and Settings were routes with no
  link — `/p/{project}/agents` existed and was unreachable.
- **Canvas layout orders columns by barycentre** instead of definition order, so fan-out and
  fan-in shapes stop crossing their own edges, and columns are centred against the tallest.
  **Tidy** re-runs the layout; until then, a node you drag stays where you put it.
- **The end-to-end scripts are "smokes", not "dogfood".** `scripts/dogfood_*` are now
  `scripts/smoke_*`, `make dogfood` is `make smoke` (and `dogfood-authz` … `dogfood-compose`
  are `smoke-authz` … `smoke-compose`), the CI job is `smoke`, and the scripts print
  `SMOKE_OK` / `SMOKE_FAIL` instead of `DOGFOOD_OK` / `DOGFOOD_FAIL`. The scripts smoke-test
  a local stack; they were never dogfooding, which is a different thing this project also
  does — running Fiber against its own repository, still described under that name in
  [docs/triggers.md](./docs/triggers.md#dogfooding-this-repo). Anything scripted against the
  old target or marker names needs updating.

### Fixed

- **A shared agent permanently lost a concurrency slot when a step's rows vanished under
  it.** `on_step_complete` returned early for a step it could no longer find, before the
  block that gives the slot back, so the in-memory `inflight` counter never came down. A
  *global* agent — one serving every project — running a step of a project that was then
  deleted would lose capacity until it reconnected. The release decision is now
  `releases_slot`, a pure function with the cases enumerated in tests.
- **A deleted agent could re-register itself into the global pool.** The `/ws/agent`
  `Hello` arm read the agent's row to decide its pool scope but flattened a missing row
  into `None`, which is also how a *global* agent is spelled. A project-scoped agent
  whose row had just been cascade-deleted could therefore replay `Hello` on its still-open
  socket and land in the pool that is offered every project's steps. Nothing read that
  field for authorization, so it was not exploitable — it is now a closed session instead
  of a silent promotion.
- **A run's canvas showed nothing for a matrix step.** It was built from the definition
  snapshot, which holds the step as the author wrote it, while statuses and logs are keyed by
  the compiled cell ids (`build__os_linux`). The node's status never resolved and clicking it
  selected a step that did not exist. The canvas is now built from the run's step runs — the
  DAG that actually ran — so each cell is its own node, tagged with its bindings.
- **Editing an edge on the canvas silently dropped pipeline and step fields.** Rebuilding the
  definition from the nodes listed the fields it knew about, so connecting two steps discarded
  pipeline `env` and `timeout_minutes`, and every step's `env`, `shell`, `working_directory`,
  `continue_on_error`, `timeout_minutes` and `secrets`.
- **Deleting a node on the canvas never reached the definition.** The graph and the saved
  pipeline disagreed until reload; `needs` pointing at the deleted step are now dropped too.
- **A connection that would close a cycle is refused** rather than drawn and rejected on save.
- **Selecting a step reset every node you had dragged.** Positions were recomputed on every
  status and selection change, which also yanked the canvas around during a live run.
- **The minimap drew no nodes on a run.** Read-only canvases withheld `onNodesChange`, which
  is how React Flow reports measured sizes back into controlled state; nodes without a
  measured size are skipped by the minimap. Draggability is gated by `nodesDraggable`.
- **Retention did nothing but purge sessions when `FIBER_RETENTION_DAYS=0`.** The disabled
  path looped separately and never reached the tick, so anything else retention grew to
  cover was silently skipped in that configuration. There is one loop now, and each part
  decides for itself whether it is switched on.

## [0.4.1] — 2026-09-11

Two durable-runtime bugs that the `http_request` task made reachable, and tokens off the
WebSocket query string. No schema change.

### Added

- **A per-attempt cap on stored step logs**, `FIBER_STEP_LOG_MAX_LINES`, default 50,000.
  Reading was bounded by pagination, but writing had no ceiling: a runaway step could fill
  the disk. Past the cap lines are dropped and one `system` line says so, since silence
  looks like a step that stopped producing output. A retry gets its own budget, truncation
  never fails a step, and `0` disables it.

### Changed

- **Tokens are off the WebSocket query string.** `/ws/agent` takes
  `Authorization: Bearer`, and `/ws/runs/{id}` takes the session token as a
  `Sec-WebSocket-Protocol` value, since a browser cannot set headers on a WebSocket. A URL
  ends up in proxy and server access logs; an agent token, which leases steps and receives
  project secrets, has no business being there. Both endpoints still accept `?token=` so an
  agent older than the server keeps working, and the API logs a warning when one arrives
  that way — but upgrade the server before the agents, not the other way round.

### Fixed

- **A durable step lasting over a minute was executed twice.** The heartbeat was written
  only when a step finished, so a step outliving the 60-second staleness threshold looked
  like a crashed fiber and was claimed and run again by the next sweep. A step now
  heartbeats every 15 seconds while it runs. The `http_request` task, whose timeout reaches
  300 seconds, made this trivially reachable.
- **Waking from a sleep no longer spends a retry.** Claiming a fiber incremented `attempts`,
  and a wake from a durable sleep is a claim, so a fiber that slept three times hit the
  limit and failed while working perfectly — a task with its own retries could not use them.
  `attempts` now counts failures and crash-reclaims, which is what the retry budget was
  always about.

## [0.4.0] — 2026-09-09

Durable fibers become useful, and accounts get the two operations they were missing.
No schema change.

### Added

- **An `http_request` durable task.** Call a URL with retries that survive a restart: the
  wait between attempts is a suspension, not a held task. 5xx, 429 and connection errors are
  retried, a 4xx is not, and each attempt carries an `Idempotency-Key` of
  `<fiber id>:<attempt>` so a receiver can make at-least-once delivery harmless. This is the
  first task driven entirely from user input, so it is the first with a threat model: it
  makes the **API** issue the request, and the API can see the backend network and cloud
  metadata. Private, loopback, link-local, unique-local and CGNAT addresses are refused, all
  resolved addresses are checked rather than the first, and redirects are not followed.
  `FIBER_HTTP_TASK_ALLOW_PRIVATE=1` lifts it, as an operator's decision.
- **Change your own password, and revoke your sessions.** `POST /api/auth/password` takes
  the current password and drops every other session for that user, since a password change
  is what someone does when they believe a credential is compromised. `DELETE
  /api/auth/sessions` does the revocation alone, for a leaked token — previously the only
  remedy was waiting out the 14-day expiry. Neither is something an admin can do to someone
  else's account.

### Fixed

- **The dogfood scripts ignored `FIBER_API_URL`.** All four hardcoded `127.0.0.1:18080`, so
  running one against a second API on another port — exactly what you do to check a change —
  silently tested whatever was already on 18080 and passed without exercising the new build.

## [0.3.2] — 2026-09-08

Two fixes for failures that were previously invisible. No schema change.

### Fixed

- **The background loops are supervised.** `reclaim`, `schedules`, `events`, `agent_cmds`,
  `fibers`, `github_status` and `retention` were spawned bare, so a panic in one killed that
  task while the process stayed up and `/ready` kept answering `ok` — leases quietly stopped
  being reclaimed, or schedules stopped firing, with nothing to say so. Each is now restarted
  with backoff, `/ready` lists any that are down and fails, and `/metrics` exposes
  `fiber_background_loop_up` and `fiber_background_loop_restarts_total`. The restart counter
  is the one to alert on: a loop that keeps recovering is failing repeatedly and nothing else
  would tell you.
- **Release assets were non-deterministic.** The publish job downloaded every workflow
  artifact, which swept in the build records the image jobs upload. Whether they appeared
  depended on which job finished first, so a release sometimes carried four stray
  `.dockerbuild` files and sometimes did not — and one of them being zero-length failed a
  publish outright. It now takes only the binary artifacts, refuses to publish an empty or
  zero-length asset, and the image jobs no longer produce build records at all.

## [0.3.1] — 2026-09-08

Durable fibers: task discovery, and cancel that means it. No schema change.

### Added

- **`GET /api/fibers/tasks`**, the durable task names this build registered. The Fibers page
  asks for that instead of carrying its own copy of the list, which went stale the moment a
  task was added or removed.

### Fixed

- **Cancelling a durable fiber works, and says so.** A cancel was recorded as `failed` with
  the message `cancelled`, which a dashboard cannot tell apart from a task that actually
  broke; there is now a `cancelled` status. It was also possible for the engine to overwrite
  it: the record it holds predates the handler running, so finishing would resurrect a fiber
  someone had stopped and report it completed. Cancel is now terminal, so the poller does not
  pick it up again, and a save is rejected outright if the row has been cancelled meanwhile.

## [0.3.0] — 2026-09-07

Pipeline schema. Three additive step fields, no schema change and nothing to do on upgrade
beyond running the new version. Existing pipelines are unaffected: every field defaults to
what the previous behaviour already was.

### Added

- **`continue_on_error:` on a step.** The failure is still recorded — the run page shows the
  step red, with its exit code and logs — but it does not fail the run and dependents run
  rather than being skipped, so a green run can contain a red step. Tolerance does not
  travel: a dependent that fails for its own reasons still cascades. A cancel is never
  tolerated, since that is an operator stopping the run rather than the step's own outcome.
  For gating, a tolerated failure counts as success, so a dependent's default `success()`
  passes.

- **`working_directory:` and `shell:` on a step.** Run somewhere else in the tree, or under
  a different interpreter than `sh`. `artifacts` paths stay relative to the workspace root
  rather than to `working_directory`, so moving a step does not silently change what it
  publishes. A directory must stay inside the workspace and a shell must be a bare program
  name; both are rejected when the pipeline compiles, again when the offer is built from the
  snapshot, and once more by the agent before it spawns anything — the agent additionally
  resolves the path, which catches a symlink the repository itself planted.

- **`env:` in the pipeline schema**, on the pipeline for every step and on a step for that
  step. Precedence runs least specific first: pipeline, then step, then matrix bindings —
  a matrix binding wins because it is what says which cell is running, and a step able to
  shadow it would make its own logs lie. Names must be usable as environment variables and
  `FIBER_*` is reserved, both checked when the pipeline compiles, so a bad name is a `400`
  rather than a variable that silently never arrives. Values are not secret: they live in
  the definition and are snapshotted onto every run. `examples/env-vars.yml` is a worked
  example.

## [0.2.7] — 2026-09-07

Dependency maintenance, including the Rust toolchain and the database driver. No schema
change: migration `012` shipped in 0.2.6 and nothing has been added since.

### Changed

- **sqlx 0.9.** Its new `SqlSafeStr` bound refuses a query string built at runtime unless it
  is explicitly asserted safe, which forced an audit of all 25 sites in `store.rs` that
  build SQL with `format!`. Every one splices only a column-list `const`; every value was
  already a bind parameter. Nothing had to change but the assertions, and `store.rs` now
  carries a note saying what any future `AssertSqlSafe` there has to hold to. The one
  genuinely dynamic helper takes `&'static str`, so the compiler — not a comment — stops a
  caller passing runtime data into it.

- Rust dependencies: `hmac` 0.13 with `sha2` 0.11 (they move in lockstep — 0.13 does not
  build against 0.10), `getrandom` 0.4, `tower-http` 0.7, and the toolchain to 1.98 across
  `rust-toolchain.toml`, `Cargo.toml`, the release workflow and the Dockerfile, which
  Dependabot only bumps in one place. The unused direct `password-hash` dependency is gone;
  the code reaches it through argon2's re-export.
- AES errors are formatted rather than wrapped with `.context()`. Whether that type
  implements `std::error::Error` depended on a `std` feature another crate happened to
  enable, and moving to `sha2` 0.11 took it away — which broke `cargo test -p fiber-core`
  while the workspace build still passed.

## [0.2.6] — 2026-09-07

**Upgrading:** this release carries migration `012`, the first schema change since the
project went public. It applies automatically when `fiber-api` starts, adds two nullable
columns, and backfills `queued_at` for steps queued at that moment. There is nothing to run
by hand and no downtime step, but an older `fiber-api` will refuse to start against the
upgraded database, so roll the API forward rather than mixing versions.

### Changed

- Web development dependencies: vitest 4 to 5, jsdom 28 to 30, and TypeScript 6 to 7.
  `@types/node` stays on 22 to match the Node the project actually runs — CI, `fiber.yml`,
  and the web image all use Node 22, and types a major ahead would accept calls the runtime
  does not have. Dependabot is now told to skip that major.

### Added

- **Queue-wait and step-duration histograms on `/metrics`.** `fiber_step_queue_wait_seconds`
  and `fiber_step_duration_seconds` make a p95 answerable; the gauges only ever showed the
  current worst case. Both are per attempt, since a retried step waited twice and ran twice.
  Migration `012` records when a step became leasable and stamps the wait onto the attempt
  that picked it up, so a later requeue cannot rewrite an earlier attempt's history.
  Attempts from before this have no wait recorded and are absent rather than counted as zero.

## [0.2.5] — 2026-09-07

Finishes the tracing work: a run now reads as one trace across the API and the agent.

### Added

- **A step is one trace across both processes.** The offer carries a W3C `traceparent`, so
  the agent's `fiber.step` span is a child of the API's `fiber.offer` rather than a root of
  its own. The field is additive and older agents ignore it; an agent that receives no trace
  context behaves as before.

### Fixed

- **`RUST_LOG` could not raise the log level.** Both binaries added a `fiber_api=info` /
  `fiber_agent=info` directive on top of the environment filter, which overrode what
  `RUST_LOG` said about that very crate, so `RUST_LOG=fiber_agent=debug` silently did
  nothing. It is now a default, applied only when `RUST_LOG` is unset.

## [0.2.4] — 2026-09-07

Observability. OpenTelemetry export worked in no previous version, and there is now a
Prometheus endpoint and instrumentation on the agent, where steps actually run.

### Fixed

- **OpenTelemetry export never worked.** The exporter is built with the async reqwest
  client, but the SDK runs batch and periodic exporters on their own threads with no Tokio
  reactor, so the first export panicked that thread and nothing ever reached a collector.
  It now uses the blocking client, which matches that threading model. Separately,
  `OTEL_EXPORTER_OTLP_ENDPOINT` is defined as a base URL and was being used verbatim, so
  every export went to `/` instead of `/v1/traces` — a real collector answers 404. The
  signal path is now appended, and a full signal URL is still accepted. Native certificate
  roots, so a collector behind a private CA works.

### Added

- **The agent exports OpenTelemetry.** A `fiber.step` span per execution with `run_id`,
  `step_run_id`, `kind` and `outcome`, plus a `fiber.agent.steps` counter and a
  `fiber.agent.step.duration` histogram measured from offer to completion, both labelled by
  outcome and by whether the step ran in a container. `service.instance.id` comes from the
  agent's `--name`, since every agent reports the same service name. Agent and API traces
  are not yet joined: the offer carries no trace context.

- **`GET /metrics`**, Prometheus exposition of the queue, runs, agents, and durable fibers.
  Off until `FIBER_METRICS_TOKEN` is set, then requires it as a bearer token — the API is
  internet-facing in a normal deployment and these figures describe your build volume, so it
  fails closed. Values are read from the database on scrape rather than counted in the
  process, so a restart resets nothing and two replicas agree.
  `fiber_oldest_queued_step_age_seconds` is the one to alert on: it climbs when no agent
  matches a step's labels, which was previously invisible until someone noticed a run
  sitting still.

## [0.2.3] — 2026-09-07

Artifacts work from a containerised agent, and Compose runs on the published images.

### Fixed

- **A containerised agent can produce and consume artifacts again.** The Compose agent runs
  on its own network, away from Postgres, Redis, and MinIO, so it could not reach the
  presigned URL the API handed it — that URL names the storage endpoint as the host reaches
  it, which inside a container is the container. Every artifact upload from it failed, and
  restores failed on the matching redirect. The agent now falls back to transferring through
  the API, which it can reach by definition. Direct transfer is still tried first, and the
  fallback logs why it engaged. `GET /api/agent/artifacts/{id}/download` accepts `?via=api`
  to stream bytes instead of redirecting.

### Changed

- **Compose runs the published images.** `deploy/docker-compose.yml` pulls
  `ghcr.io/durablefibers/fiber-api` and `fiber-agent` instead of building from the working
  tree, so a deployment needs no Rust toolchain. `FIBER_VERSION` in `deploy/.env` picks the
  tag and defaults to `latest`; `docker compose up --build` still builds locally.

## [0.2.2] — 2026-09-07

### Fixed

- **The published images are multi-architecture.** `ghcr.io/durablefibers/fiber-api` and
  `fiber-agent` carried only `linux/amd64`, so `docker run ghcr.io/durablefibers/fiber-agent`
  failed outright on Apple Silicon, arm64 Linux, and Graviton with `no matching manifest for
  linux/arm64`. Both now ship `linux/amd64` and `linux/arm64`, built on native runners rather
  than under emulation, and joined into one manifest list per tag. The release binaries
  already covered arm64; the images did not.

## [0.2.1] — 2026-09-07

Two correctness fixes found by running Fiber's own pipeline on Fiber. Anyone on `0.2.0`
whose steps use `image:` wants this one.

### Fixed

- **Docker steps no longer lose the image's `PATH`.** Step commands ran under `sh -lc`, and
  a login shell sources `/etc/profile`, which on Debian resets `PATH` to a fixed default.
  Anything the image put there was discarded, so `cargo` in `rust:*` (which lives on
  `/usr/local/cargo/bin`) was simply not found and a plain `cargo fmt` step failed with
  `sh: 1: cargo: not found`. Steps now run under `sh -c`, on the host as well, so the
  environment the agent assembles is the environment the command sees.
- **A failed artifact upload now fails the step.** A declared artifact that existed but
  could not be stored — unreadable, over the 64 MiB cap, an unsafe path, or a failed
  transfer — was logged and then ignored, so the step reported success and a later step
  that `needs` it failed with a missing file instead. The step now fails with the real
  reason and its dependents are skipped. A declared path that does not exist stays a
  warning, so a step may still declare an artifact it only sometimes produces.

## [0.2.0] — 2026-09-06

The first tagged release. Security and correctness hardening from a full platform audit,
agent packaging, run operations, GitHub commit statuses, and the open-source licensing.
Schema migrations `005`–`011` apply automatically on `fiber-api` boot. Steps without a
timeout now inherit `FIBER_STEP_TIMEOUT_DEFAULT_MINUTES` (60) — see the upgrade note in
`docs/operations.md`.

### Security

- **Global agents are instance-admin only.** Any authenticated user could previously create,
  update, delete, or rotate the token of a global agent, whose token leases steps — and receives
  the injected secrets — from every project. New `users.is_admin` flag gates that, plus
  `GET /api/agents` without a project filter and all user management
  (`GET`/`POST /api/users`, new `PUT /api/users/{id}`).
- **Agent identity is bound to its token.** `Heartbeat` no longer rebinds the session from a
  client-supplied `agent_id`, so an agent can no longer impersonate another, renew its leases, or
  receive its offers. Log lines require the step to be owned by the sender; artifacts and
  completions require a live lease. Artifact restore downloads are limited to runs where the agent
  holds a running step. A socket that never sends `Hello` is offered nothing.
- **GitHub webhooks fail closed.** Deliveries are rejected with `401` until a secret is configured
  (previously unsigned deliveries were accepted, letting anyone who knew a project id start runs).
  Webhook secrets are encrypted at rest and unique per project and provider.
- **Deployment defaults.** Compose binds Postgres, Redis, and MinIO to loopback, requires a Redis
  password, sets restart policies and a MinIO healthcheck, and reads settings from `deploy/.env`
  (the committed sample `FIBER_SECRETS_KEY` is gone). The API gained a `FIBER_CORS_ORIGINS`
  allowlist, a per-username login throttle, and masked error responses; S3 credentials are required
  rather than defaulted. The CLI writes `~/.fiber/token` as `0600` and accepts `--password-stdin` /
  `--value-stdin`.
- **Steps only see what they need.** A step's environment is cleared before its own is
  applied, so repo-supplied shell can no longer read the agent's `FIBER_AGENT_TOKEN`
  (which would let it lease other projects' steps and read their secrets). Docker steps
  receive their environment through a `0600` env-file instead of `-e KEY=VALUE`, which
  put every project secret in the host's process list. Secret values are masked as `***`
  in log lines. A new `secrets:` list on a step narrows which project secrets it gets at
  all; omitted still means all of them.
- **Container limits.** Step containers run with `--security-opt no-new-privileges` and a
  512 process limit, plus configurable `--user`, `--network`, `--memory`, and `--cpus`
  (`FIBER_AGENT_DOCKER_*`). Memory and CPU limits are off by default so an upgrade cannot
  start OOM-killing existing builds; whatever applies is logged as a `system` line.
- **Environment-variable names are validated** before reaching a step. A `docker
  --env-file` line without `=` means "copy this variable from my own environment", so a
  pipeline could otherwise use a matrix axis name containing a newline to make the docker
  client hand the step the agent's token. The docker client now also starts from a cleared
  environment.
- **Pull requests from forks are contained.** Such a run is marked untrusted: it receives
  no project secrets whatever its `secrets:` says, and its steps are offered only to
  agents bound to that project, never to the global pool. Unknown provenance counts as
  untrusted and a retry stays untrusted. This limits the blast radius rather than
  sandboxing the code — the docs say plainly that these runs belong on a disposable,
  project-dedicated agent.
- Commit statuses are posted only with a **project** token (never the instance-wide
  environment one) and only to the repository the pipeline's workspace points at, so a
  project cannot aim the instance's credentials at someone else's repository. Webhook
  `head_sha` / `head_ref` are validated before reaching `git`, which is also invoked with
  `--`; a commit outside the shallow window is fetched in bounded steps and a run that
  cannot check out its commit fails rather than building a different one.

### Added

- **The project is open source under Apache 2.0.** `LICENSE`, `NOTICE`,
  `CONTRIBUTING.md`, `SECURITY.md` (with the trust model spelled out), and a
  Contributor Covenant `CODE_OF_CONDUCT.md`, plus issue and pull-request templates
  and Dependabot for Cargo, npm, Actions, and Docker.
- **Step and run timeouts.** `timeout_minutes` on a step (per attempt) and on the pipeline (whole
  run). Agents enforce their own deadline and kill the process group and container; the server is a
  backstop after `FIBER_STEP_TIMEOUT_GRACE_MINUTES`.
- **Agent packaging.** A published `fiber-agent` image, an optional `fiber-agent` Compose service
  (isolated from Postgres/Redis/MinIO on its own network), a systemd unit, and
  `scripts/install-agent.sh` for attaching a second machine. Tagging `vX.Y.Z` runs the gate and
  publishes images plus agent/CLI binaries.
- **Runs record the commit they are for** (`head_sha`, `head_ref`, `pr_number`,
  `repo_full_name`). The agent checks out that commit exactly, so a second push while a
  run is queued no longer retargets it, and pull requests are fetched as
  `refs/pull/<n>/head` from the base repository — which is what makes a **fork's pull
  request** build at all. A retry re-runs, and reports against, the same commit.
- **GitHub commit statuses.** A webhook-triggered run reports `pending` when it starts and
  `success` / `failure` / `error` when it finishes, under the context
  `fiber/<pipeline name>`, so a pull request can require it as a check. Needs a token with
  `repo:status`; without one nothing changes, and a status that cannot be posted never
  fails a run. `FIBER_PUBLIC_URL` makes the status link to the run page.
- **Re-run a run**: `POST /api/runs/{id}/retry`, and buttons on the run page. It builds
  from the original run's definition snapshot, so it reproduces what that run executed.
  `failed_only` carries over the steps that already succeeded — copying their artifacts
  forward so dependents can still restore them — and re-runs the rest.
- **Attempt-scoped logs.** `log_lines` records which attempt produced each line, and
  `GET /api/steps/{id}/logs?attempt=N` narrows to it. The run page's attempt selector now
  changes the log pane instead of showing every attempt interleaved.
- **Pagination.** `GET /api/projects/{id}/runs` takes `?limit=&before=` and returns
  `{ items, next_cursor }`; older runs were previously unreachable past the newest 50.
  `GET /api/steps/{id}/logs` takes `?after_id=&limit=` and returns the newest lines by
  default rather than the entire log — a step that printed millions of lines could
  previously exhaust the API's memory.
- **CLI parity with the API.** `pipelines apply` pushes a `fiber.yml` (create or update,
  compiled locally first); `run --wait` / `--follow` block and exit with the run's outcome
  (0 succeeded, 1 failed, 3 timed out) so another CI system or a git hook can gate on it;
  plus `projects`, `runs list/get/cancel/retry`, `logs` (with `--attempt` and `--follow`),
  `artifacts list/download`, `logout`, `completions`, and a global `--json`.
- `fiber.yml` at the repository root: Fiber's own gate, for dogfooding.
- `--version` on `fiber-api` and `fiber-agent`.
- Agent lifecycle hardening: SIGTERM stops steps and lets the server requeue them (a rolling
  restart no longer fails a build), exponential reconnect backoff with jitter, exit on a revoked
  token, and local enforcement of `--concurrency`.
- CI runs `cargo test`, Biome, vitest, the web build, and both container images.

### Fixed

- **Offers are built from the run's definition snapshot only.** Editing a pipeline mid-run no
  longer changes the workspace, command, or artifacts of runs already started.
- **Step propagation is transactional** (run row locked, single read, fixpoint planner), so
  concurrent sibling completions cannot race and a failed chain no longer strands later steps as
  pending. `always()` steps now really run after an upstream failure, and `success()` is transitive
  over the whole ancestry, so a step after an `always()` cleanup does not run against a failed build.
- **Multi-replica safety.** Scheduled runs are claimed with a compare-and-set on `next_due_at`,
  durable fibers with `FOR UPDATE SKIP LOCKED`, and cancel / token revocation reach the replica
  holding the agent's socket over a new `fiber:agent_cmds` Redis channel. Revocation also fails
  closed: heartbeats re-check the token. Schedules created after boot now fire without a restart.
- **Retry backoff is real.** It is persisted on the row (`step_runs.not_before`) instead of a
  sleeping task pushing to a Redis list nobody read, and an attempt lost to a disconnect counts
  against `retries`.
- **Each step gets its own workspace**, so steps of one run on the same agent no longer
  overwrite each other's build output. Steps of a run share one git clone, so the second
  step costs a checkout rather than another fetch.
- **Workspaces are cleaned up**: a step's directory goes when it finishes (including on
  cancel, timeout, or failure), the run's tree when its last step on that agent finishes,
  and anything older than `FIBER_AGENT_WORKSPACE_TTL_HOURS` is swept at startup. They
  previously accumulated for the life of the agent.
- **Artifacts restore from dependencies only.** A step receives the artifacts of the
  steps it transitively `needs`, not every artifact in the run, so a parallel sibling
  cannot drop files into its workspace.
- Retention deletes an artifact blob only when no remaining run references its path, so a
  retry cannot lose the artifacts it inherited.
- Nine indexes for the hot paths (lease renewal, expired-lease reclaim, queued offers, artifact
  restore lists, the retention cascade, session purge).
- Agent log lines use a single sequence per attempt; stdout and stderr no longer collide after
  1000 lines.
- **The released binaries no longer link OpenSSL.** The agent's WebSocket client used
  `native-tls`, so the published `fiber-agent` / `fiber` tarballs failed to start on any host
  without `libssl3` — including `debian:bookworm-slim`. Both the WebSocket and the HTTP
  client now use rustls with the host's certificate store, which also means a self-hosted
  instance behind a private CA works: previously the WebSocket trusted the system store
  while artifact upload and restore trusted a bundled root list, so steps ran but their
  artifacts silently failed to transfer. `SSL_CERT_FILE` and `SSL_CERT_DIR` are honoured.
- The agent logs the whole error chain when a session fails, instead of just
  `connect websocket` with the cause dropped.

### Changed

- Container images build from one `deploy/Dockerfile` with `--target fiber-api` / `fiber-agent`,
  sharing a cargo-chef dependency layer. `deploy/Dockerfile.api` and the duplicated
  `deploy/Dockerfile.ui` / `deploy/nginx.conf` are gone.

## [0.1.0]

Initial release: DAG pipelines on a canvas, agents (global and project pools), artifacts
(local and S3), project roles, retention, GitHub push/PR triggers with path filters,
cron and interval schedules, matrix and `if`, durable fibers, and the CLI.
