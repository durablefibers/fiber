#!/usr/bin/env bash
# PreToolUse(Bash): block commands that are known-hazardous in this repo.
# Exit 2 = deny, reason on stderr goes back to Claude as feedback.
set -uo pipefail

input="$(cat)"
cmd="$(printf '%s' "$input" | jq -r '.tool_input.command // ""' 2>/dev/null)"
[ -z "$cmd" ] && exit 0

# Strip heredoc bodies before matching: writing a file that *documents* a
# hazardous command is not running it. Without this, `cat > doc.md <<'EOF'`
# containing the word pkill would be denied.
scan="$(printf '%s\n' "$cmd" | awk '
  {
    if (inhere) { if ($0 == marker || $0 == marker";") { inhere = 0 }; next }
    line = $0
    if (match(line, /<<-?[[:space:]]*[\047"]?[A-Za-z_][A-Za-z0-9_]*[\047"]?/)) {
      m = substr(line, RSTART, RLENGTH)
      gsub(/^<<-?[[:space:]]*/, "", m); gsub(/[\047"]/, "", m)
      marker = m; inhere = 1
    }
    print line
  }')"

deny() { printf 'Blocked by fiber toolkit: %s\n' "$1" >&2; exit 2; }

# Match only where the binary is invoked as a command word (start of line, or
# after a pipe/separator), not merely mentioned inside a longer string.
cmdword() { printf '%s' "$scan" | grep -Eq "(^|[;&|]|&&|\|\||\\bthen |\\bdo )[[:space:]]*(sudo[[:space:]]+)?$1"; }

# 1. pkill -f fiber-agent matches parent shells whose argv mentions the binary path.
if cmdword '(pkill|killall)[^;&|]*fiber-agent'; then
  deny "that kill pattern also matches shells whose argv contains the binary path, and can kill this session's parent.
Kill by PID instead:
  pgrep -x fiber-agent
  ps -o pid=,command= -ax | grep '[t]arget/debug/fiber-agent'
  kill <pid>
See docs/development.md 'Agent tips' and .claude/rules/dx.md."
fi

# 2. Destroying the Postgres volume wipes every run, artifact row, and the admin user.
if printf '%s' "$scan" | grep -Eq 'docker[[:space:]]+compose[^;&|]*[[:space:]]down([[:space:]]|$)[^;&|]*(-v|--volumes)'; then
  deny "'compose down -v' destroys the fiber-postgres volume (all runs, artifacts, users, secrets).
Use 'make down' to stop containers and keep data.
If you truly intend to wipe (e.g. the documented Postgres 16->17 upgrade), ask the user to run it themselves."
fi

# 3. sqlx migrations are applied on fiber-api boot and checksummed; rewriting an
#    applied file diverges from _sqlx_migrations on every existing database.
#    Only already-committed migrations are protected — writing a NEW numbered
#    file is the correct way to change the schema and must stay unobstructed.
targets="$(printf '%s' "$scan" \
  | grep -Eo 'crates/fiber-core/migrations/[0-9]{3}_[A-Za-z0-9_.-]+\.sql' | sort -u)"
if [ -n "$targets" ] && printf '%s' "$scan" | grep -Eq '(>>?|sed -i|tee|truncate)[^;&|]*crates/fiber-core/migrations/'; then
  while IFS= read -r f; do
    [ -z "$f" ] && continue
    if git -C "${CLAUDE_PROJECT_DIR:-.}" ls-files --error-unmatch "$f" >/dev/null 2>&1; then
      deny "$f is an applied sqlx migration.
Migrations run at fiber-api boot and are checksummed by sqlx; editing one breaks every database that already ran it.
Add the next numbered migration file instead."
    fi
  done <<EOF
$targets
EOF
fi

# 4. Truncating the workspace/artifact roots out from under a running agent.
if printf '%s' "$scan" | grep -Eq 'rm[[:space:]]+(-[a-zA-Z]+[[:space:]]+)*[^;&|]*(data/workspaces|data/artifacts)[[:space:]]*($|[;&|])'; then
  deny "refusing to delete data/workspaces or data/artifacts wholesale — a running fiber-agent holds leases on them.
Delete a specific run's subdirectory, or stop agents first."
fi

exit 0
