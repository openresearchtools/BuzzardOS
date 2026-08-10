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
        self.assertEqual(len(from_references), 5)
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
        for guest_directory in ("shell", "assets", "third_party/trycua-cua"):
            self.assertIn(f"!guest/{guest_directory}/", rules)
            self.assertIn(f"!guest/{guest_directory}/**", rules)
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
        self.assertIn("/sway-root/usr/include", containerfile)
        self.assertIn("/sway-root/usr/lib/x86_64-linux-gnu/pkgconfig", containerfile)
        self.assertIn("-name '*.a'", containerfile)
        self.assertIn("-name '*.la'", containerfile)

    def test_runtime_contract_names_desktop_and_appimage_dependencies(self) -> None:
        containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )
        for package in (
            "ffmpeg",
            "firefox-esr",
            "foot",
            "fuse3",
            "libfuse2t64",
            "libgbm1",
            "libglib2.0-bin",
            "libgtk-3-0t64",
            "libnss3",
            "libxkbcommon0",
            "mousepad",
            "thunar",
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
        self.assertIn("gsettings set org.gnome.desktop.interface gtk-theme", verifier)
        self.assertIn("gsettings get org.gnome.desktop.interface gtk-theme", verifier)
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
