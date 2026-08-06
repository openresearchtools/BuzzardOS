#!/usr/bin/python3
"""Drive and verify Wild Buzzard's real host Wayland application frame.

This is intentionally a hardware/session acceptance test rather than a unit
test.  It uses GNOME Mutter's own ScreenCast and RemoteDesktop D-Bus APIs so
the titlebar, menus, borders, and buttons are observed and driven through the
same host compositor as a human pointer.  No guest input API is used here.
"""

from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
from typing import Any, Callable

import dbus
from dbus.mainloop.glib import DBusGMainLoop
import gi

gi.require_version("Atspi", "2.0")
gi.require_version("GLib", "2.0")
from gi.repository import Atspi, GLib  # noqa: E402
from PIL import Image  # noqa: E402


DISPLAY_CONFIG_SERVICE = "org.gnome.Mutter.DisplayConfig"
DISPLAY_CONFIG_PATH = "/org/gnome/Mutter/DisplayConfig"
DISPLAY_CONFIG_INTERFACE = "org.gnome.Mutter.DisplayConfig"
REMOTE_DESKTOP_SERVICE = "org.gnome.Mutter.RemoteDesktop"
REMOTE_DESKTOP_PATH = "/org/gnome/Mutter/RemoteDesktop"
REMOTE_DESKTOP_INTERFACE = "org.gnome.Mutter.RemoteDesktop"
REMOTE_DESKTOP_SESSION_INTERFACE = "org.gnome.Mutter.RemoteDesktop.Session"
SCREENCAST_SERVICE = "org.gnome.Mutter.ScreenCast"
SCREENCAST_PATH = "/org/gnome/Mutter/ScreenCast"
SCREENCAST_INTERFACE = "org.gnome.Mutter.ScreenCast"
SCREENCAST_SESSION_INTERFACE = "org.gnome.Mutter.ScreenCast.Session"
SCREENCAST_STREAM_INTERFACE = "org.gnome.Mutter.ScreenCast.Stream"
GUEST_DESKTOP_RGB = (24, 55, 78)
BTN_LEFT = 272


def fail(message: str) -> None:
    raise RuntimeError(message)


def plain(value: Any) -> Any:
    if isinstance(value, dict):
        return {str(key): plain(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [plain(item) for item in value]
    if isinstance(value, (dbus.Boolean, bool)):
        return bool(value)
    if isinstance(value, (dbus.Int16, dbus.Int32, dbus.Int64,
                          dbus.UInt16, dbus.UInt32, dbus.UInt64, int)):
        return int(value)
    if isinstance(value, (dbus.Double, float)):
        return float(value)
    return str(value)


def write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(plain(value), indent=2, sort_keys=True) + "\n")


class HostSession:
    def __init__(self, artifact_dir: Path) -> None:
        DBusGMainLoop(set_as_default=True)
        self.bus = dbus.SessionBus()
        self.artifact_dir = artifact_dir
        self.artifact_dir.mkdir(parents=True, exist_ok=True)
        self.screen_session = None
        self.screen_session_interface = None
        self.screen_stream_interface = None
        self.remote_session = None
        self.remote = None
        self.node_id: int | None = None
        self.logical_width, self.logical_height, self.host_scale = (
            self._current_monitor_geometry()
        )

    def __enter__(self) -> "HostSession":
        self._start_screencast()
        self._start_remote_desktop()
        return self

    def __exit__(self, exc_type, exc, traceback) -> None:
        if self.screen_stream_interface is not None:
            try:
                self.screen_stream_interface.Stop()
            except dbus.DBusException:
                pass
        if self.screen_session_interface is not None:
            try:
                self.screen_session_interface.Stop()
            except dbus.DBusException:
                pass
        if self.remote is not None:
            try:
                self.remote.Stop()
            except dbus.DBusException:
                pass

    def _current_monitor_geometry(self) -> tuple[int, int, float]:
        obj = self.bus.get_object(DISPLAY_CONFIG_SERVICE, DISPLAY_CONFIG_PATH)
        state = dbus.Interface(obj, DISPLAY_CONFIG_INTERFACE).GetCurrentState()
        monitors = state[1]
        logical_monitors = state[2]
        if len(monitors) != 1 or len(logical_monitors) != 1:
            fail("host frame acceptance currently requires exactly one active host monitor")
        current_mode = None
        for mode in monitors[0][1]:
            if bool(mode[6].get("is-current", False)):
                current_mode = mode
                break
        if current_mode is None:
            fail("Mutter did not report a current physical monitor mode")
        physical_width = int(current_mode[1])
        physical_height = int(current_mode[2])
        scale = float(logical_monitors[0][2])
        return (
            round(physical_width / scale),
            round(physical_height / scale),
            scale,
        )

    def _start_screencast(self) -> None:
        root = self.bus.get_object(SCREENCAST_SERVICE, SCREENCAST_PATH)
        session_path = dbus.Interface(root, SCREENCAST_INTERFACE).CreateSession(
            {"disable-animations": dbus.Boolean(True)}
        )
        self.screen_session = self.bus.get_object(
            SCREENCAST_SERVICE, session_path
        )
        self.screen_session_interface = dbus.Interface(
            self.screen_session, SCREENCAST_SESSION_INTERFACE
        )
        stream_path = self.screen_session_interface.RecordArea(
            0,
            0,
            self.logical_width,
            self.logical_height,
            {"cursor-mode": dbus.UInt32(1)},
        )
        stream = self.bus.get_object(SCREENCAST_SERVICE, stream_path)
        self.screen_stream_interface = dbus.Interface(
            stream, SCREENCAST_STREAM_INTERFACE
        )
        loop = GLib.MainLoop()

        def on_stream_added(node_id: int) -> None:
            self.node_id = int(node_id)
            loop.quit()

        stream.connect_to_signal(
            "PipeWireStreamAdded",
            on_stream_added,
            dbus_interface=SCREENCAST_STREAM_INTERFACE,
        )
        GLib.timeout_add_seconds(10, lambda: (loop.quit(), False)[1])
        self.screen_session_interface.Start()
        loop.run()
        if self.node_id is None:
            fail("Mutter ScreenCast did not publish a PipeWire node")

    def _start_remote_desktop(self) -> None:
        root = self.bus.get_object(REMOTE_DESKTOP_SERVICE, REMOTE_DESKTOP_PATH)
        session_path = dbus.Interface(
            root, REMOTE_DESKTOP_INTERFACE
        ).CreateSession()
        self.remote_session = self.bus.get_object(
            REMOTE_DESKTOP_SERVICE, session_path
        )
        self.remote = dbus.Interface(
            self.remote_session, REMOTE_DESKTOP_SESSION_INTERFACE
        )
        self.remote.Start()

    def screenshot(self, name: str) -> Path:
        if self.node_id is None:
            fail("screen-cast stream is not running")
        path = self.artifact_dir / f"{name}.png"
        command = [
            "gst-launch-1.0",
            "-q",
            "pipewiresrc",
            f"path={self.node_id}",
            "do-timestamp=true",
            "num-buffers=1",
            "!",
            "videoconvert",
            "!",
            "video/x-raw,format=RGBA",
            "!",
            "pngenc",
            "!",
            "filesink",
            f"location={path}",
        ]
        completed = subprocess.run(
            command, text=True, capture_output=True, timeout=20, check=False
        )
        if completed.returncode != 0 or not path.is_file():
            fail(
                "failed to capture host compositor output: "
                f"{completed.stderr.strip()}"
            )
        return path

    def move_to_physical(self, x: float, y: float) -> None:
        # Relative motion is universally supported.  Two very large negative
        # moves clamp the pointer to the monitor's logical (0, 0), giving us a
        # reliable absolute origin without a ScreenCast stream identifier.
        self.remote.NotifyPointerMotionRelative(-10000.0, -10000.0)
        self.remote.NotifyPointerMotionRelative(
            float(x) / self.host_scale, float(y) / self.host_scale
        )

    def click_physical(self, x: float, y: float) -> None:
        self.move_to_physical(x, y)
        self.remote.NotifyPointerButton(dbus.Int32(BTN_LEFT), True)
        self.remote.NotifyPointerButton(dbus.Int32(BTN_LEFT), False)

    def keysym(self, keysym: int) -> None:
        self.remote.NotifyKeyboardKeysym(dbus.UInt32(keysym), True)
        self.remote.NotifyKeyboardKeysym(dbus.UInt32(keysym), False)

    def drag_physical(
        self, x: float, y: float, dx: float, dy: float, steps: int = 12
    ) -> None:
        self.move_to_physical(x, y)
        self.remote.NotifyPointerButton(dbus.Int32(BTN_LEFT), True)
        for _ in range(steps):
            self.remote.NotifyPointerMotionRelative(
                float(dx) / self.host_scale / steps,
                float(dy) / self.host_scale / steps,
            )
            time.sleep(0.015)
        self.remote.NotifyPointerButton(dbus.Int32(BTN_LEFT), False)


def runtime_snapshot(runtime_path: Path) -> dict[str, Any]:
    return json.loads(runtime_path.read_text())


def wait_until(
    predicate: Callable[[], bool], message: str, timeout: float = 12.0
) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    fail(message)


def host_frame() -> Any:
    for app in Atspi.get_desktop(0):
        try:
            if app.get_name() != "wildbuzzard-display":
                continue
            for index in range(app.get_child_count()):
                child = app.get_child_at_index(index)
                if child is not None and child.get_role_name() == "frame":
                    return child
        except Exception:
            continue
    fail("wildbuzzard-display host frame is absent from host AT-SPI")


def frame_size() -> tuple[int, int]:
    extents = host_frame().get_extents(Atspi.CoordType.SCREEN)
    return int(extents.width), int(extents.height)


def locate_monitor(path: Path) -> tuple[int, int, int, int]:
    image = Image.open(path).convert("RGB")
    pixels = image.load()
    min_x, min_y = image.width, image.height
    max_x = max_y = -1
    count = 0
    for y in range(image.height):
        for x in range(image.width):
            if pixels[x, y] == GUEST_DESKTOP_RGB:
                min_x = min(min_x, x)
                min_y = min(min_y, y)
                max_x = max(max_x, x)
                max_y = max(max_y, y)
                count += 1
    if count < 10000:
        fail(
            f"could not locate guest monitor colour in {path}; "
            f"only {count} matching pixels"
        )
    return min_x, min_y, max_x, max_y


def largest_guest_colour_component(
    path: Path,
) -> tuple[int, tuple[int, int, int, int] | None]:
    image = Image.open(path).convert("RGB")
    pixels = image.load()
    remaining = {
        (x, y)
        for y in range(image.height)
        for x in range(image.width)
        if pixels[x, y] == GUEST_DESKTOP_RGB
    }
    largest: list[tuple[int, int]] = []
    while remaining:
        component = [remaining.pop()]
        cursor = 0
        while cursor < len(component):
            x, y = component[cursor]
            cursor += 1
            for neighbour in (
                (x - 1, y),
                (x + 1, y),
                (x, y - 1),
                (x, y + 1),
            ):
                if neighbour in remaining:
                    remaining.remove(neighbour)
                    component.append(neighbour)
        if len(component) > len(largest):
            largest = component
    if not largest:
        return 0, None
    return len(largest), (
        min(x for x, _ in largest),
        min(y for _, y in largest),
        max(x for x, _ in largest),
        max(y for _, y in largest),
    )


def native_runtime_is_valid(runtime: dict[str, Any]) -> bool:
    window = runtime["display"]["window"]
    presentation = runtime["display"]["presentation"]
    expected_width = math.ceil(
        int(window["width"]) * int(presentation["scale_120"]) / 120
    )
    expected_height = math.ceil(
        int(window["height"]) * int(presentation["scale_120"]) / 120
    )
    return not (
        presentation["transport"] != "dmabuf"
        or not presentation["native_resolution"]
        or int(presentation["width"]) != expected_width
        or int(presentation["height"]) != expected_height
        or int(presentation["viewport_width"]) != int(window["width"])
        or int(presentation["viewport_height"]) != int(window["height"])
    )


def assert_native_runtime(runtime: dict[str, Any]) -> None:
    if not native_runtime_is_valid(runtime):
        fail("guest monitor is stretched or is not using its native dmabuf mode")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("portable_folder", type=Path)
    parser.add_argument("machine")
    parser.add_argument("artifact_dir", type=Path)
    args = parser.parse_args()

    portable_folder = args.portable_folder.resolve()
    runtime_path = portable_folder / "vm" / args.machine / "runtime.json"
    if not runtime_path.is_file():
        fail(f"runtime metadata is missing: {runtime_path}")
    runtime = runtime_snapshot(runtime_path)
    if runtime.get("state") != "running":
        fail(f"machine must be running, got {runtime.get('state')!r}")
    assert_native_runtime(runtime)

    artifact_dir = args.artifact_dir.resolve()
    artifact_dir.mkdir(parents=True, exist_ok=True)
    write_json(artifact_dir / "runtime-before.json", runtime)

    with HostSession(artifact_dir) as host:
        before_path = host.screenshot("01-before")
        before_monitor = locate_monitor(before_path)
        before_frame = frame_size()
        scale = host.host_scale
        monitor_x, monitor_y = before_monitor[0], before_monitor[1]
        outer_x = monitor_x - max(2, round(2 * scale))
        outer_y = monitor_y - round(75 * scale)
        outer_width = round(before_frame[0] * scale)
        outer_height = round(before_frame[1] * scale)

        geometry = {
            "host_logical_size": [host.logical_width, host.logical_height],
            "host_scale": scale,
            "host_frame_logical_size": list(before_frame),
            "guest_monitor_physical_bbox": list(before_monitor),
            "derived_outer_physical": [
                outer_x,
                outer_y,
                outer_width,
                outer_height,
            ],
        }
        write_json(artifact_dir / "geometry-before.json", geometry)

        # Move the complete host application by dragging only its outer
        # titlebar.  The guest monitor mode must remain unchanged.
        host.drag_physical(
            outer_x + outer_width / 2,
            outer_y + 23 * scale,
            80 * scale,
            30 * scale,
        )
        time.sleep(0.8)
        moved_path = host.screenshot("02-titlebar-drag")
        moved_monitor = locate_monitor(moved_path)
        if moved_monitor[:2] == before_monitor[:2]:
            fail("outer titlebar drag did not move the host application")
        moved_runtime = runtime_snapshot(runtime_path)
        if (
            moved_runtime["display"]["window"]["width"]
            != runtime["display"]["window"]["width"]
            or moved_runtime["display"]["window"]["height"]
            != runtime["display"]["window"]["height"]
        ):
            fail("moving the host application changed the guest monitor mode")

        # Recompute after the move, then drive all four edges and all four
        # corners.  A normal-sized window is reduced inward to stay clear of
        # monitor edges and docks; an already-small window is expanded
        # outward so the test never confuses a minimum-size clamp with a
        # broken border.
        if runtime["display"]["window"]["height"] >= 500:
            resize_cases = [
                ("left", "left", 24, 0),
                ("right", "right", -24, 0),
                ("top", "top", 0, 24),
                ("bottom", "bottom", 0, -24),
                ("top-left", "top-left", 18, 18),
                ("top-right", "top-right", -18, 18),
                ("bottom-left", "bottom-left", 18, -18),
                ("bottom-right", "bottom-right", -18, -18),
            ]
        else:
            resize_cases = [
                ("left", "left", -24, 0),
                ("right", "right", 24, 0),
                ("top", "top", 0, -24),
                ("bottom", "bottom", 0, 24),
                ("top-left", "top-left", -18, -18),
                ("top-right", "top-right", 18, -18),
                ("bottom-left", "bottom-left", -18, 18),
                ("bottom-right", "bottom-right", 18, 18),
            ]
        resize_results = []
        for index, (name, edge, dx_logical, dy_logical) in enumerate(
            resize_cases, start=3
        ):
            capture = host.screenshot(f"{index:02d}-{name}-before")
            monitor = locate_monitor(capture)
            old_size = frame_size()
            old_runtime = runtime_snapshot(runtime_path)
            presentation = old_runtime["display"]["presentation"]
            x0 = monitor[0] - max(2, round(2 * scale))
            y0 = monitor[1] - round(75 * scale)
            physical_width = int(presentation["width"])
            physical_height = int(presentation["height"])
            width = physical_width + max(3, round(3 * scale))
            height = (
                round(75 * scale)
                + physical_height
                + round(34 * scale)
            )
            left = monitor[0] - max(1, round(1.5 * scale))
            right = monitor[0] + physical_width + max(1, round(scale))
            top = y0 + 1
            bottom = (
                monitor[1]
                + physical_height
                + round(34 * scale)
                + max(1, round(2 * scale))
            )
            mid_x = monitor[0] + physical_width / 2
            mid_y = monitor[1] + physical_height / 2
            targets = {
                "left": (left, mid_y),
                "right": (right, mid_y),
                "top": (mid_x, top),
                "bottom": (mid_x, bottom),
                "top-left": (left, top),
                "top-right": (right, top),
                "bottom-left": (left, bottom),
                "bottom-right": (right, bottom),
            }
            target = targets[edge]
            host.drag_physical(
                target[0],
                target[1],
                dx_logical * scale,
                dy_logical * scale,
            )
            wait_until(
                lambda old_size=old_size: frame_size() != old_size,
                f"{name} host border did not resize the native window",
            )
            new_size = frame_size()
            wait_until(
                lambda new_size=new_size: (
                    runtime_snapshot(runtime_path)["display"]["window"]["width"]
                    == new_size[0]
                    and runtime_snapshot(runtime_path)["display"]["window"]["height"]
                    == new_size[1] - 109
                ),
                f"{name} resize did not propagate to the guest monitor",
            )
            wait_until(
                lambda: native_runtime_is_valid(runtime_snapshot(runtime_path)),
                f"{name} resize did not settle on a native dmabuf mode",
            )
            new_runtime = runtime_snapshot(runtime_path)
            assert_native_runtime(new_runtime)
            resize_results.append(
                {
                    "edge": name,
                    "host_frame_before": list(old_size),
                    "host_frame_after": list(new_size),
                    "guest_monitor_before": [
                        old_runtime["display"]["window"]["width"],
                        old_runtime["display"]["window"]["height"],
                    ],
                    "guest_monitor_after": [
                        new_runtime["display"]["window"]["width"],
                        new_runtime["display"]["window"]["height"],
                    ],
                }
            )
        write_json(artifact_dir / "resize-results.json", resize_results)
        host.screenshot("11-after-eight-direction-resize")

        # Open both host menus with real pointer clicks.  Their popup pixels
        # and AT-SPI objects are host-owned; no coordinates enter the monitor.
        current = host.screenshot("12-before-machine-menu")
        monitor = locate_monitor(current)
        current_size = frame_size()
        x0 = monitor[0] - max(2, round(2 * scale))
        menu_y = monitor[1] - round(14 * scale)
        host.click_physical(x0 + 38 * scale, menu_y)
        time.sleep(0.5)
        host.screenshot("13-machine-menu-open")
        host.click_physical(x0 + 38 * scale, menu_y)
        host.click_physical(x0 + 104 * scale, menu_y)
        time.sleep(0.5)
        host.screenshot("14-settings-menu-open")
        host.keysym(0xFF1B)  # Escape closes the host popover deterministically.
        time.sleep(0.3)

        # Drive the native maximize and restore buttons by physical pointer.
        # GNOME's CSD button centres are stable offsets from the right edge.
        x0 = monitor[0] - max(2, round(2 * scale))
        y0 = monitor[1] - round(75 * scale)
        width = round(current_size[0] * scale)
        title_y = y0 + 23 * scale
        maximize_x = x0 + width - 66 * scale
        host.click_physical(maximize_x, title_y)
        wait_until(
            lambda: bool(
                runtime_snapshot(runtime_path)["display"]["window"]["maximized"]
            ),
            "native maximize button did not maximize the host application",
        )
        wait_until(
            lambda: native_runtime_is_valid(runtime_snapshot(runtime_path)),
            "maximized guest monitor did not settle on a native dmabuf mode",
        )
        maximized = runtime_snapshot(runtime_path)
        assert_native_runtime(maximized)
        write_json(artifact_dir / "runtime-maximized.json", maximized)
        host.screenshot("15-maximized")

        # The right-edge offsets are recomputed from the maximized frame.
        max_monitor = locate_monitor(host.screenshot("16-maximized-before-restore"))
        max_size = frame_size()
        max_x0 = max_monitor[0] - max(2, round(2 * scale))
        max_y0 = max_monitor[1] - round(75 * scale)
        restore_x = max_x0 + round(max_size[0] * scale) - 66 * scale
        host.click_physical(restore_x, max_y0 + 23 * scale)
        wait_until(
            lambda: not bool(
                runtime_snapshot(runtime_path)["display"]["window"]["maximized"]
            ),
            "native restore button did not restore the host application",
        )
        wait_until(
            lambda: native_runtime_is_valid(runtime_snapshot(runtime_path)),
            "restored guest monitor did not settle on a native dmabuf mode",
        )
        restored = runtime_snapshot(runtime_path)
        assert_native_runtime(restored)
        write_json(artifact_dir / "runtime-restored.json", restored)
        host.screenshot("17-restored")

        # Drive native minimize, then restore the same application through its
        # host AT-SPI window action so acceptance can continue.
        restored_monitor = locate_monitor(
            host.screenshot("18-restored-before-minimize")
        )
        restored_size = frame_size()
        restored_x0 = restored_monitor[0] - max(2, round(2 * scale))
        restored_y0 = restored_monitor[1] - round(75 * scale)
        minimize_x = (
            restored_x0 + round(restored_size[0] * scale) - 112 * scale
        )
        host.click_physical(minimize_x, restored_y0 + 23 * scale)
        time.sleep(0.8)
        minimized_path = host.screenshot("19-minimized-host-desktop")
        minimized_pixels, _ = largest_guest_colour_component(minimized_path)
        if minimized_pixels >= 10000:
            fail("native minimize button left the host application visible")

        # Wayland xdg_toplevel has a minimize request but deliberately has no
        # minimized-state configure event.  Verify the actual compositor
        # output above, then restore exactly as a human does in GNOME's
        # overview.  The thumbnail is found from the guest monitor itself.
        host.keysym(0xFFEB)  # Super_L opens the compositor overview.
        time.sleep(0.8)
        overview_path = host.screenshot("20-overview-restore-target")
        overview_pixels, overview_bbox = largest_guest_colour_component(
            overview_path
        )
        if overview_pixels < 10000 or overview_bbox is None:
            fail("GNOME overview did not expose the minimized machine window")
        host.click_physical(
            (overview_bbox[0] + overview_bbox[2]) / 2,
            (overview_bbox[1] + overview_bbox[3]) / 2,
        )
        wait_until(
            lambda: bool(
                runtime_snapshot(runtime_path)["display"]["window"]["focused"]
            ),
            "GNOME overview did not restore the minimized host application",
        )
        host.screenshot("21-after-minimize-restore")

    final_runtime = runtime_snapshot(runtime_path)
    assert_native_runtime(final_runtime)
    write_json(artifact_dir / "runtime-after.json", final_runtime)
    write_json(
        artifact_dir / "result.json",
        {
            "result": "pass",
            "titlebar_drag": True,
            "resize_edges": 4,
            "resize_corners": 4,
            "machine_menu": True,
            "settings_menu": True,
            "maximize_restore": True,
            "minimize_restore": True,
            "host_scale": host.host_scale,
        },
    )
    print(
        "Wild Buzzard native host Wayland frame acceptance passed; "
        f"artifacts: {artifact_dir}"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"host-wayland-frame-acceptance: {error}", file=sys.stderr)
        raise
