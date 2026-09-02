#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Static gate for Podman-owned creation sources."""

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]


class CreateSourceContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.operations = (
            ROOT / "host/crates/buzzardos/src/operations.rs"
        ).read_text(encoding="utf-8")
        cls.podman = (
            ROOT / "host/crates/wb-core/src/podman.rs"
        ).read_text(encoding="utf-8")

    def test_create_and_pull_use_podmans_native_pull(self) -> None:
        self.assertIn("podman.pull(&arguments.image)?", self.operations)
        self.assertIn("create_from_pull", self.operations)
        self.assertIn("create_from_pull_positional", self.operations)

    def test_import_uses_podmans_supported_transports(self) -> None:
        self.assertIn("import_image(podman, source)?", self.operations)
        self.assertIn('format!("oci:{}", canonical.display())', self.operations)
        self.assertIn("podman.load(&canonical)?", self.operations)

    def test_rootfs_is_materialized_by_podman(self) -> None:
        self.assertIn("podman.materialize_rootfs", self.operations)
        self.assertIn('OsString::from("--rootfs")', self.podman)


if __name__ == "__main__":
    unittest.main()
