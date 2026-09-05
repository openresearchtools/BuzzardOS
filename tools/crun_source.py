#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Verify the complete vendored crun source, including pinned submodules."""
import argparse
import hashlib
import os
from pathlib import Path
import re
import stat
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
VENDOR = ROOT / "third-party/crun"


def source_digest(source: Path) -> str:
    """Hash names, executable/symlink modes and bytes in a portable order."""
    digest = hashlib.sha256()
    for path in sorted(source.rglob("*"), key=lambda p: p.relative_to(source).as_posix()):
        mode = path.lstat().st_mode
        if stat.S_ISDIR(mode):
            continue
        name = path.relative_to(source).as_posix().encode()
        if stat.S_ISLNK(mode):
            kind, content = b"120000", os.readlink(path).encode()
        elif stat.S_ISREG(mode):
            kind = b"100755" if mode & 0o111 else b"100644"
            content = path.read_bytes()
        else:
            raise ValueError(f"unexpected source file type: {path}")
        digest.update(kind + b" " + name + b"\0" + hashlib.sha256(content).digest())
    return digest.hexdigest()


def verify(vendor: Path = VENDOR) -> dict:
    record = tomllib.loads((vendor / "UPSTREAM.toml").read_text())
    if record["schema"] != 1 or record["local_patches"]:
        raise ValueError("crun must remain unmodified upstream source")
    for item in record["repository"]:
        if not re.fullmatch(r"[0-9a-f]{40}", item["commit"]):
            raise ValueError("every crun source repository needs an exact commit")
        if not (vendor / "source" / item["path"]).is_dir():
            raise ValueError(f"missing vendored source: {item['path']}")
    if record["commit"] != record["repository"][0]["commit"]:
        raise ValueError("crun release and source commit differ")
    actual = source_digest(vendor / "source")
    if actual != record["source_sha256"]:
        raise ValueError(f"crun source checksum mismatch: {actual}")
    return record


def verify_archive(archive: Path) -> None:
    """Check the shipped corresponding source without extracting or executing it."""
    verify()
    expected = {
        path.relative_to(ROOT).as_posix(): path
        for path in VENDOR.rglob("*") if path.is_symlink() or not path.is_dir()
    }
    for name in ("packaging/build-crun.sh", "tools/crun_source.py",
                 "tools/verify-elf-glibc-floor.py"):
        expected[name] = ROOT / name
    with tarfile.open(archive, "r:gz") as bundle:
        seen = set()
        for member in bundle:
            if member.isdir():
                continue
            if member.name in seen or member.name not in expected:
                raise ValueError(f"unexpected/duplicate crun source member: {member.name}")
            seen.add(member.name)
            source = expected[member.name]
            if source.is_symlink():
                matches = member.issym() and member.linkname == os.readlink(source)
            else:
                content = bundle.extractfile(member) if member.isfile() else None
                matches = (content is not None
                           and content.read() == source.read_bytes()
                           and bool(member.mode & 0o111) == bool(source.stat().st_mode & 0o111))
            if not matches:
                raise ValueError(f"crun corresponding source differs: {member.name}")
        if seen != set(expected):
            raise ValueError("crun corresponding source/build recipe is incomplete")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--print-digest", action="store_true")
    args = parser.parse_args()
    if args.print_digest:
        print(source_digest(VENDOR / "source"))
    else:
        record = verify()
        print(f"Verified crun {record['version']} at {record['commit']}")
