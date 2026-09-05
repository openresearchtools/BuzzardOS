# SPDX-License-Identifier: AGPL-3.0-or-later
import importlib.util
from pathlib import Path
import shutil
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("crun_source", ROOT / "tools/crun_source.py")
crun_source = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(crun_source)


class CrunSourceTests(unittest.TestCase):
    def test_full_source_matches_release_and_recursive_commit_pins(self):
        record = crun_source.verify()
        self.assertEqual(record["version"], "1.29.1")
        self.assertEqual(record["commit"], "f0d911de5587342cfeb16473bf32ecdfeaf25957")
        self.assertEqual(len(record["repository"]), 4)

    def test_source_changes_missing_files_and_mode_changes_fail(self):
        with tempfile.TemporaryDirectory() as temporary:
            vendor = Path(temporary) / "crun"
            shutil.copytree(crun_source.VENDOR, vendor, symlinks=True)
            path = vendor / "source/README.md"
            original = path.read_bytes()
            path.write_bytes(original + b"changed")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                crun_source.verify(vendor)
            path.write_bytes(original)
            path.chmod(0o755)
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                crun_source.verify(vendor)
            path.unlink()
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                crun_source.verify(vendor)

    def test_corresponding_source_includes_exact_build_recipe(self):
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "source.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                bundle.add(crun_source.VENDOR, arcname="third-party/crun")
                for name in ("packaging/build-crun.sh", "tools/crun_source.py",
                             "tools/verify-elf-glibc-floor.py"):
                    bundle.add(ROOT / name, arcname=name)
            crun_source.verify_archive(archive)
            with tarfile.open(archive, "w:gz") as bundle:
                bundle.add(crun_source.VENDOR, arcname="third-party/crun")
            with self.assertRaisesRegex(ValueError, "incomplete"):
                crun_source.verify_archive(archive)


if __name__ == "__main__":
    unittest.main()
