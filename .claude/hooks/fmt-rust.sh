#!/usr/bin/env bash
# PostToolUse(Edit|Write): keep Rust edits inside the `make check` gate.
# rustfmt.toml pins edition 2024 / max_width 100 — CI runs `cargo fmt --check`.
set -uo pipefail

path="$(jq -r '.tool_input.file_path // ""' 2>/dev/null)"
case "$path" in
  *.rs) ;;
  *) exit 0 ;;
esac
[ -f "$path" ] || exit 0
command -v rustfmt >/dev/null 2>&1 || exit 0

rustfmt --edition 2024 --config-path "${CLAUDE_PROJECT_DIR:-.}/rustfmt.toml" "$path" 2>/dev/null || true
exit 0
