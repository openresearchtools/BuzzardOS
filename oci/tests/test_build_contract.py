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
        self.assertEqual(len(from_references), 6)
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
        trixie = (
            ROOT / "oci/desktop/apt/debian-trixie-snapshot.sources"
        ).read_text(encoding="utf-8")
        self.assertIn(snapshots["docker.io/library/debian:sid"], sid)
        self.assertIn(snapshots["docker.io/library/rust:1.96-slim"], trixie)
        for sources in (sid, trixie):
            self.assertIn("snapshot.debian.org", sources)
            self.assertNotIn("deb.debian.org", sources)
            self.assertIn("Check-Valid-Until: no", sources)
        live = (ROOT / "oci/desktop/apt/debian-sid-live.sources").read_text(
            encoding="utf-8"
        )
        self.assertIn("http://deb.debian.org/debian", live)

    def test_docker_context_contains_every_repository_copy_input(self) -> None:
        rules = {
            line.strip()
            for line in (ROOT / ".dockerignore").read_text(encoding="utf-8").splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        }
        self.assertIn("**", rules)
        for file_name in (
            ".dockerignore",
            "LICENSE",
            "NOTICE",
            "THIRD_PARTY_NOTICES.md",
        ):
            self.assertIn(f"!{file_name}", rules)
        for directory in ("oci", "LICENSES"):
            self.assertIn(f"!{directory}/", rules)
            self.assertIn(f"!{directory}/**", rules)
        self.assertIn("!guest/", rules)
        for guest_input in (
            "Cargo.toml",
            "Cargo.lock",
            "ASSET_REVISION",
            "asset-manifest.tsv",
            "install-rootfs-assets.sh",
        ):
            self.assertIn(f"!guest/{guest_input}", rules)
        for guest_directory in (
            "clipboard-agent",
            "desktop-core",
            "settings",
            "shell",
            "shortcut-helper",
            "assets",
            "updater",
            "third_party/trycua-cua",
        ):
            self.assertIn(f"!guest/{guest_directory}/", rules)
            self.assertIn(f"!guest/{guest_directory}/**", rules)
        self.assertIn("!clipboard-protocol/", rules)
        self.assertIn("!clipboard-protocol/**", rules)
        self.assertIn("!tools/", rules)
        self.assertIn("!tools/fetch-mpl-sources.sh", rules)
        self.assertFalse(any(rule.startswith("!host/") for rule in rules))

    def test_compose_uses_only_the_local_reference_image_target(self) -> None:
        compose = (ROOT / "oci/compose.yaml").read_text(encoding="utf-8")
        self.assertIn("context: ..", compose)
        self.assertIn("dockerfile: oci/desktop/Containerfile", compose)
        self.assertIn("linux/amd64", compose)
        for publishing_key in ("push:", "registry:", "pull_policy: always"):
            self.assertNotIn(publishing_key, compose)

    def test_final_sway_payload_excludes_development_files(self) -> None:
        containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )
        self.assertIn('"/sway-root$runtime_prefix/include"', containerfile)
        self.assertIn('"/sway-root$runtime_prefix/lib/pkgconfig"', containerfile)
        self.assertIn("-name '*.a'", containerfile)
        self.assertIn("-name '*.la'", containerfile)
        self.assertIn("-Wl,-rpath,$ORIGIN/../lib", containerfile)
        self.assertIn("AS sway-runtime-artifact", containerfile)

    def test_sway_runtime_carries_one_pinned_normalized_xkb_tree(self) -> None:
        containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )
        self.assertRegex(
            containerfile,
            r"(?m)^\s+xkb-data \\$",
        )
        for required in (
            "xkb_library=$(readlink -f /usr/lib/x86_64-linux-gnu/libxkbcommon.so.0)",
            "/runtime-payload/lib/libxkbcommon.so.0",
            "libxkbcommon0.manifest.sha256",
            "libxkbcommon0.version",
            "/runtime-payload/share/doc/libxkbcommon0/copyright",
            "xkb_entry=/usr/share/X11/xkb",
            'xkb_source=$(readlink -f -- "$xkb_entry")',
            "/usr/share/xkeyboard-config-[0-9]*",
            "xkb_destination=/runtime-payload/share/X11/xkb",
            'case "$resolved" in',
            'cp -aL "$xkb_source" "$xkb_destination"',
            "xkb-data.manifest.sha256",
            "xkb-data.version",
            "/runtime-payload/share/doc/xkb-data/copyright",
        ):
            self.assertIn(required, containerfile)
        self.assertIn(
            '! find "$xkb_destination" -type l -print -quit | grep -q .',
            containerfile,
        )
        self.assertIn(
            '! find "$xkb_destination" -mindepth 1 ! -type d ! -type f',
            containerfile,
        )

    def test_portable_app_stages_the_same_pinned_xkb_tree_at_a_stable_path(self) -> None:
        packager = (ROOT / "host/build-portable-app.sh").read_text(encoding="utf-8")
        for required in (
            'host_xkb_root="$appdir/usr/share/wildbuzzard/xkb"',
            '"$guest_compositor_runtime/share/X11/xkb/."',
            '"$appdir/usr/share/wildbuzzard/xkb-data.manifest.sha256"',
            '"$appdir/usr/share/wildbuzzard/xkb-data.version"',
            '"$appdir/usr/share/doc/xkb-data/copyright"',
            '"$appdir/usr/lib/libxkbcommon.so.0"',
            '"$appdir/usr/share/wildbuzzard/libxkbcommon0.manifest.sha256"',
            '"$appdir/usr/share/doc/libxkbcommon0/copyright"',
            '"$guest_runtime_destination/$guest_revision/share/X11/xkb"',
            "host and guest pinned XKB manifests differ",
            "host and guest pinned libxkbcommon payloads differ",
            "verify_elf_relocation_closure",
            'ldd -r -- "$object"',
            "undefined symbol|relocation error|symbol lookup error",
            "gtk_builder_lib=$(pkg-config --variable=libdir gtk4)",
            'cargo_rustflags="-L native=$gtk_builder_lib',
        ):
            self.assertIn(required, packager)
        self.assertIn("verify_xkb_payload", packager)
        self.assertIn("followlinks=False", packager)
        self.assertNotIn("cp -a -- /usr/share/X11/xkb", packager)
        verifier = (ROOT / "oci/verify-image.sh").read_text(encoding="utf-8")
        self.assertIn('ldd -r -- "$runtime/lib/libxkbcommon.so.0"', verifier)
        self.assertIn("undefined symbol|relocation error|symbol lookup error", verifier)

    def test_runtime_contract_names_desktop_and_appimage_dependencies(self) -> None:
        containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )
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
            self.assertRegex(
                containerfile,
                rf"(?m)^\s+{re.escape(package)} (?:\\|&&)",
            )
        for package in (
            "chromium",
            "dolphin",
            "mesa-utils",
            "pavucontrol",
            "vulkan-tools",
            "x11-apps",
            "xterm",
        ):
            self.assertNotRegex(
                containerfile,
                rf"(?m)^\s+{re.escape(package)} (?:\\|&&)",
            )
        verifier = (ROOT / "oci/verify-image.sh").read_text(encoding="utf-8")
        self.assertRegex(verifier, r"(?m)^\s+gsettings(?:\s|\\)")
        self.assertIn("dconf-gsettings-backend", verifier)
        self.assertIn("gsettings-desktop-schemas", verifier)
        self.assertIn("gsettings list-keys org.gnome.desktop.interface", verifier)
        self.assertIn("gsettings set org.gnome.desktop.interface gtk-theme", verifier)
        self.assertIn("gsettings get org.gnome.desktop.interface gtk-theme", verifier)
        self.assertIn("AS settings-builder", containerfile)
        self.assertRegex(containerfile, r"(?m)^\s+libglib2\.0-dev \\")
        self.assertRegex(containerfile, r"(?m)^\s+libpulse-dev \\")
        self.assertIn("--package wildbuzzard-settings", containerfile)
        self.assertIn("--package wildbuzzard-shortcut-helper", containerfile)
        self.assertIn("--package wildbuzzard-clipboard-agent", containerfile)
        self.assertIn("/usr/libexec/wildbuzzard-shortcut-helper", verifier)
        self.assertIn("/libexec/wildbuzzard-clipboard-agent", verifier)
        self.assertIn("unsquashfs", verifier)
        self.assertIn("cargo clippy", containerfile)
        self.assertIn("cargo test", containerfile)
        self.assertIn("libpulse.so.0", verifier)
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
        broker = (ROOT / "host/crates/wildbuzzard-broker/src/main.rs").read_text(
            encoding="utf-8"
        )
        services = (ROOT / "guest/assets/wildbuzzard-desktop-services").read_text(
            encoding="utf-8"
        )
        readiness = (ROOT / "guest/assets/wildbuzzard-runtime-ready").read_text(
            encoding="utf-8"
        )
        unit = (ROOT / "guest/assets/wildbuzzard-runtime-ready.service").read_text(
            encoding="utf-8"
        )
        self.assertIn('"WILDBUZZARD_SESSION_TOKEN".into()', broker)
        self.assertIn("Uuid::new_v4().simple().to_string()", broker)
        self.assertIn("desktop_ready_for_session(marker, expected_session_token)", broker)
        self.assertIn("const GUEST_RUNTIME_MODE: u32 = 0o700", broker)
        self.assertIn("mktemp \"$status_dir/.desktop-ready.XXXXXX\"", services)
        self.assertIn("chmod 0600 \"$desktop_ready_tmp\"", services)
        self.assertIn("$WILDBUZZARD_SESSION_TOKEN", services)
        self.assertIn("EnvironmentFile=/run/wildbuzzard-host/driver.env", unit)
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
        )[1].split("COPY --from=sway-builder", 1)[0]
        self.assertNotIn("apt-get", cuda_section)


if __name__ == "__main__":
    unittest.main()
