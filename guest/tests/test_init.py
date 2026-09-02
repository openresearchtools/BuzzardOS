#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later

from __future__ import annotations

import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
INIT = ROOT / "guest/assets/buzzardos-init"
DESKTOP_SERVICE = ROOT / "guest/assets/buzzardos-desktop.service"


class GuestInitContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.script = INIT.read_text(encoding="utf-8")

    def test_machine_identity_is_generated_only_when_absent(self) -> None:
        self.assertIn("if [ ! -s /etc/machine-id ]; then", self.script)
        self.assertIn("systemd-machine-id-setup", self.script)

    def test_systemd_is_the_only_final_process(self) -> None:
        lines = [line.strip() for line in self.script.splitlines() if line.strip()]
        self.assertEqual(lines[-1], "exec /lib/systemd/systemd --system")

    def test_init_does_not_construct_a_container_runtime(self) -> None:
        self.assertEqual(self.script.count("exec /lib/systemd/systemd --system"), 1)

    def test_display_loss_does_not_power_off_the_persistent_machine(self) -> None:
        service = DESKTOP_SERVICE.read_text(encoding="utf-8")
        self.assertNotIn("ExecStopPost", service)
        self.assertNotIn("poweroff.target", service)


if __name__ == "__main__":
    unittest.main()
