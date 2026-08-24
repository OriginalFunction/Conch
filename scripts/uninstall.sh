#!/usr/bin/env bash
# Remove Conch binaries. Does not delete ~/.conch unless --purge is passed.
set -euo pipefail

PREFIX=""
PURGE=0

usage() {
  cat <<'EOF'
usage: uninstall.sh [--prefix DIR] [--purge]

  --prefix DIR   remove DIR/bin/conch and DIR/bin/conchd (portable install)
  --purge        also delete ~/.conch (or CONCH_DATA_DIR)

If --prefix is omitted:
  brew uninstall conch   (when brew has the formula)
  dpkg -r conch          (when the Debian package is installed)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --prefix)
      PREFIX="$2"
      shift 2
      ;;
    --purge)
      PURGE=1
      shift
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

if [[ -n "$PREFIX" ]]; then
  rm -f "$PREFIX/bin/conch" "$PREFIX/bin/conchd"
  echo "removed $PREFIX/bin/conch and $PREFIX/bin/conchd"
elif command -v brew >/dev/null 2>&1 && brew list --formula conch >/dev/null 2>&1; then
  brew uninstall conch
elif command -v dpkg >/dev/null 2>&1 && dpkg -s conch >/dev/null 2>&1; then
  dpkg -r conch
else
  echo "no prefix, Homebrew formula, or Debian package found" >&2
  echo "pass --prefix, or brew uninstall conch, or dpkg -r conch" >&2
  exit 1
fi

if [[ "$PURGE" -eq 1 ]]; then
  DATA="${CONCH_DATA_DIR:-$HOME/.conch}"
  rm -rf "$DATA"
  echo "removed $DATA"
fi
