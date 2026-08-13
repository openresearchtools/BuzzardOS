#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class HostGlibcCompatibilityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.cargo = (ROOT / "host/Cargo.toml").read_text(encoding="utf-8")
        cls.builder = (ROOT / "host/build-portable-app.sh").read_text(
            encoding="utf-8"
        )
        cls.workflow = (
            ROOT / ".github/workflows/build-release-assets.yml"
        ).read_text(encoding="utf-8")
        display_sources = ROOT / "host/crates/wildbuzzard-display/src"
        cls.display = "\n".join(
            path.read_text(encoding="utf-8")
            for path in sorted(display_sources.glob("*.rs"))
        )

    def test_host_uses_only_gtk_4_14_apis(self) -> None:
        self.assertIn('features = ["v4_14"]', self.cargo)
        self.assertNotIn('features = ["v4_18"]', self.cargo)
        self.assertIn("gtk4 >= 4.14", self.builder)
        self.assertNotIn("gtk4 >= 4.18", self.builder)
        self.assertNotIn("gtk::disable_portals()", self.display)
        self.assertNotIn("set_black_background", self.display)

    def test_builder_and_debian_smoke_images_are_digest_pinned(self) -> None:
        self.assertRegex(
            self.workflow,
            r"FROM ubuntu:24\.04@sha256:[0-9a-f]{64}",
        )
        self.assertNotIn("ubuntu:26.04", self.workflow)
        self.assertRegex(
            self.workflow,
            r"debian:trixie-slim@sha256:[0-9a-f]{64}",
        )
        self.assertIn("ldd -r --", self.workflow)
        self.assertIn("wildbuzzard-display --version", self.workflow)

    def test_complete_appdir_has_a_glibc_2_39_ceiling(self) -> None:
        self.assertRegex(
            self.builder,
            re.compile(
                r"verify-elf-glibc-floor\.py.*?--root \"\$appdir\".*?"
                r"--maximum 2\.39",
                re.DOTALL,
            ),
        )


if __name__ == "__main__":
    unittest.main()
