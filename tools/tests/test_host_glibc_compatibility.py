#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later

import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class HostGlibcCompatibilityTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.cargo = (ROOT / "host/Cargo.toml").read_text(encoding="utf-8")
        cls.packager = (ROOT / "packaging/build-debs.sh").read_text(
            encoding="utf-8"
        )
        cls.workflow = (
            ROOT / ".github/workflows/build-release-assets.yml"
        ).read_text(encoding="utf-8")
        display_sources = ROOT / "host/crates/buzzardos-display/src"
        cls.display = "\n".join(
            path.read_text(encoding="utf-8")
            for path in sorted(display_sources.glob("*.rs"))
        )

    def test_host_uses_only_gtk_4_14_apis(self) -> None:
        self.assertIn('features = ["v4_14"]', self.cargo)
        self.assertNotIn('features = ["v4_18"]', self.cargo)
        self.assertIn("libgtk-4-1 (>= 4.14)", self.packager)
        self.assertNotIn("gtk::disable_portals()", self.display)
        self.assertNotIn("set_black_background", self.display)

    def test_package_build_and_install_smoke_use_ubuntu_24(self) -> None:
        self.assertIn("runs-on: ubuntu-24.04", self.workflow)
        self.assertNotIn("ubuntu:26.04", self.workflow)
        self.assertIn("Install-smoke the host package on Ubuntu 24.04", self.workflow)
        self.assertIn("buzzardos --version", self.workflow)

    def test_host_package_uses_normal_debian_paths(self) -> None:
        self.assertIn('"$root/usr/bin/buzzardos"', self.packager)
        self.assertIn('"$root/usr/libexec/buzzardos/buzzardos-broker"', self.packager)
        self.assertIn('"$root/usr/libexec/buzzardos/buzzardos-display"', self.packager)
        self.assertNotIn("AppRun", self.packager)


if __name__ == "__main__":
    unittest.main()
