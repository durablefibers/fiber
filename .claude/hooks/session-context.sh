#!/usr/bin/env bash
# SessionStart: inject live stack state so Claude does not guess whether infra is up.
set -uo pipefail
cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

api="down"; curl -sf -m 1 http://127.0.0.1:18080/ready >/dev/null 2>&1 && api="up (:18080)"
web="down"; curl -sf -m 1 -o /dev/null http://127.0.0.1:3100 2>/dev/null && web="up (:3100)"

pg="down"; redis="down"
if command -v docker >/dev/null 2>&1; then
  running="$(docker ps --format '{{.Names}}' 2>/dev/null || true)"
  printf '%s' "$running" | grep -q fiber-postgres && pg="up (:15432)"
  printf '%s' "$running" | grep -q fiber-redis && redis="up (:16379)"
fi

agents="$(pgrep -x fiber-agent 2>/dev/null | wc -l | tr -d ' ')"
mig="$(ls crates/fiber-core/migrations/*.sql 2>/dev/null | tail -1 | xargs -r basename)"

echo "Fiber stack: api=$api web=$web postgres=$pg redis=$redis fiber-agent processes=$agents"
echo "Latest migration: ${mig:-none}. Gate before finishing: make check (cargo fmt --check + clippy -D warnings)."
[ "$pg" = "down" ] && echo "Postgres is down — 'make infra' before running the API, dogfood smokes, or anything DB-backed."
exit 0
