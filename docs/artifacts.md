# Artifacts

Artifacts are how files move between steps: every step gets its own workspace, and before
a step runs it receives the artifacts produced by the steps it transitively `needs` —
not everything the run has produced, so a parallel sibling cannot drop files into it.

Steps declare workspace-relative paths under `artifacts:`. After a **successful** step, the agent uploads each path. Later steps in the same run receive a `restore` list in their Offer and download those files into the workspace before `run`.

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

If presign returns `{ mode: "proxy" }`, the agent falls back to the local PUT proxy.

### Public endpoint

When the API talks to MinIO as `http://fiber-minio:9000` but agents run on the host, set:

```bash
FIBER_S3_ENDPOINT=http://fiber-minio:9000
FIBER_S3_PUBLIC_ENDPOINT=http://127.0.0.1:19000
```

Presign uses a **separate SigV4 client** bound to the public endpoint (host rewrite after signing would invalidate signatures).

### Compose

`deploy/docker-compose.yml` enables MinIO for `fiber-api` by default (`FIBER_S3_*` + `depends_on: fiber-minio`). Host-run API: `make api-s3` after `make infra-minio`.

### Local MinIO dogfood

```bash
make infra-minio
# other terminal:
make api-s3
python3 scripts/dogfood_s3_presign.py
```

The API creates the bucket on boot if missing. Downloads are **307** to a presigned GET URL — clients must not forward the session `Authorization` header to MinIO.

## UI

Run page lists artifacts (filtered to the selected step when present) with download. Retention deletes finished runs and their blobs — see [Operations](./operations.md).

## Legacy

WS base64 `Artifact` messages still exist in the proto; prefer HTTP (presign or proxy).
