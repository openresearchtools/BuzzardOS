#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later

import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github" / "workflows" / "build-release-assets.yml"


class ActionsArtifactWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.workflow = WORKFLOW.read_text(encoding="utf-8")
        cls.packager = (ROOT / "packaging/build-debs.sh").read_text(
            encoding="utf-8"
        )
        cls.host_matrix = (ROOT / "tools/test-host-package-matrix.sh").read_text(
            encoding="utf-8"
        )
        cls.containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )

    def test_workflow_is_manual_and_tag_driven_with_release_permission(self) -> None:
        self.assertIn("workflow_dispatch:", self.workflow)
        self.assertIn("push:\n    tags:\n      - 'v*'", self.workflow)
        self.assertIn("permissions:\n  contents: write", self.workflow)
        self.assertEqual(len(re.findall(r"(?m)^\s*permissions:\s*$", self.workflow)), 1)
        jobs = self.workflow.split("\njobs:\n", maxsplit=1)[1]
        self.assertEqual(re.findall(r"(?m)^  ([A-Za-z0-9_-]+):\s*$", jobs), ["build"])
        self.assertEqual(self.workflow.count("actions/upload-artifact@"), 1)
        self.assertIn("name: BuzzardOS-debian-packages-amd64", self.workflow)
        self.assertIn("${{ runner.temp }}/buzzardos-debs/*.deb", self.workflow)
        self.assertIn("${{ runner.temp }}/buzzardos-debs/*.deb.sha256", self.workflow)
        self.assertIn("retention-days: 7", self.workflow)
        self.assertIn("compression-level: 0", self.workflow)

    def test_workflow_publishes_release_assets_but_no_registry_output(self) -> None:
        action_uses = re.findall(r"(?m)^\s*uses:\s*([^\s#]+)", self.workflow)
        self.assertEqual(
            action_uses,
            [
                "actions/checkout@11d5960a326750d5838078e36cf38b85af677262",
                "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02",
            ],
        )
        self.assertIn('gh release create "$GITHUB_REF_NAME"', self.workflow)
        self.assertIn('gh release upload "$GITHUB_REF_NAME"', self.workflow)
        self.assertIn("test \"${#assets[@]}\" -eq 8", self.workflow)
        for forbidden in (
            r"(?im)^\s*(pull_request|schedule|workflow_run|workflow_call|registry_package|deployment|page_build)\s*:",
            r"(?im)^\s*(environment|packages|pages|deployments|id-token)\s*:",
            r"(?i)\bgit\s+push\b",
            r"(?i)\b(docker|podman)\s+(login|push)\b",
            r"(?i)\boras\s+push\b",
            r"(?i)--push\b",
            r"(?i)(api|uploads)\.github\.com",
            r"\$\{\{\s*secrets\.",
            r"(?i)\b(write-all)\b",
        ):
            self.assertNotRegex(self.workflow, forbidden)
        self.assertIn("./oci/build-local.sh", self.workflow)
        self.assertNotIn("host/build-portable-app.sh", self.workflow)
        self.assertNotIn("assemble-release-assets.sh", self.workflow)

    def test_four_versioned_debs_are_built_and_checked(self) -> None:
        for required in (
            "packaging/build-debs.sh all",
            'buzzardos_${version}_amd64.deb',
            'buzzardos-guest_${guest_version}_amd64.deb',
            'buzzardos-desktop_${desktop_version}_amd64.deb',
            'buzzardoscua_${cua_version}_amd64.deb',
            "dpkg-deb --info",
            "dpkg-deb --contents",
            "sha256sum --check --strict",
        ):
            self.assertIn(required, self.workflow)
        self.assertIn("test-host-package-matrix.sh", self.workflow)
        self.assertIn("buzzardos --version", self.host_matrix)
        self.assertIn("$project_dir/VERSION", self.packager)
        self.assertIn("Package: $package", self.packager)
        self.assertIn("Depends: $depends", self.packager)

    def test_reference_oci_consumes_packages_and_stock_sway(self) -> None:
        self.assertNotIn("packaging/build-debs.sh", self.containerfile)
        self.assertIn('"buzzardos-guest=${BUZZARDOS_GUEST_VERSION}"', self.containerfile)
        self.assertIn('"buzzardos-desktop=${BUZZARDOS_DESKTOP_VERSION}"', self.containerfile)
        self.assertIn('"buzzardoscua=${BUZZARDOS_CUA_VERSION}"', self.containerfile)
        self.assertIn("https://keyring.openresearchtools.com", self.containerfile)
        self.assertNotIn("BUZZARDOS_GUEST_DEB_DIR", self.workflow)
        self.assertNotIn("COPY debs/", self.containerfile)
        for forbidden in ("git clone", "meson setup", "wlroots.git", "sway.git"):
            self.assertNotIn(forbidden, self.containerfile)


if __name__ == "__main__":
    unittest.main()
