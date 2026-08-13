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
        cls.rootfs_builder = (ROOT / "tools/build-release-rootfs.sh").read_text(
            encoding="utf-8"
        )
        cls.assembler = (ROOT / "tools/assemble-release-assets.sh").read_text(
            encoding="utf-8"
        )
        cls.dependency_installer = (
            ROOT / "host/packaging/Install-Dependencies"
        ).read_text(encoding="utf-8")
        cls.portable_builder = (ROOT / "host/build-portable-app.sh").read_text(
            encoding="utf-8"
        )
        cls.guest_license_readme = (
            ROOT / "tools/release/guest-rootfs-licenses.README.md"
        ).read_text(encoding="utf-8")

    def test_workflow_is_manual_read_only_and_uploads_one_artifact(self) -> None:
        self.assertRegex(
            self.workflow,
            r"(?m)^on:\n  workflow_dispatch:\n\npermissions:\n  contents: read$",
        )
        self.assertEqual(len(re.findall(r"(?m)^\s*permissions:\s*$", self.workflow)), 1)
        jobs = self.workflow.split("\njobs:\n", maxsplit=1)[1]
        self.assertEqual(
            re.findall(r"(?m)^  ([A-Za-z0-9_-]+):\s*$", jobs), ["build-portable"]
        )
        self.assertEqual(self.workflow.count("actions/upload-artifact@"), 1)
        self.assertIn(
            "name: BuzzardOS-portable-linux-x86_64.tar.xz", self.workflow
        )
        self.assertIn("path: |", self.workflow)
        self.assertIn(
            "${{ runner.temp }}/buzzardos-release/BuzzardOS-portable-linux-x86_64.tar.xz",
            self.workflow,
        )
        self.assertIn(
            "${{ runner.temp }}/buzzardos-release/BuzzardOS-portable-linux-x86_64.tar.xz.sha256",
            self.workflow,
        )

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
            r"(?i)ghcr\.io",
            r"\$\{\{\s*(secrets\.|github\.token)",
            r"(?i)\b(write-all|[a-z-]+:\s*write)\b",
        ):
            self.assertNotRegex(self.workflow, forbidden)
        self.assertIn("--provenance=false --sbom=false", self.workflow)
        self.assertIn("tools/build-release-rootfs.sh", self.workflow)
        self.assertIn("tools/assemble-release-assets.sh", self.workflow)

    def test_extracted_layout_and_oci_seed_are_built_and_checked(self) -> None:
        for required in (
            "host/build-portable-app.sh",
            'mv "$RUNNER_TEMP/buzzardos-app-output/app" "$RUNNER_TEMP/BuzzardOS/app"',
            "host/packaging/BuzzardOS",
            "host/packaging/Install-Dependencies",
            "BuzzardOS-portable-linux-x86_64.tar.xz",
            "xz -t",
            "compression-level: 0",
        ):
            self.assertIn(required, self.workflow)
        self.assertIn("default-rootfs.oci.tar.zst", self.rootfs_builder)
        self.assertIn("skopeo copy", self.rootfs_builder)
        self.assertIn("xz -9e -T0", self.assembler)
        self.assertIn(
            "--pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime",
            self.assembler,
        )
        self.assertIn("verify-portable-release-inputs.py", self.assembler)
        self.assertIn("verify-elf-glibc-floor.py", self.assembler)
        self.assertIn("--maximum 2.39", self.assembler)
        self.assertIn("PAYLOAD_SHA256SUMS", self.assembler)
        self.assertIn("! find Machines shared -mindepth 1", self.assembler)
        self.assertIn('"$bundle/app/licenses/host/usr-share-doc"', self.assembler)
        self.assertLess(
            self.assembler.index('"$bundle/app/licenses/host/usr-share-doc"'),
            self.assembler.index(
                'python3 "$project_dir/tools/release_metadata.py" materialize'
            ),
        )
        self.assertIn("`../../provenance/guest/`", self.guest_license_readme)
        self.assertNotIn("`../../provenance/guest-rootfs/`", self.guest_license_readme)

    def test_dependency_installer_handles_ubuntu_userns_policy_without_disabling_it(
        self,
    ) -> None:
        self.assertIn(
            "/proc/sys/kernel/apparmor_restrict_unprivileged_userns",
            self.dependency_installer,
        )
        self.assertIn("uidmap lxc", self.dependency_installer)
        self.assertIn("/usr/bin/lxc-usernsexec", self.dependency_installer)
        self.assertIn('-m "u:1000:$uid:1"', self.dependency_installer)
        self.assertIn('-m "g:1000:$gid:1"', self.dependency_installer)
        self.assertNotIn(
            "apparmor_restrict_unprivileged_userns=0", self.dependency_installer
        )
        self.assertNotIn(
            "apparmor_restrict_unprivileged_userns = 0", self.dependency_installer
        )

    def test_runner_uses_the_trusted_ubuntu_userns_gate(self) -> None:
        self.assertIn(
            "acl attr jq lxc rsync skopeo uidmap xz-utils zstd", self.workflow
        )
        self.assertNotIn(
            "apparmor_restrict_unprivileged_userns=0", self.workflow
        )
        self.assertNotIn(
            "apparmor_restrict_unprivileged_userns = 0", self.workflow
        )

    def test_export_tar_is_pinned_to_the_oldest_supported_glibc(self) -> None:
        self.assertIn("tar_package_version=1.34+dfsg-1+deb11u1", self.portable_builder)
        self.assertIn("tar_binary_sha256=8498b0a43e820b0f", self.portable_builder)
        self.assertIn('"$tar_runtime_dir/tar.real"', self.portable_builder)
        self.assertIn('"$tar_library_dir/libacl.so.1"', self.portable_builder)
        self.assertIn("LICENSES/tar-runtime-sources.tsv", self.portable_builder)
        self.assertNotIn(
            'install -m755 "$(command -v tar)" "$appdir/usr/libexec/wildbuzzard/tar"',
            self.portable_builder,
        )
        self.assertIn("verify-elf-glibc-floor.py", self.portable_builder)
        self.assertIn("--maximum 2.39", self.portable_builder)


if __name__ == "__main__":
    unittest.main()
