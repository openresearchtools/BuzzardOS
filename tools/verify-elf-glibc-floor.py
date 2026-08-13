#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Reject bundled ELF objects that require glibc newer than the host floor."""

from __future__ import annotations

import argparse
import os
import re
import stat
import subprocess
import sys
from pathlib import Path


ELF_MAGIC = b"\x7fELF"
GLIBC_VERSION = re.compile(r"\bGLIBC_([0-9]+(?:\.[0-9]+)+)\b")
GLIBC_POST_FLOOR_ABI = re.compile(r"\bGLIBC_ABI_DT_RELR\b")
GLIBC_DT_RELR_VERSION = (2, 36)


class VerificationError(RuntimeError):
    """The payload is unsafe or exceeds its declared glibc floor."""


def parse_version(value: str) -> tuple[int, ...]:
    if re.fullmatch(r"[0-9]+(?:\.[0-9]+)+", value) is None:
        raise VerificationError(f"invalid glibc version: {value!r}")
    return tuple(int(component) for component in value.split("."))


def glibc_versions(version_info: str) -> set[tuple[int, ...]]:
    return {parse_version(match.group(1)) for match in GLIBC_VERSION.finditer(version_info)}


def elf_files(root: Path) -> list[Path]:
    if root.is_symlink() or not root.is_dir():
        raise VerificationError(f"payload root must be a real directory: {root}")
    objects: list[Path] = []
    for directory, directory_names, file_names in os.walk(root, followlinks=False):
        directory_names.sort()
        file_names.sort()
        directory_path = Path(directory)
        for name in directory_names:
            path = directory_path / name
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode):
                continue
            if not stat.S_ISDIR(mode):
                raise VerificationError(f"payload contains a non-directory entry: {path}")
        for name in file_names:
            path = directory_path / name
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode):
                continue
            if not stat.S_ISREG(mode):
                raise VerificationError(f"payload contains a special file: {path}")
            with path.open("rb") as source:
                if source.read(4) == ELF_MAGIC:
                    objects.append(path)
    return objects


def version_info(path: Path, readelf: str) -> str:
    result = subprocess.run(
        [readelf, "-W", "--version-info", "--", str(path)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        error = result.stderr.strip()
        raise VerificationError(f"cannot inspect ELF version requirements for {path}: {error}")
    return result.stdout


def verify(root: Path, floor: tuple[int, ...], readelf: str) -> tuple[int, list[str]]:
    failures: list[str] = []
    objects = elf_files(root)
    for path in objects:
        info = version_info(path, readelf)
        if GLIBC_POST_FLOOR_ABI.search(info) and floor < GLIBC_DT_RELR_VERSION:
            failures.append(
                f"{path.relative_to(root).as_posix()}: requires GLIBC_ABI_DT_RELR"
            )
            continue
        versions = glibc_versions(info)
        required = max(versions, default=())
        if required > floor:
            failures.append(
                f"{path.relative_to(root).as_posix()}: requires GLIBC_"
                f"{'.'.join(map(str, required))}"
            )
    return len(objects), failures


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--maximum", required=True)
    parser.add_argument("--readelf", default="readelf")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        root = args.root.resolve(strict=True)
        floor = parse_version(args.maximum)
        count, failures = verify(root, floor, args.readelf)
        if count == 0:
            raise VerificationError("payload contains no ELF objects")
        if failures:
            print(
                f"bundled ELF objects exceed the GLIBC_{args.maximum} host floor:",
                file=sys.stderr,
            )
            for failure in failures:
                print(f"- {failure}", file=sys.stderr)
            return 1
        print(f"verified {count} bundled ELF objects against GLIBC_{args.maximum}")
    except (OSError, VerificationError) as error:
        print(f"ELF glibc-floor verification error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
