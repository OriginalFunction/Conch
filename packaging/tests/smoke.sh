#!/usr/bin/env bash
# Exercise prefix install, generated checksums, Homebrew formula fill, and --version.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
# shellcheck source=scripts/common.sh
source "$ROOT/scripts/common.sh"

DIST=""
BIN_DIR=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dist)
      DIST="$2"
      shift 2
      ;;
    --bin-dir)
      BIN_DIR="$2"
      shift 2
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

DIST="${DIST:-$ROOT/dist}"
VERSION="$(conch_version "$ROOT")"
PLATFORM="$(conch_platform)"
TAR="conch-${VERSION}-${PLATFORM}.tar.gz"

if [[ ! -f "$DIST/$TAR" || ! -f "$DIST/SHA256SUMS" ]]; then
  echo "run scripts/release-artifacts.sh first (missing $DIST/$TAR)" >&2
  exit 1
fi

PREFIX="$(mktemp -d "${TMPDIR:-/tmp}/conch-prefix.XXXXXX")"
trap 'rm -rf "$PREFIX"' EXIT

"$ROOT/scripts/install.sh" --version "$VERSION" --prefix "$PREFIX" --dist "$DIST"

"$PREFIX/bin/conch" --version | grep -F "$VERSION"
"$PREFIX/bin/conchd" --version | grep -F "$VERSION"

if [[ -n "$BIN_DIR" ]]; then
  "$BIN_DIR/conch" --version | grep -F "$VERSION"
  "$BIN_DIR/conchd" --version | grep -F "$VERSION"
fi

TAR_SHA="$(conch_lookup_sum "$DIST/SHA256SUMS" "$TAR")"
conch_verify_sha256 "$DIST/$TAR" "$TAR_SHA"
if ! tar -tzf "$DIST/$TAR" | awk '/\/LICENSE$/ { found = 1 } END { exit found ? 0 : 1 }'; then
  echo "release archive is missing LICENSE" >&2
  exit 1
fi

if ! grep -q "$TAR_SHA" "$DIST/conch.rb"; then
  echo "Homebrew formula does not contain the current tarball checksum" >&2
  exit 1
fi

if ! grep -q "$VERSION" "$DIST/manifest.json"; then
  echo "manifest.json missing version" >&2
  exit 1
fi

"$ROOT/scripts/uninstall.sh" --prefix "$PREFIX"
if [[ -e "$PREFIX/bin/conch" || -e "$PREFIX/bin/conchd" ]]; then
  echo "uninstall left binaries behind" >&2
  exit 1
fi

echo "smoke ok: prefix install, --version, checksums, formula, uninstall"
