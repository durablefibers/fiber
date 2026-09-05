---
name: fiber-dogfood
description: Running Fiber's end-to-end smoke scripts and triaging their failures — authz, agent pools, artifacts, S3 presign, and the full Compose smoke. Use to verify a change end-to-end, or when a dogfood script fails and you need the cause rather than the symptom.
license: Apache-2.0
compatibility: Requires the durablefibers checkout, Docker, Python 3, and a built fiber-agent for the artifact smokes.
metadata:
  author: durablefibers
  version: "1.0"
---

# Dogfood smokes

These are Fiber's only end-to-end coverage. The Rust tests are pure unit tests and touch no database, so nothing else exercises the API, scheduler, agent, and artifact path together.

## Current stack state

```!
curl -sf -m 1 http://127.0.0.1:18080/ready >/dev/null 2>&1 && echo "api: ready" || echo "api: DOWN (make infra && make api)"
```

## The smokes

| Command | Covers | Preconditions |
|---|---|---|
| `make dogfood-authz` | roles, membership, agent CRUD, token rotate | infra + API |
| `make dogfood-pools` | project-scoped vs global agent pools | infra + API |
| `make dogfood-artifacts` | artifact upload/restore, path filters | infra + API + running agent |
| `make dogfood-s3` | MinIO presign upload/restore/download | `make infra-minio`, `make api-s3`, built agent |
| `make dogfood-compose` | full `up --build` | Docker, ports free |
| `make dogfood` | authz + pools + artifacts | infra + API + agent |

**Pass means the literal string `DOGFOOD_OK` on stdout.** A zero exit without that marker is a failure.

## Triage order

Establish preconditions before blaming code — `make ready`, `docker ps` for `fiber-postgres` and `fiber-redis`, `pgrep -x fiber-agent`. Then map the symptom:

| Symptom | Likely cause | Where to look |
|---|---|---|
| Connection refused / `/ready` fails | Infra, not code | `make infra`, Compose logs |
| 401 | Token or session expired, wrong auth scheme | `auth.rs` |
| 403 | Role gate — script's user lacks the role | `access.rs`, the required `ProjectRole` |
| Step stays `queued` | Label mismatch or no connected agent | `FIBER_AGENT_LABELS` vs the pipeline's `labels`; offer matching in `fiber-scheduler` |
| Step leased, never completes | Agent execution or lease expiry | agent logs, `requeue_expired_leases` |
| Artifact missing | Path rejected, size cap, or backend misconfig | `sanitize_artifact_rel_path`, `MAX_*_BYTES`, `FIBER_USE_S3` |
| Presign 403 from MinIO | Credentials or endpoint | `FIBER_S3_*` in `scripts/dev-env.sh`, MinIO up on 19000 |
| Intermittent | Poll timing | Re-run; query run/step status directly to tell "slow" from "stuck" |

## Cleaning up agents

Kill by PID only:

```bash
pgrep -x fiber-agent
kill <pid>
```

Killing by argv pattern also matches shells whose command line contains the binary path — including the parent of this session. `docs/development.md` documents this and the toolkit's Bash guard hook blocks it.

## Reporting

Per smoke: ran / passed / failed, with `DOGFOOD_OK` quoted or the failing output excerpted. Name one most-likely cause with file references, and say whether it is environmental or a real regression. Never report a smoke as passing if you only started it.
