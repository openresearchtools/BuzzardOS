#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
from __future__ import annotations

import hashlib
import importlib.util
import io
import json
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tools"))
SPEC = importlib.util.spec_from_file_location(
    "verify_portable_release_inputs",
    ROOT / "tools/verify-portable-release-inputs.py",
)
assert SPEC is not None and SPEC.loader is not None
verify_inputs = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verify_inputs)


def json_bytes(value: object) -> bytes:
    return (json.dumps(value, separators=(",", ":")) + "\n").encode()


def descriptor(contents: bytes, media_type: str) -> dict[str, object]:
    return {
        "mediaType": media_type,
        "digest": f"sha256:{hashlib.sha256(contents).hexdigest()}",
        "size": len(contents),
    }


def create_oci_seed(destination: Path, *, machine_identity: bool = False) -> str:
    layer_stream = io.BytesIO()
    with tarfile.open(fileobj=layer_stream, mode="w", format=tarfile.PAX_FORMAT):
        pass
    layer = layer_stream.getvalue()
    diff_id = f"sha256:{hashlib.sha256(layer).hexdigest()}"
    config = json_bytes(
        {
            "architecture": "amd64",
            "os": "linux",
            "config": {},
            "rootfs": {"type": "layers", "diff_ids": [diff_id]},
        }
    )
    config_descriptor = descriptor(
        config, "application/vnd.oci.image.config.v1+json"
    )
    layer_descriptor = descriptor(layer, "application/vnd.oci.image.layer.v1.tar")
    manifest_record = {
        "schemaVersion": 2,
        "config": config_descriptor,
        "layers": [layer_descriptor],
    }
    if machine_identity:
        manifest_record["annotations"] = {
            verify_inputs.MACHINE_CONFIG_ANNOTATION: '{"id":"machine-specific"}'
        }
    manifest = json_bytes(manifest_record)
    manifest_descriptor = descriptor(
        manifest, "application/vnd.oci.image.manifest.v1+json"
    )
    index = json_bytes({"schemaVersion": 2, "manifests": [manifest_descriptor]})
    marker = json_bytes({"imageLayoutVersion": "1.0.0"})
    blobs = {
        hashlib.sha256(config).hexdigest(): config,
        hashlib.sha256(layer).hexdigest(): layer,
        hashlib.sha256(manifest).hexdigest(): manifest,
    }

    plain = destination.with_suffix(".tar")
    with tarfile.open(plain, mode="w", format=tarfile.PAX_FORMAT) as archive:
        for name in (".", "./blobs", "./blobs/sha256"):
            member = tarfile.TarInfo(name)
            member.type = tarfile.DIRTYPE
            member.mode = 0o755
            archive.addfile(member)
        for digest, contents in sorted(blobs.items()):
            member = tarfile.TarInfo(f"./blobs/sha256/{digest}")
            member.size = len(contents)
            member.mode = 0o644
            archive.addfile(member, io.BytesIO(contents))
        for name, contents in (("./index.json", index), ("./oci-layout", marker)):
            member = tarfile.TarInfo(name)
            member.size = len(contents)
            member.mode = 0o644
            archive.addfile(member, io.BytesIO(contents))
    subprocess.run(
        ["zstd", "-q", "-f", "-19", str(plain), "-o", str(destination)],
        check=True,
    )
    plain.unlink()
    return str(manifest_descriptor["digest"])


class PortableReleaseInputTests(unittest.TestCase):
    commit = "a" * 40

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        temporary = Path(self.temporary.name)
        self.project = temporary / "project"
        self.stage = temporary / "stage"

        sources = set(verify_inputs.GUEST_PROVENANCE_SOURCES.values()) | {
            Path("tools/release/guest-rootfs-licenses.README.md")
        }
        for relative in sources:
            path = self.project / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f"source:{relative.as_posix()}\n", encoding="utf-8")

        runtime = self.stage / "runtime"
        guest_licenses = self.stage / "licenses/guest"
        guest_provenance = self.stage / "provenance/guest"
        runtime.mkdir(parents=True)
        guest_provenance.mkdir(parents=True)
        for name in ("project-source", "usr-share-common-licenses", "usr-share-doc"):
            (guest_licenses / name).mkdir(parents=True)
        (guest_licenses / "usr-share-common-licenses/GPL-2").write_text(
            "license\n", encoding="utf-8"
        )
        (guest_licenses / "usr-share-doc/package-copyright").write_text(
            "copyright\n", encoding="utf-8"
        )
        (guest_licenses / "README.md").write_bytes(
            (self.project / "tools/release/guest-rootfs-licenses.README.md").read_bytes()
        )

        self.archive = runtime / verify_inputs.ARCHIVE_NAME
        manifest_digest = create_oci_seed(self.archive)
        archive_sha256 = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        self.metadata = runtime / verify_inputs.METADATA_NAME
        self.metadata.write_text(
            json.dumps(
                {
                    "schema": 1,
                    "kind": "buzzardos-oci-seed",
                    "platform": {"os": "linux", "architecture": "amd64"},
                    "archive": {
                        "name": verify_inputs.ARCHIVE_NAME,
                        "size": self.archive.stat().st_size,
                        "sha256": archive_sha256,
                    },
                    "manifest_digest": manifest_digest,
                    "source_manifest_digest": f"sha256:{'c' * 64}",
                    "source_commit": self.commit,
                },
                separators=(",", ":"),
            )
            + "\n",
            encoding="utf-8",
        )
        metadata_sha256 = hashlib.sha256(self.metadata.read_bytes()).hexdigest()
        (self.stage / "ROOTFS_SHA256SUMS").write_text(
            f"{archive_sha256}  runtime/{verify_inputs.ARCHIVE_NAME}\n"
            f"{metadata_sha256}  runtime/{verify_inputs.METADATA_NAME}\n",
            encoding="utf-8",
        )
        (guest_provenance / verify_inputs.METADATA_NAME).write_bytes(
            self.metadata.read_bytes()
        )
        for bundled, relative in verify_inputs.GUEST_PROVENANCE_SOURCES.items():
            (guest_provenance / bundled).write_bytes((self.project / relative).read_bytes())

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def verify(self) -> None:
        with mock.patch.object(
            verify_inputs.release_metadata, "inspect_source_evidence", return_value={}
        ):
            verify_inputs.verify_rootfs_stage(self.stage, self.project, self.commit)

    def bind_current_archive(self) -> None:
        metadata = json.loads(self.metadata.read_text(encoding="utf-8"))
        metadata["archive"] = {
            "name": verify_inputs.ARCHIVE_NAME,
            "size": self.archive.stat().st_size,
            "sha256": hashlib.sha256(self.archive.read_bytes()).hexdigest(),
        }
        self.metadata.write_text(
            json.dumps(metadata, separators=(",", ":")) + "\n", encoding="utf-8"
        )
        (self.stage / "provenance/guest" / verify_inputs.METADATA_NAME).write_bytes(
            self.metadata.read_bytes()
        )
        (self.stage / "ROOTFS_SHA256SUMS").write_text(
            f"{metadata['archive']['sha256']}  runtime/{verify_inputs.ARCHIVE_NAME}\n"
            f"{hashlib.sha256(self.metadata.read_bytes()).hexdigest()}  "
            f"runtime/{verify_inputs.METADATA_NAME}\n",
            encoding="utf-8",
        )

    def test_accepts_complete_digest_bound_stage(self) -> None:
        self.verify()

    def test_rejects_seed_changed_after_metadata_was_written(self) -> None:
        self.archive.write_bytes(b"changed-seed")
        with self.assertRaisesRegex(verify_inputs.VerificationError, "differs from its metadata"):
            self.verify()

    def test_rejects_digest_bound_bytes_that_are_not_an_oci_seed(self) -> None:
        self.archive.write_bytes(b"not-a-zstd-oci-seed")
        self.bind_current_archive()
        with self.assertRaisesRegex(
            verify_inputs.VerificationError, "not a Zstandard frame"
        ):
            self.verify()

    def test_rejects_a_non_flattened_multi_layer_seed(self) -> None:
        with tempfile.TemporaryDirectory() as scratch_name:
            scratch = Path(scratch_name)
            decompressed = scratch / "multi-layer.tar"
            subprocess.run(
                ["zstd", "-q", "-dc", str(self.archive), "-o", str(decompressed)],
                check=True,
            )
            extracted = scratch / "layout"
            extracted.mkdir()
            with tarfile.open(decompressed) as archive:
                archive.extractall(extracted, filter="data")
            index = json.loads((extracted / "index.json").read_text(encoding="utf-8"))
            manifest_digest = index["manifests"][0]["digest"].removeprefix("sha256:")
            manifest_path = extracted / "blobs/sha256" / manifest_digest
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["layers"].append(dict(manifest["layers"][0]))
            new_manifest = json_bytes(manifest)
            new_digest = hashlib.sha256(new_manifest).hexdigest()
            new_path = extracted / "blobs/sha256" / new_digest
            new_path.write_bytes(new_manifest)
            manifest_path.unlink()
            index["manifests"][0]["digest"] = f"sha256:{new_digest}"
            index["manifests"][0]["size"] = len(new_manifest)
            (extracted / "index.json").write_bytes(json_bytes(index))
            with tarfile.open(decompressed, "w", format=tarfile.PAX_FORMAT) as archive:
                archive.add(extracted, arcname=".")
            subprocess.run(
                ["zstd", "-q", "-f", "-19", str(decompressed), "-o", str(self.archive)],
                check=True,
            )
        metadata = json.loads(self.metadata.read_text(encoding="utf-8"))
        metadata["manifest_digest"] = f"sha256:{new_digest}"
        self.metadata.write_text(
            json.dumps(metadata, separators=(",", ":")) + "\n", encoding="utf-8"
        )
        self.bind_current_archive()
        with self.assertRaisesRegex(
            verify_inputs.VerificationError, "one flattened filesystem layer"
        ):
            self.verify()

    def test_rejects_seed_with_portable_machine_identity(self) -> None:
        manifest_digest = create_oci_seed(self.archive, machine_identity=True)
        metadata = json.loads(self.metadata.read_text(encoding="utf-8"))
        metadata["manifest_digest"] = manifest_digest
        self.metadata.write_text(
            json.dumps(metadata, separators=(",", ":")) + "\n", encoding="utf-8"
        )
        self.bind_current_archive()
        with self.assertRaisesRegex(
            verify_inputs.VerificationError, "carries portable machine identity"
        ):
            self.verify()

    def test_rejects_inconsistent_checksum_manifest(self) -> None:
        (self.stage / "ROOTFS_SHA256SUMS").write_text("", encoding="utf-8")
        with self.assertRaisesRegex(
            verify_inputs.VerificationError, "ROOTFS_SHA256SUMS is incomplete"
        ):
            self.verify()

    def test_rejects_divergent_guest_provenance_metadata(self) -> None:
        (self.stage / "provenance/guest" / verify_inputs.METADATA_NAME).write_text(
            "{}\n", encoding="utf-8"
        )
        with self.assertRaisesRegex(
            verify_inputs.VerificationError, "differs from the runtime metadata"
        ):
            self.verify()

    def test_rejects_source_descriptor_not_from_the_build_commit(self) -> None:
        (self.stage / "provenance/guest/base-images.lock.toml").write_text(
            "tampered\n", encoding="utf-8"
        )
        with self.assertRaisesRegex(
            verify_inputs.VerificationError, "base-images.lock.toml differs"
        ):
            self.verify()

    def test_rejects_extra_unaccounted_provenance_file(self) -> None:
        (self.stage / "provenance/guest/extra.txt").write_text("extra\n", encoding="utf-8")
        with self.assertRaisesRegex(verify_inputs.VerificationError, "unexpected inventory"):
            self.verify()

    def test_rejects_empty_guest_notice_group(self) -> None:
        (self.stage / "licenses/guest/usr-share-doc/package-copyright").unlink()
        with self.assertRaisesRegex(verify_inputs.VerificationError, "contains no notice files"):
            self.verify()


if __name__ == "__main__":
    unittest.main()
