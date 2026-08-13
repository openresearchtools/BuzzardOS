#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
from __future__ import annotations

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "verify_elf_glibc_floor", ROOT / "tools/verify-elf-glibc-floor.py"
)
assert SPEC is not None and SPEC.loader is not None
glibc_floor = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(glibc_floor)


class GlibcFloorTests(unittest.TestCase):
    def test_parser_compares_numeric_components(self) -> None:
        versions = glibc_floor.glibc_versions(
            "Name: GLIBC_2.9\nName: GLIBC_2.31\nName: GLIBC_PRIVATE\n"
        )
        self.assertEqual(max(versions), (2, 31))
        self.assertLess(glibc_floor.parse_version("2.9"), glibc_floor.parse_version("2.31"))

    def test_verifier_checks_every_regular_elf_recursively(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "bin").mkdir()
            (root / "lib/plugins").mkdir(parents=True)
            (root / "bin/tool").write_bytes(b"\x7fELFtool")
            (root / "lib/library.so").write_bytes(b"\x7fELFlibrary")
            (root / "lib/plugins/plugin.so").write_bytes(b"\x7fELFplugin")
            (root / "README").write_text("not ELF\n", encoding="utf-8")
            os.symlink("library.so", root / "lib/library.so.1")

            requirements = {
                "bin/tool": "Name: GLIBC_2.31\n",
                "lib/library.so": "Name: GLIBC_2.17\n",
                "lib/plugins/plugin.so": "Name: GLIBC_ABI_DT_RELR\nName: GLIBC_2.43\n",
            }

            def fake_version_info(path: Path, _readelf: str) -> str:
                return requirements[path.relative_to(root).as_posix()]

            with mock.patch.object(glibc_floor, "version_info", side_effect=fake_version_info):
                count, failures = glibc_floor.verify(root, (2, 31), "readelf")

            self.assertEqual(count, 3)
            self.assertEqual(
                failures, ["lib/plugins/plugin.so: requires GLIBC_ABI_DT_RELR"]
            )

    def test_readelf_failure_is_not_silently_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "object").write_bytes(b"\x7fELFbroken")
            result = mock.Mock(returncode=1, stdout="", stderr="bad ELF")
            with mock.patch.object(glibc_floor.subprocess, "run", return_value=result):
                with self.assertRaisesRegex(glibc_floor.VerificationError, "bad ELF"):
                    glibc_floor.verify(root, (2, 31), "readelf")

    def test_dt_relr_abi_is_accepted_at_its_glibc_floor(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "object").write_bytes(b"\x7fELFobject")
            with mock.patch.object(
                glibc_floor,
                "version_info",
                return_value="Name: GLIBC_ABI_DT_RELR\nName: GLIBC_2.36\n",
            ):
                count, failures = glibc_floor.verify(root, (2, 39), "readelf")
            self.assertEqual(count, 1)
            self.assertEqual(failures, [])


if __name__ == "__main__":
    unittest.main()
