#!/usr/bin/env bash
# Build Conch release tarballs, SHA256SUMS, a manifest, and a filled Homebrew formula.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/common.sh
source "$SCRIPT_DIR/common.sh"

ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUT="$ROOT/dist"
LOCAL=0
BIN_DIR=""

usage() {
  cat <<'EOF'
usage: release-artifacts.sh [--outdir DIR] [--local] [--bin-dir DIR]

  --local     use existing binaries instead of cargo build --release
  --bin-dir   directory containing conch and conchd (default: target/release)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --outdir)
      OUT="$2"
      shift 2
      ;;
    --local)
      LOCAL=1
      shift
      ;;
    --bin-dir)
      BIN_DIR="$2"
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

VERSION="$(conch_version "$ROOT")"
PLATFORM="$(conch_platform)"
NAME="conch-${VERSION}-${PLATFORM}"
BIN_DIR="${BIN_DIR:-$ROOT/target/release}"

if [[ "$LOCAL" -eq 0 ]]; then
  (cd "$ROOT" && cargo build --workspace --release --locked --bins)
fi

if [[ ! -x "$BIN_DIR/conch" || ! -x "$BIN_DIR/conchd" ]]; then
  echo "missing binaries in $BIN_DIR (need conch and conchd)" >&2
  exit 1
fi

rm -rf "${OUT:?}/${NAME:?}"
mkdir -p "$OUT/$NAME"
cp "$BIN_DIR/conch" "$BIN_DIR/conchd" "$OUT/$NAME/"
chmod 755 "$OUT/$NAME/conch" "$OUT/$NAME/conchd"
cp "$ROOT/README.md" "$OUT/$NAME/"

TAR="$OUT/${NAME}.tar.gz"
tar -C "$OUT" -czf "$TAR" "$NAME"

SUMS="$OUT/SHA256SUMS"
: >"$SUMS"
{
  printf '%s  %s\n' "$(conch_sha256 "$TAR")" "$(basename "$TAR")"
  printf '%s  %s\n' "$(conch_sha256 "$OUT/$NAME/conch")" "conch"
  printf '%s  %s\n' "$(conch_sha256 "$OUT/$NAME/conchd")" "conchd"
} >"$SUMS"

BASE_URL="${CONCH_RELEASE_BASE_URL:-https://downloads.example.invalid/conch/v${VERSION}}"
TAR_SHA="$(conch_lookup_sum "$SUMS" "$(basename "$TAR")")"

PLACEHOLDER='0000000000000000000000000000000000000000000000000000000000000000'
SHA_DARWIN_ARM64="$PLACEHOLDER"
SHA_DARWIN_AMD64="$PLACEHOLDER"
SHA_LINUX_AMD64="$PLACEHOLDER"
SHA_LINUX_ARM64="$PLACEHOLDER"
case "$PLATFORM" in
  darwin-arm64) SHA_DARWIN_ARM64="$TAR_SHA" ;;
  darwin-amd64) SHA_DARWIN_AMD64="$TAR_SHA" ;;
  linux-amd64) SHA_LINUX_AMD64="$TAR_SHA" ;;
  linux-arm64) SHA_LINUX_ARM64="$TAR_SHA" ;;
esac

sed \
  -e "s|@VERSION@|${VERSION}|g" \
  -e "s|@BASE_URL@|${BASE_URL}|g" \
  -e "s|@SHA256_DARWIN_ARM64@|${SHA_DARWIN_ARM64}|g" \
  -e "s|@SHA256_DARWIN_AMD64@|${SHA_DARWIN_AMD64}|g" \
  -e "s|@SHA256_LINUX_AMD64@|${SHA_LINUX_AMD64}|g" \
  -e "s|@SHA256_LINUX_ARM64@|${SHA_LINUX_ARM64}|g" \
  "$ROOT/packaging/homebrew/conch.rb.in" >"$OUT/conch.rb"

cat >"$OUT/manifest.json" <<EOF
{
  "name": "conch",
  "version": "${VERSION}",
  "platform": "${PLATFORM}",
  "tarball": "$(basename "$TAR")",
  "binaries": ["conch", "conchd"],
  "sha256": {
    "$(basename "$TAR")": "${TAR_SHA}",
    "conch": "$(conch_lookup_sum "$SUMS" conch)",
    "conchd": "$(conch_lookup_sum "$SUMS" conchd)"
  },
  "ports": { "http": 7420, "tcp": 7421 },
  "data_dir": "~/.conch"
}
EOF

echo "wrote $TAR"
echo "wrote $SUMS"
echo "wrote $OUT/manifest.json"
echo "wrote $OUT/conch.rb"
