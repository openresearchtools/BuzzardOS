#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
LAUNCHER = ROOT / "host" / "packaging" / "BuzzardOS"


class PortableLauncherTests(unittest.TestCase):
    def test_launcher_resolves_relocated_bundle_without_host_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bundle = root / "relocated BuzzardOS"
            app = bundle / "app"
            app.mkdir(parents=True)
            shutil.copy2(LAUNCHER, bundle / "BuzzardOS")
            (bundle / "BuzzardOS").chmod(0o755)
            (app / "AppRun").write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' \"$BUZZARDOS_PORTABLE_DIR\" \"$APPDIR\" \"$1\"\n",
                encoding="utf-8",
            )
            (app / "AppRun").chmod(0o755)

            result = subprocess.run(
                [str(bundle / "BuzzardOS"), "argument value"],
                cwd=root,
                env={"PATH": "/definitely-not-a-host-helper-path"},
                check=True,
                capture_output=True,
                text=True,
            )

            self.assertEqual(
                result.stdout.splitlines(),
                [str(bundle), str(app), "argument value"],
            )


if __name__ == "__main__":
    unittest.main()
