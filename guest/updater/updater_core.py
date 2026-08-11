#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Fixed-operation Debian updater used inside the Wild Buzzard guest.

This module deliberately contains no D-Bus argument that can become an apt
command line.  The public daemon passes only opaque plan generations into this
engine.  Package names, versions, repository configuration and apt actions are
derived from the guest's already-configured Debian package database through
python-apt.
"""

from __future__ import annotations

import contextlib
import dataclasses
import fcntl
import hashlib
import json
import os
import re
import secrets
import stat
import tempfile
import threading
import time
from pathlib import Path
from typing import Callable, Iterable, Protocol


STATE_SCHEMA_VERSION = 2
PLAN_SCHEMA_VERSION = 1
RUNTIME_MANIFEST_SCHEMA_VERSION = 1
RUNTIME_READINESS_SCHEMA_VERSION = 1

MAX_JSON_BYTES = 1024 * 1024
MAX_LOG_BYTES = 1024 * 1024
MAX_LOG_FILES = 8
MAX_PACKAGES = 16_384
MAX_REPOSITORY_ERRORS = 1_024
MAX_TEXT_BYTES = 16_384
MAX_RUNTIME_FILES = 4_096
MAX_RUNTIME_ENTRIES = MAX_RUNTIME_FILES * 2 + 32
MAX_RUNTIME_FILE_BYTES = 512 * 1024 * 1024
MAX_DPKG_LIST_BYTES = 64 * 1024 * 1024
MAX_DYNAMIC_TEXT_BYTES = 4 * 1024
PROGRESS_WRITE_INTERVAL_SECONDS = 0.1
PACKAGE_LOCK_TIMEOUT_SECONDS = 30.0

GENERATION_RE = re.compile(r"^[0-9a-f]{64}$")
REVISION_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._+~-]{0,127}$")
LOG_ID_RE = re.compile(r"^attempt-[1-9][0-9]*-[0-9a-f]{16}\.log$")
PACKAGE_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9+.:~-]{0,255}$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")

STATUSES = {
    "never_checked",
    "checking",
    "up_to_date",
    "available",
    "installing",
    "failed",
    "restart_recommended",
}
PROGRESS_PHASES = {
    "refreshing",
    "resolving",
    "downloading",
    "installing",
    "repairing",
}
PROGRESS_UNITS = {"bytes", "packages", "steps"}
ACTIONS = {"upgrade", "install"}

REQUIRED_RUNTIME_FILES = {
    "bin/sway",
    "bin/swaymsg",
    "bin/cua-driver",
    "libexec/wildbuzzard-shell",
    "libexec/wildbuzzard-settings",
    "libexec/wildbuzzard-shortcut-helper",
    "libexec/wildbuzzard-clipboard-agent",
    "libexec/wildbuzzard-updater",
}


class UpdaterError(RuntimeError):
    """Base class for an expected, user-visible updater failure."""


class BusyError(UpdaterError):
    pass


class StalePlanError(UpdaterError):
    pass


class RuntimeGateError(UpdaterError):
    pass


class CancelledDownload(UpdaterError):
    pass


class RepositoryRefreshError(UpdaterError):
    pass


@dataclasses.dataclass(frozen=True)
class Paths:
    state_dir: Path = Path("/var/lib/wildbuzzard-updater")
    log_dir: Path = Path("/var/log/wildbuzzard-updater")
    lock_path: Path = Path("/run/lock/wildbuzzard-updater.lock")
    runtime_root: Path = Path("/opt/wildbuzzard/runtime")
    dpkg_info: Path = Path("/var/lib/dpkg/info")
    reboot_required: Path = Path("/run/reboot-required")
    reboot_packages: Path = Path("/run/reboot-required.pkgs")
    runtime_owner_uid: int = 0

    @property
    def state_path(self) -> Path:
        return self.state_dir / "state.json"

    @property
    def plan_path(self) -> Path:
        return self.state_dir / "plan.json"

    @property
    def notification_path(self) -> Path:
        return self.state_dir / "notification.json"


@dataclasses.dataclass(frozen=True, order=True)
class PackageRecord:
    name: str
    installed_version: str
    candidate_version: str
    download_size: int
    security_origin: str | None
    action: str = "upgrade"

    def to_json(self) -> dict[str, object]:
        return dataclasses.asdict(self)

    @classmethod
    def from_json(cls, value: object) -> "PackageRecord":
        if not isinstance(value, dict) or set(value) != {
            "name",
            "installed_version",
            "candidate_version",
            "download_size",
            "security_origin",
            "action",
        }:
            raise UpdaterError("plan contains an invalid package record")
        record = cls(
            name=value["name"],
            installed_version=value["installed_version"],
            candidate_version=value["candidate_version"],
            download_size=value["download_size"],
            security_origin=value["security_origin"],
            action=value["action"],
        )
        validate_package(record)
        return record


@dataclasses.dataclass(frozen=True)
class Plan:
    generation: str
    checked_at_unix_seconds: int
    packages: tuple[PackageRecord, ...]
    download_size: int
    runtime_revision: str

    def to_json(self) -> dict[str, object]:
        return {
            "schema_version": PLAN_SCHEMA_VERSION,
            "generation": self.generation,
            "checked_at_unix_seconds": self.checked_at_unix_seconds,
            "packages": [package.to_json() for package in self.packages],
            "download_size": self.download_size,
            "runtime_revision": self.runtime_revision,
        }

    @classmethod
    def from_json(cls, value: object) -> "Plan":
        if not isinstance(value, dict) or set(value) != {
            "schema_version",
            "generation",
            "checked_at_unix_seconds",
            "packages",
            "download_size",
            "runtime_revision",
        }:
            raise UpdaterError("plan document has unexpected fields")
        if not _is_int(value["schema_version"]) or value["schema_version"] != PLAN_SCHEMA_VERSION:
            raise UpdaterError("unsupported updater plan schema")
        packages_value = value["packages"]
        if not isinstance(packages_value, list) or len(packages_value) > MAX_PACKAGES:
            raise UpdaterError("plan package list is invalid or too large")
        plan = cls(
            generation=value["generation"],
            checked_at_unix_seconds=value["checked_at_unix_seconds"],
            packages=tuple(PackageRecord.from_json(item) for item in packages_value),
            download_size=value["download_size"],
            runtime_revision=value["runtime_revision"],
        )
        validate_plan(plan)
        return plan


class AptBackend(Protocol):
    def refresh(self, progress: Callable[[str, int, int, str | None, bool], None]) -> list[str]:
        ...

    def resolve_plan(self) -> tuple[PackageRecord, ...]:
        ...

    def install(
        self,
        plan: Plan,
        progress: Callable[[str, int, int, str | None, bool], None],
        cancelled: threading.Event,
    ) -> None:
        ...

    def repair(
        self,
        plan: Plan,
        progress: Callable[[str, int, int, str | None, bool], None],
    ) -> None:
        ...

    def needs_repair(self) -> bool:
        ...


def _is_int(value: object) -> bool:
    return type(value) is int


def _sanitize_dynamic_text(
    value: object,
    *,
    fallback: str,
    maximum: int = MAX_DYNAMIC_TEXT_BYTES,
) -> str:
    """Return bounded single-line evidence suitable for state and logs.

    apt, dpkg, repositories, and GLib can return arbitrary text.  Their
    diagnostics remain evidence, but may not inject control characters or
    make the strict state schema unwritable merely by being very large.
    """

    text = str(value) if value is not None else fallback
    text = " ".join(text.replace("\x00", " ").split()) or fallback
    encoded = text.encode("utf-8", "replace")
    if len(encoded) <= maximum:
        return text
    suffix = "…"
    allowance = maximum - len(suffix.encode("utf-8"))
    shortened = encoded[: max(0, allowance)]
    while shortened:
        try:
            return shortened.decode("utf-8") + suffix
        except UnicodeDecodeError:
            shortened = shortened[:-1]
    return suffix


def _current_debian_download_detail(owner: object) -> str:
    """Return a bounded package-oriented label for python-apt progress.

    ``apt_pkg.Acquire.items`` is untrusted repository-derived state.  We never
    expose its URI or destination path.  A canonical Debian archive basename
    lets the UI say which package is being downloaded; otherwise it uses a
    generic truthful label.
    """

    items = getattr(owner, "items", ())
    if not isinstance(items, (list, tuple)):
        return "Downloading package archives"
    candidates: list[tuple[bool, str]] = []
    for item in items[:MAX_PACKAGES]:
        if bool(getattr(item, "complete", False)):
            continue
        filename = os.path.basename(str(getattr(item, "destfile", "")))
        if not filename.endswith(".deb") or "_" not in filename:
            continue
        package = filename.split("_", 1)[0]
        if not PACKAGE_RE.fullmatch(package):
            continue
        candidates.append((bool(getattr(item, "active_subprocess", "")), package))
    if not candidates:
        return "Downloading package archives"
    package = next((name for active, name in candidates if active), candidates[0][1])
    return f"Downloading {package}"


def _validate_text(field: str, value: object, *, allow_empty: bool = False) -> str:
    if not isinstance(value, str):
        raise UpdaterError(f"{field} must be text")
    if not allow_empty and not value:
        raise UpdaterError(f"{field} must not be empty")
    if len(value.encode("utf-8")) > MAX_TEXT_BYTES:
        raise UpdaterError(f"{field} exceeds the bounded text limit")
    if any(ord(character) < 0x20 or ord(character) == 0x7F for character in value):
        raise UpdaterError(f"{field} contains a control character")
    return value


def validate_generation(value: object) -> str:
    generation = _validate_text("plan generation", value)
    if not GENERATION_RE.fullmatch(generation):
        raise UpdaterError("plan generation is not a canonical opaque identifier")
    return generation


def validate_revision(value: object) -> str:
    revision = _validate_text("runtime revision", value)
    if not REVISION_RE.fullmatch(revision):
        raise UpdaterError("runtime revision is not a safe single path component")
    return revision


def validate_package(package: PackageRecord) -> None:
    if not PACKAGE_RE.fullmatch(_validate_text("package name", package.name)):
        raise UpdaterError(f"invalid Debian package name {package.name!r}")
    _validate_text("installed version", package.installed_version)
    _validate_text("candidate version", package.candidate_version)
    if not _is_int(package.download_size) or not 0 <= package.download_size <= 2**63 - 1:
        raise UpdaterError("package download size is invalid")
    if package.security_origin is not None:
        _validate_text("security origin", package.security_origin)
    if package.action not in ACTIONS:
        raise UpdaterError("package action is not an allowed updater action")


def validate_plan(plan: Plan) -> None:
    validate_generation(plan.generation)
    validate_revision(plan.runtime_revision)
    if not _is_int(plan.checked_at_unix_seconds) or plan.checked_at_unix_seconds <= 0:
        raise UpdaterError("plan check time is invalid")
    if not plan.packages or len(plan.packages) > MAX_PACKAGES:
        raise UpdaterError("actionable plan must contain a bounded package list")
    seen: set[str] = set()
    total = 0
    previous: PackageRecord | None = None
    for package in plan.packages:
        validate_package(package)
        if package.name in seen:
            raise UpdaterError(f"duplicate package in plan: {package.name}")
        if previous is not None and package < previous:
            raise UpdaterError("plan package records are not in canonical order")
        previous = package
        seen.add(package.name)
        total += package.download_size
        if total > 2**63 - 1:
            raise UpdaterError("plan download size overflow")
    if plan.download_size != total:
        raise UpdaterError("plan download total does not equal its package records")


def _real_directory(path: Path, mode: int) -> None:
    path.mkdir(parents=True, mode=mode, exist_ok=True)
    metadata = path.lstat()
    if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISDIR(metadata.st_mode):
        raise UpdaterError(f"managed path is not a real directory: {path}")
    os.chmod(path, mode)


def read_bounded_json(path: Path, maximum: int = MAX_JSON_BYTES) -> object:
    flags = (
        os.O_RDONLY
        | os.O_CLOEXEC
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_NONBLOCK", 0)
    )
    descriptor = os.open(path, flags)
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise UpdaterError(f"managed JSON path is not a regular file: {path}")
        if metadata.st_size > maximum:
            raise UpdaterError(f"managed JSON file exceeds {maximum} bytes: {path}")
        data = bytearray()
        while len(data) <= maximum:
            chunk = os.read(descriptor, min(65_536, maximum + 1 - len(data)))
            if not chunk:
                break
            data.extend(chunk)
        if len(data) > maximum:
            raise UpdaterError(f"managed JSON file exceeds {maximum} bytes: {path}")
        def reject_duplicate_keys(pairs: list[tuple[str, object]]) -> dict[str, object]:
            result: dict[str, object] = {}
            for key, value in pairs:
                if key in result:
                    raise UpdaterError(f"managed JSON contains duplicate key {key!r}: {path}")
                result[key] = value
            return result

        return json.loads(data, object_pairs_hook=reject_duplicate_keys)
    finally:
        os.close(descriptor)


def atomic_write(path: Path, data: bytes, mode: int) -> None:
    if len(data) > MAX_JSON_BYTES:
        raise UpdaterError("managed updater document exceeds its bounded size")
    # The shared state directory must remain traversable by the interactive
    # shell even while the root-only plan file is replaced. Confidentiality
    # belongs to each file's mode, not a parent mode that oscillates depending
    # on which document happened to be written last.
    _real_directory(path.parent, 0o755)
    try:
        existing = path.lstat()
    except FileNotFoundError:
        existing = None
    if existing is not None and (stat.S_ISLNK(existing.st_mode) or not stat.S_ISREG(existing.st_mode)):
        raise UpdaterError(f"refusing to replace unsafe managed path: {path}")
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        os.fchmod(descriptor, mode)
        view = memoryview(data)
        while view:
            written = os.write(descriptor, view)
            if written <= 0:
                raise OSError("short updater-state write")
            view = view[written:]
        os.fsync(descriptor)
        os.close(descriptor)
        descriptor = -1
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temporary)


def atomic_write_json(path: Path, value: object, mode: int) -> None:
    data = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
    atomic_write(path, data + b"\n", mode)


def _hash_regular(path: Path, maximum: int) -> str:
    flags = (
        os.O_RDONLY
        | os.O_CLOEXEC
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_NONBLOCK", 0)
    )
    descriptor = os.open(path, flags)
    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise RuntimeGateError(f"runtime asset is not a regular file: {path}")
        if metadata.st_size > maximum:
            raise RuntimeGateError(f"runtime asset exceeds its bounded size: {path}")
        digest = hashlib.sha256()
        remaining = maximum + 1
        while remaining:
            chunk = os.read(descriptor, min(1024 * 1024, remaining))
            if not chunk:
                break
            digest.update(chunk)
            remaining -= len(chunk)
        if remaining == 0 and os.read(descriptor, 1):
            raise RuntimeGateError(f"runtime asset exceeds its bounded size: {path}")
        return digest.hexdigest()
    finally:
        os.close(descriptor)


def _dpkg_runtime_owners(paths: Paths) -> list[str]:
    if not paths.dpkg_info.exists():
        raise RuntimeGateError("dpkg ownership database is unavailable")
    prefix = f"{paths.runtime_root}/".encode()
    owners: list[str] = []
    consumed = 0
    for listing in sorted(paths.dpkg_info.glob("*.list")):
        metadata = listing.lstat()
        if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
            continue
        consumed += metadata.st_size
        if consumed > MAX_DPKG_LIST_BYTES:
            raise RuntimeGateError("dpkg ownership inventory exceeds its bounded scan limit")
        flags = (
            os.O_RDONLY
            | os.O_CLOEXEC
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_NONBLOCK", 0)
        )
        descriptor = os.open(listing, flags)
        try:
            data = os.read(descriptor, metadata.st_size + 1)
        finally:
            os.close(descriptor)
        if any(line.startswith(prefix) for line in data.splitlines()):
            owners.append(listing.name.removesuffix(".list"))
    return owners


def _require_protected_metadata(
    path: Path,
    metadata: os.stat_result,
    expected_uid: int,
    kind: str,
) -> None:
    if metadata.st_uid != expected_uid:
        raise RuntimeGateError(f"{kind} is not owned by the protected runtime owner: {path}")
    if stat.S_IMODE(metadata.st_mode) & 0o022:
        raise RuntimeGateError(f"{kind} is group/world writable: {path}")


def _runtime_file(
    revision_dir: Path,
    canonical_revision: Path,
    relative: str,
    expected_uid: int,
) -> tuple[Path, os.stat_result]:
    current = revision_dir
    components = relative.split("/")
    for index, component in enumerate(components):
        current = current / component
        metadata = current.lstat()
        if stat.S_ISLNK(metadata.st_mode):
            raise RuntimeGateError(f"runtime manifest path contains a symbolic link: {relative}")
        _require_protected_metadata(current, metadata, expected_uid, "runtime path component")
        if index + 1 == len(components):
            if not stat.S_ISREG(metadata.st_mode):
                raise RuntimeGateError(f"runtime asset is not a regular file: {relative}")
        elif not stat.S_ISDIR(metadata.st_mode):
            raise RuntimeGateError(f"runtime asset parent is not a directory: {relative}")
    resolved = current.resolve(strict=True)
    if os.path.commonpath((str(canonical_revision), str(resolved))) != str(canonical_revision):
        raise RuntimeGateError(f"runtime asset escaped its revision: {relative}")
    return current, metadata


def _validate_runtime_inventory(
    revision_dir: Path,
    manifested_files: set[str],
    expected_uid: int,
) -> None:
    """Reject unmanifested payloads and every link or special node.

    Empty protected directories are harmless and permitted, but every regular
    file that can be reached under the active revision must be either bound by
    the manifest or one of the two records that bind/attest that manifest.
    """

    allowed_records = {"runtime.manifest.json", "readiness.json"}
    pending = [revision_dir]
    entries_seen = 0
    while pending:
        directory = pending.pop()
        with os.scandir(directory) as entries:
            for entry in entries:
                entries_seen += 1
                if entries_seen > MAX_RUNTIME_ENTRIES:
                    raise RuntimeGateError(
                        "protected runtime tree exceeds its bounded entry limit"
                    )
                path = Path(entry.path)
                metadata = entry.stat(follow_symlinks=False)
                relative = path.relative_to(revision_dir).as_posix()
                if stat.S_ISLNK(metadata.st_mode):
                    raise RuntimeGateError(
                        f"protected runtime tree contains a symbolic link: {relative}"
                    )
                _require_protected_metadata(
                    path,
                    metadata,
                    expected_uid,
                    "runtime inventory entry",
                )
                if stat.S_ISDIR(metadata.st_mode):
                    pending.append(path)
                elif stat.S_ISREG(metadata.st_mode):
                    if relative not in manifested_files and relative not in allowed_records:
                        raise RuntimeGateError(
                            f"protected runtime contains an unmanifested file: {relative}"
                        )
                else:
                    raise RuntimeGateError(
                        f"protected runtime contains a special file: {relative}"
                    )


def inspect_runtime_gate(paths: Paths) -> tuple[bool, str | None, str | None]:
    """Return readiness, revision and a precise diagnostic.

    The current link is intentionally relative and contains one revision
    component.  Each payload file is checked against a bounded manifest, and
    readiness binds to the exact manifest hash.  A directory existing at
    /opt/.../current is therefore not enough to enable package installation.
    """

    try:
        runtime_metadata = paths.runtime_root.lstat()
        if stat.S_ISLNK(runtime_metadata.st_mode) or not stat.S_ISDIR(runtime_metadata.st_mode):
            raise RuntimeGateError("protected runtime root is not a real directory")
        _require_protected_metadata(
            paths.runtime_root, runtime_metadata, paths.runtime_owner_uid, "runtime root"
        )
        current = paths.runtime_root / "current"
        metadata = current.lstat()
        if not stat.S_ISLNK(metadata.st_mode):
            raise RuntimeGateError("protected runtime current is not a symbolic link")
        if metadata.st_uid != paths.runtime_owner_uid:
            raise RuntimeGateError("protected runtime current link has the wrong owner")
        target = os.readlink(current)
        revision = validate_revision(target)
        revision_dir = paths.runtime_root / revision
        revision_metadata = revision_dir.lstat()
        if stat.S_ISLNK(revision_metadata.st_mode) or not stat.S_ISDIR(revision_metadata.st_mode):
            raise RuntimeGateError("protected runtime revision is not a real directory")
        _require_protected_metadata(
            revision_dir, revision_metadata, paths.runtime_owner_uid, "runtime revision"
        )
        manifest_path = revision_dir / "runtime.manifest.json"
        readiness_path = revision_dir / "readiness.json"
        for record_path, label in (
            (manifest_path, "runtime manifest"),
            (readiness_path, "runtime readiness"),
        ):
            record_metadata = record_path.lstat()
            if stat.S_ISLNK(record_metadata.st_mode) or not stat.S_ISREG(record_metadata.st_mode):
                raise RuntimeGateError(f"{label} is not a real file")
            _require_protected_metadata(
                record_path, record_metadata, paths.runtime_owner_uid, label
            )
        manifest_bytes = json.dumps(
            read_bounded_json(manifest_path),
            sort_keys=True,
            separators=(",", ":"),
            ensure_ascii=False,
        ).encode()
        manifest = json.loads(manifest_bytes)
        if not isinstance(manifest, dict) or set(manifest) != {
            "schema_version",
            "revision",
            "files",
        }:
            raise RuntimeGateError("protected runtime manifest has unexpected fields")
        if (
            not _is_int(manifest["schema_version"])
            or manifest["schema_version"] != RUNTIME_MANIFEST_SCHEMA_VERSION
        ):
            raise RuntimeGateError("protected runtime manifest schema is unsupported")
        if validate_revision(manifest["revision"]) != revision:
            raise RuntimeGateError("protected runtime manifest revision does not match current")
        files = manifest["files"]
        if not isinstance(files, dict) or not files or len(files) > MAX_RUNTIME_FILES:
            raise RuntimeGateError("protected runtime manifest file inventory is invalid")
        if not REQUIRED_RUNTIME_FILES.issubset(files):
            missing = sorted(REQUIRED_RUNTIME_FILES.difference(files))
            raise RuntimeGateError(f"protected runtime is missing required files: {', '.join(missing)}")
        _validate_runtime_inventory(
            revision_dir,
            set(files),
            paths.runtime_owner_uid,
        )
        canonical_revision = revision_dir.resolve(strict=True)
        for relative, record in sorted(files.items()):
            if (
                not isinstance(relative, str)
                or relative.startswith("/")
                or not relative
                or any(component in {"", ".", ".."} for component in relative.split("/"))
                or not isinstance(record, dict)
                or set(record) != {"sha256", "mode"}
                or not isinstance(record["sha256"], str)
                or not SHA256_RE.fullmatch(record["sha256"])
                or not _is_int(record["mode"])
                or not 0 <= record["mode"] <= 0o777
            ):
                raise RuntimeGateError("protected runtime manifest contains an unsafe record")
            candidate, candidate_metadata = _runtime_file(
                revision_dir,
                canonical_revision,
                relative,
                paths.runtime_owner_uid,
            )
            if stat.S_IMODE(candidate_metadata.st_mode) != record["mode"]:
                raise RuntimeGateError(f"runtime asset mode differs from manifest: {relative}")
            if _hash_regular(candidate, MAX_RUNTIME_FILE_BYTES) != record["sha256"]:
                raise RuntimeGateError(f"runtime asset digest differs from manifest: {relative}")
        readiness = read_bounded_json(readiness_path)
        if not isinstance(readiness, dict) or set(readiness) != {
            "schema_version",
            "revision",
            "manifest_sha256",
            "ready",
        }:
            raise RuntimeGateError("protected runtime readiness record has unexpected fields")
        if (
            not _is_int(readiness["schema_version"])
            or readiness["schema_version"] != RUNTIME_READINESS_SCHEMA_VERSION
        ):
            raise RuntimeGateError("protected runtime readiness schema is unsupported")
        if validate_revision(readiness["revision"]) != revision or readiness["ready"] is not True:
            raise RuntimeGateError("protected runtime has not passed desktop readiness")
        manifest_digest = hashlib.sha256(manifest_bytes).hexdigest()
        if readiness["manifest_sha256"] != manifest_digest:
            raise RuntimeGateError("protected runtime readiness does not bind the installed manifest")
        owners = _dpkg_runtime_owners(paths)
        if owners:
            raise RuntimeGateError(
                "protected runtime is still owned by dpkg packages: " + ", ".join(owners[:8])
            )
        return True, revision, None
    except (OSError, ValueError, json.JSONDecodeError, UpdaterError) as error:
        return (
            False,
            None,
            _sanitize_dynamic_text(
                error,
                fallback="protected runtime validation failed",
            ),
        )


def _empty_state(
    paths: Paths,
    runtime_gate: tuple[bool, str | None, str | None] | None = None,
) -> dict[str, object]:
    runtime_ready, runtime_revision, runtime_diagnostic = (
        runtime_gate if runtime_gate is not None else inspect_runtime_gate(paths)
    )
    return {
        "schema_version": STATE_SCHEMA_VERSION,
        "state_generation": 1,
        "status": "never_checked",
        "checked_at_unix_seconds": None,
        "repository_errors": [],
        "packages": [],
        "download_size": 0,
        "plan_generation": None,
        "progress": None,
        "failure": runtime_diagnostic,
        "repair_available": False,
        "restart_reasons": [],
        "last_log_id": None,
        "runtime_revision": runtime_revision,
        "runtime_ready": runtime_ready,
    }


def validate_progress(value: object) -> None:
    if value is None:
        return
    if not isinstance(value, dict) or set(value) != {
        "phase",
        "completed",
        "total",
        "unit",
        "detail",
        "cancellable",
    }:
        raise UpdaterError("update progress has unexpected fields")
    if value["phase"] not in PROGRESS_PHASES or value["unit"] not in PROGRESS_UNITS:
        raise UpdaterError("update progress phase or unit is invalid")
    expected_unit = {
        "refreshing": "bytes",
        "resolving": "steps",
        "downloading": "bytes",
        "installing": "packages",
        "repairing": "packages",
    }[value["phase"]]
    if value["unit"] != expected_unit:
        raise UpdaterError("update progress unit does not match its phase")
    completed, total = value["completed"], value["total"]
    if (
        not _is_int(completed)
        or not _is_int(total)
        or completed < 0
        or total < 0
        or completed > total
        or total > 2**63 - 1
    ):
        raise UpdaterError("update progress totals are invalid")
    if value["detail"] is not None:
        _validate_text("progress detail", value["detail"])
    if not isinstance(value["cancellable"], bool):
        raise UpdaterError("update progress cancellable must be boolean")
    if value["cancellable"] and value["phase"] != "downloading":
        raise UpdaterError("only package download progress may be cancelled")


def validate_state(state: object) -> dict[str, object]:
    expected = {
        "schema_version",
        "state_generation",
        "status",
        "checked_at_unix_seconds",
        "repository_errors",
        "packages",
        "download_size",
        "plan_generation",
        "progress",
        "failure",
        "repair_available",
        "restart_reasons",
        "last_log_id",
        "runtime_revision",
        "runtime_ready",
    }
    if not isinstance(state, dict) or set(state) != expected:
        raise UpdaterError("update state has unexpected fields")
    if not _is_int(state["schema_version"]) or state["schema_version"] != STATE_SCHEMA_VERSION:
        raise UpdaterError("unsupported update-state schema")
    if (
        not _is_int(state["state_generation"])
        or state["state_generation"] <= 0
    ):
        raise UpdaterError("update state generation is invalid")
    status_value = state["status"]
    if status_value not in STATUSES:
        raise UpdaterError("update status is invalid")
    checked = state["checked_at_unix_seconds"]
    if checked is not None and (
        not _is_int(checked) or checked <= 0
    ):
        raise UpdaterError("update check time is invalid")
    errors = state["repository_errors"]
    if not isinstance(errors, list) or len(errors) > MAX_REPOSITORY_ERRORS:
        raise UpdaterError("repository-error list is invalid or too large")
    for error in errors:
        _validate_text("repository error", error)
    packages_value = state["packages"]
    if not isinstance(packages_value, list) or len(packages_value) > MAX_PACKAGES:
        raise UpdaterError("update package list is invalid or too large")
    packages = tuple(PackageRecord.from_json(value) for value in packages_value)
    if len({package.name for package in packages}) != len(packages):
        raise UpdaterError("update state contains duplicate packages")
    if list(packages) != sorted(packages):
        raise UpdaterError("update-state packages are not in canonical order")
    total = sum(package.download_size for package in packages)
    if not _is_int(state["download_size"]) or total != state["download_size"]:
        raise UpdaterError("update-state download size is inconsistent")
    generation = state["plan_generation"]
    if generation is not None:
        validate_generation(generation)
    validate_progress(state["progress"])
    if state["failure"] is not None:
        _validate_text("update failure", state["failure"])
    if not isinstance(state["repair_available"], bool):
        raise UpdaterError("repair_available must be boolean")
    reasons = state["restart_reasons"]
    if not isinstance(reasons, list) or len(reasons) > 256:
        raise UpdaterError("restart-reason list is invalid or too large")
    for reason in reasons:
        _validate_text("restart reason", reason)
    if state["last_log_id"] is not None and (
        not isinstance(state["last_log_id"], str)
        or not LOG_ID_RE.fullmatch(state["last_log_id"])
    ):
        raise UpdaterError("last log identifier is invalid")
    if state["runtime_revision"] is not None:
        validate_revision(state["runtime_revision"])
    if not isinstance(state["runtime_ready"], bool):
        raise UpdaterError("runtime_ready must be boolean")
    if state["runtime_ready"] != (state["runtime_revision"] is not None):
        raise UpdaterError("runtime_ready requires exactly one validated runtime revision")

    has_check = checked is not None
    has_plan = generation is not None
    has_packages = bool(packages)
    progress = state["progress"]
    if status_value == "never_checked" and (
        has_check
        or has_plan
        or has_packages
        or errors
        or state["download_size"]
        or progress
        or state["repair_available"]
        or reasons
    ):
        raise UpdaterError("never_checked state contains completed check data")
    if status_value == "checking":
        if (
            has_check
            or has_plan
            or has_packages
            or errors
            or state["download_size"]
            or not progress
            or progress["phase"] not in {"refreshing", "resolving"}
            or state["failure"] is not None
            or state["repair_available"]
            or reasons
        ):
            raise UpdaterError("checking state is incoherent")
    if status_value == "up_to_date" and (
        not has_check
        or has_plan
        or has_packages
        or errors
        or state["download_size"]
        or progress
        or state["failure"] is not None
        or state["repair_available"]
        or reasons
    ):
        raise UpdaterError("up_to_date state is incoherent")
    if status_value == "available" and (
        not has_check
        or not has_plan
        or not has_packages
        or progress
        or errors
        or state["failure"] is not None
        or state["repair_available"]
        or reasons
    ):
        raise UpdaterError("actionable update state is incoherent")
    if status_value == "installing" and (
        not has_check
        or not has_plan
        or not has_packages
        or not progress
        or progress["phase"] not in {"downloading", "installing", "repairing"}
        or errors
        or state["failure"] is not None
        or state["repair_available"]
        or reasons
    ):
        raise UpdaterError("installing state is incoherent")
    if status_value == "restart_recommended" and (
        not has_check
        or not has_plan
        or not has_packages
        or not reasons
        or progress
        or errors
        or state["failure"] is not None
        or state["repair_available"]
    ):
        raise UpdaterError("restart_recommended state is incoherent")
    if status_value == "failed" and state["failure"] is None and not errors:
        raise UpdaterError("failed state contains no failure evidence")
    if status_value == "failed" and (
        ((has_plan or has_packages) and not has_check)
        or (has_packages and not has_plan)
        or progress
        or reasons
    ):
        raise UpdaterError("failed state is incoherent")
    if state["repair_available"] and (status_value != "failed" or not has_plan):
        raise UpdaterError("repair is offered without an updater-generated failed plan")
    if status_value != "restart_recommended" and reasons:
        raise UpdaterError("restart reasons exist outside restart_recommended state")
    return state


def load_state(paths: Paths) -> dict[str, object]:
    try:
        paths.state_path.lstat()
    except FileNotFoundError:
        return _empty_state(paths)
    return validate_state(read_bounded_json(paths.state_path))


def migrate_legacy_state(paths: Paths) -> dict[str, object] | None:
    """Invalidate the one known v1 wire shape without accepting unknown data.

    Version 1 had no operation generation, progress, recovery, runtime gate, or
    exact package action.  Retaining one of its plans would therefore be
    unsafe.  A recognized v1 document becomes an explicit failed/recheck v2
    state; newer and malformed schemas are preserved untouched.
    """

    value = read_bounded_json(paths.state_path)
    if not isinstance(value, dict) or not _is_int(value.get("schema_version")):
        return None
    if value["schema_version"] != 1:
        return None
    expected = {
        "schema_version",
        "status",
        "checked_at_unix_seconds",
        "repository_errors",
        "packages",
        "download_size",
        "plan_generation",
    }
    if set(value) != expected:
        return None
    packages = value["packages"]
    if not isinstance(packages, list) or len(packages) > MAX_PACKAGES:
        return None
    old_package_keys = {
        "name",
        "installed_version",
        "candidate_version",
        "download_size",
        "security_origin",
    }
    if any(not isinstance(package, dict) or set(package) != old_package_keys for package in packages):
        return None
    migrated = _empty_state(paths)
    migrated.update(
        status="failed",
        failure=(
            "Updater state schema 1 was safely invalidated because it did not bind "
            "an exact protected-runtime transaction. Run Check again."
        ),
    )
    validate_state(migrated)
    atomic_write_json(paths.state_path, migrated, 0o644)
    return migrated


def load_plan(paths: Paths) -> Plan:
    return Plan.from_json(read_bounded_json(paths.plan_path))


class AttemptLog:
    def __init__(self, paths: Paths, state_generation: int):
        _real_directory(paths.log_dir, 0o700)
        self.paths = paths
        self.id = f"attempt-{state_generation}-{secrets.token_hex(8)}.log"
        self.path = paths.log_dir / self.id
        flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
        self.descriptor = os.open(self.path, flags, 0o600)
        self.written = 0
        self.write("operation log opened")
        self._prune()

    def _prune(self) -> None:
        logs = sorted(
            (
                path
                for path in self.paths.log_dir.iterdir()
                if path != self.path
                and path.is_file()
                and not path.is_symlink()
                and LOG_ID_RE.fullmatch(path.name)
            ),
            key=lambda path: (path.stat().st_mtime_ns, path.name),
        )
        retained_old_logs = max(0, MAX_LOG_FILES - 1)
        for old in logs[: max(0, len(logs) - retained_old_logs)]:
            with contextlib.suppress(OSError):
                old.unlink()

    def write(self, message: str) -> None:
        safe = _sanitize_dynamic_text(message, fallback="updater diagnostic unavailable")
        line = f"{int(time.time())} {safe}\n".encode()
        if self.written + len(line) > MAX_LOG_BYTES:
            return
        view = memoryview(line)
        while view:
            written = os.write(self.descriptor, view)
            if written <= 0:
                raise OSError("short updater log write")
            view = view[written:]
        self.written += len(line)

    def close(self) -> None:
        if self.descriptor >= 0:
            os.fsync(self.descriptor)
            os.close(self.descriptor)
            self.descriptor = -1

    def __enter__(self) -> "AttemptLog":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


class UpdateEngine:
    def __init__(self, backend: AptBackend, paths: Paths = Paths()):
        self.backend = backend
        self.paths = paths
        self._mutex = threading.Lock()
        self._cancel = threading.Event()
        self._operation: threading.Thread | None = None
        self._runtime_gate = inspect_runtime_gate(paths)
        self._last_progress_write = 0.0
        self._last_progress_phase: str | None = None
        self._pending_progress: tuple[dict[str, object], AttemptLog] | None = None
        _real_directory(paths.state_dir, 0o755)
        _real_directory(paths.log_dir, 0o700)
        _real_directory(paths.lock_path.parent, 0o755)
        try:
            paths.state_path.lstat()
        except FileNotFoundError:
            self._write_state(_empty_state(paths, self._runtime_gate))
        else:
            try:
                load_state(paths)
            except UpdaterError:
                if migrate_legacy_state(paths) is None:
                    raise
        self._reconcile_interrupted_state()

    def state(self) -> dict[str, object]:
        with self._mutex:
            return load_state(self.paths)

    def state_json(self) -> str:
        return json.dumps(self.state(), sort_keys=True, separators=(",", ":"), ensure_ascii=False)

    def _write_state(self, state: dict[str, object]) -> None:
        validate_state(state)
        atomic_write_json(self.paths.state_path, state, 0o644)

    def _transition(self, **changes: object) -> dict[str, object]:
        state = load_state(self.paths)
        state.update(changes)
        state["state_generation"] = int(state["state_generation"]) + 1
        runtime_ready, revision, _diagnostic = self._runtime_gate
        state["runtime_ready"] = runtime_ready
        state["runtime_revision"] = revision
        self._write_state(state)
        return state

    def _refresh_runtime_gate(self) -> tuple[bool, str | None, str | None]:
        self._runtime_gate = inspect_runtime_gate(self.paths)
        return self._runtime_gate

    def _reconcile_interrupted_state(self) -> None:
        state = load_state(self.paths)
        if state["status"] not in {"checking", "installing"}:
            return
        operation = "repository check" if state["status"] == "checking" else "package transaction"
        repair_available = False
        progress = state.get("progress")
        if (
            state["status"] == "installing"
            and isinstance(progress, dict)
            and progress.get("phase") in {"installing", "repairing"}
            and state.get("plan_generation") is not None
        ):
            try:
                plan = load_plan(self.paths)
                if plan.generation == state["plan_generation"]:
                    repair_available = bool(self.backend.needs_repair())
            except Exception:
                repair_available = False
        self._transition(
            status="failed",
            progress=None,
            failure=(
                f"The updater service restarted during an active {operation}. "
                + (
                    "The package database reports an incomplete transaction; use Retry / Repair."
                    if repair_available
                    else "No repairable dpkg transaction was proven; run Check again."
                )
            ),
            repair_available=repair_available,
            restart_reasons=[],
        )

    @contextlib.contextmanager
    def _serialized(self) -> Iterable[None]:
        flags = os.O_RDWR | os.O_CREAT | os.O_CLOEXEC | getattr(os, "O_NOFOLLOW", 0)
        descriptor = os.open(self.paths.lock_path, flags, 0o600)
        try:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise BusyError("another updater or package operation owns the Wild Buzzard lock") from error
            yield
        finally:
            os.close(descriptor)

    def _progress(
        self,
        phase: str,
        completed: int,
        total: int,
        detail: str | None,
        cancellable: bool,
        log: AttemptLog,
    ) -> None:
        if not _is_int(completed) or not _is_int(total):
            raise UpdaterError("package backend returned non-integer progress")
        bounded_total = max(0, min(total, 2**63 - 1))
        bounded_completed = max(0, min(completed, bounded_total))
        unit = {
            "refreshing": "bytes",
            "resolving": "steps",
            "downloading": "bytes",
            "installing": "packages",
            "repairing": "packages",
        }.get(phase)
        if unit is None:
            raise UpdaterError("package backend returned an unknown progress phase")
        progress = {
            "phase": phase,
            "completed": bounded_completed,
            "total": bounded_total,
            "unit": unit,
            "detail": (
                _sanitize_dynamic_text(detail, fallback="Package operation in progress")
                if detail is not None
                else None
            ),
            "cancellable": cancellable,
        }
        validate_progress(progress)
        self._pending_progress = (progress, log)
        now = time.monotonic()
        if (
            phase != self._last_progress_phase
            or now - self._last_progress_write >= PROGRESS_WRITE_INTERVAL_SECONDS
        ):
            self._flush_progress()

    def _reset_progress_throttle(self) -> None:
        self._last_progress_write = 0.0
        self._last_progress_phase = None
        self._pending_progress = None

    def _flush_progress(self) -> None:
        if self._pending_progress is None:
            return
        progress, log = self._pending_progress
        self._pending_progress = None
        log.write(
            f"progress {progress['phase']} {progress['completed']}/{progress['total']} "
            f"{progress['detail'] or ''}"
        )
        self._transition(
            status=(
                "checking"
                if progress["phase"] in {"refreshing", "resolving"}
                else "installing"
            ),
            progress=progress,
        )
        self._last_progress_write = time.monotonic()
        self._last_progress_phase = str(progress["phase"])

    def check(self) -> None:
        with self._serialized():
            self._refresh_runtime_gate()
            self._reset_progress_throttle()
            initial = load_state(self.paths)
            with AttemptLog(self.paths, int(initial["state_generation"]) + 1) as log:
                self._transition(
                    status="checking",
                    checked_at_unix_seconds=None,
                    repository_errors=[],
                    packages=[],
                    download_size=0,
                    plan_generation=None,
                    progress={
                        "phase": "refreshing",
                        "completed": 0,
                        "total": 1,
                        "unit": "bytes",
                        "detail": "Refreshing configured Debian repositories",
                        "cancellable": False,
                    },
                    failure=None,
                    repair_available=False,
                    restart_reasons=[],
                    last_log_id=log.id,
                )
                repository_errors = self.backend.refresh(
                    lambda phase, done, total, detail, cancellable: self._progress(
                        phase, done, total, detail, cancellable, log
                    )
                )
                if repository_errors:
                    bounded_errors = [
                        _sanitize_dynamic_text(error, fallback="Repository refresh failed")
                        for error in repository_errors[:MAX_REPOSITORY_ERRORS]
                    ]
                    for error in bounded_errors:
                        log.write(f"repository error: {error}")
                    self._transition(
                        status="failed",
                        checked_at_unix_seconds=int(time.time()),
                        repository_errors=bounded_errors,
                        progress=None,
                        failure="One or more configured repositories could not be refreshed.",
                        repair_available=False,
                        restart_reasons=[],
                    )
                    raise RepositoryRefreshError(
                        "one or more configured repositories could not be refreshed"
                    )
                self._progress("resolving", 0, 1, "Resolving exact candidate versions", False, log)
                packages = tuple(sorted(self.backend.resolve_plan()))
                if len(packages) > MAX_PACKAGES:
                    raise UpdaterError("resolved update plan exceeds the package limit")
                for package in packages:
                    validate_package(package)
                self._flush_progress()
                checked = int(time.time())
                if not packages:
                    self._refresh_runtime_gate()
                    with contextlib.suppress(FileNotFoundError):
                        self.paths.plan_path.unlink()
                    self._transition(
                        status="up_to_date",
                        checked_at_unix_seconds=checked,
                        repository_errors=[],
                        packages=[],
                        download_size=0,
                        plan_generation=None,
                        progress=None,
                        failure=None,
                        repair_available=False,
                        restart_reasons=[],
                    )
                    log.write("no package updates are available")
                    return
                runtime_ready, revision, diagnostic = self._refresh_runtime_gate()
                if not runtime_ready or revision is None:
                    raise RuntimeGateError(diagnostic or "protected runtime is not ready")
                plan = Plan(
                    generation=secrets.token_hex(32),
                    checked_at_unix_seconds=checked,
                    packages=packages,
                    download_size=sum(package.download_size for package in packages),
                    runtime_revision=revision,
                )
                validate_plan(plan)
                atomic_write_json(self.paths.plan_path, plan.to_json(), 0o600)
                self._transition(
                    status="available",
                    checked_at_unix_seconds=checked,
                    repository_errors=[],
                    packages=[package.to_json() for package in packages],
                    download_size=plan.download_size,
                    plan_generation=plan.generation,
                    progress=None,
                    failure=None,
                    repair_available=False,
                    restart_reasons=[],
                )
                log.write(f"published opaque plan {plan.generation} with {len(packages)} packages")

    def install_plan(self, generation: str) -> None:
        generation = validate_generation(generation)
        with self._serialized():
            runtime_ready, revision, diagnostic = self._refresh_runtime_gate()
            self._reset_progress_throttle()
            state = load_state(self.paths)
            if state["status"] != "available" or state["plan_generation"] != generation:
                raise StalePlanError("the selected update plan is no longer the available plan")
            plan = load_plan(self.paths)
            if plan.generation != generation:
                raise StalePlanError("the selected generation does not match the stored exact plan")
            if not runtime_ready or revision != plan.runtime_revision:
                raise RuntimeGateError(diagnostic or "protected runtime changed after plan creation")
            current = tuple(sorted(self.backend.resolve_plan()))
            if current != plan.packages:
                raise StalePlanError("installed or candidate package versions changed; run Check again")
            self._cancel.clear()
            with AttemptLog(self.paths, int(state["state_generation"]) + 1) as log:
                self._transition(
                    status="installing",
                    progress={
                        "phase": "downloading",
                        "completed": 0,
                        "total": plan.download_size,
                        "unit": "bytes",
                        "detail": "Preparing package downloads",
                        "cancellable": True,
                    },
                    failure=None,
                    repair_available=False,
                    restart_reasons=[],
                    last_log_id=log.id,
                )
                try:
                    self.backend.install(
                        plan,
                        lambda phase, done, total, detail, cancellable: self._progress(
                            phase, done, total, detail, cancellable, log
                        ),
                        self._cancel,
                    )
                except CancelledDownload:
                    self._refresh_runtime_gate()
                    self._transition(
                        status="failed",
                        progress=None,
                        failure="Package download was cancelled before dpkg installation began.",
                        repair_available=False,
                    )
                    log.write("download cancelled before package installation")
                    return
                self._flush_progress()
                runtime_ready, revision, diagnostic = self._refresh_runtime_gate()
                if not runtime_ready or revision != plan.runtime_revision:
                    raise RuntimeGateError(
                        diagnostic or "protected runtime changed during package installation"
                    )
                reasons = read_restart_reasons(self.paths)
                if reasons:
                    self._transition(
                        status="restart_recommended",
                        progress=None,
                        failure=None,
                        repair_available=False,
                        restart_reasons=reasons,
                    )
                else:
                    self._transition(
                        status="up_to_date",
                        checked_at_unix_seconds=int(time.time()),
                        repository_errors=[],
                        packages=[],
                        download_size=0,
                        plan_generation=None,
                        progress=None,
                        failure=None,
                        repair_available=False,
                        restart_reasons=[],
                    )
                    with contextlib.suppress(FileNotFoundError):
                        self.paths.plan_path.unlink()
                log.write("package installation completed")

    def retry_repair(self, generation: str) -> None:
        generation = validate_generation(generation)
        with self._serialized():
            runtime_ready, revision, diagnostic = self._refresh_runtime_gate()
            self._reset_progress_throttle()
            state = load_state(self.paths)
            if (
                state["status"] != "failed"
                or state["plan_generation"] != generation
                or not state["repair_available"]
            ):
                raise StalePlanError("repair is not authorized for this generation")
            plan = load_plan(self.paths)
            if plan.generation != generation:
                raise StalePlanError("repair generation does not match the attempted plan")
            if not runtime_ready or revision != plan.runtime_revision:
                raise RuntimeGateError(diagnostic or "protected runtime changed after the failed plan")
            if not self.backend.needs_repair():
                raise StalePlanError("the Debian package database no longer requires repair")
            with AttemptLog(self.paths, int(state["state_generation"]) + 1) as log:
                self._transition(
                    status="installing",
                    progress={
                        "phase": "repairing",
                        "completed": 0,
                        "total": len(plan.packages),
                        "unit": "packages",
                        "detail": "Repairing the updater-generated package transaction",
                        "cancellable": False,
                    },
                    failure=None,
                    repair_available=False,
                    last_log_id=log.id,
                )
                self.backend.repair(
                    plan,
                    lambda phase, done, total, detail, cancellable: self._progress(
                        phase, done, total, detail, cancellable, log
                    ),
                )
                self._flush_progress()
                runtime_ready, revision, diagnostic = self._refresh_runtime_gate()
                if not runtime_ready or revision != plan.runtime_revision:
                    raise RuntimeGateError(
                        diagnostic or "protected runtime changed during package repair"
                    )
                reasons = read_restart_reasons(self.paths)
                if reasons:
                    self._transition(
                        status="restart_recommended",
                        progress=None,
                        failure=None,
                        repair_available=False,
                        restart_reasons=reasons,
                    )
                else:
                    self._transition(
                        status="up_to_date",
                        checked_at_unix_seconds=int(time.time()),
                        repository_errors=[],
                        packages=[],
                        download_size=0,
                        plan_generation=None,
                        progress=None,
                        failure=None,
                        repair_available=False,
                        restart_reasons=[],
                    )
                    with contextlib.suppress(FileNotFoundError):
                        self.paths.plan_path.unlink()
                log.write("repair completed")

    def cancel_download(self, generation: str) -> None:
        generation = validate_generation(generation)
        state = self.state()
        progress = state["progress"]
        if (
            state["status"] != "installing"
            or state["plan_generation"] != generation
            or not isinstance(progress, dict)
            or progress["phase"] != "downloading"
            or progress["cancellable"] is not True
        ):
            raise StalePlanError("this generation has no cancellable package download")
        self._cancel.set()

    def _record_worker_failure(self, operation: str, error: BaseException) -> None:
        self._refresh_runtime_gate()
        try:
            state = load_state(self.paths)
        except Exception:
            # Preserve an unknown/newer or otherwise unreadable state file;
            # overwriting it would destroy the evidence needed to repair it.
            return
        progress = state.get("progress")
        active_repair_phase = bool(
            isinstance(progress, dict)
            and (
                progress.get("phase") in {"installing", "repairing"}
                or (operation == "repair" and progress.get("phase") == "downloading")
            )
        )
        repair_candidate = bool(
            operation in {"install", "repair"}
            and state["status"] == "installing"
            and state["plan_generation"] is not None
            and active_repair_phase
        )
        repair_available = False
        repair_diagnostic: str | None = None
        if repair_candidate:
            try:
                repair_available = bool(self.backend.needs_repair())
            except Exception as repair_error:
                repair_diagnostic = _sanitize_dynamic_text(
                    repair_error,
                    fallback="could not inspect Debian repair state",
                )
        message = _sanitize_dynamic_text(
            f"{operation} failed: {error}",
            fallback=f"{operation} failed without a diagnostic",
        )
        if repair_diagnostic is not None:
            message = _sanitize_dynamic_text(
                f"{message}; repair inspection failed: {repair_diagnostic}",
                fallback=message,
            )
        try:
            with self._mutex:
                self._transition(
                    status="failed",
                    progress=None,
                    failure=message,
                    repair_available=repair_available,
                    restart_reasons=[],
                )
        except Exception:
            # Filesystem write failures cannot be repaired in memory.  The last
            # valid on-disk state is deliberately retained rather than replaced
            # by a schema-bypassing emergency document.
            return

    def _start(self, operation: str, function: Callable[[], None]) -> int:
        with self._mutex:
            if self._operation is not None and self._operation.is_alive():
                raise BusyError("an updater operation is already running")

            def worker() -> None:
                try:
                    function()
                except BaseException as error:  # worker boundary must persist evidence
                    self._record_worker_failure(operation, error)

            thread = threading.Thread(target=worker, name=f"wildbuzzard-updater-{operation}")
            thread.daemon = True
            self._operation = thread
            thread.start()
            return int(load_state(self.paths)["state_generation"])

    def start_check(self) -> int:
        return self._start("check", self.check)

    def start_install(self, generation: str) -> int:
        generation = validate_generation(generation)
        return self._start("install", lambda: self.install_plan(generation))

    def start_repair(self, generation: str) -> int:
        generation = validate_generation(generation)
        return self._start("repair", lambda: self.retry_repair(generation))


def read_restart_reasons(paths: Paths) -> list[str]:
    try:
        marker = paths.reboot_required.lstat()
    except FileNotFoundError:
        return []
    if stat.S_ISLNK(marker.st_mode) or not stat.S_ISREG(marker.st_mode):
        raise UpdaterError("restart marker is not a regular file")
    reasons = ["A guest restart is recommended by installed packages."]
    try:
        packages_metadata = paths.reboot_packages.lstat()
    except FileNotFoundError:
        packages_metadata = None
    if packages_metadata is not None:
        if stat.S_ISLNK(packages_metadata.st_mode) or not stat.S_ISREG(packages_metadata.st_mode):
            raise UpdaterError("restart package evidence is not a regular file")
        if packages_metadata.st_size > MAX_JSON_BYTES:
            raise UpdaterError("restart package evidence exceeds the bounded limit")
        flags = (
            os.O_RDONLY
            | os.O_CLOEXEC
            | getattr(os, "O_NOFOLLOW", 0)
            | getattr(os, "O_NONBLOCK", 0)
        )
        descriptor = os.open(paths.reboot_packages, flags)
        try:
            opened = os.fstat(descriptor)
            if not stat.S_ISREG(opened.st_mode) or opened.st_size > MAX_JSON_BYTES:
                raise UpdaterError("restart package evidence changed to an unsafe file")
            data = bytearray()
            while len(data) <= MAX_JSON_BYTES:
                chunk = os.read(descriptor, min(65_536, MAX_JSON_BYTES + 1 - len(data)))
                if not chunk:
                    break
                data.extend(chunk)
        finally:
            os.close(descriptor)
        if len(data) > MAX_JSON_BYTES:
            raise UpdaterError("restart package evidence exceeds the bounded limit")
        packages = []
        for line in bytes(data).decode("utf-8", "strict").splitlines():
            value = line.strip()
            if value and PACKAGE_RE.fullmatch(value):
                packages.append(value)
        if packages:
            reasons.append("Restart requested by: " + ", ".join(sorted(set(packages))[:128]))
    return reasons


class PythonAptBackend:
    """python-apt backend.  No subprocess or apt command string is used."""

    def __init__(
        self,
        rootdir: str | None = None,
        lock_timeout_seconds: float = PACKAGE_LOCK_TIMEOUT_SECONDS,
    ):
        self.rootdir = rootdir
        self.lock_timeout_seconds = lock_timeout_seconds

    def _root_path(self, absolute: str) -> Path:
        relative = absolute.removeprefix("/")
        return Path(self.rootdir or "/") / relative

    @staticmethod
    def _lock_owner(metadata: os.stat_result) -> str:
        identity = (
            f"{os.major(metadata.st_dev):02x}:"
            f"{os.minor(metadata.st_dev):02x}:{metadata.st_ino}"
        )
        try:
            descriptor = os.open(
                "/proc/locks",
                os.O_RDONLY | os.O_CLOEXEC | getattr(os, "O_NONBLOCK", 0),
            )
            try:
                data = bytearray()
                while len(data) <= MAX_JSON_BYTES:
                    chunk = os.read(descriptor, min(65_536, MAX_JSON_BYTES + 1 - len(data)))
                    if not chunk:
                        break
                    data.extend(chunk)
            finally:
                os.close(descriptor)
            if len(data) > MAX_JSON_BYTES:
                return "another package process"
            lines = bytes(data).decode("ascii", "replace").splitlines()
        except OSError:
            return "another package process"
        for line in lines:
            fields = line.split()
            if len(fields) >= 6 and fields[5] == identity:
                pid = fields[4]
                return f"package lock owner PID {pid}" if pid != "-1" else "remote package lock owner"
        return "another package process"

    def _busy_package_lock(self) -> tuple[Path, str] | None:
        for absolute in (
            "/var/lib/dpkg/lock-frontend",
            "/var/lib/dpkg/lock",
            "/var/lib/apt/lists/lock",
            "/var/cache/apt/archives/lock",
        ):
            path = self._root_path(absolute)
            try:
                metadata = path.lstat()
            except FileNotFoundError:
                continue
            if stat.S_ISLNK(metadata.st_mode) or not stat.S_ISREG(metadata.st_mode):
                raise UpdaterError(f"package lock path is not a regular file: {path}")
            descriptor = os.open(
                path,
                os.O_RDWR
                | os.O_CLOEXEC
                | getattr(os, "O_NOFOLLOW", 0)
                | getattr(os, "O_NONBLOCK", 0),
            )
            try:
                opened = os.fstat(descriptor)
                try:
                    fcntl.lockf(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    return path, self._lock_owner(opened)
                finally:
                    with contextlib.suppress(OSError):
                        fcntl.lockf(descriptor, fcntl.LOCK_UN)
            finally:
                os.close(descriptor)
        return None

    def _wait_for_package_locks(
        self,
        progress: Callable[[str, int, int, str | None, bool], None] | None = None,
        phase: str = "resolving",
    ) -> None:
        deadline = time.monotonic() + max(0.0, self.lock_timeout_seconds)
        last_detail: str | None = None
        while True:
            busy = self._busy_package_lock()
            if busy is None:
                return
            path, owner = busy
            last_detail = _sanitize_dynamic_text(
                f"Waiting for {path.name}: {owner}",
                fallback="Waiting for the Debian package database lock",
            )
            if progress is not None:
                progress(phase, 0, 1, last_detail, False)
            if time.monotonic() >= deadline:
                raise BusyError(
                    _sanitize_dynamic_text(
                        f"timed out waiting for Debian package lock {path}: {owner}",
                        fallback=last_detail or "timed out waiting for a Debian package lock",
                    )
                )
            time.sleep(0.1)

    def _modules(self):
        try:
            import apt
            import apt.progress.base
        except ImportError as error:
            raise UpdaterError("python3-apt is not installed in this guest") from error
        return apt, apt.progress.base

    def _cache(self):
        apt, _ = self._modules()
        try:
            return apt.Cache(rootdir=self.rootdir)
        except Exception as error:
            detail = _sanitize_dynamic_text(error, fallback="python-apt cache error")
            raise UpdaterError(f"cannot open the Debian package cache: {detail}") from error

    @staticmethod
    def _system_lock():
        try:
            import apt_pkg
        except ImportError as error:
            raise UpdaterError("python3-apt is not installed in this guest") from error
        return apt_pkg.SystemLock()

    @staticmethod
    def _security_origin(candidate: object) -> str | None:
        values = []
        for origin in getattr(candidate, "origins", []):
            text = " ".join(
                str(getattr(origin, field, "") or "")
                for field in ("origin", "label", "archive", "site", "component")
            )
            if "security" in text.casefold():
                values.append(text.strip())
        return (
            _sanitize_dynamic_text(
                sorted(set(value for value in values if value))[0],
                fallback="Debian security repository",
            )
            if values
            else None
        )

    def _resolve(self, cache: object) -> tuple[PackageRecord, ...]:
        try:
            cache.upgrade(dist_upgrade=False)
        except Exception as error:
            detail = _sanitize_dynamic_text(error, fallback="python-apt dependency resolution error")
            raise UpdaterError(f"cannot resolve a safe Debian upgrade plan: {detail}") from error
        records = []
        for package in cache.get_changes():
            if package.marked_delete:
                raise UpdaterError(
                    f"safe update resolution unexpectedly requested package removal: {package.name}"
                )
            if not (package.marked_upgrade or package.marked_install):
                continue
            candidate = package.candidate
            if candidate is None:
                raise UpdaterError(f"package has no candidate version: {package.name}")
            installed = package.installed
            records.append(
                PackageRecord(
                    name=package.name,
                    installed_version=installed.version if installed else "not installed",
                    candidate_version=candidate.version,
                    download_size=max(0, int(candidate.size)),
                    security_origin=self._security_origin(candidate),
                    action="upgrade" if installed else "install",
                )
            )
        return tuple(sorted(records))

    def refresh(self, progress: Callable[[str, int, int, str | None, bool], None]) -> list[str]:
        apt, base = self._modules()
        errors: list[str] = []
        self._wait_for_package_locks(progress, "refreshing")

        class Acquire(base.AcquireProgress):
            def pulse(self, owner):  # type: ignore[no-untyped-def]
                total = max(1, int(getattr(owner, "total_bytes", 0)))
                current = max(0, min(int(getattr(owner, "current_bytes", 0)), total))
                progress("refreshing", current, total, "Refreshing repository metadata", False)
                return True

            def fail(self, item):  # type: ignore[no-untyped-def]
                description = str(getattr(item, "description", "repository item"))
                error_text = str(getattr(item, "error_text", "download failed"))
                if len(errors) < MAX_REPOSITORY_ERRORS:
                    errors.append(
                        _sanitize_dynamic_text(
                            f"{description}: {error_text}",
                            fallback="Repository item download failed",
                        )
                    )

        cache = self._cache()
        try:
            result = cache.update(fetch_progress=Acquire())
            cache.open()
        except Exception as error:
            detail = _sanitize_dynamic_text(error, fallback="python-apt refresh error")
            raise UpdaterError(f"repository refresh failed: {detail}") from error
        if result is False and not errors:
            errors.append("python-apt reported that repository refresh did not complete")
        return errors[:MAX_REPOSITORY_ERRORS]

    def resolve_plan(self) -> tuple[PackageRecord, ...]:
        self._wait_for_package_locks()
        return self._resolve(self._cache())

    def install(
        self,
        plan: Plan,
        progress: Callable[[str, int, int, str | None, bool], None],
        cancelled: threading.Event,
    ) -> None:
        _, base = self._modules()
        self._wait_for_package_locks(progress, "downloading")
        if self.needs_repair():
            raise UpdaterError(
                "the Debian package database was already incomplete before this update; "
                "repair it explicitly before creating a new plan"
            )
        resolved = self._resolve(self._cache())
        if resolved != plan.packages:
            raise StalePlanError("package candidates changed before the transaction lock was acquired")

        class Acquire(base.AcquireProgress):
            def pulse(self, owner):  # type: ignore[no-untyped-def]
                total = max(0, int(getattr(owner, "total_bytes", plan.download_size)))
                current = max(0, min(int(getattr(owner, "current_bytes", 0)), total))
                progress(
                    "downloading",
                    current,
                    total,
                    _current_debian_download_detail(owner),
                    True,
                )
                return not cancelled.is_set()

            def stop(self):
                if cancelled.is_set():
                    raise CancelledDownload("package download cancelled")

        class Install(base.InstallProgress):
            def status_change(self, pkg, percent, status_text):  # type: ignore[no-untyped-def]
                completed = max(0, min(int(percent * len(plan.packages) / 100), len(plan.packages)))
                detail = _sanitize_dynamic_text(
                    f"{pkg}: {status_text}", fallback="Installing Debian package"
                )
                progress("installing", completed, len(plan.packages), detail, False)

            def error(self, pkg, errormsg):  # type: ignore[no-untyped-def]
                detail = _sanitize_dynamic_text(
                    f"{pkg}: {errormsg}", fallback="dpkg package failure"
                )
                raise UpdaterError(f"dpkg failed while processing {detail}")

        try:
            # Cache.commit() takes the same apt system lock internally.  apt's
            # lock is reference-counted, so holding it here closes the race
            # between exact-plan validation and commit while the nested lock
            # remains balanced.
            with self._system_lock():
                cache = self._cache()
                locked_resolved = self._resolve(cache)
                if locked_resolved != plan.packages:
                    raise StalePlanError(
                        "package candidates changed while acquiring the transaction lock"
                    )
                cache.commit(fetch_progress=Acquire(), install_progress=Install())
        except CancelledDownload:
            raise
        except UpdaterError:
            raise
        except Exception as error:
            if cancelled.is_set():
                raise CancelledDownload("package download cancelled") from error
            detail = _sanitize_dynamic_text(error, fallback="python-apt transaction error")
            raise UpdaterError(f"Debian package transaction failed: {detail}") from error
        verification = self._cache()
        for package in plan.packages:
            installed = verification[package.name].installed
            if installed is None or installed.version != package.candidate_version:
                raise UpdaterError(
                    f"installed version verification failed for {package.name}: "
                    f"expected {package.candidate_version}"
                )

    def repair(
        self,
        plan: Plan,
        progress: Callable[[str, int, int, str | None, bool], None],
    ) -> None:
        _, base = self._modules()
        self._wait_for_package_locks(progress, "repairing")

        class Acquire(base.AcquireProgress):
            def pulse(self, owner):  # type: ignore[no-untyped-def]
                total = max(0, int(getattr(owner, "total_bytes", 0)))
                current = max(0, min(int(getattr(owner, "current_bytes", 0)), total))
                progress("downloading", current, total, "Downloading repair dependencies", False)
                return True

        class Install(base.InstallProgress):
            def status_change(self, pkg, percent, status_text):  # type: ignore[no-untyped-def]
                completed = max(0, min(int(percent * max(1, len(changes)) / 100), max(1, len(changes))))
                detail = _sanitize_dynamic_text(
                    f"{pkg}: {status_text}", fallback="Repairing Debian package"
                )
                progress("repairing", completed, max(1, len(changes)), detail, False)

        try:
            with self._system_lock():
                cache = self._cache()
                try:
                    cache.fix_broken()
                except Exception as error:
                    detail = _sanitize_dynamic_text(
                        error, fallback="python-apt repair resolution error"
                    )
                    raise UpdaterError(
                        f"python-apt could not construct a repair transaction: {detail}"
                    ) from error
                changes = cache.get_changes()
                if not changes and cache.broken_count == 0:
                    return
                authorized = {package.name: package for package in plan.packages}
                unauthorized = sorted(
                    package.name for package in changes if package.name not in authorized
                )
                if unauthorized:
                    raise UpdaterError(
                        "repair would modify packages outside the updater-generated transaction: "
                        + ", ".join(unauthorized[:32])
                    )
                for package in changes:
                    if package.marked_delete:
                        raise UpdaterError(
                            f"repair unexpectedly requested package removal: {package.name}"
                        )
                    expected = authorized[package.name]
                    candidate = package.candidate
                    if package.marked_install or package.marked_upgrade:
                        if candidate is None:
                            raise StalePlanError(
                                f"repair candidate disappeared for {package.name}; run Check again"
                            )
                        if candidate.version != expected.candidate_version:
                            raise StalePlanError(
                                f"repair candidate changed for {package.name}; run Check again"
                            )
                cache.commit(fetch_progress=Acquire(), install_progress=Install())
        except UpdaterError:
            raise
        except Exception as error:
            detail = _sanitize_dynamic_text(error, fallback="python-apt repair error")
            raise UpdaterError(f"Debian package repair failed: {detail}") from error
        verification = self._cache()
        if verification.broken_count:
            raise UpdaterError("Debian package database still reports broken packages after repair")
        for package in plan.packages:
            installed = verification[package.name].installed
            if installed is None or installed.version != package.candidate_version:
                raise UpdaterError(
                    f"repair did not complete the exact updater plan for {package.name}: "
                    f"expected {package.candidate_version}"
                )

    def needs_repair(self) -> bool:
        self._wait_for_package_locks()
        cache = self._cache()
        if int(getattr(cache, "broken_count", 0)) > 0:
            return True
        try:
            import apt_pkg
        except ImportError as error:
            raise UpdaterError("python3-apt is not installed in this guest") from error
        incomplete_states = {
            apt_pkg.CURSTATE_UNPACKED,
            apt_pkg.CURSTATE_HALF_CONFIGURED,
            apt_pkg.CURSTATE_HALF_INSTALLED,
        }
        reinstall_required = getattr(apt_pkg, "INSTSTATE_REINSTREQ", None)
        for package in cache:
            raw = getattr(package, "_pkg", None)
            if raw is None:
                continue
            if getattr(raw, "current_state", None) in incomplete_states:
                return True
            if reinstall_required is not None and getattr(raw, "inst_state", None) == reinstall_required:
                return True
        return False
