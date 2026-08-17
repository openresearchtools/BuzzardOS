#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Verify the rootfs, licensing, and provenance inputs to the portable bundle."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import stat
import subprocess
import sys
import tarfile
from pathlib import Path
from typing import BinaryIO

import release_metadata


ARCHIVE_NAME = "default-rootfs.oci.tar.zst"
METADATA_NAME = "default-rootfs.oci.json"
COMMIT = re.compile(r"^[0-9a-f]{40}$")
DIGEST = re.compile(r"^sha256:[0-9a-f]{64}$")
JSON_BLOB_LIMIT = 16 * 1024 * 1024
MACHINE_CONFIG_ANNOTATION = "org.openresearchtools.buzzardos.machine-config.v1"
LAYER_MEDIA_TYPES = {
    "application/vnd.oci.image.layer.v1.tar",
    "application/vnd.oci.image.layer.v1.tar+gzip",
    "application/vnd.oci.image.layer.v1.tar+zstd",
}

GUEST_PROVENANCE_SOURCES = {
    "base-images.lock.toml": Path("oci/base-images.lock.toml"),
    "SWAY_UPSTREAM.toml": Path("oci/desktop/SWAY_UPSTREAM.toml"),
    "release-components.toml": Path("LICENSES/release-components.toml"),
    "oci-packages.tsv": Path("LICENSES/generated/oci-packages.tsv"),
    "TRYCUA_UPSTREAM.toml": Path("guest/third_party/trycua-cua/UPSTREAM.toml"),
    "TRYCUA_CHANGES.BUZZARDOS.md": Path(
        "guest/third_party/trycua-cua/CHANGES.BUZZARDOS.md"
    ),
}


class VerificationError(RuntimeError):
    """A portable release input is missing, unsafe, or inconsistent."""


def require_directory(path: Path, description: str) -> None:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise VerificationError(f"cannot inspect {description} {path}: {error}") from error
    if not stat.S_ISDIR(metadata.st_mode):
        raise VerificationError(f"{description} must be a real directory: {path}")


def require_regular(path: Path, description: str) -> os.stat_result:
    try:
        metadata = path.lstat()
    except OSError as error:
        raise VerificationError(f"cannot inspect {description} {path}: {error}") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise VerificationError(f"{description} must be a regular non-symlink file: {path}")
    return metadata


def require_exact_directory(path: Path, names: set[str], description: str) -> None:
    require_directory(path, description)
    observed = {entry.name for entry in path.iterdir()}
    if observed != names:
        missing = sorted(names - observed)
        extra = sorted(observed - names)
        raise VerificationError(
            f"{description} has an unexpected inventory; missing={missing}, extra={extra}"
        )


def require_regular_file_tree(path: Path, description: str) -> None:
    require_directory(path, description)
    regular_files = 0
    for entry in path.rglob("*"):
        mode = entry.lstat().st_mode
        if stat.S_ISDIR(mode):
            continue
        if not stat.S_ISREG(mode):
            raise VerificationError(f"{description} contains a non-regular entry: {entry}")
        regular_files += 1
    if regular_files == 0:
        raise VerificationError(f"{description} contains no notice files")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def read_json(path: Path, description: str) -> object:
    require_regular(path, description)
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise VerificationError(f"{description} is not valid UTF-8 JSON: {error}") from error


def read_member(source: BinaryIO, size: int, *, retain: bool) -> tuple[str, bytes | None]:
    digest = hashlib.sha256()
    remaining = size
    contents = bytearray() if retain else None
    while remaining:
        block = source.read(min(1024 * 1024, remaining))
        if not block:
            raise VerificationError("OCI seed archive contains a truncated member")
        digest.update(block)
        if contents is not None:
            contents.extend(block)
        remaining -= len(block)
    return digest.hexdigest(), bytes(contents) if contents is not None else None


def descriptor_blob(
    descriptor: object,
    blobs: dict[str, tuple[int, bytes | None]],
    description: str,
) -> tuple[str, bytes | None]:
    if not isinstance(descriptor, dict):
        raise VerificationError(f"OCI {description} descriptor is not an object")
    digest = descriptor.get("digest")
    size = descriptor.get("size")
    if not isinstance(digest, str) or DIGEST.fullmatch(digest) is None:
        raise VerificationError(f"OCI {description} descriptor has an invalid digest")
    if not isinstance(size, int) or size < 0:
        raise VerificationError(f"OCI {description} descriptor has an invalid size")
    record = blobs.get(digest.removeprefix("sha256:"))
    if record is None or record[0] != size:
        raise VerificationError(f"OCI {description} blob is missing or has the wrong size")
    return digest, record[1]


def parse_json_bytes(contents: bytes | None, description: str) -> object:
    if contents is None:
        raise VerificationError(f"OCI {description} is unreasonably large")
    try:
        return json.loads(contents)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise VerificationError(f"OCI {description} is not valid UTF-8 JSON: {error}") from error


def verify_oci_seed_archive(archive: Path, expected_manifest_digest: str) -> None:
    with archive.open("rb") as source:
        if source.read(4) != b"\x28\xb5\x2f\xfd":
            raise VerificationError("OCI seed archive is not a Zstandard frame")

    decompressor = subprocess.Popen(
        ["zstd", "-q", "-dc", "--", str(archive)],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    assert decompressor.stdout is not None
    assert decompressor.stderr is not None
    root_files: dict[str, bytes] = {}
    blobs: dict[str, tuple[int, bytes | None]] = {}
    observed: set[str] = set()
    try:
        with tarfile.open(fileobj=decompressor.stdout, mode="r|") as stream:
            for member in stream:
                name = release_metadata.normalize_tar_name(member.name, "OCI seed member")
                if name in observed:
                    raise VerificationError(f"OCI seed archive repeats a member: {name}")
                observed.add(name)
                if member.isdir():
                    if name not in {".", "blobs", "blobs/sha256"}:
                        raise VerificationError(f"OCI seed archive has an unexpected directory: {name}")
                    continue
                if not member.isfile():
                    raise VerificationError(f"OCI seed archive has a non-regular member: {name}")
                extracted = stream.extractfile(member)
                if extracted is None:
                    raise VerificationError(f"cannot read OCI seed member: {name}")
                digest, contents = read_member(
                    extracted, member.size, retain=member.size <= JSON_BLOB_LIMIT
                )
                if name in {"oci-layout", "index.json"}:
                    if contents is None:
                        raise VerificationError(f"OCI seed {name} is unreasonably large")
                    root_files[name] = contents
                    continue
                prefix = "blobs/sha256/"
                if not name.startswith(prefix) or re.fullmatch(r"[0-9a-f]{64}", name[len(prefix) :]) is None:
                    raise VerificationError(f"OCI seed archive has an unexpected file: {name}")
                blob_name = name[len(prefix) :]
                if digest != blob_name:
                    raise VerificationError(f"OCI seed blob name does not match its digest: {name}")
                blobs[blob_name] = (member.size, contents)
        while decompressor.stdout.read(1024 * 1024):
            pass
    except VerificationError:
        decompressor.terminate()
        decompressor.stdout.close()
        decompressor.stderr.read()
        decompressor.stderr.close()
        decompressor.wait()
        raise
    except (tarfile.TarError, OSError) as error:
        decompressor.terminate()
        decompressor.stdout.close()
        decompressor.stderr.read()
        decompressor.stderr.close()
        decompressor.wait()
        raise VerificationError(f"cannot read OCI seed archive: {error}") from error
    decompressor.stdout.close()
    decompression_error = decompressor.stderr.read().decode("utf-8", errors="replace").strip()
    decompressor.stderr.close()
    if decompressor.wait() != 0:
        raise VerificationError(f"OCI seed decompression failed: {decompression_error}")

    marker = parse_json_bytes(root_files.get("oci-layout"), "layout marker")
    if marker != {"imageLayoutVersion": "1.0.0"}:
        raise VerificationError("OCI seed layout marker is not version 1.0.0")
    index = parse_json_bytes(root_files.get("index.json"), "index")
    if not isinstance(index, dict) or index.get("schemaVersion") != 2:
        raise VerificationError("OCI seed index must use schemaVersion 2")
    manifests = index.get("manifests")
    if not isinstance(manifests, list) or len(manifests) != 1:
        raise VerificationError("OCI seed index must contain exactly one manifest")
    manifest_digest, manifest_contents = descriptor_blob(manifests[0], blobs, "manifest")
    if manifest_digest != expected_manifest_digest:
        raise VerificationError("OCI seed manifest differs from its metadata")
    manifest = parse_json_bytes(manifest_contents, "manifest")
    if not isinstance(manifest, dict) or manifest.get("schemaVersion") != 2:
        raise VerificationError("OCI seed manifest must use schemaVersion 2")
    annotations = manifest.get("annotations") or {}
    if not isinstance(annotations, dict) or MACHINE_CONFIG_ANNOTATION in annotations:
        raise VerificationError("OCI seed manifest carries portable machine identity")
    config_digest, config_contents = descriptor_blob(manifest.get("config"), blobs, "config")
    config = parse_json_bytes(config_contents, "config")
    if not isinstance(config, dict) or config.get("os") != "linux" or config.get(
        "architecture"
    ) != "amd64":
        raise VerificationError("OCI seed config is not linux/amd64")
    layers = manifest.get("layers")
    if not isinstance(layers, list) or len(layers) != 1:
        raise VerificationError("OCI seed manifest must contain one flattened filesystem layer")
    layer = layers[0]
    if not isinstance(layer, dict) or layer.get("mediaType") not in LAYER_MEDIA_TYPES:
        raise VerificationError("OCI seed layer has an unsupported media type")
    layer_digests = {descriptor_blob(layer, blobs, "layer")[0]}
    rootfs = config.get("rootfs")
    if (
        not isinstance(rootfs, dict)
        or rootfs.get("type") != "layers"
        or not isinstance(rootfs.get("diff_ids"), list)
        or len(rootfs["diff_ids"]) != 1
        or not isinstance(rootfs["diff_ids"][0], str)
        or DIGEST.fullmatch(rootfs["diff_ids"][0]) is None
    ):
        raise VerificationError("OCI seed config must describe exactly one filesystem diff ID")
    referenced = {
        manifest_digest.removeprefix("sha256:"),
        config_digest.removeprefix("sha256:"),
        *(digest.removeprefix("sha256:") for digest in layer_digests),
    }
    if set(blobs) != referenced:
        raise VerificationError("OCI seed has missing, duplicate, or unreferenced blobs")


def verify_rootfs_stage(rootfs_stage: Path, project_root: Path, source_commit: str) -> None:
    if COMMIT.fullmatch(source_commit) is None:
        raise VerificationError("source commit must be a 40-character lowercase Git object id")

    require_exact_directory(
        rootfs_stage,
        {"runtime", "licenses", "provenance", "ROOTFS_SHA256SUMS"},
        "rootfs release stage",
    )
    runtime = rootfs_stage / "runtime"
    require_exact_directory(runtime, {ARCHIVE_NAME, METADATA_NAME}, "rootfs runtime")
    archive = runtime / ARCHIVE_NAME
    archive_metadata = require_regular(archive, "OCI seed archive")
    archive_sha256 = sha256(archive)

    metadata_path = runtime / METADATA_NAME
    metadata = read_json(metadata_path, "OCI seed metadata")
    if not isinstance(metadata, dict):
        raise VerificationError("OCI seed metadata must be a JSON object")
    expected_keys = {
        "schema",
        "kind",
        "platform",
        "archive",
        "manifest_digest",
        "source_manifest_digest",
        "source_commit",
    }
    if set(metadata) != expected_keys:
        raise VerificationError("OCI seed metadata has missing or extra fields")
    if metadata.get("schema") != 1 or metadata.get("kind") != "buzzardos-oci-seed":
        raise VerificationError("OCI seed metadata identity or schema is unsupported")
    if metadata.get("platform") != {"os": "linux", "architecture": "amd64"}:
        raise VerificationError("OCI seed metadata platform is not linux/amd64")
    if metadata.get("source_commit") != source_commit:
        raise VerificationError("OCI seed metadata source commit differs from the build")
    if not isinstance(metadata.get("manifest_digest"), str) or DIGEST.fullmatch(
        metadata["manifest_digest"]
    ) is None:
        raise VerificationError("OCI seed metadata has an invalid manifest digest")
    if not isinstance(metadata.get("source_manifest_digest"), str) or DIGEST.fullmatch(
        metadata["source_manifest_digest"]
    ) is None:
        raise VerificationError("OCI seed metadata has an invalid source manifest digest")
    expected_archive = {
        "name": ARCHIVE_NAME,
        "size": archive_metadata.st_size,
        "sha256": archive_sha256,
    }
    if metadata.get("archive") != expected_archive:
        raise VerificationError("OCI seed archive differs from its metadata")
    verify_oci_seed_archive(archive, metadata["manifest_digest"])

    checksums = rootfs_stage / "ROOTFS_SHA256SUMS"
    require_regular(checksums, "rootfs checksum manifest")
    expected_checksums = (
        f"{archive_sha256}  runtime/{ARCHIVE_NAME}\n"
        f"{sha256(metadata_path)}  runtime/{METADATA_NAME}\n"
    ).encode()
    if checksums.read_bytes() != expected_checksums:
        raise VerificationError("ROOTFS_SHA256SUMS is incomplete or inconsistent")

    licenses = rootfs_stage / "licenses"
    require_exact_directory(licenses, {"guest"}, "rootfs license groups")
    guest_licenses = licenses / "guest"
    require_exact_directory(
        guest_licenses,
        {"README.md", "project-source", "usr-share-common-licenses", "usr-share-doc"},
        "guest license evidence",
    )
    require_regular(guest_licenses / "README.md", "guest license README")
    require_directory(guest_licenses / "project-source", "guest project source")
    for directory in ("usr-share-common-licenses", "usr-share-doc"):
        require_regular_file_tree(guest_licenses / directory, f"guest license {directory}")
    if (guest_licenses / "README.md").read_bytes() != (
        project_root / "tools/release/guest-rootfs-licenses.README.md"
    ).read_bytes():
        raise VerificationError("guest license README differs from the project source")
    try:
        release_metadata.inspect_source_evidence(
            guest_licenses / "project-source", source_commit
        )
    except release_metadata.MetadataError as error:
        raise VerificationError(f"guest corresponding-source evidence is invalid: {error}") from error

    provenance = rootfs_stage / "provenance"
    require_exact_directory(provenance, {"guest"}, "rootfs provenance groups")
    guest_provenance = provenance / "guest"
    expected_provenance = set(GUEST_PROVENANCE_SOURCES) | {METADATA_NAME}
    require_exact_directory(guest_provenance, expected_provenance, "guest provenance")
    provenance_metadata = guest_provenance / METADATA_NAME
    require_regular(provenance_metadata, "guest OCI seed metadata")
    if provenance_metadata.read_bytes() != metadata_path.read_bytes():
        raise VerificationError("guest provenance OCI metadata differs from the runtime metadata")
    for bundled_name, source_name in GUEST_PROVENANCE_SOURCES.items():
        bundled = guest_provenance / bundled_name
        source = project_root / source_name
        require_regular(bundled, f"guest provenance {bundled_name}")
        require_regular(source, f"project provenance source {source_name}")
        if bundled.read_bytes() != source.read_bytes():
            raise VerificationError(
                f"guest provenance {bundled_name} differs from the project source"
            )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rootfs-stage", type=Path, required=True)
    parser.add_argument("--project-root", type=Path, required=True)
    parser.add_argument("--source-commit", required=True)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        rootfs_stage = args.rootfs_stage.resolve(strict=True)
        project_root = args.project_root.resolve(strict=True)
        verify_rootfs_stage(rootfs_stage, project_root, args.source_commit)
    except (OSError, VerificationError) as error:
        print(f"portable release input error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
