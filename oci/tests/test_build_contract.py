#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later

import unittest
import re
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class OciBuildContractTests(unittest.TestCase):
    def test_every_non_scratch_base_is_an_exact_amd64_manifest(self) -> None:
        containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )
        lock = tomllib.loads(
            (ROOT / "oci/base-images.lock.toml").read_text(encoding="utf-8")
        )
        self.assertEqual(lock["schema"], 1)
        self.assertEqual(lock["platform"], "linux/amd64")
        locked = {
            image["reference"]: image["manifest_digest"]
            for image in lock["image"]
        }
        from_references = [
            match.group(1)
            for match in re.finditer(r"^FROM\s+(\S+)", containerfile, re.MULTILINE)
            if match.group(1) != "scratch"
        ]
        self.assertEqual(len(from_references), 1)
        for reference in from_references:
            name, separator, digest = reference.partition("@")
            self.assertEqual(separator, "@", reference)
            self.assertRegex(digest, r"^sha256:[0-9a-f]{64}$")
            self.assertEqual(digest, locked[name], reference)

    def test_dockerfile_frontend_is_an_exact_amd64_manifest(self) -> None:
        containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )
        lock = tomllib.loads(
            (ROOT / "oci/base-images.lock.toml").read_text(encoding="utf-8")
        )
        frontend = lock["frontend"]
        self.assertEqual(
            containerfile.splitlines()[0],
            f'# syntax={frontend["reference"]}@{frontend["manifest_digest"]}',
        )

    def test_build_time_debian_repositories_are_immutable_snapshots(self) -> None:
        lock = tomllib.loads(
            (ROOT / "oci/base-images.lock.toml").read_text(encoding="utf-8")
        )
        snapshots = {
            image["reference"]: image["apt_snapshot"]
            for image in lock["image"]
        }
        sid = (ROOT / "oci/desktop/apt/debian-sid-snapshot.sources").read_text(
            encoding="utf-8"
        )
        self.assertIn(snapshots["docker.io/library/debian:sid"], sid)
        self.assertIn("snapshot.debian.org", sid)
        self.assertNotIn("deb.debian.org", sid)
        self.assertIn("Check-Valid-Until: no", sid)
        live = (ROOT / "oci/desktop/apt/debian-sid-live.sources").read_text(
            encoding="utf-8"
        )
        self.assertIn("http://deb.debian.org/debian", live)

    def test_buildah_uses_a_minimal_context_and_discards_its_private_store(self) -> None:
        builder = (ROOT / "oci/build-local.sh").read_text(encoding="utf-8")
        for required in (
            "buildah_local build",
            "--storage-driver vfs",
            "--no-cache",
            "--pull=always",
            'install -m 0644 "$guest_deb" "$desktop_deb" "$cua_deb"',
            'rm -rf -- "$context" "$work"',
        ):
            self.assertIn(required, builder)
        for redundant in ("docker ", "podman ", "skopeo", "crane"):
            self.assertNotIn(redundant, builder)

    def test_reference_image_uses_only_distribution_sway_and_wlroots(self) -> None:
        containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )
        packages = (ROOT / "packaging/build-debs.sh").read_text(encoding="utf-8")
        self.assertIn("sway (>= 1.9)", packages)
        self.assertIn("xkb-data", packages)
        for forbidden in (
            "AS sway-builder",
            "AS sway-runtime-artifact",
            "SWAY_COMMIT",
            "WLROOTS_COMMIT",
            "wlroots.git",
            "meson setup sway",
            "meson setup wlroots",
            "/runtime-payload/bin/sway",
        ):
            self.assertNotIn(forbidden, containerfile)
        session = (ROOT / "guest/assets/buzzardos-sway-session").read_text(
            encoding="utf-8"
        )
        self.assertIn("/usr/bin/sway", session)
        self.assertIn("/usr/bin/swaymsg", session)

    def test_guest_defaults_do_not_overwrite_distribution_gtk_configuration(self) -> None:
        manifest = (ROOT / "guest/desktop-asset-manifest.tsv").read_text(encoding="utf-8")
        for forbidden in (
            "etc/gtk-3.0/settings.ini",
            "etc/gtk-4.0/settings.ini",
            "etc/xdg/kwalletrc",
        ):
            self.assertNotRegex(manifest, rf"(?m)\t{re.escape(forbidden)}$")
        for managed in (
            "etc/buzzardos/xdg/gtk-3.0/settings.ini",
            "etc/buzzardos/xdg/gtk-4.0/settings.ini",
            "etc/buzzardos/xdg/kwalletrc",
        ):
            self.assertRegex(manifest, rf"(?m)\t{re.escape(managed)}$")

    def test_runtime_contract_names_desktop_and_appimage_dependencies(self) -> None:
        containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )
        packages = (ROOT / "packaging/build-debs.sh").read_text(encoding="utf-8")
        for package in (
            "ffmpeg",
            "firefox-esr",
            "foot",
            "fuse3",
            "gsettings-desktop-schemas",
            "libfuse2t64",
            "libgbm1",
            "libglib2.0-bin",
            "libgtk-3-0t64",
            "libgtk-4-1",
            "libnss3",
            "libpulse0",
            "libxkbcommon0",
            "mousepad",
            "squashfs-tools",
            "thunar",
            "xkb-data",
            "xwayland",
        ):
            self.assertIn(package, packages)
        for package in (
            "chromium",
            "dolphin",
            "mesa-utils",
            "pavucontrol",
            "vulkan-tools",
            "x11-apps",
            "xterm",
        ):
            self.assertNotRegex(packages, rf"(?:^|, ){re.escape(package)}(?:,|')")
        verifier = (ROOT / "oci/verify-image.sh").read_text(encoding="utf-8")
        self.assertRegex(verifier, r"(?m)^\s+gsettings(?:\s|\\)")
        self.assertIn("dconf-gsettings-backend", verifier)
        self.assertIn("gsettings-desktop-schemas", verifier)
        self.assertIn("gsettings list-keys org.gnome.desktop.interface", verifier)
        self.assertIn("gsettings set org.gnome.desktop.interface gtk-theme", verifier)
        self.assertIn("gsettings get org.gnome.desktop.interface gtk-theme", verifier)
        self.assertIn("buzzardos-guest_*_amd64.deb", containerfile)
        self.assertIn("buzzardos-desktop_*_amd64.deb", containerfile)
        self.assertIn("buzzardoscua_*_amd64.deb", containerfile)
        self.assertNotIn("AS deb-builder", containerfile)
        self.assertNotIn("cargo build", containerfile)
        self.assertNotIn("packaging/build-debs.sh", containerfile)
        self.assertNotIn("COPY . .", containerfile)
        self.assertIn("/usr/libexec/buzzardos-shortcut-helper", verifier)
        self.assertIn("/libexec/buzzardos-clipboard-agent", verifier)
        self.assertIn("unsquashfs", verifier)
        self.assertNotIn("AS shell-builder", containerfile)
        self.assertNotIn("AS settings-builder", containerfile)
        self.assertIn("libpulse.so.0", verifier)
        provisioning = (ROOT / "oci/desktop/provision-image.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("20auto-upgrades", provisioning)
        self.assertIn("unattended-upgrades", packages)
        self.assertNotIn("openssh-client", containerfile)
        self.assertNotIn("buzzardos-updater.service", containerfile)
        for forbidden in (
            "blender",
            "build-essential",
            "chromium",
            "dolphin",
            "kwin-wayland",
            "mesa-utils",
            "pavucontrol",
            "plasma-workspace",
            "rustc",
            "vulkan-tools",
            "waybar",
            "x11-apps",
            "xterm",
        ):
            self.assertIn(forbidden, verifier)

    def test_settings_sound_uses_native_pulse_client_without_shell_helpers(self) -> None:
        sound = (ROOT / "guest/settings/src/sound.rs").read_text(encoding="utf-8")
        for required in (
            "libpulse_binding",
            "ContextFlagSet::NOAUTOSPAWN",
            "InterestMaskSet::SINK_INPUT",
            "InterestMaskSet::SOURCE_OUTPUT",
            "connect_record",
            "MICROPHONE_TEST_HARD_LIMIT",
        ):
            self.assertIn(required, sound)
        for forbidden in (
            "std::process::Command",
            "wpctl",
            "pactl",
            "parec",
            "paplay",
        ):
            self.assertNotIn(forbidden, sound)

    def test_runtime_readiness_is_bound_to_one_broker_session(self) -> None:
        broker = (ROOT / "host/crates/buzzardos-broker/src/main.rs").read_text(
            encoding="utf-8"
        )
        services = (ROOT / "guest/assets/buzzardos-desktop-services").read_text(
            encoding="utf-8"
        )
        readiness = (ROOT / "guest/assets/buzzardos-runtime-ready").read_text(
            encoding="utf-8"
        )
        unit = (ROOT / "guest/assets/buzzardos-runtime-ready.service").read_text(
            encoding="utf-8"
        )
        self.assertIn('"BUZZARDOS_SESSION_TOKEN".into()', broker)
        self.assertIn("Uuid::new_v4().simple().to_string()", broker)
        self.assertIn("desktop_ready_for_session(marker, expected_session_token)", broker)
        self.assertIn("const GUEST_RUNTIME_MODE: u32 = 0o700", broker)
        self.assertIn("mktemp \"$status_dir/.desktop-ready.XXXXXX\"", services)
        self.assertIn("chmod 0600 \"$desktop_ready_tmp\"", services)
        self.assertIn("$BUZZARDOS_SESSION_TOKEN", services)
        self.assertIn("EnvironmentFile=/run/buzzardos-host/driver.env", unit)
        self.assertIn("SESSION_TOKEN_RE", readiness)
        self.assertIn("HOST_RUNTIME_MODE = 0o700", readiness)
        self.assertIn("read_desktop_ready(DESKTOP_READY, session_token)", readiness)

    def test_managed_sway_config_does_not_invoke_unpackaged_swaybg(self) -> None:
        sway_config = (ROOT / "guest/assets/sway-config").read_text(
            encoding="utf-8"
        )
        containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )
        verifier = (ROOT / "oci/verify-image.sh").read_text(encoding="utf-8")

        output_background = re.search(
            r"(?m)^\s*output\b[^\n]*\sbg(?:\s|$)", sway_config
        )
        explicit_swaybg = re.search(
            r"(?m)^\s*exec(?:_always)?\b[^\n]*\bswaybg(?:\s|$)", sway_config
        )
        invokes_swaybg = output_background is not None or explicit_swaybg is not None
        packages_swaybg = re.search(
            r"(?m)^\s+swaybg(?:\s+\\|\s+&&)", containerfile
        ) is not None
        verifies_swaybg = re.search(
            r"(?m)^\s+swaybg(?:\s+\\|\s*$)", verifier
        ) is not None

        self.assertFalse(
            invokes_swaybg and not (packages_swaybg and verifies_swaybg),
            "managed Sway config invokes swaybg without packaging and verifying it",
        )

    def test_cuda_packages_are_exact_hash_verified_downloads(self) -> None:
        containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )
        expected_hashes = (
            "282d46cada9eea16e4c61147d8fffb8d2197491d9c86fa7afc6b00746bd433cc",
            "5ba60863efe4334deefd9af6f45bdbce0805438cc16a23b0f120fd838323c8b8",
            "f52bd03da5b0445eb1fce5e9aa141d28ef7530285b3f4f18a9190d8f90d5a78a",
            "b17bfbf57e2eebb5c893355cf15d64de23dbc2ccc227a250323bd2d533b71e84",
            "0a19b72fb4ab5657343407f21afa88764359f06c9c1b9890df03fc89912b53f7",
        )
        for digest in expected_hashes:
            self.assertIn(digest, containerfile)
        for package in (
            "cuda-toolkit-config-common",
            "cuda-toolkit-13-config-common",
            "cuda-toolkit-13-1-config-common",
            "cuda-cudart-13-1",
            "libcublas-13-1",
        ):
            self.assertIn(f"/{package}_", containerfile)
        self.assertIn("sha256sum --check", containerfile)
        self.assertNotIn("cuda-keyring", containerfile)
        cuda_section = containerfile.split(
            "# The reference machine carries only the CUDA runtime", 1
        )[1].split("COPY apt/debian-sid-live.sources", 1)[0]
        self.assertNotIn("apt-get", cuda_section)


if __name__ == "__main__":
    unittest.main()
