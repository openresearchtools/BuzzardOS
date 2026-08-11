#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later

from __future__ import annotations

import hashlib
import shutil
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from tools import license_audit


REQUIRED_XKB_FILES = (
    "compat/complete",
    "keycodes/evdev",
    "rules/evdev",
    "rules/evdev.lst",
    "symbols/us",
    "types/complete",
)


def write_tree(root: Path) -> str:
    rows = []
    for relative in REQUIRED_XKB_FILES:
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        payload = f"fixture:{relative}\n".encode()
        path.write_bytes(payload)
        rows.append((relative, hashlib.sha256(payload).hexdigest()))
    return "".join(
        f"{digest}  {relative}\n" for relative, digest in sorted(rows)
    )


class AppdirXkbPayloadTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.appdir = Path(self.temporary.name) / "AppDir"
        self.host_root = self.appdir / "usr/share/wildbuzzard/xkb"
        manifest = write_tree(self.host_root)
        host_metadata = self.appdir / "usr/share/wildbuzzard"
        (host_metadata / "xkb-data.manifest.sha256").write_text(
            manifest, encoding="utf-8", newline="\n"
        )
        (host_metadata / "xkb-data.version").write_text(
            "2.47-1\n", encoding="utf-8", newline="\n"
        )
        host_notice = self.appdir / "usr/share/doc/xkb-data/copyright"
        host_notice.parent.mkdir(parents=True)
        host_notice.write_text("fixture notice\n", encoding="utf-8")

        self.guest_revision = (
            self.appdir / "usr/bin/wildbuzzard-guest-runtime/revision-1"
        )
        guest_root = self.guest_revision / "share/X11/xkb"
        guest_root.parent.mkdir(parents=True)
        shutil.copytree(self.host_root, guest_root)
        guest_metadata = self.guest_revision / "share/wildbuzzard"
        guest_metadata.mkdir(parents=True)
        shutil.copy2(
            host_metadata / "xkb-data.manifest.sha256",
            guest_metadata / "xkb-data.manifest.sha256",
        )
        shutil.copy2(
            host_metadata / "xkb-data.version",
            guest_metadata / "xkb-data.version",
        )
        guest_notice = self.guest_revision / "share/doc/xkb-data/copyright"
        guest_notice.parent.mkdir(parents=True)
        shutil.copy2(host_notice, guest_notice)

        host_library = self.appdir / "usr/lib/libxkbcommon.so.0"
        host_library.parent.mkdir(parents=True)
        host_library.write_bytes(b"ELF fixture")
        library_digest = hashlib.sha256(host_library.read_bytes()).hexdigest()
        (host_metadata / "libxkbcommon0.manifest.sha256").write_text(
            f"{library_digest}  lib/libxkbcommon.so.0\n", encoding="utf-8"
        )
        (host_metadata / "libxkbcommon0.version").write_text(
            "1.11.0-1\n", encoding="utf-8"
        )
        host_library_notice = self.appdir / "usr/share/doc/libxkbcommon0/copyright"
        host_library_notice.parent.mkdir(parents=True)
        host_library_notice.write_text("library notice\n", encoding="utf-8")
        guest_library = self.guest_revision / "lib/libxkbcommon.so.0"
        guest_library.parent.mkdir(parents=True)
        shutil.copy2(host_library, guest_library)
        shutil.copy2(
            host_metadata / "libxkbcommon0.manifest.sha256",
            guest_metadata / "libxkbcommon0.manifest.sha256",
        )
        shutil.copy2(
            host_metadata / "libxkbcommon0.version",
            guest_metadata / "libxkbcommon0.version",
        )
        guest_library_notice = (
            self.guest_revision / "share/doc/libxkbcommon0/copyright"
        )
        guest_library_notice.parent.mkdir(parents=True)
        shutil.copy2(host_library_notice, guest_library_notice)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def audit(self) -> list[str]:
        completed = mock.Mock(
            returncode=0,
            stdout="Library soname: [libxkbcommon.so.0]\n",
            stderr="",
        )
        with mock.patch.object(license_audit.subprocess, "run", return_value=completed):
            return license_audit.audit_appdir_xkb_payload(self.appdir)

    def test_exact_host_and_guest_payloads_pass(self) -> None:
        self.assertEqual(self.audit(), [])

    def test_host_tree_mutation_is_rejected(self) -> None:
        (self.host_root / "symbols/us").write_text("changed\n", encoding="utf-8")
        issues = self.audit()
        self.assertTrue(any("differs from its manifest" in issue for issue in issues))
        self.assertTrue(any("host and guest XKB payloads differ" in issue for issue in issues))

    def test_symlink_in_tree_is_rejected(self) -> None:
        (self.host_root / "symbols/link").symlink_to("us")
        issues = self.audit()
        self.assertTrue(any("contains a symlink" in issue for issue in issues))

    def test_host_library_mutation_is_rejected(self) -> None:
        (self.appdir / "usr/lib/libxkbcommon.so.0").write_bytes(b"changed")
        issues = self.audit()
        self.assertTrue(
            any("libxkbcommon differs from its manifest" in issue for issue in issues)
        )
        self.assertTrue(
            any("host and guest libxkbcommon payloads differ" in issue for issue in issues)
        )

    def test_unresolved_relocation_closure_is_rejected(self) -> None:
        calls = []

        def run(command, **kwargs):
            calls.append((command, kwargs))
            if command[0] == "readelf":
                return mock.Mock(
                    returncode=0,
                    stdout="Library soname: [libxkbcommon.so.0]\n",
                    stderr="",
                )
            return mock.Mock(
                returncode=0,
                stdout="libfixture.so.1 => not found\n",
                stderr="undefined symbol: fixture_symbol\n",
            )

        with mock.patch.object(license_audit.subprocess, "run", side_effect=run):
            issues = license_audit.audit_appdir_xkb_payload(self.appdir)
        self.assertTrue(
            any("host libxkbcommon has an incomplete relocation closure" in issue for issue in issues)
        )
        self.assertTrue(
            any("guest libxkbcommon has an incomplete relocation closure" in issue for issue in issues)
        )
        ldd_calls = [call for call in calls if call[0][0:3] == ["ldd", "-r", "--"]]
        self.assertEqual(len(ldd_calls), 2)
        for command, kwargs in ldd_calls:
            self.assertEqual(kwargs["env"]["LD_LIBRARY_PATH"], str(Path(command[-1]).parent))

    def test_unsafe_manifest_path_is_never_followed(self) -> None:
        manifest = self.appdir / "usr/share/wildbuzzard/xkb-data.manifest.sha256"
        manifest.write_text(f"{'0' * 64}  ../../escape\n", encoding="utf-8")
        issues = self.audit()
        self.assertTrue(any("invalid row" in issue for issue in issues))


if __name__ == "__main__":
    unittest.main()
