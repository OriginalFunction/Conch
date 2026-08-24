#!/usr/bin/env bash
# Install Conch via Homebrew from a generated formula (checksums from release-artifacts).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/common.sh
source "$SCRIPT_DIR/common.sh"

ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FORMULA=""
DIST=""
VERSION=""
WORK=""
TAP_NAME=""

cleanup() {
  if [[ -n "$TAP_NAME" ]] && command -v brew >/dev/null 2>&1; then
    brew untap "$TAP_NAME" >/dev/null 2>&1 || true
  fi
  if [[ -n "$WORK" ]]; then
    rm -rf "$WORK"
  fi
}
trap cleanup EXIT

usage() {
  cat <<'EOF'
usage: install-homebrew.sh [--formula FILE] [--dist DIR]
       install-homebrew.sh --version X.Y.Z

Uses a formula whose sha256 values were filled by release-artifacts.sh.
With --version, downloads conch.rb from GitHub and verifies its workflow
attestation before invoking Homebrew. Does not hand-edit checksums. Requires brew.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --formula)
      FORMULA="$2"
      shift 2
      ;;
    --dist)
      DIST="$2"
      shift 2
      ;;
    --version)
      VERSION="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

if [[ -n "$VERSION" ]]; then
  if [[ -n "$FORMULA" || -n "$DIST" ]]; then
    echo "--version cannot be combined with --formula or --dist" >&2
    exit 1
  fi
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/conch-brew.XXXXXX")"
  FORMULA="$WORK/conch.rb"
  conch_download \
    "https://github.com/OriginalFunction/Conch/releases/download/v${VERSION}/conch.rb" \
    "$FORMULA"
  conch_verify_github_attestation "$FORMULA" "$VERSION"
fi

FORMULA="${FORMULA:-${DIST:+$DIST/conch.rb}}"
FORMULA="${FORMULA:-$ROOT/dist/conch.rb}"

if [[ ! -f "$FORMULA" ]]; then
  echo "missing formula $FORMULA (run scripts/release-artifacts.sh first)" >&2
  exit 1
fi

CURRENT_PLATFORM="$(conch_platform)"
CURRENT_SHA="$(awk -v suffix="-${CURRENT_PLATFORM}.tar.gz\"" '
  $1 == "url" && index($0, suffix) { target = 1; next }
  target && $1 == "sha256" { gsub(/\"/, "", $2); print $2; exit }
' "$FORMULA")"
if [[ ! "$CURRENT_SHA" =~ ^[0-9a-f]{64}$ ]] || \
  [[ "$CURRENT_SHA" == "0000000000000000000000000000000000000000000000000000000000000000" ]]; then
  echo "formula has no release checksum for the current platform ($CURRENT_PLATFORM)" >&2
  exit 1
fi

if grep -q '0000000000000000000000000000000000000000000000000000000000000000' "$FORMULA"; then
  echo "formula still has placeholder checksums for some platforms; that is expected for unbuilt targets." >&2
  echo "the current-platform url/sha256 is populated and will be used." >&2
fi

if ! command -v brew >/dev/null 2>&1; then
  echo "brew not found; formula is at $FORMULA" >&2
  echo "install Homebrew, then: brew install --formula $(printf '%q' "$FORMULA")" >&2
  exit 1
fi

# Current Homebrew accepts third-party formulae only from a tap. Use an
# ephemeral, process-unique local tap so the verified formula is installed
# without requiring users to configure a permanent repository.
TAP_NAME="originalfunction/conch-installer-$$"
brew tap-new --no-git "$TAP_NAME" >/dev/null
TAP_DIR="$(brew --repository "$TAP_NAME")"
mkdir -p "$TAP_DIR/Formula"
cp "$FORMULA" "$TAP_DIR/Formula/conch.rb"
brew install --formula "$TAP_NAME/conch"
