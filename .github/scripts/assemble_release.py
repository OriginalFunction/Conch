#!/usr/bin/env python3
"""Verify platform artifacts and assemble GitHub release metadata."""

import argparse
import hashlib
import json
from pathlib import Path
import re


PLATFORMS = ("darwin-arm64", "darwin-amd64", "linux-amd64", "linux-arm64")
SHA_PLACEHOLDERS = {
    "darwin-arm64": "@SHA256_DARWIN_ARM64@",
    "darwin-amd64": "@SHA256_DARWIN_AMD64@",
    "linux-amd64": "@SHA256_LINUX_AMD64@",
    "linux-arm64": "@SHA256_LINUX_ARM64@",
}


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_sidecar(asset: Path) -> str:
    sidecar = asset.with_name(f"{asset.name}.sha256")
    if not sidecar.is_file():
        raise SystemExit(f"missing checksum sidecar: {sidecar}")
    fields = sidecar.read_text(encoding="utf-8").strip().split()
    if len(fields) != 2 or fields[1].lstrip("*") != asset.name:
        raise SystemExit(f"malformed checksum sidecar: {sidecar}")
    actual = sha256(asset)
    if fields[0].lower() != actual:
        raise SystemExit(f"checksum mismatch for {asset.name}")
    return actual


def assemble(dist: Path, template: Path, repo: str, version: str, tag: str, commit: str) -> None:
    expected_tag = f"v{version}"
    if tag != expected_tag:
        raise SystemExit(f"tag {tag!r} must equal {expected_tag!r}")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise SystemExit(f"invalid GitHub repository: {repo!r}")

    expected_asset_names = {
        f"conch-{version}-{platform}.tar.gz" for platform in PLATFORMS
    }
    expected_asset_names.update(
        {f"conch_{version}_amd64.deb", f"conch_{version}_arm64.deb"}
    )
    actual_asset_names = {
        path.name for pattern in ("*.tar.gz", "*.deb") for path in dist.glob(pattern)
    }
    if actual_asset_names != expected_asset_names:
        missing = sorted(expected_asset_names - actual_asset_names)
        unexpected = sorted(actual_asset_names - expected_asset_names)
        raise SystemExit(f"release asset set mismatch; missing={missing}, unexpected={unexpected}")

    assets = []
    tar_hashes = {}
    for platform in PLATFORMS:
        asset = dist / f"conch-{version}-{platform}.tar.gz"
        if not asset.is_file():
            raise SystemExit(f"missing platform archive: {asset}")
        digest = verify_sidecar(asset)
        tar_hashes[platform] = digest
        assets.append(
            {"name": asset.name, "kind": "archive", "platform": platform, "sha256": digest}
        )

    for arch in ("amd64", "arm64"):
        asset = dist / f"conch_{version}_{arch}.deb"
        if not asset.is_file():
            raise SystemExit(f"missing Debian package: {asset}")
        digest = verify_sidecar(asset)
        assets.append(
            {"name": asset.name, "kind": "debian", "platform": f"linux-{arch}", "sha256": digest}
        )

    assets.sort(key=lambda item: item["name"])
    (dist / "SHA256SUMS").write_text(
        "".join(f"{item['sha256']}  {item['name']}\n" for item in assets),
        encoding="utf-8",
    )

    base_url = f"https://github.com/{repo}/releases/download/{tag}"
    formula = template.read_text(encoding="utf-8")
    formula = formula.replace("@VERSION@", version).replace("@BASE_URL@", base_url)
    formula = re.sub(
        r'^  homepage ".*"$', f'  homepage "https://github.com/{repo}"', formula, flags=re.MULTILINE
    )
    for platform, placeholder in SHA_PLACEHOLDERS.items():
        formula = formula.replace(placeholder, tar_hashes[platform])
    if "@" in formula or re.search(r'(?<![0-9a-f])0{64}(?![0-9a-f])', formula):
        raise SystemExit("Homebrew formula still contains an unresolved value")
    (dist / "conch.rb").write_text(formula, encoding="utf-8")

    manifest = {
        "schema_version": 1,
        "name": "conch",
        "version": version,
        "tag": tag,
        "repository": repo,
        "commit": commit,
        "binaries": ["conch", "conchd"],
        "assets": assets,
    }
    (dist / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )

    for sidecar in dist.glob("*.sha256"):
        sidecar.unlink()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--dist", type=Path, required=True)
    parser.add_argument("--formula-template", type=Path, required=True)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    args = parser.parse_args()
    assemble(
        args.dist,
        args.formula_template,
        args.repo,
        args.version,
        args.tag,
        args.commit,
    )


if __name__ == "__main__":
    main()
