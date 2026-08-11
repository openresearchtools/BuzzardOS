# SPDX-License-Identifier: AGPL-3.0-or-later
"""Static contracts for destructive desktop lifecycle acceptance actions."""

from pathlib import Path
import re
import unittest


PROJECT_DIR = Path(__file__).resolve().parents[2]
HARDWARE_ACCEPTANCE = PROJECT_DIR / "tests/acceptance/hardware-acceptance.sh"
SWAY_CONFIG = PROJECT_DIR / "guest/assets/sway-config"
CUA_VIRTUAL_KEYBOARD = (
    PROJECT_DIR
    / "guest/third_party/trycua-cua/cua-driver/rust/crates/platform-linux/src"
    / "wayland/virtual_keyboard.rs"
)
GNOME_HOST_KEYBOARD = (
    PROJECT_DIR / "tests/acceptance/gnome-host-keyboard-input.py"
)


class HardwareAcceptanceContractTests(unittest.TestCase):
    def setUp(self) -> None:
        self.script = HARDWARE_ACCEPTANCE.read_text(encoding="utf-8")
        self.sway_config = SWAY_CONFIG.read_text(encoding="utf-8")
        self.cua_keyboard = CUA_VIRTUAL_KEYBOARD.read_text(encoding="utf-8")
        self.gnome_host_keyboard = GNOME_HOST_KEYBOARD.read_text(encoding="utf-8")

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
        self.assertIn("$output.logical_width", self.script)
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

    def test_cua_keyboard_handoff_acceptance_is_ordered_and_observable(self) -> None:
        coexistence = self.script.split(
            "# Verifiable CUA/human-keyboard coexistence", maxsplit=1
        )[1].split(
            "# Classic state changes remain compositor-owned", maxsplit=1
        )[0]
        ordered_fragments = (
            "assert_cua_ok start_session",
            "assert_cua_ok hotkey",
            r'[\"ctrl\",\"l\"]',
            "assert_cua_ok type_text",
            "host_keyboard_input z",
            'expected_cua_host_value="${marker%?}z"',
            'cat /home/wildbuzzard/.wildbuzzard-cua-input) == "$expected_cua_host_value"',
            "assert_cua_ok press_key",
            r'\"key\":\"backspace\"',
            "assert_cua_ok type_text",
            r'\"text\":\"z\"',
            "assert_cua_ok press_key",
            r'\"key\":\"enter\"',
            "cat /home/wildbuzzard/.wildbuzzard-peer-while-cua-input) == az",
            "assert_cua_ok end_session",
            "host_keyboard_input q",
            "cat /home/wildbuzzard/.wildbuzzard-peer-after-cua-input) == xq",
        )
        cursor = 0
        for fragment in ordered_fragments:
            cursor = coexistence.index(fragment, cursor) + len(fragment)

        self.assertIn('"IFS= read -e -r -i ab coexist_value"', coexistence)
        self.assertIn('"IFS= read -e -r -i xy teardown_value"', coexistence)
        first_type = coexistence.index("assert_cua_ok type_text")
        host_input = coexistence.index("host_keyboard_input z", first_type)
        next_cua_key = coexistence.index("assert_cua_ok press_key", first_type)
        self.assertLess(first_type, host_input)
        self.assertLess(host_input, next_cua_key)
        self.assertLess(host_input, coexistence.index("assert_cua_ok end_session"))
        self.assertNotIn("guest wtype", coexistence)
        self.assertIn('wb window "$machine" focus-monitor', self.script)
        self.assertIn("gnome-host-keyboard-input.py", self.script)
        self.assertIn("WILDBUZZARD_ACCEPT_HOST_INPUT_HOOK", self.script)

    def test_cua_session_end_waits_for_a_neutral_keyboard_acknowledgement(self) -> None:
        self.assertIn("register_session_end_hook", self.cua_keyboard)
        self.assertIn(
            "send_timeout(Cmd::Reset { reply }, remaining)", self.cua_keyboard
        )
        self.assertIn(
            "receive.recv_timeout(DEADLINE.saturating_sub(started.elapsed()))",
            self.cua_keyboard,
        )
        self.assertIn("self.keyboard.modifiers(0, 0, 0, 0)", self.cua_keyboard)
        self.assertIn("self.pressed.cleanup_transitions()", self.cua_keyboard)
        self.assertIn("same_client_neutral", self.cua_keyboard)
        self.assertIn("session.restore_fixed_neutral().is_ok()", self.cua_keyboard)
        self.assertIn("cancelled_teardown_unproven = true", self.cua_keyboard)
        self.assertIn(
            "cancelled CUA keyboard could not prove same-client neutral delivery",
            self.cua_keyboard,
        )
        restore = self.cua_keyboard.split(
            "fn restore_fixed_neutral(&mut self)", maxsplit=1
        )[1].split("fn emit(", maxsplit=1)[0]
        self.assertIn("self.reset(None)?", restore)
        self.assertIn("self.install_keymap(XKB_KEYMAP, None)?", restore)
        self.assertNotIn("active_keymap != XKB_KEYMAP", restore)
        self.assertIn(
            "ctrl_l_text_enter_then_parent_backspace_starts_from_neutral_state",
            self.cua_keyboard,
        )
        self.assertIn(
            "interrupted_ctrl_l_reset_then_parent_backspace_is_neutral",
            self.cua_keyboard,
        )
        self.assertIn(
            "session_end_reset_then_parent_backspace_is_neutral",
            self.cua_keyboard,
        )
        self.assertIn(
            "disconnected_after_key_down_uses_same_device_destroy_releases_only",
            self.cua_keyboard,
        )
        self.assertNotIn("repair_transitions", self.cua_keyboard)
        self.assertIn("wlr_keyboard_finish emits releases", self.cua_keyboard)
        self.assertIn("SHUTDOWN_EPOCH.fetch_add", self.cua_keyboard)
        self.assertIn("cancellable_delay(delay_ms, admission)", self.cua_keyboard)
        hook = self.cua_keyboard.split("register_session_end_hook", 1)[1].split(
            "owner_thread", 1
        )[0]
        self.assertNotIn("OPERATION_LOCK.try_lock", hook)
        self.assertGreaterEqual(self.cua_keyboard.count("keycode: 14"), 2)

    def test_active_cua_typing_is_cancelled_before_real_host_input(self) -> None:
        coexistence = self.script.split(
            "# Verifiable CUA/human-keyboard coexistence", maxsplit=1
        )[1].split(
            "# Classic state changes remain compositor-owned", maxsplit=1
        )[0]
        ordered = (
            "active_cua_session=",
            "guest cua-driver type_text",
            ".wildbuzzard-active-cua-progress",
            'assert_cua_ok end_session "{\\"session\\":\\"$active_cua_session\\"}"',
            "host_keyboard_input z",
            "cancelled CUA type_text did not return promptly",
            'contains("cancel")',
            ".wildbuzzard-active-cua-late",
        )
        cursor = 0
        for fragment in ordered:
            cursor = coexistence.index(fragment, cursor) + len(fragment)
        self.assertIn("[[ $active_cua_value =~ ^k*z$ ]]", coexistence)
        self.assertIn(
            "guest test ! -s /home/wildbuzzard/.wildbuzzard-active-cua-late",
            coexistence,
        )

    def test_supported_sway_text_does_not_spawn_one_shot_wtype(self) -> None:
        wayland = (
            PROJECT_DIR
            / "guest/third_party/trycua-cua/cua-driver/rust/crates/platform-linux/src"
            / "wayland/mod.rs"
        ).read_text(encoding="utf-8")
        self.assertNotIn('Command::new("wtype")', wayland)
        self.assertIn("virtual_keyboard::type_text", wayland)
        self.assertIn("Cmd::TypeText", self.cua_keyboard)

    def test_gnome_host_input_is_bounded_focused_and_clipboard_free(self) -> None:
        self.assertIn("EVDEV_BACKSPACE = 14", self.gnome_host_keyboard)
        self.assertEqual(
            self.gnome_host_keyboard.count("wait_monitor_focus(status)"), 2
        )
        self.assertIn('frame.get_action_name(index) == "default.activate"', self.gnome_host_keyboard)
        self.assertIn("finally:", self.gnome_host_keyboard)
        self.assertRegex(
            self.gnome_host_keyboard,
            r"(?s)CreateSession\(\s*timeout=DBUS_TIMEOUT\s*\)",
        )
        self.assertIn("remote.Start(timeout=DBUS_TIMEOUT)", self.gnome_host_keyboard)
        self.assertIn(
            "remote.NotifyKeyboardKeycode(dbus.UInt32(code), True, timeout=DBUS_TIMEOUT)",
            self.gnome_host_keyboard,
        )
        self.assertIn(
            "remote.NotifyKeyboardKeysym(dbus.UInt32(symbol), True, timeout=DBUS_TIMEOUT)",
            self.gnome_host_keyboard,
        )
        self.assertIn("remote.Stop(timeout=DBUS_TIMEOUT)", self.gnome_host_keyboard)
        self.assertIn("Mutter RemoteDesktop Stop failed", self.gnome_host_keyboard)
        self.assertIn("raise operation_error", self.gnome_host_keyboard)
        self.assertNotIn("EnableClipboard", self.gnome_host_keyboard)
        self.assertNotIn("nsenter", self.gnome_host_keyboard)
        self.assertNotIn("cua-driver", self.gnome_host_keyboard)
        self.assertNotIn("wtype", self.gnome_host_keyboard)


if __name__ == "__main__":
    unittest.main()
