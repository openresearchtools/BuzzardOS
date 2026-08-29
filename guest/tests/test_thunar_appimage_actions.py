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
        self.assertEqual(len(actions), 5)
        expected = {
            "Run AppImage": "run-path",
            "Extract and Run AppImage (Persistent)": "extract-and-run",
            "Extract and Run --no-sandbox": "extract-and-run-no-sandbox",
            "Add AppImage to Applications": "register-applications",
            "Add AppImage to Desktop": "register-desktop",
        }
        for action in actions:
            name = action.findtext("name")
            command = action.findtext("command") or ""
            self.assertIn(name, expected)
            self.assertEqual(
                command,
                "/usr/libexec/buzzardos-shortcut-helper "
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
        manifest = (ROOT / "guest/desktop-asset-manifest.tsv").read_text(encoding="utf-8")
        self.assertIn(
            "0644\tassets/thunar-uca.xml\tetc/buzzardos/xdg/Thunar/uca.xml",
            manifest,
        )
        provision = (ROOT / "oci/desktop/provision-image.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "/usr/libexec/buzzardos-shortcut-helper install-thunar-actions",
            provision,
        )

    def test_thunar_places_use_standard_user_dirs_and_buzzard_icons(self) -> None:
        manifest = (ROOT / "guest/desktop-asset-manifest.tsv").read_text(
            encoding="utf-8"
        )
        for icon in (
            "user-home",
            "user-desktop",
            "folder-documents",
            "folder-download",
            "folder-music",
            "folder-pictures",
            "folder-publicshare",
            "folder-templates",
            "folder-videos",
        ):
            mapping = (
                f"assets/icons/BuzzardOS/scalable/places/{icon}.svg\t"
                f"usr/share/icons/BuzzardOS/scalable/places/{icon}.svg"
            )
            self.assertIn(mapping, manifest)

        provision = (ROOT / "oci/desktop/provision-image.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("/usr/bin/xdg-user-dirs-update", provision)
        self.assertIn("file:///home/buzzard/Documents Documents", provision)
        self.assertIn("file:///home/buzzard/Downloads Downloads", provision)
        for unwanted in ("Music Music", "Pictures Pictures", "Videos Videos"):
            self.assertNotIn(unwanted, provision)

        guest_session = (ROOT / "guest/assets/buzzardos-session").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("xdg-user-dirs-update", guest_session)
        self.assertNotIn("install-thunar-actions", guest_session)
        self.assertNotIn("install -d", guest_session)

        shell = (ROOT / "guest/shell/src/main.rs").read_text(encoding="utf-8")
        self.assertNotIn("xdg-user-dirs-update", shell)
        self.assertNotIn("install_thunar_actions", shell)

        packaging = (ROOT / "packaging/build-debs.sh").read_text(encoding="utf-8")
        self.assertIn("thunar, xdg-user-dirs, xdg-utils", packaging)


if __name__ == "__main__":
    unittest.main()
