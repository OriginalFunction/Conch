#!/usr/bin/env bash
# Install a verified Conch .deb, or bootstrap a signed apt repository.
# Never pipes a script to a shell. Download, verify, then install.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/common.sh
source "$SCRIPT_DIR/common.sh"

VERSION=""
DEB=""
SUMS=""
REPO=""

usage() {
  cat <<'EOF'
usage:
  install-debian.sh --deb FILE --sums SHA256SUMS
      Verify FILE against SHA256SUMS, then dpkg -i (or apt-get install ./FILE).

  install-debian.sh --version X.Y.Z
      Download the matching .deb and SHA256SUMS from the GitHub release over
      HTTPS, verify GitHub workflow attestations and the checksum, then apt-get
      install the local package.

No curl|sh. Root is required only for the actual package install step.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version)
      VERSION="$2"
      shift 2
      ;;
    --deb)
      DEB="$2"
      shift 2
      ;;
    --sums)
      SUMS="$2"
      shift 2
      ;;
    --repo)
      REPO="$2"
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

install_deb() {
  local deb="$1"
  if command -v apt-get >/dev/null 2>&1; then
    apt-get install -y "./$deb"
  else
    dpkg -i "$deb"
  fi
}

if [[ -n "$DEB" ]]; then
  if [[ -z "$SUMS" ]]; then
    echo "--deb requires --sums" >&2
    exit 1
  fi
  EXPECTED="$(conch_lookup_sum "$SUMS" "$(basename "$DEB")")"
  conch_verify_sha256 "$DEB" "$EXPECTED"
  install_deb "$DEB"
  exit 0
fi

if [[ -n "$REPO" ]]; then
  echo "Conch v1 does not publish an apt repository; use --version for an attested GitHub .deb" >&2
  exit 1
fi

if [[ -n "$VERSION" ]]; then
  ARCH="$(conch_debian_arch)"
  NAME="conch_${VERSION}_${ARCH}.deb"
  BASE="https://github.com/OriginalFunction/Conch/releases/download/v${VERSION}"
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/conch-deb.XXXXXX")"
  trap 'rm -rf "$WORK"' EXIT
  conch_download "$BASE/$NAME" "$WORK/$NAME"
  conch_download "$BASE/SHA256SUMS" "$WORK/SHA256SUMS"
  conch_verify_github_attestation "$WORK/$NAME" "$VERSION"
  conch_verify_github_attestation "$WORK/SHA256SUMS" "$VERSION"
  EXPECTED="$(conch_lookup_sum "$WORK/SHA256SUMS" "$NAME")"
  conch_verify_sha256 "$WORK/$NAME" "$EXPECTED"
  (
    cd "$WORK"
    install_deb "$NAME"
  )
  exit 0
fi

usage >&2
exit 1
