#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import os
import shutil
import struct
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "release_metadata", ROOT / "tools/release_metadata.py"
)
assert SPEC is not None and SPEC.loader is not None
release_metadata = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_metadata)


def compress_tar(destination: Path, members: list[tarfile.TarInfo], payloads: dict[str, bytes]) -> None:
    plain = destination.parent / f".{destination.name}.plain"
    with tarfile.open(plain, "w", format=tarfile.PAX_FORMAT) as archive:
        for member in members:
            payload = payloads.get(member.name)
            archive.addfile(member, io.BytesIO(payload) if payload is not None else None)
    subprocess.run(
        [
            "zstd",
            "-q",
            "-f",
            "-19",
            "--long=27",
            str(plain),
            "-o",
            str(destination),
        ],
        check=True,
    )
    plain.unlink()


def member(name: str, kind: bytes, size: int = 0, owner: int = 0) -> tarfile.TarInfo:
    value = tarfile.TarInfo(name)
    value.type = kind
    value.size = size
    value.uid = owner
    value.gid = owner
    value.mode = 0o755 if kind == tarfile.DIRTYPE else 0o644
    value.mtime = 1
    return value


class RootfsArchiveTests(unittest.TestCase):
    def test_archive_inventory_includes_hardlinks(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "rootfs.tar.zst"
            root = member(".", tarfile.DIRTYPE)
            regular = member("./file", tarfile.REGTYPE, 3)
            hardlink = member("./other", tarfile.LNKTYPE)
            hardlink.linkname = "./file"
            symlink = member("./link", tarfile.SYMTYPE)
            symlink.linkname = "/file"
            compress_tar(archive, [root, regular, hardlink, symlink], {"./file": b"abc"})

            record = release_metadata.inspect_zstd_archive(archive)
            self.assertEqual(
                record["tree"]["counts"],
                {"directories": 0, "regular_files": 2, "symlinks": 1, "other": 0},
            )
            self.assertEqual(record["tree"]["regular_file_bytes"], 6)
            self.assertEqual(record["tree"]["hardlink_groups"], 1)
            self.assertEqual(record["tree"]["owners"], [[0, 0]])

    def test_archive_rejects_parent_escape(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "unsafe.tar.zst"
            escape = member("../escape", tarfile.REGTYPE, 1)
            compress_tar(
                archive,
                [member(".", tarfile.DIRTYPE), escape],
                {"../escape": b"x"},
            )
            with self.assertRaisesRegex(release_metadata.MetadataError, "escapes"):
                release_metadata.inspect_zstd_archive(archive)

    def test_archive_accepts_literal_backslash_in_posix_name(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "systemd-name.tar.zst"
            systemd_name = (
                r"./usr/lib/systemd/system/"
                r"system-systemd\x2dmute\x2dconsole.slice"
            )
            entry = member(systemd_name, tarfile.REGTYPE, 1)
            compress_tar(
                archive,
                [member(".", tarfile.DIRTYPE), entry],
                {systemd_name: b"x"},
            )

            record = release_metadata.inspect_zstd_archive(archive)
            self.assertEqual(record["tree"]["counts"]["regular_files"], 1)

    def test_archive_rejects_special_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "fifo.tar.zst"
            compress_tar(
                archive,
                [member(".", tarfile.DIRTYPE), member("./fifo", tarfile.FIFOTYPE)],
                {},
            )
            with self.assertRaisesRegex(release_metadata.MetadataError, "socket/device/FIFO"):
                release_metadata.inspect_zstd_archive(archive)

    def test_archive_rejects_subordinate_host_owner(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "owner.tar.zst"
            compress_tar(archive, [member(".", tarfile.DIRTYPE, owner=100000)], {})
            with self.assertRaisesRegex(release_metadata.MetadataError, "non-canonical owner"):
                release_metadata.inspect_zstd_archive(archive)


class RootfsTreeTests(unittest.TestCase):
    def test_guest_asset_installer_matches_release_rootfs_contract(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            temporary_path = Path(temporary)
            rootfs = temporary_path / "rootfs"
            rootfs.mkdir()
            for relative in [
                "lib/systemd/systemd",
                "usr/bin/sway",
                "var/lib/dpkg/status",
            ]:
                path = rootfs / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"fixture")

            shell = temporary_path / "wildbuzzard-shell"
            cua_driver = temporary_path / "cua-driver"
            for executable in [shell, cua_driver]:
                executable.write_bytes(b"fixture")
                executable.chmod(0o755)
            subprocess.run(
                [
                    str(ROOT / "guest/install-rootfs-assets.sh"),
                    str(rootfs),
                    str(shell),
                    str(cua_driver),
                ],
                check=True,
            )

            canonical_rootfs = rootfs.resolve()
            original_lstat = Path.lstat

            def canonical_owner(path: Path) -> os.stat_result:
                metadata = original_lstat(path)
                if path == canonical_rootfs:
                    fields = list(metadata)
                    fields[4] = 0
                    fields[5] = 0
                    return os.stat_result(fields)
                return metadata

            # Release assembly runs this inspection as root. Keep the unit
            # test unprivileged while modeling only the canonical root owner.
            with mock.patch.object(Path, "lstat", canonical_owner):
                record = release_metadata.inspect_rootfs(rootfs)

            self.assertGreater(record["counts"]["regular_files"], 0)
            self.assertTrue((rootfs / "usr/libexec/wildbuzzard-shell").is_file())
            self.assertTrue((rootfs / "usr/local/bin/cua-driver").is_file())
            self.assertFalse((rootfs / "usr/bin/wildbuzzard-shell").exists())
            self.assertFalse((rootfs / "usr/bin/wildbuzzard-cua-driver").exists())


class BundleInventoryTests(unittest.TestCase):
    def test_inventory_rejects_symlink(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "file").write_text("data", encoding="utf-8")
            (root / "link").symlink_to("file")
            with self.assertRaisesRegex(release_metadata.MetadataError, "symlink"):
                release_metadata.bundle_inventory(root, set())

    def test_checksum_file_is_sorted_and_detects_change(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "z").write_text("last", encoding="utf-8")
            (root / "a").write_text("first", encoding="utf-8")
            expected = release_metadata.checksum_contents(root)
            release_metadata.write_bundle_checksums(
                type("Arguments", (), {"root": root})()
            )
            self.assertEqual((root / "SHA256SUMS").read_bytes(), expected)
            self.assertLess(expected.index(b"  a\n"), expected.index(b"  z\n"))
            (root / "a").write_text("changed", encoding="utf-8")
            self.assertNotEqual((root / "SHA256SUMS").read_bytes(), release_metadata.checksum_contents(root))

    def test_complete_bundle_manifest_rejects_extra_file(self) -> None:
        commit = subprocess.check_output(
            ["git", "-C", str(ROOT), "rev-parse", "HEAD^{commit}"], text=True
        ).strip()
        with tempfile.TemporaryDirectory() as temporary:
            bundle = Path(temporary) / "WildBuzzard"
            for relative in [
                "runtime",
                "licenses/appimage/usr-share-doc",
                "licenses/guest-rootfs/usr-share-common-licenses",
                "licenses/guest-rootfs/usr-share-doc",
                "licenses/guest-rootfs/project-source",
                "provenance/appimage",
                "provenance/guest-rootfs",
                "vm",
                "shared",
                "cache",
            ]:
                (bundle / relative).mkdir(parents=True, exist_ok=True)
            (bundle / "README.md").write_text("bundle\n", encoding="utf-8")
            (bundle / "licenses/appimage/README.md").write_text("host\n", encoding="utf-8")
            (bundle / "licenses/guest-rootfs/README.md").write_text("guest\n", encoding="utf-8")
            appimage = bundle / release_metadata.APPIMAGE_NAME
            appimage.write_bytes(b"fake-appimage")
            appimage.chmod(0o755)

            rootfs_archive = bundle / "runtime" / release_metadata.ROOTFS_ARCHIVE_NAME
            compress_tar(rootfs_archive, [member(".", tarfile.DIRTYPE)], {})
            rootfs_record = release_metadata.inspect_zstd_archive(rootfs_archive)
            provenance_mapping = {
                "oci/base-images.lock.toml": "base-images.lock.toml",
                "oci/desktop/SWAY_UPSTREAM.toml": "SWAY_UPSTREAM.toml",
                "guest/third_party/trycua-cua/UPSTREAM.toml": "TRYCUA_UPSTREAM.toml",
                "guest/third_party/trycua-cua/CHANGES.WILDBUZZARD.md": "TRYCUA_CHANGES.WILDBUZZARD.md",
                "LICENSES/release-components.toml": "release-components.toml",
                "LICENSES/generated/oci-packages.tsv": "oci-packages.tsv",
            }
            provenance_records = []
            for source_path, name in provenance_mapping.items():
                destination = bundle / "provenance/guest-rootfs" / name
                destination.write_text(f"{name}\n", encoding="utf-8")
                provenance_records.append(
                    {
                        "path": source_path,
                        "size": destination.stat().st_size,
                        "sha256": release_metadata.sha256_file(destination),
                    }
                )
            rootfs_manifest = {
                "schema": 1,
                "kind": "wildbuzzard-flat-rootfs",
                "platform": {"os": "linux", "architecture": "amd64"},
                "source": {"commit": commit},
                "archive": rootfs_record,
                "provenance_files": provenance_records,
            }
            runtime_manifest = bundle / "runtime" / release_metadata.ROOTFS_MANIFEST_NAME
            runtime_manifest.write_text(json.dumps(rootfs_manifest) + "\n", encoding="utf-8")
            shutil.copyfile(
                runtime_manifest,
                bundle / "provenance/guest-rootfs" / release_metadata.ROOTFS_MANIFEST_NAME,
            )
            (bundle / "provenance/guest-rootfs/ROOTFS_SHA256SUMS").write_text(
                f"{release_metadata.sha256_file(rootfs_archive)}  runtime/{release_metadata.ROOTFS_ARCHIVE_NAME}\n"
                f"{release_metadata.sha256_file(runtime_manifest)}  runtime/{release_metadata.ROOTFS_MANIFEST_NAME}\n",
                encoding="utf-8",
            )
            appimage_manifest = {
                "schema": 1,
                "kind": "wildbuzzard-appimage",
                "platform": {"os": "linux", "architecture": "amd64"},
                "source": {"commit": commit, "corresponding_source": {}},
                "artifact": {
                    "name": release_metadata.APPIMAGE_NAME,
                    "size": appimage.stat().st_size,
                    "sha256": release_metadata.sha256_file(appimage),
                },
            }
            (bundle / "provenance/appimage/WildBuzzard-AppImage-linux-x86_64.json").write_text(
                json.dumps(appimage_manifest) + "\n", encoding="utf-8"
            )

            with mock.patch.object(
                release_metadata, "inspect_source_evidence", return_value={}
            ):
                release_metadata.create_bundle_manifest(
                    type(
                        "Arguments",
                        (),
                        {
                            "root": bundle,
                            "source_commit": commit,
                            "output": bundle / release_metadata.BUNDLE_MANIFEST.as_posix(),
                        },
                    )()
                )
                release_metadata.write_bundle_checksums(
                    type("Arguments", (), {"root": bundle})()
                )
                release_metadata.verify_bundle(type("Arguments", (), {"root": bundle})())
                (bundle / "unexpected").write_text("not allowed\n", encoding="utf-8")
                with self.assertRaisesRegex(release_metadata.MetadataError, "entries differ"):
                    release_metadata.verify_bundle(type("Arguments", (), {"root": bundle})())


class MaterializeTreeTests(unittest.TestCase):
    def test_internal_notice_link_becomes_regular_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            temporary_path = Path(temporary)
            source = temporary_path / "source"
            destination = temporary_path / "destination"
            (source / "package").mkdir(parents=True)
            destination.mkdir()
            (source / "package/copyright.real").write_text("notice\n", encoding="utf-8")
            (source / "package/copyright").symlink_to("copyright.real")
            release_metadata.materialize_tree(source, destination)
            output = destination / "package/copyright"
            self.assertTrue(output.is_file())
            self.assertFalse(output.is_symlink())
            self.assertEqual(output.read_text(encoding="utf-8"), "notice\n")

    def test_escaping_notice_link_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            temporary_path = Path(temporary)
            source = temporary_path / "source"
            destination = temporary_path / "destination"
            source.mkdir()
            destination.mkdir()
            (temporary_path / "outside").write_text("not evidence\n", encoding="utf-8")
            (source / "link").symlink_to("../outside")
            with self.assertRaisesRegex(release_metadata.MetadataError, "escapes"):
                release_metadata.materialize_tree(source, destination)


class SourceEvidenceTests(unittest.TestCase):
    def test_source_archive_is_exact_git_commit(self) -> None:
        commit = subprocess.check_output(
            ["git", "-C", str(ROOT), "rev-parse", "HEAD^{commit}"], text=True
        ).strip()
        with tempfile.TemporaryDirectory() as temporary:
            evidence = Path(temporary)
            archive_name = f"BuzzardOS-source-{commit}.tar.zst"
            archive_path = evidence / archive_name
            git_archive = subprocess.check_output(
                [
                    "git",
                    "-C",
                    str(ROOT),
                    "archive",
                    "--format=tar",
                    f"--prefix=BuzzardOS-{commit}/",
                    commit,
                ]
            )
            compressed = subprocess.run(
                ["zstd", "-q", "-f", "-19", "--long=27", "-o", str(archive_path)],
                input=git_archive,
                check=True,
            )
            self.assertEqual(compressed.returncode, 0)
            digest = hashlib.sha256(archive_path.read_bytes()).hexdigest()
            provenance = {
                "schema": 1,
                "repository": "https://github.com/openresearchtools/BuzzardOS",
                "commit": commit,
                "source_date_epoch": 1,
                "archive": {
                    "name": archive_name,
                    "sha256": digest,
                    "size": archive_path.stat().st_size,
                    "format": "tar+zstd",
                    "uncompressed_sha256": hashlib.sha256(git_archive).hexdigest(),
                    "uncompressed_size": len(git_archive),
                },
            }
            (evidence / "source-provenance.json").write_text(
                json.dumps(provenance) + "\n", encoding="utf-8"
            )
            (evidence / "SHA256SUMS").write_text(
                f"{digest}  {archive_name}\n", encoding="utf-8"
            )

            record = release_metadata.inspect_source_evidence(evidence, commit)
            self.assertEqual(record["uncompressed_size"], len(git_archive))
            self.assertEqual(record["uncompressed_sha256"], hashlib.sha256(git_archive).hexdigest())


class IdMapTests(unittest.TestCase):
    @staticmethod
    def capability(
        revision: int,
        *,
        permitted: int = 0x0000000200000001,
        inheritable: int = 0x0000000800000004,
        effective: bool = True,
        root_id: int | None = None,
    ) -> bytes:
        magic = revision | (release_metadata.VFS_CAP_FLAGS_EFFECTIVE if effective else 0)
        value = struct.pack(
            "<IIIII",
            magic,
            permitted & 0xFFFFFFFF,
            inheritable & 0xFFFFFFFF,
            permitted >> 32,
            inheritable >> 32,
        )
        if root_id is not None:
            value += struct.pack("<I", root_id)
        return value

    def test_keep_id_mapping_matches_launcher_contract(self) -> None:
        self.assertEqual(release_metadata.guest_id_to_host(0, 1001, 100000), 100000)
        self.assertEqual(release_metadata.guest_id_to_host(999, 1001, 100000), 100999)
        self.assertEqual(release_metadata.guest_id_to_host(1000, 1001, 100000), 1001)
        self.assertEqual(release_metadata.guest_id_to_host(1001, 1001, 100000), 101000)
        self.assertEqual(release_metadata.guest_id_to_host(65535, 1001, 100000), 165534)

    def test_keep_id_mapping_rejects_out_of_range_guest_id(self) -> None:
        with self.assertRaisesRegex(release_metadata.MetadataError, "outside"):
            release_metadata.guest_id_to_host(65536, 1001, 100000)

    def test_capability_parser_decodes_v2_and_v3(self) -> None:
        v2 = self.capability(release_metadata.VFS_CAP_REVISION_2)
        v3 = self.capability(release_metadata.VFS_CAP_REVISION_3, root_id=100000)
        self.assertEqual(
            release_metadata.parse_file_capability(v2, "v2"),
            (
                release_metadata.VFS_CAP_REVISION_2,
                0x0000000200000001,
                0x0000000800000004,
                True,
                None,
            ),
        )
        self.assertEqual(
            release_metadata.parse_file_capability(v3, "v3")[-1], 100000
        )

    def test_capability_parser_rejects_malformed_forms(self) -> None:
        v2 = self.capability(release_metadata.VFS_CAP_REVISION_2)
        with self.assertRaisesRegex(release_metadata.MetadataError, "size"):
            release_metadata.parse_file_capability(v2[:-1], "short v2")
        with self.assertRaisesRegex(release_metadata.MetadataError, "unsupported revision"):
            release_metadata.parse_file_capability(
                self.capability(0x01000000)[:12], "v1"
            )
        invalid_flags = bytearray(v2)
        invalid_flags[0] |= 0x02
        with self.assertRaisesRegex(release_metadata.MetadataError, "unsupported flags"):
            release_metadata.parse_file_capability(bytes(invalid_flags), "flags")

    def test_idmapped_capability_accepts_only_semantic_v2_to_v3_conversion(self) -> None:
        canonical = self.capability(release_metadata.VFS_CAP_REVISION_2)
        mapped = self.capability(
            release_metadata.VFS_CAP_REVISION_3, root_id=100000
        )
        release_metadata.verify_idmapped_file_capability(
            "usr/bin/helper", canonical, mapped, 100000
        )

        wrong_masks = self.capability(
            release_metadata.VFS_CAP_REVISION_3,
            permitted=0x10,
            root_id=100000,
        )
        with self.assertRaisesRegex(release_metadata.MetadataError, "masks or flags"):
            release_metadata.verify_idmapped_file_capability(
                "usr/bin/helper", canonical, wrong_masks, 100000
            )
        with self.assertRaisesRegex(release_metadata.MetadataError, "root ID"):
            release_metadata.verify_idmapped_file_capability(
                "usr/bin/helper",
                canonical,
                self.capability(release_metadata.VFS_CAP_REVISION_3, root_id=100001),
                100000,
            )
        with self.assertRaisesRegex(release_metadata.MetadataError, "missing mapped"):
            release_metadata.verify_idmapped_file_capability(
                "usr/bin/helper", canonical, None, 100000
            )


if __name__ == "__main__":
    unittest.main()
