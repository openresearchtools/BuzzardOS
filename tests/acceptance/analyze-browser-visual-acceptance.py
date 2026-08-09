#!/usr/bin/python3
"""Measure native browser clarity and continuous host-pointer frame traces."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

from PIL import Image, ImageChops, ImageStat


MAGENTA = (255, 0, 168)
CYAN = (0, 255, 213)
ORANGE = (255, 138, 0)


def near(pixel: tuple[int, int, int], color: tuple[int, int, int], tolerance: int) -> bool:
    return all(abs(pixel[index] - color[index]) <= tolerance for index in range(3))

def count_near(image: Image.Image, color: tuple[int, int, int], tolerance: int) -> int:
    masks = [
        band.point(
            lambda value, target=target: (
                255 if abs(value - target) <= tolerance else 0
            )
        )
        for band, target in zip(image.convert("RGB").split(), color)
    ]
    combined = ImageChops.multiply(ImageChops.multiply(masks[0], masks[1]), masks[2])
    return combined.histogram()[255]


def color_bbox(
    image: Image.Image,
    color: tuple[int, int, int],
    tolerance: int = 0,
    crop: tuple[int, int, int, int] | None = None,
) -> tuple[int, int, int, int]:
    rgb = image.convert("RGB")
    left, top, right, bottom = crop or (0, 0, rgb.width, rgb.height)
    pixels = rgb.load()
    matches = [
        (x, y)
        for y in range(top, bottom)
        for x in range(left, right)
        if near(pixels[x, y], color, tolerance)
    ]
    if not matches:
        raise RuntimeError(f"marker {color} is absent")
    return (
        min(x for x, _ in matches),
        min(y for _, y in matches),
        max(x for x, _ in matches),
        max(y for _, y in matches),
    )


def stripe_metrics(image: Image.Image) -> dict[str, Any]:
    rgb = image.convert("RGB")
    bbox = color_bbox(rgb, CYAN, tolerance=2)
    pixels = rgb.load()
    best: tuple[float, int, float, float, float] | None = None
    for y in range(bbox[1] + 1, bbox[3]):
        values = [
            sum(pixels[x, y]) / 3.0
            for x in range(bbox[0] + 1, bbox[2])
        ]
        if len(values) < 2:
            continue
        contrast = (
            sum(abs(right - left) for left, right in zip(values, values[1:]))
            / (len(values) - 1)
            / 255.0
        )
        extreme_fraction = sum(value <= 8 or value >= 247 for value in values) / len(values)
        intermediate_fraction = 1.0 - extreme_fraction
        candidate = (
            contrast * extreme_fraction,
            y,
            contrast,
            extreme_fraction,
            intermediate_fraction,
        )
        if best is None or candidate[0] > best[0]:
            best = candidate
    if best is None:
        raise RuntimeError("stripe marker has no measurable interior row")
    return {
        "bbox_inclusive": list(bbox),
        "physical_width": bbox[2] - bbox[0] + 1,
        "physical_height": bbox[3] - bbox[1] + 1,
        "sample_y": best[1],
        "mean_adjacent_contrast": best[2],
        "extreme_pixel_fraction": best[3],
        "intermediate_pixel_fraction": best[4],
        "sharp": best[2] >= 0.90 and best[3] >= 0.98,
    }


def edge_metrics(image: Image.Image) -> dict[str, Any]:
    rgb = image.convert("RGB")
    bbox = color_bbox(rgb, ORANGE, tolerance=2)
    pixels = rgb.load()
    best: tuple[int, int, list[float]] | None = None
    # The fixture's four-pixel orange border is intentionally neither black
    # nor white. Measure only the interior hard-edge field so the border does
    # not become six false "filtered" pixels at its antialiased corners.
    interior_left = bbox[0] + 5
    interior_right = bbox[2] - 4
    for y in range(bbox[1] + 1, bbox[3]):
        values = [
            sum(pixels[x, y]) / 3.0
            for x in range(interior_left, interior_right)
        ]
        extreme = sum(value <= 8 or value >= 247 for value in values)
        if best is None or extreme > best[0]:
            best = (extreme, y, values)
    if best is None:
        raise RuntimeError("edge marker has no measurable interior row")
    values = best[2]
    intermediate = [index for index, value in enumerate(values) if 8 < value < 247]
    return {
        "bbox_inclusive": list(bbox),
        "sample_y": best[1],
        "intermediate_pixels": len(intermediate),
        "hard_edge": len(intermediate) <= 2,
    }


def image_metrics(
    path: Path, crop: tuple[int, int, int, int] | None = None
) -> dict[str, Any]:
    image = Image.open(path).convert("RGB")
    if crop is not None:
        image = image.crop(crop)
    return {
        "path": str(path),
        "width": image.width,
        "height": image.height,
        "fixture_bbox_inclusive": list(color_bbox(image, MAGENTA, tolerance=2)),
        "stripes": stripe_metrics(image),
        "edge": edge_metrics(image),
    }


def trace_metrics(
    trace_dir: Path,
    crop: tuple[int, int, int, int],
    reference_magenta_pixels: int,
) -> dict[str, Any]:
    frames = sorted(trace_dir.glob("frame-*.jpg"))
    if not frames:
        raise RuntimeError(f"no continuous trace frames in {trace_dir}")
    marker_counts: list[int] = []
    variances: list[float] = []
    for frame in frames:
        image = Image.open(frame).convert("RGB").crop(crop)
        marker_counts.append(count_near(image, MAGENTA, 35))
        # A uniform Starting/blank replacement has very low variance. Sampling
        # every 16th pixel keeps the all-frame check inexpensive.
        sample = image.resize(
            (max(1, image.width // 16), max(1, image.height // 16)),
            Image.Resampling.NEAREST,
        )
        variances.append(sum(ImageStat.Stat(sample.convert("L")).var))
    # JPEG chroma subsampling changes how many pixels remain within a fixed
    # RGB tolerance even when every frame contains the exact same fixture.
    # Establish the trace's encoded baseline from its best frame, while still
    # requiring that baseline to retain at least one quarter of the lossless
    # PNG marker. A real placeholder/blank frame then falls well below 70% of
    # that trace-local baseline instead of failing every otherwise continuous
    # frame merely because JPEG compressed magenta.
    encoded_reference = max(marker_counts)
    marker_baseline_valid = encoded_reference >= max(
        100, reference_magenta_pixels // 4
    )
    marker_floor = max(100, int(encoded_reference * 0.70))
    marker_loss_frames = sum(count < marker_floor for count in marker_counts)
    blank_frames = sum(variance < 100.0 for variance in variances)
    return {
        "directory": str(trace_dir),
        "frame_count": len(frames),
        "first_frame": frames[0].name,
        "last_frame": frames[-1].name,
        "minimum_fixture_marker_pixels": min(marker_counts),
        "maximum_fixture_marker_pixels": max(marker_counts),
        "lossless_fixture_marker_pixels": reference_magenta_pixels,
        "encoded_marker_baseline_valid": marker_baseline_valid,
        "encoded_marker_floor": marker_floor,
        "marker_loss_frames": marker_loss_frames,
        "minimum_luma_variance": min(variances),
        "blank_frames": blank_frames,
        "continuous_fixture": (
            marker_baseline_valid and marker_loss_frames == 0 and blank_frames == 0
        ),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host-firefox", required=True, type=Path)
    parser.add_argument("--guest-native", required=True, type=Path)
    parser.add_argument("--guest-presented", required=True, type=Path)
    parser.add_argument("--trace-dir", required=True, type=Path)
    parser.add_argument("--monitor-crop", required=True, help="left,top,right,bottom")
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()

    crop = tuple(int(value) for value in args.monitor_crop.split(","))
    if len(crop) != 4:
        raise RuntimeError("--monitor-crop requires left,top,right,bottom")
    host = image_metrics(args.host_firefox)
    guest = image_metrics(args.guest_native)
    presented = image_metrics(args.guest_presented, crop)
    presented_image = Image.open(args.guest_presented).convert("RGB").crop(crop)
    presented_marker_pixels = count_near(presented_image, MAGENTA, 35)
    trace = trace_metrics(args.trace_dir, crop, presented_marker_pixels)

    guest_fixture = guest["fixture_bbox_inclusive"]
    presented_fixture = presented["fixture_bbox_inclusive"]
    guest_extent = (
        guest_fixture[2] - guest_fixture[0] + 1,
        guest_fixture[3] - guest_fixture[1] + 1,
    )
    presented_extent = (
        presented_fixture[2] - presented_fixture[0] + 1,
        presented_fixture[3] - presented_fixture[1] + 1,
    )
    mapping_exact = guest_extent == presented_extent
    result = {
        "result": "pass"
        if (
            host["stripes"]["sharp"]
            and guest["stripes"]["sharp"]
            and presented["stripes"]["sharp"]
            and host["edge"]["hard_edge"]
            and guest["edge"]["hard_edge"]
            and presented["edge"]["hard_edge"]
            and mapping_exact
            and trace["continuous_fixture"]
        )
        else "fail",
        "host_firefox": host,
        "guest_native": guest,
        "guest_presented": presented,
        "guest_fixture_extent": list(guest_extent),
        "presented_fixture_extent": list(presented_extent),
        "native_mapping_exact": mapping_exact,
        "pointer_trace": trace,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0 if result["result"] == "pass" else 1


if __name__ == "__main__":
    raise SystemExit(main())
