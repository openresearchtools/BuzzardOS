#!/usr/bin/python3
"""Drive and verify Buzzard OS's real host Wayland application frame.

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
from PIL import Image, ImageChops  # noqa: E402


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
GUEST_DESKTOP_RGB = (32, 34, 37)
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
    runtime = json.loads(runtime_path.read_text())
    machine = json.loads((runtime_path.parent / "machine.json").read_text())
    machine_id = str(machine["id"]).replace("-", "")
    status_dir = (
        Path(os.environ["XDG_RUNTIME_DIR"])
        / "buzzardos"
        / "machines"
        / machine_id
        / "host-status"
    )
    runtime["display"] = {
        "window": json.loads((status_dir / "window.json").read_text()),
        "presentation": json.loads((status_dir / "presentation.json").read_text()),
    }
    return runtime


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
            if app.get_name() != "buzzardos-display":
                continue
            for index in range(app.get_child_count()):
                child = app.get_child_at_index(index)
                if child is not None and child.get_role_name() == "frame":
                    return child
        except Exception:
            continue
    fail("buzzardos-display host frame is absent from host AT-SPI")


def frame_size() -> tuple[int, int]:
    extents = host_frame().get_extents(Atspi.CoordType.SCREEN)
    return int(extents.width), int(extents.height)


def menu_item_window_center(name: str) -> tuple[float, float]:
    """Return a top-menu centre in host-window logical coordinates.

    GTK exposes reliable window-relative menu geometry even on Wayland, where
    global application coordinates are intentionally unavailable to AT-SPI.
    This keeps acceptance independent of translated labels and font metrics.
    """

    pending = [host_frame()]
    while pending:
        node = pending.pop()
        try:
            if node.get_role_name() == "menu item" and node.get_name() == name:
                extents = node.get_extents(Atspi.CoordType.WINDOW)
                if extents.width <= 0 or extents.height <= 0:
                    fail(f"host menu {name!r} has no accessible geometry")
                return (
                    float(extents.x) + float(extents.width) / 2.0,
                    float(extents.y) + float(extents.height) / 2.0,
                )
            for index in range(node.get_child_count()):
                child = node.get_child_at_index(index)
                if child is not None:
                    pending.append(child)
        except Exception:
            continue
    fail(f"host menu {name!r} is absent from host AT-SPI")


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


def locate_monitor_from_full_width_shell_line(
    path: Path,
    width: int,
    height: int,
) -> tuple[int, int]:
    """Locate a fully occupied guest even when applications cover its desktop.

    The reference shell reserves the complete output width for the persistent
    bottom panel.  Its separator and lower border contain full-width solid
    scanlines.  Selecting the lowest exact-width run finds the monitor's
    physical bottom independently of application pixels, host-window
    position, or Wayland's intentional lack of global toplevel coordinates.
    """

    image = Image.open(path).convert("RGB")
    pixels = image.load()
    candidates: list[tuple[int, int]] = []
    for y in range(image.height):
        x = 0
        while x < image.width:
            colour = pixels[x, y]
            end = x + 1
            while end < image.width and pixels[end, y] == colour:
                end += 1
            if end - x == width:
                monitor_y = y - height + 1
                if monitor_y >= 0 and x + width <= image.width:
                    candidates.append((x, monitor_y))
            x = end
    if not candidates:
        fail(
            "could not locate the full-width guest shell panel in "
            f"{path} for a {width}x{height} monitor"
        )
    return candidates[-1]


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


def pointer_continuity_sweep(
    host: HostSession,
    runtime_path: Path,
    artifact_dir: Path,
) -> dict[str, Any]:
    """Record the real host output while moving over the complete guest.

    A former regression treated a valid dmabuf cursor as a fatal guest-display
    error.  The monitor consequently exposed the host-owned white "Machine
    failed" state for one frame whenever the cursor changed over some clients.
    Static screenshots and API exit status cannot detect that failure, so this
    check records at 120 fps and rejects any catastrophic whole-monitor
    transition.
    """

    runtime = runtime_snapshot(runtime_path)
    assert_native_runtime(runtime)
    presentation = runtime["display"]["presentation"]
    width = int(presentation["width"])
    height = int(presentation["height"])
    before = host.screenshot("22-pointer-sweep-before")
    monitor_x, monitor_y = locate_monitor_from_full_width_shell_line(
        before,
        width,
        height,
    )
    image = Image.open(before)
    crop_right = image.width - monitor_x - width
    crop_bottom = image.height - monitor_y - height
    if min(monitor_x, monitor_y, crop_right, crop_bottom) < 0:
        fail("guest monitor crop exceeds the host ScreenCast frame")

    frame_dir = artifact_dir / "pointer-sweep-frames"
    frame_dir.mkdir(parents=True, exist_ok=True)
    command = [
        "gst-launch-1.0",
        "-q",
        "pipewiresrc",
        f"path={host.node_id}",
        "do-timestamp=true",
        "num-buffers=240",
        "!",
        "videocrop",
        f"left={monitor_x}",
        f"right={crop_right}",
        f"top={monitor_y}",
        f"bottom={crop_bottom}",
        "!",
        "videoconvert",
        "!",
        "videorate",
        "!",
        "video/x-raw,framerate=120/1",
        "!",
        "jpegenc",
        "quality=94",
        "!",
        "multifilesink",
        f"location={frame_dir}/frame-%06d.jpg",
    ]
    recorder = subprocess.Popen(
        command,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    time.sleep(0.15)
    rows = [
        monitor_y + round(height * fraction)
        for fraction in (0.03, 0.12, 0.25, 0.48, 0.70, 0.90, 0.97)
    ]
    for repeat in range(4):
        for row_index, y in enumerate(rows):
            left_to_right = (repeat + row_index) % 2 == 0
            if left_to_right:
                columns = range(monitor_x + 20, monitor_x + width - 20, 30)
            else:
                columns = range(monitor_x + width - 21, monitor_x + 19, -30)
            for x in columns:
                host.move_to_physical(x, y)
                time.sleep(0.002)
    _, stderr = recorder.communicate(timeout=20)
    if recorder.returncode != 0:
        fail(f"120-fps pointer recording failed: {stderr.strip()}")

    frames = sorted(frame_dir.glob("frame-*.jpg"))
    if len(frames) < 120:
        fail(f"pointer recording produced only {len(frames)} frames")
    previous = None
    maximum = 0.0
    maximum_pair: list[int] | None = None
    for index, path in enumerate(frames):
        current = Image.open(path).convert("RGB").resize((160, 100))
        if previous is not None:
            difference = ImageChops.difference(previous, current)
            pixels = (
                difference.get_flattened_data()
                if hasattr(difference, "get_flattened_data")
                else difference.getdata()
            )
            changed = sum(1 for pixel in pixels if max(pixel) > 75) / 16000
            if changed > maximum:
                maximum = changed
                maximum_pair = [index - 1, index]
        previous = current

    after = runtime_snapshot(runtime_path)
    if after.get("state") != "running":
        fail(f"pointer sweep changed machine state to {after.get('state')!r}")
    if maximum >= 0.30:
        fail(
            "pointer motion exposed a catastrophic monitor transition: "
            f"{maximum:.2%} of sampled pixels changed between frames "
            f"{maximum_pair}"
        )
    result = {
        "recorded_frames": len(frames),
        "rate_fps": 120,
        "maximum_changed_fraction": maximum,
        "maximum_pair": maximum_pair,
        "machine_state_after": after.get("state"),
        "dropped_frames_after": after["display"]["presentation"]["dropped_frames"],
        "passed": True,
    }
    write_json(artifact_dir / "pointer-sweep-result.json", result)
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("machine_dir", type=Path)
    parser.add_argument("machine")
    parser.add_argument("artifact_dir", type=Path)
    args = parser.parse_args()

    machine_dir = args.machine_dir.resolve()
    runtime_path = machine_dir / "runtime.json"
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

        # Open every host menu with real pointer clicks.  Their popup pixels
        # and AT-SPI objects are host-owned; no coordinates enter the monitor.
        current = host.screenshot("12-before-machine-menu")
        monitor = locate_monitor(current)
        current_size = frame_size()
        x0 = monitor[0] - max(2, round(2 * scale))
        outer_y = monitor[1] - round(75 * scale)
        for index, name in enumerate(
            ("Machine", "Ports", "Devices", "Settings"), start=13
        ):
            menu_x, menu_y = menu_item_window_center(name)
            host.click_physical(
                x0 + menu_x * scale,
                outer_y + menu_y * scale,
            )
            time.sleep(0.5)
            host.screenshot(f"{index:02d}-{name.lower()}-menu-open")
            host.keysym(0xFF1B)
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
        pointer_sweep = pointer_continuity_sweep(
            host,
            runtime_path,
            artifact_dir,
        )

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
            "ports_menu": True,
            "devices_menu": True,
            "settings_menu": True,
            "maximize_restore": True,
            "minimize_restore": True,
            "pointer_continuity_sweep": pointer_sweep,
            "host_scale": host.host_scale,
        },
    )
    print(
        "Buzzard OS native host Wayland frame acceptance passed; "
        f"artifacts: {artifact_dir}"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"host-wayland-frame-acceptance: {error}", file=sys.stderr)
        raise
