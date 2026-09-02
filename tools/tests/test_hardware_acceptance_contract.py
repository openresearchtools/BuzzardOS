#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later

from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
HARDWARE = ROOT / "tests/acceptance/hardware-acceptance.sh"
INTEGRATIONS = ROOT / "tests/acceptance/integration-acceptance.sh"
CUA = ROOT / "tests/acceptance/guest-cua.sh"


class PodmanAcceptanceContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.hardware = HARDWARE.read_text(encoding="utf-8")
        cls.integrations = INTEGRATIONS.read_text(encoding="utf-8")
        cls.cua = CUA.read_text(encoding="utf-8")

    def test_all_guest_entry_uses_native_podman_exec(self) -> None:
        for script in (self.hardware, self.integrations, self.cua):
            self.assertIn("podman exec", script)

    def test_unchanged_restart_requires_persistent_container_identity(self) -> None:
        for script in (self.hardware, self.integrations):
            self.assertIn("wb restart", script)
            self.assertIn("container_id", script)
            self.assertIn("inspect --format '{{.Id}}'", script)

    def test_hardware_journey_covers_native_sudo_and_portability(self) -> None:
        for required in (
            "/usr/bin/sudo",
            "apt-get -o Dpkg::Use-Pty=0 update",
            "install --yes",
            "wb export",
            " clone ",
            " import ",
            "--mode clone",
            "get_desktop_state",
        ):
            self.assertIn(required, self.hardware)

    def test_integration_journey_checks_podman_owned_ports_and_labels(self) -> None:
        self.assertIn("podman port", self.integrations)
        self.assertIn("org.openresearchtools.buzzardos.managed", self.integrations)
        self.assertIn("org.openresearchtools.buzzardos.machine-id", self.integrations)


if __name__ == "__main__":
    unittest.main()
