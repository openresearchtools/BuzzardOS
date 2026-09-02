# SPDX-License-Identifier: AGPL-3.0-or-later
"""Static gates for the daemonless, single-crate Buzzard CUA boundary."""

from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
CUA = ROOT / "cua"


class CuaReductionContractTests(unittest.TestCase):
    def test_product_version_is_independent_from_trycua_provenance(self) -> None:
        version = (CUA / "VERSION").read_text(encoding="utf-8").strip()
        manifest = (CUA / "Cargo.toml").read_text(encoding="utf-8")
        skill = (CUA / "Skills/buzzard-cua/SKILL.md").read_text(encoding="utf-8")
        upstream = (CUA / "UPSTREAM.toml").read_text(encoding="utf-8")
        notices = (ROOT / "THIRD_PARTY_NOTICES.md").read_text(encoding="utf-8")

        self.assertRegex(version, r"^\d+\.\d+\.\d+$")
        self.assertNotIn("buzzard", version)
        self.assertIn(f'version = "{version}"', manifest)
        self.assertIn(f"version: {version}", skill)
        upstream_version = re.search(r'^upstream_version = "([^"]+)"$', upstream, re.M)
        self.assertIsNotNone(upstream_version)
        self.assertNotEqual(version, upstream_version.group(1))
        self.assertNotIn(f"Buzzard CUA {upstream_version.group(1)}", notices)

    def test_cua_is_exactly_one_rust_crate(self) -> None:
        manifests = sorted(
            path.relative_to(CUA).as_posix() for path in CUA.rglob("Cargo.toml")
        )
        self.assertEqual(manifests, ["Cargo.toml"])
        manifest = (CUA / "Cargo.toml").read_text(encoding="utf-8")
        self.assertIn('name = "buzzardoscua"', manifest)
        self.assertIn('name = "cua"', manifest)
        self.assertNotIn("[workspace]", manifest)

    def test_obsolete_upstream_runtime_tree_is_deleted(self) -> None:
        self.assertFalse((ROOT / "guest/third_party/trycua-cua").exists())
        for name in ("browser", "recording", "session_tools.rs", "server.rs", "daemon.rs"):
            self.assertFalse((CUA / "src/core" / name).exists(), name)
        self.assertFalse((CUA / "src/contract/session.rs").exists())

    def test_package_has_no_cua_service_or_server_command(self) -> None:
        packaging = (ROOT / "packaging/build-debs.sh").read_text(encoding="utf-8")
        guest_assets = "\n".join(
            path.read_text(encoding="utf-8", errors="replace")
            for path in sorted((ROOT / "guest/assets").iterdir())
            if path.is_file()
        )
        self.assertIn('"$root/usr/bin/cua"', packaging)
        self.assertIn('cua/Skills/buzzard-cua/SKILL.md', packaging)
        self.assertNotIn("buzzardoscua.service", packaging + guest_assets)
        self.assertNotIn("buzzardoscua serve", packaging + guest_assets)

    def test_numbered_outputs_never_enter_parent_wayland_input_or_presentation(self) -> None:
        session = (ROOT / "guest/assets/buzzardos-sway-session").read_text(
            encoding="utf-8"
        )
        gateway = (
            ROOT / "host/crates/buzzardos-display/src/guest_display.rs"
        ).read_text(encoding="utf-8")
        self.assertIn("export WLR_BACKENDS=headless,wayland", session)
        self.assertIn("export WLR_HEADLESS_OUTPUTS=0", session)
        self.assertIn("export WLR_WL_OUTPUTS=1", session)
        self.assertIn("if state.primary_surface.is_none()", gateway)
        self.assertIn("if !host_facing", gateway)
        self.assertIn("self.pointers.iter().take(1)", gateway)

    def test_dynamic_outputs_receive_workspace_scoped_desktop_and_taskbar(self) -> None:
        shell = (ROOT / "guest/shell/src/main.rs").read_text(encoding="utf-8")
        self.assertIn("fn ensure_auxiliary_output", shell)
        self.assertIn("Some(output)", shell)
        self.assertIn("fn windows_for_output", shell)
        self.assertIn("fn draw_auxiliary_outputs", shell)
        self.assertIn("self.ensure_auxiliary_output(qh, &output)", shell)

    def test_public_sources_have_no_session_lifecycle_or_telemetry_switch(self) -> None:
        active = "\n".join(
            path.read_text(encoding="utf-8", errors="replace")
            for path in sorted(CUA.rglob("*.rs"))
        )
        self.assertNotIn("start_session", active)
        self.assertNotIn("end_session", active)
        self.assertNotIn("TELEMETRY_ENABLED", active)
        self.assertNotIn("posthog", active.lower())

    def test_cua_uses_only_native_numbered_seat_cursors(self) -> None:
        active = "\n".join(
            path.read_text(encoding="utf-8", errors="replace")
            for path in sorted(CUA.rglob("*.rs"))
        )
        manifest = (CUA / "Cargo.toml").read_text(encoding="utf-8")
        self.assertFalse((CUA / "src/cursor").exists())
        self.assertFalse((CUA / "src/platform/overlay.rs").exists())
        self.assertFalse((CUA / "src/platform/wayland/overlay.rs").exists())
        self.assertFalse((CUA / "assets/cursor").exists())
        for obsolete in (
            "set_agent_cursor_enabled",
            "set_agent_cursor_motion",
            "set_agent_cursor_theme",
            "get_agent_cursor_state",
            "OverlayCommand",
            "CursorRegistry",
        ):
            self.assertNotIn(obsolete, active)
        for dependency in ("fontdue", "postcard", "tiny-skia", "zstd"):
            self.assertNotIn(dependency, manifest)
        wayland = (CUA / "src/platform/wayland/mod.rs").read_text(encoding="utf-8")
        self.assertIn("move_cursor_absolute", wayland)
        self.assertIn("capture_output(1, &output", wayland)

    def test_launched_apps_cannot_hold_one_shot_cli_transports_open(self) -> None:
        tools = (CUA / "src/platform/tools/impl_.rs").read_text(encoding="utf-8")
        launch = tools[tools.index("impl Tool for LaunchAppTool") :]
        self.assertIn(".stdin(Stdio::null())", launch)
        self.assertIn(".stdout(Stdio::null())", launch)
        self.assertIn(".stderr(Stdio::null())", launch)


if __name__ == "__main__":
    unittest.main()
