#!/usr/bin/env bash
# SessionStart: inject live stack state so Claude does not guess whether infra is up.
set -uo pipefail
cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

api="down"; curl -sf -m 1 http://127.0.0.1:18080/ready >/dev/null 2>&1 && api="up (:18080)"
ui="down"; curl -sf -m 1 -o /dev/null http://127.0.0.1:3100 2>/dev/null && ui="up (:3100)"

# Probe the ports, not a container engine: this works the same under Docker, Podman or a
# host install, and a refused localhost connect fails at once. Asking `docker ps` saw
# nothing under Podman, and blocked for minutes while Docker Desktop was hung.
listening() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }
pg="down"; listening 15432 && pg="up (:15432)"
redis="down"; listening 16379 && redis="up (:16379)"

# Host processes only; an agent running in a container does not show up here.
agents="$(pgrep -x fiber-agent 2>/dev/null | wc -l | tr -d ' ')"
mig="$(ls crates/fiber-core/migrations/*.sql 2>/dev/null | tail -1 | xargs -r basename)"

echo "Fiber stack: api=$api ui=$ui postgres=$pg redis=$redis host fiber-agent processes=$agents"
echo "Latest migration: ${mig:-none}. Gate before finishing: make check (cargo fmt --check + clippy -D warnings)."
[ "$pg" = "down" ] && echo "Postgres is down — 'make infra' before running the API, smoke scripts, or anything DB-backed."
exit 0
