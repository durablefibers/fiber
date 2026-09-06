# Agents

Agents are outbound WebSocket workers that execute CI steps.

An agent's identity is the token it connected with. The server binds `agent_id` from that token and ignores the `agent_id` carried in messages (a mismatch is logged). Log lines are accepted only for a step **last leased to that agent** (so the tail of a cancelled or reclaimed step is still recorded); artifacts and completions additionally require the lease to be **live** — anything after a reclaim or cancel is dropped, which is the at-least-once contract: the re-leased attempt reports its own result. Artifact restore downloads are limited to runs in which the agent holds a running step. A socket that never sends `Hello` is offered nothing, and pool scope always comes from the agent's database row, never from the connection.

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
curl -fsSL https://raw.githubusercontent.com/durablefibers/fiber/main/scripts/install-agent.sh \
  | sudo FIBER_AGENT_TOKEN="$FIBER_AGENT_TOKEN" bash -s -- \
      --api-url wss://ci.example.com --labels os=linux
```

Pass the token in the environment rather than as `--token`: argv is world-readable in
`ps` for as long as the script runs.

Downloads the release binary for the host architecture (verifying its checksum), creates the
`fiber` system user, checks the tarball against its published `.sha256` (integrity, not
provenance — both come from the same release), writes `/etc/fiber/agent.env` (mode `0640`,
root-owned so the token is not world-readable), installs [`deploy/fiber-agent.service`](../deploy/fiber-agent.service), and
enables it. Add `--docker` to allow `image:` steps (adds `fiber` to the `docker` group).
`--uninstall` removes the service, keeping `/etc/fiber` and `/var/lib/fiber`.

Re-running it is the upgrade path: settings you do not pass again are read back from
`/etc/fiber/agent.env`, and the service is restarted so the new binary takes effect.

It installs from a published release, so it needs one to exist for the host platform
(linux x86_64 / arm64). For an air-gapped host, or before the first release, pass a tarball you
built yourself — `cargo build --release -p fiber-agent -p fiber-cli` then
`tar -C target/release -czf fiber-agent.tar.gz fiber-agent fiber`:

```bash
sudo ./scripts/install-agent.sh --api-url wss://ci.example.com --token … --tarball fiber-agent.tar.gz
```

```bash
journalctl -u fiber-agent -f          # logs
sudo systemctl restart fiber-agent    # after editing /etc/fiber/agent.env
```

The unit sets `RestartPreventExitStatus=2`, so an agent whose token was revoked stops instead of
restart-looping, and `TimeoutStopSec=30` so SIGTERM can stop steps before the kill. It also runs
with `ProtectSystem=full` and `NoNewPrivileges=true`: `/usr` and `/etc` are read-only to steps and
`sudo` does not work, so a pipeline that expects to install packages system-wide will fail here.
Relax those in a drop-in if your builds need it.

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

Connects to `/ws/agent?token=…`, sends `Hello`, then heartbeats every **10s**. Pool scope comes from the token's agent row (not from the client).

## Label matching

A step is offered only if:

1. The agent is **global** or bound to the step's **project**, and  
2. **Every** step label appears on the agent (empty step labels match any agent).

## Lifecycle

| Event | Behavior |
|---|---|
| Heartbeat | Touches `last_seen_at`, renews leases, may receive new offers. Retried steps are not offered before their backoff (`not_before`) |
| Step timeout | Every offer carries `timeout_minutes`; the agent kills the process group at the deadline and reports `failed` (`timed out after N min`). The server fails it itself after a grace period if the agent does not |
| SIGTERM / SIGINT | In-flight step processes are stopped and the socket is closed **without** reporting a result, so the server requeues those steps to another agent (a rolling agent restart does not fail a build). The agent exits once its steps are stopped (≤ 10 s) |
| Reconnect | Exponential backoff 1 s → 30 s with jitter; a `401` (revoked token) exits the process with status 2 instead of retrying forever |
| Concurrency | `--concurrency` is enforced locally with a process-wide semaphore as well as by the server; a step parked on it still counts against its `timeout_minutes`, which start when the offer is received |
| Disconnect / WS close | Agent marked offline; in-flight steps requeued (the bounced attempt counts against `retries`) |
| Stale | No heartbeat for `FIBER_AGENT_STALE_SECS` (default **45**) → offline + requeue |
| Token rotate | `POST /api/agents/{id}/rotate-token` — new token once; force-disconnect; old session cannot keep leasing |
| Update | `PUT /api/agents/{id}` — name / labels / concurrency (inflight preserved; pool unchanged) |
| Delete | `DELETE /api/agents/{id}` — disconnect cleanup then delete |

## Presence in UI

- **online** — DB online and last seen within ~45s  
- **stale** — flagged online but heartbeat is old (until reclaim marks offline)  
- **offline** — disconnected or never seen  

## Executor

- **Shell** (`FIBER_AGENT_USE_DOCKER=false`): runs `run` in the step's workspace directory.
- **Docker**: if `image` is set (or docker mode on), runs inside the image with the workspace mounted, as a named container (`fiber-step-<uuid>`) so cancel and timeout `docker kill` it rather than only the client.

Process groups: cancel kills the step's process group so grandchildren die too.

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
