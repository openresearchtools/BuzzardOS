#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Generate and verify Buzzard OS release licensing evidence."""

from __future__ import annotations

import argparse
import csv
import fnmatch
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib
from typing import Iterable


ROOT = Path(__file__).resolve().parents[1]
LICENSES = ROOT / "LICENSES"
GENERATED = LICENSES / "generated"
HOST_MANIFEST = ROOT / "host/Cargo.toml"
HOST_LOCK = ROOT / "host/Cargo.lock"
GUEST_MANIFEST = ROOT / "guest/Cargo.toml"
GUEST_LOCK = ROOT / "guest/Cargo.lock"
CUA_ROOT = ROOT / "cua"
CUA_MANIFEST = CUA_ROOT / "Cargo.toml"
CUA_LOCK = CUA_ROOT / "Cargo.lock"
TARGET = "x86_64-unknown-linux-gnu"
OCI_PACKAGE_INVENTORIES = {
    "standard": GENERATED / "oci-packages.tsv",
    "cuda": GENERATED / "oci-packages.cuda.tsv",
}
CUDA_KEYRING_LICENSE_SHA256 = (
    "be0f15ae130d46adb2c2aed7229518da353f28f1471d80b4dce62d909c6ceb2d"
)
SHA256_PATTERN = re.compile(r"[0-9a-f]{64}")
DEBIAN_BINARY_PACKAGE_PATTERN = re.compile(
    r"[a-z0-9][a-z0-9+.-]*(?::[a-z0-9][a-z0-9-]*)?"
)

NOTICE_NAME = re.compile(
    r"^(?:LICENSE|COPYING|COPYRIGHT|NOTICE|NOTICES)(?:[-._].*)?$", re.IGNORECASE
)
LEGACY_LICENSES = {
    "MIT/Apache-2.0": "MIT OR Apache-2.0",
    "Apache-2.0/MIT": "Apache-2.0 OR MIT",
    "Apache-2.0 / MIT": "Apache-2.0 OR MIT",
    "Unlicense/MIT": "Unlicense OR MIT",
}
BLOCKING_STATUS = re.compile(
    r"^(?:missing-|needs-|requires-|package-has-no-|release-blocker)"
)


class AuditError(RuntimeError):
    pass


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run(command: list[str], *, cwd: Path = ROOT) -> str:
    completed = subprocess.run(
        command,
        cwd=cwd,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise AuditError(f"command failed ({' '.join(command)}): {detail}")
    return completed.stdout


def read_toml(path: Path) -> dict:
    try:
        with path.open("rb") as source:
            return tomllib.load(source)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise AuditError(f"cannot read {path.relative_to(ROOT)}: {error}") from error


def atomic_write(path: Path, contents: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            "w", encoding="utf-8", newline="\n", dir=path.parent, delete=False
        ) as output:
            temporary = Path(output.name)
            output.write(contents)
        temporary.chmod(0o644)
        os.replace(temporary, path)
        temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def atomic_copy(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary: Path | None = None
    try:
        with source.open("rb") as input_stream, tempfile.NamedTemporaryFile(
            "wb", dir=destination.parent, delete=False
        ) as output_stream:
            temporary = Path(output_stream.name)
            shutil.copyfileobj(input_stream, output_stream)
        temporary.chmod(0o644)
        os.replace(temporary, destination)
        temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def cargo_metadata(manifest: Path) -> dict:
    output = run(
        [
            "cargo",
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--manifest-path",
            str(manifest),
        ]
    )
    return json.loads(output)


def cargo_release_keys(
    manifest: Path, packages: tuple[str, ...] | None
) -> set[tuple[str, str]]:
    base_command = [
        "cargo",
        "tree",
        "--locked",
        "--manifest-path",
        str(manifest),
        "--target",
        TARGET,
        "--edges",
        "normal,build",
        "--prefix",
        "none",
        "--format",
        "{p}",
    ]
    commands = (
        [base_command + ["--workspace"]]
        if packages is None
        else [base_command + ["--package", package] for package in packages]
    )
    keys: set[tuple[str, str]] = set()
    for command in commands:
        for line in run(command).splitlines():
            line = line.strip()
            while re.search(r" \([^)]*\)$", line):
                line = re.sub(r" \([^)]*\)$", "", line)
            if " v" not in line:
                continue
            name, version = line.rsplit(" v", 1)
            if name and version and " " not in version:
                keys.add((name, version))
    return keys


def lock_checksums(lock_path: Path) -> dict[tuple[str, str, str], str]:
    lock = read_toml(lock_path)
    result: dict[tuple[str, str, str], str] = {}
    for package in lock.get("package", []):
        source = package.get("source")
        checksum = package.get("checksum")
        if source is not None and checksum is not None:
            result[(package["name"], package["version"], source)] = checksum
    return result


def vcs_info(crate_root: Path) -> tuple[str, str]:
    path = crate_root / ".cargo_vcs_info.json"
    if not path.is_file():
        return "", ""
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        return data.get("git", {}).get("sha1", ""), data.get("path_in_vcs", "")
    except (OSError, json.JSONDecodeError):
        return "", ""


def fallback_map() -> dict[str, dict]:
    data = read_toml(LICENSES / "cargo-fallbacks.toml")
    result: dict[str, dict] = {}
    for fallback in data.get("fallback", []):
        for package in fallback.get("packages", []):
            if package in result:
                raise AuditError(f"duplicate Cargo fallback for {package}")
            result[package] = fallback
    return result


def notice_candidates(package: dict, fallbacks: dict[str, dict]) -> list[tuple[str, bytes]]:
    crate_root = Path(package["manifest_path"]).parent.resolve()
    candidates: dict[str, Path] = {}
    explicit = package.get("license_file")
    if explicit:
        explicit_path = Path(explicit).resolve()
        if not explicit_path.is_relative_to(crate_root):
            raise AuditError(
                f"{package['name']} {package['version']} license file escapes its crate"
            )
        candidates[explicit_path.relative_to(crate_root).as_posix()] = explicit_path
    for path in crate_root.rglob("*"):
        if not path.is_file():
            continue
        if NOTICE_NAME.match(path.name):
            resolved = path.resolve()
            if not resolved.is_relative_to(crate_root):
                raise AuditError(
                    f"{package['name']} {package['version']} notice escapes its crate"
                )
            candidates[path.relative_to(crate_root).as_posix()] = resolved
    if candidates:
        return [(name, path.read_bytes()) for name, path in sorted(candidates.items())]

    package_key = f"{package['name']}@{package['version']}"
    fallback = fallbacks.get(package_key)
    if fallback is None:
        raise AuditError(f"{package_key} ships no license/notice text and has no fallback")
    commit, _ = vcs_info(crate_root)
    if commit != fallback["vcs_commit"]:
        raise AuditError(
            f"{package_key} fallback commit {fallback['vcs_commit']} != crate {commit or 'missing'}"
        )
    result = []
    for relative in fallback.get("files", []):
        path = (ROOT / relative).resolve()
        if not path.is_relative_to(ROOT) or not path.is_file():
            raise AuditError(f"{package_key} fallback file missing: {relative}")
        result.append((f"fallback:{relative}", path.read_bytes()))
    if not result:
        raise AuditError(f"{package_key} fallback has no files")
    return result


def sanitize(value: object) -> str:
    return str(value or "").replace("\t", " ").replace("\r", " ").replace("\n", " ")


def build_cargo_graph(
    graph: str,
    manifest: Path,
    lock_path: Path,
    packages: tuple[str, ...] | None,
    fallbacks: dict[str, dict],
) -> tuple[str, list[dict], dict[str, dict]]:
    metadata = cargo_metadata(manifest)
    release_keys = cargo_release_keys(manifest, packages)
    checksums = lock_checksums(lock_path)
    selected = [
        item
        for item in metadata["packages"]
        if (item["name"], item["version"]) in release_keys
    ]
    external = [item for item in selected if item.get("source") is not None]
    local = [item for item in selected if item.get("source") is None]
    rows: list[dict] = []
    contents: dict[str, dict] = {}
    for item in external:
        raw_license = item.get("license") or ""
        if not raw_license and not item.get("license_file"):
            raise AuditError(f"{item['name']} {item['version']} has no license metadata")
        source = item["source"]
        checksum = checksums.get((item["name"], item["version"], source))
        if checksum is None:
            raise AuditError(f"{item['name']} {item['version']} has no lock checksum")
        commit, path_in_vcs = vcs_info(Path(item["manifest_path"]).parent)
        notices = []
        for notice_path, data in notice_candidates(item, fallbacks):
            digest = sha256_bytes(data)
            notices.append(f"{notice_path}@sha256:{digest}")
            entry = contents.setdefault(digest, {"data": data, "users": []})
            entry["users"].append(
                f"{graph}: {item['name']} {item['version']} — {notice_path}"
            )
        rows.append(
            {
                "name": item["name"],
                "version": item["version"],
                "source": source,
                "checksum": checksum,
                "license": raw_license or f"LicenseRef-file:{item['license_file']}",
                "normalized_license": LEGACY_LICENSES.get(raw_license, raw_license),
                "repository": sanitize(item.get("repository")),
                "vcs_commit": commit,
                "path_in_vcs": path_in_vcs,
                "notice_files": ";".join(notices),
            }
        )
    rows.sort(key=lambda row: (row["name"], row["version"], row["source"]))
    local_missing = sorted(
        f"{item['name']}@{item['version']}"
        for item in local
        if not item.get("license") and not item.get("license_file")
    )
    header = (
        f"# graph={graph}\ttarget={TARGET}\tlock_sha256={sha256_file(lock_path)}"
        f"\texternal_packages={len(rows)}\n"
    )
    buffer = io.StringIO(newline="")
    buffer.write(header)
    fields = [
        "name",
        "version",
        "source",
        "checksum",
        "license",
        "normalized_license",
        "repository",
        "vcs_commit",
        "path_in_vcs",
        "notice_files",
    ]
    writer = csv.DictWriter(buffer, fields, delimiter="\t", lineterminator="\n")
    writer.writeheader()
    writer.writerows(rows)
    return buffer.getvalue(), [{"package": value} for value in local_missing], contents


def merged_notice_bundle(graph_contents: Iterable[dict[str, dict]]) -> str:
    merged: dict[str, dict] = {}
    for content in graph_contents:
        for digest, entry in content.items():
            target = merged.setdefault(digest, {"data": entry["data"], "users": []})
            if target["data"] != entry["data"]:
                raise AuditError(f"SHA-256 collision while merging license text {digest}")
            target["users"].extend(entry["users"])
    output = [
        "Buzzard OS locked Rust dependency license and notice files\n",
        "Generated from the x86_64 Linux normal/build release graphs.\n",
        "Package versions and crates.io checksums are in the adjacent TSV files.\n",
    ]
    for digest in sorted(merged):
        entry = merged[digest]
        output.extend(
            [
                "\n" + "=" * 80 + "\n",
                f"SHA256: {digest}\n",
                "Used by:\n",
            ]
        )
        for user in sorted(set(entry["users"])):
            output.append(f"- {user}\n")
        output.append("\n")
        text = entry["data"].decode("utf-8", errors="replace")
        output.append(text)
        if not text.endswith("\n"):
            output.append("\n")
    return "".join(output)


def cargo_outputs() -> tuple[dict[Path, str], list[str]]:
    fallbacks = fallback_map()
    host_tsv, host_local, host_contents = build_cargo_graph(
        "host-workspace", HOST_MANIFEST, HOST_LOCK, None, fallbacks
    )
    guest_tsv, guest_local, guest_contents = build_cargo_graph(
        "buzzardos-guest",
        GUEST_MANIFEST,
        GUEST_LOCK,
        ("buzzardos-clipboard-agent",),
        fallbacks,
    )
    desktop_tsv, desktop_local, desktop_contents = build_cargo_graph(
        "buzzardos-desktop",
        GUEST_MANIFEST,
        GUEST_LOCK,
        (
            "buzzardos-desktop",
            "buzzardos-settings",
            "buzzardos-shortcut-helper",
        ),
        fallbacks,
    )
    cua_tsv, cua_local, cua_contents = build_cargo_graph(
        "buzzardoscua",
        CUA_MANIFEST,
        CUA_LOCK,
        ("buzzardoscua",),
        fallbacks,
    )
    issues = [
        f"local Cargo package lacks license metadata: {item['package']}"
        for item in host_local + guest_local + desktop_local + cua_local
    ]
    return (
        {
            GENERATED / "cargo-host.tsv": host_tsv,
            GENERATED / "cargo-buzzardos-guest.tsv": guest_tsv,
            GENERATED / "cargo-buzzardos-desktop.tsv": desktop_tsv,
            GENERATED / "cargo-cua.tsv": cua_tsv,
            GENERATED / "RUST_DEPENDENCY_LICENSES.buzzardos.txt": (
                merged_notice_bundle([host_contents])
            ),
            GENERATED / "RUST_DEPENDENCY_LICENSES.buzzardos-guest.txt": (
                merged_notice_bundle([guest_contents])
            ),
            GENERATED / "RUST_DEPENDENCY_LICENSES.buzzardos-desktop.txt": (
                merged_notice_bundle([desktop_contents])
            ),
            GENERATED / "RUST_DEPENDENCY_LICENSES.buzzardoscua.txt": (
                merged_notice_bundle([cua_contents])
            ),
        },
        issues,
    )


def validate_generated(outputs: dict[Path, str], generate: bool) -> None:
    for path, expected in outputs.items():
        if generate:
            atomic_write(path, expected)
            print(f"generated {path.relative_to(ROOT)}")
            continue
        try:
            # Some upstream notice texts intentionally contain CRLF.  Compare
            # bytes so universal-newline decoding cannot make a freshly
            # generated notice bundle appear stale.
            actual = path.read_bytes()
        except OSError as error:
            raise AuditError(f"missing generated inventory {path.relative_to(ROOT)}") from error
        if actual != expected.encode("utf-8"):
            raise AuditError(
                f"stale generated inventory {path.relative_to(ROOT)}; run "
                "tools/check-licenses.sh --generate --structural"
            )


def validate_upstream_sources() -> None:
    data = read_toml(LICENSES / "upstream/SOURCES.toml")
    seen: set[str] = set()
    for record in data.get("file", []):
        relative = record["path"]
        if relative in seen:
            raise AuditError(f"duplicate upstream notice record: {relative}")
        seen.add(relative)
        path = (ROOT / relative).resolve()
        if not path.is_relative_to(ROOT) or not path.is_file():
            raise AuditError(f"upstream notice missing: {relative}")
        actual = sha256_file(path)
        if actual != record["sha256"]:
            raise AuditError(
                f"upstream notice checksum mismatch: {relative}: {actual}"
            )


def validate_provenance() -> None:
    required = [
        ROOT / "LICENSE",
        ROOT / "NOTICE",
        ROOT / "THIRD_PARTY_NOTICES.md",
        LICENSES / "mpl-sources.tsv",
        LICENSES / "release-components.toml",
        LICENSES / "guest-assets.toml",
        LICENSES / "package-inputs.toml",
        LICENSES / "rust-runtime.toml",
        LICENSES / "package-notices/buzzardos.md",
        LICENSES / "package-notices/buzzardos-guest.md",
        LICENSES / "package-notices/buzzardos-desktop.md",
        LICENSES / "package-notices/buzzardoscua.md",
        ROOT / "packaging/copyright/buzzardos",
        ROOT / "packaging/copyright/buzzardos-guest",
        ROOT / "packaging/copyright/buzzardos-desktop",
        ROOT / "packaging/copyright/buzzardoscua",
        CUA_ROOT / "LICENSE.trycua.md",
        CUA_ROOT / "CITATION.cff",
        CUA_ROOT / "UPSTREAM.toml",
        CUA_ROOT / "CHANGES.BUZZARDOS.md",
    ]
    for path in required:
        if not path.is_file() or path.stat().st_size == 0:
            raise AuditError(f"required licensing evidence missing: {path.relative_to(ROOT)}")
    upstream = read_toml(CUA_ROOT / "UPSTREAM.toml")
    expected_commit = "10279552e2bbe479e367a082f78b1b98ee85a697"
    if upstream.get("upstream_commit") != expected_commit or upstream.get("license") != "MIT":
        raise AuditError("TryCua upstream commit/license record changed unexpectedly")
    validate_upstream_sources()
    validate_build_pins()
    validate_package_inputs()
    validate_rust_runtime_record()
    validate_mpl_sources()
    validate_oci_package_inventory_record()


def mpl_source_records() -> list[tuple[str, str, str, str]]:
    records: list[tuple[str, str, str, str]] = []
    with (LICENSES / "mpl-sources.tsv").open(encoding="utf-8", newline="") as source:
        for row in csv.reader(source, delimiter="\t"):
            if not row or row[0].startswith("#"):
                continue
            if len(row) != 4:
                raise AuditError("invalid MPL source manifest row")
            records.append((row[0], row[1], row[2], row[3]))
    return records


def mpl_source_records_for_inventory(
    inventory: Path,
) -> list[tuple[str, str, str, str]]:
    available = {
        (name, version): (name, version, checksum, url)
        for name, version, checksum, url in mpl_source_records()
    }
    selected: list[tuple[str, str, str, str]] = []
    with inventory.open(encoding="utf-8", newline="") as source:
        rows = csv.DictReader(
            (line for line in source if not line.startswith("#")), delimiter="\t"
        )
        for row in rows:
            if "MPL-2.0" not in row.get("normalized_license", ""):
                continue
            key = (row["name"], row["version"])
            record = available.get(key)
            if record is None:
                raise AuditError(
                    f"MPL source manifest has no entry for {key[0]} {key[1]}"
                )
            selected.append(record)
    return sorted(selected)


def validate_mpl_sources() -> None:
    records = mpl_source_records()
    expected = {
        ("option-ext", "0.2.0", "04744f49eae99ab78e0d5c0b603ab218f515ea8cfe5a456d7629ad883a3b6e7d"),
        ("uniffi", "0.31.0", "b8c6dec3fc6645f71a16a3fa9ff57991028153bd194ca97f4b55e610c73ce66a"),
        ("uniffi_core", "0.31.0", "b0ef62e69762fbb9386dcb6c87cd3dd05d525fa8a3a579a290892e60ddbda47e"),
        ("uniffi_internal_macros", "0.31.0", "98f51ebca0d9a4b2aa6c644d5ede45c56f73906b96403c08a1985e75ccb64a01"),
        ("uniffi_macros", "0.31.0", "db9d12529f1223d014fd501e5f29ca0884d15d6ed5ddddd9f506e55350327dc3"),
        ("uniffi_meta", "0.31.0", "9df6d413db2827c68588f8149d30d49b71d540d46539e435b23a7f7dbd4d4f86"),
        ("uniffi_pipeline", "0.31.0", "a806dddc8208f22efd7e95a5cdf88ed43d0f3271e8f63b47e757a8bbdb43b63a"),
    }
    actual = {(name, version, checksum) for name, version, checksum, _url in records}
    if actual != expected or len(records) != len(expected):
        raise AuditError("MPL source manifest differs from the locked release graph")
    for name, version, checksum, url in records:
        if not re.fullmatch(r"[0-9a-f]{64}", checksum):
            raise AuditError(f"invalid MPL source checksum: {name} {version}")
        expected_url = f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
        if url != expected_url:
            raise AuditError(f"invalid MPL source URL: {name} {version}")


def require_literals(path: Path, literals: Iterable[str]) -> None:
    try:
        contents = path.read_text(encoding="utf-8")
    except OSError as error:
        raise AuditError(f"cannot inspect build pins in {path.relative_to(ROOT)}") from error
    for literal in literals:
        if literal not in contents:
            raise AuditError(
                f"license record/build pin mismatch in {path.relative_to(ROOT)}: {literal}"
            )


def validate_build_pins() -> None:
    require_literals(
        ROOT / "oci/desktop/Containerfile",
        [
            "# syntax=docker/dockerfile:1.7@sha256:b5f3b260a9678e1d83d2fce86eeddf79420b79147eaba2a25986f47133d73720",
            "FROM docker.io/library/debian:sid@sha256:900a6f89c05e3f3323f274eb9ce3bb2d35695fac097360dfc6f1cfe2e921996b",
            "https://keyring.openresearchtools.com",
            '"buzzardoscua=${BUZZARDOS_CUA_VERSION}"',
            '"buzzardos-guest=${BUZZARDOS_GUEST_VERSION}"',
            '"buzzardos-desktop=${BUZZARDOS_DESKTOP_VERSION}"',
        ],
    )
    require_literals(
        ROOT / "oci/desktop/Containerfile.cuda",
        [
            "ARG NV_CUDA_CUDART_VERSION=13.3.29-1",
            "ARG NV_CUDA_LIB_VERSION=13.3.1-1",
            "ARG NV_CUDA_COMPAT_VERSION=610.43.02-1",
            "ARG NV_LIBCUBLAS_VERSION=13.6.0.2-1",
            "ARG NV_LIBNCCL_PACKAGE_VERSION=2.30.7-1+cuda13.3",
        ],
    )


def containerfile_apt_blocks() -> list[list[str]]:
    contents = (ROOT / "oci/desktop/Containerfile").read_text(encoding="utf-8")
    normalized = re.sub(r"\\\r?\n", " ", contents)
    pattern = re.compile(
        r"apt-get\s+-o\s+\S+\s+install\s+--yes\s+--no-install-recommends\s+"
        r"(.*?)\s+&&"
    )
    blocks = [shlex.split(match) for match in pattern.findall(normalized)]
    return [
        block
        for block in blocks
        if not all(item.startswith("/tmp/") for item in block)
        and not any("=" in item for item in block)
    ]


def validate_package_inputs() -> None:
    data = read_toml(LICENSES / "package-inputs.toml")
    records = sorted(data.get("apt_block", []), key=lambda item: item.get("order", 0))
    orders = [record.get("order") for record in records]
    if orders != list(range(1, len(records) + 1)):
        raise AuditError("package-inputs apt blocks have invalid ordering")
    expected = [record.get("packages", []) for record in records]
    actual = containerfile_apt_blocks()
    if actual != expected:
        raise AuditError(
            "oci/desktop/Containerfile apt inputs changed without updating "
            "LICENSES/package-inputs.toml"
        )

    containerfile = (ROOT / "oci/desktop/Containerfile").read_text(encoding="utf-8")
    direct_records = sorted(
        data.get("direct_package_block", []), key=lambda item: item.get("order", 0)
    )
    direct_orders = [record.get("order") for record in direct_records]
    if direct_orders != list(range(len(records) + 1, len(records) + len(direct_records) + 1)):
        raise AuditError("package-inputs direct package blocks have invalid ordering")
    for record in direct_records:
        packages = record.get("packages", [])
        checksums = record.get("sha256", [])
        if not packages or len(packages) != len(checksums):
            raise AuditError("direct package block has incomplete package/checksum evidence")
        for checksum in checksums:
            if not re.fullmatch(r"[0-9a-f]{64}", checksum) or checksum not in containerfile:
                raise AuditError(
                    "direct package checksum changed without updating "
                    "LICENSES/package-inputs.toml"
                )
        for package in packages:
            name, separator, version = package.partition("=")
            if not separator or not name or not version or name not in containerfile:
                raise AuditError(
                    "direct package identity changed without updating "
                    "LICENSES/package-inputs.toml"
                )
            variable = re.fullmatch(r"\$\{([A-Z0-9_]+)\}", version)
            if variable is not None:
                if f"ARG {variable.group(1)}=" not in containerfile:
                    raise AuditError(
                        "direct package version pin changed without updating "
                        "LICENSES/package-inputs.toml"
                    )
            elif version not in containerfile:
                raise AuditError(
                    "direct package version changed without updating "
                    "LICENSES/package-inputs.toml"
                )

    repository_records = sorted(
        data.get("repository_package_block", []), key=lambda item: item.get("order", 0)
    )
    repository_orders = [record.get("order") for record in repository_records]
    expected_start = len(records) + len(direct_records) + 1
    if repository_orders != list(
        range(expected_start, expected_start + len(repository_records))
    ):
        raise AuditError("package-inputs repository package blocks have invalid ordering")
    for record in repository_records:
        relative = record.get("containerfile", "")
        path = (ROOT / relative).resolve()
        if not relative or not path.is_relative_to(ROOT) or not path.is_file():
            raise AuditError("repository package block has an invalid Containerfile")
        contents = path.read_text(encoding="utf-8")
        key_checksum = record.get("repository_key_sha256", "")
        if (
            re.fullmatch(r"[0-9a-f]{64}", key_checksum) is None
            or key_checksum not in contents
        ):
            raise AuditError("repository package block has no pinned repository key evidence")
        packages = record.get("packages", [])
        if not packages:
            raise AuditError("repository package block has no packages")
        for package in packages:
            name, separator, version = package.partition("=")
            variable = re.fullmatch(r"\$\{([A-Z0-9_]+)\}", version)
            if not separator or not name or variable is None or name not in contents:
                raise AuditError("repository package block has an invalid package pin")
            if f"ARG {variable.group(1)}=" not in contents:
                raise AuditError("repository package version pin is missing from Containerfile")

    actual_images = [
        value
        for value in re.findall(r"^FROM(?:\s+--platform=\S+)?\s+(\S+)", containerfile, re.MULTILINE)
        if value != "scratch"
    ]
    expected_images: list[str] = []
    for record in data.get("base_image", []):
        pinned_reference = f'{record["reference"]}@{record["digest"]}'
        expected_images.extend([pinned_reference] * len(record.get("used_by", [])))
    if sorted(actual_images) != sorted(expected_images):
        raise AuditError(
            "Containerfile base images changed without updating LICENSES/package-inputs.toml"
        )



def source_notice_hashes() -> set[str]:
    sources = read_toml(LICENSES / "upstream/SOURCES.toml")
    return {record["sha256"] for record in sources.get("file", [])}


def validate_rust_runtime_record() -> None:
    rust_runtime = read_toml(LICENSES / "rust-runtime.toml")
    if not re.fullmatch(r"[0-9a-f]{64}", rust_runtime.get("standard_library_notice_sha256", "")):
        raise AuditError("Rust standard-library notice record has no checksum")


def asset_files() -> list[str]:
    output = run(
        [
            "git",
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "guest/assets",
            "host/packaging",
        ]
    )
    return sorted(
        path
        for path in output.splitlines()
        if path and (ROOT / path).is_file()
    )


def asset_matches(path: str, record: dict) -> bool:
    included = any(fnmatch.fnmatchcase(path, pattern) for pattern in record.get("paths", []))
    excluded = any(fnmatch.fnmatchcase(path, pattern) for pattern in record.get("exclude", []))
    return included and not excluded


def validate_assets() -> list[str]:
    data = read_toml(LICENSES / "guest-assets.toml")
    records = data.get("asset", [])
    issues: list[str] = []
    identifiers: set[str] = set()
    for path in asset_files():
        matches = [record for record in records if asset_matches(path, record)]
        if not matches:
            raise AuditError(f"repository asset has no licensing classification: {path}")
        if len(matches) > 1:
            raise AuditError(f"repository asset has overlapping classifications: {path}")
    for record in records:
        identifier = record.get("id", "")
        if not identifier or identifier in identifiers:
            raise AuditError(f"invalid or duplicate asset id: {identifier!r}")
        identifiers.add(identifier)
        matched = False
        for pattern in record.get("paths", []):
            glob_pattern = pattern + "/*" if pattern.endswith("/**") else pattern
            matched = matched or any(path.is_file() for path in ROOT.glob(glob_pattern))
        if not matched:
            raise AuditError(f"asset classification matches no files: {identifier}")
        for evidence in record.get("evidence", []):
            relative = evidence.split(":", 1)[0]
            if not (ROOT / relative).is_file():
                raise AuditError(f"asset {identifier} evidence missing: {relative}")
        if record.get("license") == "NOASSERTION" or BLOCKING_STATUS.match(
            record.get("status", "")
        ):
            issues.append(
                f"asset {record.get('id', '<unnamed>')}: {record.get('status', 'NOASSERTION')}"
            )
    return issues


def component_blockers() -> list[str]:
    data = read_toml(LICENSES / "release-components.toml")
    identifiers: set[str] = set()
    issues: list[str] = []
    for component in data.get("component", []):
        identifier = component.get("id", "")
        if not identifier or identifier in identifiers:
            raise AuditError(f"invalid or duplicate release component id: {identifier!r}")
        identifiers.add(identifier)
        for key, value in component.items():
            if key != "evidence" and not key.endswith("_evidence"):
                continue
            evidence_values = value if isinstance(value, list) else [value]
            for evidence in evidence_values:
                if key == "artifact_evidence":
                    surface, separator, artifact_path = evidence.partition(":")
                    if (
                        separator != ":"
                        or surface not in {"host-app", "oci"}
                        or not artifact_path.startswith("/")
                        or artifact_path.endswith("/")
                        or "//" in artifact_path
                        or "\\" in artifact_path
                        or any(
                            part in {"", ".", ".."}
                            for part in artifact_path.removeprefix("/").split("/")
                        )
                        or re.fullmatch(r"/[A-Za-z0-9._+@/-]+", artifact_path) is None
                    ):
                        raise AuditError(
                            f"component {identifier} has invalid artifact evidence: {evidence}"
                        )
                    continue
                relative = evidence
                if not (ROOT / relative).is_file():
                    raise AuditError(f"component {identifier} evidence missing: {relative}")
        status = component.get("status", "")
        if BLOCKING_STATUS.match(status):
            issues.append(f"component {identifier}: {status}")
    return issues


def load_oci_package_inventory(path: Path) -> tuple[list[tuple[str, str]], str]:
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise AuditError(
            f"missing generated OCI inventory {path.relative_to(ROOT)}"
        ) from error
    if not raw or not raw.endswith(b"\n") or b"\r" in raw:
        raise AuditError("OCI package inventory must be non-empty LF-terminated UTF-8")
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise AuditError("OCI package inventory is not valid UTF-8") from error

    rows: list[tuple[str, str]] = []
    seen: set[str] = set()
    package_pattern = re.compile(r"^[a-z0-9][a-z0-9+.-]*(?::[a-z0-9][a-z0-9-]*)?$")
    for number, line in enumerate(text.splitlines(), start=1):
        fields = line.split("\t")
        if len(fields) != 2 or not all(fields):
            raise AuditError(f"invalid OCI package inventory row {number}")
        package, version = fields
        if package_pattern.fullmatch(package) is None:
            raise AuditError(f"invalid OCI package inventory row {number}")
        if package in seen:
            raise AuditError(f"duplicate OCI package inventory entry: {package}")
        seen.add(package)
        rows.append((package, version))
    if rows != sorted(rows):
        raise AuditError("OCI package inventory is not sorted deterministically")
    return rows, sha256_bytes(raw)


def validate_oci_package_inventory_record() -> None:
    data = read_toml(LICENSES / "release-components.toml")
    component_ids = {
        "standard": "local-machine-build-evidence",
        "cuda": "local-cuda-machine-build-evidence",
    }
    for variant, inventory in OCI_PACKAGE_INVENTORIES.items():
        rows, digest = load_oci_package_inventory(inventory)
        identifier = component_ids[variant]
        matches = [
            component
            for component in data.get("component", [])
            if component.get("id") == identifier
        ]
        if len(matches) != 1:
            raise AuditError(
                f"{variant} machine-build evidence component is missing or duplicated"
            )
        component = matches[0]
        expected_evidence = str(inventory.relative_to(ROOT))
        if expected_evidence not in component.get("evidence", []):
            raise AuditError(
                f"{variant} machine-build evidence does not cite its inventory"
            )
        if component.get("package_count") != len(rows):
            raise AuditError(
                f"{variant} machine-build count differs from generated inventory"
            )
        if component.get("package_inventory_sha256") != digest:
            raise AuditError(
                f"{variant} machine-build checksum differs from generated inventory"
            )
        if (
            re.fullmatch(
                r"sha256:[0-9a-f]{64}", str(component.get("image_id", ""))
            )
            is None
        ):
            raise AuditError(f"{variant} machine-build evidence has invalid image_id")
        if (
            component.get("status")
            != "local-verification-evidence-not-a-distributed-buzzard-artifact"
        ):
            raise AuditError(
                f"{variant} machine-build status must exclude release distribution"
            )



def verify_copy(root: Path, destination: str, source: Path, issues: list[str], label: str) -> None:
    target = root / destination
    if not target.is_file():
        issues.append(f"{label} notice missing: {destination}")
    elif sha256_file(target) != sha256_file(source):
        issues.append(f"{label} notice differs from audited source: {destination}")



def verify_hash(
    root: Path,
    destination: str,
    expected: str,
    issues: list[str],
    label: str,
) -> None:
    target = root / destination
    if not target.is_file():
        issues.append(f"{label} payload missing: {destination}")
    elif sha256_file(target) != expected:
        issues.append(f"{label} payload checksum mismatch: {destination}")



def audit_debian_package(archive: Path) -> list[str]:
    if not archive.is_file():
        raise AuditError(f"Debian package does not exist: {archive}")
    package = run(["dpkg-deb", "--field", str(archive), "Package"]).strip()
    # The local side-by-side host build ships the identical licensed component,
    # under its own installation paths; it is not a fifth component.
    component = "buzzardos" if package == "buzzardos-pod" else package
    inventories = {
        "buzzardos": GENERATED / "cargo-host.tsv",
        "buzzardos-guest": GENERATED / "cargo-buzzardos-guest.tsv",
        "buzzardos-desktop": GENERATED / "cargo-buzzardos-desktop.tsv",
        "buzzardoscua": GENERATED / "cargo-cua.tsv",
    }
    inventory = inventories.get(component)
    if inventory is None:
        raise AuditError(f"not a Buzzard binary package: {package or archive.name}")
    package_sources: dict[str, Path] = {
        "copyright": ROOT / f"packaging/copyright/{component}",
        "LICENSE": ROOT / "LICENSE",
        "NOTICE": ROOT / "NOTICE",
        "THIRD_PARTY_NOTICES.md": LICENSES / f"package-notices/{component}.md",
        "RUST_DEPENDENCY_LICENSES.txt": GENERATED
        / f"RUST_DEPENDENCY_LICENSES.{component}.txt",
    }
    package_sources["cargo-host.tsv" if component == "buzzardos" else "cargo-dependencies.tsv"] = (
        inventory
    )
    if package == "buzzardoscua":
        package_sources["cargo-cua.tsv"] = package_sources.pop("cargo-dependencies.tsv")
        package_sources.update(
            {
                "LICENSE.trycua-cua.md": CUA_ROOT / "LICENSE.trycua.md",
                "CITATION.cff": CUA_ROOT / "CITATION.cff",
                "UPSTREAM.toml": CUA_ROOT / "UPSTREAM.toml",
                "CHANGES.BUZZARDOS.md": CUA_ROOT / "CHANGES.BUZZARDOS.md",
                "virtual-keyboard-unstable-v1.xml": CUA_ROOT
                / "protocol/virtual-keyboard-unstable-v1.xml",
            }
        )

    issues: list[str] = []
    with tempfile.TemporaryDirectory(prefix="buzzardos-deb-audit-") as temporary:
        root = Path(temporary) / "root"
        run(["dpkg-deb", "--extract", str(archive), str(root)])
        document_root = root / "usr/share/doc"
        document_packages = (
            {path.name for path in document_root.iterdir() if path.is_dir()}
            if document_root.is_dir()
            else set()
        )
        if document_packages != {package}:
            issues.append(
                f"{package} documentation crosses package boundaries: "
                + ", ".join(sorted(document_packages))
            )
        for filename, source in package_sources.items():
            verify_copy(
                root,
                f"usr/share/doc/{package}/{filename}",
                source,
                issues,
                package,
            )
        rust_runtime = read_toml(LICENSES / "rust-runtime.toml")
        verify_hash(
            root,
            f"usr/share/doc/{package}/rust/COPYRIGHT-library.html",
            rust_runtime["standard_library_notice_sha256"],
            issues,
            package,
        )
        expected_mpl = {
            f"{name}-{version}.crate": checksum
            for name, version, checksum, _url in mpl_source_records_for_inventory(
                inventory
            )
        }
        mpl_root = root / f"usr/share/doc/{package}/sources/mpl"
        actual_mpl = (
            {path.name for path in mpl_root.iterdir() if path.is_file()}
            if mpl_root.is_dir()
            else set()
        )
        if actual_mpl != set(expected_mpl):
            issues.append(f"{package} MPL source set differs from its Cargo closure")
        for filename, checksum in expected_mpl.items():
            verify_hash(
                root,
                f"usr/share/doc/{package}/sources/mpl/{filename}",
                checksum,
                issues,
                package,
            )
    print(f"inspected Debian package: {package} ({archive.name})")
    return issues


def dpkg_status(rootfs: Path) -> list[dict[str, str]]:
    path = rootfs / "var/lib/dpkg/status"
    if not path.is_file():
        raise AuditError(f"guest rootfs has no dpkg status: {path}")
    packages = []
    current: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines() + [""]:
        if not line:
            if current.get("Package") and current.get("Status", "").endswith(" installed"):
                packages.append(current)
            current = {}
        elif line[0].isspace():
            continue
        else:
            key, separator, value = line.partition(": ")
            if separator:
                current[key] = value
    return packages


def audit_guest_rootfs(rootfs: Path) -> list[str]:
    if not rootfs.is_dir():
        raise AuditError(f"guest rootfs does not exist: {rootfs}")
    issues: list[str] = []
    packages = dpkg_status(rootfs)
    is_cuda = any(package["Package"] == "cuda-keyring" for package in packages)
    inventory = OCI_PACKAGE_INVENTORIES["cuda" if is_cuda else "standard"]
    recorded_inventory, _digest = load_oci_package_inventory(inventory)
    observed_inventory = sorted(
        (
            package["Package"]
            + (
                f":{package.get('Architecture', '')}"
                if package.get("Multi-Arch") == "same"
                else ""
            ),
            package.get("Version", ""),
        )
        for package in packages
    )
    if observed_inventory != recorded_inventory:
        issues.append(
            "OCI installed package inventory differs from "
            f"{inventory.relative_to(ROOT)}"
        )
    for package in packages:
        name = package["Package"]
        if not (rootfs / "usr/share/doc" / name / "copyright").exists():
            issues.append(
                f"OCI package has no copyright file: {name}={package.get('Version', '?')}"
            )
    required = {
        "usr/share/doc/buzzardos-guest/copyright": ROOT
        / "packaging/copyright/buzzardos-guest",
        "usr/share/doc/buzzardos-guest/LICENSE": ROOT / "LICENSE",
        "usr/share/doc/buzzardos-guest/NOTICE": ROOT / "NOTICE",
        "usr/share/doc/buzzardos-guest/THIRD_PARTY_NOTICES.md": LICENSES
        / "package-notices/buzzardos-guest.md",
        "usr/share/doc/buzzardos-guest/RUST_DEPENDENCY_LICENSES.txt": GENERATED
        / "RUST_DEPENDENCY_LICENSES.buzzardos-guest.txt",
        "usr/share/doc/buzzardos-guest/cargo-dependencies.tsv": GENERATED
        / "cargo-buzzardos-guest.tsv",
        "usr/share/doc/buzzardos-desktop/copyright": ROOT
        / "packaging/copyright/buzzardos-desktop",
        "usr/share/doc/buzzardos-desktop/LICENSE": ROOT / "LICENSE",
        "usr/share/doc/buzzardos-desktop/NOTICE": ROOT / "NOTICE",
        "usr/share/doc/buzzardos-desktop/THIRD_PARTY_NOTICES.md": LICENSES
        / "package-notices/buzzardos-desktop.md",
        "usr/share/doc/buzzardos-desktop/RUST_DEPENDENCY_LICENSES.txt": GENERATED
        / "RUST_DEPENDENCY_LICENSES.buzzardos-desktop.txt",
        "usr/share/doc/buzzardos-desktop/cargo-dependencies.tsv": GENERATED
        / "cargo-buzzardos-desktop.tsv",
        "usr/share/doc/buzzardoscua/copyright": ROOT
        / "packaging/copyright/buzzardoscua",
        "usr/share/doc/buzzardoscua/LICENSE": ROOT / "LICENSE",
        "usr/share/doc/buzzardoscua/THIRD_PARTY_NOTICES.md": LICENSES
        / "package-notices/buzzardoscua.md",
        "usr/share/doc/buzzardoscua/RUST_DEPENDENCY_LICENSES.txt": GENERATED
        / "RUST_DEPENDENCY_LICENSES.buzzardoscua.txt",
        "usr/share/doc/buzzardoscua/cargo-cua.tsv": GENERATED / "cargo-cua.tsv",
        "usr/share/doc/buzzardoscua/LICENSE.trycua-cua.md": CUA_ROOT / "LICENSE.trycua.md",
        "usr/share/doc/buzzardoscua/CITATION.cff": CUA_ROOT / "CITATION.cff",
        "usr/share/doc/buzzardoscua/UPSTREAM.toml": CUA_ROOT / "UPSTREAM.toml",
        "usr/share/doc/buzzardoscua/CHANGES.BUZZARDOS.md": CUA_ROOT / "CHANGES.BUZZARDOS.md",
        "usr/share/doc/buzzardoscua/virtual-keyboard-unstable-v1.xml": CUA_ROOT / "protocol/virtual-keyboard-unstable-v1.xml",
    }
    for destination, source in required.items():
        verify_copy(rootfs, destination, source, issues, "OCI")
    rust_runtime = read_toml(LICENSES / "rust-runtime.toml")
    package_inventories = {
        "buzzardos-guest": GENERATED / "cargo-buzzardos-guest.tsv",
        "buzzardos-desktop": GENERATED / "cargo-buzzardos-desktop.tsv",
        "buzzardoscua": GENERATED / "cargo-cua.tsv",
    }
    for package, inventory in package_inventories.items():
        verify_hash(
            rootfs,
            f"usr/share/doc/{package}/rust/COPYRIGHT-library.html",
            rust_runtime["standard_library_notice_sha256"],
            issues,
            "OCI",
        )
        for name, version, checksum, _url in mpl_source_records_for_inventory(inventory):
            verify_hash(
                rootfs,
                f"usr/share/doc/{package}/sources/mpl/{name}-{version}.crate",
                checksum,
                issues,
                "OCI",
            )
    if is_cuda:
        for relative in [
            "usr/share/doc/cuda-cudart-13-3/copyright",
            "usr/share/doc/libcublas-13-3/copyright",
        ]:
            if not (rootfs / relative).is_file():
                issues.append(f"OCI notice missing: {relative}")
        verify_hash(
            rootfs,
            "usr/share/doc/cuda-keyring/copyright",
            CUDA_KEYRING_LICENSE_SHA256,
            issues,
            "OCI",
        )
        cudart_notice = rootfs / "usr/share/doc/cuda-cudart-13-3/copyright"
        meta_notice = rootfs / "usr/share/doc/cuda-libraries-13-3/copyright"
        if (
            cudart_notice.is_file()
            and meta_notice.is_file()
            and sha256_file(cudart_notice) != sha256_file(meta_notice)
        ):
            issues.append(
                "OCI CUDA libraries metapackage notice differs from the installed CUDA EULA"
            )
    print(f"inspected OCI rootfs: {len(packages)} installed dpkg packages")
    return issues


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--generate",
        action="store_true",
        help="rewrite the deterministic locked Cargo inventories and notice bundle",
    )
    parser.add_argument(
        "--structural",
        action="store_true",
        help="validate evidence while reporting, but not failing on, recorded release blockers",
    )
    parser.add_argument("--guest-rootfs", type=Path, help="also audit an extracted OCI rootfs")
    parser.add_argument(
        "--deb",
        type=Path,
        action="append",
        default=[],
        help="also audit an exact Buzzard Debian package; may be repeated",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        validate_provenance()
        outputs, cargo_issues = cargo_outputs()
        validate_generated(outputs, args.generate)
        issues = component_blockers() + validate_assets() + cargo_issues
        artifact_issues: list[str] = []
        if args.guest_rootfs is not None:
            artifact_issues.extend(audit_guest_rootfs(args.guest_rootfs.resolve()))
        for archive in args.deb:
            artifact_issues.extend(audit_debian_package(archive.resolve()))
    except (AuditError, OSError, ValueError, json.JSONDecodeError) as error:
        print(f"license audit error: {error}", file=sys.stderr)
        return 2

    all_issues = issues + artifact_issues
    if all_issues:
        print("release licensing blockers:", file=sys.stderr)
        for issue in sorted(set(all_issues)):
            print(f"- {issue}", file=sys.stderr)
    if artifact_issues or (issues and not args.structural):
        return 1
    print("license evidence and locked Cargo inventories are structurally consistent")
    return 0


if __name__ == "__main__":
    sys.exit(main())
