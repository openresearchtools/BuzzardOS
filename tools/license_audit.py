#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Generate and verify Wild Buzzard's release licensing evidence."""

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
import tarfile
import tempfile
import tomllib
from typing import Iterable
import zipfile


ROOT = Path(__file__).resolve().parents[1]
LICENSES = ROOT / "LICENSES"
GENERATED = LICENSES / "generated"
HOST_MANIFEST = ROOT / "host/Cargo.toml"
HOST_LOCK = ROOT / "host/Cargo.lock"
GUEST_MANIFEST = ROOT / "guest/Cargo.toml"
GUEST_LOCK = ROOT / "guest/Cargo.lock"
CUA_ROOT = ROOT / "guest/third_party/trycua-cua"
CUA_MANIFEST = CUA_ROOT / "cua-driver/rust/Cargo.toml"
CUA_LOCK = CUA_ROOT / "cua-driver/rust/Cargo.lock"
TARGET = "x86_64-unknown-linux-gnu"
OCI_PACKAGE_INVENTORY = GENERATED / "oci-packages.tsv"

NON_DPKG_APPDIR_ELFS = {
    "usr/bin/wildbuzzard",
    "usr/bin/wildbuzzard-broker",
    "usr/bin/wildbuzzard-cua-driver",
    "usr/bin/wildbuzzard-display",
    "usr/bin/wildbuzzard-shell",
    "usr/lib/libnvidia-container-go.so.1.19.1",
    "usr/lib/libnvidia-container.so.1.19.1",
    "usr/libexec/wildbuzzard/crane",
    "usr/libexec/wildbuzzard/nvidia-cdi-hook",
    "usr/libexec/wildbuzzard/nvidia-container-cli",
    "usr/libexec/wildbuzzard/nvidia-ctk",
    "usr/libexec/wildbuzzard/slirp4netns",
    "usr/share/doc/wildbuzzard/sources/appimage-runtime/relink-kit/objects/runtime.o",
}
NON_DPKG_APPDIR_MIRRORS = {
    "usr/bin/nvidia-cdi-hook": "usr/libexec/wildbuzzard/nvidia-cdi-hook",
    "usr/bin/nvidia-container-cli": "usr/libexec/wildbuzzard/nvidia-container-cli",
    "usr/bin/nvidia-ctk": "usr/libexec/wildbuzzard/nvidia-ctk",
    "usr/bin/slirp4netns": "usr/libexec/wildbuzzard/slirp4netns",
}
NON_DPKG_APPDIR_ELFS.update(NON_DPKG_APPDIR_MIRRORS)

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


def cargo_release_keys(manifest: Path, package: str | None) -> set[tuple[str, str]]:
    command = [
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
    if package is None:
        command.append("--workspace")
    else:
        command.extend(["--package", package])
    keys: set[tuple[str, str]] = set()
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
    package: str | None,
    fallbacks: dict[str, dict],
) -> tuple[str, list[dict], dict[str, dict]]:
    metadata = cargo_metadata(manifest)
    release_keys = cargo_release_keys(manifest, package)
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
        "Wild Buzzard locked Rust dependency license and notice files\n",
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
        "guest-shell", GUEST_MANIFEST, GUEST_LOCK, "wildbuzzard-shell", fallbacks
    )
    cua_tsv, cua_local, cua_contents = build_cargo_graph(
        "cua-driver", CUA_MANIFEST, CUA_LOCK, "cua-driver", fallbacks
    )
    issues = [
        f"local Cargo package lacks license metadata: {item['package']}"
        for item in host_local + guest_local + cua_local
    ]
    return (
        {
            GENERATED / "cargo-host.tsv": host_tsv,
            GENERATED / "cargo-guest.tsv": guest_tsv,
            GENERATED / "cargo-cua.tsv": cua_tsv,
            GENERATED / "RUST_DEPENDENCY_LICENSES.txt": merged_notice_bundle(
                [host_contents, guest_contents, cua_contents]
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
        LICENSES / "crane-dependencies.toml",
        LICENSES / "nvidia-go-dependencies.toml",
        LICENSES / "go-runtime.toml",
        LICENSES / "go-source-archives.tsv",
        LICENSES / "slirp4netns-sources.tsv",
        LICENSES / "appimage-runtime-dependencies.toml",
        LICENSES / "rust-runtime.toml",
        CUA_ROOT / "LICENSE.md",
        CUA_ROOT / "CITATION.cff",
        CUA_ROOT / "UPSTREAM.toml",
        CUA_ROOT / "CHANGES.WILDBUZZARD.md",
        ROOT / "oci/desktop/SWAY_UPSTREAM.toml",
    ]
    for path in required:
        if not path.is_file() or path.stat().st_size == 0:
            raise AuditError(f"required licensing evidence missing: {path.relative_to(ROOT)}")
    upstream = read_toml(CUA_ROOT / "UPSTREAM.toml")
    expected_commit = "10279552e2bbe479e367a082f78b1b98ee85a697"
    if upstream.get("upstream_commit") != expected_commit or upstream.get("license") != "MIT":
        raise AuditError("TryCua upstream commit/license record changed unexpectedly")
    sway = read_toml(ROOT / "oci/desktop/SWAY_UPSTREAM.toml")
    if sway.get("sway", {}).get("commit") != "88869399f421d9180dd8b6ed0b5a1f4a3585d252":
        raise AuditError("Sway source pin changed without a license record update")
    if sway.get("wlroots", {}).get("commit") != "d783533489e1f75d6886c2ab5c5960090ef268f8":
        raise AuditError("wlroots source pin changed without a license record update")
    validate_upstream_sources()
    validate_build_pins()
    validate_package_inputs()
    validate_embedded_dependency_records()
    validate_mpl_sources()
    validate_go_source_archives()
    validate_slirp4netns_sources()
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


def go_source_archive_records() -> list[tuple[str, str, str, str, str]]:
    records: list[tuple[str, str, str, str, str]] = []
    identifiers: set[str] = set()
    archive_names: set[str] = set()
    with (LICENSES / "go-source-archives.tsv").open(encoding="utf-8", newline="") as source:
        for row in csv.reader(source, delimiter="\t"):
            if not row or row[0].startswith("#"):
                continue
            if len(row) != 5:
                raise AuditError("invalid Go source archive manifest row")
            identifier, archive_name, url, checksum, license_expression = row
            if identifier in identifiers or archive_name in archive_names:
                raise AuditError(f"duplicate Go source archive record: {identifier}")
            if not re.fullmatch(r"[A-Za-z0-9._@+-]+", identifier):
                raise AuditError(f"invalid Go source archive id: {identifier}")
            if not re.fullmatch(r"[A-Za-z0-9._@+-]+", archive_name):
                raise AuditError(f"invalid Go source archive name: {archive_name}")
            if not re.fullmatch(r"[0-9a-f]{64}", checksum):
                raise AuditError(f"invalid Go source archive checksum: {identifier}")
            if not url.startswith("https://") or not license_expression:
                raise AuditError(f"incomplete Go source archive record: {identifier}")
            identifiers.add(identifier)
            archive_names.add(archive_name)
            records.append((identifier, archive_name, url, checksum, license_expression))
    return records


def go_proxy_escape(module: str) -> str:
    return "".join(f"!{character.lower()}" if character.isupper() else character for character in module)


def validate_go_source_archives() -> None:
    records = go_source_archive_records()
    by_identifier = {record[0]: record for record in records}
    go_runtime = read_toml(LICENSES / "go-runtime.toml")
    go_versions = {
        binary.get("go_version", "").removeprefix("go")
        for binary in go_runtime.get("binary", [])
    }
    expected_identifiers: set[str] = set()
    for version in go_versions:
        identifier = f"go-{version}"
        expected_identifiers.add(identifier)
        record = by_identifier.get(identifier)
        if record is None:
            raise AuditError(f"Go compiler source archive missing: {identifier}")
        if record[1] != f"go{version}.src.tar.gz" or record[2] != (
            f"https://go.dev/dl/go{version}.src.tar.gz"
        ):
            raise AuditError(f"Go compiler source archive pin is inconsistent: {identifier}")

    module_sets = validate_go_module_records(LICENSES / "nvidia-go-dependencies.toml")
    modules = set().union(*module_sets.values())
    for module, version in modules:
        identifier = f"module-{module.replace('/', '_')}@{version}"
        expected_identifiers.add(identifier)
        record = by_identifier.get(identifier)
        if record is None:
            raise AuditError(f"NVIDIA Go module source archive missing: {module}@{version}")
        expected_archive = f"{module.replace('/', '_')}@{version}.zip"
        expected_url = f"https://proxy.golang.org/{go_proxy_escape(module)}/@v/{version}.zip"
        if record[1] != expected_archive or record[2] != expected_url:
            raise AuditError(f"NVIDIA Go module source pin is inconsistent: {module}@{version}")
    if set(by_identifier) != expected_identifiers:
        raise AuditError("Go source archive manifest has an unexpected record set")


def slirp4netns_source_records() -> list[tuple[str, str, str]]:
    records: list[tuple[str, str, str]] = []
    with (LICENSES / "slirp4netns-sources.tsv").open(
        encoding="utf-8", newline=""
    ) as source:
        for row in csv.reader(source, delimiter="\t"):
            if not row or row[0].startswith("#"):
                continue
            if len(row) != 3:
                raise AuditError("invalid slirp4netns source manifest row")
            records.append((row[0], row[1], row[2]))
    return records


def validate_slirp4netns_sources() -> None:
    expected = {
        (
            "slirp4netns_1.3.3-1.dsc",
            "3cabaaca6123b7cc442029b216f97c64177c97ad0714a350493384417bd0ef28",
        ),
        (
            "slirp4netns_1.3.3.orig.tar.gz",
            "2422cd6869da0374943ba8f01425fe50a49a29138ddead0e0fedbe3aa22aa483",
        ),
        (
            "slirp4netns_1.3.3-1.debian.tar.xz",
            "d1618f4596c9fb2e10bb3a8e568cf98b2495a40603baaebc47c0e3ec97d085fc",
        ),
    }
    records = slirp4netns_source_records()
    actual = {(name, checksum) for name, _url, checksum in records}
    if actual != expected or len(records) != len(expected):
        raise AuditError("slirp4netns source manifest differs from the audited package")
    base = "https://archive.ubuntu.com/ubuntu/pool/universe/s/slirp4netns/"
    for name, url, checksum in records:
        if url != base + name or not re.fullmatch(r"[0-9a-f]{64}", checksum):
            raise AuditError(f"invalid slirp4netns source pin: {name}")


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
        ROOT / "host/build-appimage.sh",
        [
            "crane_version=v0.21.8",
            "crane_sha256=59b59f68ee37aba51f5523d69ec779ee925d9be4e279f9220eca357267f2ee67",
            "slirp_package_version=1.3.3-1",
            "slirp_deb_sha256=dda3ca5101c58e9585bfd6e7b9d26831090327120cfb5092172ead355f968dd4",
            "slirp_binary_sha256=20581c54ee53ae32e908c9b318481e5a71b72a13f850ce41722e402cb524b325",
            "linuxdeploy_version=1-alpha-20251107-1",
            "linuxdeploy_sha256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d",
            "appimage_runtime_sha256=a861c1b4c90ea8a3968753db768c647b068f563929992dc97ffdbce90247a7e6",
            "appimage_runtime_relink_manifest_sha256=86468af49dcbe4c24067a6a6091eff2c6676063d4a39e6407f011d26a25f2828",
            "appimage_runtime_metadata_sha256=73928c6597ace903a8662b66f72906f142ffb2a9c2848b7249a75364c659ef11",
            "zig_version=0.14.1",
            "zig_sha256=24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c",
            "cargo_zigbuild_version=0.21.8",
            "nvidia_toolkit_version=1.19.1-1",
            "nvidia_toolkit_base_sha256=b6c5b4e77a28cde0197cc0e64edf75538604775d9f8aea502cef667e7e5b2132",
            "nvidia_container_tools_sha256=5642763d51961a2295dff09990048a5dcee81edbea2a8c5084e47b09ccf17268",
            "nvidia_container_library_sha256=d73bb582af893135198ef81cb22135c790a75d2ad72910446477c6c4430f3e6b",
        ],
    )
    require_literals(
        ROOT / "tools/build-appimage-runtime.sh",
        [
            "runtime_commit=75849dce7cc37e4319b633df1f116ca895c71a12",
            "runtime_epoch=1782256868",
            "zig_version=0.14.1",
            "zig_archive_sha256=24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c",
            "runtime_archive_sha256=b7af4960da4b90364e935a3281d04fad6560da4813c012414fa2f738291ad443",
            "fuse_archive_sha256=70589cfd5e1cff7ccd6ac91c86c01be340b227285c5e200baa284e401eea2ca0",
            "squashfuse_archive_sha256=db0238c5981dabbd80ee09ae15387f390091668ca060a7bc38047912491443d3",
            "zstd_download_sha256=8c29e06cf42aacc1eafc4077ae2ec6c6fcb96a626157e0593d5e82a34fd403c1",
            "zstd_archive_sha256=30f35f71c1203369dc979ecde0400ffea93c27391bfd2ac5a9715d2173d92ff7",
            "zlib_archive_url=https://github.com/madler/zlib/archive/refs/tags/v1.3.2.tar.gz",
            "zlib_archive_sha256=b99a0b86c0ba9360ec7e78c4f1e43b1cbdf1e6936c8fa0f6835c0cd694a495a1",
            "mimalloc_archive_sha256=0eed39319f139afde8515010ff59baf24de9e47ea316a315398e8027d198202d",
            "meson_wheel_sha256=82c6818dc81743c96de3a458f06175776ebfde4081195ea31ea6971838f25e38",
            "-target x86_64-linux-musl",
            "-Wl,--build-id=none -Wl,--strip-debug",
            "BUILD-INPUTS.sha256",
        ],
    )
    require_literals(
        ROOT / "oci/desktop/Containerfile",
        [
            "# syntax=docker/dockerfile:1.7@sha256:b5f3b260a9678e1d83d2fce86eeddf79420b79147eaba2a25986f47133d73720",
            "FROM docker.io/library/rust:1.96-slim@sha256:d8f0d5c09580253ecdd6d6894ff112b2b760683ff2a74585e5189f2578728ce4 AS shell-builder",
            "FROM docker.io/library/rust:1.96-slim@sha256:d8f0d5c09580253ecdd6d6894ff112b2b760683ff2a74585e5189f2578728ce4 AS cua-builder",
            "FROM docker.io/library/debian:sid@sha256:900a6f89c05e3f3323f274eb9ce3bb2d35695fac097360dfc6f1cfe2e921996b AS sway-builder",
            "FROM docker.io/library/debian:sid@sha256:900a6f89c05e3f3323f274eb9ce3bb2d35695fac097360dfc6f1cfe2e921996b",
            "ARG SWAY_COMMIT=88869399f421d9180dd8b6ed0b5a1f4a3585d252",
            "ARG WLROOTS_COMMIT=d783533489e1f75d6886c2ab5c5960090ef268f8",
            "ARG CUDA_CUDART_VERSION=13.1.80-1",
            "ARG CUDA_CUBLAS_VERSION=13.2.2.2-1",
        ],
    )


def containerfile_apt_blocks() -> list[list[str]]:
    contents = (ROOT / "oci/desktop/Containerfile").read_text(encoding="utf-8")
    normalized = re.sub(r"\\\r?\n", " ", contents)
    pattern = re.compile(
        r"apt-get\s+-o\s+\S+\s+install\s+--yes\s+--no-install-recommends\s+"
        r"(.*?)\s+&&"
    )
    return [shlex.split(match) for match in pattern.findall(normalized)]


def appimage_notice_loop_packages() -> list[str]:
    contents = (ROOT / "host/build-appimage.sh").read_text(encoding="utf-8")
    normalized = re.sub(r"\\\r?\n", " ", contents)
    match = re.search(
        r"for package in\s+(.*?)\s*;\s*do\s+"
        r'copyright="/usr/share/doc/\$package/copyright"',
        normalized,
        re.DOTALL,
    )
    if match is None:
        raise AuditError("cannot locate the AppImage host-package notice loop")
    return shlex.split(match.group(1))


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

    configured = data.get("appimage_host_payload", {}).get("notice_loop_packages", [])
    if appimage_notice_loop_packages() != configured:
        raise AuditError(
            "host/build-appimage.sh notice packages changed without updating "
            "LICENSES/package-inputs.toml"
        )


def source_notice_hashes() -> set[str]:
    sources = read_toml(LICENSES / "upstream/SOURCES.toml")
    return {record["sha256"] for record in sources.get("file", [])}


def validate_go_module_records(path: Path) -> dict[str, set[tuple[str, str]]]:
    data = read_toml(path)
    result: dict[str, set[tuple[str, str]]] = {}
    for binary in data.get("binary", []):
        name = binary.get("name", "")
        if not name or name in result:
            raise AuditError(f"invalid or duplicate Go binary record in {path.relative_to(ROOT)}")
        modules = {
            (module.get("path", ""), module.get("version", ""))
            for module in binary.get("modules", [])
        }
        if any(not module or not version for module, version in modules):
            raise AuditError(f"incomplete Go module record for {name}")
        if len(modules) != len(binary.get("modules", [])):
            raise AuditError(f"duplicate Go module record for {name}")
        result[name] = modules
    return result


def validate_embedded_dependency_records() -> None:
    crane = read_toml(LICENSES / "crane-dependencies.toml")
    modules = crane.get("module", [])
    pairs = {(item.get("path", ""), item.get("version", "")) for item in modules}
    if len(pairs) != 11 or len(pairs) != len(modules):
        raise AuditError("crane dependency inventory is incomplete or contains duplicates")
    recorded_hashes = source_notice_hashes()
    for module in modules:
        if not module.get("module_sum", "").startswith("h1:"):
            raise AuditError(f"crane module has no Go sum: {module.get('path', '?')}")
        if not re.fullmatch(r"[0-9a-f]{64}", module.get("archive_sha256", "")):
            raise AuditError(f"crane module has no archive checksum: {module.get('path', '?')}")
        if not module.get("license") or not module.get("license_files"):
            raise AuditError(f"crane module has no license evidence: {module.get('path', '?')}")
        for notice in module["license_files"]:
            _, separator, digest = notice.rpartition("@sha256:")
            if not separator or digest not in recorded_hashes:
                raise AuditError(
                    f"crane module notice is not preserved: {module.get('path', '?')} {notice}"
                )

    runtime = read_toml(LICENSES / "appimage-runtime-dependencies.toml")
    expected_runtime = {
        "runtime_source_commit": "75849dce7cc37e4319b633df1f116ca895c71a12",
        "runtime_source_archive_sha256": "b7af4960da4b90364e935a3281d04fad6560da4813c012414fa2f738291ad443",
        "runtime_target": "x86_64-linux-musl",
        "runtime_binary_sha256": "a861c1b4c90ea8a3968753db768c647b068f563929992dc97ffdbce90247a7e6",
        "runtime_binary_size": 455096,
        "appimage_marker_offset": 1024,
        "static_marker_offset": 2048,
        "appimage_mutable_region_offset": 442480,
        "appimage_mutable_region_size": 16,
        "toolchain_sha256": "24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c",
        "relink_manifest_sha256": "86468af49dcbe4c24067a6a6091eff2c6676063d4a39e6407f011d26a25f2828",
        "runtime_metadata_sha256": "73928c6597ace903a8662b66f72906f142ffb2a9c2848b7249a75364c659ef11",
    }
    if any(runtime.get(key) != value for key, value in expected_runtime.items()):
        raise AuditError("AppImage runtime dependency record changed without updating its pin")
    if runtime.get("runtime_static_pie") is not True:
        raise AuditError("AppImage runtime record does not require a static PIE")
    expected_runtime_inputs = {
        "type2-runtime-75849dce.tar.gz": "b7af4960da4b90364e935a3281d04fad6560da4813c012414fa2f738291ad443",
        "fuse-3.15.0.tar.xz": "70589cfd5e1cff7ccd6ac91c86c01be340b227285c5e200baa284e401eea2ca0",
        "squashfuse-0.5.2.tar.gz": "db0238c5981dabbd80ee09ae15387f390091668ca060a7bc38047912491443d3",
        "zstd-1.5.6.tar.gz": "30f35f71c1203369dc979ecde0400ffea93c27391bfd2ac5a9715d2173d92ff7",
        "zlib-1.3.2.tar.gz": "b99a0b86c0ba9360ec7e78c4f1e43b1cbdf1e6936c8fa0f6835c0cd694a495a1",
        "mimalloc-2.1.7.tar.gz": "0eed39319f139afde8515010ff59baf24de9e47ea316a315398e8027d198202d",
        "meson-1.7.2-py3-none-any.whl": "82c6818dc81743c96de3a458f06175776ebfde4081195ea31ea6971838f25e38",
    }
    recorded_runtime_inputs = {
        dependency.get("archive", ""): dependency.get("source_sha256", "")
        for dependency in runtime.get("dependency", [])
        if dependency.get("relink_kit_input") is True
    }
    if recorded_runtime_inputs != expected_runtime_inputs:
        raise AuditError("AppImage runtime source/relink input set differs from its pin")
    zstd_record = next(
        (
            item
            for item in runtime.get("dependency", [])
            if item.get("name") == "zstd"
        ),
        None,
    )
    if zstd_record is None or zstd_record.get("download_sha256") != (
        "8c29e06cf42aacc1eafc4077ae2ec6c6fcb96a626157e0593d5e82a34fd403c1"
    ) or "gzip -n" not in zstd_record.get("normalization", ""):
        raise AuditError(
            "zstd upstream download and deterministic normalization are not recorded"
        )
    libfuse = next(
        (item for item in runtime.get("dependency", []) if item.get("name") == "libfuse"),
        None,
    )
    if libfuse is None or libfuse.get("license") != "LGPL-2.1-only" or (
        libfuse.get("relinkable") is not True
    ):
        raise AuditError("AppImage runtime libfuse relinkability record is incomplete")
    release_components = read_toml(LICENSES / "release-components.toml")
    release_runtime = next(
        (
            item
            for item in release_components.get("component", [])
            if item.get("id") == "appimage-type2-runtime"
        ),
        None,
    )
    if release_runtime is None or any(
        release_runtime.get(component_key) != runtime.get(runtime_key)
        for component_key, runtime_key in {
            "source_commit": "runtime_source_commit",
            "source_archive_sha256": "runtime_source_archive_sha256",
            "sha256": "runtime_binary_sha256",
            "runtime_size": "runtime_binary_size",
            "digest_patch_offset": "appimage_mutable_region_offset",
            "digest_patch_size": "appimage_mutable_region_size",
            "relink_manifest_sha256": "relink_manifest_sha256",
            "runtime_metadata_sha256": "runtime_metadata_sha256",
        }.items()
    ):
        raise AuditError("AppImage runtime release component differs from its dependency record")
    for dependency in runtime.get("dependency", []):
        for evidence in dependency.get("evidence", []):
            evidence_path = ROOT / evidence
            if not evidence_path.is_file():
                raise AuditError(f"AppImage runtime evidence missing: {evidence}")

    nvidia = validate_go_module_records(LICENSES / "nvidia-go-dependencies.toml")
    if set(nvidia) != {"nvidia-ctk", "nvidia-cdi-hook"}:
        raise AuditError("NVIDIA Go dependency inventory has an unexpected binary set")

    go_runtime = read_toml(LICENSES / "go-runtime.toml")
    expected_go_binaries = {
        "crane": ("go1.26.5", "764901b59be6583890901f6c3b87e3ecb41dce7e10b58ee2772eb0b3b7e7f4c7"),
        "nvidia-ctk": ("go1.26.3", "891cc1c4055da8e98892d6e3ade5aae87c11b0b1e17115e74c2861d86e2f6eb9"),
        "nvidia-cdi-hook": ("go1.26.3", "cca9969335a8d84d59611ee3da0de9c7942ad2202e927c75ec0735df79052a75"),
    }
    actual_go_binaries = {
        item.get("name"): (item.get("go_version"), item.get("sha256"))
        for item in go_runtime.get("binary", [])
    }
    if actual_go_binaries != expected_go_binaries:
        raise AuditError("Go runtime inventory differs from the pinned helper binaries")
    if go_runtime.get("root_license_sha256") not in recorded_hashes:
        raise AuditError("Go runtime root license is not preserved")
    if go_runtime.get("patent_grant_sha256") not in recorded_hashes:
        raise AuditError("Go runtime patent grant is not preserved")

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
    return sorted(path for path in output.splitlines() if path)


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
                        or surface not in {"appimage", "oci"}
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


def load_oci_package_inventory() -> tuple[list[tuple[str, str]], str]:
    try:
        raw = OCI_PACKAGE_INVENTORY.read_bytes()
    except OSError as error:
        raise AuditError(
            f"missing generated OCI inventory {OCI_PACKAGE_INVENTORY.relative_to(ROOT)}"
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
    rows, digest = load_oci_package_inventory()
    data = read_toml(LICENSES / "release-components.toml")
    matches = [
        component
        for component in data.get("component", [])
        if component.get("id") == "oci-debian-package-closure"
    ]
    if len(matches) != 1:
        raise AuditError("OCI package closure release component is missing or duplicated")
    component = matches[0]
    expected_evidence = str(OCI_PACKAGE_INVENTORY.relative_to(ROOT))
    if expected_evidence not in component.get("evidence", []):
        raise AuditError("OCI package closure does not cite its generated inventory")
    if component.get("package_count") != len(rows):
        raise AuditError("OCI package closure count differs from generated inventory")
    if component.get("package_inventory_sha256") != digest:
        raise AuditError("OCI package closure checksum differs from generated inventory")
    for key in ["image_manifest_digest", "image_config_digest", "archive_sha256"]:
        if re.fullmatch(r"(?:sha256:)?[0-9a-f]{64}", str(component.get(key, ""))) is None:
            raise AuditError(f"OCI package closure has invalid {key}")
    if component.get("status") != "current-built-image-package-inventory-recorded-and-audited":
        raise AuditError("OCI package closure status does not describe the recorded build")


def elf_files(root: Path) -> Iterable[Path]:
    for path in root.rglob("*"):
        if not path.is_file() or path.is_symlink():
            continue
        try:
            with path.open("rb") as source:
                if source.read(4) == b"\x7fELF":
                    yield path
        except OSError:
            continue


def elf_build_id(path: Path, cache: dict[Path, str]) -> str:
    resolved = path.resolve()
    if resolved in cache:
        return cache[resolved]
    completed = subprocess.run(
        ["readelf", "--notes", str(resolved)],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    match = re.search(r"Build ID: ([0-9a-fA-F]+)", completed.stdout)
    value = match.group(1).lower() if match is not None else ""
    cache[resolved] = value
    return value


def dpkg_candidates(basenames: set[str]) -> dict[str, set[tuple[str, Path]]]:
    result = {basename: set() for basename in basenames}
    if not basenames:
        return result
    completed = subprocess.run(
        ["dpkg-query", "-S", *[f"*/{name}" for name in sorted(basenames)]],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    for line in completed.stdout.splitlines():
        owner_field, separator, installed_path = line.partition(": ")
        if not separator:
            continue
        host_path = Path("/") / installed_path.lstrip("/")
        basename = host_path.name
        if basename not in result:
            continue
        for owner in owner_field.split(", "):
            if owner:
                result[basename].add((owner, host_path))
    return result


def dpkg_versions(packages: set[str]) -> dict[str, str]:
    if not packages:
        return {}
    completed = subprocess.run(
        [
            "dpkg-query",
            "-W",
            "-f=${binary:Package}\\t${Version}\\n",
            *sorted(packages),
        ],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode != 0:
        raise AuditError(
            "cannot read build-host package versions: "
            + (completed.stderr.strip() or completed.stdout.strip())
        )
    versions: dict[str, str] = {}
    for line in completed.stdout.splitlines():
        binary_package, separator, version = line.partition("\t")
        if not separator or not binary_package or not version:
            raise AuditError(f"invalid dpkg-query version row: {line!r}")
        previous = versions.setdefault(binary_package, version)
        if previous != version:
            raise AuditError(
                f"multiple installed versions found for package {binary_package}"
            )
    missing = packages - versions.keys()
    if missing:
        raise AuditError(
            "build-host package versions are missing: " + ", ".join(sorted(missing))
        )
    return versions


def appdir_host_closure(
    appdir: Path,
) -> tuple[list[tuple[str, str, str, str]], list[str], int]:
    payloads = [
        path
        for path in elf_files(appdir)
        if path.relative_to(appdir).as_posix() not in NON_DPKG_APPDIR_ELFS
    ]
    candidates = dpkg_candidates({path.name for path in payloads})
    build_ids: dict[Path, str] = {}
    path_owners: list[tuple[str, set[str]]] = []
    issues: list[str] = []
    all_owners: set[str] = set()
    for path in sorted(payloads, key=lambda item: item.relative_to(appdir).as_posix()):
        relative = path.relative_to(appdir).as_posix()
        appdir_build_id = elf_build_id(path, build_ids)
        owners: set[str] = set()
        for package, host_path in sorted(candidates.get(path.name, set())):
            if not host_path.is_file():
                continue
            if appdir_build_id:
                matches = elf_build_id(host_path, build_ids) == appdir_build_id
            else:
                matches = sha256_file(host_path) == sha256_file(path)
            if matches:
                owners.add(package)
        if not owners:
            issues.append(
                f"AppDir ELF has no exact build-host package mapping: {relative}"
            )
        path_owners.append((relative, owners))
        all_owners.update(owners)

    versions = dpkg_versions(all_owners)
    copyright_hashes: dict[str, str] = {}
    for package in sorted(all_owners):
        document_package = package.split(":", 1)[0]
        copyright_path = Path("/usr/share/doc") / document_package / "copyright"
        if not copyright_path.is_file():
            issues.append(f"build-host package copyright is missing: {copyright_path}")
            continue
        copyright_hashes[package] = sha256_file(copyright_path)

    rows: list[tuple[str, str, str, str]] = []
    for relative, owners in path_owners:
        for package in sorted(owners):
            copyright_hash = copyright_hashes.get(package)
            if copyright_hash is not None:
                rows.append((relative, package, versions[package], copyright_hash))
    return rows, issues, len(payloads)


def render_host_closure(rows: list[tuple[str, str, str, str]]) -> str:
    output = [
        "# Wild Buzzard AppImage build-host package copyright closure v1",
        "# appdir_path\tpackage\tversion\tcopyright_sha256",
    ]
    output.extend("\t".join(row) for row in rows)
    return "\n".join(output) + "\n"


def verify_appdir_host_notices(appdir: Path) -> tuple[list[str], int]:
    rows, issues, payload_count = appdir_host_closure(appdir)
    packages = {row[1] for row in rows}
    for package in sorted(packages):
        document_package = package.split(":", 1)[0]
        source = Path("/usr/share/doc") / document_package / "copyright"
        destination = appdir / "usr/share/doc" / document_package / "copyright"
        if not destination.is_file():
            issues.append(
                f"AppDir host-package notice missing: {destination.relative_to(appdir)}"
            )
        elif sha256_file(destination) != sha256_file(source):
            issues.append(
                "AppDir host-package notice differs from build host: "
                f"{destination.relative_to(appdir)}"
            )
    manifest = appdir / "usr/share/doc/wildbuzzard/host-package-closure.tsv"
    expected_manifest = render_host_closure(rows).encode("utf-8")
    if not manifest.is_file():
        issues.append(
            "AppDir host-package closure manifest missing: "
            "usr/share/doc/wildbuzzard/host-package-closure.tsv"
        )
    elif manifest.read_bytes() != expected_manifest:
        issues.append("AppDir host-package closure manifest differs from audited payload")
    return issues, payload_count


def stage_appdir_host_notices(appdir: Path) -> None:
    if not appdir.is_dir():
        raise AuditError(f"AppDir does not exist: {appdir}")
    rows, issues, payload_count = appdir_host_closure(appdir)
    if issues:
        raise AuditError("; ".join(issues))
    for package in sorted({row[1] for row in rows}):
        document_package = package.split(":", 1)[0]
        atomic_copy(
            Path("/usr/share/doc") / document_package / "copyright",
            appdir / "usr/share/doc" / document_package / "copyright",
        )
    atomic_write(
        appdir / "usr/share/doc/wildbuzzard/host-package-closure.tsv",
        render_host_closure(rows),
    )
    verification_issues, _ = verify_appdir_host_notices(appdir)
    if verification_issues:
        raise AuditError("; ".join(verification_issues))
    print(
        "staged AppDir host-package notices: "
        f"{payload_count} ELF payloads, {len({row[1] for row in rows})} packages"
    )


def verify_copy(root: Path, destination: str, source: Path, issues: list[str], label: str) -> None:
    target = root / destination
    if not target.is_file():
        issues.append(f"{label} notice missing: {destination}")
    elif sha256_file(target) != sha256_file(source):
        issues.append(f"{label} notice differs from audited source: {destination}")


def go_build_modules(path: Path) -> set[tuple[str, str]]:
    try:
        data = path.read_bytes()
    except OSError as error:
        raise AuditError(f"cannot read Go binary build information: {path}") from error
    return {
        (module.decode("utf-8"), version.decode("utf-8"))
        for module, version in re.findall(
            rb"(?:^|\n)dep\t([^\t\n]+)\t([^\t\n]+)\t", data
        )
    }


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


def archive_notice_members(path: Path) -> dict[str, bytes]:
    notices: dict[str, bytes] = {}

    def is_notice(name: str) -> bool:
        basename = name.rstrip("/").rsplit("/", 1)[-1]
        return re.match(r"^(?:LICENSE|COPYING|NOTICE|PATENTS)", basename, re.I) is not None

    if path.name.endswith(".zip"):
        with zipfile.ZipFile(path) as archive:
            for member in archive.infolist():
                if member.is_dir() or not is_notice(member.filename):
                    continue
                notices[member.filename] = archive.read(member)
    else:
        with tarfile.open(path, "r:gz") as archive:
            for member in archive.getmembers():
                if not (member.isfile() or member.islnk()) or not is_notice(member.name):
                    continue
                extracted = archive.extractfile(member)
                if extracted is None:
                    raise AuditError(f"cannot read notice member {member.name} from {path}")
                notices[member.name] = extracted.read()
    return notices


def audit_appdir_go_sources(appdir: Path) -> list[str]:
    issues: list[str] = []
    root = appdir / "usr/share/doc/wildbuzzard/sources/go"
    records = go_source_archive_records()
    expected_archives = {record[1] for record in records}
    archive_directory = root / "archives"
    actual_archives = (
        {path.name for path in archive_directory.iterdir() if path.is_file()}
        if archive_directory.is_dir()
        else set()
    )
    if actual_archives != expected_archives:
        issues.append("AppDir Go source archive set differs from the audited manifest")

    expected_notices: dict[str, bytes] = {}
    for identifier, archive_name, _url, checksum, _license in records:
        archive_path = archive_directory / archive_name
        if not archive_path.is_file():
            issues.append(f"AppDir Go source archive missing: {archive_name}")
            continue
        if sha256_file(archive_path) != checksum:
            issues.append(f"AppDir Go source archive checksum mismatch: {archive_name}")
            continue
        for member, contents in archive_notice_members(archive_path).items():
            expected_notices[f"{identifier}/{member}"] = contents

    notice_directory = root / "notices"
    actual_notice_paths = (
        {
            path.relative_to(notice_directory).as_posix()
            for path in notice_directory.rglob("*")
            if path.is_file() and not path.is_symlink()
        }
        if notice_directory.is_dir()
        else set()
    )
    if actual_notice_paths != set(expected_notices):
        issues.append("AppDir Go source notice set differs from the source archives")
    else:
        for relative, contents in expected_notices.items():
            if (notice_directory / relative).read_bytes() != contents:
                issues.append(f"AppDir Go source notice differs from archive: {relative}")

    expected_checksums = "".join(
        f"{checksum}  {archive_name}\n"
        for _identifier, archive_name, _url, checksum, _license in sorted(
            records, key=lambda record: record[1]
        )
    ).encode("utf-8")
    checksums_path = root / "SHA256SUMS"
    if not checksums_path.is_file() or checksums_path.read_bytes() != expected_checksums:
        issues.append("AppDir Go source SHA256SUMS is missing or non-deterministic")

    expected_license_index = (
        "# id\tarchive\tlicense-expression\n"
        + "".join(
            f"{identifier}\t{archive_name}\t{license_expression}\n"
            for identifier, archive_name, _url, _checksum, license_expression in records
        )
    ).encode("utf-8")
    license_index = root / "LICENSES.tsv"
    if not license_index.is_file() or license_index.read_bytes() != expected_license_index:
        issues.append("AppDir Go source LICENSES.tsv differs from the audited manifest")
    return issues


def audit_appdir_runtime_kit(appdir: Path) -> list[str]:
    issues: list[str] = []
    runtime = read_toml(LICENSES / "appimage-runtime-dependencies.toml")
    root = appdir / "usr/share/doc/wildbuzzard/sources/appimage-runtime"
    metadata_path = root / "runtime-metadata.toml"
    kit = root / "relink-kit"
    manifest_path = kit / "BUILD-INPUTS.sha256"

    if not metadata_path.is_file():
        issues.append("AppDir AppImage runtime metadata is missing")
    else:
        if sha256_file(metadata_path) != runtime["runtime_metadata_sha256"]:
            issues.append("AppDir AppImage runtime metadata checksum mismatch")
        else:
            metadata = read_toml(metadata_path)
            expected_metadata = {
                "schema": 1,
                "target": runtime["runtime_target"],
                "elf_type": runtime["runtime_elf_type"],
                "static_pie": True,
                "runtime_source_commit": runtime["runtime_source_commit"],
                "source_date_epoch": runtime["runtime_source_date_epoch"],
                "zig_version": "0.14.1",
                "zig_archive_sha256": runtime["toolchain_sha256"],
                "runtime_sha256": runtime["runtime_binary_sha256"],
                "runtime_size": runtime["runtime_binary_size"],
                "appimage_marker_offset": runtime["appimage_marker_offset"],
                "static_marker_offset": runtime["static_marker_offset"],
                "digest_patch_offset": runtime["appimage_mutable_region_offset"],
                "digest_patch_size": runtime["appimage_mutable_region_size"],
                "relink_manifest_sha256": runtime["relink_manifest_sha256"],
            }
            if metadata != expected_metadata:
                issues.append("AppDir AppImage runtime metadata fields differ from the audit")

    if not manifest_path.is_file():
        issues.append("AppDir AppImage runtime relink manifest is missing")
        return issues
    if sha256_file(manifest_path) != runtime["relink_manifest_sha256"]:
        issues.append("AppDir AppImage runtime relink manifest checksum mismatch")
        return issues

    recorded: dict[str, str] = {}
    for line in manifest_path.read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  \./([A-Za-z0-9._+@/-]+)", line)
        if match is None:
            issues.append("AppDir AppImage runtime relink manifest has an invalid row")
            return issues
        checksum, relative = match.groups()
        parts = relative.split("/")
        if any(part in {"", ".", ".."} for part in parts) or relative in recorded:
            issues.append("AppDir AppImage runtime relink manifest has an unsafe path")
            return issues
        recorded[relative] = checksum

    actual: set[str] = set()
    if kit.is_dir():
        for path in kit.rglob("*"):
            if path == manifest_path:
                continue
            relative = path.relative_to(kit).as_posix()
            if path.is_symlink():
                issues.append(f"AppDir AppImage runtime relink kit contains symlink: {relative}")
            elif path.is_file():
                actual.add(relative)
    if actual != set(recorded):
        issues.append("AppDir AppImage runtime relink kit file set differs from its manifest")
        return issues
    for relative, expected in recorded.items():
        if sha256_file(kit / relative) != expected:
            issues.append(f"AppDir AppImage runtime relink file checksum mismatch: {relative}")

    expected_inputs = {
        dependency["archive"]: dependency["source_sha256"]
        for dependency in runtime.get("dependency", [])
        if dependency.get("relink_kit_input") is True
    }
    actual_inputs = {
        relative.removeprefix("inputs/"): checksum
        for relative, checksum in recorded.items()
        if relative.startswith("inputs/")
    }
    if actual_inputs != expected_inputs:
        issues.append("AppDir AppImage runtime source archive set differs from the audit")
    required_relink_files = {
        "README.md",
        "build-appimage-runtime.sh",
        "relink.sh",
        "objects/data-sections-zig.ld",
        "objects/libmimalloc.a",
        "objects/libsquashfuse.a",
        "objects/libsquashfuse_ll.a",
        "objects/libz.a",
        "objects/libzstd.a",
        "objects/runtime.o",
    }
    if not required_relink_files.issubset(recorded):
        issues.append("AppDir AppImage runtime relink object set is incomplete")
    for command in ["build-appimage-runtime.sh", "relink.sh"]:
        command_path = kit / command
        if command_path.is_file() and not os.access(command_path, os.X_OK):
            issues.append(f"AppDir AppImage runtime command is not executable: {command}")
    return issues


def audit_appdir(appdir: Path) -> list[str]:
    if not appdir.is_dir():
        raise AuditError(f"AppDir does not exist: {appdir}")
    issues: list[str] = []
    required = {
        "usr/share/doc/wildbuzzard/LICENSE": ROOT / "LICENSE",
        "usr/share/doc/wildbuzzard/NOTICE": ROOT / "NOTICE",
        "usr/share/doc/wildbuzzard/THIRD_PARTY_NOTICES.md": ROOT / "THIRD_PARTY_NOTICES.md",
        "usr/share/doc/wildbuzzard-cua/LICENSE.trycua-cua.md": CUA_ROOT / "LICENSE.md",
        "usr/share/doc/wildbuzzard-cua/CITATION.cff": CUA_ROOT / "CITATION.cff",
        "usr/share/doc/wildbuzzard-cua/UPSTREAM.toml": CUA_ROOT / "UPSTREAM.toml",
        "usr/share/doc/wildbuzzard-cua/CHANGES.WILDBUZZARD.md": CUA_ROOT / "CHANGES.WILDBUZZARD.md",
        "usr/share/doc/wildbuzzard-cua/Inter-OFL.txt": CUA_ROOT / "cua-driver/rust/crates/cursor-overlay/assets/Inter-OFL.txt",
        "usr/share/doc/wildbuzzard-cua/virtual-keyboard-unstable-v1.xml": CUA_ROOT / "cua-driver/rust/crates/platform-linux/protocol/virtual-keyboard-unstable-v1.xml",
    }
    for source in sorted(path for path in LICENSES.rglob("*") if path.is_file()):
        relative = source.relative_to(LICENSES).as_posix()
        required[f"usr/share/doc/wildbuzzard/licenses/{relative}"] = source
    for destination, source in required.items():
        verify_copy(appdir, destination, source, issues, "AppDir")

    expected_hashes = {
        "usr/libexec/wildbuzzard/crane": "764901b59be6583890901f6c3b87e3ecb41dce7e10b58ee2772eb0b3b7e7f4c7",
        "usr/libexec/wildbuzzard/slirp4netns": "20581c54ee53ae32e908c9b318481e5a71b72a13f850ce41722e402cb524b325",
    }
    nvidia_data = read_toml(LICENSES / "nvidia-go-dependencies.toml")
    for binary in nvidia_data.get("binary", []):
        expected_hashes[f"usr/libexec/wildbuzzard/{binary['name']}"] = binary["sha256"]
    for destination, expected in expected_hashes.items():
        verify_hash(appdir, destination, expected, issues, "AppDir")
    for mirror, canonical in NON_DPKG_APPDIR_MIRRORS.items():
        mirror_path = appdir / mirror
        canonical_path = appdir / canonical
        if not mirror_path.is_file():
            issues.append(f"AppDir linuxdeploy helper mirror missing: {mirror}")
        elif not canonical_path.is_file():
            issues.append(f"AppDir pinned helper payload missing: {canonical}")
        else:
            build_ids: dict[Path, str] = {}
            mirror_build_id = elf_build_id(mirror_path, build_ids)
            canonical_build_id = elf_build_id(canonical_path, build_ids)
            same_payload = (
                bool(mirror_build_id)
                and mirror_build_id == canonical_build_id
            ) or (
                not mirror_build_id
                and not canonical_build_id
                and sha256_file(mirror_path) == sha256_file(canonical_path)
            )
            if not same_payload:
                issues.append(
                    "AppDir linuxdeploy helper mirror differs from pinned payload: "
                    f"{mirror}"
                )
    rust_runtime = read_toml(LICENSES / "rust-runtime.toml")
    verify_hash(
        appdir,
        "usr/share/doc/wildbuzzard/rust/COPYRIGHT-library.html",
        rust_runtime["standard_library_notice_sha256"],
        issues,
        "AppDir",
    )
    for name, version, checksum, _url in mpl_source_records():
        verify_hash(
            appdir,
            f"usr/share/doc/wildbuzzard/sources/mpl/{name}-{version}.crate",
            checksum,
            issues,
            "AppDir",
        )
    slirp_source_root = appdir / "usr/share/doc/wildbuzzard/sources/slirp4netns"
    expected_slirp_checksums = "".join(
        f"{checksum}  {name}\n"
        for name, _url, checksum in sorted(slirp4netns_source_records())
    ).encode("utf-8")
    for name, _url, checksum in slirp4netns_source_records():
        verify_hash(
            appdir,
            f"usr/share/doc/wildbuzzard/sources/slirp4netns/{name}",
            checksum,
            issues,
            "AppDir",
        )
    checksum_file = slirp_source_root / "SHA256SUMS"
    if not checksum_file.is_file() or checksum_file.read_bytes() != expected_slirp_checksums:
        issues.append("AppDir slirp4netns source SHA256SUMS differs from the audited manifest")
    expected_mpl_archives = {
        f"{name}-{version}.crate" for name, version, _checksum, _url in mpl_source_records()
    }
    mpl_directory = appdir / "usr/share/doc/wildbuzzard/sources/mpl"
    actual_mpl_archives = (
        {path.name for path in mpl_directory.iterdir() if path.is_file()}
        if mpl_directory.is_dir()
        else set()
    )
    if actual_mpl_archives != expected_mpl_archives:
        issues.append("AppDir MPL source archive set differs from the locked graph")
    issues.extend(audit_appdir_runtime_kit(appdir))
    issues.extend(audit_appdir_go_sources(appdir))

    crane_path = appdir / "usr/libexec/wildbuzzard/crane"
    if crane_path.is_file():
        actual_crane = go_build_modules(crane_path)
        actual_crane.discard(("github.com/google/go-containerregistry", "(devel)"))
        expected_crane = {
            (module["path"], module["version"])
            for module in read_toml(LICENSES / "crane-dependencies.toml").get("module", [])
        }
        if actual_crane != expected_crane:
            issues.append("AppDir crane Go dependency inventory differs from the audited set")

    expected_nvidia = validate_go_module_records(LICENSES / "nvidia-go-dependencies.toml")
    for binary, expected_modules in expected_nvidia.items():
        binary_path = appdir / "usr/libexec/wildbuzzard" / binary
        if binary_path.is_file() and go_build_modules(binary_path) != expected_modules:
            issues.append(f"AppDir {binary} Go dependency inventory differs from the audited set")

    for package in [
        "slirp4netns",
        "nvidia-container-toolkit-base",
        "libnvidia-container-tools",
        "libnvidia-container1",
    ]:
        if not (appdir / "usr/share/doc" / package / "copyright").is_file():
            issues.append(f"AppDir notice missing: usr/share/doc/{package}/copyright")
    host_notice_issues, mapped_payloads = verify_appdir_host_notices(appdir)
    issues.extend(host_notice_issues)
    print(f"inspected AppDir: {mapped_payloads} mapped ELF payloads")
    return issues


def audit_appimage(appimage: Path) -> list[str]:
    if not appimage.is_file():
        raise AuditError(f"AppImage does not exist: {appimage}")
    runtime = read_toml(LICENSES / "appimage-runtime-dependencies.toml")
    runtime_size = int(runtime["runtime_binary_size"])
    patch_offset = int(runtime["appimage_mutable_region_offset"])
    patch_size = int(runtime["appimage_mutable_region_size"])
    with appimage.open("rb") as source:
        prefix = bytearray(source.read(runtime_size))
    if len(prefix) != runtime_size:
        raise AuditError("AppImage is shorter than its recorded type-2 runtime")
    if prefix[8:11] != b"AI\x02":
        raise AuditError("AppImage does not carry the type-2 AppImage magic")
    marker_offset = int(runtime["appimage_marker_offset"])
    static_offset = int(runtime["static_marker_offset"])
    if prefix[marker_offset : marker_offset + 3] != b"AI\x02":
        raise AuditError("AppImage runtime has no canonical .appimage marker")
    if prefix[static_offset : static_offset + 6] != b"static":
        raise AuditError("AppImage runtime has no canonical .static marker")
    if patch_offset < 0 or patch_size <= 0 or patch_offset + patch_size > runtime_size:
        raise AuditError("AppImage runtime mutable digest region is out of range")
    prefix[patch_offset : patch_offset + patch_size] = b"\0" * patch_size
    issues: list[str] = []
    if sha256_bytes(bytes(prefix)) != runtime["runtime_binary_sha256"]:
        issues.append("AppImage runtime differs from the checksum-pinned runtime outside .digest_md5")

    unsquashfs = shutil.which("unsquashfs")
    if unsquashfs is None:
        raise AuditError("unsquashfs is required to audit a final AppImage")
    with tempfile.TemporaryDirectory(prefix="wildbuzzard-license-appimage-") as temporary:
        extracted = Path(temporary) / "AppDir"
        completed = subprocess.run(
            [
                unsquashfs,
                "-no-progress",
                "-offset",
                str(runtime_size),
                "-dest",
                str(extracted),
                str(appimage),
            ],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        if completed.returncode != 0:
            raise AuditError(f"cannot extract AppImage for license audit: {completed.stderr.strip()}")
        issues.extend(audit_appdir(extracted))
    print(f"inspected final AppImage: {appimage}")
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
    recorded_inventory, _digest = load_oci_package_inventory()
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
            f"{OCI_PACKAGE_INVENTORY.relative_to(ROOT)}"
        )
    for package in packages:
        name = package["Package"]
        if not (rootfs / "usr/share/doc" / name / "copyright").exists():
            issues.append(
                f"OCI package has no copyright file: {name}={package.get('Version', '?')}"
            )
    required = {
        "usr/share/doc/wildbuzzard/LICENSE": ROOT / "LICENSE",
        "usr/share/doc/wildbuzzard/NOTICE": ROOT / "NOTICE",
        "usr/share/doc/wildbuzzard/THIRD_PARTY_NOTICES.md": ROOT / "THIRD_PARTY_NOTICES.md",
        "usr/share/doc/wildbuzzard/RUST_DEPENDENCY_LICENSES.txt": GENERATED / "RUST_DEPENDENCY_LICENSES.txt",
        "usr/share/doc/wildbuzzard/cargo-guest.tsv": GENERATED / "cargo-guest.tsv",
        "usr/share/doc/wildbuzzard/cargo-cua.tsv": GENERATED / "cargo-cua.tsv",
        "usr/share/doc/wildbuzzard-cua/LICENSE.trycua-cua.md": CUA_ROOT / "LICENSE.md",
        "usr/share/doc/wildbuzzard-cua/CITATION.cff": CUA_ROOT / "CITATION.cff",
        "usr/share/doc/wildbuzzard-cua/UPSTREAM.toml": CUA_ROOT / "UPSTREAM.toml",
        "usr/share/doc/wildbuzzard-cua/CHANGES.WILDBUZZARD.md": CUA_ROOT / "CHANGES.WILDBUZZARD.md",
        "usr/share/doc/wildbuzzard-cua/Inter-OFL.txt": CUA_ROOT / "cua-driver/rust/crates/cursor-overlay/assets/Inter-OFL.txt",
        "usr/share/doc/wildbuzzard-cua/virtual-keyboard-unstable-v1.xml": CUA_ROOT / "cua-driver/rust/crates/platform-linux/protocol/virtual-keyboard-unstable-v1.xml",
        "usr/share/doc/wildbuzzard-sway/LICENSE.sway": LICENSES / "upstream/sway-1.12-LICENSE",
        "usr/share/doc/wildbuzzard-sway/LICENSE.wlroots": LICENSES / "upstream/wlroots-0.20.2-LICENSE",
        "usr/share/doc/wildbuzzard-sway/UPSTREAM.toml": ROOT / "oci/desktop/SWAY_UPSTREAM.toml",
    }
    for destination, source in required.items():
        verify_copy(rootfs, destination, source, issues, "OCI")
    rust_runtime = read_toml(LICENSES / "rust-runtime.toml")
    verify_hash(
        rootfs,
        "usr/share/doc/wildbuzzard/rust/COPYRIGHT-library.html",
        rust_runtime["standard_library_notice_sha256"],
        issues,
        "OCI",
    )
    for name, version, checksum, _url in mpl_source_records():
        verify_hash(
            rootfs,
            f"usr/share/doc/wildbuzzard/sources/mpl/{name}-{version}.crate",
            checksum,
            issues,
            "OCI",
        )
    for relative in [
        "usr/share/doc/cuda-cudart-13-1/copyright",
        "usr/share/doc/libcublas-13-1/copyright",
    ]:
        if not (rootfs / relative).is_file():
            issues.append(f"OCI notice missing: {relative}")
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
    parser.add_argument("--appdir", type=Path, help="also audit a built AppDir")
    parser.add_argument(
        "--stage-appdir-host-notices",
        type=Path,
        help="copy and record the exact build-host package notices after linuxdeploy",
    )
    parser.add_argument(
        "--appimage",
        type=Path,
        help="audit the pinned runtime and extracted contents of a final AppImage",
    )
    parser.add_argument("--guest-rootfs", type=Path, help="also audit an extracted OCI rootfs")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.stage_appdir_host_notices is not None:
        if any(
            [
                args.generate,
                args.structural,
                args.appdir is not None,
                args.appimage is not None,
                args.guest_rootfs is not None,
            ]
        ):
            print(
                "license audit error: --stage-appdir-host-notices cannot be combined "
                "with audit options",
                file=sys.stderr,
            )
            return 2
        try:
            stage_appdir_host_notices(args.stage_appdir_host_notices.resolve())
        except (AuditError, OSError, ValueError) as error:
            print(f"license audit error: {error}", file=sys.stderr)
            return 2
        return 0
    try:
        validate_provenance()
        outputs, cargo_issues = cargo_outputs()
        validate_generated(outputs, args.generate)
        issues = component_blockers() + validate_assets() + cargo_issues
        artifact_issues: list[str] = []
        if args.appdir is not None:
            artifact_issues.extend(audit_appdir(args.appdir.resolve()))
        if args.appimage is not None:
            artifact_issues.extend(audit_appimage(args.appimage.resolve()))
        if args.guest_rootfs is not None:
            artifact_issues.extend(audit_guest_rootfs(args.guest_rootfs.resolve()))
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
