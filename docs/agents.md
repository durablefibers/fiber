# Agents

Agents are outbound WebSocket workers that execute CI steps.

An agent's identity is the token it connected with. The server binds `agent_id` from that token and ignores the `agent_id` carried in messages (a mismatch is logged). Log lines are accepted only for a step **last leased to that agent** (so the tail of a cancelled or reclaimed step is still recorded); artifacts and completions additionally require the lease to be **live** — anything after a reclaim or cancel is dropped, which is the at-least-once contract: the re-leased attempt reports its own result. Artifact restore downloads are limited to runs in which the agent holds a running step. A socket that never sends `Hello` is offered nothing, and pool scope always comes from the agent's database row, never from the connection.

A lease belongs to the agent, not to the socket. A step keeps running through a lost connection — an API restart, a proxy timeout, a network blip — and the agent renews its leases on its first heartbeat back; the server judges every late message by the row (is the step still `running` under this agent, on the attempt the message names?), never by which session it arrived on. Every `Offer` carries the attempt number and the agent echoes it on each log batch, artifact, and completion — and on the HTTP artifact routes, as `X-Fiber-Attempt` on the upload and an `attempt` field in the presign and complete bodies ([api](./api.md#agent-endpoints)) — so output an agent held through a reclaim can never be filed under, overwrite, or close the attempt that replaced it; a message whose attempt is not the row's is dropped with a warning, and an upload for one is refused with `401`. A log line the agent re-sends after an aborted write can appear twice: `log_lines` is append-only with no dedupe. See [Lifecycle](#lifecycle) for the limits.

`Hello` carries `protocol_version` (`fiber_proto::PROTOCOL_VERSION`, currently **2**; absent from older agents, read as `0`). The server logs it and uses it to tell an agent that keeps its steps across sessions (1 and up) from one that cancels them on any close (0), and a later server may refuse a revision it no longer supports. It is not yet stored on the agent row. `Welcome` carries the **server's** revision the same way, and the agent reads its absence as "older than `log_batch`" and sends one `log_chunk` per line to that server — so a rolling deploy in either order keeps a build's log, though **upgrade the API before the agents** remains the supported direction. Revision 2 adds batched log messages; see [Log path](#log-path).

## Pools

| Scope | `project_id` | Who they serve |
|---|---|---|
| **Global** | `null` | Any project's queued steps (label match still applies) |
| **Project** | UUID | Only that project's steps |

Create a global agent from **Agents** (`/agents`) — **instance admins only**, because a global agent's token leases steps (and receives secrets) from every project. Create a project agent from `/p/{project}/agents`; that requires **admin** on the project. See [authz](./authz.md#instance-admin).

## Register

```http
POST /api/agents
{ "name": "local", "labels": ["os=linux", "docker=true"], "concurrency": 1 }
```

Project-scoped:

```http
POST /api/agents
{ "name": "team-a", "labels": ["os=linux"], "concurrency": 1, "project_id": "<uuid>" }
```

List (optional filter includes globals + that project's agents):

```http
GET /api/agents
GET /api/agents?project_id=<uuid>
```

Response includes a **plaintext token once**. Store it as `FIBER_AGENT_TOKEN`.

CLI:

```bash
cargo run -p fiber-cli -- agents create --name local --labels os=linux,docker=true
cargo run -p fiber-cli -- agents rotate $AGENT_ID
cargo run -p fiber-cli -- agents list --project-id $PROJECT_ID
```

See [cli.md](./cli.md).

## Install

Three ways to attach a machine, all needing a token from **Register** above.

### systemd (a build host)

```bash
export FIBER_AGENT_TOKEN=…
tag=$(basename "$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
  https://github.com/durablefibers/fiber/releases/latest)")

curl -fsSL "https://raw.githubusercontent.com/durablefibers/fiber/$tag/scripts/install-agent.sh" \
  | sudo --preserve-env=FIBER_AGENT_TOKEN,GH_TOKEN bash -s -- \
      --api-url wss://ci.example.com --labels os=linux
```

Two details that are not decoration:

- **Fetch the script from `$tag`, not from `main`.** This script performs every check
  below; taking the binary from a release and the verifier from a moving branch only
  moves the problem one file along.
- **`sudo --preserve-env=…`, not `sudo VAR=… bash`.** Both keep the token out of the
  installer's argv, but `sudo VAR=… bash` puts it in *sudo's* argv, which is just as
  world-readable in `ps`. Preserving `GH_TOKEN` as well is what lets the provenance
  check run at all under `sudo` — see below.

Downloads the release binary for the host architecture, creates the `fiber` system user,
writes `/etc/fiber/agent.env` (mode `0640`, root-owned so the token is not world-readable),
installs [`deploy/fiber-agent.service`](../deploy/fiber-agent.service), and enables it. Add
`--docker` to allow `image:` steps (adds `fiber` to the `docker` group). `--uninstall`
removes the service, keeping `/etc/fiber` and `/var/lib/fiber`.

#### What it verifies

This is still `curl | sudo bash`, so be clear about what each check is worth:

| | Guarantee | Failure |
|---|---|---|
| **One tag** | `--version latest` is resolved to a concrete tag by following the `/releases/latest` redirect, and the tarball, the checksums and the systemd unit all come from *that* tag. If the unit cannot be obtained for that tag the install aborts, rather than pairing a new binary with the unit already on disk. | fatal |
| **Checksum** | The tarball's SHA-256 must match the release's `SHA256SUMS` (or, on releases published before that file existed, the per-asset `.sha256`). Integrity only — both files come from the same release over the same channel, so it catches a truncated download, not a compromised release. | fatal, unless `--insecure-skip-checksum` |
| **Unit** | The systemd unit has no attestation of its own, so it comes from the release assets **only** when `SHA256SUMS` itself verified as attested; otherwise from git at the same tag. Either way the install aborts rather than reusing the unit already on disk. | fatal, **not** skippable |
| **Provenance** | When `gh` is usable, `gh attestation verify` checks the tarball's build-provenance attestation, pinned to this repository, to `.github/workflows/release.yml`, to `refs/tags/<the tag being installed>` and to a GitHub-hosted runner. Ref-pinning is what stops an attacker serving a genuine, still-validly-attested build of an *older, vulnerable* tag. | fatal **if the check runs and fails**, unless `--insecure-skip-attestation`; see the caveat below if it cannot run |

Two exceptions to "one tag", both announced in the output: running the script from a
checkout takes the unit from that checkout, and a release with no attested `SHA256SUMS`
(or none at all) makes the installer take the unit from `raw.githubusercontent.com` at the
right tag.

That last case is a *stronger* path, not a weaker one, and it is the reason the unit is
gated on an attested manifest. The unit decides which binary runs as which user. Someone
who can edit an already-published release's assets — without any commit, tag or push —
could otherwise leave the genuine tarball untouched, rewrite `SHA256SUMS` to keep the real
tarball hash while substituting the hash of their own unit, and upload that unit. Every
check would pass, including the tarball's provenance. Requiring `SHA256SUMS` to be attested
closes that, and falling back to git closes it again, because editing the repository at a
tag needs write access to the repository.

**The provenance check is the one that quietly does not happen.** It needs `gh` installed
*and authenticated as the user running the script* — and under `sudo`, `env_reset` drops
`GH_TOKEN` and points `HOME` at `/root`, so on a normal host the usual outcome is "gh is
installed but not authenticated". That is a loud warning before anything is installed, not
an error. To make it real, either preserve the token (`sudo --preserve-env=GH_TOKEN`, as in
the command above) or verify as yourself first with `--check-only`. Pass
`--require-attestation` to refuse to install unless provenance actually verified.

Releases up to and including **v0.6.2** carry no attestations at all; install those with
`--insecure-skip-attestation` (the checksum is still enforced).

It finishes by printing the resolved tag, the tarball digest, the digest of the
`fiber-agent` binary it installed, and where the unit came from.

#### Verifying a download by hand

`--check-only` runs the whole resolve-download-verify path and exits without touching the
host. It needs no root — run it **as yourself**, where `gh` is authenticated, rather than
under `sudo` — and it works on macOS as well as Linux:

```bash
tag=$(basename "$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
  https://github.com/durablefibers/fiber/releases/latest)")
curl -fsSL "https://raw.githubusercontent.com/durablefibers/fiber/$tag/scripts/install-agent.sh" \
  | bash -s -- --check-only
```

Its exit code is the answer, so it can gate a deployment script:

| Exit | Meaning |
|---|---|
| `0` | Both the checksum and the provenance verified. |
| `1` | A check ran and failed — mismatched checksum, or an attestation that does not match this repo/workflow/tag. Treat the download as hostile. |
| `2` | A check could not be obtained: no usable `gh`, no published checksum, or an `--insecure-skip-*` flag. Nothing failed; less was proved than you asked for. |

```
latest resolves to vX.Y.Z
downloading https://github.com/durablefibers/fiber/releases/download/vX.Y.Z/fiber-agent-x86_64-unknown-linux-gnu.tar.gz
checksum ok (SHA256SUMS)
provenance ok (gh attestation verify)

release:    vX.Y.Z   (durablefibers/fiber)
asset:      fiber-agent-x86_64-unknown-linux-gnu.tar.gz
  sha256    …
fiber-agent binary
  sha256    …
checksum:   OK — matches SHA256SUMS from release vX.Y.Z (integrity only: same origin as the tarball)
provenance: VERIFIED — built by durablefibers/fiber/.github/workflows/release.yml (GitHub build provenance)
```

Or do it yourself, without the script:

```bash
tag=vX.Y.Z
asset=fiber-agent-x86_64-unknown-linux-gnu.tar.gz
base=https://github.com/durablefibers/fiber/releases/download/$tag
curl -fsSLO "$base/$asset"
curl -fsSLO "$base/SHA256SUMS"

# Check just this asset against the manifest. `sha256sum --ignore-missing` would also
# work on GNU coreutils, but it does not exist on macOS — awk out the one line instead.
awk -v f="$asset" '$2 == f' SHA256SUMS | sha256sum -c -     # macOS: shasum -a 256 -c -

gh attestation verify "$asset" \
  --repo durablefibers/fiber \
  --signer-workflow durablefibers/fiber/.github/workflows/release.yml \
  --source-ref "refs/tags/$tag" \
  --deny-self-hosted-runners
```

All four `gh` flags matter. `--repo` alone accepts an attestation minted by *any* workflow
in the repository; `--signer-workflow` pins which one; `--source-ref` pins the tag, without
which an older attested release passes; `--deny-self-hosted-runners` is what makes the
"GitHub-hosted runner" claim true rather than assumed.

See [operations](./operations.md#images-and-releases) for what a release attests to and
how to verify the container images.

#### Upgrading and rolling back

Re-running it is the upgrade path: settings you do not pass again are read back from
`/etc/fiber/agent.env`, and the service is restarted so the new binary takes effect. The
installer records `FIBER_AGENT_INSTALLED_VERSION` and `FIBER_AGENT_INSTALLED_SHA256` in
that file, so a re-run prints what it is upgrading from (the agent itself ignores both).

The binary it replaces is kept as `/usr/local/bin/fiber-agent.prev` (and `fiber.prev`), so
a bad rollout is one copy away from undone:

```bash
sudo cp -p /usr/local/bin/fiber-agent.prev /usr/local/bin/fiber-agent
sudo systemctl restart fiber-agent
```

It installs from a published release, so it needs one to exist for the host platform
(linux x86_64 / arm64). For an air-gapped host, or before the first release, pass a tarball you
built yourself — `--tarball` skips both the checksum and the provenance check and says so,
because you supplied the bits — `cargo build --release -p fiber-agent -p fiber-cli` then
`tar -C target/release -czf fiber-agent.tar.gz fiber-agent fiber`:

```bash
sudo ./scripts/install-agent.sh --api-url wss://ci.example.com --token … --tarball fiber-agent.tar.gz
```

```bash
journalctl -u fiber-agent -f          # logs
sudo systemctl restart fiber-agent    # after editing /etc/fiber/agent.env
```

The unit sets `RestartPreventExitStatus=2`, so an agent whose token was revoked stops instead of
restart-looping, and `TimeoutStopSec=30` so SIGTERM can stop steps before the kill.

It is also hardened, and the hardening is visible to your builds. `ProtectSystem=full` and
`NoNewPrivileges=true` make `/usr` and `/etc` read-only and stop `sudo` working, so a
pipeline that installs packages system-wide fails here. `CapabilityBoundingSet=` drops every
capability (no `ping`, no binding ports below 1024, and no `newuidmap`/`newgidmap`, so
rootless podman cannot map a uid range). `RestrictAddressFamilies=` allows
`AF_UNIX AF_INET AF_INET6 AF_NETLINK` only, so raw/packet sockets and `AF_ALG` fail.
`UMask=0027` means files a step creates are not world-readable. `ProtectHome=true` hides
`/home`, `/root` and `/run/user/*` — see [configuration](./configuration.md) under
`FIBER_AGENT_ENV_PASSTHROUGH` for what that costs an SSH agent socket.

Two more are worth knowing about before you blame the build:

- **`SystemCallArchitectures=native`** refuses non-native personalities, so 32-bit
  binaries do not run — `cargo test --target i686-*`, wine32, some vendor SDKs. Relax with
  `SystemCallArchitectures=native x86`.
- **`RestrictSUIDSGID=true`** blocks *creating* setuid/setgid files, which `dpkg-deb` and
  `rpmbuild` need when the package being built contains one. Relax with
  `RestrictSUIDSGID=false`.

`MemoryDenyWriteExecute` and `RestrictNamespaces` are deliberately **not** set: the first
breaks every JIT (JVM, Node, .NET), the second breaks `unshare` and rootless container
tooling. The unit file lists the rest of what was considered and rejected, with reasons.

Relax any of it in a drop-in (`/etc/systemd/system/fiber-agent.service.d/`) if your builds
need it — a drop-in survives the next `install-agent.sh` run, edits to the unit do not.

### Container

```bash
docker run -d --restart unless-stopped --name fiber-agent \
  -e FIBER_API_URL=wss://ci.example.com \
  -e FIBER_AGENT_TOKEN=… \
  -e FIBER_AGENT_LABELS=os=linux \
  -v fiber_workspaces:/data/workspaces \
  --init \
  ghcr.io/durablefibers/fiber-agent:latest
```

`--init` reaps step grandchildren that reparent to the agent. Use a named volume as shown:
a host bind mount arrives root-owned and the agent runs as uid 10001, so it could not write
workspaces.

The image runs as a non-root user and ships `git`. `image:` steps additionally need the Docker
CLI and a mounted socket — mounting `/var/run/docker.sock` grants root on the host, so do it only
on hosts where that is acceptable.

Alongside a Compose deployment, the bundled worker starts with a profile:

```bash
echo "FIBER_AGENT_TOKEN=…" >> deploy/.env
docker compose -f deploy/docker-compose.yml --profile agent up -d fiber-agent
```

### From source

```bash
export FIBER_AGENT_TOKEN=…
export FIBER_API_URL=ws://127.0.0.1:18080
export FIBER_AGENT_NAME=local
export FIBER_AGENT_LABELS=os=linux,docker=true
export FIBER_AGENT_CONCURRENCY=1
export FIBER_AGENT_USE_DOCKER=true   # false = host shell
export FIBER_AGENT_WORKSPACE_DIR=./data/workspaces
cargo run -p fiber-agent
# or: cargo run -p fiber-cli -- agent --token "$FIBER_AGENT_TOKEN"
```

## Run

Connects to `/ws/agent?token=…`, sends `Hello`, then heartbeats every **10s**. Pool scope comes from the token's agent row (not from the client). A `concurrency` below 1 is treated as 1 — an agent that is online but can never be offered anything is a misconfiguration nobody would notice.

## Label matching

A step is offered only if:

1. The agent is **global** or bound to the step's **project**, and  
2. **Every** step label appears on the agent (empty step labels match any agent).

Steps are offered **oldest-queued first** (`queued_at`), so a step requeued after a lost lease keeps its place rather than sorting behind every step that has never started. The label match is applied in the query too, so a long run of steps for some other kind of agent cannot push this agent's work past the scan limit.

An offer is **all or nothing**. It is built from the run's snapshot, the project's secrets and the run's artifacts after the lease is taken, and the `step_attempts` row opens only once it is built and about to be sent. If something cannot be read, nothing is sent, and what happens next depends on why:

- **A database error** backs the lease out: the step returns to the queue with no attempt spent and a 30-second `not_before` backoff, so a step whose offer keeps failing is not the very next thing every agent tries and cannot block the queue behind it. The fill pass then moves on to the next candidate (skipping what it already backed out, and stopping after five failures). Logged at `error` with the run and step ids.
- **A secret that cannot be decrypted** (a wrong or rotated `FIBER_SECRETS_KEY`) will not clear on its own, so the step is **failed** through the ordinary completion path with `cannot decrypt project secret NAME (is FIBER_SECRETS_KEY right?)` on the step and its attempt; `retries` apply, dependents skip, and the run finalises. Nothing runs without its secrets.

The alternative was an offer with no workspace, env or restores, which ran the step in an empty directory and spent a retry on it.

The server counts an agent's in-flight steps **from the database** (`step_runs` running under it), not from what it remembers delivering, so a `Cancel` that never reached the agent, or a completion the replica never saw, frees the slot as soon as the row leaves `running`.

## Lifecycle

| Event | Behavior |
|---|---|
| Heartbeat | Touches `last_seen_at`, renews leases, then receives offers until every free slot is filled — a concurrency-4 agent fills in one heartbeat, not four. Retried steps are not offered before their backoff (`not_before`) |
| Step timeout | Every offer carries `timeout_minutes`; the agent kills the process group at the deadline and reports `failed` (`timed out after N min`). The server fails it itself after a grace period if the agent does not |
| SIGTERM / SIGINT | In-flight step processes are stopped **without** reporting a result; once they are gone (≤ 10 s) the agent sends `Goodbye` and closes. `Goodbye` makes the server requeue those steps at once rather than when their leases expire, so a rolling agent restart hands the work to another agent within seconds. The bounced attempt counts against `retries` with one extra try, so a step bounced once never fails — even with `retries: 0` — but a step that loses more than `retries + 1` leases fails. A rolling restart of a whole pool can bounce the same step twice (it is re-leased immediately, with no backoff), which does fail a `retries: 0` step; give such steps `retries: 1` or restart agents one at a time |
| Disconnect / WS close | The session ends; the steps do not. The agent is marked offline, but every step it was running stays `running` under its lease and keeps executing on the agent, its output buffered locally (up to 10 000 lines or 8 MB; past that the oldest log lines go, never a completion or an artifact, and a system line says how many were lost). Nothing is requeued |
| Reconnect | Exponential backoff 1 s → 30 s with jitter; a `401` (revoked token) exits the process with status 2 instead of retrying forever. On reconnect the agent sends `Hello`, the first heartbeat renews every lease it still holds, and the buffered output is flushed in order; the server counts those steps against the agent's concurrency before offering it more. A step that finished while disconnected reports its result now, and it is accepted as long as the row is still `running` under this agent |
| Lease lost | `Welcome` carries `lease_secs` (300, clamped to a day). If no frame has arrived from the server within that minus three heartbeats (**270 s**, counted from the last frame received before the outage — the server pings every 15 s, and a reconnect that fails does not move it), the agent stops its in-flight steps, drops their buffered log lines, and reports each one `failed` with `lease lost while the agent was disconnected` on its own attempt. The server takes that report only if it has not already reclaimed the step, so the row never sits `running` under an agent that has stopped working on it; either way the step retries under its existing budget. A new offer for the same step purges anything still queued from the earlier attempt. Server side, the reclaim loop requeues an expired lease with the same budget as before: it counts against `retries` with one extra try, and past `retries + 1` lost leases the step fails with `lease lost after N attempts`. A completion that arrives after the reclaim is ignored as stale |
| Concurrency | `--concurrency` is enforced locally with a process-wide semaphore as well as by the server; a step parked on it still counts against its `timeout_minutes`, which start when the offer is received |
| Stale | No heartbeat for `FIBER_AGENT_STALE_SECS` (default **45**) → marked offline, and its socket is closed on whichever replica holds it (a `DropSession` command over `fiber:agent_cmds`; a replica older than that command ignores it, and its 45 s server ping closes the socket anyway) so it reconnects. Its leases are not touched; they expire on their own if it never comes back |
| Server silent | The agent ends a session itself after **45 s** without any frame from a server that sends `lease_secs` (such servers ping every 15 s), so a black-holed connection arms the lease watchdog instead of quietly outliving the lease |
| API unreachable mid-step | An artifact upload or restore that fails with a network error or a `5xx` is retried for up to the lease grace (270 s), polling for a session when there is none, before it fails the step |
| Token revoked | The next heartbeat on a rotated or deleted token ends the session with an immediate requeue, whatever the agent declared — decided on the socket's replica, so it does not depend on Redis |
| Server ping | The server pings the socket every 15 s and closes it (code 1008, "liveness timeout") after 45 s without any frame; the close takes the Disconnect path (leases kept). A healthy agent's 10 s heartbeat answers long before |
| Server shutdown | On SIGTERM the API sends Close 1012 ("server shutting down") to every agent and waits for the sessions to end (up to 20 s) before exiting; the agent reconnects with its usual backoff and its in-flight steps follow the Disconnect and Reconnect rows — they keep running |
| Older agent | An agent whose `Hello` has no `protocol_version` cancels its steps on any close, so for it a close is still the end of the attempt: the server requeues its steps at once on disconnect, exactly as before |
| Token rotate | `POST /api/agents/{id}/rotate-token` — new token once; force-disconnect; old session cannot keep leasing |
| Update | `PUT /api/agents/{id}` — name / labels / concurrency (pool unchanged) |
| Delete | `DELETE /api/agents/{id}` — disconnect cleanup then delete |

## Log path

A step's stdout and stderr are read as **bytes**, one line at a time, and decoded lossily:
a Latin-1 filename in an `ls` listing, or any other byte that is not UTF-8, becomes the
Unicode replacement character and the rest of the step's output still arrives. (Before
0.6.2 the reader stopped at the first such byte, the child got SIGPIPE on its next write,
and the step ended at exit 141 with an empty log.) A line longer than **64 KiB** is cut
there and marked with a visible truncation marker; reading resumes at the next newline.
The server applies the same cap again to whatever an agent sends it.

Lines are coalesced into a `log_batch` message and flushed on whichever comes first:
**50 ms**, **64 KB**, or **500 lines**. The server refuses a batch of more than 2 000
lines, keeping the first 2 000 and saying how many it dropped.

`seq` is assigned where the line is read, counts from zero per attempt across `stdout`,
`stderr` and the agent's own `system` lines, and is never renumbered — a batch re-sent
after a reconnect carries the numbers it had the first time.

A step's log is read back in **storage order** (`log_lines.id`), which is also the cursor
`fiber logs --follow` and the UI resume from, and `seq` agrees with it: while a step is
running the agent's own notes travel down the same channel as its piped output, so a
cancel's "killing step process" cannot overtake the output that preceded it. Notes from
before the pipes exist (workspace prep) go straight out and are first by definition. Every
notice inserted after the fact — dropped lines, lines that could not be stored — carries
the `seq` of the gap it reports, so the two orders never disagree.

The server stores a batch in one statement and publishes one event for it, where before
it cost two queries and a Redis publish per line. An agent older than the batch keeps
sending one `log_chunk` per line and the server handles it identically; an agent newer
than the *server* unpacks its batches back into chunks, decided per session from
`Welcome.protocol_version`.

Between the pipes and the batcher is a bounded queue, and the agent waits for room in its
outbox rather than dropping output while a session is up: a step that prints faster than
the control plane can store blocks on its own `write`, which is what stops
`yes | head -n 10000000` from buffering gigabytes. With the socket **down** there is
nothing to wait for and the outbox bound applies instead — 10 000 lines or 8 MB, oldest
first, with a system line saying how many were lost and where. The same notice appears if
the socket is up but has not drained within 30 s. Once a step's process is gone the agent
waits for the buffered output to reach the server — 60 s after an ordinary exit, 10 s
after a cancel or timeout, which have to be reported promptly — and says so if it gives
up with output still queued.

Two caps sit past that. `FIBER_STEP_LOG_MAX_LINES` (default 50 000) bounds the lines
stored per **attempt**, counted across sessions, with one `system` line at the cap saying
so; a retry gets a fresh budget.

`log_lines` is append-only with no unique key, so a message the agent re-sends after a
session ended between the socket accepting it and the agent dropping it from its outbox
is stored twice. Batching does not change how often that happens — at most one message
per interrupted session, as before — but it changes how much of it you see: the duplicate
is now up to 500 lines rather than one. The lines keep their original `seq`, so the
repeat is identifiable.

## Presence in UI

- **online** — DB online and last seen within ~45s  
- **stale** — flagged online but heartbeat is old (until reclaim marks offline)  
- **offline** — disconnected or never seen  

## Executor

- **Shell** (`FIBER_AGENT_USE_DOCKER=false`): runs `run` in the step's workspace directory.
- **Docker**: if `image` is set and docker mode is on, runs inside the image with the workspace mounted, as a named container (`fiber-step-<uuid>`) so cancel and timeout `docker kill` it rather than only the client. The image is checked against the docker reference grammar and passed after `--`, so a pipeline cannot smuggle a `docker run` flag through it.

Process groups: cancel kills the step's process group so grandchildren die too.

Steps and git run with no stdin and `GIT_TERMINAL_PROMPT=0`: anything that would wait on a terminal fails at once instead of at the timeout. Git is limited to the `file`, `git`, `http`, `https`, and `ssh` transports (`GIT_ALLOW_PROTOCOL`, unless the operator set it), and the agent refuses a `workspace.repo` that is not one of those or an scp-like `user@host:path`, so the `ext::` transport — which runs a command on the host — is unreachable however the definition was written. A credential in the remote URL is masked in the log.

## What a step can see

Steps run repo-supplied shell, so the agent narrows what is reachable:

| | Behaviour |
|---|---|
| **Environment** | Cleared, then rebuilt: shell basics (`PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `LANG`, `LANGUAGE`, `LC_ALL`, `LC_CTYPE`, `TZ`, `TERM`, `TMPDIR`), proxy and CA settings (`HTTP(S)_PROXY`, `NO_PROXY`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, `CURL_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`), anything in `FIBER_AGENT_ENV_PASSTHROUGH`, then the offer's env. The agent's own variables — notably `FIBER_AGENT_TOKEN` — are not inherited |
| **Secrets** | Only the project secrets the step asks for (`secrets:` in the pipeline; all of them when omitted). Values are masked as `***` in log lines and in the step's error |
| **Docker env** | Passed with `--env-file` on a `0600` temporary file, never `-e KEY=VALUE`, which would put every secret in the host's process list. The docker client is itself started with a cleared environment, and variable names are validated, so a hostile name cannot make docker copy one of its own variables into the step |
| **Container limits** | `--security-opt no-new-privileges` and `--pids-limit 512` by default, plus `--user`, `--network`, `--memory`, `--cpus` from `FIBER_AGENT_DOCKER_*`. Memory and CPU limits are off unless set; whatever applies is printed as a `system` log line so an exit 137 is diagnosable |
| **Workspace** | One directory per step, deleted when the step finishes — including on cancel, timeout, or failure. The run's tree goes when its last step on this agent finishes; anything older than `FIBER_AGENT_WORKSPACE_TTL_HOURS` (24) is swept at startup |
| **Artifacts** | Only those produced by the steps this one transitively `needs` |
| **Untrusted runs** | A fork's pull request is offered only to agents bound to that project, never to the global pool, and receives no secrets — see [triggers](./triggers.md#pull-requests-from-forks) |

A step with a git workspace is cloned from one reference clone per run, so a second step
costs a local object copy rather than another fetch, and the checkout is self-contained
(`git` works the same inside a container as on the host).

Masking is a substring match on the secret's value, so it does not catch a value the step
transforms first (base64, URL-encoding) or one shorter than 8 characters. Treat it as a
guard against accidental `echo`, not as permission to print secrets.

This is a boundary, not a sandbox: anyone who can write a `fiber.yml` still runs code as
the agent user. Keep agents on hosts you would grant those people.
