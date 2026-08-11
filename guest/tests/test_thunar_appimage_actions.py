#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later

import unittest
import xml.etree.ElementTree as ET
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class ThunarAppImageActionContractTests(unittest.TestCase):
    def test_actions_use_only_the_fixed_single_path_helper_abi(self) -> None:
        asset = ROOT / "guest/assets/thunar-uca.xml"
        root = ET.fromstring(asset.read_text(encoding="utf-8"))
        self.assertEqual(root.tag, "actions")
        actions = list(root.findall("action"))
        self.assertEqual(len(actions), 2)
        expected = {
            "Add to Applications": "register-applications",
            "Add Desktop Shortcut": "register-desktop",
        }
        for action in actions:
            name = action.findtext("name")
            command = action.findtext("command") or ""
            self.assertIn(name, expected)
            self.assertEqual(
                command,
                "/usr/libexec/wildbuzzard-shortcut-helper "
                f"{expected[name]} %f",
            )
            self.assertEqual(action.findtext("range"), "1-1")
            self.assertEqual(
                action.findtext("patterns"), "*.AppImage;*.appimage"
            )
            self.assertIsNotNone(action.find("other-files"))
            for forbidden in ("sh -c", "%F", "`", "$("):
                self.assertNotIn(forbidden, command)

    def test_asset_and_fail_open_session_migration_are_wired(self) -> None:
        manifest = (ROOT / "guest/asset-manifest.tsv").read_text(encoding="utf-8")
        self.assertIn(
            "0644\tassets/thunar-uca.xml\tetc/wildbuzzard/xdg/Thunar/uca.xml",
            manifest,
        )
        session = (ROOT / "guest/assets/wildbuzzard-session").read_text(
            encoding="utf-8"
        )
        invocation = (
            "if ! /usr/libexec/wildbuzzard-shortcut-helper "
            "install-thunar-actions >/dev/null; then"
        )
        self.assertIn(invocation, session)
        self.assertIn("without preventing the desktop from booting", session)


if __name__ == "__main__":
    unittest.main()
