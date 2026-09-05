#!/usr/bin/env bash
# Self-test for the fiber toolkit hooks. Run: bash .claude/hooks/selftest.sh
# Verifies each guard denies what it should and, just as importantly, allows
# the everyday commands it must not get in the way of.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
HOOKS=".claude/hooks"
pass=0; fail=0

payload() { printf '{"tool_input":{"command":%s}}' "$(jq -Rs . <<<"$1")"; }
path_payload() { printf '{"tool_input":{"file_path":%s}}' "$(jq -Rs . <<<"$1")"; }

# expect <deny|allow> <label> <hook> <payload-json>
expect() {
  local want="$1" label="$2" hook="$3" body="$4" out rc
  out="$(printf '%s' "$body" | "$hook" 2>&1)"; rc=$?
  local got="allow"; [ "$rc" -eq 2 ] && got="deny"
  if [ "$got" = "$want" ]; then
    printf '  ok   %-52s (%s)\n' "$label" "$got"; pass=$((pass+1))
  else
    printf '  FAIL %-52s want=%s got=%s rc=%s\n' "$label" "$want" "$got" "$rc"
    [ -n "$out" ] && printf '       %s\n' "$(head -1 <<<"$out")"
    fail=$((fail+1))
  fi
}

echo "guard-bash.sh"
expect deny  "pkill -f fiber-agent"            "$HOOKS/guard-bash.sh" "$(payload 'pkill -f fiber-agent')"
expect deny  "killall -f fiber-agent"          "$HOOKS/guard-bash.sh" "$(payload 'killall -f fiber-agent')"
expect deny  "chained pkill after &&"          "$HOOKS/guard-bash.sh" "$(payload 'make down && pkill -f fiber-agent')"
expect deny  "compose down --volumes"          "$HOOKS/guard-bash.sh" "$(payload 'docker compose -f deploy/docker-compose.yml down -v')"
expect deny  "append to applied migration"     "$HOOKS/guard-bash.sh" "$(payload 'echo "ALTER TABLE runs ADD COLUMN x int;" >> crates/fiber-core/migrations/001_initial.sql')"
expect deny  "sed -i on applied migration"     "$HOOKS/guard-bash.sh" "$(payload 'sed -i "" s/a/b/ crates/fiber-core/migrations/003_retention.sql')"
expect deny  "rm -rf data/workspaces"          "$HOOKS/guard-bash.sh" "$(payload 'rm -rf data/workspaces')"
expect allow "pgrep -x fiber-agent"            "$HOOKS/guard-bash.sh" "$(payload 'pgrep -x fiber-agent')"
expect allow "make down"                       "$HOOKS/guard-bash.sh" "$(payload 'make down')"
expect allow "compose down (no volumes)"       "$HOOKS/guard-bash.sh" "$(payload 'docker compose -f deploy/docker-compose.yml down')"
expect allow "new numbered migration"          "$HOOKS/guard-bash.sh" "$(payload 'echo "CREATE TABLE t();" > crates/fiber-core/migrations/005_new.sql')"
expect allow "rm one run workspace"            "$HOOKS/guard-bash.sh" "$(payload 'rm -rf data/workspaces/86d1af38-6695-4658-b5d8-d2ca54334670')"
expect allow "heredoc mentioning pkill"        "$HOOKS/guard-bash.sh" "$(payload "$(printf "cat > d.md <<'MD'\nNever pkill -f fiber-agent.\nMD")")"
expect allow "grep for the string"             "$HOOKS/guard-bash.sh" "$(payload 'grep -rn "pkill -f fiber-agent" docs/')"
expect allow "make check"                      "$HOOKS/guard-bash.sh" "$(payload 'make check')"

echo "guard-edits.sh"
expect deny  "edit committed migration"        "$HOOKS/guard-edits.sh" "$(path_payload "$PWD/crates/fiber-core/migrations/001_initial.sql")"
expect deny  "edit routeTree.gen.ts"           "$HOOKS/guard-edits.sh" "$(path_payload "$PWD/apps/web/src/routeTree.gen.ts")"
expect deny  "edit data/ runtime state"        "$HOOKS/guard-edits.sh" "$(path_payload "$PWD/data/artifacts/x/y/out.txt")"
expect deny  "edit Cargo.lock"                 "$HOOKS/guard-edits.sh" "$(path_payload "$PWD/Cargo.lock")"
expect allow "new migration file"              "$HOOKS/guard-edits.sh" "$(path_payload "$PWD/crates/fiber-core/migrations/005_new.sql")"
expect allow "edit store.rs"                   "$HOOKS/guard-edits.sh" "$(path_payload "$PWD/crates/fiber-core/src/store.rs")"
expect allow "edit a web route"                "$HOOKS/guard-edits.sh" "$(path_payload "$PWD/apps/web/src/routes/index.tsx")"

echo "naming-guard.sh"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
printf 'let x = std::env::var("DF_DATABASE_URL");\n' > "$tmp/bad.rs"
printf 'let x = std::env::var("FIBER_DATABASE_URL");\n' > "$tmp/good.rs"
expect deny  "DF_ env var prefix"              "$HOOKS/naming-guard.sh" "$(path_payload "$tmp/bad.rs")"
expect allow "FIBER_ env var prefix"           "$HOOKS/naming-guard.sh" "$(path_payload "$tmp/good.rs")"

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ] || exit 1
