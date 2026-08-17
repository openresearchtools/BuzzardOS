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
HOST_KEY_TYPES = ("rsa", "ecdsa", "ed25519")


@unittest.skipUnless(BWRAP, "bubblewrap is required to isolate the guest init test")
class GuestInitTests(unittest.TestCase):
    def run_init(
        self,
        host_keys: dict[str, bytes],
        *,
        machine_id: bytes = b"fixture-machine-id\n",
        ssh_keygen_available: bool = True,
    ) -> tuple[list[str], dict[str, bytes]]:
        with tempfile.TemporaryDirectory(prefix="buzzardos-init-") as temporary:
            sandbox = Path(temporary)
            etc = sandbox / "etc"
            ssh = etc / "ssh"
            run = sandbox / "run"
            commands = sandbox / "commands"
            usr_bin = sandbox / "usr-bin"
            ssh.mkdir(parents=True)
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

            for key_type, contents in host_keys.items():
                (ssh / f"ssh_host_{key_type}_key").write_bytes(contents)

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

            ssh_keygen = usr_bin / "ssh-keygen"
            if ssh_keygen_available:
                ssh_keygen.write_text(
                    "#!/bin/sh\n"
                    "test \"$#\" -eq 1\n"
                    "test \"$1\" = -A\n"
                    "printf '%s\\n' 'ssh-keygen -A' >>/run/init-events\n"
                    "for type in rsa ecdsa ed25519; do\n"
                    "    key=/etc/ssh/ssh_host_${type}_key\n"
                    "    if [ ! -e \"$key\" ] && [ ! -L \"$key\" ]; then\n"
                    "        printf 'generated-%s\\n' \"$type\" >\"$key\"\n"
                    "    fi\n"
                    "done\n",
                    encoding="utf-8",
                )
                ssh_keygen.chmod(0o755)

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
            resulting_keys = {
                key_type: path.read_bytes()
                for key_type in HOST_KEY_TYPES
                if (path := ssh / f"ssh_host_{key_type}_key").exists()
            }
            return events, resulting_keys

    def test_clone_generates_only_missing_host_keys_after_machine_id_setup(self) -> None:
        original = {
            "rsa": b"original-rsa\n",
            "ed25519": b"original-ed25519\n",
        }
        events, keys = self.run_init(original, machine_id=b"")

        self.assertEqual(events, ["machine-id", "ssh-keygen -A", "systemd --system"])
        self.assertEqual(keys["rsa"], original["rsa"])
        self.assertEqual(keys["ed25519"], original["ed25519"])
        self.assertEqual(keys["ecdsa"], b"generated-ecdsa\n")

    def test_existing_host_key_set_is_never_regenerated(self) -> None:
        original = {
            key_type: f"original-{key_type}\n".encode("ascii")
            for key_type in HOST_KEY_TYPES
        }
        events, keys = self.run_init(original)

        self.assertEqual(events, ["systemd --system"])
        self.assertEqual(keys, original)

    def test_guest_without_ssh_keygen_still_boots(self) -> None:
        events, keys = self.run_init({}, ssh_keygen_available=False)

        self.assertEqual(events, ["systemd --system"])
        self.assertEqual(keys, {})


if __name__ == "__main__":
    unittest.main()
