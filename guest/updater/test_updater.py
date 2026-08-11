#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later

from __future__ import annotations

import hashlib
import gzip
import json
import os
import stat
import subprocess
import shutil
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest import mock
from email.utils import formatdate

sys.path.insert(0, str(Path(__file__).resolve().parent))

import updater_core as updater
import wildbuzzard_updater as updater_service


REVISION = "test-runtime-1"
GENERATION = "a" * 64


def canonical_json(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


class RuntimeFixture:
    def __init__(self, root: Path):
        self.root = root
        self.runtime = root / "runtime"
        self.revision = self.runtime / REVISION
        self.state = root / "state"
        self.logs = root / "logs"
        self.dpkg_info = root / "dpkg-info"
        self.run = root / "run"
        for directory in (
            self.runtime,
            self.revision,
            self.state,
            self.logs,
            self.dpkg_info,
            self.run,
        ):
            directory.mkdir(parents=True, mode=0o755)
            directory.chmod(0o755)
        files: dict[str, dict[str, object]] = {}
        for relative in sorted(updater.REQUIRED_RUNTIME_FILES):
            path = self.revision / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            for parent in (path.parent, *path.parent.parents):
                if parent == self.revision.parent:
                    break
                if parent.is_relative_to(self.revision):
                    parent.chmod(0o755)
            payload = f"fixture:{relative}\n".encode()
            path.write_bytes(payload)
            path.chmod(0o755)
            files[relative] = {
                "sha256": hashlib.sha256(payload).hexdigest(),
                "mode": 0o755,
            }
        manifest = {
            "schema_version": updater.RUNTIME_MANIFEST_SCHEMA_VERSION,
            "revision": REVISION,
            "files": files,
        }
        manifest_path = self.revision / "runtime.manifest.json"
        manifest_path.write_bytes(canonical_json(manifest) + b"\n")
        manifest_path.chmod(0o644)
        readiness = {
            "schema_version": updater.RUNTIME_READINESS_SCHEMA_VERSION,
            "revision": REVISION,
            "manifest_sha256": hashlib.sha256(canonical_json(manifest)).hexdigest(),
            "ready": True,
        }
        readiness_path = self.revision / "readiness.json"
        readiness_path.write_bytes(canonical_json(readiness) + b"\n")
        readiness_path.chmod(0o644)
        (self.runtime / "current").symlink_to(REVISION)

    def paths(self) -> updater.Paths:
        return updater.Paths(
            state_dir=self.state,
            log_dir=self.logs,
            lock_path=self.run / "updater.lock",
            runtime_root=self.runtime,
            dpkg_info=self.dpkg_info,
            reboot_required=self.run / "reboot-required",
            reboot_packages=self.run / "reboot-required.pkgs",
            runtime_owner_uid=os.getuid(),
        )


class FakeBackend:
    def __init__(self, packages: tuple[updater.PackageRecord, ...] = ()):
        self.packages = packages
        self.repository_errors: list[str] = []
        self.install_error: Exception | None = None
        self.repair_error: Exception | None = None
        self.repair_needed = False
        self.refresh_pulses = 0

    def refresh(self, progress):
        for index in range(self.refresh_pulses):
            progress("refreshing", index, max(1, self.refresh_pulses), "refresh", False)
        return list(self.repository_errors)

    def resolve_plan(self):
        return self.packages

    def install(self, plan, progress, cancelled: threading.Event):
        progress("downloading", 1, max(1, plan.download_size), "download", True)
        if isinstance(self.install_error, updater.CancelledDownload):
            raise self.install_error
        progress("installing", 0, len(plan.packages), "install", False)
        if self.install_error is not None:
            raise self.install_error
        progress("installing", len(plan.packages), len(plan.packages), "done", False)
        self.packages = ()

    def repair(self, plan, progress):
        progress("repairing", 0, len(plan.packages), "repair", False)
        if self.repair_error is not None:
            raise self.repair_error
        self.repair_needed = False
        self.packages = ()

    def needs_repair(self):
        return self.repair_needed


def one_package() -> tuple[updater.PackageRecord, ...]:
    return (
        updater.PackageRecord(
            name="example-package",
            installed_version="1.0-1",
            candidate_version="1.0-2",
            download_size=4096,
            security_origin="Debian security",
            action="upgrade",
        ),
    )


def wait_for_worker(engine: updater.UpdateEngine) -> None:
    operation = engine._operation
    if operation is not None:
        operation.join(timeout=5)
        if operation.is_alive():
            raise AssertionError("updater worker did not finish")


class UpdaterCoreTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.fixture = RuntimeFixture(Path(self.temporary.name))
        self.paths = self.fixture.paths()

    def tearDown(self):
        self.temporary.cleanup()

    def test_runtime_gate_accepts_exact_protected_revision(self):
        self.assertEqual(updater.inspect_runtime_gate(self.paths), (True, REVISION, None))

    def test_runtime_gate_rejects_group_writable_component(self):
        (self.fixture.revision / "libexec").chmod(0o775)
        ready, revision, diagnostic = updater.inspect_runtime_gate(self.paths)
        self.assertFalse(ready)
        self.assertIsNone(revision)
        self.assertIn("group/world writable", diagnostic or "")

    def test_runtime_gate_rejects_symlinked_payload_component(self):
        binary = self.fixture.revision / "bin" / "sway"
        binary.unlink()
        binary.symlink_to("swaymsg")
        ready, _, diagnostic = updater.inspect_runtime_gate(self.paths)
        self.assertFalse(ready)
        self.assertIn("symbolic link", diagnostic or "")

    def test_runtime_gate_rejects_unmanifested_payload(self):
        extra = self.fixture.revision / "libexec/hidden-replacement"
        extra.write_text("unexpected\n", encoding="utf-8")
        extra.chmod(0o755)
        ready, _, diagnostic = updater.inspect_runtime_gate(self.paths)
        self.assertFalse(ready)
        self.assertIn("unmanifested file", diagnostic or "")

    def test_boolean_is_not_accepted_as_json_integer(self):
        state = updater._empty_state(self.paths)
        state["state_generation"] = True
        with self.assertRaisesRegex(updater.UpdaterError, "generation"):
            updater.validate_state(state)
        package = one_package()[0]
        with self.assertRaisesRegex(updater.UpdaterError, "download size"):
            updater.validate_package(
                updater.PackageRecord(
                    package.name,
                    package.installed_version,
                    package.candidate_version,
                    True,
                    package.security_origin,
                    package.action,
                )
            )

    def test_progress_unit_must_match_its_phase(self):
        progress = {
            "phase": "downloading",
            "completed": 1,
            "total": 2,
            "unit": "packages",
            "detail": None,
            "cancellable": False,
        }
        with self.assertRaisesRegex(updater.UpdaterError, "does not match"):
            updater.validate_progress(progress)

    def test_duplicate_json_keys_are_rejected(self):
        path = Path(self.temporary.name) / "duplicate.json"
        path.write_text('{"schema_version":2,"schema_version":2}', encoding="utf-8")
        with self.assertRaisesRegex(updater.UpdaterError, "duplicate key"):
            updater.read_bounded_json(path)

    def test_managed_json_fifo_is_rejected_without_waiting_for_a_writer(self):
        path = Path(self.temporary.name) / "state.fifo"
        os.mkfifo(path)
        started = time.monotonic()
        with self.assertRaisesRegex(updater.UpdaterError, "regular file"):
            updater.read_bounded_json(path)
        self.assertLess(time.monotonic() - started, 0.5)

    def test_broken_state_symlink_is_not_treated_as_missing(self):
        self.paths.state_path.symlink_to("missing-state-target")
        with self.assertRaises(OSError):
            updater.load_state(self.paths)
        self.assertTrue(self.paths.state_path.is_symlink())

    def test_check_publishes_exact_available_plan(self):
        backend = FakeBackend(one_package())
        engine = updater.UpdateEngine(backend, self.paths)
        engine.check()
        state = engine.state()
        self.assertEqual(state["status"], "available")
        self.assertEqual(state["packages"], [one_package()[0].to_json()])
        self.assertRegex(state["plan_generation"], updater.GENERATION_RE)
        plan = updater.load_plan(self.paths)
        self.assertEqual(plan.packages, one_package())
        self.assertEqual(plan.runtime_revision, REVISION)
        self.assertEqual(stat.S_IMODE(self.paths.state_dir.stat().st_mode), 0o755)
        self.assertEqual(stat.S_IMODE(self.paths.plan_path.stat().st_mode), 0o600)

    def test_check_without_candidates_is_up_to_date(self):
        engine = updater.UpdateEngine(FakeBackend(), self.paths)
        engine.check()
        state = engine.state()
        self.assertEqual(state["status"], "up_to_date")
        self.assertEqual(state["packages"], [])
        self.assertIsNone(state["plan_generation"])

    def test_terminal_check_refreshes_the_cached_runtime_gate(self):
        protected = self.fixture.revision / "bin/sway"

        class MutatingResolve(FakeBackend):
            def resolve_plan(inner_self):
                protected.write_text("changed after operation start\n", encoding="utf-8")
                return ()

        engine = updater.UpdateEngine(MutatingResolve(), self.paths)
        engine.check()
        state = engine.state()
        self.assertEqual(state["status"], "up_to_date")
        self.assertFalse(state["runtime_ready"])
        self.assertIsNone(state["runtime_revision"])

    def test_stale_candidate_set_is_rejected_before_dpkg(self):
        backend = FakeBackend(one_package())
        engine = updater.UpdateEngine(backend, self.paths)
        engine.check()
        generation = engine.state()["plan_generation"]
        backend.packages = ()
        with self.assertRaisesRegex(updater.StalePlanError, "changed"):
            engine.install_plan(generation)
        self.assertEqual(engine.state()["status"], "available")

    def test_cancelled_download_never_offers_dpkg_repair(self):
        backend = FakeBackend(one_package())
        engine = updater.UpdateEngine(backend, self.paths)
        engine.check()
        generation = engine.state()["plan_generation"]
        backend.install_error = updater.CancelledDownload("cancelled")
        engine.install_plan(generation)
        state = engine.state()
        self.assertEqual(state["status"], "failed")
        self.assertFalse(state["repair_available"])

    def test_only_proven_failed_transaction_can_be_repaired(self):
        backend = FakeBackend(one_package())
        engine = updater.UpdateEngine(backend, self.paths)
        engine.check()
        generation = engine.state()["plan_generation"]
        backend.install_error = RuntimeError("dpkg fixture failure")
        backend.repair_needed = True
        engine.start_install(generation)
        wait_for_worker(engine)
        self.assertTrue(engine.state()["repair_available"])
        backend.install_error = None
        engine.start_repair(generation)
        wait_for_worker(engine)
        self.assertEqual(engine.state()["status"], "up_to_date")

    def test_package_transaction_cannot_replace_protected_runtime_payload(self):
        guest_usr_sway = self.fixture.root / "usr/bin/sway"

        class MutatingBackend(FakeBackend):
            def install(inner_self, plan, progress, cancelled):
                guest_usr_sway.parent.mkdir(parents=True, exist_ok=True)
                guest_usr_sway.write_text("dpkg-owned sway\n", encoding="utf-8")
                super().install(plan, progress, cancelled)

        protected = self.fixture.revision / "bin/sway"
        before = hashlib.sha256(protected.read_bytes()).hexdigest()
        backend = MutatingBackend(one_package())
        engine = updater.UpdateEngine(backend, self.paths)
        engine.check()
        engine.install_plan(engine.state()["plan_generation"])
        self.assertEqual(hashlib.sha256(protected.read_bytes()).hexdigest(), before)
        self.assertEqual(updater.inspect_runtime_gate(self.paths), (True, REVISION, None))

    def test_repair_rejects_changes_outside_exact_failed_plan(self):
        plan = updater.Plan(
            generation=GENERATION,
            checked_at_unix_seconds=1,
            packages=one_package(),
            download_size=one_package()[0].download_size,
            runtime_revision=REVISION,
        )

        class Change:
            name = "unrelated-user-package"
            marked_delete = False
            marked_install = True
            marked_upgrade = False
            candidate = type("Candidate", (), {"version": "9"})()

        class Cache:
            broken_count = 1

            @staticmethod
            def fix_broken():
                return None

            @staticmethod
            def get_changes():
                return [Change()]

        class Base:
            class AcquireProgress:
                pass

            class InstallProgress:
                pass

        backend = updater.PythonAptBackend(rootdir=str(self.fixture.root))
        with (
            mock.patch.object(backend, "_wait_for_package_locks"),
            mock.patch.object(backend, "_modules", return_value=(object(), Base)),
            mock.patch.object(backend, "_cache", return_value=Cache()),
            mock.patch.object(backend, "_system_lock", return_value=mock.MagicMock()),
        ):
            with self.assertRaisesRegex(updater.UpdaterError, "outside"):
                backend.repair(plan, lambda *_: None)

    def test_install_revalidates_the_exact_plan_while_holding_the_apt_lock(self):
        plan = updater.Plan(
            generation=GENERATION,
            checked_at_unix_seconds=1,
            packages=one_package(),
            download_size=one_package()[0].download_size,
            runtime_revision=REVISION,
        )
        events: list[str] = []

        class Lock:
            def __enter__(self):
                events.append("lock-entered")
                return self

            def __exit__(self, *_args):
                events.append("lock-released")
                return False

        class Cache:
            committed = False

            def commit(self, **_kwargs):
                self.committed = True

        class Base:
            class AcquireProgress:
                pass

            class InstallProgress:
                pass

        cache = Cache()
        backend = updater.PythonAptBackend(rootdir=str(self.fixture.root))
        with (
            mock.patch.object(backend, "_wait_for_package_locks"),
            mock.patch.object(backend, "needs_repair", return_value=False),
            mock.patch.object(backend, "_modules", return_value=(object(), Base)),
            mock.patch.object(backend, "_cache", return_value=cache),
            mock.patch.object(backend, "_system_lock", return_value=Lock()),
            mock.patch.object(backend, "_resolve", side_effect=(plan.packages, ())),
        ):
            with self.assertRaisesRegex(updater.StalePlanError, "acquiring"):
                backend.install(plan, lambda *_: None, threading.Event())
        self.assertEqual(events, ["lock-entered", "lock-released"])
        self.assertFalse(cache.committed)

    def test_package_lock_contention_reports_owner_and_times_out(self):
        lock = self.fixture.root / "var/lib/dpkg/lock-frontend"
        lock.parent.mkdir(parents=True, exist_ok=True)
        lock.touch()
        holder = subprocess.Popen(
            [
                sys.executable,
                "-c",
                (
                    "import fcntl, os, sys; "
                    "fd=os.open(sys.argv[1], os.O_RDWR); "
                    "fcntl.lockf(fd, fcntl.LOCK_EX); "
                    "print(os.getpid(), flush=True); "
                    "sys.stdin.buffer.read(1)"
                ),
                str(lock),
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            assert holder.stdout is not None
            owner_pid = holder.stdout.readline().strip()
            self.assertRegex(owner_pid, r"^[1-9][0-9]*$")
            backend = updater.PythonAptBackend(
                rootdir=str(self.fixture.root),
                lock_timeout_seconds=0.2,
            )
            evidence: list[str] = []
            started = time.monotonic()
            with self.assertRaises(updater.BusyError) as raised:
                backend._wait_for_package_locks(
                    lambda _phase, _done, _total, detail, _cancellable: evidence.append(
                        detail or ""
                    )
                )
            elapsed = time.monotonic() - started
            self.assertGreaterEqual(elapsed, 0.18)
            self.assertLess(elapsed, 2.0)
            self.assertIn(f"PID {owner_pid}", str(raised.exception))
            self.assertTrue(any(f"PID {owner_pid}" in detail for detail in evidence))
        finally:
            if holder.stdin is not None:
                holder.stdin.write("x")
                holder.stdin.flush()
                holder.stdin.close()
            holder.wait(timeout=2)
            if holder.stdout is not None:
                holder.stdout.close()
            if holder.stderr is not None:
                holder.stderr.close()

    def test_repository_errors_are_sanitized_bounded_and_persisted(self):
        backend = FakeBackend()
        backend.repository_errors = ["bad\x00\nrepository " + "x" * 100_000]
        engine = updater.UpdateEngine(backend, self.paths)
        engine.start_check()
        wait_for_worker(engine)
        state = engine.state()
        self.assertEqual(state["status"], "failed")
        self.assertEqual(len(state["repository_errors"]), 1)
        evidence = state["repository_errors"][0]
        self.assertNotIn("\x00", evidence)
        self.assertNotIn("\n", evidence)
        self.assertLessEqual(len(evidence.encode()), updater.MAX_DYNAMIC_TEXT_BYTES)

    def test_worker_failure_cannot_leave_installing_state_stuck(self):
        backend = FakeBackend(one_package())
        engine = updater.UpdateEngine(backend, self.paths)
        engine.check()
        generation = engine.state()["plan_generation"]
        backend.install_error = RuntimeError("broken\n" + "z" * 100_000)
        backend.repair_needed = True
        engine.start_install(generation)
        wait_for_worker(engine)
        state = engine.state()
        self.assertEqual(state["status"], "failed")
        self.assertTrue(state["repair_available"])
        self.assertLessEqual(len(state["failure"].encode()), updater.MAX_DYNAMIC_TEXT_BYTES)

    def test_restart_reconciles_interrupted_download_without_repair(self):
        backend = FakeBackend(one_package())
        engine = updater.UpdateEngine(backend, self.paths)
        engine.check()
        engine._transition(
            status="installing",
            progress={
                "phase": "downloading",
                "completed": 1,
                "total": 4096,
                "unit": "bytes",
                "detail": "download",
                "cancellable": True,
            },
        )
        restarted = updater.UpdateEngine(backend, self.paths)
        state = restarted.state()
        self.assertEqual(state["status"], "failed")
        self.assertFalse(state["repair_available"])
        self.assertIn("restarted", state["failure"])

    def test_restart_reconciles_interrupted_dpkg_with_proven_repair(self):
        backend = FakeBackend(one_package())
        engine = updater.UpdateEngine(backend, self.paths)
        engine.check()
        engine._transition(
            status="installing",
            progress={
                "phase": "installing",
                "completed": 0,
                "total": 1,
                "unit": "packages",
                "detail": "install",
                "cancellable": False,
            },
        )
        backend.repair_needed = True
        restarted = updater.UpdateEngine(backend, self.paths)
        self.assertTrue(restarted.state()["repair_available"])

    def test_progress_is_coalesced_and_final_state_is_flushed(self):
        backend = FakeBackend(one_package())
        backend.refresh_pulses = 1_000
        engine = updater.UpdateEngine(backend, self.paths)
        before = int(engine.state()["state_generation"])
        with mock.patch.object(updater.time, "monotonic", return_value=50.0):
            engine.check()
        state = engine.state()
        self.assertEqual(state["status"], "available")
        self.assertLess(int(state["state_generation"]) - before, 15)

    def test_legacy_v1_state_is_explicitly_invalidated(self):
        legacy = {
            "schema_version": 1,
            "status": "never_checked",
            "checked_at_unix_seconds": None,
            "repository_errors": [],
            "packages": [],
            "download_size": 0,
            "plan_generation": None,
        }
        updater.atomic_write_json(self.paths.state_path, legacy, 0o644)
        engine = updater.UpdateEngine(FakeBackend(), self.paths)
        state = engine.state()
        self.assertEqual(state["schema_version"], 2)
        self.assertEqual(state["status"], "failed")
        self.assertIn("schema 1", state["failure"])

    def test_restart_package_fifo_is_rejected_without_blocking(self):
        self.paths.reboot_required.write_text("restart", encoding="utf-8")
        os.mkfifo(self.paths.reboot_packages)
        with self.assertRaisesRegex(updater.UpdaterError, "regular file"):
            updater.read_restart_reasons(self.paths)

    def test_attempt_log_retries_short_writes(self):
        original_write = os.write

        def short_write(descriptor: int, data: object) -> int:
            view = memoryview(data)
            return original_write(descriptor, view[: min(3, len(view))])

        with mock.patch.object(updater.os, "write", side_effect=short_write):
            with updater.AttemptLog(self.paths, 1) as log:
                log.write("complete short-write evidence")
            contents = log.path.read_text(encoding="utf-8")
        self.assertIn("operation log opened", contents)
        self.assertIn("complete short-write evidence", contents)

    def test_attempt_log_pruning_always_retains_the_active_log(self):
        timestamp = 1_700_000_000_000_000_000
        for index in range(updater.MAX_LOG_FILES + 4):
            path = self.paths.log_dir / f"attempt-{index + 1}-{index:016x}.log"
            path.write_text("old\n", encoding="utf-8")
            os.utime(path, ns=(timestamp, timestamp))
        with updater.AttemptLog(self.paths, 99) as log:
            current = log.path
        retained = list(self.paths.log_dir.glob("attempt-*.log"))
        self.assertIn(current, retained)
        self.assertLessEqual(len(retained), updater.MAX_LOG_FILES)


class UpdaterInterfaceTests(unittest.TestCase):
    def test_transient_worker_has_only_fixed_systemd_unit_arguments(self):
        command = updater_service._transient_worker_command(
            "install",
            GENERATION,
            "wildbuzzard-update-install-0123456789abcdef",
        )
        self.assertEqual(command[0], "/usr/bin/systemd-run")
        self.assertIn("--property=Type=exec", command)
        self.assertIn("--property=UMask=0077", command)
        self.assertIn("--setenv=PYTHONDONTWRITEBYTECODE=1", command)
        worker_separator = command.index("--")
        self.assertEqual(command[worker_separator + 2], "-B")
        self.assertEqual(command[-2:], ["--worker-install", GENERATION])
        self.assertNotIn("sh", command)
        self.assertNotIn("-c", command)

        with self.assertRaises(updater.UpdaterError):
            updater_service._transient_worker_command(
                "install",
                GENERATION,
                "wildbuzzard-update-install-bad;name",
            )

    def test_dbus_introspection_exposes_only_fixed_methods(self):
        script = Path(__file__).resolve().parent / "wildbuzzard_updater.py"
        result = subprocess.run(
            ["/usr/bin/python3", str(script), "--print-introspection"],
            check=True,
            capture_output=True,
            text=True,
        )
        for method in ("Check", "GetState", "InstallPlan", "RetryRepair", "CancelDownload"):
            self.assertIn(f'method name="{method}"', result.stdout)
        for forbidden in ("command", "package", "path", "repository", "argument"):
            self.assertNotIn(f'arg name="{forbidden}"', result.stdout)


class SignedLocalAptTests(unittest.TestCase):
    def test_python_apt_resolves_a_signed_local_repository_without_host_mutation(self):
        required = ("dpkg-deb", "dpkg-scanpackages", "gpg")
        if any(shutil.which(tool) is None for tool in required):
            self.skipTest("local signed-APT fixture tools are unavailable")
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            root = base / "apt-root"
            repo = base / "repo"
            gnupg = base / "gnupg"
            package = base / "package"
            for directory in (
                root / "etc/apt/sources.list.d",
                root / "etc/apt/keyrings",
                root / "var/lib/dpkg",
                root / "var/lib/apt/lists/partial",
                root / "var/cache/apt/archives/partial",
                repo / "pool/main/w/wb-updater-fixture",
                repo / "dists/stable/main/binary-amd64",
                gnupg,
                package / "DEBIAN",
            ):
                directory.mkdir(parents=True, exist_ok=True)
            gnupg.chmod(0o700)
            (package / "DEBIAN/control").write_text(
                "Package: wb-updater-fixture\n"
                "Version: 2.0-1\n"
                "Architecture: all\n"
                "Maintainer: Wild Buzzard Tests <test@example.invalid>\n"
                "Description: signed updater fixture\n",
                encoding="utf-8",
            )
            deb = repo / "pool/main/w/wb-updater-fixture/wb-updater-fixture_2.0-1_all.deb"
            subprocess.run(
                ["dpkg-deb", "--build", str(package), str(deb)],
                check=True,
                capture_output=True,
            )
            scan = subprocess.run(
                ["dpkg-scanpackages", "pool", "/dev/null"],
                cwd=repo,
                check=True,
                capture_output=True,
            )
            packages_path = repo / "dists/stable/main/binary-amd64/Packages"
            packages_path.write_bytes(scan.stdout)
            with (packages_path.with_suffix(".gz")).open("wb") as destination:
                with gzip.GzipFile(
                    filename="",
                    mode="wb",
                    fileobj=destination,
                    mtime=0,
                ) as compressed:
                    compressed.write(scan.stdout)
            release_root = repo / "dists/stable"
            release_entries = []
            for relative in (
                "main/binary-amd64/Packages",
                "main/binary-amd64/Packages.gz",
            ):
                payload = (release_root / relative).read_bytes()
                release_entries.append(
                    f" {hashlib.sha256(payload).hexdigest()} {len(payload)} {relative}"
                )
            release = release_root / "Release"
            release.write_text(
                "Origin: Wild Buzzard Test\n"
                "Label: Wild Buzzard Test\n"
                "Suite: stable\n"
                "Codename: stable\n"
                "Architectures: amd64\n"
                "Components: main\n"
                f"Date: {formatdate(usegmt=True)}\n"
                "SHA256:\n"
                + "\n".join(release_entries)
                + "\n",
                encoding="utf-8",
            )
            environment = {**os.environ, "GNUPGHOME": str(gnupg)}
            subprocess.run(
                [
                    "gpg",
                    "--batch",
                    "--passphrase",
                    "",
                    "--quick-gen-key",
                    "Wild Buzzard Test <test@example.invalid>",
                    "rsa2048",
                    "sign",
                    "0",
                ],
                check=True,
                capture_output=True,
                env=environment,
            )
            subprocess.run(
                [
                    "gpg",
                    "--batch",
                    "--yes",
                    "--clearsign",
                    "--digest-algo",
                    "SHA256",
                    "--output",
                    str(release_root / "InRelease"),
                    str(release),
                ],
                check=True,
                capture_output=True,
                env=environment,
            )
            exported = subprocess.run(
                ["gpg", "--batch", "--export"],
                check=True,
                capture_output=True,
                env=environment,
            ).stdout
            keyring = root / "etc/apt/keyrings/wildbuzzard-test.gpg"
            keyring.write_bytes(exported)
            (root / "etc/apt/sources.list").write_text(
                f"deb [signed-by={keyring}] file:{repo} stable main\n",
                encoding="utf-8",
            )
            (root / "var/lib/dpkg/status").write_text(
                "Package: wb-updater-fixture\n"
                "Status: install ok installed\n"
                "Priority: optional\n"
                "Section: misc\n"
                "Installed-Size: 1\n"
                "Maintainer: Wild Buzzard Tests <test@example.invalid>\n"
                "Architecture: all\n"
                "Version: 1.0-1\n"
                "Description: installed updater fixture\n",
                encoding="utf-8",
            )
            backend = updater.PythonAptBackend(
                rootdir=str(root),
                lock_timeout_seconds=0.5,
            )
            errors = backend.refresh(lambda *_: None)
            self.assertEqual(errors, [])
            self.assertEqual(
                backend.resolve_plan(),
                (
                    updater.PackageRecord(
                        name="wb-updater-fixture",
                        installed_version="1.0-1",
                        candidate_version="2.0-1",
                        download_size=deb.stat().st_size,
                        security_origin=None,
                        action="upgrade",
                    ),
                ),
            )


if __name__ == "__main__":
    unittest.main()
