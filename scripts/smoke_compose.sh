#!/usr/bin/env bash
# Smoke: full Compose stack (postgres, redis, minio, api, ui).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMPOSE=(docker compose -f "$ROOT/deploy/docker-compose.yml")
FAILS=0

ok() { echo "OK  $*"; }
fail() { echo "FAIL $*"; FAILS=$((FAILS + 1)); }

# The deployment profile expects deploy/.env; supply throwaway values when absent.
# The four credentials are required by deploy/docker-compose.yml (no working defaults),
# so without these exports `docker compose` refuses to interpolate the file at all.
# FIBER_ADMIN_PASSWORD has to stay `fiber`: _compose_pipeline.py logs in with it.
export FIBER_SECRETS_KEY="${FIBER_SECRETS_KEY:-$(openssl rand -hex 32)}"
export FIBER_POSTGRES_PASSWORD="${FIBER_POSTGRES_PASSWORD:-fiber}"
export FIBER_REDIS_PASSWORD="${FIBER_REDIS_PASSWORD:-fiber}"
export FIBER_S3_ACCESS_KEY="${FIBER_S3_ACCESS_KEY:-fiber}"
export FIBER_S3_SECRET_KEY="${FIBER_S3_SECRET_KEY:-fiberfiber}"
export FIBER_ADMIN_PASSWORD="${FIBER_ADMIN_PASSWORD:-fiber}"
# Artifacts default to the local filesystem now that MinIO is optional; this smoke keeps
# covering the S3 path, so it opts in and starts fiber-minio (naming a service enables
# its profile).
export FIBER_S3_BUCKET="${FIBER_S3_BUCKET:-fiber-artifacts}"

echo "== compose build + up =="
# Stop legacy Compose project that may own 15432/16379
docker compose -p deploy -f "$ROOT/deploy/docker-compose.yml" stop fiber-postgres fiber-redis 2>/dev/null || true
pkill -x fiber-api 2>/dev/null || true
"${COMPOSE[@]}" up -d --build fiber-postgres fiber-redis fiber-minio
"${COMPOSE[@]}" up -d --build fiber-api fiber-ui

echo "== wait ready =="
READY=0
for i in $(seq 1 90); do
  if curl -sf http://127.0.0.1:18080/ready >/tmp/fiber-compose-ready.json 2>/dev/null; then
    if python3 -c 'import json; d=json.load(open("/tmp/fiber-compose-ready.json")); raise SystemExit(0 if d.get("ok") else 1)'; then
      ok "api /ready"
      READY=1
      break
    fi
  fi
  sleep 2
done
if [[ $READY -eq 0 ]]; then
  fail "api /ready timeout"
  "${COMPOSE[@]}" logs --tail=80 fiber-api || true
fi

if curl -sf -o /dev/null http://127.0.0.1:18080/health; then
  ok "api /health"
else
  fail "api /health"
fi

# Compose starts fiber-ui only once fiber-api is healthy, so this check runs the
# instant nginx's container appears — before it has bound the port. The API check above
# retries and this one did not, which on a cold runner lost the race by ~50ms.
CODE=000
for i in $(seq 1 30); do
  CODE=$(curl -sf -o /dev/null -w "%{http_code}" http://127.0.0.1:3100/ || true)
  if [[ "$CODE" == "200" ]]; then
    break
  fi
  sleep 2
done
if [[ "$CODE" == "200" ]]; then
  ok "ui :3100 HTTP $CODE"
else
  fail "ui :3100 HTTP $CODE"
  "${COMPOSE[@]}" logs --tail=40 fiber-ui || true
fi

LOGIN=$(curl -sf -X POST http://127.0.0.1:18080/api/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"username":"admin","password":"fiber"}' || true)
if echo "$LOGIN" | python3 -c 'import json,sys; d=json.load(sys.stdin); raise SystemExit(0 if "token" in d else 1)' 2>/dev/null; then
  ok "login admin/fiber"
else
  fail "login: $LOGIN"
fi

if curl -sf http://127.0.0.1:19000/minio/health/live >/dev/null; then
  ok "minio health"
else
  fail "minio health"
fi

# A stack that cannot run a pipeline is not a working stack: mint an agent token,
# start the containerised worker, and drive a real run end to end.
echo "== pipeline through the compose agent =="
AGENT_TOKEN=$(python3 "$ROOT/scripts/_compose_pipeline.py" create-agent || true)
if [[ -z "$AGENT_TOKEN" ]]; then
  fail "create compose agent"
else
  ok "created compose agent"
  export FIBER_AGENT_TOKEN="$AGENT_TOKEN"
  "${COMPOSE[@]}" --profile agent up -d --build fiber-agent
  if python3 "$ROOT/scripts/_compose_pipeline.py" run-pipeline; then
    ok "pipeline ran to success on the compose agent"
  else
    fail "pipeline on compose agent"
    "${COMPOSE[@]}" logs --tail=40 fiber-agent || true
  fi

  # A deploy must not restart the builds in flight: a step's lease outlives the agent's
  # session. Restart the API under a running step and expect it to finish on attempt 1,
  # with every line it printed while the API was away.
  echo "== api restart under a running step =="
  RESTART_RUN=$(python3 "$ROOT/scripts/_compose_pipeline.py" restart-run-start || true)
  if [[ -z "$RESTART_RUN" ]]; then
    fail "start a step to restart the api under"
  else
    "${COMPOSE[@]}" restart fiber-api
    for i in $(seq 1 60); do
      if curl -sf http://127.0.0.1:18080/ready >/dev/null 2>&1; then
        break
      fi
      sleep 1
    done
    if python3 "$ROOT/scripts/_compose_pipeline.py" restart-run-verify "$RESTART_RUN"; then
      ok "step survived the api restart on attempt 1"
    else
      fail "step did not survive the api restart"
      "${COMPOSE[@]}" logs --tail=60 fiber-api || true
      "${COMPOSE[@]}" logs --tail=40 fiber-agent || true
    fi
  fi
  "${COMPOSE[@]}" --profile agent rm -sf fiber-agent >/dev/null 2>&1 || true
  python3 "$ROOT/scripts/_compose_pipeline.py" cleanup >/dev/null 2>&1 || true
fi

echo "---"
if [[ $FAILS -gt 0 ]]; then
  echo "SMOKE_FAIL compose failures=$FAILS"
  exit 1
fi
echo "SMOKE_OK compose"
exit 0
