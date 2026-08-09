#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later

import errno
import os
import runpy
import socket
import stat
import tempfile
import unittest
from pathlib import Path


ASSETS = Path(__file__).resolve().parents[1] / "assets"
EXECUTOR = ASSETS / "wildbuzzard-fusermount-exec"


class FusermountBridgeContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.bridge = runpy.run_path(str(EXECUTOR))
        cls.ValidationError = cls.bridge["ValidationError"]

    def test_wrapper_passes_only_caller_pid_fd_and_argument_separator(self) -> None:
        wrapper = (ASSETS / "wildbuzzard-fusermount").read_text()
        self.assertIn("caller=$$", wrapper)
        self.assertIn("communication_fd=${_FUSE_COMMFD:--1}", wrapper)
        self.assertIn("/usr/bin/systemd-run", wrapper)
        self.assertIn('"$communication_fd"', wrapper)
        lines = [line.strip() for line in wrapper.splitlines()]
        separator = lines.index("-- \\")
        self.assertEqual(lines[separator + 1], '"$@"')

    def test_accepts_only_exact_pinned_runtime_argument_shapes(self) -> None:
        parse = self.bridge["parse_invocation"]
        accepted = (
            (["--version"], -1, "version"),
            (["-o", "ro,nosuid,nodev", "--", "/tmp/.mount_AAAAAA"], 7, "mount"),
            (
                [
                    "-o",
                    "ro,nosuid,nodev,subtype=squashfuse",
                    "--",
                    "/run/user/1000/.mount_BBBBBB",
                ],
                8,
                "mount",
            ),
            (["-u", "-q", "-z", "--", "/shared/.mount_CCCCCC"], -1, "unmount"),
            (
                ["--auto-unmount", "--", "/home/wildbuzzard/.mount_DDDDDD"],
                9,
                "auto-unmount",
            ),
        )
        for arguments, descriptor, expected_kind in accepted:
            with self.subTest(arguments=arguments):
                self.assertEqual(parse(arguments, descriptor).kind, expected_kind)

        rejected = (
            (["--help"], -1),
            (["--version", "extra"], -1),
            (["--version"], 7),
            (["-o", "rw,nosuid,nodev", "--", "/tmp/.mount_bad"], 7),
            (["-o", "ro,nosuid,nodev,allow_other", "--", "/tmp/.mount_bad"], 7),
            (["-o", "ro,nosuid,nodev,subtype=evil", "--", "/tmp/.mount_bad"], 7),
            (["-o", "ro,nosuid,nodev", "--", "/tmp/.mount_bad"], -1),
            (["-u", "-z", "-q", "--", "/tmp/.mount_bad"], -1),
            (["-u", "-q", "-z", "--", "/tmp/.mount_bad"], 7),
            (["--auto-unmount", "/tmp/.mount_bad"], 7),
            (["--auto-unmount", "--", "/tmp/.mount_bad"], -1),
            (["-o", "ro,nosuid,nodev", "--", "/etc/.mount_bad"], 7),
            (["-o", "ro,nosuid,nodev", "--", "/tmp/not-a-runtime-dir"], 7),
            (["-o", "ro,nosuid,nodev", "--", "/tmp/../tmp/.mount_bad"], 7),
        )
        for arguments, descriptor in rejected:
            with self.subTest(arguments=arguments, descriptor=descriptor):
                with self.assertRaises(self.ValidationError):
                    parse(arguments, descriptor)

    def test_requires_all_uid_and_gid_slots_to_be_1000(self) -> None:
        parse = self.bridge["parse_caller_status"]
        valid = "PPid:\t42\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n"
        self.assertEqual(parse(valid), 42)
        for invalid in (
            valid.replace("1000\t1000\t1000\t1000", "1000\t0\t0\t0", 1),
            valid.replace("Gid:\t1000\t1000\t1000\t1000", "Gid:\t1000\t0\t0\t0"),
            valid.replace("PPid:\t42", "PPid:\t1"),
        ):
            with self.assertRaises(self.ValidationError):
                parse(invalid)

    def test_pidfd_is_opened_before_status_credentials_are_read(self) -> None:
        events = []
        read_fd, write_fd = os.pipe()
        os.close(write_fd)

        def open_pidfd(process_id: int, flags: int) -> int:
            events.append(("pidfd", process_id, flags))
            return os.dup(read_fd)

        def read_status(process_id: int) -> str:
            events.append(("status", process_id))
            return "PPid:\t42\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n"

        pidfd, parent = self.bridge["pin_and_validate_caller"](
            77,
            pidfd_open=open_pidfd,
            status_reader=read_status,
        )
        try:
            self.assertEqual(parent, 42)
            self.assertEqual(events, [("pidfd", 77, 0), ("status", 77)])
        finally:
            os.close(pidfd)
            os.close(read_fd)

    def test_invalid_credentials_close_the_pinned_pidfd(self) -> None:
        read_fd, write_fd = os.pipe()
        os.close(write_fd)
        pinned = os.dup(read_fd)
        with self.assertRaises(self.ValidationError):
            self.bridge["pin_and_validate_caller"](
                77,
                pidfd_open=lambda _pid, _flags: pinned,
                status_reader=lambda _pid: "PPid:\t42\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\n",
            )
        with self.assertRaises(OSError) as closed:
            os.fstat(pinned)
        self.assertEqual(closed.exception.errno, errno.EBADF)
        os.close(read_fd)

    def duplicate_for_test(self, source: socket.socket, expected_peer_pid: int) -> int:
        return self.bridge["duplicate_communication_fd"](
            123,
            7,
            expected_peer_pid,
            duplicator=lambda _pidfd, _descriptor: os.dup(source.fileno()),
        )

    def test_commfd_requires_connected_unix_stream_and_matching_peer_credentials(self) -> None:
        left, right = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            duplicated = self.duplicate_for_test(left, os.getpid())
            try:
                self.assertTrue(os.get_inheritable(duplicated))
            finally:
                os.close(duplicated)
            with self.assertRaises(self.ValidationError):
                self.duplicate_for_test(left, os.getpid() + 1)
        finally:
            left.close()
            right.close()

        left, right = socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM)
        try:
            with self.assertRaises(self.ValidationError):
                self.duplicate_for_test(left, os.getpid())
        finally:
            left.close()
            right.close()

        unconnected = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            with self.assertRaises(self.ValidationError):
                self.duplicate_for_test(unconnected, os.getpid())
        finally:
            unconnected.close()

        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            client.connect(listener.getsockname())
            server, _address = listener.accept()
            try:
                with self.assertRaises(self.ValidationError):
                    self.duplicate_for_test(client, os.getpid())
            finally:
                server.close()
        finally:
            client.close()
            listener.close()

        regular_fd = os.open("/dev/null", os.O_RDONLY)
        try:
            with self.assertRaises(self.ValidationError):
                self.bridge["duplicate_communication_fd"](
                    123,
                    7,
                    os.getpid(),
                    duplicator=lambda _pidfd, _descriptor: os.dup(regular_fd),
                )
        finally:
            os.close(regular_fd)

    def test_mountpoint_is_real_owned_mode_0700_and_symlink_free(self) -> None:
        open_mountpoint = self.bridge["open_validated_mountpoint"]
        with tempfile.TemporaryDirectory(prefix=".mount_", dir="/tmp") as temporary:
            mountpoint = Path(temporary)
            mountpoint.chmod(0o700)
            descriptor = open_mountpoint(str(mountpoint), "mount")
            os.close(descriptor)

            mountpoint.chmod(0o755)
            with self.assertRaises(self.ValidationError):
                open_mountpoint(str(mountpoint), "mount")

        with tempfile.TemporaryDirectory(dir="/tmp") as temporary:
            parent = Path(temporary)
            target = parent / "target"
            target.mkdir(mode=0o700)
            link = parent / ".mount_link"
            link.symlink_to(target)
            with self.assertRaises(self.ValidationError):
                open_mountpoint(str(link), "mount")

            real_parent = parent / "real-parent"
            real_parent.mkdir()
            nested = real_parent / ".mount_nested"
            nested.mkdir(mode=0o700)
            linked_parent = parent / "linked-parent"
            linked_parent.symlink_to(real_parent, target_is_directory=True)
            with self.assertRaises(self.ValidationError):
                open_mountpoint(str(linked_parent / nested.name), "mount")

    def test_unmount_requires_exact_approved_fuse_mount_record(self) -> None:
        approved = self.bridge["is_approved_fuse_mount"]
        path = "/tmp/.mount_Test42"
        good = f"10 9 0:42 / {path} ro,nosuid,nodev,relatime - fuse.squashfuse image ro\n"
        self.assertTrue(approved(path, good))
        self.assertFalse(approved(path, good.replace("nodev,", "")))
        self.assertFalse(approved(path, good.replace("fuse.squashfuse", "ext4")))
        self.assertFalse(approved(path + "x", good))

    def test_real_helper_is_opened_without_following_symlinks_and_executes_by_fd(self) -> None:
        open_helper = self.bridge["open_real_fusermount"]
        descriptor = open_helper("/usr/bin/fusermount3")
        try:
            self.assertTrue(stat.S_ISREG(os.fstat(descriptor).st_mode))
            captured = []
            self.bridge["fexecve_real"](
                descriptor,
                ["fusermount3", "--version"],
                {},
                executor=lambda *values: captured.append(values),
            )
            self.assertEqual(captured, [(descriptor, ["fusermount3", "--version"], {})])
        finally:
            os.close(descriptor)

        with tempfile.TemporaryDirectory() as temporary:
            link = Path(temporary) / "fusermount3"
            link.symlink_to("/usr/bin/fusermount3")
            with self.assertRaises(self.ValidationError):
                open_helper(str(link))

    def test_credential_drop_uses_user_gid_shape_and_minimal_environment(self) -> None:
        calls = []
        self.bridge["drop_to_fusermount_credentials"](
            setgroups=lambda groups: calls.append(("groups", groups)),
            setresgid=lambda real, effective, saved: calls.append(
                ("gid", real, effective, saved)
            ),
            setresuid=lambda real, effective, saved: calls.append(
                ("uid", real, effective, saved)
            ),
        )
        self.assertEqual(
            calls,
            [("groups", []), ("gid", 1000, 1000, 1000), ("uid", 1000, 0, 0)],
        )
        executor = EXECUTOR.read_text()
        self.assertIn('environment = {}', executor)
        self.assertNotIn("os.environ.copy", executor)


if __name__ == "__main__":
    unittest.main()
