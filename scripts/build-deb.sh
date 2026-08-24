#!/usr/bin/env bash
# Assemble a Debian package for conch/conchd. Uses dpkg-deb when available.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/common.sh
source "$SCRIPT_DIR/common.sh"

ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUT="$ROOT/dist"
BIN_DIR="${BIN_DIR:-$ROOT/target/release}"
ARCH="$(conch_debian_arch)"
VERSION="$(conch_version "$ROOT")"
PKG="conch_${VERSION}_${ARCH}"

mkdir -p "$OUT"
STAGE="$OUT/$PKG"
rm -rf "$STAGE"
mkdir -p "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/lib/systemd/system" \
  "$STAGE/usr/share/doc/conch"

if [[ ! -x "$BIN_DIR/conch" || ! -x "$BIN_DIR/conchd" ]]; then
  echo "missing binaries in $BIN_DIR" >&2
  exit 1
fi

cp "$BIN_DIR/conch" "$BIN_DIR/conchd" "$STAGE/usr/bin/"
chmod 755 "$STAGE/usr/bin/conch" "$STAGE/usr/bin/conchd"
cp "$ROOT/packaging/systemd/conchd.service" "$STAGE/lib/systemd/system/"
cp "$ROOT/README.md" "$STAGE/usr/share/doc/conch/"
cp "$ROOT/packaging/debian/copyright" "$STAGE/usr/share/doc/conch/"
gzip -n -9 -c "$ROOT/packaging/debian/changelog" >"$STAGE/usr/share/doc/conch/changelog.Debian.gz"

SIZE="$(du -sk "$STAGE" | awk '{ print $1 }')"
sed \
  -e "s|@VERSION@|${VERSION}|g" \
  -e "s|@ARCH@|${ARCH}|g" \
  -e "s|@SIZE@|${SIZE}|g" \
  "$ROOT/packaging/debian/control.in" >"$STAGE/DEBIAN/control"
cp "$ROOT/packaging/debian/postinst" "$ROOT/packaging/debian/prerm" "$STAGE/DEBIAN/"
chmod 755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm"

if command -v dpkg-deb >/dev/null 2>&1; then
  dpkg-deb --build "$STAGE" "$OUT/${PKG}.deb"
  printf '%s  %s\n' "$(conch_sha256 "$OUT/${PKG}.deb")" "$(basename "$OUT/${PKG}.deb")" >>"$OUT/SHA256SUMS"
  echo "wrote $OUT/${PKG}.deb"
else
  tar -C "$OUT" -czf "$OUT/${PKG}.debian.tar.gz" "$PKG"
  printf '%s  %s\n' "$(conch_sha256 "$OUT/${PKG}.debian.tar.gz")" "$(basename "$OUT/${PKG}.debian.tar.gz")" >>"$OUT/SHA256SUMS"
  echo "dpkg-deb not found; wrote $OUT/${PKG}.debian.tar.gz for inspection"
fi
