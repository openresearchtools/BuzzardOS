# SPDX-License-Identifier: AGPL-3.0-or-later
"""Static contracts for destructive desktop lifecycle acceptance actions."""

from pathlib import Path
import unittest


PROJECT_DIR = Path(__file__).resolve().parents[2]
HARDWARE_ACCEPTANCE = PROJECT_DIR / "tests/acceptance/hardware-acceptance.sh"


class HardwareAcceptanceContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.script = HARDWARE_ACCEPTANCE.read_text(encoding="utf-8")

    def test_sway_reload_uses_ipc_without_terminating_the_compositor(self) -> None:
        self.assertNotIn('guest kill -HUP "$compositor_pid"', self.script)
        self.assertIn("reload_result=$(guest swaymsg -r reload)", self.script)
        self.assertIn(
            'wait_sway_config_contains "# persistent guest OS edit: $marker"',
            self.script,
        )

    def test_reload_proves_process_output_and_cua_continuity(self) -> None:
        self.assertGreaterEqual(
            self.script.count('[[ $(guest pgrep -xo sway) == "$compositor_pid" ]]'),
            2,
        )
        self.assertIn(
            'wait_native_window_frame_after "$reload_frame_counters"', self.script
        )
        self.assertIn("wait_sway_output_matches_runtime", self.script)
        self.assertIn("wait_cua_capture_matches_runtime", self.script)


if __name__ == "__main__":
    unittest.main()
