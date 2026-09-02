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

    def test_manager_folder_dialog_guarantees_folder_creation(self) -> None:
        self.assertIn("gtk::FileChooserWidget::new", self.display)
        self.assertIn("chooser.set_create_folders(true)", self.display)
        self.assertIn('gtk::Button::with_label("New Folder…")', self.display)
        self.assertNotIn("FileChooserNative", self.display)

    def test_package_build_stays_on_oldest_host_and_install_smokes_all_hosts(self) -> None:
        self.assertIn("runs-on: ubuntu-24.04", self.workflow)
        matrix = (ROOT / "tools/test-host-package-matrix.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("docker.io/library/ubuntu:24.04", matrix)
        self.assertIn("docker.io/library/debian:13", matrix)
        self.assertIn("docker.io/library/ubuntu:26.04", matrix)
        self.assertIn("buzzardos --version", matrix)
        self.assertIn("podman --version", matrix)
        self.assertIn("test-host-package-matrix.sh", self.workflow)

    def test_host_package_uses_normal_debian_paths(self) -> None:
        self.assertIn('"$root/usr/bin/buzzardos"', self.packager)
        self.assertIn('"$root/usr/libexec/buzzardos/buzzardos-display"', self.packager)
        self.assertNotIn("AppRun", self.packager)

    def test_host_package_uses_distro_podman_without_copied_helpers(self) -> None:
        self.assertIn("podman", self.packager)
        self.assertIn("buildah", self.packager)
        self.assertIn("passt", self.packager)


if __name__ == "__main__":
    unittest.main()
