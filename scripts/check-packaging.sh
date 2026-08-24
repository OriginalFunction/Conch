#!/usr/bin/env bash
# Syntax-check packaging scripts and run prefix/debian smoke against local binaries.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/common.sh
source "$SCRIPT_DIR/common.sh"

ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

SCRIPTS=(
  scripts/common.sh
  scripts/release-artifacts.sh
  scripts/install.sh
  scripts/install-homebrew.sh
  scripts/install-debian.sh
  scripts/uninstall.sh
  scripts/build-deb.sh
  scripts/check-packaging.sh
  scripts/test-install-security.sh
  packaging/tests/smoke.sh
  packaging/debian/postinst
  packaging/debian/prerm
)

for script in "${SCRIPTS[@]}"; do
  bash -n "$script"
done

if command -v shellcheck >/dev/null 2>&1; then
  shellcheck -x "${SCRIPTS[@]}"
else
  echo "shellcheck not installed; bash -n only"
fi

BIN_DIR="${BIN_DIR:-$ROOT/target/release}"
if [[ ! -x "$BIN_DIR/conch" || ! -x "$BIN_DIR/conchd" ]]; then
  cargo build --workspace --release --locked --bins
  BIN_DIR="$ROOT/target/release"
fi

"$SCRIPT_DIR/release-artifacts.sh" --local --bin-dir "$BIN_DIR" --outdir "$ROOT/dist"
(umask 000; "$SCRIPT_DIR/build-deb.sh")

packaging/tests/smoke.sh --dist "$ROOT/dist" --bin-dir "$BIN_DIR"
scripts/test-install-security.sh "$ROOT/dist"
echo "packaging checks passed"
