# SPDX-License-Identifier: AGPL-3.0-or-later
"""Static contracts for destructive desktop lifecycle acceptance actions."""

from pathlib import Path
import re
import unittest


PROJECT_DIR = Path(__file__).resolve().parents[2]
HARDWARE_ACCEPTANCE = PROJECT_DIR / "tests/acceptance/hardware-acceptance.sh"
SWAY_CONFIG = PROJECT_DIR / "guest/assets/sway-config"


class HardwareAcceptanceContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.script = HARDWARE_ACCEPTANCE.read_text(encoding="utf-8")
        self.sway_config = SWAY_CONFIG.read_text(encoding="utf-8")

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

    def test_guest_json_is_parsed_by_host_jq(self) -> None:
        self.assertNotRegex(self.script, r"\bguest\s+jq\b")

    def test_portable_relocation_waits_for_both_appdir_leases(self) -> None:
        relocation = self.script.split(
            "# Move the complete stopped portable folder", maxsplit=1
        )[1].split(
            "# `stop` must not return while its detached broker", maxsplit=1
        )[0]
        self.assertEqual(relocation.count('wb window "$machine" close'), 2)
        self.assertEqual(relocation.count("wait_appdir_lease_released"), 2)
        self.assertIn(
            """relocation_outbound_broker_pid=$(jq -er '.launcher_pid' \"$runtime\")""",
            relocation,
        )
        self.assertIn(
            """relocation_return_broker_pid=$(jq -er '.launcher_pid' \"$runtime\")""",
            relocation,
        )
        outbound_close = relocation.index('wb window "$machine" close')
        outbound_wait = relocation.index("wait_appdir_lease_released")
        outbound_move = relocation.index('mv -- "$relocation_original" "$relocation_target"')
        self.assertLess(outbound_close, outbound_wait)
        self.assertLess(outbound_wait, outbound_move)
        return_close = relocation.index(
            'wb window "$machine" close', outbound_close + 1
        )
        return_wait = relocation.index(
            "wait_appdir_lease_released", outbound_wait + 1
        )
        return_move = relocation.index('mv -- "$relocation_target" "$relocation_original"')
        self.assertLess(return_close, return_wait)
        self.assertLess(return_wait, return_move)

    def test_fractional_scale_override_uses_fresh_display_lifecycles(self) -> None:
        fractional = self.script.split(
            "# Exercise the native fractional-scale bridge", maxsplit=1
        )[1].split(
            'rm -f -- "$portable_dir/shared/.wildbuzzard-acceptance"', maxsplit=1
        )[0]
        self.assertNotIn('wb stop "$machine"', fractional)
        self.assertEqual(fractional.count('wb window "$machine" close'), 2)
        self.assertEqual(fractional.count("wait_stopped"), 2)
        self.assertEqual(fractional.count("wait_appdir_lease_released"), 2)
        self.assertEqual(fractional.count("wait_scaled_window_frame 180"), 3)

        ordered_fragments = (
            "fractional_baseline_broker_pid=$(jq -er '.launcher_pid' \"$runtime\")",
            'process_start_time "$fractional_baseline_broker_pid"',
            'appdir_for_process "$fractional_baseline_broker_pid"',
            'wb window "$machine" close',
            "wait_stopped",
            "wait_appdir_lease_released \\\n"
            '    "$fractional_baseline_broker_pid" \\\n'
            '    "$fractional_baseline_broker_start_time" \\\n'
            '    "$fractional_baseline_appdir"',
            "WILDBUZZARD_TEST_FRACTIONAL_SCALE_120=180",
            "fractional_override_broker_pid=$(jq -er '.launcher_pid' \"$runtime\")",
            'process_start_time "$fractional_override_broker_pid"',
            'appdir_for_process "$fractional_override_broker_pid"',
            "wait_scaled_window_frame 180",
            "wait_scaled_window_frame 180",
            "wait_scaled_window_frame 180",
            'wb window "$machine" close',
            "wait_stopped",
            "wait_appdir_lease_released \\\n"
            '    "$fractional_override_broker_pid" \\\n'
            '    "$fractional_override_broker_start_time" \\\n'
            '    "$fractional_override_appdir"',
            'wb start "$machine" --detach',
        )
        cursor = 0
        for fragment in ordered_fragments:
            cursor = fractional.index(fragment, cursor) + len(fragment)

    def test_titlebar_drag_uses_sway_decoration_and_output_geometry(self) -> None:
        self.assertIn("titlebar_drag_for_pid()", self.script)
        self.assertGreaterEqual(self.script.count("titlebar_drag_for_pid \"$"), 2)
        self.assertIn("($state.decoration) as $decoration", self.script)
        self.assertIn("($state.border_width) as $border", self.script)
        self.assertIn("$output.guest_logical_width", self.script)
        self.assertIn("$output.physical_width", self.script)
        self.assertNotIn("from_y: (.y + 10)", self.script)

    def test_xwayland_border_assertion_matches_managed_sway_config(self) -> None:
        configured_widths = re.findall(
            r"(?m)^\s*for_window\s+\[all\]\s+floating\s+enable,\s*"
            r"border\s+normal\s+(\d+)\s*$",
            self.sway_config,
        )
        self.assertEqual(len(configured_widths), 1)
        configured_width = configured_widths[0]

        xeyes_start = self.script.index("xeyes=$(wait_for_window xeyes)")
        xeyes_end = self.script.index("xeyes_before_drag=", xeyes_start)
        xeyes_assertion = self.script[xeyes_start:xeyes_end]
        asserted_widths = re.findall(
            r"\.border_width\s*==\s*(\d+)", xeyes_assertion
        )
        self.assertEqual(asserted_widths, [configured_width])


if __name__ == "__main__":
    unittest.main()
