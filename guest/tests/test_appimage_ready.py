#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later

import ctypes
import errno
import fcntl
import os
import runpy
import stat
import subprocess
import tempfile
import time
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "assets" / "wildbuzzard-appimage-ready"


class AppImageReadyTests(unittest.TestCase):
    @staticmethod
    def wait_for(predicate, message: str) -> None:
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.01)
        raise AssertionError(message)

    def test_only_real_appimages_gain_owner_execute(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            nested = root / "nested"
            nested.mkdir()
            appimage = nested / "Program.AppImage"
            invalid = root / "not-an-appimage.AppImage"
            target = root / "symlink-target.AppImage"
            link = root / "linked.AppImage"
            appimage.write_bytes(b"\x7fELF\x02\x01\x01\x00AI\x02" + b"payload")
            invalid.write_bytes(b"ordinary data")
            target.write_bytes(b"\x7fELF\x02\x01\x01\x00AI\x02" + b"payload")
            link.symlink_to(target.name)
            for path in (appimage, invalid, target):
                path.chmod(0o640)

            subprocess.run(
                [str(SCRIPT), "--once", "--root", str(root)],
                check=True,
            )

            self.assertEqual(stat.S_IMODE(appimage.stat().st_mode), 0o740)
            self.assertEqual(stat.S_IMODE(invalid.stat().st_mode), 0o640)
            self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o740)
            self.assertTrue(link.is_symlink())

    def test_type_one_marker_is_supported(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            appimage = Path(temporary) / "legacy.AppImage"
            appimage.write_bytes(b"\x7fELF\x01\x01\x01\x00AI\x01")
            appimage.chmod(0o600)
            subprocess.run(
                [str(SCRIPT), "--once", "--root", temporary],
                check=True,
            )
            self.assertEqual(stat.S_IMODE(appimage.stat().st_mode), 0o700)

    def test_inotify_descriptor_is_nonblocking(self) -> None:
        namespace = runpy.run_path(str(SCRIPT))
        try:
            watcher = namespace["Inotify"]()
        except OSError as error:
            if error.errno == 28:  # ENOSPC: host-wide inotify instance limit.
                self.skipTest("host has no free inotify instances")
            raise
        try:
            flags = fcntl.fcntl(watcher.descriptor, fcntl.F_GETFL)
            self.assertTrue(flags & os.O_NONBLOCK)
        finally:
            watcher.close()

    def test_enospc_releases_every_partial_inotify_watch(self) -> None:
        namespace = runpy.run_path(str(SCRIPT))
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "first").mkdir()
            (root / "second").mkdir()
            try:
                watcher = namespace["Inotify"]()
            except OSError as error:
                if error.errno == errno.ENOSPC:
                    self.skipTest("host has no free inotify instances")
                raise
            try:
                descriptor = watcher.descriptor
                calls = 0

                def exhausted_add_watch(*arguments: object) -> int:
                    nonlocal calls
                    calls += 1
                    if calls == 1:
                        # Model one successfully registered watch without
                        # depending on the host user's shared inotify budget.
                        return 1234
                    ctypes.set_errno(errno.ENOSPC)
                    return -1

                watcher._add_watch = exhausted_add_watch
                with self.assertRaises(namespace["InotifyCapacityExhausted"]):
                    watcher.add_tree(root)

                self.assertGreaterEqual(calls, 2)
                self.assertEqual(watcher.descriptor, -1)
                self.assertEqual(watcher.paths, {})
                self.assertEqual(watcher.roots, set())
                self.assertEqual(watcher.fallback_paths, set())
                with self.assertRaises(OSError) as closed:
                    os.fstat(descriptor)
                self.assertEqual(closed.exception.errno, errno.EBADF)
            finally:
                watcher.close()

    def test_periodic_scan_delay_is_bounded_and_adaptive(self) -> None:
        namespace = runpy.run_path(str(SCRIPT))
        delay = namespace["fallback_scan_delay"]
        self.assertEqual(delay(0.01), 1.0)
        self.assertEqual(delay(2.5), 2.5)
        self.assertEqual(delay(60.0), 5.0)

    def test_live_watcher_authorizes_atomic_arrivals_and_new_directories(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "shared"
            root.mkdir()
            watcher = subprocess.Popen(
                [str(SCRIPT), "--root", str(root)],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            try:
                staging = Path(temporary) / "download.tmp"
                staging.write_bytes(b"\x7fELF\x02\x01\x01\x00AI\x02" + b"payload")
                staging.chmod(0o644)
                arrived = root / "Electron.AppImage"
                os.replace(staging, arrived)
                self.wait_for(
                    lambda: bool(arrived.stat().st_mode & stat.S_IXUSR),
                    "atomically moved AppImage did not gain owner execute",
                )

                staged_directory = Path(temporary) / "completed-download"
                staged_directory.mkdir()
                arrived_with_directory = staged_directory / "Bundled.AppImage"
                arrived_with_directory.write_bytes(
                    b"\x7fELF\x02\x01\x01\x00AI\x02" + b"payload"
                )
                arrived_with_directory.chmod(0o640)
                nested = root / "downloads"
                os.replace(staged_directory, nested)
                self.wait_for(
                    lambda: bool(
                        (nested / "Bundled.AppImage").stat().st_mode & stat.S_IXUSR
                    ),
                    "AppImage inside an atomically moved directory was not authorized",
                )

                closed = nested / "Application.AppImage"
                with closed.open("wb") as stream:
                    stream.write(b"\x7fELF\x02\x01\x01\x00AI\x02" + b"payload")
                closed.chmod(0o640)
                self.wait_for(
                    lambda: bool(closed.stat().st_mode & stat.S_IXUSR),
                    "close-written AppImage in a new directory was not authorized",
                )
            finally:
                watcher.terminate()
                stdout, stderr = watcher.communicate(timeout=5)
                self.assertEqual(stdout, "")
                self.assertNotIn("Traceback", stderr)


if __name__ == "__main__":
    unittest.main()
