# SPDX-License-Identifier: AGPL-3.0-or-later
"""Static gate for the canonical create/pull source split."""

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]


class CreateSourceContractTests(unittest.TestCase):
    def test_create_routes_through_the_generic_oci_importer(self) -> None:
        source = (ROOT / "host/crates/buzzardos/src/main.rs").read_text(
            encoding="utf-8"
        )
        create_arm = source.split("Some(Commands::Create", 1)[1].split(
            "Some(Commands::Pull", 1
        )[0]
        self.assertIn("import_machine(", create_arm)
        self.assertIn("source: &image", create_arm)
        self.assertIn("mode: ImportModeArg::Clone", create_arm)
        self.assertNotIn("create(\n", create_arm)


if __name__ == "__main__":
    unittest.main()
