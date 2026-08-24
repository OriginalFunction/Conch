# shellcheck shell=bash
# Shared helpers for Conch packaging. Source this file; do not execute it.

set -euo pipefail

conch_repo_root() {
  local dir
  dir="$(cd "$(dirname "${BASH_SOURCE[1]}")/.." && pwd)"
  printf '%s\n' "$dir"
}

conch_version() {
  local root="${1:-}"
  if [[ -z "$root" ]]; then
    root="$(conch_repo_root)"
  fi
  awk '
    $0 == "[workspace.package]" { hit = 1; next }
    hit && $1 == "version" {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  ' "$root/Cargo.toml"
}

conch_os() {
  case "$(uname -s)" in
    Darwin) printf 'darwin\n' ;;
    Linux) printf 'linux\n' ;;
    *)
      echo "unsupported OS: $(uname -s)" >&2
      return 1
      ;;
  esac
}

conch_arch() {
  case "$(uname -m)" in
    arm64 | aarch64) printf 'arm64\n' ;;
    x86_64 | amd64) printf 'amd64\n' ;;
    *)
      echo "unsupported architecture: $(uname -m)" >&2
      return 1
      ;;
  esac
}

conch_platform() {
  printf '%s-%s\n' "$(conch_os)" "$(conch_arch)"
}

conch_debian_arch() {
  case "$(conch_arch)" in
    arm64) printf 'arm64\n' ;;
    amd64) printf 'amd64\n' ;;
  esac
}

conch_sha256() {
  local file="$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | awk '{ print $1 }'
  else
    echo "need sha256sum or shasum" >&2
    return 1
  fi
}

conch_verify_sha256() {
  local file="$1"
  local expected="$2"
  local actual
  actual="$(conch_sha256 "$file")"
  if [[ "$actual" != "$expected" ]]; then
    echo "checksum mismatch for $file" >&2
    echo "  expected $expected" >&2
    echo "  actual   $actual" >&2
    return 1
  fi
}

conch_lookup_sum() {
  local sums="$1"
  local name="$2"
  awk -v name="$name" '$2 == name { print $1; found = 1 } END { exit found ? 0 : 1 }' "$sums"
}

conch_download() {
  local url="$1"
  local dest="$2"
  if command -v curl >/dev/null 2>&1; then
    curl --fail --silent --show-error --location \
      --proto '=https' --proto-redir '=https' \
      "$url" --output "$dest"
  elif command -v wget >/dev/null 2>&1; then
    wget --https-only -qO "$dest" "$url"
  else
    echo "need curl or wget to download $url" >&2
    return 1
  fi
}

conch_require_https() {
  local url="$1"
  if [[ "$url" != https://* ]]; then
    echo "remote release URLs must use HTTPS: $url" >&2
    return 1
  fi
}

conch_verify_github_attestation() {
  local file="$1"
  local version="$2"
  if ! command -v gh >/dev/null 2>&1; then
    echo "GitHub CLI (gh) is required to verify release attestations" >&2
    return 1
  fi
  gh attestation verify "$file" \
    --repo OriginalFunction/Conch \
    --signer-workflow github.com/OriginalFunction/Conch/.github/workflows/release.yml \
    --source-ref "refs/tags/v${version}" \
    --deny-self-hosted-runners >/dev/null
}
