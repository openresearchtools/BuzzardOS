#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Explicit guest-only Firefox acceptance; launch through the real Sway service.

Run as the guest desktop user with SWAYSOCK set. The temporary Firefox profile
is independent of the user's profile; no browser sandbox or renderer is disabled.
Marionette is a loopback-only test endpoint, not a shipped Buzzard component.
"""

import json
import os
from pathlib import Path
import shlex
import signal
import socket
import subprocess
import tempfile
import time


CGROUP = Path("/sys/fs/cgroup")
DESKTOP = CGROUP / "system.slice/buzzardos-desktop.service"


def events():
    return {str(path): (path / "pids.events").read_text()
            for path in (CGROUP, DESKTOP)}


def profile_processes(profile):
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            if entry.stat().st_uid != os.getuid():
                continue
            args = (entry / "cmdline").read_bytes().split(b"\0")
            if (str(profile).encode() in args and b"--profile" in args
                    and "firefox" in Path(os.fsdecode(args[0])).name):
                yield int(entry.name)
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue


def close_profile(profile):
    # Only this invocation's fresh profile may be terminated, never user Firefox.
    for pid in profile_processes(profile):
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + 10
    while list(profile_processes(profile)):
        if time.monotonic() >= deadline:
            raise RuntimeError("Temporary test Firefox did not exit")
        time.sleep(0.1)


class Marionette:
    def __init__(self, connection):
        self.connection = connection
        self.sequence = 0
        self.packet()

    def packet(self):
        prefix = bytearray()
        while True:
            byte = self.connection.recv(1)
            if not byte:
                raise RuntimeError("Firefox closed the connection")
            if byte == b":":
                break
            prefix.extend(byte)
            if len(prefix) > 10:
                raise RuntimeError("Invalid Marionette packet length")
        remaining = int(prefix)
        if not 0 < remaining <= 16_000_000:
            raise RuntimeError("Invalid Marionette packet size")
        body = bytearray()
        while remaining:
            chunk = self.connection.recv(remaining)
            if not chunk:
                raise RuntimeError("Firefox closed an incomplete packet")
            body.extend(chunk)
            remaining -= len(chunk)
        return json.loads(body)

    def command(self, name, arguments):
        self.sequence += 1
        payload = json.dumps([0, self.sequence, name, arguments]).encode()
        self.connection.sendall(str(len(payload)).encode() + b":" + payload)
        response = self.packet()
        assert response[:2] == [1, self.sequence], response
        assert response[2] is None, response
        value = response[3]
        return value.get("value", value) if isinstance(value, dict) else value

    def script(self, script):
        return self.command("WebDriver:ExecuteScript", {
            "script": script, "args": [], "newSandbox": True, "sandbox": None})


def run(profile):
    before = events()
    for path in (CGROUP, DESKTOP):
        assert (path / "pids.max").read_text().strip() == "max", path
    crashes = Path.home() / ".mozilla/firefox/Crash Reports/pending"
    old_crashes = set(crashes.glob("*.extra"))
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    (profile / "user.js").write_text(
        f'user_pref("marionette.port", {port});\n', encoding="utf-8")
    argv = ["env", "MOZ_ENABLE_WAYLAND=1", "firefox-esr", "--no-remote",
            "--profile", str(profile), "--marionette",
            "--remote-allow-system-access", "about:blank"]
    result = subprocess.run(["swaymsg", "-r", "exec " + shlex.join(argv)],
                            check=True, capture_output=True, text=True, timeout=10)
    assert all(row.get("success") for row in json.loads(result.stdout)), result.stdout
    connection = None
    browser = None
    pid = None
    try:
        deadline = time.monotonic() + 45
        while connection is None:
            try:
                connection = socket.create_connection(("127.0.0.1", port), timeout=1)
            except OSError:
                if time.monotonic() >= deadline:
                    raise RuntimeError("Firefox startup timed out")
                time.sleep(0.1)
        connection.settimeout(60)
        browser = Marionette(connection)
        browser.command("WebDriver:NewSession", {"capabilities": {"alwaysMatch": {}}})
        browser.command("Marionette:SetContext", {"value": "chrome"})
        pid = browser.script("return Services.appinfo.processID;")
        cgroup = Path(f"/proc/{pid}/cgroup").read_text().strip()
        assert cgroup == "0::/system.slice/buzzardos-desktop.service", cgroup
        browser.command("Marionette:SetContext", {"value": "content"})
        browser.command("WebDriver:SetTimeouts", {"pageLoad": 45000, "script": 15000})
        report = {"browser_cgroup": cgroup, "pages": [], "peak_desktop_tasks": 0}
        for url in ("https://example.com", "https://www.mozilla.org/en-US/",
                    "https://www.python.org", "https://www.kernel.org"):
            window = browser.command("WebDriver:NewWindow", {"type": "tab"})
            browser.command("WebDriver:SwitchToWindow", {"handle": window["handle"]})
            browser.command("WebDriver:Navigate", {"url": url})
            page = browser.script("return {title:document.title, url:location.href, "
                                  "textLength:document.body.innerText.length};")
            assert page["url"].startswith("https://"), page
            assert page["title"] and page["textLength"] > 100, page
            tasks = int((DESKTOP / "pids.current").read_text())
            report["peak_desktop_tasks"] = max(report["peak_desktop_tasks"], tasks)
            report["pages"].append(page)
            print(json.dumps({"page": page, "desktop_tasks": tasks}), flush=True)
        webgl = browser.script("""
            const c=document.createElement('canvas'); c.width=32; c.height=32;
            document.body.append(c);
            const gl=c.getContext('webgl2',{preserveDrawingBuffer:true});
            if(!gl) throw Error('WebGL2 unavailable');
            gl.clearColor(1,0,0,1); gl.clear(gl.COLOR_BUFFER_BIT);
            const p=new Uint8Array(4); gl.readPixels(0,0,1,1,gl.RGBA,gl.UNSIGNED_BYTE,p);
            const d=gl.getExtension('WEBGL_debug_renderer_info');
            return {pixel:[...p],renderer:gl.getParameter(d.UNMASKED_RENDERER_WEBGL)};
        """)
        assert webgl["pixel"] == [255, 0, 0, 255], webgl
        assert not any(word in webgl["renderer"].lower()
                       for word in ("software", "llvmpipe", "softpipe")), webgl
        browser.command("Marionette:SetContext", {"value": "chrome"})
        gfx = browser.script("const g=Cc['@mozilla.org/gfx/info;1'].getService(Ci.nsIGfxInfo);"
                             "return {protocol:g.windowProtocol,features:g.getFeatures()};")
        assert gfx["protocol"] == "wayland", gfx
        assert gfx["features"]["compositor"] == "webrender", gfx
        assert gfx["features"]["hwCompositing"]["status"] == "available", gfx
        assert report["peak_desktop_tasks"] > 307, "Old task ceiling was not exercised"
        assert events() == before, "Task-limit rejections increased"
        assert not set(crashes.glob("*.extra")) - old_crashes, "New Firefox crash reports"
        report.update(webgl=webgl, graphics=gfx, pids_events_unchanged=True, passed=True)
    finally:
        try:
            if browser is not None:
                browser.command("Marionette:Quit", {"flags": ["eAttemptQuit"]})
        except (OSError, RuntimeError, AssertionError):
            pass
        finally:
            if connection is not None:
                connection.close()
            close_profile(profile)
    assert events() == before, "Task-limit rejections increased during shutdown"
    assert not set(crashes.glob("*.extra")) - old_crashes, "New Firefox crash reports"
    print(json.dumps(report), flush=True)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="buzzard-firefox-task-acceptance-") as temporary:
        run(Path(temporary))
