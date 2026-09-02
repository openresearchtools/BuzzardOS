#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later

"""Typed display-scale and keyboard Settings contract tests."""

from importlib.machinery import SourceFileLoader
import importlib.util
import fcntl
import hashlib
import json
import os
from pathlib import Path
import socket
import stat
import tempfile
import unittest
from unittest import mock


REPOSITORY = Path(__file__).resolve().parents[2]
SYSTEM_XKB_ROOT = "/usr/share/X11/xkb"
SCRIPT = REPOSITORY / "guest/assets/buzzardos-output-sync"
LOADER = SourceFileLoader("buzzardos_output_sync", str(SCRIPT))
SPEC = importlib.util.spec_from_loader(LOADER.name, LOADER)
assert SPEC is not None
OUTPUT_SYNC = importlib.util.module_from_spec(SPEC)
LOADER.exec_module(OUTPUT_SYNC)


def geometry(host_scale, guest_scale, generation=9, width=1919, height=1079):
    return {
        "physical_width": width,
        "physical_height": height,
        "host_surface_scale_120": host_scale,
        "guest_ui_scale_120": guest_scale,
        "logical_width": max(1, width * 120 // guest_scale),
        "logical_height": max(1, height * 120 // guest_scale),
        "geometry_generation": generation,
    }


def state(host_scale, preset, generation=9, width=1919, height=1079):
    guest_scale = OUTPUT_SYNC.SCALE_PRESETS[preset]
    if guest_scale is None:
        guest_scale = host_scale
    return {
        "schema": 7,
        **geometry(host_scale, guest_scale, generation, width, height),
    }


class OutputScaleContractTests(unittest.TestCase):
    @staticmethod
    def sway_output(identifier, name, value, index=0):
        return {
            "id": identifier,
            "name": name,
            "active": True,
            "scale": value["guest_ui_scale_120"] / 120,
            "rect": {
                "x": index * value["logical_width"],
                "y": 0,
                "width": value["logical_width"],
                "height": value["logical_height"],
            },
            "current_mode": {
                "width": value["physical_width"],
                "height": value["physical_height"],
            },
        }

    def test_host_scale_and_guest_preset_matrix_preserves_physical_mode(self):
        for host_scale in (120, 150, 160, 180, 210, 240):
            for preset in OUTPUT_SYNC.SCALE_PRESETS:
                with self.subTest(host_scale=host_scale, preset=preset):
                    value = state(host_scale, preset)
                    self.assertTrue(OUTPUT_SYNC.valid_state(value))
                    self.assertEqual(value["physical_width"], 1919)
                    self.assertEqual(value["physical_height"], 1079)
                    expected = OUTPUT_SYNC.SCALE_PRESETS[preset] or host_scale
                    self.assertEqual(value["guest_ui_scale_120"], expected)
                    self.assertEqual(value["logical_width"], 1919 * 120 // expected)
                    self.assertEqual(value["logical_height"], 1079 * 120 // expected)

    def test_legacy_aliases_booleans_and_incoherent_geometry_are_rejected(self):
        value = state(160, "automatic")
        value["scale_120"] = value.pop("guest_ui_scale_120")
        self.assertFalse(OUTPUT_SYNC.valid_state(value))

        value = state(160, "automatic")
        value["geometry_generation"] = True
        self.assertFalse(OUTPUT_SYNC.valid_state(value))

        value = state(160, "automatic")
        value["logical_width"] += 1
        self.assertFalse(OUTPUT_SYNC.valid_state(value))

    def test_request_schema_is_exact_and_generation_aware(self):
        request = {
            "schema": 1,
            "method": "SetGuestScale",
            "preset": "125",
            "current_geometry_generation": 44,
        }
        self.assertTrue(OUTPUT_SYNC.valid_request(request))
        request["command"] = "swaymsg output * scale 99"
        self.assertFalse(OUTPUT_SYNC.valid_request(request))
        del request["command"]
        request["current_geometry_generation"] = True
        self.assertFalse(OUTPUT_SYNC.valid_request(request))

    def test_stale_request_is_rejected_without_reaching_native_display(self):
        current = state(160, "automatic", generation=12)
        request = {
            "schema": 1,
            "method": "SetGuestScale",
            "preset": "150",
            "current_geometry_generation": 11,
        }
        with mock.patch.object(OUTPUT_SYNC, "read_state", return_value=current), mock.patch.object(
            OUTPUT_SYNC, "send_host_request"
        ) as send:
            response = OUTPUT_SYNC.apply_request(request)
        self.assertFalse(response["ok"])
        self.assertEqual(response["error"]["code"], "stale_geometry")
        self.assertEqual(response["error"]["current_geometry"]["geometry_generation"], 12)
        send.assert_not_called()

    def test_same_size_new_generation_invalidates_pending_commit(self):
        committed = state(160, "125", generation=31)
        response = {
            "schema": 1,
            "ok": True,
            "preset": "125",
            "geometry": OUTPUT_SYNC.state_geometry(committed),
        }
        newer = state(160, "125", generation=32)
        with mock.patch.object(OUTPUT_SYNC, "read_state", return_value=newer):
            result = OUTPUT_SYNC.wait_for_sway_commit(response)
        self.assertFalse(result["ok"])
        self.assertEqual(result["error"]["code"], "stale_geometry")
        self.assertEqual(result["error"]["current_geometry"]["physical_width"], 1919)

    def test_success_is_reported_only_after_sway_matches_exact_geometry(self):
        committed = state(150, "175", generation=51, width=2561, height=1441)
        response = {
            "schema": 1,
            "ok": True,
            "preset": "175",
            "geometry": OUTPUT_SYNC.state_geometry(committed),
        }
        with mock.patch.object(OUTPUT_SYNC, "read_state", return_value=committed), mock.patch.object(
            OUTPUT_SYNC, "sway_matches", return_value=True
        ), mock.patch.object(OUTPUT_SYNC, "configure_sway") as configure:
            result = OUTPUT_SYNC.wait_for_sway_commit(response)
        self.assertEqual(result, response)
        configure.assert_not_called()

    def test_every_active_output_must_match_the_resized_geometry_and_layout(self):
        value = state(120, "automatic", width=1600, height=900)
        outputs = [
            self.sway_output(3, "WL-1", value, 0),
            self.sway_output(6, "HEADLESS-1", value, 1),
            self.sway_output(9, "HEADLESS-2", value, 2),
        ]
        self.assertTrue(OUTPUT_SYNC.sway_matches(value, outputs))

        stale_mode = json.loads(json.dumps(outputs))
        stale_mode[1]["current_mode"]["width"] = 1280
        self.assertFalse(OUTPUT_SYNC.sway_matches(value, stale_mode))

        overlapping = json.loads(json.dumps(outputs))
        overlapping[2]["rect"]["x"] = 2560
        self.assertFalse(OUTPUT_SYNC.sway_matches(value, overlapping))

    def test_resize_atomically_resizes_and_repacks_all_active_outputs(self):
        value = state(150, "automatic", width=2000, height=1200)
        stale = state(150, "automatic", width=1280, height=800)
        outputs = [
            self.sway_output(9, "HEADLESS-2", stale, 2),
            self.sway_output(3, "WL-1", value, 0),
            self.sway_output(6, "HEADLESS-1", stale, 1),
        ]
        completed = mock.Mock(returncode=0)
        with mock.patch.object(OUTPUT_SYNC, "sway_outputs", return_value=outputs), mock.patch.object(
            OUTPUT_SYNC.subprocess, "run", return_value=completed
        ) as run:
            self.assertTrue(OUTPUT_SYNC.configure_sway(value))
        self.assertEqual(
            run.call_args.args[0],
            [
                "swaymsg",
                "--quiet",
                "output WL-1 mode 2000x1200 scale 1.250000000 pos 0 0; "
                "output HEADLESS-1 mode 2000x1200 scale 1.250000000 pos 1600 0; "
                "output HEADLESS-2 mode 2000x1200 scale 1.250000000 pos 3200 0",
            ],
        )

    def test_invalid_output_name_cannot_become_a_sway_command(self):
        value = state(120, "automatic")
        outputs = [self.sway_output(3, "WL-1; exec foot", value, 0)]
        with mock.patch.object(OUTPUT_SYNC, "sway_outputs", return_value=outputs), mock.patch.object(
            OUTPUT_SYNC.subprocess, "run"
        ) as run:
            self.assertFalse(OUTPUT_SYNC.configure_sway(value))
        run.assert_not_called()

    def test_protocol_accepts_exactly_one_bounded_newline_delimited_request(self):
        left, right = socket.socketpair()
        self.addCleanup(left.close)
        self.addCleanup(right.close)
        request = b'{"schema":1}\n'
        right.sendall(request)
        self.assertEqual(OUTPUT_SYNC.receive_line(left), {"schema": 1})

        left2, right2 = socket.socketpair()
        self.addCleanup(left2.close)
        self.addCleanup(right2.close)
        right2.sendall(b"{}\n{}\n")
        with self.assertRaisesRegex(ValueError, "only one request"):
            OUTPUT_SYNC.receive_line(left2)

    def test_settings_endpoint_is_a_private_session_owned_socket(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(
            OUTPUT_SYNC.os.environ, {"XDG_RUNTIME_DIR": directory}
        ):
            listener, path = OUTPUT_SYNC.control_listener()
            try:
                metadata = OUTPUT_SYNC.os.lstat(path)
                self.assertTrue(stat.S_ISSOCK(metadata.st_mode))
                self.assertEqual(metadata.st_uid, OUTPUT_SYNC.os.geteuid())
                self.assertEqual(stat.S_IMODE(metadata.st_mode), 0o600)
            finally:
                listener.close()
                OUTPUT_SYNC.os.unlink(path)

    def test_keyboard_request_is_typed_bounded_and_not_a_command_surface(self):
        keyboard = {
            "model": "pc105",
            "layout": "us,gb",
            "variant": "intl,",
            "options": "compose:ralt,grp:alt_shift_toggle",
        }
        request = {
            "schema": 1,
            "method": "SetGuestKeyboard",
            "keyboard": keyboard,
        }
        self.assertTrue(OUTPUT_SYNC.valid_keyboard_request(request))
        for hostile in ["us;exec foot", "../../symbols/us", "us\n", "us $(id)"]:
            request["keyboard"] = {**keyboard, "layout": hostile}
            self.assertFalse(OUTPUT_SYNC.valid_keyboard_request(request))
        request["keyboard"] = keyboard
        request["command"] = "swaymsg exec foot"
        self.assertFalse(OUTPUT_SYNC.valid_keyboard_request(request))

    def test_real_common_and_compose_keymaps_compile_with_libxkbcommon(self):
        for keyboard in [
            {"model": "pc105", "layout": "us", "variant": "", "options": ""},
            {"model": "pc105", "layout": "gb", "variant": "", "options": ""},
            {"model": "pc105", "layout": "de", "variant": "", "options": ""},
            {
                "model": "pc105",
                "layout": "us",
                "variant": "intl",
                "options": "compose:ralt",
            },
            {
                "model": "pc105",
                "layout": "us,de",
                "variant": ",nodeadkeys",
                "options": "grp:alt_shift_toggle",
            },
        ]:
            with self.subTest(keyboard=keyboard):
                keymap = OUTPUT_SYNC.compile_keymap(
                    keyboard, SYSTEM_XKB_ROOT, require_packaged=False
                )
                self.assertTrue(keymap.startswith(b"xkb_keymap"))
                self.assertLessEqual(len(keymap), OUTPUT_SYNC.MAX_KEYMAP_BYTES)

    def test_invalid_but_well_formed_layout_fails_before_sway(self):
        keyboard = {
            "model": "pc105",
            "layout": "not-a-real-layout",
            "variant": "",
            "options": "",
        }
        self.assertTrue(OUTPUT_SYNC.valid_keyboard(keyboard))
        with self.assertRaisesRegex(ValueError, "libxkbcommon rejected"):
            OUTPUT_SYNC.compile_keymap(
                keyboard, SYSTEM_XKB_ROOT, require_packaged=False
            )

    def test_explicit_empty_components_ignore_inherited_xkb_defaults(self):
        keyboard = dict(OUTPUT_SYNC.DEFAULT_KEYBOARD)
        with mock.patch.dict(
            OUTPUT_SYNC.os.environ,
            {"XKB_DEFAULT_LAYOUT": "de", "XKB_DEFAULT_OPTIONS": "compose:ralt"},
        ):
            inherited = OUTPUT_SYNC.compile_keymap(
                keyboard, SYSTEM_XKB_ROOT, require_packaged=False
            )
        with mock.patch.dict(OUTPUT_SYNC.os.environ, {}, clear=True):
            clean = OUTPUT_SYNC.compile_keymap(
                keyboard, SYSTEM_XKB_ROOT, require_packaged=False
            )
        self.assertEqual(OUTPUT_SYNC.keyboard_digest(inherited), OUTPUT_SYNC.keyboard_digest(clean))

    def test_missing_explicit_libxkbcommon_fails_instead_of_falling_back(self):
        with self.assertRaises(FileNotFoundError):
            OUTPUT_SYNC.compile_keymap(
                dict(OUTPUT_SYNC.DEFAULT_KEYBOARD),
                SYSTEM_XKB_ROOT,
                "/missing/buzzardos/libxkbcommon.so.0",
                require_packaged=False,
            )

    def test_keyboard_endpoint_is_distinct_and_private(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(
            OUTPUT_SYNC.os.environ, {"XDG_RUNTIME_DIR": directory}
        ):
            listener, path = OUTPUT_SYNC.control_listener(
                "buzzardos-keyboard-settings.sock"
            )
            try:
                metadata = OUTPUT_SYNC.os.lstat(path)
                self.assertTrue(stat.S_ISSOCK(metadata.st_mode))
                self.assertEqual(stat.S_IMODE(metadata.st_mode), 0o600)
                self.assertTrue(path.endswith("buzzardos-keyboard-settings.sock"))
            finally:
                listener.close()
                OUTPUT_SYNC.os.unlink(path)

    def test_native_keyboard_protocol_never_transmits_a_path_or_serialized_keymap(self):
        keyboard = dict(OUTPUT_SYNC.DEFAULT_KEYBOARD)
        request = OUTPUT_SYNC._keyboard_request(
            "PrepareKeyboardMap", "1" * 32, "a" * 64, keyboard
        )
        self.assertEqual(
            set(request),
            {
                "schema",
                "method",
                "token",
                "model",
                "layout",
                "variant",
                "options",
                "keymap_sha256",
            },
        )
        self.assertNotIn("path", json.dumps(request))
        response = {
            "schema": 1,
            "ok": True,
            "method": "PrepareKeyboardMap",
            "state": "prepared",
            "active_keymap_sha256": "b" * 64,
            "pending_token": "1" * 32,
            "pending_keymap_sha256": "a" * 64,
        }
        self.assertTrue(
            OUTPUT_SYNC.valid_keyboard_host_response(response, "PrepareKeyboardMap")
        )
        response["command"] = "swaymsg"
        self.assertFalse(
            OUTPUT_SYNC.valid_keyboard_host_response(response, "PrepareKeyboardMap")
        )

    def test_keyboard_target_is_the_nested_physical_device_not_cua(self):
        inventory = [
            {
                "identifier": "0:0:cua-virtual-keyboard",
                "name": "cua-virtual-keyboard",
                "type": "keyboard",
                "xkb_active_layout_name": "English (US)",
            },
            {
                "identifier": "1:2:wayland-keyboard-buzzardos-seat",
                "name": "wayland-keyboard-buzzardos-seat",
                "type": "keyboard",
                "xkb_active_layout_name": "English (UK)",
            },
        ]
        completed = mock.Mock(returncode=0, stdout=__import__("json").dumps(inventory).encode())
        with mock.patch.object(OUTPUT_SYNC.subprocess, "run", return_value=completed):
            identifier, active_name = OUTPUT_SYNC.nested_physical_keyboard()
        self.assertEqual(identifier, "1:2:wayland-keyboard-buzzardos-seat")
        self.assertEqual(active_name, "English (UK)")

    def test_keyboard_target_requires_exactly_one_nested_physical_device(self):
        matching = {
            "identifier": "1:2:wayland-keyboard-buzzardos-seat",
            "name": "wayland-keyboard-buzzardos-seat",
            "type": "keyboard",
            "xkb_active_layout_name": "English (US)",
        }
        for inventory in [[], [matching, {**matching, "identifier": "2:3:duplicate"}]]:
            with self.subTest(count=len(inventory)):
                completed = mock.Mock(returncode=0, stdout=json.dumps(inventory).encode())
                with mock.patch.object(
                    OUTPUT_SYNC.subprocess, "run", return_value=completed
                ), self.assertRaisesRegex(RuntimeError, "exactly one"):
                    OUTPUT_SYNC.nested_physical_keyboard()

    def test_failed_keymap_write_leaves_no_session_runtime_fragment(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(
            OUTPUT_SYNC.os.environ, {"XDG_RUNTIME_DIR": directory}
        ), mock.patch.object(OUTPUT_SYNC.os, "write", side_effect=OSError("full")):
            contents = b"xkb_keymap {}"
            with self.assertRaisesRegex(OSError, "full"):
                OUTPUT_SYNC.write_managed_keymap(
                    contents, "a" * 32, OUTPUT_SYNC.keyboard_digest(contents)
                )
            self.assertEqual(list(Path(directory).iterdir()), [])

    def test_keymap_journal_snapshots_have_unique_owner_only_paths(self):
        contents = b"xkb_keymap { xkb_keycodes { minimum = 8; maximum = 255; }; };"
        digest = OUTPUT_SYNC.keyboard_digest(contents)
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(
            OUTPUT_SYNC.os.environ, {"XDG_RUNTIME_DIR": directory}
        ):
            first = OUTPUT_SYNC.write_managed_keymap(contents, "1" * 32, digest)
            second = OUTPUT_SYNC.write_managed_keymap(contents, "2" * 32, digest)
            self.assertNotEqual(first, second)
            self.assertEqual(Path(first).read_bytes(), contents)
            self.assertEqual(stat.S_IMODE(os.lstat(first).st_mode), 0o600)
            with self.assertRaises(FileExistsError):
                OUTPUT_SYNC.write_managed_keymap(contents, "1" * 32, digest)
            self.assertEqual(Path(first).read_bytes(), contents)

    def test_sway_receives_only_a_permanently_sealed_memfd(self):
        contents = b"xkb_keymap { xkb_keycodes { minimum = 8; maximum = 255; }; };"
        digest = OUTPUT_SYNC.keyboard_digest(contents)
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(
            OUTPUT_SYNC.os.environ, {"XDG_RUNTIME_DIR": directory}
        ):
            snapshot = {
                "keyboard": dict(OUTPUT_SYNC.DEFAULT_KEYBOARD),
                "digest": digest,
                "token": "3" * 32,
                "path": OUTPUT_SYNC.write_managed_keymap(contents, "3" * 32, digest),
            }
            proc_path = OUTPUT_SYNC.sealed_sway_keymap_path(snapshot)
            descriptor = int(Path(proc_path).name)
            required = (
                fcntl.F_SEAL_SHRINK
                | fcntl.F_SEAL_GROW
                | fcntl.F_SEAL_WRITE
                | fcntl.F_SEAL_SEAL
            )
            self.assertEqual(fcntl.fcntl(descriptor, fcntl.F_GET_SEALS) & required, required)
            self.assertEqual(Path(proc_path).read_bytes(), contents)
            with self.assertRaises(OSError):
                os.write(descriptor, b"changed")
            OUTPUT_SYNC.remove_managed_keymap(snapshot["path"])

    def test_replacement_and_chmod_after_open_cannot_change_sway_bytes(self):
        contents = b"xkb_keymap { // canonical\n};"
        digest = OUTPUT_SYNC.keyboard_digest(contents)
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(
            OUTPUT_SYNC.os.environ, {"XDG_RUNTIME_DIR": directory}
        ):
            snapshot = {
                "keyboard": dict(OUTPUT_SYNC.DEFAULT_KEYBOARD),
                "digest": digest,
                "token": "4" * 32,
                "path": OUTPUT_SYNC.write_managed_keymap(contents, "4" * 32, digest),
            }
            observed = []

            def swaymsg(command, **_kwargs):
                proc_path = command[-1].rsplit(" xkb_file ", 1)[1]
                replacement = Path(directory) / "replacement"
                replacement.write_bytes(b"hostile replacement")
                replacement.chmod(0o777)
                os.replace(replacement, snapshot["path"])
                observed.append(Path(proc_path).read_bytes())
                return mock.Mock(returncode=0, stdout=b"", stderr=b"")

            with mock.patch.object(
                OUTPUT_SYNC,
                "nested_physical_keyboard",
                return_value=("1:2:wayland-keyboard-buzzardos-seat", "English (US)"),
            ), mock.patch.object(OUTPUT_SYNC.subprocess, "run", side_effect=swaymsg):
                self.assertEqual(OUTPUT_SYNC.apply_sway_keymap(snapshot), "English (US)")
            self.assertEqual(observed, [contents])
            self.assertEqual(Path(snapshot["path"]).read_bytes(), b"hostile replacement")
            OUTPUT_SYNC.remove_managed_keymap(snapshot["path"])

    def test_snapshot_read_stays_bound_to_opened_inode_during_path_replacement(self):
        contents = b"xkb_keymap { // opened inode\n};"
        digest = OUTPUT_SYNC.keyboard_digest(contents)
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(
            OUTPUT_SYNC.os.environ, {"XDG_RUNTIME_DIR": directory}
        ):
            snapshot = {
                "keyboard": dict(OUTPUT_SYNC.DEFAULT_KEYBOARD),
                "digest": digest,
                "token": "6" * 32,
                "path": OUTPUT_SYNC.write_managed_keymap(contents, "6" * 32, digest),
            }
            real_read = OUTPUT_SYNC.os.read
            replaced = False

            def replace_then_read(descriptor, length):
                nonlocal replaced
                if not replaced:
                    replaced = True
                    replacement = Path(directory) / "replacement"
                    replacement.write_bytes(b"different inode")
                    replacement.chmod(0o600)
                    os.replace(replacement, snapshot["path"])
                return real_read(descriptor, length)

            with mock.patch.object(OUTPUT_SYNC.os, "read", side_effect=replace_then_read):
                self.assertEqual(OUTPUT_SYNC.read_keymap_snapshot(snapshot), contents)
            self.assertEqual(Path(snapshot["path"]).read_bytes(), b"different inode")
            OUTPUT_SYNC.remove_managed_keymap(snapshot["path"])

    def test_host_prepare_digest_equals_the_exact_sealed_sway_payload(self):
        contents = b"xkb_keymap { // digest parity\n};"
        digest = OUTPUT_SYNC.keyboard_digest(contents)
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(
            OUTPUT_SYNC.os.environ, {"XDG_RUNTIME_DIR": directory}
        ):
            snapshot = {
                "keyboard": dict(OUTPUT_SYNC.DEFAULT_KEYBOARD),
                "digest": digest,
                "token": "5" * 32,
                "path": OUTPUT_SYNC.write_managed_keymap(contents, "5" * 32, digest),
            }
            request = OUTPUT_SYNC._keyboard_request(
                "PrepareKeyboardMap",
                snapshot["token"],
                snapshot["digest"],
                snapshot["keyboard"],
            )
            proc_path = OUTPUT_SYNC.sealed_sway_keymap_path(snapshot)
            self.assertEqual(
                hashlib.sha256(Path(proc_path).read_bytes()).hexdigest(),
                request["keymap_sha256"],
            )
            OUTPUT_SYNC.remove_managed_keymap(snapshot["path"])

    def test_full_settings_schema_rejects_valid_keyboard_inside_invalid_document(self):
        document = {
            "schema_version": 3,
            "generation": 7,
            "appearance": {
                "theme": "dark",
                "background": {"kind": "dark_plain"},
                "capped_task_buttons": True,
                "pinned_applications": [],
            },
            "display": {"guest_ui_scale": "125"},
            "keyboard": dict(OUTPUT_SYNC.DEFAULT_KEYBOARD),
            "updates": {"last_notified_plan_generation": None},
        }
        with tempfile.TemporaryDirectory() as directory, mock.patch.object(
            OUTPUT_SYNC, "settings_path", return_value=os.path.join(directory, "settings.json")
        ):
            Path(OUTPUT_SYNC.settings_path()).write_text(json.dumps(document))
            self.assertEqual(
                OUTPUT_SYNC.persisted_preferences(),
                {"preset": "125", "keyboard": OUTPUT_SYNC.DEFAULT_KEYBOARD},
            )
            document["appearance"]["background"]["path"] = "/host/escape"
            Path(OUTPUT_SYNC.settings_path()).write_text(json.dumps(document))
            self.assertIsNone(OUTPUT_SYNC.persisted_preferences())

    def test_shared_manually_authored_contract_matches_persisted_settings(self):
        fixture = json.loads(
            (REPOSITORY / "tests/fixtures/xkb-settings-contract.json").read_text()
        )
        self.assertEqual(fixture["schema"], 1)
        for case in fixture["cases"]:
            with self.subTest(case=case["name"]), tempfile.TemporaryDirectory() as directory:
                document = {
                    "schema_version": 3,
                    "generation": 0,
                    "appearance": {
                        "theme": "dark",
                        "background": {"kind": "dark_plain"},
                        "capped_task_buttons": True,
                        "pinned_applications": [],
                    },
                    "display": {"guest_ui_scale": "automatic"},
                    "keyboard": case["keyboard"],
                    "updates": {"last_notified_plan_generation": None},
                }
                path = os.path.join(directory, "settings.json")
                Path(path).write_text(json.dumps(document))
                with mock.patch.object(OUTPUT_SYNC, "settings_path", return_value=path):
                    preferences = OUTPUT_SYNC.persisted_preferences()
                self.assertEqual(preferences is not None, case["valid"])

    def test_keyboard_component_byte_bounds_are_exact(self):
        for field, (_minimum, maximum) in OUTPUT_SYNC.XKB_LIMITS.items():
            with self.subTest(field=field):
                keyboard = dict(OUTPUT_SYNC.DEFAULT_KEYBOARD)
                keyboard[field] = "a" * maximum
                self.assertTrue(OUTPUT_SYNC.valid_keyboard(keyboard))
                keyboard[field] += "a"
                self.assertFalse(OUTPUT_SYNC.valid_keyboard(keyboard))

    def test_transaction_success_swaps_active_map_and_removes_previous(self):
        old = {
            "keyboard": dict(OUTPUT_SYNC.DEFAULT_KEYBOARD),
            "digest": "a" * 64,
            "token": "1" * 32,
            "path": "/runtime/old.xkb",
        }
        new = {
            "keyboard": {**OUTPUT_SYNC.DEFAULT_KEYBOARD, "layout": "gb"},
            "digest": "b" * 64,
            "token": "2" * 32,
            "path": "/runtime/new.xkb",
        }
        manager = OUTPUT_SYNC.KeyboardTransactionManager(old)
        prepared = {
            "schema": 1,
            "ok": True,
            "method": "PrepareKeyboardMap",
            "state": "prepared",
            "active_keymap_sha256": old["digest"],
            "pending_token": new["token"],
            "pending_keymap_sha256": new["digest"],
        }
        committed = {
            "schema": 1,
            "ok": True,
            "method": "CommitKeyboardMap",
            "state": "committed",
            "active_keymap_sha256": new["digest"],
        }
        with mock.patch.object(OUTPUT_SYNC, "_new_keymap", return_value=new), mock.patch.object(
            OUTPUT_SYNC, "prepare_keyboard_host", return_value=prepared
        ), mock.patch.object(
            OUTPUT_SYNC, "apply_sway_keymap", return_value="English (UK)"
        ), mock.patch.object(
            OUTPUT_SYNC, "finish_keyboard_host", return_value=committed
        ), mock.patch.object(
            OUTPUT_SYNC, "read_transaction_journal", return_value=None
        ), mock.patch.object(OUTPUT_SYNC, "write_transaction_journal") as write, mock.patch.object(
            OUTPUT_SYNC, "remove_managed_keymap"
        ) as remove:
            active_name = manager.apply(new["keyboard"])
        self.assertEqual(active_name, "English (UK)")
        self.assertIs(manager.active, new)
        self.assertEqual(
            [call.args[2] for call in write.call_args_list],
            ["created", "prepared", "sway_applied", "commit_sent", "committed"],
        )
        remove.assert_not_called()

    def test_sway_failure_restores_previous_map_before_host_abort(self):
        old = {
            "keyboard": dict(OUTPUT_SYNC.DEFAULT_KEYBOARD),
            "digest": "a" * 64,
            "token": "1" * 32,
            "path": "/runtime/old.xkb",
        }
        new = {
            "keyboard": {**OUTPUT_SYNC.DEFAULT_KEYBOARD, "layout": "de"},
            "digest": "b" * 64,
            "token": "2" * 32,
            "path": "/runtime/new.xkb",
        }
        manager = OUTPUT_SYNC.KeyboardTransactionManager(old)
        prepared = {
            "schema": 1,
            "ok": True,
            "method": "PrepareKeyboardMap",
            "state": "prepared",
            "active_keymap_sha256": old["digest"],
            "pending_token": new["token"],
            "pending_keymap_sha256": new["digest"],
        }
        aborted = {
            "schema": 1,
            "ok": True,
            "method": "AbortKeyboardMap",
            "state": "aborted",
            "active_keymap_sha256": old["digest"],
        }
        calls = []

        def sway(snapshot):
            calls.append(("sway", snapshot))
            if snapshot == new:
                raise RuntimeError("rejected")
            return "English (US)"

        def terminal(method, token, digest):
            calls.append((method, token, digest))
            return aborted

        with mock.patch.object(OUTPUT_SYNC, "_new_keymap", return_value=new), mock.patch.object(
            OUTPUT_SYNC, "prepare_keyboard_host", return_value=prepared
        ), mock.patch.object(OUTPUT_SYNC, "apply_sway_keymap", side_effect=sway), mock.patch.object(
            OUTPUT_SYNC, "finish_keyboard_host", side_effect=terminal
        ), mock.patch.object(
            OUTPUT_SYNC, "read_transaction_journal", return_value=None
        ), mock.patch.object(OUTPUT_SYNC, "write_transaction_journal"), mock.patch.object(
            OUTPUT_SYNC, "clear_transaction_journal"
        ), mock.patch.object(OUTPUT_SYNC, "remove_managed_keymap"):
            with self.assertRaisesRegex(RuntimeError, "previous keyboard layout was restored"):
                manager.apply(new["keyboard"])
        self.assertEqual(calls[0:2], [("sway", new), ("sway", old)])
        self.assertEqual(calls[2][0], "AbortKeyboardMap")

    def test_unconfirmed_abort_after_guest_restore_forces_supervised_recovery(self):
        old = {
            "keyboard": dict(OUTPUT_SYNC.DEFAULT_KEYBOARD),
            "digest": "a" * 64,
            "token": "1" * 32,
            "path": "/runtime/old.xkb",
        }
        new = {
            "keyboard": {**OUTPUT_SYNC.DEFAULT_KEYBOARD, "layout": "de"},
            "digest": "b" * 64,
            "token": "2" * 32,
            "path": "/runtime/new.xkb",
        }
        unknown = {
            "schema": 1,
            "ok": True,
            "method": "StatusKeyboardMap",
            "state": "unknown",
            "active_keymap_sha256": old["digest"],
        }
        manager = OUTPUT_SYNC.KeyboardTransactionManager(old)
        with mock.patch.object(
            OUTPUT_SYNC, "apply_sway_keymap", return_value="English (US)"
        ), mock.patch.object(
            OUTPUT_SYNC, "finish_keyboard_host", return_value=unknown
        ), mock.patch.object(OUTPUT_SYNC, "clear_transaction_journal") as clear:
            with self.assertRaises(OUTPUT_SYNC.FatalKeyboardTransaction):
                manager._restore_before_abort(new, "requested map failed")
        clear.assert_not_called()

    def test_lost_commit_response_is_reconciled_by_status(self):
        token = "1" * 32
        digest = "a" * 64
        committed = {
            "schema": 1,
            "ok": True,
            "method": "StatusKeyboardMap",
            "state": "committed",
            "active_keymap_sha256": digest,
        }
        with mock.patch.object(
            OUTPUT_SYNC,
            "send_keyboard_host_request",
            side_effect=[socket.timeout("lost"), committed],
        ) as send:
            response = OUTPUT_SYNC.finish_keyboard_host(
                "CommitKeyboardMap", token, digest
            )
        self.assertEqual(response, committed)
        self.assertEqual(send.call_count, 2)

    def test_unknown_commit_outcome_forces_supervised_restart_with_journal(self):
        old = {
            "keyboard": dict(OUTPUT_SYNC.DEFAULT_KEYBOARD),
            "digest": "a" * 64,
            "token": "1" * 32,
            "path": "/runtime/old.xkb",
        }
        new = {
            "keyboard": {**OUTPUT_SYNC.DEFAULT_KEYBOARD, "layout": "de"},
            "digest": "b" * 64,
            "token": "2" * 32,
            "path": "/runtime/new.xkb",
        }
        prepared = {
            "schema": 1,
            "ok": True,
            "method": "PrepareKeyboardMap",
            "state": "prepared",
            "active_keymap_sha256": old["digest"],
            "pending_token": new["token"],
            "pending_keymap_sha256": new["digest"],
        }
        manager = OUTPUT_SYNC.KeyboardTransactionManager(old)
        with mock.patch.object(
            OUTPUT_SYNC, "read_transaction_journal", return_value=None
        ), mock.patch.object(
            OUTPUT_SYNC, "_new_keymap", return_value=new
        ), mock.patch.object(
            OUTPUT_SYNC, "prepare_keyboard_host", return_value=prepared
        ), mock.patch.object(
            OUTPUT_SYNC, "apply_sway_keymap", return_value="German"
        ), mock.patch.object(
            OUTPUT_SYNC,
            "finish_keyboard_host",
            side_effect=socket.timeout("status unavailable"),
        ), mock.patch.object(
            OUTPUT_SYNC, "write_transaction_journal"
        ) as write, mock.patch.object(
            OUTPUT_SYNC, "clear_transaction_journal"
        ) as clear:
            with self.assertRaises(OUTPUT_SYNC.FatalKeyboardTransaction):
                manager.apply(new["keyboard"])
        self.assertEqual(write.call_args_list[-1].args[2], "commit_sent")
        clear.assert_not_called()

    def test_fresh_sway_uses_same_digest_prepare_even_when_host_is_already_non_us(self):
        desired = {**OUTPUT_SYNC.DEFAULT_KEYBOARD, "layout": "de"}
        prior = {
            "keyboard": desired,
            "digest": "a" * 64,
            "token": "1" * 32,
            "path": "/runtime/prior.xkb",
        }
        requested = {**prior, "token": "2" * 32, "path": "/runtime/requested.xkb"}
        status = {
            "schema": 1,
            "ok": True,
            "method": "StatusKeyboardMap",
            "state": "unknown",
            "active_keymap_sha256": prior["digest"],
        }
        with mock.patch.object(
            OUTPUT_SYNC, "_new_keymap", side_effect=[prior, requested]
        ), mock.patch.object(
            OUTPUT_SYNC, "keyboard_host_status", return_value=status
        ), mock.patch.object(
            OUTPUT_SYNC.KeyboardTransactionManager, "_apply_prepared_map"
        ) as apply, mock.patch.object(
            OUTPUT_SYNC.KeyboardTransactionManager, "acknowledge_persisted_commit"
        ), mock.patch.object(OUTPUT_SYNC, "apply_sway_keymap") as direct_apply:
            manager = OUTPUT_SYNC.KeyboardTransactionManager._bootstrap_clean(desired)
        apply.assert_called_once_with(requested)
        direct_apply.assert_not_called()
        self.assertEqual(manager.active, prior)

    def test_private_journal_round_trips_only_valid_owner_snapshots(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(
            OUTPUT_SYNC.os.environ, {"XDG_RUNTIME_DIR": directory}
        ):
            def snapshot(token, keyboard, marker):
                contents = f"xkb_keymap {{ // {marker}\n}}".encode()
                digest = OUTPUT_SYNC.keyboard_digest(contents)
                return {
                    "keyboard": keyboard,
                    "digest": digest,
                    "token": token,
                    "path": OUTPUT_SYNC.write_managed_keymap(contents, token, digest),
                }

            prior = snapshot("1" * 32, dict(OUTPUT_SYNC.DEFAULT_KEYBOARD), "old")
            requested = snapshot(
                "2" * 32,
                {**OUTPUT_SYNC.DEFAULT_KEYBOARD, "layout": "gb"},
                "new",
            )
            OUTPUT_SYNC.write_transaction_journal(prior, requested, "prepared")
            journal = OUTPUT_SYNC.read_transaction_journal()
            self.assertEqual(journal["phase"], "prepared")
            self.assertEqual(stat.S_IMODE(os.lstat(OUTPUT_SYNC.transaction_journal_path()).st_mode), 0o600)
            document = json.loads(Path(OUTPUT_SYNC.transaction_journal_path()).read_text())
            document["command"] = "swaymsg exec foot"
            Path(OUTPUT_SYNC.transaction_journal_path()).write_text(json.dumps(document))
            os.chmod(OUTPUT_SYNC.transaction_journal_path(), 0o600)
            with self.assertRaisesRegex(RuntimeError, "malformed"):
                OUTPUT_SYNC.read_transaction_journal()

    def test_desktop_services_runs_output_sync_without_retry_probe_loop(self):
        services = (REPOSITORY / "guest/assets/buzzardos-desktop-services").read_text()
        self.assertIn(
            'start_service "$runtime/libexec/buzzardos-output-sync"',
            services,
        )
        self.assertNotIn("start_output_sync_supervisor", services)
        self.assertNotIn("consecutive failure level", services)

    def test_failed_ready_publish_leaves_no_session_runtime_fragment(self):
        with tempfile.TemporaryDirectory() as directory, mock.patch.dict(
            OUTPUT_SYNC.os.environ, {"XDG_RUNTIME_DIR": directory}
        ), mock.patch.object(
            OUTPUT_SYNC, "nested_physical_keyboard", return_value=("device", "English (US)")
        ), mock.patch.object(OUTPUT_SYNC.os, "replace", side_effect=OSError("replace")):
            with self.assertRaisesRegex(OSError, "replace"):
                OUTPUT_SYNC.publish_ready()
            self.assertEqual(list(Path(directory).iterdir()), [])


if __name__ == "__main__":
    unittest.main()
