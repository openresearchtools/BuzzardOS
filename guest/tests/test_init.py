#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later

from __future__ import annotations

import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
INIT = ROOT / "guest/assets/buzzardos-init"
BWRAP = shutil.which("bwrap")
@unittest.skipUnless(BWRAP, "bubblewrap is required to isolate the guest init test")
class GuestInitTests(unittest.TestCase):
    def run_init(
        self,
        *,
        machine_id: bytes = b"fixture-machine-id\n",
    ) -> tuple[list[str], bytes]:
        with tempfile.TemporaryDirectory(prefix="buzzardos-init-") as temporary:
            sandbox = Path(temporary)
            etc = sandbox / "etc"
            run = sandbox / "run"
            commands = sandbox / "commands"
            usr_bin = sandbox / "usr-bin"
            etc.mkdir(parents=True)
            run.mkdir()
            commands.mkdir()
            usr_bin.mkdir()
            (etc / "machine-id").write_bytes(machine_id)
            (etc / "passwd").write_text(
                "root:x:0:0:root:/root:/bin/sh\n"
                "buzzardos:x:1000:1000:Buzzard OS:/home/buzzard:/bin/sh\n",
                encoding="utf-8",
            )
            (etc / "group").write_text(
                "root:x:0:\n" "buzzardos:x:1000:\n",
                encoding="utf-8",
            )

            machine_id_setup = commands / "systemd-machine-id-setup"
            machine_id_setup.write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' machine-id >>/run/init-events\n"
                "printf '%s\\n' generated-machine-id >/etc/machine-id\n",
                encoding="utf-8",
            )
            machine_id_setup.chmod(0o755)

            shutil.copy2("/bin/sh", usr_bin / "sh", follow_symlinks=True)
            install = shutil.which("install")
            self.assertIsNotNone(install)
            shutil.copy2(install, usr_bin / "install", follow_symlinks=True)

            systemd = commands / "systemd"
            systemd.write_text(
                "#!/bin/sh\n"
                "printf 'systemd' >>/run/init-events\n"
                "printf ' %s' \"$@\" >>/run/init-events\n"
                "printf '\\n' >>/run/init-events\n",
                encoding="utf-8",
            )
            systemd.chmod(0o755)

            systemd_target = str(Path("/lib/systemd/systemd").resolve())
            command = [
                str(BWRAP),
                "--die-with-parent",
                "--unshare-user",
                "--unshare-pid",
                "--unshare-net",
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--bind",
                str(etc),
                "/etc",
                "--bind",
                str(run),
                "/run",
                "--ro-bind",
                str(usr_bin),
                "/usr/bin",
                "--ro-bind",
                str(systemd),
                systemd_target,
                "--setenv",
                "PATH",
                f"{commands}:/usr/bin:/bin",
                "/bin/sh",
                str(INIT),
            ]
            completed = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(
                completed.returncode,
                0,
                f"guest init failed:\nstdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
            )

            events = (run / "init-events").read_text(encoding="utf-8").splitlines()
            return events, (etc / "machine-id").read_bytes()

    def test_clone_generates_machine_id_before_systemd(self) -> None:
        events, machine_id = self.run_init(machine_id=b"")

        self.assertEqual(events, ["machine-id", "systemd --system"])
        self.assertEqual(machine_id, b"generated-machine-id\n")

    def test_existing_machine_id_is_preserved(self) -> None:
        original = b"fixture-machine-id\n"
        events, machine_id = self.run_init(machine_id=original)

        self.assertEqual(events, ["systemd --system"])
        self.assertEqual(machine_id, original)


if __name__ == "__main__":
    unittest.main()
