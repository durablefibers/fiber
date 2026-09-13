#!/usr/bin/env bash
# PostToolUse(Edit|Write): Biome (not Prettier/ESLint) owns apps/ui formatting.
set -uo pipefail

path="$(jq -r '.tool_input.file_path // ""' 2>/dev/null)"
case "$path" in
  */apps/ui/*.ts|*/apps/ui/*.tsx|*/apps/ui/*.json|*/apps/ui/*.css) ;;
  *) exit 0 ;;
esac
case "$path" in
  */routeTree.gen.ts|*/dist/*|*/node_modules/*) exit 0 ;;
esac
[ -f "$path" ] || exit 0

ui="${CLAUDE_PROJECT_DIR:-.}/apps/ui"
[ -x "$ui/node_modules/.bin/biome" ] || exit 0
(cd "$ui" && ./node_modules/.bin/biome check --write "$path" >/dev/null 2>&1) || true
exit 0
