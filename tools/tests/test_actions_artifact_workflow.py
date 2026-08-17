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
        cls.containerfile = (ROOT / "oci/desktop/Containerfile").read_text(
            encoding="utf-8"
        )

    def test_workflow_is_manual_read_only_and_uploads_one_artifact(self) -> None:
        self.assertRegex(
            self.workflow,
            r"(?m)^on:\n  workflow_dispatch:\n\npermissions:\n  contents: read$",
        )
        self.assertEqual(len(re.findall(r"(?m)^\s*permissions:\s*$", self.workflow)), 1)
        jobs = self.workflow.split("\njobs:\n", maxsplit=1)[1]
        self.assertEqual(re.findall(r"(?m)^  ([A-Za-z0-9_-]+):\s*$", jobs), ["build"])
        self.assertEqual(self.workflow.count("actions/upload-artifact@"), 1)
        self.assertIn("name: BuzzardOS-debian-packages-amd64", self.workflow)
        self.assertIn("${{ runner.temp }}/buzzardos-debs/*.deb", self.workflow)
        self.assertIn("${{ runner.temp }}/buzzardos-debs/*.deb.sha256", self.workflow)
        self.assertIn("retention-days: 7", self.workflow)
        self.assertIn("compression-level: 0", self.workflow)

    def test_workflow_has_no_publisher_or_registry_output(self) -> None:
        action_uses = re.findall(r"(?m)^\s*uses:\s*([^\s#]+)", self.workflow)
        self.assertEqual(
            action_uses,
            [
                "actions/checkout@11d5960a326750d5838078e36cf38b85af677262",
                "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02",
            ],
        )
        for forbidden in (
            r"(?im)^\s*(push|pull_request|schedule|workflow_run|workflow_call|release|registry_package|deployment|page_build)\s*:",
            r"(?im)^\s*(environment|packages|pages|deployments|id-token)\s*:",
            r"(?i)\bgh\s+(release|api)\b",
            r"(?i)\bgit\s+push\b",
            r"(?i)\b(docker|podman)\s+(login|push)\b",
            r"(?i)\boras\s+push\b",
            r"(?i)--push\b",
            r"(?i)(api|uploads)\.github\.com",
            r"\$\{\{\s*(secrets\.|github\.token)",
            r"(?i)\b(write-all|[a-z-]+:\s*write)\b",
        ):
            self.assertNotRegex(self.workflow, forbidden)
        self.assertIn("./oci/build-local.sh", self.workflow)
        self.assertNotIn("host/build-portable-app.sh", self.workflow)
        self.assertNotIn("assemble-release-assets.sh", self.workflow)

    def test_three_versioned_debs_are_built_and_checked(self) -> None:
        for required in (
            "packaging/build-debs.sh all",
            'buzzardos_${version}_amd64.deb',
            'buzzardos-guest-desktop_${version}_amd64.deb',
            'buzzardcua_${cua_version}_amd64.deb',
            "dpkg-deb --info",
            "dpkg-deb --contents",
            "sha256sum --check --strict",
            "buzzardos --version",
        ):
            self.assertIn(required, self.workflow)
        self.assertIn("$project_dir/VERSION", self.packager)
        self.assertIn("Package: $package", self.packager)
        self.assertIn("Depends: $depends", self.packager)

    def test_reference_oci_consumes_packages_and_stock_sway(self) -> None:
        self.assertIn("packaging/build-debs.sh guest cua", self.containerfile)
        self.assertIn("sway", self.containerfile)
        self.assertIn("buzzardos-guest-desktop_", self.containerfile)
        self.assertIn("buzzardcua_", self.containerfile)
        for forbidden in ("git clone", "meson setup", "wlroots.git", "sway.git"):
            self.assertNotIn(forbidden, self.containerfile)


if __name__ == "__main__":
    unittest.main()
