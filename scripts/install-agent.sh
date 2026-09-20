#!/usr/bin/env bash
# Install fiber-agent as a systemd service.
#
#   export FIBER_AGENT_TOKEN=...
#   tag=$(basename "$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
#     https://github.com/durablefibers/fiber/releases/latest)")
#   curl -fsSL "https://raw.githubusercontent.com/durablefibers/fiber/$tag/scripts/install-agent.sh" \
#     | sudo --preserve-env=FIBER_AGENT_TOKEN bash -s -- --api-url wss://ci.example.com
#
# Fetch this script from a tag, not from `main`: it is the thing that performs every
# check below, so pinning the binary while taking the verifier from a moving branch
# only moves the problem. `--preserve-env` keeps the token out of sudo's argv.
#
# Create the token first (instance admin):
#   fiber agents create --name build-01 --labels os=linux
#
# What this verifies before it writes anything to the host:
#
#   1. `--version latest` is resolved to a concrete tag by following the
#      /releases/latest redirect, and *every* file — tarball, checksums and the
#      systemd unit — is then fetched from that one tag. A host can no longer end up
#      running a released binary with `main`'s unit.
#   2. The tarball's SHA-256 must match the release's `SHA256SUMS` (or the per-asset
#      `.sha256` on releases published before SHA256SUMS existed). A missing or
#      mismatched checksum is fatal. This is integrity, not provenance: both files
#      come from the same release over the same channel.
#   3. Provenance: if the `gh` CLI is usable, the tarball's build-provenance
#      attestation is verified with `gh attestation verify`, pinned to this repo, to
#      the release workflow, to `refs/tags/<the tag being installed>`, and to a
#      GitHub-hosted runner. Pinning the ref is what stops a release-page attacker
#      replacing the assets with a genuine, still-validly-attested build of an older
#      vulnerable tag. This is the only check a compromised release page cannot forge.
#   4. The systemd unit comes from the same tag, and its hash from the same attested
#      SHA256SUMS, because the unit decides which binary runs as which user.
#
# Under `sudo`, `env_reset` drops GH_TOKEN and points HOME at /root, so `gh` is very
# often present but not authenticated — in which case (3) does not happen. The script
# says so loudly, before it installs anything. Pass `--require-attestation` to make
# that fatal instead.
#
# `--check-only` runs 1-3 and exits without touching the host — use it to verify a
# download by hand, or from a non-root shell where `gh` is actually authenticated. It
# exits 0 only when both the checksum and the provenance check passed.
set -euo pipefail

REPO="${FIBER_REPO:-durablefibers/fiber}"
VERSION="${FIBER_VERSION:-latest}"
# Binds an attestation to the workflow that is allowed to produce releases, not just
# to the repository. Override for a fork that publishes from a different path.
SIGNER_WORKFLOW="${FIBER_SIGNER_WORKFLOW:-$REPO/.github/workflows/release.yml}"
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
CHECK_ONLY="false"
SKIP_CHECKSUM="false"
SKIP_ATTESTATION="false"
REQUIRE_ATTESTATION="false"

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
  --check-only       Resolve, download and verify a release, then exit (no root).
                     Exits 0 only if both the checksum and the provenance verified,
                     2 if either could not be obtained, 1 if either failed.
  --require-attestation
                     Refuse to install unless provenance actually verified. Without
                     it, an unusable `gh` is a loud warning, not an error.
  --uninstall        Stop, disable, and remove the service

Escape hatches — each one prints exactly which guarantee it is giving up:

  --insecure-skip-checksum      Install even if the release publishes no checksum,
                                or if it does not match.
  --insecure-skip-attestation   Install even if `gh attestation verify` fails
                                (e.g. a release published before attestations).

The token is read from $FIBER_AGENT_TOKEN when --token is omitted; prefer that, since
argv is world-readable in ps for the life of the script.
USAGE
}

die() { echo "$*" >&2; exit 1; }

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
    --check-only) CHECK_ONLY="true"; shift ;;
    --require-attestation) REQUIRE_ATTESTATION="true"; shift ;;
    --insecure-skip-checksum) SKIP_CHECKSUM="true"; shift ;;
    --insecure-skip-attestation) SKIP_ATTESTATION="true"; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage; exit 1 ;;
  esac
done

command -v curl >/dev/null || die "curl is required"

if [[ "$CHECK_ONLY" != "true" ]]; then
  [[ $EUID -eq 0 ]] || die "run as root (sudo), or pass --check-only to verify a download only"
  command -v systemctl >/dev/null || die "systemd is required"
fi

if [[ -n "${UNINSTALL:-}" ]]; then
  systemctl disable --now fiber-agent 2>/dev/null || true
  rm -f "$SERVICE"
  systemctl daemon-reload
  echo "removed the fiber-agent service."
  echo "still present: $BIN_DIR/fiber-agent, $BIN_DIR/fiber, $ETC_DIR, $STATE_DIR, and the 'fiber' user."
  exit 0
fi

# --- platform -----------------------------------------------------------------

case "$(uname -s)" in
  Linux) ;;
  Darwin)
    # A macOS host cannot run the unit, but it can verify a macOS release asset.
    [[ "$CHECK_ONLY" == "true" ]] ||
      die "this installer targets Linux + systemd; see docs/agents.md for other hosts"
    ;;
  *) die "this installer targets Linux + systemd; see docs/agents.md for other hosts" ;;
esac
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64|Linux/amd64) TARGET="x86_64-unknown-linux-gnu" ;;
  Linux/aarch64|Linux/arm64) TARGET="aarch64-unknown-linux-gnu" ;;
  Darwin/arm64) TARGET="aarch64-apple-darwin" ;;
  *) die "unsupported platform: $(uname -s) $(uname -m)" ;;
esac

# The asset keeps its published name so a checksum manifest, which records that name,
# verifies without rewriting.
ASSET="fiber-agent-$TARGET.tar.gz"

# --- release resolution -------------------------------------------------------

RESOLVED_TAG=""

# `latest` is a redirect, not a tag. Resolve it once, up front, and build every URL
# from the result: the tarball, the checksums and the systemd unit all come from the
# same tag. Fetching the unit from `main` (what this script used to do) could pair a
# released binary with an unreleased unit.
resolve_tag() {
  [[ -n "$RESOLVED_TAG" ]] && return 0
  if [[ "$VERSION" != "latest" ]]; then
    RESOLVED_TAG="$VERSION"
    return 0
  fi
  local effective
  effective="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
    "https://github.com/$REPO/releases/latest")" ||
    die "could not reach https://github.com/$REPO/releases/latest to resolve 'latest'"
  RESOLVED_TAG="${effective##*/}"
  case "$RESOLVED_TAG" in
    v[0-9]*) ;;
    *) die "'latest' resolved to '$RESOLVED_TAG' via $effective, which is not a version tag" ;;
  esac
  echo "latest resolves to $RESOLVED_TAG"
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    die "neither sha256sum nor shasum is available; cannot verify anything"
  fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

CHECKSUM_RESULT=""
PROVENANCE_RESULT=""
ASSET_SHA256=""
# Only true when the guarantee was actually obtained — a skip or an unusable `gh`
# leaves these false, which is what --check-only's exit code reports.
CHECKSUM_OK="false"
PROVENANCE_OK="false"
UNIT_ORIGIN=""

verify_checksum() {
  local base="$1" expected="" source=""
  if curl -fsSL "$base/SHA256SUMS" -o "$TMP/SHA256SUMS" 2>/dev/null; then
    source="SHA256SUMS"
    # `sha256sum` writes "<hash>  <name>"; the binary-mode marker is a leading '*'.
    expected="$(awk -v f="$ASSET" '$2 == f || $2 == "*" f {print $1; exit}' "$TMP/SHA256SUMS")"
  elif curl -fsSL "$base/$ASSET.sha256" -o "$TMP/$ASSET.sha256" 2>/dev/null; then
    source="$ASSET.sha256"
    expected="$(awk '{print $1; exit}' "$TMP/$ASSET.sha256")"
  fi

  if [[ -z "$expected" ]]; then
    if [[ "$SKIP_CHECKSUM" == "true" ]]; then
      CHECKSUM_RESULT="SKIPPED — release $RESOLVED_TAG publishes no checksum for $ASSET and --insecure-skip-checksum was passed"
      echo "WARNING: $CHECKSUM_RESULT" >&2
      echo "WARNING: you are trusting the transport and the release page, nothing else." >&2
      return 0
    fi
    die "no published checksum for $ASSET in release $RESOLVED_TAG (looked for SHA256SUMS and $ASSET.sha256).
Refusing to install unverified bits. Pass --insecure-skip-checksum to override."
  fi

  # Lowercased through tr, not ${x,,}: macOS still ships bash 3.2 and --check-only
  # is meant to run there.
  expected="$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')"
  if [[ "$expected" != "$(printf '%s' "$ASSET_SHA256" | tr '[:upper:]' '[:lower:]')" ]]; then
    if [[ "$SKIP_CHECKSUM" == "true" ]]; then
      CHECKSUM_RESULT="SKIPPED — $source says $expected, the download is $ASSET_SHA256, and --insecure-skip-checksum was passed"
      echo "WARNING: $CHECKSUM_RESULT" >&2
      return 0
    fi
    die "checksum mismatch for $ASSET
  expected:   $expected   (from $source)
  downloaded: $ASSET_SHA256
Do not install this. Re-download, and if it happens again report it."
  fi
  CHECKSUM_OK="true"
  CHECKSUM_RESULT="OK — matches $source from release $RESOLVED_TAG (integrity only: same origin as the tarball)"
  echo "checksum ok ($source)"
}

# Every "we did not check" path warns here and now. The old code set a result string
# and returned silently, so on the documented `curl | sudo bash` path — where sudo's
# env_reset drops GH_TOKEN and HOME, making an unauthenticated `gh` the normal case —
# the operator learned that provenance was skipped only in the summary, after the
# agent was installed and running.
provenance_unavailable() {
  local why="$1"
  PROVENANCE_RESULT="NOT CHECKED — $why"
  echo "WARNING: provenance NOT CHECKED — $why" >&2
  echo "WARNING: the checksum only proves this tarball matches the release page. Nothing" >&2
  echo "WARNING: here proves the release page itself was not tampered with." >&2
  echo "WARNING: verify as yourself instead:  install-agent.sh --check-only" >&2
  if [[ "$REQUIRE_ATTESTATION" == "true" ]]; then
    die "--require-attestation was passed and provenance could not be checked."
  fi
}

verify_attestation() {
  local file="$1"
  if ! command -v gh >/dev/null 2>&1; then
    provenance_unavailable "the gh CLI is not installed (https://cli.github.com)."
    return 0
  fi
  if ! gh auth status >/dev/null 2>&1; then
    provenance_unavailable "gh is installed but not authenticated for this user. Under
    sudo this is expected: env_reset drops GH_TOKEN and HOME points at /root. Use
    'sudo --preserve-env=GH_TOKEN', or run --check-only as yourself first."
    return 0
  fi
  # --source-ref is what binds the attestation to the *version* being installed.
  # Without it, a genuine and still-validly-attested tarball from an older, vulnerable
  # tag passes both the checksum (swap SHA256SUMS too) and the provenance check.
  # --deny-self-hosted-runners makes the "GitHub-hosted runner" claim in the docs real.
  if gh attestation verify "$file" --repo "$REPO" \
      --signer-workflow "$SIGNER_WORKFLOW" \
      --source-ref "refs/tags/$RESOLVED_TAG" \
      --deny-self-hosted-runners \
      >"$TMP/attest.out" 2>&1; then
    PROVENANCE_OK="true"
    PROVENANCE_RESULT="VERIFIED — $SIGNER_WORKFLOW at refs/tags/$RESOLVED_TAG, GitHub-hosted runner"
    echo "provenance ok (gh attestation verify)"
    return 0
  fi
  if [[ "$SKIP_ATTESTATION" == "true" ]]; then
    PROVENANCE_RESULT="SKIPPED — gh attestation verify failed and --insecure-skip-attestation was passed"
    echo "WARNING: $PROVENANCE_RESULT" >&2
    echo "WARNING: nothing proves this tarball came from $SIGNER_WORKFLOW." >&2
    sed 's/^/  gh: /' "$TMP/attest.out" >&2
    return 0
  fi
  echo "gh attestation verify failed for $file:" >&2
  sed 's/^/  gh: /' "$TMP/attest.out" >&2
  die "no valid build provenance for $ASSET from $SIGNER_WORKFLOW at refs/tags/$RESOLVED_TAG.
Releases up to and including v0.6.2 carry no attestations — for those, pass
--insecure-skip-attestation (the checksum is still enforced). Otherwise treat this
as a tampered download: an attestation that exists but does not match this tag is
exactly what an asset swapped in from an older release looks like."
}

# --- fetch and verify ---------------------------------------------------------

# What gets recorded and reported. For a release this is the tag; for --tarball there is
# no tag, and RESOLVED_TAG stays empty so the unit fetch below can still resolve one.
INSTALLED_FROM=""

if [[ -n "${TARBALL:-}" ]]; then
  [[ -f "$TARBALL" ]] || die "no such tarball: $TARBALL"
  ASSET_SHA256="$(sha256_of "$TARBALL")"
  echo "installing from $TARBALL (sha256 $ASSET_SHA256)"
  CHECKSUM_RESULT="NOT CHECKED — you supplied the file with --tarball; there is nothing to compare it to"
  PROVENANCE_RESULT="NOT CHECKED — you supplied the file with --tarball; you own its provenance"
  INSTALLED_FROM="local-tarball"
  tar -xzf "$TARBALL" -C "$TMP"
else
  resolve_tag
  INSTALLED_FROM="$RESOLVED_TAG"
  BASE="https://github.com/$REPO/releases/download/$RESOLVED_TAG"
  echo "downloading $BASE/$ASSET"
  if ! curl -fsSL "$BASE/$ASSET" -o "$TMP/$ASSET"; then
    echo "download failed — no published release for this platform yet?" >&2
    echo "Build the tarball yourself and pass --tarball, or see:" >&2
    echo "  https://github.com/$REPO/blob/$RESOLVED_TAG/docs/agents.md#from-source" >&2
    exit 1
  fi
  ASSET_SHA256="$(sha256_of "$TMP/$ASSET")"
  verify_checksum "$BASE"
  verify_attestation "$TMP/$ASSET"
  tar -xzf "$TMP/$ASSET" -C "$TMP"
fi

[[ -f "$TMP/fiber-agent" ]] || die "the tarball does not contain fiber-agent"
BIN_SHA256="$(sha256_of "$TMP/fiber-agent")"

report() {
  echo
  echo "release:    $INSTALLED_FROM   ($REPO)"
  echo "asset:      $ASSET"
  echo "  sha256    $ASSET_SHA256"
  echo "fiber-agent binary"
  echo "  sha256    $BIN_SHA256"
  echo "checksum:   $CHECKSUM_RESULT"
  echo "provenance: $PROVENANCE_RESULT"
  if [[ -n "$UNIT_ORIGIN" ]]; then
    echo "unit:       $UNIT_ORIGIN"
  fi
  echo
}

if [[ "$CHECK_ONLY" == "true" ]]; then
  report
  echo "--check-only: nothing was installed."
  # A verification command has to report verification in its exit code, or a script
  # that calls it learns nothing. 0 = both guarantees; 2 = one could not be obtained
  # (no usable gh, or an --insecure-skip-*); 1 = a check ran and failed, which the
  # die() paths above already produced.
  if [[ "$CHECKSUM_OK" == "true" && "$PROVENANCE_OK" == "true" ]]; then
    exit 0
  fi
  echo "--check-only: NOT fully verified — see the checksum/provenance lines above." >&2
  exit 2
fi

# --- settings -----------------------------------------------------------------

PREV_VERSION="unknown"
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
  PREV_VERSION="${FIBER_AGENT_INSTALLED_VERSION:-unknown}"
  echo "reusing settings from $ETC_DIR/agent.env for anything not passed"
  echo "upgrading from $PREV_VERSION to $INSTALLED_FROM"
fi

[[ -n "$API_URL" ]] || die "--api-url is required"
[[ -n "$TOKEN" ]] || die "--token is required (or set FIBER_AGENT_TOKEN)"

command -v git >/dev/null || echo "warning: git is not installed; pipelines with a workspace will fail" >&2
if ! ldd --version 2>&1 | grep -qiE 'glibc|gnu libc'; then
  echo "warning: this build targets glibc; on musl (Alpine) it will fail to start" >&2
fi

# --- the unit -----------------------------------------------------------------

# The unit decides which binary runs as which user, so where it comes from matters as
# much as the binary does. Resolved *before* anything is written to the host, so a
# failure here cannot leave a new binary paired with the old unit. In order:
#   1. the checkout this script is running from
#   2. the release asset, whose hash is in the attested SHA256SUMS
#   3. raw.githubusercontent.com at the resolved tag — right tag, unverified
UNIT_SRC=""

# `$0` is the literal string "bash" under `curl | sudo bash -s --`, and `[[ -f bash ]]`
# resolves it against the current directory: a planted ./bash next to a planted
# ../deploy/fiber-agent.service would have installed an attacker's unit — with its own
# ExecStart= and User=root — the next time an admin ran the documented command from
# that directory. BASH_SOURCE[0] is unset for a script read from stdin and set only
# when bash is executing a real file, so it cannot be spoofed that way. The basename
# check is belt and braces.
SELF="${BASH_SOURCE[0]:-}"
if [[ -n "$SELF" && -f "$SELF" && "$(basename "$SELF")" == "install-agent.sh" ]]; then
  candidate="$(dirname "$SELF")/../deploy/fiber-agent.service"
  if [[ -f "$candidate" ]]; then
    UNIT_SRC="$candidate"
    UNIT_ORIGIN="$candidate (this checkout, not the release)"
  fi
fi

if [[ -z "$UNIT_SRC" ]]; then
  if [[ -n "${TARBALL:-}" ]]; then
    echo "warning: --tarball has no tag to pair the unit with; taking it from the newest" >&2
    echo "         release instead. Run the installer from a checkout to avoid that." >&2
  fi
  resolve_tag
  unit_asset_url="https://github.com/$REPO/releases/download/$RESOLVED_TAG/fiber-agent.service"
  unit_raw_url="https://raw.githubusercontent.com/$REPO/$RESOLVED_TAG/deploy/fiber-agent.service"
  unit_expected=""
  if [[ -f "$TMP/SHA256SUMS" ]]; then
    unit_expected="$(awk '$2 == "fiber-agent.service" || $2 == "*fiber-agent.service" {print $1; exit}' "$TMP/SHA256SUMS")"
  fi
  if [[ -n "$unit_expected" ]] && curl -fsSL "$unit_asset_url" -o "$TMP/fiber-agent.service" 2>/dev/null; then
    unit_actual="$(sha256_of "$TMP/fiber-agent.service" | tr '[:upper:]' '[:lower:]')"
    unit_expected="$(printf '%s' "$unit_expected" | tr '[:upper:]' '[:lower:]')"
    if [[ "$unit_expected" != "$unit_actual" ]]; then
      die "checksum mismatch for fiber-agent.service
  expected (SHA256SUMS): $unit_expected
  downloaded:            $unit_actual
Do not install this: the unit chooses what runs as which user."
    fi
    UNIT_SRC="$TMP/fiber-agent.service"
    UNIT_ORIGIN="$unit_asset_url (sha256 in SHA256SUMS)"
    echo "unit checksum ok (SHA256SUMS)"
  elif curl -fsSL "$unit_raw_url" -o "$TMP/fiber-agent.service"; then
    UNIT_SRC="$TMP/fiber-agent.service"
    UNIT_ORIGIN="$unit_raw_url (UNVERIFIED)"
    echo "warning: release $RESOLVED_TAG does not publish fiber-agent.service as an asset," >&2
    echo "         so the unit came from $unit_raw_url. It is the right tag, but its hash" >&2
    echo "         is in no checksum manifest and covered by no attestation." >&2
  else
    # Keeping whatever unit is already on disk would pair a new binary with an old
    # unit, which is the exact mismatch this installer exists to prevent.
    die "could not obtain deploy/fiber-agent.service for $RESOLVED_TAG from either
  $unit_asset_url
  $unit_raw_url
Refusing to install a new binary against the unit already on disk. Re-run with
network access, or from a checkout of the matching tag."
  fi
fi

# --- install ------------------------------------------------------------------

# Keep the binary we are replacing so a bad rollout is one `cp` away from undone.
# `cp -p` rather than `mv`: the running process keeps its open inode either way, but a
# copy leaves a working binary in place if the install below fails.
for bin in fiber-agent fiber; do
  if [[ -f "$BIN_DIR/$bin" ]]; then
    cp -p "$BIN_DIR/$bin" "$BIN_DIR/$bin.prev"
  fi
done
install -m 0755 "$TMP/fiber-agent" "$BIN_DIR/fiber-agent"
if [[ -f "$TMP/fiber" ]]; then
  install -m 0755 "$TMP/fiber" "$BIN_DIR/fiber"
fi

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
# Written by scripts/install-agent.sh. Read back on the next run so an upgrade can say
# what it is replacing; the agent itself ignores them.
FIBER_AGENT_INSTALLED_VERSION=$INSTALLED_FROM
FIBER_AGENT_INSTALLED_SHA256=$BIN_SHA256
ENV
chown root:fiber "$ETC_DIR/agent.env"
chmod 0640 "$ETC_DIR/agent.env"
umask 022

if [[ "$USE_DOCKER" == "true" ]]; then
  getent group docker >/dev/null && usermod -aG docker fiber \
    || echo "warning: no docker group; Docker steps will fail" >&2
fi

echo "installing the unit from $UNIT_ORIGIN"
install -m 0644 "$UNIT_SRC" "$SERVICE"

systemctl daemon-reload
# enable --now is a no-op on a running unit, so an upgrade would keep the old binary.
systemctl enable fiber-agent
systemctl restart fiber-agent
sleep 2
systemctl --no-pager --lines=10 status fiber-agent || true
report
echo "installed. logs: journalctl -u fiber-agent -f"
echo "settings:      $ETC_DIR/agent.env   (systemctl restart fiber-agent after editing)"
if [[ -f "$BIN_DIR/fiber-agent.prev" ]]; then
  echo "rollback:      cp -p $BIN_DIR/fiber-agent.prev $BIN_DIR/fiber-agent && systemctl restart fiber-agent"
fi
