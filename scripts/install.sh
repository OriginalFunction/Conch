#!/usr/bin/env bash
# Verified prefix installer for macOS/Linux. Writes only under --prefix.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/common.sh
source "$SCRIPT_DIR/common.sh"

VERSION=""
PREFIX=""
DIST=""
BASE_URL="${CONCH_RELEASE_BASE_URL:-}"
OS=""
ARCH=""

usage() {
  cat <<'EOF'
usage: install.sh --version VER --prefix DIR [--dist DIR] [--base-url URL] [--os OS] [--arch ARCH]

Downloads (or copies) a release tarball, verifies its GitHub workflow
attestation and SHA-256 from SHA256SUMS, and
installs conch and conchd into PREFIX/bin. Does not write outside PREFIX.
Does not install or start a service. Does not touch ~/.conch.

Remote installs require HTTPS and GitHub CLI (`gh`). `--dist` is the explicit
local-file path and therefore does not require an online attestation lookup.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      VERSION="$2"
      shift 2
      ;;
    --prefix)
      PREFIX="$2"
      shift 2
      ;;
    --dist)
      DIST="$2"
      shift 2
      ;;
    --base-url)
      BASE_URL="$2"
      shift 2
      ;;
    --os)
      OS="$2"
      shift 2
      ;;
    --arch)
      ARCH="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -z "$VERSION" || -z "$PREFIX" ]]; then
  usage >&2
  exit 1
fi

OS="${OS:-$(conch_os)}"
ARCH="${ARCH:-$(conch_arch)}"
PLATFORM="${OS}-${ARCH}"
NAME="conch-${VERSION}-${PLATFORM}"
TAR="${NAME}.tar.gz"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/conch-install.XXXXXX")"
cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT

if [[ -n "$DIST" ]]; then
  cp "$DIST/$TAR" "$DIST/SHA256SUMS" "$WORKDIR/"
else
  if [[ -z "$BASE_URL" ]]; then
    echo "set --base-url or CONCH_RELEASE_BASE_URL, or pass --dist" >&2
    exit 1
  fi
  conch_require_https "$BASE_URL"
  conch_download "${BASE_URL%/}/$TAR" "$WORKDIR/$TAR"
  conch_download "${BASE_URL%/}/SHA256SUMS" "$WORKDIR/SHA256SUMS"
  conch_verify_github_attestation "$WORKDIR/$TAR" "$VERSION"
  conch_verify_github_attestation "$WORKDIR/SHA256SUMS" "$VERSION"
fi

EXPECTED="$(conch_lookup_sum "$WORKDIR/SHA256SUMS" "$TAR")"
conch_verify_sha256 "$WORKDIR/$TAR" "$EXPECTED"

mkdir -p "$WORKDIR/extract"
tar -xzf "$WORKDIR/$TAR" -C "$WORKDIR/extract"

SRC="$WORKDIR/extract/$NAME"
if [[ ! -x "$SRC/conch" || ! -x "$SRC/conchd" ]]; then
  echo "tarball missing conch/conchd" >&2
  exit 1
fi

mkdir -p "$PREFIX/bin"
cp "$SRC/conch" "$SRC/conchd" "$PREFIX/bin/"
chmod 755 "$PREFIX/bin/conch" "$PREFIX/bin/conchd"

echo "installed $PREFIX/bin/conch and $PREFIX/bin/conchd"
echo "smoke: $PREFIX/bin/conch --version && $PREFIX/bin/conchd --version"
