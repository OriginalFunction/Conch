#!/usr/bin/env python3
"""Build a byte-stable Conch release archive for one target platform."""

import argparse
import gzip
import hashlib
from pathlib import Path
import tarfile


PLATFORMS = ("darwin-arm64", "darwin-amd64", "linux-amd64", "linux-arm64")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def tar_info(name: str, mode: int, epoch: int, size: int = 0) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.mode = mode
    info.mtime = epoch
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.size = size
    return info


def add_bytes(archive: tarfile.TarFile, name: str, data: bytes, mode: int, epoch: int) -> None:
    import io

    archive.addfile(tar_info(name, mode, epoch, len(data)), io.BytesIO(data))


def package(
    bin_dir: Path,
    readme: Path,
    license_file: Path,
    out_dir: Path,
    version: str,
    platform: str,
    epoch: int,
) -> Path:
    binaries = [bin_dir / "conch", bin_dir / "conchd"]
    for binary in binaries:
        if not binary.is_file():
            raise SystemExit(f"missing release binary: {binary}")
    if not readme.is_file():
        raise SystemExit(f"missing README: {readme}")
    if not license_file.is_file():
        raise SystemExit(f"missing license: {license_file}")

    out_dir.mkdir(parents=True, exist_ok=True)
    root = f"conch-{version}-{platform}"
    output = out_dir / f"{root}.tar.gz"

    # gzip filename and timestamps, tar ownership, ordering, modes, and timestamps
    # are all explicit so two packages of identical inputs are byte-for-byte equal.
    with output.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, compresslevel=9, mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as archive:
                directory = tar_info(root, 0o755, epoch)
                directory.type = tarfile.DIRTYPE
                archive.addfile(directory)
                add_bytes(archive, f"{root}/conch", binaries[0].read_bytes(), 0o755, epoch)
                add_bytes(archive, f"{root}/conchd", binaries[1].read_bytes(), 0o755, epoch)
                add_bytes(archive, f"{root}/README.md", readme.read_bytes(), 0o644, epoch)
                add_bytes(archive, f"{root}/LICENSE", license_file.read_bytes(), 0o644, epoch)

    checksum = sha256(output)
    output.with_name(f"{output.name}.sha256").write_text(
        f"{checksum}  {output.name}\n", encoding="utf-8"
    )
    return output


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--readme", type=Path, required=True)
    parser.add_argument("--license", dest="license_file", type=Path, required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--platform", choices=PLATFORMS, required=True)
    parser.add_argument("--epoch", type=int, required=True)
    args = parser.parse_args()
    if args.epoch < 0:
        parser.error("--epoch must be non-negative")
    output = package(
        args.bin_dir,
        args.readme,
        args.license_file,
        args.out_dir,
        args.version,
        args.platform,
        args.epoch,
    )
    print(output)


if __name__ == "__main__":
    main()
