#!/usr/bin/env bash
# Smoke: full Compose stack (postgres, redis, minio, api, web).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMPOSE=(docker compose -f "$ROOT/deploy/docker-compose.yml")
FAILS=0

ok() { echo "OK  $*"; }
fail() { echo "FAIL $*"; FAILS=$((FAILS + 1)); }

# The deployment profile expects deploy/.env; supply throwaway values when absent.
export FIBER_SECRETS_KEY="${FIBER_SECRETS_KEY:-$(openssl rand -hex 32)}"
export FIBER_REDIS_PASSWORD="${FIBER_REDIS_PASSWORD:-fiber}"

echo "== compose build + up =="
# Stop legacy Compose project that may own 15432/16379
docker compose -p deploy -f "$ROOT/deploy/docker-compose.yml" stop fiber-postgres fiber-redis 2>/dev/null || true
pkill -x fiber-api 2>/dev/null || true
"${COMPOSE[@]}" up -d --build fiber-postgres fiber-redis fiber-minio
"${COMPOSE[@]}" up -d --build fiber-api fiber-web

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

CODE=$(curl -sf -o /dev/null -w "%{http_code}" http://127.0.0.1:3100/ || true)
if [[ "$CODE" == "200" ]]; then
  ok "web :3100 HTTP $CODE"
else
  fail "web :3100 HTTP $CODE"
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

echo "---"
if [[ $FAILS -gt 0 ]]; then
  echo "DOGFOOD_FAIL compose failures=$FAILS"
  exit 1
fi
echo "DOGFOOD_OK compose"
exit 0
