#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Create and verify Wild Buzzard release payload metadata."""

from __future__ import annotations

import argparse
import errno
import hashlib
import json
import os
import re
import shutil
import stat
import struct
import subprocess
import sys
import tarfile
from pathlib import Path, PurePosixPath
from typing import BinaryIO, Iterable


ROOT = Path(__file__).resolve().parent.parent
SHA256_DIGEST = re.compile(r"^sha256:([0-9a-f]{64})$")
GIT_COMMIT = re.compile(r"^[0-9a-f]{40}$")
ROOTFS_ARCHIVE_NAME = "WildBuzzard-rootfs-linux-x86_64.tar.zst"
ROOTFS_MANIFEST_NAME = "WildBuzzard-rootfs-linux-x86_64.json"
APPIMAGE_NAME = "WildBuzzard-x86_64.AppImage"
BUNDLE_MANIFEST = PurePosixPath("provenance/bundle-files.json")
BUNDLE_CHECKSUMS = PurePosixPath("SHA256SUMS")
SECURITY_CAPABILITY_XATTR = "security.capability"
VFS_CAP_REVISION_MASK = 0xFF000000
VFS_CAP_REVISION_2 = 0x02000000
VFS_CAP_REVISION_3 = 0x03000000
VFS_CAP_FLAGS_EFFECTIVE = 0x000001


class MetadataError(RuntimeError):
    """A release input or its provenance is invalid."""


class HashingReader:
    """Record the digest and byte count of a sequential binary stream."""

    def __init__(self, source: BinaryIO) -> None:
        self.source = source
        self.digest = hashlib.sha256()
        self.size = 0

    def read(self, size: int = -1) -> bytes:
        block = self.source.read(size)
        self.digest.update(block)
        self.size += len(block)
        return block


def sha256_stream(source: BinaryIO) -> tuple[str, int]:
    digest = hashlib.sha256()
    size = 0
    for block in iter(lambda: source.read(1024 * 1024), b""):
        digest.update(block)
        size += len(block)
    return digest.hexdigest(), size


def sha256_file(path: Path) -> str:
    with path.open("rb") as source:
        return sha256_stream(source)[0]


def read_json(path: Path) -> object:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise MetadataError(f"cannot read JSON {path}: {error}") from error


def write_json_atomic(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def require_regular(path: Path, description: str) -> os.stat_result:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise MetadataError(f"cannot inspect {description} {path}: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise MetadataError(f"{description} must be a regular non-symlink file: {path}")
    return metadata


def normalize_tar_name(raw: str, description: str) -> str:
    # Tar member names use POSIX path semantics. A backslash is a legal literal
    # filename byte on Linux (and appears in systemd's escaped unit names); it
    # is not a separator and therefore is not a traversal signal here.
    if not raw or "\0" in raw or "\n" in raw or "\r" in raw:
        raise MetadataError(f"{description} has an unsafe name: {raw!r}")
    path = PurePosixPath(raw)
    if path.is_absolute() or ".." in path.parts:
        raise MetadataError(f"{description} escapes its archive root: {raw!r}")
    parts = tuple(part for part in path.parts if part not in ("", "."))
    return "." if not parts else "/".join(parts)


def descriptor_blob(
    layout: Path, descriptor: dict[str, object]
) -> tuple[Path, bytes]:
    digest = descriptor.get("digest")
    size = descriptor.get("size")
    if not isinstance(digest, str) or (match := SHA256_DIGEST.fullmatch(digest)) is None:
        raise MetadataError("OCI descriptor has an invalid sha256 digest")
    if not isinstance(size, int) or size < 0:
        raise MetadataError("OCI descriptor has an invalid size")
    path = layout / "blobs" / "sha256" / match.group(1)
    metadata = require_regular(path, "OCI blob")
    resolved = path.resolve(strict=True)
    try:
        resolved.relative_to(layout)
    except ValueError as error:
        raise MetadataError(f"OCI blob escapes its layout: {path}") from error
    if metadata.st_size != size:
        raise MetadataError(f"OCI blob size mismatch for {digest}")
    contents = path.read_bytes()
    if hashlib.sha256(contents).hexdigest() != match.group(1):
        raise MetadataError(f"OCI blob digest mismatch for {digest}")
    return path, contents


def inspect_oci_layout(layout: Path) -> dict[str, object]:
    layout = layout.resolve(strict=True)
    require_regular(layout / "oci-layout", "OCI layout marker")
    require_regular(layout / "index.json", "OCI index")
    if read_json(layout / "oci-layout") != {"imageLayoutVersion": "1.0.0"}:
        raise MetadataError("OCI layout marker is not version 1.0.0")
    index = read_json(layout / "index.json")
    if not isinstance(index, dict) or index.get("schemaVersion") != 2:
        raise MetadataError("OCI index must use schemaVersion 2")
    manifests = index.get("manifests")
    if not isinstance(manifests, list) or len(manifests) != 1:
        raise MetadataError("OCI layout must contain exactly one platform manifest")
    descriptor = manifests[0]
    if not isinstance(descriptor, dict):
        raise MetadataError("OCI manifest descriptor is invalid")
    _, manifest_bytes = descriptor_blob(layout, descriptor)
    try:
        manifest = json.loads(manifest_bytes)
    except json.JSONDecodeError as error:
        raise MetadataError(f"OCI manifest is invalid JSON: {error}") from error
    if not isinstance(manifest, dict) or manifest.get("schemaVersion") != 2:
        raise MetadataError("OCI manifest must use schemaVersion 2")
    config_descriptor = manifest.get("config")
    layers = manifest.get("layers")
    if not isinstance(config_descriptor, dict):
        raise MetadataError("OCI manifest has no config descriptor")
    if not isinstance(layers, list) or not layers:
        raise MetadataError("OCI manifest has no filesystem layers")
    _, config_bytes = descriptor_blob(layout, config_descriptor)
    try:
        config = json.loads(config_bytes)
    except json.JSONDecodeError as error:
        raise MetadataError(f"OCI config is invalid JSON: {error}") from error
    if not isinstance(config, dict):
        raise MetadataError("OCI config is not an object")
    if config.get("os") != "linux" or config.get("architecture") != "amd64":
        raise MetadataError("release OCI config is not linux/amd64")
    layer_records: list[dict[str, object]] = []
    for layer in layers:
        if not isinstance(layer, dict):
            raise MetadataError("OCI layer descriptor is invalid")
        descriptor_blob(layout, layer)
        layer_records.append(
            {
                "digest": layer["digest"],
                "size": layer["size"],
                "media_type": layer.get("mediaType"),
            }
        )
    return {
        "manifest_digest": descriptor["digest"],
        "manifest_size": descriptor["size"],
        "config_digest": config_descriptor["digest"],
        "config_size": config_descriptor["size"],
        "layers": layer_records,
    }


def dpkg_inventory(rootfs: Path) -> tuple[bytes, int]:
    status_path = rootfs / "var/lib/dpkg/status"
    text = status_path.read_text(encoding="utf-8", errors="replace")
    records: list[dict[str, str]] = []
    current: dict[str, str] = {}
    for line in text.splitlines() + [""]:
        if not line:
            if current.get("Package") and current.get("Status", "").endswith(" installed"):
                records.append(current)
            current = {}
        elif line[0].isspace():
            continue
        else:
            key, separator, value = line.partition(": ")
            if separator:
                current[key] = value
    rows: list[tuple[str, str]] = []
    for package in records:
        name = package["Package"]
        if package.get("Multi-Arch") == "same":
            architecture = package.get("Architecture")
            if not architecture:
                raise MetadataError(f"Multi-Arch package has no architecture: {name}")
            name = f"{name}:{architecture}"
        version = package.get("Version")
        if not version:
            raise MetadataError(f"installed package has no version: {name}")
        rows.append((name, version))
    rows.sort()
    if len({name for name, _ in rows}) != len(rows):
        raise MetadataError("dpkg inventory contains duplicate binary package names")
    raw = "".join(f"{name}\t{version}\n" for name, version in rows).encode()
    return raw, len(rows)


def filesystem_paths(root: Path) -> Iterable[Path]:
    yield root
    for directory, directory_names, file_names in os.walk(root, followlinks=False):
        directory_names.sort()
        file_names.sort()
        for name in [*directory_names, *file_names]:
            yield Path(directory) / name


def materialize_tree(source: Path, destination: Path) -> None:
    """Copy a notice tree without carrying links, xattrs, owners, or specials."""

    if source.is_symlink() or not source.is_dir():
        raise MetadataError(f"materialization source is not a real directory: {source}")
    source_root = source.resolve(strict=True)
    if destination.is_symlink() or not destination.is_dir():
        raise MetadataError(
            f"materialization destination is not a real directory: {destination}"
        )
    if any(destination.iterdir()):
        raise MetadataError(f"materialization destination is not empty: {destination}")

    def copy_entry(path: Path, output: Path, directory_stack: set[Path]) -> None:
        metadata = path.lstat()
        effective = path
        if stat.S_ISLNK(metadata.st_mode):
            target = os.readlink(path)
            if not target or os.path.isabs(target):
                raise MetadataError(f"notice tree contains an absolute/empty symlink: {path}")
            try:
                effective = (path.parent / target).resolve(strict=True)
            except (OSError, RuntimeError) as error:
                raise MetadataError(f"notice symlink is broken or cyclic: {path}") from error
            try:
                effective.relative_to(source_root)
            except ValueError as error:
                raise MetadataError(f"notice symlink escapes its evidence group: {path}") from error
            metadata = effective.lstat()
        if stat.S_ISDIR(metadata.st_mode):
            resolved = effective.resolve(strict=True)
            if resolved in directory_stack:
                raise MetadataError(f"notice tree contains a directory symlink cycle: {path}")
            output.mkdir(mode=stat.S_IMODE(metadata.st_mode))
            for child in sorted(effective.iterdir(), key=lambda item: os.fsencode(item.name)):
                copy_entry(child, output / child.name, directory_stack | {resolved})
        elif stat.S_ISREG(metadata.st_mode):
            with effective.open("rb") as input_file, output.open("xb") as output_file:
                shutil.copyfileobj(input_file, output_file, length=1024 * 1024)
            output.chmod(stat.S_IMODE(metadata.st_mode))
        else:
            raise MetadataError(f"notice tree contains a socket/device/FIFO: {path}")

    for entry in sorted(source_root.iterdir(), key=lambda item: os.fsencode(item.name)):
        copy_entry(entry, destination / entry.name, {source_root})


def materialize_command(args: argparse.Namespace) -> None:
    materialize_tree(args.source, args.destination)


def guest_id_to_host(guest_id: int, host_id: int, subordinate_start: int) -> int:
    if not 0 <= guest_id <= 65_535:
        raise MetadataError(f"guest ID is outside the supported map: {guest_id}")
    if guest_id < 1000:
        return subordinate_start + guest_id
    if guest_id == 1000:
        return host_id
    return subordinate_start + guest_id - 1


def parse_file_capability(
    value: bytes, description: str
) -> tuple[int, int, int, bool, int | None]:
    """Decode the Linux VFS v2/v3 security.capability wire format."""

    if len(value) < 4:
        raise MetadataError(f"{description} file capability is shorter than its header")
    (magic_etc,) = struct.unpack_from("<I", value)
    revision = magic_etc & VFS_CAP_REVISION_MASK
    flags = magic_etc & ~VFS_CAP_REVISION_MASK
    if flags & ~VFS_CAP_FLAGS_EFFECTIVE:
        raise MetadataError(
            f"{description} file capability has unsupported flags 0x{flags:06x}"
        )
    if revision == VFS_CAP_REVISION_2:
        expected_size = 20
    elif revision == VFS_CAP_REVISION_3:
        expected_size = 24
    else:
        raise MetadataError(
            f"{description} file capability has unsupported revision "
            f"0x{revision:08x}"
        )
    if len(value) != expected_size:
        raise MetadataError(
            f"{description} file capability revision "
            f"{revision >> 24} has size {len(value)}, expected {expected_size}"
        )

    _magic, permitted_low, inheritable_low, permitted_high, inheritable_high = (
        struct.unpack_from("<IIIII", value)
    )
    permitted = permitted_low | (permitted_high << 32)
    inheritable = inheritable_low | (inheritable_high << 32)
    root_id = struct.unpack_from("<I", value, 20)[0] if expected_size == 24 else None
    return (
        revision,
        permitted,
        inheritable,
        bool(flags & VFS_CAP_FLAGS_EFFECTIVE),
        root_id,
    )


def read_file_capability(path: Path, description: str) -> bytes | None:
    try:
        return os.getxattr(path, SECURITY_CAPABILITY_XATTR, follow_symlinks=False)
    except OSError as error:
        if error.errno == errno.ENODATA:
            return None
        raise MetadataError(
            f"cannot inspect {description} file capability at {path}: {error}"
        ) from error


def verify_idmapped_file_capability(
    relative: str,
    canonical_value: bytes | None,
    mapped_value: bytes | None,
    expected_root_id: int,
) -> None:
    if canonical_value is None and mapped_value is None:
        return
    if canonical_value is None or mapped_value is None:
        missing = "canonical" if canonical_value is None else "mapped"
        raise MetadataError(
            f"ID-mapped rootfs is missing {missing} file capability at {relative}"
        )

    canonical = parse_file_capability(
        canonical_value, f"canonical rootfs {relative}"
    )
    mapped = parse_file_capability(mapped_value, f"mapped rootfs {relative}")
    if canonical[0] != VFS_CAP_REVISION_2 or canonical[4] is not None:
        raise MetadataError(
            f"canonical rootfs file capability at {relative} is not VFS revision 2"
        )
    if mapped[0] != VFS_CAP_REVISION_3 or mapped[4] is None:
        raise MetadataError(
            f"mapped rootfs file capability at {relative} is not namespaced VFS revision 3"
        )
    if mapped[4] != expected_root_id:
        raise MetadataError(
            f"mapped rootfs file capability at {relative} has namespace root ID "
            f"{mapped[4]}, expected {expected_root_id}"
        )
    if canonical[1:4] != mapped[1:4]:
        raise MetadataError(
            f"ID-mapped rootfs file capability masks or flags differ at {relative}"
        )


def verify_idmapped_copy(args: argparse.Namespace) -> None:
    for value, description in [
        (args.subuid_start, "subordinate UID start"),
        (args.subgid_start, "subordinate GID start"),
    ]:
        if not 0 <= value <= 0xFFFFFFFF:
            raise MetadataError(f"{description} is outside the Linux ID range: {value}")
    for root, description in [
        (args.canonical, "canonical rootfs"),
        (args.mapped, "mapped rootfs"),
    ]:
        if root.is_symlink() or not root.is_dir():
            raise MetadataError(f"{description} is not a real directory: {root}")
    canonical = args.canonical.resolve(strict=True)
    mapped = args.mapped.resolve(strict=True)

    def records(root: Path) -> dict[str, os.stat_result]:
        return {
            path.relative_to(root).as_posix(): path.lstat()
            for path in filesystem_paths(root)
        }

    canonical_records = records(canonical)
    mapped_records = records(mapped)
    if canonical_records.keys() != mapped_records.keys():
        raise MetadataError("ID-mapped rootfs has a different path inventory")
    for relative, source in canonical_records.items():
        destination = mapped_records[relative]
        expected_uid = guest_id_to_host(source.st_uid, args.host_uid, args.subuid_start)
        expected_gid = guest_id_to_host(source.st_gid, args.host_gid, args.subgid_start)
        if (destination.st_uid, destination.st_gid) != (expected_uid, expected_gid):
            raise MetadataError(
                "ID-mapped ownership differs at "
                f"{relative}: expected {expected_uid}:{expected_gid}, "
                f"found {destination.st_uid}:{destination.st_gid}"
            )
        canonical_path = canonical if relative == "." else canonical / relative
        mapped_path = mapped if relative == "." else mapped / relative
        verify_idmapped_file_capability(
            relative,
            read_file_capability(canonical_path, "canonical rootfs"),
            read_file_capability(mapped_path, "mapped rootfs"),
            args.subuid_start,
        )


def inspect_rootfs(rootfs: Path) -> dict[str, object]:
    rootfs = rootfs.resolve(strict=True)
    root_metadata = rootfs.lstat()
    if not stat.S_ISDIR(root_metadata.st_mode):
        raise MetadataError("rootfs is not a directory")
    if (root_metadata.st_uid, root_metadata.st_gid) != (0, 0):
        raise MetadataError("canonical rootfs root must be owned by numeric 0:0")
    required = [
        "lib/systemd/systemd",
        "usr/libexec/wildbuzzard-init",
        "usr/bin/sway",
        "usr/bin/wildbuzzard-shell",
        "usr/bin/wildbuzzard-cua-driver",
        "var/lib/dpkg/status",
    ]
    missing = [relative for relative in required if not (rootfs / relative).is_file()]
    if missing:
        raise MetadataError(f"rootfs is missing required files: {', '.join(missing)}")

    counts = {"directories": 0, "regular_files": 0, "symlinks": 0, "other": 0}
    regular_bytes = 0
    xattrs = 0
    capabilities = 0
    hardlink_inodes: dict[tuple[int, int], int] = {}
    owners: set[tuple[int, int]] = set()
    for path in filesystem_paths(rootfs):
        metadata = path.lstat()
        owners.add((metadata.st_uid, metadata.st_gid))
        if metadata.st_uid > 65535 or metadata.st_gid > 65535:
            raise MetadataError(
                f"rootfs contains non-canonical subordinate ownership at {path}: "
                f"{metadata.st_uid}:{metadata.st_gid}"
            )
        if path != rootfs:
            mode = metadata.st_mode
            if stat.S_ISDIR(mode):
                counts["directories"] += 1
            elif stat.S_ISREG(mode):
                counts["regular_files"] += 1
                regular_bytes += metadata.st_size
                if metadata.st_nlink > 1:
                    key = (metadata.st_dev, metadata.st_ino)
                    hardlink_inodes[key] = hardlink_inodes.get(key, 0) + 1
            elif stat.S_ISLNK(mode):
                counts["symlinks"] += 1
            else:
                counts["other"] += 1
                raise MetadataError(f"rootfs contains a socket/device/FIFO: {path}")
            if path.name.startswith(".wh."):
                raise MetadataError(f"flattened rootfs retains an OCI whiteout: {path}")
        try:
            names = os.listxattr(path, follow_symlinks=False)
        except OSError as error:
            raise MetadataError(f"cannot inspect xattrs on {path}: {error}") from error
        xattrs += len(names)
        capabilities += int("security.capability" in names)
    inventory, package_count = dpkg_inventory(rootfs)
    return {
        "counts": counts,
        "regular_file_bytes": regular_bytes,
        "hardlink_groups": sum(1 for count in hardlink_inodes.values() if count > 1),
        "xattr_count": xattrs,
        "file_capability_count": capabilities,
        "owners": [[uid, gid] for uid, gid in sorted(owners)],
        "package_count": package_count,
        "package_inventory_sha256": hashlib.sha256(inventory).hexdigest(),
    }


def inspect_tar_stream(source: BinaryIO, description: str) -> dict[str, object]:
    records: dict[str, tuple[str, int, str | None]] = {}
    owners: set[tuple[int, int]] = set()
    xattr_count = 0
    capability_count = 0
    with tarfile.open(fileobj=source, mode="r|") as archive:
        for member in archive:
            name = normalize_tar_name(member.name, description)
            if name in records:
                raise MetadataError(f"{description} repeats an archive path: {name}")
            if member.uid < 0 or member.gid < 0 or member.uid > 65535 or member.gid > 65535:
                raise MetadataError(
                    f"{description} has non-canonical owner {member.uid}:{member.gid}: {name}"
                )
            owners.add((member.uid, member.gid))
            if PurePosixPath(name).name.startswith(".wh."):
                raise MetadataError(f"{description} retains an OCI whiteout: {name}")
            xattrs = [key for key in member.pax_headers if key.startswith("SCHILY.xattr.")]
            xattr_count += len(xattrs)
            capability_count += int("SCHILY.xattr.security.capability" in xattrs)
            if member.isdir():
                kind, size, target = "directory", 0, None
            elif member.isreg():
                kind, size, target = "file", member.size, None
            elif member.issym():
                kind, size, target = "symlink", 0, member.linkname
            elif member.islnk():
                kind = "hardlink"
                size = 0
                target = normalize_tar_name(member.linkname, f"{description} hardlink")
            else:
                raise MetadataError(f"{description} contains a socket/device/FIFO: {name}")
            records[name] = (kind, size, target)
    if records.get(".", (None, 0, None))[0] != "directory":
        raise MetadataError(f"{description} does not contain a root directory entry")

    def file_size(name: str, seen: set[str]) -> int:
        if name in seen:
            raise MetadataError(f"{description} contains a hardlink cycle at {name}")
        record = records.get(name)
        if record is None:
            raise MetadataError(f"{description} hardlink target is missing: {name}")
        kind, size, target = record
        if kind == "file":
            return size
        if kind != "hardlink" or target is None:
            raise MetadataError(f"{description} hardlink targets a non-file: {name}")
        return file_size(target, seen | {name})

    counts = {"directories": 0, "regular_files": 0, "symlinks": 0, "other": 0}
    regular_bytes = 0
    link_roots: dict[str, int] = {}
    for name, (kind, size, target) in records.items():
        if name == ".":
            continue
        if kind == "directory":
            counts["directories"] += 1
        elif kind in {"file", "hardlink"}:
            counts["regular_files"] += 1
            regular_bytes += size if kind == "file" else file_size(name, set())
            root = name
            while records[root][0] == "hardlink":
                next_target = records[root][2]
                assert next_target is not None
                root = next_target
            link_roots[root] = link_roots.get(root, 0) + 1
        elif kind == "symlink":
            counts["symlinks"] += 1
    return {
        "counts": counts,
        "regular_file_bytes": regular_bytes,
        "hardlink_groups": sum(1 for count in link_roots.values() if count > 1),
        "xattr_count": xattr_count,
        "file_capability_count": capability_count,
        "owners": [[uid, gid] for uid, gid in sorted(owners)],
    }


def inspect_zstd_archive(archive: Path) -> dict[str, object]:
    archive = archive.resolve(strict=True)
    metadata = require_regular(archive, "rootfs archive")
    with archive.open("rb") as source:
        if source.read(4) != b"\x28\xb5\x2f\xfd":
            raise MetadataError("rootfs archive is not a Zstandard frame")
    process = subprocess.Popen(
        ["zstd", "-T0", "-dc", "--", str(archive)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert process.stdout is not None
    assert process.stderr is not None
    reader = HashingReader(process.stdout)
    try:
        tree = inspect_tar_stream(reader, "rootfs archive")
        while reader.read(1024 * 1024):
            pass
    except Exception:
        process.terminate()
        process.stdout.close()
        process.stderr.read()
        process.stderr.close()
        process.wait()
        raise
    process.stdout.close()
    error = process.stderr.read().decode("utf-8", errors="replace").strip()
    process.stderr.close()
    status = process.wait()
    if status != 0:
        raise MetadataError(f"zstd decompression failed with status {status}: {error}")
    return {
        "name": archive.name,
        "media_type": "application/vnd.wildbuzzard.rootfs.v1.tar+zstd",
        "size": metadata.st_size,
        "sha256": sha256_file(archive),
        "uncompressed_size": reader.size,
        "uncompressed_sha256": reader.digest.hexdigest(),
        "compression": {"codec": "zstd", "level": 19, "long_window_log": 27},
        "tree": tree,
    }


def file_record(path: Path) -> dict[str, object]:
    return {
        "path": path.relative_to(ROOT).as_posix(),
        "sha256": sha256_file(path),
        "size": path.stat().st_size,
    }


def inspect_source_evidence(directory: Path, expected_commit: str) -> dict[str, object]:
    if not GIT_COMMIT.fullmatch(expected_commit):
        raise MetadataError("source commit must be a 40-character lowercase Git object id")
    if directory.is_symlink() or not directory.is_dir():
        raise MetadataError(f"project source evidence is not a real directory: {directory}")
    provenance_path = directory / "source-provenance.json"
    checksums_path = directory / "SHA256SUMS"
    require_regular(provenance_path, "project source provenance")
    require_regular(checksums_path, "project source checksum")
    provenance = read_json(provenance_path)
    if not isinstance(provenance, dict) or provenance.get("schema") != 1:
        raise MetadataError("project source provenance schema is unsupported")
    if provenance.get("commit") != expected_commit:
        raise MetadataError("project source provenance commit differs from the build")
    archive_record = provenance.get("archive")
    if not isinstance(archive_record, dict):
        raise MetadataError("project source provenance has no archive record")
    archive_name = archive_record.get("name")
    expected_name = f"BuzzardOS-source-{expected_commit}.tar.zst"
    if archive_name != expected_name:
        raise MetadataError("project source archive has an unexpected name")
    archive_path = directory / expected_name
    metadata = require_regular(archive_path, "project source archive")
    entries = sorted(path.name for path in directory.iterdir())
    if entries != sorted([expected_name, "SHA256SUMS", "source-provenance.json"]):
        raise MetadataError("project source evidence contains missing or extra entries")
    digest = sha256_file(archive_path)
    if archive_record.get("sha256") != digest or archive_record.get("size") != metadata.st_size:
        raise MetadataError("project source archive differs from its provenance")
    expected_checksums = f"{digest}  {expected_name}\n".encode()
    if checksums_path.read_bytes() != expected_checksums:
        raise MetadataError("project source SHA256SUMS is not exact")
    with archive_path.open("rb") as source:
        if source.read(4) != b"\x28\xb5\x2f\xfd":
            raise MetadataError("project source archive is not Zstandard")
    decompressor = subprocess.Popen(
        ["zstd", "-T0", "-dc", "--", str(archive_path)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert decompressor.stdout is not None
    assert decompressor.stderr is not None
    uncompressed_sha256, uncompressed_size = sha256_stream(decompressor.stdout)
    decompressor.stdout.close()
    decompression_error = decompressor.stderr.read().decode("utf-8", errors="replace").strip()
    decompressor.stderr.close()
    if decompressor.wait() != 0:
        raise MetadataError(
            f"project source archive failed Zstandard verification: {decompression_error}"
        )
    if (
        archive_record.get("uncompressed_sha256") != uncompressed_sha256
        or archive_record.get("uncompressed_size") != uncompressed_size
    ):
        raise MetadataError("project source tar stream differs from its provenance")

    expected_archive = subprocess.Popen(
        [
            "git",
            "-C",
            str(ROOT),
            "archive",
            "--format=tar",
            f"--prefix=BuzzardOS-{expected_commit}/",
            expected_commit,
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert expected_archive.stdout is not None
    assert expected_archive.stderr is not None
    expected_sha256, expected_size = sha256_stream(expected_archive.stdout)
    expected_archive.stdout.close()
    git_error = expected_archive.stderr.read().decode("utf-8", errors="replace").strip()
    expected_archive.stderr.close()
    if expected_archive.wait() != 0:
        raise MetadataError(f"cannot reconstruct exact project source: {git_error}")
    if (expected_sha256, expected_size) != (uncompressed_sha256, uncompressed_size):
        raise MetadataError("project source archive is not the exact Git commit archive")
    return {
        "archive": expected_name,
        "sha256": digest,
        "size": metadata.st_size,
        "uncompressed_sha256": uncompressed_sha256,
        "uncompressed_size": uncompressed_size,
        "source_date_epoch": provenance.get("source_date_epoch"),
        "repository": provenance.get("repository"),
    }


def create_rootfs_manifest(args: argparse.Namespace) -> None:
    rootfs = args.rootfs.resolve(strict=True)
    archive = args.archive.resolve(strict=True)
    layout = args.oci_layout.resolve(strict=True)
    output = args.output.resolve()
    pins = [
        ROOT / "oci/base-images.lock.toml",
        ROOT / "oci/desktop/SWAY_UPSTREAM.toml",
        ROOT / "guest/third_party/trycua-cua/UPSTREAM.toml",
        ROOT / "guest/third_party/trycua-cua/CHANGES.WILDBUZZARD.md",
        ROOT / "LICENSES/release-components.toml",
        ROOT / "LICENSES/generated/oci-packages.tsv",
    ]
    for path in pins:
        require_regular(path, "required provenance file")
    expected_inventory = (ROOT / "LICENSES/generated/oci-packages.tsv").read_bytes()
    inventory, package_count = dpkg_inventory(rootfs)
    if inventory != expected_inventory:
        raise MetadataError("rootfs package inventory differs from release evidence")
    source_commit = args.source_commit
    if not GIT_COMMIT.fullmatch(source_commit):
        raise MetadataError("source commit must be a 40-character lowercase Git object id")
    archive_record = inspect_zstd_archive(archive)
    rootfs_record = inspect_rootfs(rootfs)
    for field in [
        "counts",
        "regular_file_bytes",
        "hardlink_groups",
        "file_capability_count",
        "owners",
    ]:
        if archive_record["tree"][field] != rootfs_record[field]:
            raise MetadataError(f"flat rootfs archive metadata differs from rootfs: {field}")
    record = {
        "schema": 1,
        "kind": "wildbuzzard-flat-rootfs",
        "platform": {"os": "linux", "architecture": "amd64"},
        "source": {
            "repository": "https://github.com/openresearchtools/BuzzardOS",
            "commit": source_commit,
        },
        "archive": archive_record,
        "rootfs": rootfs_record,
        "source_oci": inspect_oci_layout(layout),
        "provenance_files": [file_record(path) for path in pins],
    }
    if rootfs_record["package_count"] != package_count:
        raise MetadataError("rootfs package count changed during manifest generation")
    write_json_atomic(output, record)


def verify_source_command(args: argparse.Namespace) -> None:
    inspect_source_evidence(args.directory.resolve(strict=True), args.source_commit)


def verify_rootfs_manifest(args: argparse.Namespace) -> None:
    manifest = read_json(args.manifest.resolve(strict=True))
    if (
        not isinstance(manifest, dict)
        or manifest.get("schema") != 1
        or manifest.get("kind") != "wildbuzzard-flat-rootfs"
        or manifest.get("platform") != {"os": "linux", "architecture": "amd64"}
    ):
        raise MetadataError("rootfs manifest identity or schema is unsupported")
    archive = args.archive.resolve(strict=True)
    expected = manifest.get("archive")
    if not isinstance(expected, dict):
        raise MetadataError("rootfs manifest has no archive record")
    actual = inspect_zstd_archive(archive)
    if actual != expected:
        raise MetadataError("rootfs archive differs from its complete manifest record")


def create_appimage_manifest(args: argparse.Namespace) -> None:
    appimage = args.appimage.resolve(strict=True)
    appdir = args.appdir.resolve(strict=True)
    metadata = require_regular(appimage, "AppImage")
    if metadata.st_mode & 0o111 == 0:
        raise MetadataError("AppImage is not executable")
    if not (appdir / "AppRun").is_file():
        raise MetadataError("extracted AppImage has no AppRun")
    source = inspect_source_evidence(
        appdir / "usr/share/doc/wildbuzzard/sources/project", args.source_commit
    )
    notices = sum(
        1 for path in (appdir / "usr/share/doc").glob("*/copyright") if path.is_file()
    )
    write_json_atomic(
        args.output.resolve(),
        {
            "schema": 1,
            "kind": "wildbuzzard-appimage",
            "platform": {"os": "linux", "architecture": "amd64"},
            "source": {
                "repository": "https://github.com/openresearchtools/BuzzardOS",
                "commit": args.source_commit,
                "corresponding_source": source,
            },
            "artifact": {
                "name": APPIMAGE_NAME,
                "size": metadata.st_size,
                "sha256": sha256_file(appimage),
            },
            "embedded_package_notice_count": notices,
        },
    )


def clean_relative_path(path: Path, root: Path) -> str:
    relative = path.relative_to(root).as_posix()
    if any(character in relative for character in ("\0", "\n", "\r", "\\")):
        raise MetadataError(f"bundle path is not checksum-safe: {relative!r}")
    return relative


def bundle_inventory(root: Path, excluded: set[PurePosixPath]) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for path in filesystem_paths(root):
        if path == root:
            continue
        relative = clean_relative_path(path, root)
        if PurePosixPath(relative) in excluded:
            continue
        metadata = path.lstat()
        mode = stat.S_IMODE(metadata.st_mode)
        try:
            xattrs = os.listxattr(path, follow_symlinks=False)
        except OSError as error:
            raise MetadataError(f"cannot inspect bundle xattrs on {path}: {error}") from error
        if xattrs:
            raise MetadataError(f"portable bundle contains unsupported xattrs: {relative}")
        if stat.S_ISDIR(metadata.st_mode):
            records.append({"path": relative, "type": "directory", "mode": mode})
        elif stat.S_ISREG(metadata.st_mode):
            records.append(
                {
                    "path": relative,
                    "type": "file",
                    "mode": mode,
                    "size": metadata.st_size,
                    "sha256": sha256_file(path),
                }
            )
        elif stat.S_ISLNK(metadata.st_mode):
            raise MetadataError(f"portable bundle contains a symlink: {relative}")
        else:
            raise MetadataError(f"portable bundle contains a socket/device/FIFO: {relative}")
    records.sort(key=lambda item: str(item["path"]))
    return records


def require_exact_directory(path: Path, names: set[str], description: str) -> None:
    if path.is_symlink() or not path.is_dir():
        raise MetadataError(f"{description} is not a real directory: {path}")
    actual = {entry.name for entry in path.iterdir()}
    if actual != names:
        missing = sorted(names - actual)
        extra = sorted(actual - names)
        raise MetadataError(f"{description} entries differ; missing={missing}, extra={extra}")


def verify_bundle_layout(root: Path, source_commit: str, complete: bool) -> None:
    if not GIT_COMMIT.fullmatch(source_commit):
        raise MetadataError("source commit must be a 40-character lowercase Git object id")
    expected_top = {
        APPIMAGE_NAME,
        "README.md",
        "runtime",
        "licenses",
        "provenance",
        "vm",
        "shared",
        "cache",
    }
    if complete:
        expected_top.add("SHA256SUMS")
    require_exact_directory(root, expected_top, "portable bundle root")
    for name in ["vm", "shared", "cache"]:
        require_exact_directory(root / name, set(), f"portable {name} directory")
    require_exact_directory(
        root / "runtime", {ROOTFS_ARCHIVE_NAME, ROOTFS_MANIFEST_NAME}, "runtime directory"
    )
    require_exact_directory(root / "licenses", {"appimage", "guest-rootfs"}, "license groups")
    require_exact_directory(
        root / "licenses/appimage", {"README.md", "usr-share-doc"}, "AppImage license group"
    )
    require_exact_directory(
        root / "licenses/guest-rootfs",
        {
            "README.md",
            "usr-share-common-licenses",
            "usr-share-doc",
            "project-source",
        },
        "guest rootfs license group",
    )
    provenance_names = {"appimage", "guest-rootfs"}
    if complete:
        provenance_names.add("bundle-files.json")
    require_exact_directory(root / "provenance", provenance_names, "provenance groups")
    require_exact_directory(
        root / "provenance/appimage", {"WildBuzzard-AppImage-linux-x86_64.json"},
        "AppImage provenance group",
    )
    require_exact_directory(
        root / "provenance/guest-rootfs",
        {
            "ROOTFS_SHA256SUMS",
            "SWAY_UPSTREAM.toml",
            "TRYCUA_CHANGES.WILDBUZZARD.md",
            "TRYCUA_UPSTREAM.toml",
            ROOTFS_MANIFEST_NAME,
            "base-images.lock.toml",
            "oci-packages.tsv",
            "release-components.toml",
        },
        "guest rootfs provenance group",
    )
    for path in [root / APPIMAGE_NAME, root / "README.md"]:
        require_regular(path, "portable bundle payload")
    appimage_metadata = (root / APPIMAGE_NAME).stat()
    if appimage_metadata.st_mode & 0o111 == 0:
        raise MetadataError("portable bundle AppImage is not executable")

    rootfs_archive = root / "runtime" / ROOTFS_ARCHIVE_NAME
    rootfs_manifest = root / "runtime" / ROOTFS_MANIFEST_NAME
    verify_rootfs_manifest(
        argparse.Namespace(archive=rootfs_archive, manifest=rootfs_manifest)
    )
    guest_manifest = root / "provenance/guest-rootfs" / ROOTFS_MANIFEST_NAME
    require_regular(guest_manifest, "guest rootfs provenance manifest")
    if guest_manifest.read_bytes() != rootfs_manifest.read_bytes():
        raise MetadataError("guest provenance rootfs manifest differs from runtime manifest")
    rootfs_record = read_json(rootfs_manifest)
    if not isinstance(rootfs_record, dict):
        raise MetadataError("rootfs manifest is not an object")
    rootfs_source = rootfs_record.get("source")
    if not isinstance(rootfs_source, dict) or rootfs_source.get("commit") != source_commit:
        raise MetadataError("rootfs provenance commit differs from bundle commit")
    provenance_mapping = {
        "oci/base-images.lock.toml": "base-images.lock.toml",
        "oci/desktop/SWAY_UPSTREAM.toml": "SWAY_UPSTREAM.toml",
        "guest/third_party/trycua-cua/UPSTREAM.toml": "TRYCUA_UPSTREAM.toml",
        "guest/third_party/trycua-cua/CHANGES.WILDBUZZARD.md": "TRYCUA_CHANGES.WILDBUZZARD.md",
        "LICENSES/release-components.toml": "release-components.toml",
        "LICENSES/generated/oci-packages.tsv": "oci-packages.tsv",
    }
    provenance_records = rootfs_record.get("provenance_files")
    if not isinstance(provenance_records, list):
        raise MetadataError("rootfs manifest has no provenance file records")
    observed_records: set[str] = set()
    for record in provenance_records:
        if not isinstance(record, dict) or record.get("path") not in provenance_mapping:
            raise MetadataError("rootfs manifest has an unknown provenance file")
        relative = str(record["path"])
        observed_records.add(relative)
        bundled = root / "provenance/guest-rootfs" / provenance_mapping[relative]
        metadata = require_regular(bundled, "bundled guest provenance")
        if record.get("size") != metadata.st_size or record.get("sha256") != sha256_file(bundled):
            raise MetadataError(f"bundled guest provenance differs: {relative}")
    if observed_records != set(provenance_mapping):
        raise MetadataError("rootfs provenance file inventory is incomplete")
    expected_rootfs_checksums = (
        f"{sha256_file(rootfs_archive)}  runtime/{ROOTFS_ARCHIVE_NAME}\n"
        f"{sha256_file(rootfs_manifest)}  runtime/{ROOTFS_MANIFEST_NAME}\n"
    ).encode()
    if (root / "provenance/guest-rootfs/ROOTFS_SHA256SUMS").read_bytes() != expected_rootfs_checksums:
        raise MetadataError("guest ROOTFS_SHA256SUMS is incomplete or incorrect")

    appimage_manifest = read_json(
        root / "provenance/appimage/WildBuzzard-AppImage-linux-x86_64.json"
    )
    if (
        not isinstance(appimage_manifest, dict)
        or appimage_manifest.get("schema") != 1
        or appimage_manifest.get("kind") != "wildbuzzard-appimage"
        or appimage_manifest.get("platform") != {"os": "linux", "architecture": "amd64"}
    ):
        raise MetadataError("AppImage provenance manifest is invalid")
    appimage_source = appimage_manifest.get("source")
    if not isinstance(appimage_source, dict) or appimage_source.get("commit") != source_commit:
        raise MetadataError("AppImage provenance commit differs from bundle commit")
    artifact = appimage_manifest.get("artifact")
    if not isinstance(artifact, dict):
        raise MetadataError("AppImage provenance has no artifact record")
    if (
        artifact.get("name") != APPIMAGE_NAME
        or artifact.get("size") != appimage_metadata.st_size
        or artifact.get("sha256") != sha256_file(root / APPIMAGE_NAME)
    ):
        raise MetadataError("bundled AppImage differs from its provenance")
    appimage_corresponding_source = inspect_source_evidence(
        root / "licenses/appimage/usr-share-doc/wildbuzzard/sources/project",
        source_commit,
    )
    if appimage_source.get("corresponding_source") != appimage_corresponding_source:
        raise MetadataError("AppImage corresponding-source record differs from bundled source")
    inspect_source_evidence(root / "licenses/guest-rootfs/project-source", source_commit)


def create_bundle_manifest(args: argparse.Namespace) -> None:
    root = args.root.resolve(strict=True)
    output = args.output.resolve()
    expected_output = root / BUNDLE_MANIFEST.as_posix()
    if output != expected_output:
        raise MetadataError(f"bundle manifest must be written to {expected_output}")
    if output.exists() or output.is_symlink() or (root / BUNDLE_CHECKSUMS.as_posix()).exists():
        raise MetadataError("bundle manifest/checksums already exist")
    verify_bundle_layout(root, args.source_commit, complete=False)
    record = {
        "schema": 1,
        "kind": "wildbuzzard-portable-bundle",
        "source": {
            "repository": "https://github.com/openresearchtools/BuzzardOS",
            "commit": args.source_commit,
        },
        "bundle_root": root.name,
        "files": bundle_inventory(root, {BUNDLE_MANIFEST, BUNDLE_CHECKSUMS}),
    }
    write_json_atomic(output, record)


def checksum_contents(root: Path) -> bytes:
    records = bundle_inventory(root, {BUNDLE_CHECKSUMS})
    lines = [
        f"{record['sha256']}  {record['path']}\n"
        for record in records
        if record["type"] == "file"
    ]
    return "".join(lines).encode()


def write_bundle_checksums(args: argparse.Namespace) -> None:
    root = args.root.resolve(strict=True)
    destination = root / BUNDLE_CHECKSUMS.as_posix()
    if destination.exists() or destination.is_symlink():
        raise MetadataError("bundle SHA256SUMS already exists")
    temporary = destination.with_name(f".{destination.name}.tmp")
    temporary.write_bytes(checksum_contents(root))
    os.replace(temporary, destination)


def verify_bundle(args: argparse.Namespace) -> None:
    root = args.root.resolve(strict=True)
    manifest = read_json(root / BUNDLE_MANIFEST.as_posix())
    if (
        not isinstance(manifest, dict)
        or manifest.get("schema") != 1
        or manifest.get("kind") != "wildbuzzard-portable-bundle"
    ):
        raise MetadataError("portable bundle manifest identity or schema is unsupported")
    source = manifest.get("source")
    if (
        not isinstance(source, dict)
        or source.get("repository") != "https://github.com/openresearchtools/BuzzardOS"
        or not isinstance(source.get("commit"), str)
    ):
        raise MetadataError("portable bundle manifest has no source commit")
    commit = source["commit"]
    verify_bundle_layout(root, commit, complete=True)
    if manifest.get("bundle_root") != root.name:
        raise MetadataError("portable bundle root name differs from its manifest")
    actual = bundle_inventory(root, {BUNDLE_MANIFEST, BUNDLE_CHECKSUMS})
    if manifest.get("files") != actual:
        raise MetadataError("portable bundle has missing, extra, or changed files")
    checksums = root / BUNDLE_CHECKSUMS.as_posix()
    require_regular(checksums, "portable bundle checksums")
    if checksums.read_bytes() != checksum_contents(root):
        raise MetadataError("portable bundle SHA256SUMS is incomplete or incorrect")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    rootfs = subcommands.add_parser("rootfs", help="write a flat-rootfs manifest")
    rootfs.add_argument("--rootfs", type=Path, required=True)
    rootfs.add_argument("--archive", type=Path, required=True)
    rootfs.add_argument("--oci-layout", type=Path, required=True)
    rootfs.add_argument("--source-commit", required=True)
    rootfs.add_argument("--output", type=Path, required=True)
    rootfs.set_defaults(function=create_rootfs_manifest)
    verify = subcommands.add_parser("verify", help="verify a flat-rootfs archive")
    verify.add_argument("--archive", type=Path, required=True)
    verify.add_argument("--manifest", type=Path, required=True)
    verify.set_defaults(function=verify_rootfs_manifest)
    appimage = subcommands.add_parser("appimage", help="write an AppImage manifest")
    appimage.add_argument("--appimage", type=Path, required=True)
    appimage.add_argument("--appdir", type=Path, required=True)
    appimage.add_argument("--source-commit", required=True)
    appimage.add_argument("--output", type=Path, required=True)
    appimage.set_defaults(function=create_appimage_manifest)
    bundle = subcommands.add_parser("bundle", help="write a portable bundle manifest")
    bundle.add_argument("--root", type=Path, required=True)
    bundle.add_argument("--source-commit", required=True)
    bundle.add_argument("--output", type=Path, required=True)
    bundle.set_defaults(function=create_bundle_manifest)
    checksums = subcommands.add_parser("checksums", help="write bundle SHA256SUMS")
    checksums.add_argument("--root", type=Path, required=True)
    checksums.set_defaults(function=write_bundle_checksums)
    verify_bundle_parser = subcommands.add_parser(
        "verify-bundle", help="verify a complete portable bundle directory"
    )
    verify_bundle_parser.add_argument("--root", type=Path, required=True)
    verify_bundle_parser.set_defaults(function=verify_bundle)
    materialize = subcommands.add_parser(
        "materialize", help="materialize a verified internal-link notice tree"
    )
    materialize.add_argument("--source", type=Path, required=True)
    materialize.add_argument("--destination", type=Path, required=True)
    materialize.set_defaults(function=materialize_command)
    idmapped = subcommands.add_parser(
        "verify-idmapped-copy",
        help="verify canonical guest owners against a keep-id subordinate mapping",
    )
    idmapped.add_argument("--canonical", type=Path, required=True)
    idmapped.add_argument("--mapped", type=Path, required=True)
    idmapped.add_argument("--host-uid", type=int, required=True)
    idmapped.add_argument("--host-gid", type=int, required=True)
    idmapped.add_argument("--subuid-start", type=int, required=True)
    idmapped.add_argument("--subgid-start", type=int, required=True)
    idmapped.set_defaults(function=verify_idmapped_copy)
    verify_source = subcommands.add_parser(
        "verify-source", help="verify exact project corresponding-source evidence"
    )
    verify_source.add_argument("--directory", type=Path, required=True)
    verify_source.add_argument("--source-commit", required=True)
    verify_source.set_defaults(function=verify_source_command)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        args.function(args)
    except (MetadataError, OSError, subprocess.SubprocessError, tarfile.TarError) as error:
        print(f"release metadata error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
