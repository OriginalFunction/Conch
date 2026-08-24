#!/usr/bin/env python3
"""Unit tests for deterministic GitHub release packaging."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
PACKAGE = ROOT / ".github" / "scripts" / "package_release.py"
ASSEMBLE = ROOT / ".github" / "scripts" / "assemble_release.py"
FORMULA = ROOT / "packaging" / "homebrew" / "conch.rb.in"
VERSION = "9.8.7"
EPOCH = 1_700_000_000
PLATFORMS = ("darwin-arm64", "darwin-amd64", "linux-amd64", "linux-arm64")


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_sidecar(asset: Path) -> None:
    asset.with_name(f"{asset.name}.sha256").write_text(
        f"{digest(asset)}  {asset.name}\n", encoding="utf-8"
    )


class ReleaseScriptsTest(unittest.TestCase):
    def populate_assets(self, dist: Path) -> list[str]:
        names = []
        for platform in PLATFORMS:
            asset = dist / f"conch-{VERSION}-{platform}.tar.gz"
            asset.write_bytes(f"archive:{platform}\n".encode())
            write_sidecar(asset)
            names.append(asset.name)
        for arch in ("amd64", "arm64"):
            asset = dist / f"conch_{VERSION}_{arch}.deb"
            asset.write_bytes(f"debian:{arch}\n".encode())
            write_sidecar(asset)
            names.append(asset.name)
        return names

    def test_archive_is_deterministic_and_canonical(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            work = Path(temp)
            bin_dir = work / "bin"
            bin_dir.mkdir()
            (bin_dir / "conch").write_bytes(b"conch executable\n")
            (bin_dir / "conchd").write_bytes(b"conchd executable\n")
            readme = work / "README.md"
            readme.write_text("# Conch\n", encoding="utf-8")
            license_file = work / "LICENSE"
            license_file.write_text("MIT License\n", encoding="utf-8")

            outputs = []
            for index in range(2):
                out = work / f"out-{index}"
                subprocess.run(
                    [
                        sys.executable,
                        str(PACKAGE),
                        "--bin-dir",
                        str(bin_dir),
                        "--readme",
                        str(readme),
                        "--license",
                        str(license_file),
                        "--out-dir",
                        str(out),
                        "--version",
                        VERSION,
                        "--platform",
                        "linux-amd64",
                        "--epoch",
                        str(EPOCH),
                    ],
                    check=True,
                )
                outputs.append(out / f"conch-{VERSION}-linux-amd64.tar.gz")

            self.assertEqual(outputs[0].read_bytes(), outputs[1].read_bytes())
            root = f"conch-{VERSION}-linux-amd64"
            with tarfile.open(outputs[0], "r:gz") as archive:
                self.assertEqual(
                    archive.getnames(),
                    [
                        root,
                        f"{root}/conch",
                        f"{root}/conchd",
                        f"{root}/README.md",
                        f"{root}/LICENSE",
                    ],
                )
                for member in archive.getmembers():
                    self.assertEqual(member.mtime, EPOCH)
                    self.assertEqual(member.uid, 0)
                    self.assertEqual(member.gid, 0)
                self.assertEqual(archive.getmember(f"{root}/conch").mode, 0o755)
                self.assertEqual(archive.getmember(f"{root}/README.md").mode, 0o644)
                self.assertEqual(archive.getmember(f"{root}/LICENSE").mode, 0o644)
                self.assertEqual(archive.extractfile(f"{root}/conch").read(), b"conch executable\n")
                self.assertEqual(archive.extractfile(f"{root}/LICENSE").read(), b"MIT License\n")

    def test_assembler_verifies_and_fills_every_platform(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            dist = Path(temp)
            expected_names = self.populate_assets(dist)

            subprocess.run(
                [
                    sys.executable,
                    str(ASSEMBLE),
                    "--dist",
                    str(dist),
                    "--formula-template",
                    str(FORMULA),
                    "--repo",
                    "example/conch",
                    "--version",
                    VERSION,
                    "--tag",
                    f"v{VERSION}",
                    "--commit",
                    "0123456789abcdef",
                ],
                check=True,
            )

            sums = (dist / "SHA256SUMS").read_text(encoding="utf-8").splitlines()
            self.assertEqual([line.split(None, 1)[1] for line in sums], sorted(expected_names))
            formula = (dist / "conch.rb").read_text(encoding="utf-8")
            self.assertNotIn("@", formula)
            self.assertNotIn("0" * 64, formula)
            self.assertIn("https://github.com/example/conch", formula)
            for platform in PLATFORMS:
                self.assertIn(digest(dist / f"conch-{VERSION}-{platform}.tar.gz"), formula)

            manifest = json.loads((dist / "manifest.json").read_text(encoding="utf-8"))
            self.assertEqual(manifest["repository"], "example/conch")
            self.assertEqual(manifest["tag"], f"v{VERSION}")
            self.assertEqual(manifest["commit"], "0123456789abcdef")
            self.assertEqual([item["name"] for item in manifest["assets"]], sorted(expected_names))
            self.assertEqual(list(dist.glob("*.sha256")), [])

    def test_assembler_rejects_a_corrupt_platform_asset(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            dist = Path(temp)
            self.populate_assets(dist)
            corrupt = dist / f"conch-{VERSION}-darwin-arm64.tar.gz"
            corrupt.write_bytes(b"modified after checksum\n")
            result = subprocess.run(
                [
                    sys.executable,
                    str(ASSEMBLE),
                    "--dist",
                    str(dist),
                    "--formula-template",
                    str(FORMULA),
                    "--repo",
                    "example/conch",
                    "--version",
                    VERSION,
                    "--tag",
                    f"v{VERSION}",
                    "--commit",
                    "0123456789abcdef",
                ],
                capture_output=True,
                text=True,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("checksum mismatch", result.stderr)


if __name__ == "__main__":
    unittest.main()
