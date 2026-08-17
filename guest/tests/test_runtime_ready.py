#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later

from __future__ import annotations

import hashlib
import importlib.machinery
import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "guest/assets/buzzardos-runtime-ready"


def load_runtime_ready():
    loader = importlib.machinery.SourceFileLoader("buzzardos_runtime_ready", str(SCRIPT))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


class RuntimeReadyTests(unittest.TestCase):
    def setUp(self) -> None:
        self.module = load_runtime_ready()
        self.uid = os.getuid()
        self.gid = os.getgid()

    def test_json_rejects_duplicate_keys_and_fifo_without_blocking(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            duplicate = root / "duplicate.json"
            duplicate.write_text('{"files":{"a":1,"a":2}}', encoding="utf-8")
            duplicate.chmod(0o644)
            with self.assertRaisesRegex(self.module.ReadinessError, "duplicate key"):
                self.module.read_json(duplicate, self.uid, self.gid)

            fifo = root / "metadata.fifo"
            os.mkfifo(fifo, 0o644)
            with self.assertRaisesRegex(self.module.ReadinessError, "metadata is unsafe"):
                self.module.read_json(fifo, self.uid, self.gid)

    def test_hash_rejects_a_file_that_grows_while_being_read(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "runtime-file"
            path.write_bytes(b"protected payload")
            path.chmod(0o644)
            real_read = os.read
            appended = False

            def growing_read(descriptor: int, length: int) -> bytes:
                nonlocal appended
                value = real_read(descriptor, length)
                if value and not appended:
                    appended = True
                    with path.open("ab") as stream:
                        stream.write(b"!")
                return value

            with mock.patch.object(self.module.os, "read", side_effect=growing_read):
                with self.assertRaisesRegex(self.module.ReadinessError, "grew while hashing"):
                    self.module.hash_regular(path, 1024, self.uid, self.gid)

    def test_tree_walk_has_an_independent_entry_budget(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            revision = "test-revision"
            runtime = Path(temporary) / revision
            runtime.mkdir()
            files: dict[str, dict[str, object]] = {}
            for relative in sorted(self.module.REQUIRED_RUNTIME_FILES):
                path = runtime / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(relative.encode("ascii"))
                path.chmod(0o755)
                files[relative] = {
                    "sha256": hashlib.sha256(relative.encode("ascii")).hexdigest(),
                    "mode": 0o755,
                }
            manifest = runtime / "runtime.manifest.json"
            manifest.write_text(
                json.dumps(
                    {"schema_version": 1, "revision": revision, "files": files},
                    sort_keys=True,
                    separators=(",", ":"),
                ),
                encoding="utf-8",
            )
            manifest.chmod(0o644)
            for directory in [runtime, *[path for path in runtime.rglob("*") if path.is_dir()]]:
                directory.chmod(0o755)

            with mock.patch.object(self.module, "MAX_ENTRIES", 1):
                with self.assertRaisesRegex(self.module.ReadinessError, "too many entries"):
                    self.module.validated_manifest(revision, runtime, self.uid, self.gid)

    def test_desktop_marker_is_owner_mode_and_session_bound(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            status = Path(temporary) / "buzzardos-host"
            status.mkdir(mode=self.module.HOST_RUNTIME_MODE)
            status.chmod(self.module.HOST_RUNTIME_MODE)
            marker = status / "desktop-ready"
            token = "0123456789abcdef0123456789abcdef"
            marker.write_text(f"{token}\n", encoding="ascii")
            marker.chmod(0o600)

            self.module.read_desktop_ready(
                marker,
                token,
                self.uid,
                self.gid,
                self.uid,
                self.gid,
            )
            with self.assertRaisesRegex(self.module.ReadinessError, "another session"):
                self.module.read_desktop_ready(
                    marker,
                    "f" * 32,
                    self.uid,
                    self.gid,
                    self.uid,
                    self.gid,
                )
            marker.chmod(0o644)
            with self.assertRaisesRegex(self.module.ReadinessError, "evidence is unsafe"):
                self.module.read_desktop_ready(
                    marker,
                    token,
                    self.uid,
                    self.gid,
                    self.uid,
                    self.gid,
                )

    def test_desktop_marker_rejects_a_traversable_runtime_directory(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            status = Path(temporary) / "buzzardos-host"
            status.mkdir(mode=0o711)
            status.chmod(0o711)
            marker = status / "desktop-ready"
            token = "0123456789abcdef0123456789abcdef"
            marker.write_text(f"{token}\n", encoding="ascii")
            marker.chmod(0o600)

            with self.assertRaisesRegex(
                self.module.ReadinessError, "readiness directory is unsafe"
            ):
                self.module.read_desktop_ready(
                    marker,
                    token,
                    self.uid,
                    self.gid,
                    self.uid,
                    self.gid,
                )


if __name__ == "__main__":
    unittest.main()
