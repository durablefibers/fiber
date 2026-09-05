#!/usr/bin/env bash
# PostToolUse(Edit|Write): Biome (not Prettier/ESLint) owns apps/web formatting.
set -uo pipefail

path="$(jq -r '.tool_input.file_path // ""' 2>/dev/null)"
case "$path" in
  */apps/web/*.ts|*/apps/web/*.tsx|*/apps/web/*.json|*/apps/web/*.css) ;;
  *) exit 0 ;;
esac
case "$path" in
  */routeTree.gen.ts|*/dist/*|*/node_modules/*) exit 0 ;;
esac
[ -f "$path" ] || exit 0

web="${CLAUDE_PROJECT_DIR:-.}/apps/web"
[ -x "$web/node_modules/.bin/biome" ] || exit 0
(cd "$web" && ./node_modules/.bin/biome check --write "$path" >/dev/null 2>&1) || true
exit 0
