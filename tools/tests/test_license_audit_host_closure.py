#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later

from __future__ import annotations

import hashlib
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from tools import license_audit


class AppDirHostClosureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.appdir = Path(self.temporary.name) / "AppDir"
        self.payload_relative = "usr/lib/libexample.so.1"
        self.payload = self.appdir / self.payload_relative
        self.payload.parent.mkdir(parents=True)
        self.payload.write_bytes(b"\x7fELFcross-run-payload")
        self.notice = self.appdir / "usr/share/doc/libexample1/copyright"
        self.notice.parent.mkdir(parents=True)
        self.notice.write_bytes(b"Example package copyright\n")
        self.manifest = self.appdir / license_audit.HOST_CLOSURE_MANIFEST
        self.manifest.parent.mkdir(parents=True, exist_ok=True)
        self.row = (
            self.payload_relative,
            self._sha256(self.payload),
            "libexample1:amd64",
            "1:2.3.4-5ubuntu6.1",
            self._sha256(self.notice),
        )
        self._write_rows([self.row])

    @staticmethod
    def _sha256(path: Path) -> str:
        return hashlib.sha256(path.read_bytes()).hexdigest()

    def _write_rows(self, rows: list[tuple[str, str, str, str, str]]) -> None:
        self.manifest.write_text(
            license_audit.render_host_closure(rows), encoding="utf-8", newline="\n"
        )

    def test_verification_uses_only_appdir_bytes(self) -> None:
        excluded = self.appdir / "usr/bin/buzzardos"
        excluded.parent.mkdir(parents=True)
        excluded.write_bytes(b"\x7fELFexcluded-project-binary")
        guest_runtime = (
            self.appdir
            / "usr/bin/buzzardos-guest-runtime/0.1.0+assets.57/bin/cua-driver"
        )
        guest_runtime.parent.mkdir(parents=True)
        guest_runtime.write_bytes(b"\x7fELFguest-runtime-with-separate-provenance")
        with (
            mock.patch.object(
                license_audit,
                "dpkg_candidates",
                side_effect=AssertionError("must not query the audit runner"),
            ),
            mock.patch.object(
                license_audit,
                "dpkg_versions",
                side_effect=AssertionError("must not query the audit runner"),
            ),
        ):
            issues, payload_count = license_audit.verify_appdir_host_notices(
                self.appdir
            )
        self.assertEqual(issues, [])
        self.assertEqual(payload_count, 1)

    def test_payload_hash_is_bound(self) -> None:
        self.payload.write_bytes(b"\x7fELFtampered-payload")
        issues, payload_count = license_audit.verify_appdir_host_notices(self.appdir)
        self.assertEqual(payload_count, 1)
        self.assertIn(
            f"AppDir host-package payload differs from staged build: {self.payload_relative}",
            issues,
        )

    def test_exact_elf_set_is_required(self) -> None:
        extra = self.appdir / "usr/lib/libunrecorded.so"
        extra.write_bytes(b"\x7fELFunrecorded")
        issues, payload_count = license_audit.verify_appdir_host_notices(self.appdir)
        self.assertEqual(payload_count, 2)
        self.assertIn(
            "AppDir ELF is absent from host-package closure: usr/lib/libunrecorded.so",
            issues,
        )

    def test_embedded_notice_hash_is_bound(self) -> None:
        self.notice.write_bytes(b"different notice\n")
        issues, _ = license_audit.verify_appdir_host_notices(self.appdir)
        self.assertIn(
            "AppDir host-package notice hash differs: "
            "usr/share/doc/libexample1/copyright",
            issues,
        )

    def test_unsafe_payload_path_is_rejected(self) -> None:
        unsafe = ("../escape.so",) + self.row[1:]
        self._write_rows([unsafe])
        with self.assertRaisesRegex(license_audit.AuditError, "unsafe payload path"):
            license_audit.verify_appdir_host_notices(self.appdir)

    def test_duplicate_payload_package_mapping_is_rejected(self) -> None:
        self._write_rows([self.row, self.row])
        with self.assertRaisesRegex(
            license_audit.AuditError, "duplicate payload/package mapping"
        ):
            license_audit.verify_appdir_host_notices(self.appdir)

    def test_noncanonical_row_order_is_rejected(self) -> None:
        later = ("z/liblater.so",) + self.row[1:]
        self._write_rows([later, self.row])
        with self.assertRaisesRegex(license_audit.AuditError, "not canonical"):
            license_audit.verify_appdir_host_notices(self.appdir)

    def test_v1_manifest_is_rejected(self) -> None:
        self.manifest.write_text(
            "# Buzzard OS AppImage build-host package copyright closure v1\n"
            "# appdir_path\tpackage\tversion\tcopyright_sha256\n",
            encoding="utf-8",
        )
        with self.assertRaisesRegex(license_audit.AuditError, "canonical v2"):
            license_audit.verify_appdir_host_notices(self.appdir)


if __name__ == "__main__":
    unittest.main()
