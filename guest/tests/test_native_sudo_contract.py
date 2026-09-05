#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
ASSETS = ROOT / "guest/assets"


class NativeSudoContractTests(unittest.TestCase):
    def test_guest_handoff_uses_fixed_socket_without_broad_polkit_authorization(self) -> None:
        self.assertFalse((ASSETS / "49-buzzardos-root.rules").exists())
        manifest = (ROOT / "guest/runtime-asset-manifest.tsv").read_text(
            encoding="utf-8"
        )
        self.assertIn("buzzardos-sudo.socket", manifest)
        self.assertIn("buzzardos-sudo@.service", manifest)
        self.assertNotIn("pkexec", manifest)
        self.assertNotIn("systemd-run", manifest)

    def test_native_sudo_policy_requires_the_machine_password(self) -> None:
        sudoers = (ASSETS / "90-buzzardos-user-sudo").read_text(encoding="utf-8")
        self.assertIn("user ALL=(ALL:ALL) ALL", sudoers)
        self.assertNotIn("NOPASSWD", sudoers)

    def test_appimage_mount_handoff_remains_socket_activated(self) -> None:
        manifest = (ROOT / "guest/runtime-asset-manifest.tsv").read_text(
            encoding="utf-8"
        )
        socket = (ASSETS / "buzzardos-fusermount.socket").read_text(
            encoding="utf-8"
        )
        service = (ASSETS / "buzzardos-fusermount@.service").read_text(
            encoding="utf-8"
        )
        self.assertIn("SocketUser=user", socket)
        self.assertIn("SocketGroup=user", socket)
        self.assertIn("SocketMode=0600", socket)
        self.assertIn("Accept=yes", socket)
        self.assertIn("User=root", service)
        self.assertIn("/buzzardos-fusermount-exec --serve", service)
        self.assertIn("assets/buzzardos-fusermount.socket", manifest)
        self.assertIn("assets/buzzardos-fusermount@.service", manifest)

    def test_reference_image_owns_the_documented_initial_credential(self) -> None:
        provision = (ROOT / "oci/desktop/provision-image.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("'user:buzzard' | chpasswd", provision)
        self.assertNotIn("usermod --lock", provision)
        self.assertIn("/usr/local/bin/sudo", provision)
        self.assertIn("/usr/local/bin/sudoedit", provision)

    def test_settings_policy_helper_is_invoked_by_native_sudo(self) -> None:
        ui = (ROOT / "guest/settings/src/ui.rs").read_text(encoding="utf-8")
        self.assertIn('const GUEST_SUDO: &str = "/usr/libexec/buzzardos-guest/sudo";', ui)
        self.assertIn(
            'const SUDO_POLICY_HELPER: &str = "/usr/libexec/buzzardos-guest/sudo-policy";',
            ui,
        )
        manifest = (ROOT / "guest/runtime-asset-manifest.tsv").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("91-buzzardos-passwordless", manifest)


if __name__ == "__main__":
    unittest.main()
