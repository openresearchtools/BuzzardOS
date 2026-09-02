# SPDX-License-Identifier: AGPL-3.0-or-later

import csv
import io
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
GENERATED = ROOT / "LICENSES/generated"


def inventory_names(path: Path) -> set[str]:
    lines = [line for line in path.read_text().splitlines() if not line.startswith("#")]
    return {row["name"] for row in csv.DictReader(io.StringIO("\n".join(lines)), delimiter="\t")}


class PackageLicenseBoundaryTests(unittest.TestCase):
    def test_each_rust_payload_has_its_own_inventory_and_notice_bundle(self) -> None:
        expected = {
            "buzzardos": "cargo-host.tsv",
            "buzzardos-guest": "cargo-buzzardos-guest.tsv",
            "buzzardos-desktop": "cargo-buzzardos-desktop.tsv",
            "buzzardoscua": "cargo-cua.tsv",
        }
        for package, inventory in expected.items():
            with self.subTest(package=package):
                self.assertTrue((GENERATED / inventory).is_file())
                self.assertTrue(
                    (GENERATED / f"RUST_DEPENDENCY_LICENSES.{package}.txt").is_file()
                )

    def test_guest_mechanics_does_not_inherit_desktop_dependency_graph(self) -> None:
        mechanics = inventory_names(GENERATED / "cargo-buzzardos-guest.tsv")
        desktop = inventory_names(GENERATED / "cargo-buzzardos-desktop.tsv")
        self.assertIn("wl-clipboard-rs", mechanics)
        self.assertNotIn("gtk4", mechanics)
        self.assertIn("gtk4", desktop)
        self.assertGreater(len(desktop), len(mechanics))

    def test_packaging_installs_package_specific_evidence(self) -> None:
        build = (ROOT / "packaging/build-debs.sh").read_text()
        for package in ("buzzardos", "buzzardos-guest", "buzzardos-desktop", "buzzardoscua"):
            self.assertIn(f"packaging/copyright/{package}", build)
            self.assertIn(f"package-notices/{package}.md", build)
            self.assertIn(f"RUST_DEPENDENCY_LICENSES.{package}.txt", build)
        self.assertNotIn('"$project_dir/THIRD_PARTY_NOTICES.md"', build)
        self.assertNotIn("generated/cargo-guest.tsv", build)
        self.assertIn('sha256sum "$filename"', build)

    def test_workflow_checks_portable_sidecars_from_the_output_directory(self) -> None:
        workflow = (ROOT / ".github/workflows/build-release-assets.yml").read_text()
        self.assertIn('cd "$output"', workflow)
        self.assertIn('"$(basename "$package").sha256"', workflow)
        self.assertIn("<cua/VERSION", workflow)
        self.assertIn('license_audit+=(--deb "$package")', workflow)

    def test_host_about_excludes_machine_and_guest_license_claims(self) -> None:
        manager = (
            ROOT / "host/crates/buzzardos-display/src/machine_manager.rs"
        ).read_text()
        self.assertIn("MACHINE_LICENSE_EXCLUSION", manager)
        self.assertIn("machine images or root filesystems", manager)
        self.assertIn("SYSTEM PACKAGES — NOT BUNDLED", manager)
        self.assertIn("generated/cargo-host.tsv", manager)
        self.assertNotIn("cargo-buzzardos-guest.tsv", manager)
        self.assertNotIn("RUST_DEPENDENCY_LICENSES.buzzardoscua", manager)

    def test_package_notices_do_not_claim_apt_dependencies_are_bundled(self) -> None:
        for package in ("buzzardos", "buzzardos-guest", "buzzardos-desktop", "buzzardoscua"):
            notice = (ROOT / f"LICENSES/package-notices/{package}.md").read_text()
            self.assertRegex(notice, r"(?i)not bundled|installed separately|separate packages")

    def test_host_package_ships_recipes_but_not_guest_debs(self) -> None:
        build = (ROOT / "packaging/build-debs.sh").read_text()
        host_block = build.split("build_host() {", 1)[1].split("build_guest() {", 1)[0]
        self.assertIn("containerfiles/desktop/Containerfile", host_block)
        self.assertIn("containerfiles/desktop/Containerfile.cuda", host_block)
        self.assertNotIn("buzzardos-guest_*_amd64.deb", host_block)
        self.assertNotIn("buzzardos-desktop_*_amd64.deb", host_block)
        self.assertNotIn("buzzardoscua_*_amd64.deb", host_block)

    def test_host_package_declares_stock_container_runtime(self) -> None:
        build = (ROOT / "packaging/build-debs.sh").read_text()
        host_notice = (ROOT / "LICENSES/package-notices/buzzardos.md").read_text()
        copyright_record = (ROOT / "packaging/copyright/buzzardos").read_text()
        host_block = build.split("build_host() {", 1)[1].split("build_guest() {", 1)[0]
        self.assertIn("podman", host_block)
        self.assertIn("buildah", host_block)
        self.assertIn("host executables", copyright_record)
        self.assertIn("Podman, Buildah", host_notice)


if __name__ == "__main__":
    unittest.main()
