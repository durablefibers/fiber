#!/usr/bin/env bash
# PostToolUse(Edit|Write): enforce the fiber-* / FIBER_* product prefix.
# .claude/rules/naming.md — the repo folder is `durablefibers`, the code never is.
# The GitHub organisation is also `durablefibers`, so its URLs and image paths
# (ghcr.io/durablefibers/fiber-api, durablefibers/fiber) are not violations.
# Advisory: exits 2 so Claude sees the feedback and fixes it, without reverting the edit.
set -uo pipefail

path="$(jq -r '.tool_input.file_path // ""' 2>/dev/null)"
case "$path" in
  *.rs|*.ts|*.tsx|*.toml|*.yml|*.yaml|*.sh|*.sql) ;;
  *) exit 0 ;;
esac
[ -f "$path" ] || exit 0
case "$path" in
  */.claude/*|*/docs/*|*/README.md|*/target/*|*/node_modules/*) exit 0 ;;
esac

hits="$(grep -nE '\b(durablefibers|durable_fibers)\b|\bDF_[A-Z_]+|\bdf-(api|core|agent|cli|web|scheduler|durable|proto)\b' "$path" 2>/dev/null | grep -v 'Projects/durablefibers' \
  | grep -vE '(ghcr\.io|github\.com|raw\.githubusercontent\.com)/durablefibers/|\bdurablefibers/fiber\b' | head -5)"
[ -z "$hits" ] && exit 0

{
  echo "Naming violation in $path — product prefix is 'fiber' / 'FIBER_*', never df / durablefibers (.claude/rules/naming.md):"
  echo "$hits"
  echo "Rename to fiber-* (crates, binaries, Docker services) or FIBER_* (env vars) and re-apply."
} >&2
exit 2
