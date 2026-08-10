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

    def test_workflow_has_only_manual_artifact_outputs(self) -> None:
        self.assertRegex(
            self.workflow,
            r"(?m)^on:\n  workflow_dispatch:\n\npermissions: \{\}$",
        )
        self.assertEqual(self.workflow.count("actions/upload-artifact@"), 2)
        self.assertIn("name: WildBuzzard-x86_64.AppImage", self.workflow)
        self.assertIn(
            "name: WildBuzzard-portable-x86_64.tar.zst", self.workflow
        )
        self.assertEqual(self.workflow.count("contents: read"), 3)
        for forbidden in (
            "contents: write",
            "packages: write",
            "gh release",
            "docker push",
            "ghcr.io",
            "softprops/action-gh-release",
            "actions/create-release",
        ):
            self.assertNotIn(forbidden, self.workflow)

    def test_runner_userns_allowance_is_exact_temporary_and_verified(self) -> None:
        for required in (
            "^/opt/wildbuzzard-actions-[0-9]+-[0-9]+$",
            "apparmor_restrict_unprivileged_userns)\" = 1",
            "install -o root -g root -m 0555",
            '"$APPIMAGE_SHA256"',
            "flags=(unconfined)",
            "'  userns,'",
            "sudo apparmor_parser -K -a \"$CI_APPARMOR_POLICY\"",
            '--storage-dir "$doctor_storage" doctor',
            "full UID/GID mapping: yes",
            "sudo apparmor_parser -K -R \"$CI_APPARMOR_POLICY\"",
            "if: ${{ always() }}",
        ):
            self.assertIn(required, self.workflow)
        self.assertNotIn(
            "apparmor_restrict_unprivileged_userns=0", self.workflow
        )
        self.assertNotRegex(self.workflow, re.compile(r"sudo\s+.*\bcreate\b"))


if __name__ == "__main__":
    unittest.main()
