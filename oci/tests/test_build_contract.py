#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later

import unittest
import re
import tomllib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


class OciBuildContractTests(unittest.TestCase):
    def test_complete_desktop_has_no_additional_service_task_ceiling(self) -> None:
        from configparser import ConfigParser

        unit = ConfigParser(interpolation=None, strict=False)
        unit.read(ROOT / "guest/assets/buzzardos-desktop.service")
        self.assertEqual(unit["Service"]["TasksMax"], "infinity")

    def containerfiles(self) -> list[Path]:
        return [
            ROOT / "oci/desktop/Containerfile",
            ROOT / "oci/desktop/Containerfile.cuda",
        ]

    def test_every_non_scratch_base_is_an_exact_amd64_manifest(self) -> None:
        lock = tomllib.loads(
            (ROOT / "oci/base-images.lock.toml").read_text(encoding="utf-8")
        )
        self.assertEqual(lock["schema"], 1)
        self.assertEqual(lock["platform"], "linux/amd64")
        locked = {
            image["reference"]: image["manifest_digest"]
            for image in lock["image"]
        }
        for path in self.containerfiles():
            containerfile = path.read_text(encoding="utf-8")
            from_references = [
                match.group(1)
                for match in re.finditer(r"^FROM\s+(\S+)", containerfile, re.MULTILINE)
                if match.group(1) != "scratch"
            ]
            self.assertEqual(len(from_references), 1, path)
            for reference in from_references:
                name, separator, digest = reference.partition("@")
                self.assertEqual(separator, "@", reference)
                self.assertRegex(digest, r"^sha256:[0-9a-f]{64}$")
                self.assertEqual(digest, locked[name], reference)

    def test_dockerfile_frontend_is_an_exact_amd64_manifest(self) -> None:
        lock = tomllib.loads(
            (ROOT / "oci/base-images.lock.toml").read_text(encoding="utf-8")
        )
        frontend = lock["frontend"]
        for path in self.containerfiles():
            containerfile = path.read_text(encoding="utf-8")
            self.assertEqual(
                containerfile.splitlines()[0],
                f'# syntax={frontend["reference"]}@{frontend["manifest_digest"]}',
                path,
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

    def test_finished_images_retain_a_live_apt_catalogue(self) -> None:
        for path in self.containerfiles():
            containerfile = path.read_text(encoding="utf-8")
            live_source = containerfile.rfind(
                "COPY apt/debian-sid-live.sources"
            )
            self.assertGreaterEqual(live_source, 0, path)
            finished_image = containerfile[live_source:]
            self.assertRegex(
                finished_image,
                r"apt-get(?:\s+-o\s+Acquire::ForceIPv4=true)?\s+update",
                path,
            )
            self.assertNotIn(
                "rm -rf /var/lib/apt/lists/*",
                finished_image,
                path,
            )

        provision = (ROOT / "oci/desktop/provision-image.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn('APT::Periodic::Update-Package-Lists "1";', provision)
        self.assertIn(
            "systemctl enable apt-daily.timer apt-daily-upgrade.timer",
            provision,
        )

    def test_buildah_uses_a_minimal_context_and_discards_its_private_store(self) -> None:
        builder = (ROOT / "oci/build-local.sh").read_text(encoding="utf-8")
        for required in (
            "buildah_local build",
            "--storage-driver vfs",
            "--no-cache",
            "--pull=always",
            'install -d -m 0755 "$context/apt"',
            'variant=${BUZZARDOS_OCI_VARIANT:-standard}',
            'containerfile=Containerfile.cuda',
            'BUZZARDOS_EXPECT_CUDA="$expect_cuda"',
            'rm -rf -- "$context" "$work"',
        ):
            self.assertIn(required, builder)
        for redundant in ("docker ", "podman ", "skopeo", "crane"):
            self.assertNotIn(redundant, builder)
        self.assertNotIn("BUZZARDOS_GUEST_DEB_DIR", builder)
        self.assertNotIn('"$context/debs"', builder)

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
        self.assertIn("https://keyring.openresearchtools.com", containerfile)
        self.assertIn('"buzzardos-guest=${BUZZARDOS_GUEST_VERSION}"', containerfile)
        self.assertIn('"buzzardos-desktop=${BUZZARDOS_DESKTOP_VERSION}"', containerfile)
        self.assertIn('"buzzardoscua=${BUZZARDOS_CUA_VERSION}"', containerfile)
        self.assertIn("OPENRESEARCHTOOLS_KEYRING_SHA256", containerfile)
        self.assertNotIn("COPY debs/", containerfile)
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

    def test_production_startup_has_no_acceptance_probes(self) -> None:
        display = (ROOT / "host/crates/buzzardos/src/display.rs").read_text(
            encoding="utf-8"
        )
        operations = (ROOT / "host/crates/buzzardos/src/operations.rs").read_text(
            encoding="utf-8"
        )
        services = (ROOT / "guest/assets/buzzardos-desktop-services").read_text(
            encoding="utf-8"
        )
        self.assertIn("Uuid::new_v4().simple().to_string()", display)
        self.assertIn("BUZZARDOS_SESSION_TOKEN={session_token}", display)
        self.assertIn('join("desktop-ready")', operations)
        self.assertIn("value.trim() == session_token", operations)
        self.assertIn("mktemp \"$status_dir/.desktop-ready.XXXXXX\"", services)
        self.assertIn("chmod 0644 \"$desktop_ready_tmp\"", services)
        self.assertIn("$BUZZARDOS_SESSION_TOKEN", services)
        self.assertNotIn("health_report", services)
        self.assertNotIn("get_desktop_state", services)
        self.assertNotIn("start_output_sync_supervisor", services)
        self.assertFalse((ROOT / "guest/assets/buzzardos-runtime-ready").exists())
        self.assertFalse((ROOT / "guest/assets/buzzardos-runtime-ready.service").exists())

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

    def test_managed_sway_config_does_not_grab_a_global_keyboard_modifier(self) -> None:
        sway_config = (ROOT / "guest/assets/sway-config").read_text(
            encoding="utf-8"
        )

        self.assertNotRegex(sway_config, r"(?m)^\s*set\s+\$mod\b")
        self.assertNotRegex(sway_config, r"(?m)^\s*floating_modifier\b")
        self.assertNotRegex(
            sway_config,
            r"(?m)^\s*bind(?:code|sym)\b(?![^\n]*\bbutton[1-9]\b)",
        )

    def test_runtime_creates_no_buzzard_logs_or_activity_telemetry(self) -> None:
        host_sources = "\n".join(
            path.read_text(encoding="utf-8")
            for path in (ROOT / "host/crates").rglob("*.rs")
        )
        cua_tools = (ROOT / "cua/src/platform/tools/impl_.rs").read_text(
            encoding="utf-8"
        )
        cua_manifest = (ROOT / "cua/Cargo.toml").read_text(encoding="utf-8")
        update_state = (ROOT / "guest/desktop-core/src/state.rs").read_text(
            encoding="utf-8"
        )
        desktop_shell = (ROOT / "guest/shell/src/main.rs").read_text(
            encoding="utf-8"
        )
        desktop_services = (
            ROOT / "guest/assets/buzzardos-desktop-services"
        ).read_text(encoding="utf-8")
        sway_session = (ROOT / "guest/assets/buzzardos-sway-session").read_text(
            encoding="utf-8"
        )
        integration_agent = (
            ROOT / "guest/assets/buzzardos-integration-agent"
        ).read_text(encoding="utf-8")
        desktop_unit = (
            ROOT / "guest/assets/buzzardos-desktop.service"
        ).read_text(encoding="utf-8")

        for forbidden in (
            "InputStats",
            "input.json",
            "clipboard.json",
            "offload-verification.json",
            "monitor-continuity.json",
            "last_key",
            "last_button",
            "received_events",
            "forwarded_events",
        ):
            self.assertNotIn(forbidden, host_sources)
        for forbidden in (
            "display-gateway.log",
            "BUZZARDOS_KEEP_RUNTIME",
        ):
            self.assertNotIn(forbidden, host_sources)
        self.assertNotIn("BUZZARDOS_CUA_EVIDENCE_DIR", cua_tools)
        self.assertNotIn("tracing =", cua_manifest)
        self.assertNotIn("last_log_id", update_state)
        self.assertNotIn("opened controls for", desktop_shell)
        self.assertNotIn(".log", desktop_services)
        self.assertNotIn(".log", sway_session)
        self.assertNotIn(".log", integration_agent)
        self.assertIn('"$@" >/dev/null 2>&1 &', desktop_services)
        self.assertIn("export PIPEWIRE_DEBUG=0", desktop_services)
        self.assertIn("export SPA_DEBUG=0", desktop_services)
        self.assertIn("export PIPEWIRE_LOG_SYSTEMD=false", desktop_services)
        self.assertIn(">/dev/null 2>&1 &", sway_session)
        self.assertIn("StandardOutput=null", desktop_unit)
        self.assertIn("StandardError=null", desktop_unit)
        self.assertFalse(
            (ROOT / "host/crates/buzzardos-display/src/offload_verifier.rs").exists()
        )

    def test_cuda_variant_is_the_complete_standard_file_plus_one_tail(self) -> None:
        standard = (ROOT / "oci/desktop/Containerfile").read_bytes()
        cuda = (ROOT / "oci/desktop/Containerfile.cuda").read_bytes()
        self.assertTrue(cuda.startswith(standard + b"\n"))
        self.assertNotIn(b"CUDA", standard)

        cuda_tail = cuda[len(standard) :].decode("utf-8")
        for required in (
            "cuda-keyring_",
            "cuda-cudart-13-3=",
            "cuda-compat-13-3=",
            "cuda-libraries-13-3=",
            "libnpp-13-3=",
            "cuda-nvtx-13-3=",
            "libcusparse-13-3=",
            "libcublas-13-3=",
            "libnccl2=",
            "NVIDIA_VISIBLE_DEVICES=all",
            "NVIDIA_DRIVER_CAPABILITIES=compute,utility",
            "NVIDIA_PRODUCT_NAME=CUDA",
            "CUDA_KEYRING_SOURCE_COMMIT=c63770f25b9ece2006956c9be86a72b20c2e67ba",
            "CUDA_KEYRING_LICENSE_SHA256=be0f15ae130d46adb2c2aed7229518da353f28f1471d80b4dce62d909c6ceb2d",
            "/usr/share/doc/cuda-keyring/copyright",
            "/usr/share/doc/cuda-libraries-13-3/copyright",
            "sha256sum --check --strict",
        ):
            self.assertIn(required, cuda_tail)
        for forbidden in ("nvcc", "cuda-toolkit-13-3=", "cuda-drivers"):
            self.assertNotIn(forbidden, cuda_tail)


if __name__ == "__main__":
    unittest.main()
