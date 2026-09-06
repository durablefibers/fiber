#!/usr/bin/env bash
# Sourceable local defaults for host-run fiber-api / fiber-agent / fiber-cli.
# Usage:  source scripts/dev-env.sh
#         make api   # already sources this

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export FIBER_ROOT="$ROOT"

export FIBER_DATABASE_URL="${FIBER_DATABASE_URL:-postgres://fiber:fiber@127.0.0.1:15432/fiber}"
export FIBER_REDIS_URL="${FIBER_REDIS_URL:-redis://:${FIBER_REDIS_PASSWORD:-fiber}@127.0.0.1:16379}"
export FIBER_LISTEN="${FIBER_LISTEN:-0.0.0.0:18080}"
export FIBER_ARTIFACTS_DIR="${FIBER_ARTIFACTS_DIR:-$ROOT/data/artifacts}"
export FIBER_ADMIN_USER="${FIBER_ADMIN_USER:-admin}"
export FIBER_ADMIN_PASSWORD="${FIBER_ADMIN_PASSWORD:-fiber}"
export FIBER_RETENTION_DAYS="${FIBER_RETENTION_DAYS:-30}"
export RUST_LOG="${RUST_LOG:-info,fiber_api=info,fiber_agent=info}"
# Browser origins allowed by the API (default already covers the :3100 dev server).
# export FIBER_CORS_ORIGINS=http://localhost:3100,http://127.0.0.1:3100
# Step timeouts (minutes): default for steps without `timeout_minutes`, and the
# server-side grace before it fails a step the agent did not stop itself.
# export FIBER_STEP_TIMEOUT_DEFAULT_MINUTES=60
# export FIBER_STEP_TIMEOUT_GRACE_MINUTES=5

# Optional secrets encryption (generate once: openssl rand -hex 32)
# export FIBER_SECRETS_KEY=...

# Optional MinIO (make infra-minio first)
if [[ "${FIBER_USE_S3:-}" == "1" || "${FIBER_USE_S3:-}" == "true" ]]; then
  export FIBER_S3_BUCKET="${FIBER_S3_BUCKET:-fiber-artifacts}"
  export FIBER_S3_ENDPOINT="${FIBER_S3_ENDPOINT:-http://127.0.0.1:19000}"
  export FIBER_S3_ACCESS_KEY="${FIBER_S3_ACCESS_KEY:-fiber}"
  export FIBER_S3_SECRET_KEY="${FIBER_S3_SECRET_KEY:-fiberfiber}"
  export FIBER_S3_REGION="${FIBER_S3_REGION:-us-east-1}"
fi

export FIBER_API_URL="${FIBER_API_URL:-http://127.0.0.1:18080}"
export FIBER_AGENT_WORKSPACE_DIR="${FIBER_AGENT_WORKSPACE_DIR:-$ROOT/data/workspaces}"
export FIBER_AGENT_USE_DOCKER="${FIBER_AGENT_USE_DOCKER:-false}"
export FIBER_AGENT_NAME="${FIBER_AGENT_NAME:-local}"
export FIBER_AGENT_LABELS="${FIBER_AGENT_LABELS:-os=linux}"
export FIBER_AGENT_CONCURRENCY="${FIBER_AGENT_CONCURRENCY:-1}"
export VITE_FIBER_API_URL="${VITE_FIBER_API_URL:-http://127.0.0.1:18080}"

mkdir -p "$FIBER_ARTIFACTS_DIR" "$FIBER_AGENT_WORKSPACE_DIR"
