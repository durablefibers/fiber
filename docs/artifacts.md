# Artifacts

Artifacts are how files move between steps: every step gets its own workspace, and before
a step runs it receives the artifacts produced by the steps it transitively `needs` —
not everything the run has produced, so a parallel sibling cannot drop files into it.

Steps declare workspace-relative paths under `artifacts:`. After a **successful** step, the agent uploads each path. Later steps in the same run receive a `restore` list in their Offer and download those files into the workspace before `run`.

### Agents that cannot reach object storage

Presigned URLs name the storage endpoint as the outside world reaches it
(`FIBER_S3_PUBLIC_ENDPOINT`), which is right for a browser and for an agent on the same
network. An agent placed behind a boundary — the Compose agent runs on its own network,
deliberately away from Postgres, Redis, and MinIO — cannot use that address.

Such an agent falls back to the API, which it can reach by definition, since that is where
its offers come from. Uploads go through `PUT /api/agent/steps/{id}/artifacts` and restores
through `GET /api/agent/artifacts/{id}/download?via=api`. Both log a `system` line saying
the presigned route was unreachable, so the slower path is visible rather than silent.
Direct transfer is still tried first and still used wherever it works.

### What may be declared

An `artifacts:` entry names one **regular file**, by a plain relative path inside the
workspace:

- **A symlink is refused**, at any component of the path. The repository controls its own
  working tree, so `out/build.log -> ~/.ssh/id_rsa` (or `out -> /`) would otherwise upload
  a file the step never produced into a store every project reader can download.
- **A directory is refused.** It used to be skipped with a note, which left the step green
  and the artifact absent — the dependent step then failed at restore time, or worse, ran
  without it. Archive it in the step instead: `tar czf dist.tgz dist` and declare
  `dist.tgz`.
- **An absolute path, `..`, or anything else that is not a plain relative path is
  refused.** These read outside the workspace entirely.

### Caps

| Limit | Default | Override |
|---|---|---|
| One artifact | 64 MiB | not configurable |
| Artifacts per step | 50 | `FIBER_MAX_ARTIFACTS_PER_STEP` |
| Total bytes per step | 512 MiB | `FIBER_MAX_ARTIFACT_BYTES_PER_STEP` |

The per-step caps are checked before the bytes move (on presign and on the proxy `PUT`)
and again when a direct upload is completed. Re-uploading the *same* name does not count
twice: a step is at-least-once, and its row is replaced rather than added. Over a cap, the
upload is a 400 and the step fails with the server's reason in its log.

The checks before the bytes move are advice — a presigned URL is better refused than
signed. The gate is the row: `create_artifact` re-checks the caps under a per-step lock in
the transaction that inserts it, so two uploads racing for the last slot cannot both see
room and both land. An upload refused there has already stored its bytes, and the object
is deleted the way a rejected `complete` is. The caps hold against a modified agent, not
only the stock one that uploads a step's artifacts one at a time.

### When an upload fails

A declared artifact that exists but could not be stored **fails the step** — an unreadable
file, one over a cap, a symlink, a directory, an unsafe path, or a failed transfer.
Otherwise the step would report success while a later step that `needs` it fails with a
missing file, and the log would blame the wrong step.

A declared path that simply does not exist is a warning, not a failure, so a step may
declare an artifact it only sometimes produces. Nothing downstream can restore it either
way, so check the step's `system` log lines if a `restore` list looks short.

## Local backend (default)

- Files under `FIBER_ARTIFACTS_DIR` (default `./data/artifacts`)
- Agent uploads: `PUT /api/agent/steps/{step_run_id}/artifacts` with header `X-Fiber-Artifact-Path` and raw body (max **64 MiB**)
- Download: user `GET /api/artifacts/{id}/download`; agent `GET /api/agent/artifacts/{id}/download`

## S3 / MinIO

Set `FIBER_S3_BUCKET` (and usually endpoint/credentials). API uses the AWS SDK with path-style addressing.

Agent upload path when S3 is enabled:

1. `POST /api/agent/steps/{id}/artifacts/presign` `{ "path", "size" }` → `{ mode: "presign", upload_url, stored_path }`
2. HTTP `PUT` body to `upload_url` (no extra Content-Type header)
3. `POST /api/agent/steps/{id}/artifacts/complete` `{ "path", "size", "stored_path" }` — API HEADs the object and registers metadata

The presigned URL signs `Content-Length` with the declared size, so it is only usable for
an object of exactly that many bytes — an agent (or anything that gets hold of the URL
inside its ten minutes) cannot write more than the step asked to write. Send the body as
one request with its length set, which every ordinary HTTP client does; a chunked upload
of the same bytes will not match the signature.

If the object store refuses a rejected upload's bytes, or `complete` refuses them (wrong
size, over a cap), the API deletes the object it found: a rejected upload writes no row,
and retention follows rows.

If presign returns `{ mode: "proxy" }`, the agent falls back to the local PUT proxy.

### Objects with no row

An upload that got as far as the object store but never reached `complete` — the agent
died mid-step, the step was reclaimed — leaves an object nothing references. Once an hour
retention lists up to 1 000 objects under `artifacts/` older than
`FIBER_RETENTION_ORPHAN_HOURS` (default 24, `0` disables), asks the database which of them
are still referenced, and deletes the rest. It runs only when run retention is on
(`FIBER_RETENTION_DAYS` above `0`): setting that to zero means this instance deletes
nothing.

Because it deletes, it is deliberately narrow and fails closed:

- Only the `artifacts/{run uuid}/{step uuid}/{name}` layout this process writes is
  considered; symlinks and anything else under the root are ignored.
- "Still referenced" is asked twice — by stored path *and* by object key — and an object
  has to be unreferenced both ways before it goes. The key does not depend on how the
  artifact root is spelled, so a root that changed between boots cannot make live
  artifacts look orphaned. (The local root is also resolved to one absolute path at
  startup, so it cannot change by spelling alone.)
- A failure to answer either question keeps every object for that tick.

**One instance owns everything under `artifacts/` in its bucket or directory.** Two
deployments sharing a bucket will delete each other's objects, because each sees the
other's as having no row. Give them separate buckets, or separate prefixes, before
sharing one.

### Public endpoint

When the API talks to MinIO as `http://fiber-minio:9000` but agents run on the host, set:

```bash
FIBER_S3_ENDPOINT=http://fiber-minio:9000
FIBER_S3_PUBLIC_ENDPOINT=http://127.0.0.1:19000
```

Presign uses a **separate SigV4 client** bound to the public endpoint (host rewrite after signing would invalidate signatures).

### Compose

`deploy/docker-compose.yml` uses the **local filesystem by default** — the `fiber_artifacts` volume — and keeps MinIO
behind a Compose profile. To store artifacts in MinIO instead, set `FIBER_S3_BUCKET=fiber-artifacts` in
`deploy/.env` and bring the stack up with the profile:

```bash
docker compose -f deploy/docker-compose.yml --profile minio up -d
```

Switching backends does not move existing blobs: each artifact row keeps the path it was written with, so
artifacts stored under the other backend stop downloading until you switch back. Host-run API: `make api-s3`
after `make infra-minio`.

### Local MinIO smoke

```bash
make infra-minio
# other terminal:
make api-s3
python3 scripts/smoke_s3_presign.py
```

The API creates the bucket on boot if missing. Downloads are **307** to a presigned GET URL — clients must not forward the session `Authorization` header to MinIO.

## UI

Run page lists artifacts (filtered to the selected step when present) with download. Retention deletes finished runs and their blobs — see [Operations](./operations.md).

## Legacy

WS base64 `Artifact` messages still exist in the proto; prefer HTTP (presign or proxy).
That path is limited to 8 MiB per artifact and is subject to the same per-step caps.
