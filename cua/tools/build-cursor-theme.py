#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Build original, static native Xcursor assets. Build-time only; no runtime renderer.

Artwork uses a 24-unit grid, a precise hotspot and a two-tone outline. The
stdlib-only rasterizer supersamples the small vector polygons for every size.
No upstream artwork is copied; unlisted tool shapes inherit distro Adwaita.
"""
import argparse
import math
from pathlib import Path
import struct

ACCENT = (229, 57, 53)
DARK = (20, 39, 46)
# Exact rasters for the supported 100/125/133/150/175/200% output scales.
# Each is rendered from geometry, never by resizing a lower-resolution image.
SIZES = (24, 30, 32, 36, 42, 48, 64)
IMAGE_TYPE = 0xFFFD0002

# (hotspot, polygons, Xcursor/Wayland shape aliases)
SHAPES = {
    "default": ((3, 3), [[(3, 3), (3, 19), (7.5, 15), (11, 22),
                           (14, 20.5), (10.5, 14), (17, 14)]],
                ["left_ptr", "arrow", "top_left_arrow"]),
    "text": ((12, 12), [[(8, 3), (16, 3), (16, 5), (13, 5), (13, 19),
                        (16, 19), (16, 21), (8, 21), (8, 19), (11, 19),
                        (11, 5), (8, 5)]], ["xterm"]),
    "crosshair": ((12, 12), [[(11, 3), (13, 3), (13, 11), (21, 11),
                            (21, 13), (13, 13), (13, 21), (11, 21),
                            (11, 13), (3, 13), (3, 11), (11, 11)]], ["cross"]),
    "pointer": ((8, 3), [[(7, 3), (9, 3), (10, 4), (10, 10), (12, 8),
                          (14, 9), (16, 9), (19, 11), (19, 17), (16, 21),
                          (9, 21), (5, 16), (3, 12), (5, 11), (7, 14),
                          (7, 4)]], ["hand1", "hand2", "pointing_hand"]),
    "ew-resize": ((12, 12), [[(3, 12), (8, 7), (8, 10.5), (16, 10.5),
                             (16, 7), (21, 12), (16, 17), (16, 13.5),
                             (8, 13.5), (8, 17)]],
                  ["col-resize", "sb_h_double_arrow", "h_double_arrow",
                   "e-resize", "w-resize", "left_side", "right_side"]),
    "move": ((12, 12), [[(12, 2), (16, 6), (13.5, 6), (13.5, 10.5),
                         (18, 10.5), (18, 8), (22, 12), (18, 16),
                         (18, 13.5), (13.5, 13.5), (13.5, 18), (16, 18),
                         (12, 22), (8, 18), (10.5, 18), (10.5, 13.5),
                         (6, 13.5), (6, 16), (2, 12), (6, 8), (6, 10.5),
                         (10.5, 10.5), (10.5, 6), (8, 6)]], ["fleur", "all-scroll"]),
}


def rotated(shape, angle, aliases):
    hotspot, polygons, _ = SHAPES[shape]
    c, s = math.cos(angle), math.sin(angle)
    return hotspot, [[(12 + (x - 12) * c - (y - 12) * s,
                      12 + (x - 12) * s + (y - 12) * c) for x, y in p]
                    for p in polygons], aliases


SHAPES["ns-resize"] = rotated("ew-resize", math.pi / 2,
    ["row-resize", "sb_v_double_arrow", "v_double_arrow", "n-resize", "s-resize",
     "top_side", "bottom_side"])
SHAPES["nwse-resize"] = rotated("ew-resize", math.pi / 4,
    ["nw-resize", "se-resize", "top_left_corner", "bottom_right_corner"])
SHAPES["nesw-resize"] = rotated("ew-resize", -math.pi / 4,
    ["ne-resize", "sw-resize", "top_right_corner", "bottom_left_corner"])
SHAPES["vertical-text"] = rotated("text", math.pi / 2, [])


def inside(x, y, polygon):
    result = False
    for (ax, ay), (bx, by) in zip(polygon, polygon[1:] + polygon[:1]):
        if (ay > y) != (by > y) and x < (bx - ax) * (y - ay) / (by - ay) + ax:
            result = not result
    return result


def edge_distance(x, y, polygon):
    result = float("inf")
    for (ax, ay), (bx, by) in zip(polygon, polygon[1:] + polygon[:1]):
        dx, dy = bx - ax, by - ay
        t = max(0, min(1, ((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)))
        result = min(result, math.hypot(x - ax - t * dx, y - ay - t * dy))
    return result


def pixels(polygons, size):
    result = bytearray()
    for y in range(size):
        for x in range(size):
            totals = [0, 0, 0, 0]
            for sy in range(4):
                for sx in range(4):
                    px, py = (x + (sx + .5) / 4) * 24 / size, (y + (sy + .5) / 4) * 24 / size
                    if any(inside(px, py, p) for p in polygons):
                        colour = ACCENT
                    else:
                        distance = min(edge_distance(px, py, p) for p in polygons)
                        colour = DARK if distance < .65 else ((255, 255, 255) if distance < 1 else None)
                    if colour is not None:
                        for i, component in enumerate((*colour, 255)):
                            totals[i] += component
            r, g, b, a = [(value + 8) // 16 for value in totals]
            result.extend((b, g, r, a))  # Xcursor premultiplied little-endian ARGB
    return bytes(result)


def xcursor(shape):
    hotspot, polygons, _ = shape
    chunks = []
    for size in SIZES:
        x, y = [round(value * size / 24) for value in hotspot]
        chunks.append(struct.pack("<9I", 36, IMAGE_TYPE, size, 1, size, size, x, y, 0)
                      + pixels(polygons, size))
    offset = 16 + 12 * len(chunks)
    toc = bytearray()
    for size, chunk in zip(SIZES, chunks):
        toc.extend(struct.pack("<3I", IMAGE_TYPE, size, offset))
        offset += len(chunk)
    return struct.pack("<4I", 0x72756358, 16, 0x10000, len(chunks)) + toc + b"".join(chunks)


def build(destination):
    destination.mkdir(parents=True, exist_ok=True)
    (destination / "index.theme").write_text(
        "[Icon Theme]\nName=Buzzard OS Agent\nComment=Compact red agent cursors\nInherits=Adwaita\n")
    cursors = destination / "cursors"
    cursors.mkdir(exist_ok=True)
    for name, shape in SHAPES.items():
        (cursors / name).write_bytes(xcursor(shape))
        for alias in shape[2]:
            (cursors / alias).symlink_to(name)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    build(parser.parse_args().destination)
