#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
APPRUN = ROOT / "host" / "packaging" / "AppRun"


class AppRunTests(unittest.TestCase):
    def test_empty_path_uses_proc_uid_and_executes_only_bundled_launcher(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            appdir = Path(temporary) / "AppDir"
            launcher = appdir / "usr" / "bin" / "wildbuzzard"
            launcher.parent.mkdir(parents=True)
            shutil.copy2(APPRUN, appdir / "AppRun")
            launcher.write_text(
                "#!/bin/sh\n"
                "set -eu\n"
                "printf '%s\\n' \"$GST_REGISTRY_1_0\" \"$PATH\" \"$@\" "
                "> \"$WILDBUZZARD_APPRUN_CAPTURE\"\n",
                encoding="utf-8",
            )
            launcher.chmod(0o755)
            capture = Path(temporary) / "capture"

            completed = subprocess.run(
                [str(appdir / "AppRun"), "first", "second argument"],
                check=False,
                capture_output=True,
                text=True,
                env={
                    "PATH": "",
                    "WILDBUZZARD_APPRUN_CAPTURE": str(capture),
                },
            )

            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(
                capture.read_text(encoding="utf-8").splitlines(),
                [
                    f"/tmp/wildbuzzard-gstreamer-registry-{os.getuid()}.bin",
                    str(appdir / "usr" / "bin"),
                    "first",
                    "second argument",
                ],
            )

    def test_uid_resolution_has_no_host_command_fallback(self) -> None:
        script = APPRUN.read_text(encoding="utf-8")
        self.assertIn("done < /proc/self/status", script)
        self.assertNotIn("$(id -u)", script)
        self.assertNotIn("$(dirname", script)

    def test_host_wayland_client_precedes_the_portable_fallback_for_host_mesa(self) -> None:
        script = APPRUN.read_text(encoding="utf-8")
        self.assertIn(
            "/usr/lib/x86_64-linux-gnu/libwayland-client.so.0",
            script,
        )
        self.assertIn(
            'export LD_PRELOAD="$host_wayland_client${LD_PRELOAD:+:$LD_PRELOAD}"',
            script,
        )
        self.assertLess(
            script.index("host_wayland_client="),
            script.index('exec "$appdir/usr/bin/wildbuzzard"'),
        )


if __name__ == "__main__":
    unittest.main()
