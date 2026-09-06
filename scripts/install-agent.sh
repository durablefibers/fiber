#!/usr/bin/env bash
# Install fiber-agent as a systemd service.
#
#   curl -fsSL https://raw.githubusercontent.com/durablefibers/fiber/main/scripts/install-agent.sh \
#     | sudo bash -s -- --api-url wss://ci.example.com --token "$FIBER_AGENT_TOKEN"
#
# Create the token first (instance admin):
#   fiber agents create --name build-01 --labels os=linux
set -euo pipefail

REPO="${FIBER_REPO:-durablefibers/fiber}"
VERSION="${FIBER_VERSION:-latest}"
API_URL=""
TOKEN="${FIBER_AGENT_TOKEN:-}"
NAME="$(hostname -s 2>/dev/null || hostname 2>/dev/null || echo fiber-agent)"
LABELS="os=linux"
CONCURRENCY="2"
USE_DOCKER="false"
BIN_DIR="/usr/local/bin"
ETC_DIR="/etc/fiber"
STATE_DIR="/var/lib/fiber"
SERVICE="/etc/systemd/system/fiber-agent.service"

usage() {
  cat <<'USAGE'
Usage: install-agent.sh --api-url <url> --token <token> [options]

  --api-url URL      Fiber API (ws://, wss://, http:// or https://)   [required]
  --token TOKEN      Agent token from `fiber agents create`           [required]
  --name NAME        Agent display name                     [default: hostname]
  --labels LIST      Comma-separated labels                 [default: os=linux]
  --concurrency N    Max parallel steps                             [default: 2]
  --docker           Enable Docker steps (needs docker + group membership)
  --version VER      Release tag to install                   [default: latest]
  --tarball PATH     Install from a local release tarball instead of downloading
  --uninstall        Stop, disable, and remove the service

The token is read from $FIBER_AGENT_TOKEN when --token is omitted; prefer that, since
argv is world-readable in ps for the life of the script.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --api-url) API_URL="$2"; shift 2 ;;
    --token) TOKEN="$2"; shift 2 ;;
    --name) NAME="$2"; shift 2 ;;
    --labels) LABELS="$2"; shift 2 ;;
    --concurrency) CONCURRENCY="$2"; shift 2 ;;
    --docker) USE_DOCKER="true"; shift ;;
    --version) VERSION="$2"; shift 2 ;;
    --tarball) TARBALL="$2"; shift 2 ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage; exit 1 ;;
  esac
done

[[ $EUID -eq 0 ]] || { echo "run as root (sudo)" >&2; exit 1; }
command -v systemctl >/dev/null || { echo "systemd is required" >&2; exit 1; }

if [[ -n "${UNINSTALL:-}" ]]; then
  systemctl disable --now fiber-agent 2>/dev/null || true
  rm -f "$SERVICE"
  systemctl daemon-reload
  echo "removed the fiber-agent service."
  echo "still present: $BIN_DIR/fiber-agent, $BIN_DIR/fiber, $ETC_DIR, $STATE_DIR, and the 'fiber' user."
  exit 0
fi

# Re-running is the upgrade path: keep settings that were not passed this time.
if [[ -f "$ETC_DIR/agent.env" ]]; then
  # shellcheck disable=SC1091
  . "$ETC_DIR/agent.env"
  API_URL="${API_URL:-${FIBER_API_URL:-}}"
  TOKEN="${TOKEN:-${FIBER_AGENT_TOKEN:-}}"
  [[ "$NAME" == "$(hostname -s 2>/dev/null || echo "$NAME")" ]] && NAME="${FIBER_AGENT_NAME:-$NAME}"
  [[ "$LABELS" == "os=linux" ]] && LABELS="${FIBER_AGENT_LABELS:-$LABELS}"
  [[ "$CONCURRENCY" == "2" ]] && CONCURRENCY="${FIBER_AGENT_CONCURRENCY:-$CONCURRENCY}"
  [[ "$USE_DOCKER" == "false" ]] && USE_DOCKER="${FIBER_AGENT_USE_DOCKER:-$USE_DOCKER}"
  echo "reusing settings from $ETC_DIR/agent.env for anything not passed"
fi

[[ -n "$API_URL" ]] || { echo "--api-url is required" >&2; exit 1; }
[[ -n "$TOKEN" ]] || { echo "--token is required (or set FIBER_AGENT_TOKEN)" >&2; exit 1; }

case "$(uname -s)" in
  Linux) ;;
  *) echo "this installer targets Linux + systemd; see docs/agents.md for other hosts" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) TARGET="x86_64-unknown-linux-gnu" ;;
  aarch64|arm64) TARGET="aarch64-unknown-linux-gnu" ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

command -v git >/dev/null || echo "warning: git is not installed; pipelines with a workspace will fail" >&2
if ! ldd --version 2>&1 | grep -qiE 'glibc|gnu libc'; then
  echo "warning: this build targets glibc; on musl (Alpine) it will fail to start" >&2
fi

# The asset keeps its published name so the .sha256 manifest (which records that name)
# verifies without rewriting.
ASSET="fiber-agent-$TARGET.tar.gz"
if [[ "$VERSION" == "latest" ]]; then
  URL="https://github.com/$REPO/releases/latest/download/$ASSET"
else
  URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

if [[ -n "${TARBALL:-}" ]]; then
  [[ -f "$TARBALL" ]] || { echo "no such tarball: $TARBALL" >&2; exit 1; }
  echo "installing from $TARBALL"
  tar -xzf "$TARBALL" -C "$TMP"
else
echo "downloading $URL"
if ! curl -fsSL "$URL" -o "$TMP/$ASSET"; then
  echo "download failed — no published release for this platform yet?" >&2
  echo "Build the tarball yourself and pass --tarball, or see:" >&2
  echo "  https://github.com/$REPO/blob/main/docs/agents.md#from-source" >&2
  exit 1
fi
if curl -fsSL "$URL.sha256" -o "$TMP/$ASSET.sha256" 2>/dev/null; then
  if (cd "$TMP" && sha256sum -c "$ASSET.sha256" >/dev/null 2>&1); then
    echo "checksum ok"
  else
    echo "checksum mismatch for $ASSET" >&2
    exit 1
  fi
else
  echo "warning: no published checksum for this release" >&2
fi
tar -xzf "$TMP/$ASSET" -C "$TMP"
fi
install -m 0755 "$TMP/fiber-agent" "$BIN_DIR/fiber-agent"
[[ -f "$TMP/fiber" ]] && install -m 0755 "$TMP/fiber" "$BIN_DIR/fiber"

id -u fiber >/dev/null 2>&1 || useradd --system --home-dir "$STATE_DIR" --create-home --shell /usr/sbin/nologin fiber
install -d -o fiber -g fiber -m 0750 "$STATE_DIR" "$STATE_DIR/workspaces"
install -d -m 0755 "$ETC_DIR"

# The token is a bearer credential: root-owned, agent-readable only.
umask 077
cat > "$ETC_DIR/agent.env" <<ENV
FIBER_API_URL=$API_URL
FIBER_AGENT_TOKEN=$TOKEN
FIBER_AGENT_NAME=$NAME
FIBER_AGENT_LABELS=$LABELS
FIBER_AGENT_CONCURRENCY=$CONCURRENCY
FIBER_AGENT_USE_DOCKER=$USE_DOCKER
FIBER_AGENT_WORKSPACE_DIR=$STATE_DIR/workspaces
RUST_LOG=info,fiber_agent=info
ENV
chown root:fiber "$ETC_DIR/agent.env"
chmod 0640 "$ETC_DIR/agent.env"
umask 022

if [[ "$USE_DOCKER" == "true" ]]; then
  getent group docker >/dev/null && usermod -aG docker fiber \
    || echo "warning: no docker group; Docker steps will fail" >&2
fi

# Pair the unit with the version being installed, not with main.
UNIT_REF="${VERSION}"
[[ "$UNIT_REF" == "latest" ]] && UNIT_REF="main"
UNIT_URL="https://raw.githubusercontent.com/$REPO/$UNIT_REF/deploy/fiber-agent.service"
if [[ -f "$(dirname "$0")/../deploy/fiber-agent.service" ]]; then
  install -m 0644 "$(dirname "$0")/../deploy/fiber-agent.service" "$SERVICE"
else
  curl -fsSL "$UNIT_URL" -o "$SERVICE"
fi

systemctl daemon-reload
# enable --now is a no-op on a running unit, so an upgrade would keep the old binary.
systemctl enable fiber-agent
systemctl restart fiber-agent
sleep 2
systemctl --no-pager --lines=10 status fiber-agent || true
echo
echo "installed. logs: journalctl -u fiber-agent -f"
echo "settings:      $ETC_DIR/agent.env   (systemctl restart fiber-agent after editing)"
