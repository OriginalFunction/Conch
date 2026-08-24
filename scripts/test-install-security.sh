#!/usr/bin/env bash
# Fail-closed installer tests using local release fixtures and stubbed network tools.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/common.sh
source "$SCRIPT_DIR/common.sh"

ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST="${1:-$ROOT/dist}"
VERSION="$(conch_version "$ROOT")"
export CONCH_TEST_VERSION="$VERSION"
OS="$(conch_os)"
ARCH="$(conch_arch)"
TAR="conch-${VERSION}-${OS}-${ARCH}.tar.gz"

if [[ ! -f "$DIST/$TAR" || ! -f "$DIST/SHA256SUMS" ]]; then
  echo "missing release fixtures in $DIST" >&2
  exit 1
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/conch-install-security.XXXXXX")"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT

expect_failure() {
  if "$@" >"$WORK/stdout" 2>"$WORK/stderr"; then
    echo "expected failure: $*" >&2
    exit 1
  fi
}

mkdir -p "$WORK/tampered"
cp "$DIST/$TAR" "$DIST/SHA256SUMS" "$WORK/tampered/"
printf 'tamper' >>"$WORK/tampered/$TAR"
expect_failure "$SCRIPT_DIR/install.sh" \
  --version "$VERSION" --prefix "$WORK/prefix-tampered" --dist "$WORK/tampered" \
  --os "$OS" --arch "$ARCH"

expect_failure "$SCRIPT_DIR/install.sh" \
  --version "$VERSION" --prefix "$WORK/prefix-http" \
  --base-url "http://downloads.invalid/v${VERSION}" --os "$OS" --arch "$ARCH"

mkdir -p "$WORK/bin" "$WORK/remote"
cp "$DIST/$TAR" "$DIST/SHA256SUMS" "$WORK/remote/"
if [[ -f "$DIST/conch.rb" ]]; then
  cp "$DIST/conch.rb" "$WORK/remote/"
fi
# The single-quoted lines are intentionally emitted as a separate fake tool.
# shellcheck disable=SC2016
printf '%s\n' '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'url=""; dest=""' \
  'printf "%s\n" "$*" >>"${FAKE_CURL_LOG:-/dev/null}"' \
  'while [[ $# -gt 0 ]]; do case "$1" in -o|--output) dest="$2"; shift 2 ;; --proto|--proto-redir) shift 2 ;; -*) shift ;; *) url="$1"; shift ;; esac; done' \
  'cp "$FAKE_RELEASE_DIR/${url##*/}" "$dest"' >"$WORK/bin/curl"
# shellcheck disable=SC2016
printf '%s\n' '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'repo=""; workflow=""; source_ref=""; deny="0"' \
  'while [[ $# -gt 0 ]]; do case "$1" in --repo) repo="$2"; shift 2 ;; --signer-workflow) workflow="$2"; shift 2 ;; --source-ref) source_ref="$2"; shift 2 ;; --deny-self-hosted-runners) deny="1"; shift ;; *) shift ;; esac; done' \
  'if [[ "$repo" != OriginalFunction/Conch || "$workflow" != github.com/OriginalFunction/Conch/.github/workflows/release.yml || "$source_ref" != "refs/tags/v${CONCH_TEST_VERSION}" || "$deny" != 1 || "${FAIL_ATTEST:-0}" == 1 || "${FAIL_IDENTITY:-0}" == 1 ]]; then exit 42; fi' \
  'exit 0' >"$WORK/bin/gh"
chmod 755 "$WORK/bin/curl" "$WORK/bin/gh"

# These package-manager stubs prove verification completes before an installer
# can invoke a state-changing tool.
# shellcheck disable=SC2016
printf '%s\n' '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'printf "%s\n" "$*" >>"$FAKE_PACKAGE_LOG"' \
  'case "${1:-}" in' \
  '  tap-new) tap="${!#}"; mkdir -p "$FAKE_BREW_ROOT/${tap//\//-}/Formula" ;;' \
  '  --repository) tap="$2"; printf "%s\n" "$FAKE_BREW_ROOT/${tap//\//-}" ;;' \
  'esac' \
  'exit 0' >"$WORK/bin/brew"
# shellcheck disable=SC2016
printf '%s\n' '#!/usr/bin/env bash' \
  'printf "%s\n" "$*" >>"$FAKE_PACKAGE_LOG"' \
  'exit 0' >"$WORK/bin/apt-get"
chmod 755 "$WORK/bin/brew" "$WORK/bin/apt-get"

CURL_LOG="$WORK/curl.log"
: >"$CURL_LOG"
PATH="$WORK/bin:$PATH" FAKE_RELEASE_DIR="$WORK/remote" FAKE_CURL_LOG="$CURL_LOG" \
  "$SCRIPT_DIR/install.sh" --version "$VERSION" --prefix "$WORK/prefix-valid" \
  --base-url "https://downloads.invalid/v${VERSION}" --os "$OS" --arch "$ARCH"
test -x "$WORK/prefix-valid/bin/conch"
test -x "$WORK/prefix-valid/bin/conchd"
grep -Fq -- "--proto =https" "$CURL_LOG"
grep -Fq -- "--proto-redir =https" "$CURL_LOG"

expect_failure env PATH="$WORK/bin:$PATH" FAKE_RELEASE_DIR="$WORK/remote" FAIL_IDENTITY=1 \
  "$SCRIPT_DIR/install.sh" --version "$VERSION" --prefix "$WORK/prefix-identity" \
  --base-url "https://downloads.invalid/v${VERSION}" --os "$OS" --arch "$ARCH"

expect_failure env PATH="$WORK/bin:$PATH" FAKE_RELEASE_DIR="$WORK/remote" FAIL_ATTEST=1 \
  "$SCRIPT_DIR/install.sh" --version "$VERSION" --prefix "$WORK/prefix-attestation" \
  --base-url "https://downloads.invalid/v${VERSION}" --os "$OS" --arch "$ARCH"

mkdir -p "$WORK/bad-sums"
cp "$DIST/$TAR" "$DIST/SHA256SUMS" "$WORK/bad-sums/"
sed -i.bak "s/^[0-9a-f][0-9a-f]*/$(printf '0%.0s' {1..64})/" "$WORK/bad-sums/SHA256SUMS"
expect_failure env PATH="$WORK/bin:$PATH" FAKE_RELEASE_DIR="$WORK/bad-sums" \
  "$SCRIPT_DIR/install.sh" --version "$VERSION" --prefix "$WORK/prefix-sums" \
  --base-url "https://downloads.invalid/v${VERSION}" --os "$OS" --arch "$ARCH"

PACKAGE_LOG="$WORK/package-manager.log"
FAKE_BREW_ROOT="$WORK/fake-brew"
mkdir -p "$FAKE_BREW_ROOT"
: >"$PACKAGE_LOG"
PATH="$WORK/bin:$PATH" FAKE_RELEASE_DIR="$WORK/remote" FAKE_PACKAGE_LOG="$PACKAGE_LOG" \
  FAKE_BREW_ROOT="$FAKE_BREW_ROOT" \
  "$SCRIPT_DIR/install-homebrew.sh" --version "$VERSION"
grep -Fq 'install --formula' "$PACKAGE_LOG"

CURRENT_SHA="$(conch_sha256 "$DIST/$TAR")"
ZERO_SHA="0000000000000000000000000000000000000000000000000000000000000000"
awk -v current="$CURRENT_SHA" -v zero="$ZERO_SHA" '
  !replaced && index($0, current) { sub(current, zero); replaced = 1 }
  { print }
' "$DIST/conch.rb" >"$WORK/current-placeholder.rb"
expect_failure env PATH="$WORK/bin:$PATH" FAKE_PACKAGE_LOG="$PACKAGE_LOG" \
  FAKE_BREW_ROOT="$FAKE_BREW_ROOT" \
  "$SCRIPT_DIR/install-homebrew.sh" --formula "$WORK/current-placeholder.rb"

expect_failure env PATH="/usr/bin:/bin" \
  "$SCRIPT_DIR/install-homebrew.sh" --formula "$DIST/conch.rb"

: >"$PACKAGE_LOG"
expect_failure env PATH="$WORK/bin:$PATH" FAKE_RELEASE_DIR="$WORK/remote" \
  FAKE_PACKAGE_LOG="$PACKAGE_LOG" FAIL_IDENTITY=1 \
  "$SCRIPT_DIR/install-homebrew.sh" --version "$VERSION"
test ! -s "$PACKAGE_LOG"

DEB="conch_${VERSION}_$(conch_debian_arch).deb"
if [[ -f "$DIST/$DEB" ]]; then
  cp "$DIST/$DEB" "$WORK/remote/"
  : >"$PACKAGE_LOG"
  PATH="$WORK/bin:$PATH" FAKE_RELEASE_DIR="$WORK/remote" FAKE_PACKAGE_LOG="$PACKAGE_LOG" \
    "$SCRIPT_DIR/install-debian.sh" --version "$VERSION"
  grep -Fq 'install -y' "$PACKAGE_LOG"

  ABS_DEB="$(cd "$DIST" && pwd)/$DEB"
  ABS_SUMS="$(cd "$DIST" && pwd)/SHA256SUMS"
  : >"$PACKAGE_LOG"
  PATH="$WORK/bin:$PATH" FAKE_PACKAGE_LOG="$PACKAGE_LOG" \
    "$SCRIPT_DIR/install-debian.sh" --deb "$ABS_DEB" --sums "$ABS_SUMS"
  grep -Fq "install -y $ABS_DEB" "$PACKAGE_LOG"
  if grep -Fq './/' "$PACKAGE_LOG"; then
    echo "absolute package path was prefixed with ./" >&2
    exit 1
  fi

  : >"$PACKAGE_LOG"
  expect_failure env PATH="$WORK/bin:$PATH" FAKE_RELEASE_DIR="$WORK/remote" \
    FAKE_PACKAGE_LOG="$PACKAGE_LOG" FAIL_ATTEST=1 \
    "$SCRIPT_DIR/install-debian.sh" --version "$VERSION"
  test ! -s "$PACKAGE_LOG"
fi

echo "installer security checks passed"
