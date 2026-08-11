#!/usr/bin/python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Inject one bounded keyboard sequence through Mutter into Wild Buzzard.

This is an acceptance-only host input source. It never connects to the guest,
never enables Mutter's clipboard integration, and always stops its temporary
RemoteDesktop session. The caller must first request `window focus-monitor` so
GTK's internal focus belongs to the embedded monitor rather than host chrome.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import time
from typing import Any

import dbus
import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402


REMOTE_DESKTOP_SERVICE = "org.gnome.Mutter.RemoteDesktop"
REMOTE_DESKTOP_PATH = "/org/gnome/Mutter/RemoteDesktop"
REMOTE_DESKTOP_INTERFACE = "org.gnome.Mutter.RemoteDesktop"
REMOTE_DESKTOP_SESSION_INTERFACE = "org.gnome.Mutter.RemoteDesktop.Session"
EVDEV_BACKSPACE = 14
XKB_RETURN = 0xFF0D
DBUS_TIMEOUT = 3.0


def fail(message: str) -> None:
    raise RuntimeError(message)


def process_arguments(pid: int) -> list[str]:
    payload = Path(f"/proc/{pid}/cmdline").read_bytes()
    return [item.decode("utf-8") for item in payload.split(b"\0") if item]


def child_processes(pid: int) -> list[int]:
    path = Path(f"/proc/{pid}/task/{pid}/children")
    return [int(item) for item in path.read_text(encoding="ascii").split()]


def host_status_directory(broker_pid: int) -> Path:
    pending = [broker_pid]
    visited: set[int] = set()
    matches: list[Path] = []
    while pending:
        pid = pending.pop()
        if pid in visited:
            continue
        visited.add(pid)
        try:
            arguments = process_arguments(pid)
            pending.extend(child_processes(pid))
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
        if not arguments or Path(arguments[0]).name != "wildbuzzard-display":
            continue
        try:
            index = arguments.index("--status-dir")
            status = Path(arguments[index + 1])
        except (ValueError, IndexError):
            fail(f"wildbuzzard-display process {pid} has no --status-dir")
        matches.append(status)
    if len(matches) != 1:
        fail(
            "expected exactly one wildbuzzard-display below broker "
            f"{broker_pid}, found {len(matches)}"
        )
    return matches[0]


def host_frame(title: str) -> Any:
    matches = []
    for desktop_index in range(Atspi.get_desktop_count()):
        desktop = Atspi.get_desktop(desktop_index)
        for app_index in range(desktop.get_child_count()):
            try:
                app = desktop.get_child_at_index(app_index)
                if app is None or app.get_name() != "wildbuzzard-display":
                    continue
                for child_index in range(app.get_child_count()):
                    child = app.get_child_at_index(child_index)
                    if (
                        child is not None
                        and child.get_role_name() == "frame"
                        and child.get_name() == title
                    ):
                        matches.append(child)
            except Exception:
                continue
    if len(matches) != 1:
        fail(
            f"expected one wildbuzzard-display AT-SPI frame titled {title!r}, "
            f"found {len(matches)}"
        )
    return matches[0]


def activate_frame(frame: Any) -> None:
    for index in range(frame.get_n_actions()):
        if frame.get_action_name(index) == "default.activate":
            if not frame.do_action(index):
                fail("wildbuzzard-display AT-SPI activation was rejected")
            return
    fail("wildbuzzard-display frame exposes no default.activate action")


def read_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def wait_monitor_focus(status: Path) -> None:
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        try:
            window = read_json(status / "window.json")
            input_state = read_json(status / "input.json")
            if window.get("focused") is True and input_state.get("monitor_focused") is True:
                return
        except (FileNotFoundError, json.JSONDecodeError):
            pass
        time.sleep(0.05)
    fail("native toplevel and embedded monitor did not both acquire host focus")


def keycode(remote: dbus.Interface, code: int) -> None:
    remote.NotifyKeyboardKeycode(dbus.UInt32(code), True, timeout=DBUS_TIMEOUT)
    remote.NotifyKeyboardKeycode(dbus.UInt32(code), False, timeout=DBUS_TIMEOUT)


def keysym(remote: dbus.Interface, symbol: int) -> None:
    remote.NotifyKeyboardKeysym(dbus.UInt32(symbol), True, timeout=DBUS_TIMEOUT)
    remote.NotifyKeyboardKeysym(dbus.UInt32(symbol), False, timeout=DBUS_TIMEOUT)


def inject(broker_pid: int, title: str, replacement: str) -> None:
    if len(replacement) != 1 or not replacement.isascii() or not replacement.isalpha():
        fail("replacement must be exactly one ASCII letter")
    status = host_status_directory(broker_pid)
    activate_frame(host_frame(title))
    wait_monitor_focus(status)

    bus = dbus.SessionBus()
    root = bus.get_object(
        REMOTE_DESKTOP_SERVICE, REMOTE_DESKTOP_PATH, introspect=False
    )
    session_path = dbus.Interface(root, REMOTE_DESKTOP_INTERFACE).CreateSession(
        timeout=DBUS_TIMEOUT
    )
    session_object = bus.get_object(
        REMOTE_DESKTOP_SERVICE, session_path, introspect=False
    )
    remote = dbus.Interface(session_object, REMOTE_DESKTOP_SESSION_INTERFACE)
    operation_error: BaseException | None = None
    try:
        remote.Start(timeout=DBUS_TIMEOUT)
        # Creating or starting a desktop-control session can itself activate
        # shell UI. Never send a key based only on the pre-Start focus sample.
        wait_monitor_focus(status)
        keycode(remote, EVDEV_BACKSPACE)
        keysym(remote, ord(replacement.lower()))
        keysym(remote, XKB_RETURN)
    except BaseException as error:
        operation_error = error
    finally:
        # Even a timed-out Start may have reached Mutter, so Stop is attempted
        # whenever CreateSession returned an object, not only after Start's
        # reply. Process/bus teardown is a last resort and never silent proof.
        try:
            remote.Stop(timeout=DBUS_TIMEOUT)
        except Exception as stop_error:
            message = f"Mutter RemoteDesktop Stop failed: {stop_error}"
            if operation_error is None:
                operation_error = RuntimeError(message)
            else:
                operation_error.add_note(message)
    if operation_error is not None:
        raise operation_error


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--broker-pid", required=True, type=int)
    parser.add_argument("--title", required=True)
    parser.add_argument("--replacement", required=True)
    arguments = parser.parse_args()
    inject(arguments.broker_pid, arguments.title, arguments.replacement)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"GNOME host keyboard injection failed: {error}", file=__import__("sys").stderr)
        raise SystemExit(1)
