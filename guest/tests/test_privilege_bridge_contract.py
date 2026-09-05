#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
ASSETS = ROOT / "guest/assets"


class PrivilegeBridgeContractTests(unittest.TestCase):
    def test_guest_has_no_broad_polkit_authorization(self) -> None:
        self.assertFalse((ASSETS / "49-buzzardos-root.rules").exists())
        payload = "\n".join(
            path.read_text(encoding="utf-8")
            for path in (
                ASSETS / "buzzardos-fusermount",
                ASSETS / "buzzardos-fusermount-exec",
                ASSETS / "buzzardos-sudo.socket",
                ASSETS / "buzzardos-sudo@.service",
                ASSETS / "buzzardos-fusermount.socket",
                ASSETS / "buzzardos-fusermount@.service",
            )
        )
        self.assertNotIn("pkexec", payload)
        self.assertNotIn("systemd-run", payload)
        self.assertNotIn("org.freedesktop.systemd1.manage-units", payload)

    def test_sudo_policy_requires_the_machine_password(self) -> None:
        sudoers = (ASSETS / "90-buzzardos-user-sudo").read_text(encoding="utf-8")
        self.assertIn("user ALL=(ALL:ALL) ALL", sudoers)
        self.assertNotIn("NOPASSWD", sudoers)

    def test_services_are_fixed_private_socket_activations(self) -> None:
        manifest = (ROOT / "guest/runtime-asset-manifest.tsv").read_text(
            encoding="utf-8"
        )
        for name, executable in (
            ("buzzardos-sudo", "buzzardos-sudo-exec --serve"),
            ("buzzardos-fusermount", "buzzardos-fusermount-exec --serve"),
        ):
            socket = (ASSETS / f"{name}.socket").read_text(encoding="utf-8")
            service = (ASSETS / f"{name}@.service").read_text(encoding="utf-8")
            self.assertIn("SocketUser=user", socket)
            self.assertIn("SocketGroup=user", socket)
            self.assertIn("SocketMode=0600", socket)
            self.assertIn("Accept=yes", socket)
            self.assertIn("User=root", service)
            self.assertIn(f"/{executable}", service)
            self.assertIn(f"assets/{name}.socket", manifest)
            self.assertIn(f"assets/{name}@.service", manifest)

    def test_reference_image_owns_the_documented_initial_credential(self) -> None:
        provision = (ROOT / "oci/desktop/provision-image.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("'user:buzzard' | chpasswd", provision)
        self.assertNotIn("usermod --lock", provision)
        self.assertIn("/usr/local/bin/sudo", provision)
        self.assertIn("/usr/local/bin/sudoedit", provision)

    def test_passwordless_policy_is_not_present_in_the_image_by_default(self) -> None:
        manifest = (ROOT / "guest/runtime-asset-manifest.tsv").read_text(
            encoding="utf-8"
        )
        self.assertNotIn("91-buzzardos-passwordless", manifest)
        installer = (ROOT / "guest/install-rootfs-assets.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("/usr/libexec/buzzardos-guest/sudo-policy", installer)


if __name__ == "__main__":
    unittest.main()
