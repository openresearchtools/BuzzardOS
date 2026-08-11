#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later

"""Generate every Wild Buzzard logo variant from one reviewed geometry file."""

from __future__ import annotations

import argparse
from decimal import Decimal
import hashlib
import json
import os
from pathlib import Path
import re
import sys
import tempfile
from typing import Any
from xml.sax.saxutils import escape


SCRIPT_DIR = Path(__file__).resolve().parent
REPOSITORY = SCRIPT_DIR.parents[1]
SOURCE = SCRIPT_DIR / "buzzard-mark.json"
GENERATED_NOTICE = (
    "Generated from guest/branding/buzzard-mark.json; do not edit by hand. "
    "Trademark and visual-similarity clearance is pending."
)
SVG_HEADER = (
    '<!-- SPDX-License-Identifier: AGPL-3.0-or-later -->\n'
    '<!-- Copyright (C) 2026 Open Research Tools contributors -->\n'
)
STATIC_OUTPUTS = {
    "host-icon-dark": REPOSITORY / "host/packaging/wildbuzzard.svg",
    "guest-icon-dark": (
        REPOSITORY
        / "guest/assets/icons/WildBuzzard/scalable/apps/wildbuzzard.svg"
    ),
    "guest-settings-icon-dark": (
        REPOSITORY
        / "guest/assets/icons/WildBuzzard/scalable/apps/wildbuzzard-settings.svg"
    ),
    "guest-symbolic": (
        REPOSITORY
        / "guest/assets/icons/WildBuzzard/symbolic/apps/wildbuzzard-symbolic.svg"
    ),
    "guest-settings-symbolic": (
        REPOSITORY
        / "guest/assets/icons/WildBuzzard/symbolic/apps/wildbuzzard-settings-symbolic.svg"
    ),
    "mark-dark": REPOSITORY / "guest/assets/branding/wildbuzzard-mark-dark.svg",
    "mark-light": REPOSITORY / "guest/assets/branding/wildbuzzard-mark-light.svg",
    "icon-light": REPOSITORY / "guest/assets/branding/wildbuzzard-icon-light.svg",
    "wallpaper-presets": (
        REPOSITORY / "guest/assets/branding/wallpaper-presets.json"
    ),
}
COLOR = re.compile(r"#[0-9A-Fa-f]{6}\Z")
IDENTIFIER = re.compile(r"[a-z][a-z0-9]*(?:-[a-z0-9]+)*\Z")
PATH_DATA = re.compile(
    r"(?:"
    r"[MmZzLlHhVvCcSsQqTtAa]"
    r"|[-+]?(?:(?:[0-9]+(?:\.[0-9]*)?)|(?:\.[0-9]+))(?:[eE][-+]?[0-9]+)?"
    r"|[ \t\r\n,]"
    r")+\Z"
)
PATH_COMMAND = re.compile(r"[MmZzLlHhVvCcSsQqTtAa]")
MAX_PATH_DATA_LENGTH = 100_000
PATH_ROLES = {"main", "secondary", "accent", "detail"}
FILL_RULES = {"nonzero", "evenodd"}
PALETTE_NAMES = {"icon_dark", "icon_light", "unboxed_dark", "unboxed_light"}
TOP_LEVEL_FIELDS = {
    "schema",
    "candidate_id",
    "candidate_status",
    "geometry_revision",
    "artboard",
    "pose",
    "paths",
    "symbolic_paths",
    "palettes",
    "wallpaper",
}
PATH_FIELDS = {"id", "role", "fill_rule", "d"}
SYMBOLIC_PATH_FIELDS = {"id", "fill_rule", "d"}
CANDIDATE_STATUSES = {"draft", "clearance-pending", "accepted"}
REVISION = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}\.[a-z0-9-]+\Z")
NORMALIZED_PATH_TOKEN = re.compile(
    r"[MLCZ]|[-+]?(?:(?:[0-9]+(?:\.[0-9]*)?)|(?:\.[0-9]+))(?:[eE][-+]?[0-9]+)?"
)
NORMALIZED_PATH_ARITY = {"M": 2, "L": 2, "C": 6, "Z": 0}
MAX_WALLPAPER_DIMENSION = 65_535
WALLPAPER_PRESETS = {
    "dark-plain": ("Dark Plain", None),
    "dark-logo": ("Dark + Logo", "unboxed_dark"),
    "light-plain": ("Light Plain", None),
    "light-logo": ("Light + Logo", "unboxed_light"),
}
REVIEW_SIZES = (16, 24, 32, 64, 256)
REVIEW_WIDTH = 1024
REVIEW_HEIGHT = 720


def exact_fields(value: dict[str, Any], expected: set[str], context: str) -> None:
    actual = set(value)
    if actual != expected:
        missing = sorted(expected - actual)
        unknown = sorted(actual - expected)
        raise ValueError(
            f"{context} fields do not match the schema "
            f"(missing={missing!r}, unknown={unknown!r})"
        )


def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    result: dict[str, Any] = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key!r}")
        result[key] = value
    return result


def load_source(path: Path = SOURCE) -> dict[str, Any]:
    with path.open("r", encoding="utf-8") as handle:
        data = json.load(
            handle,
            parse_float=Decimal,
            object_pairs_hook=reject_duplicate_keys,
        )
    validate_source(data)
    return data


def validate_normalized_path(
    path_data: Any,
    safe_minimum: Decimal,
    safe_maximum: Decimal,
) -> None:
    if (
        not isinstance(path_data, str)
        or not path_data
        or len(path_data) > MAX_PATH_DATA_LENGTH
        or not PATH_DATA.fullmatch(path_data)
        or not PATH_COMMAND.search(path_data)
    ):
        raise ValueError("branding path data contains unsupported syntax")

    commands = re.findall(r"[A-Za-z]", path_data)
    if any(command not in NORMALIZED_PATH_ARITY for command in commands):
        raise ValueError(
            "branding paths must use normalized absolute M, L, C, and Z commands"
        )

    tokens = NORMALIZED_PATH_TOKEN.findall(path_data)
    compact_source = re.sub(r"[ \t\r\n,]", "", path_data)
    if "".join(tokens) != compact_source:
        raise ValueError("branding path data contains unsupported syntax")

    index = 0
    open_subpath = False
    drawn_segment = False
    while index < len(tokens):
        command = tokens[index]
        if command not in NORMALIZED_PATH_ARITY:
            raise ValueError(
                "branding paths must use normalized absolute M, L, C, and Z commands"
            )
        index += 1
        arity = NORMALIZED_PATH_ARITY[command]
        if index + arity > len(tokens) or any(
            token in NORMALIZED_PATH_ARITY for token in tokens[index : index + arity]
        ):
            raise ValueError("branding path command has the wrong number of coordinates")
        if command == "M":
            if open_subpath:
                raise ValueError("branding path subpaths must close before a new move")
            open_subpath = True
            drawn_segment = False
        elif command in {"L", "C"}:
            if not open_subpath:
                raise ValueError("branding path drawing command precedes its move")
            drawn_segment = True
        elif command == "Z":
            if not open_subpath or not drawn_segment:
                raise ValueError("branding path contains an empty or unmatched close")
            open_subpath = False
            continue

        for token in tokens[index : index + arity]:
            coordinate = Decimal(token)
            if coordinate < safe_minimum or coordinate > safe_maximum:
                raise ValueError("branding path coordinate leaves the reviewed safe area")
        index += arity

    if open_subpath:
        raise ValueError("branding path subpaths must be closed")


def validate_path_record(
    path: dict[str, Any],
    fields: set[str],
    safe_minimum: Decimal,
    safe_maximum: Decimal,
    *,
    symbolic: bool,
) -> None:
    exact_fields(path, fields, "symbolic path" if symbolic else "branding path")
    identifier = path.get("id")
    if not isinstance(identifier, str) or not IDENTIFIER.fullmatch(identifier):
        raise ValueError("branding path identifiers must use safe lowercase tokens")
    if not symbolic:
        role = path.get("role")
        if not isinstance(role, str) or role not in PATH_ROLES:
            raise ValueError("branding paths use an unsupported palette role")
    fill_rule = path.get("fill_rule")
    if not isinstance(fill_rule, str) or fill_rule not in FILL_RULES:
        raise ValueError("branding paths use an unsupported fill rule")
    validate_normalized_path(path.get("d"), safe_minimum, safe_maximum)


def validate_source(data: dict[str, Any]) -> None:
    if not isinstance(data, dict):
        raise ValueError("branding geometry must be a JSON object")
    exact_fields(data, TOP_LEVEL_FIELDS, "branding geometry")
    if data.get("schema") != 1:
        raise ValueError("unsupported branding geometry schema")
    candidate_id = data.get("candidate_id")
    if not isinstance(candidate_id, str) or not IDENTIFIER.fullmatch(candidate_id):
        raise ValueError("branding candidate identifier is invalid")
    if data.get("candidate_status") not in CANDIDATE_STATUSES:
        raise ValueError("branding candidate status is invalid")
    geometry_revision = data.get("geometry_revision")
    if not isinstance(geometry_revision, str) or not REVISION.fullmatch(
        geometry_revision
    ):
        raise ValueError("branding geometry revision is invalid")
    artboard = data.get("artboard", {})
    if not isinstance(artboard, dict):
        raise ValueError("branding artboard must be an object")
    exact_fields(artboard, {"width", "height", "safe_space_min"}, "artboard")
    if (artboard.get("width"), artboard.get("height")) != (256, 256):
        raise ValueError("the reviewed master artboard must be 256 by 256")
    safe_space = artboard.get("safe_space_min")
    if isinstance(safe_space, bool) or not isinstance(safe_space, int):
        raise ValueError("branding safe space must be an integer")
    if safe_space < 0 or safe_space >= 128:
        raise ValueError("branding safe space is outside the artboard")
    safe_minimum = Decimal(safe_space)
    safe_maximum = Decimal(256 - safe_space)
    paths = data.get("paths", [])
    if not isinstance(paths, list):
        raise ValueError("branding paths must be a list")
    if not 2 <= len(paths) <= 4:
        raise ValueError("the production portrait must contain two to four paths")
    if any(not isinstance(path, dict) for path in paths):
        raise ValueError("every branding path must be an object")
    for path in paths:
        validate_path_record(
            path,
            PATH_FIELDS,
            safe_minimum,
            safe_maximum,
            symbolic=False,
        )
    identifier_values = [path["id"] for path in paths]
    identifiers = set(identifier_values)
    if len(identifiers) != len(paths):
        raise ValueError("branding path identifiers must be unique and non-empty")
    roles = [path["role"] for path in paths]
    symbolic_paths = data.get("symbolic_paths", [])
    if not isinstance(symbolic_paths, list) or not 1 <= len(symbolic_paths) <= 2:
        raise ValueError("the symbolic variant must contain one or two paths")
    if any(not isinstance(path, dict) for path in symbolic_paths):
        raise ValueError("every symbolic path must be an object")
    for path in symbolic_paths:
        validate_path_record(
            path,
            SYMBOLIC_PATH_FIELDS,
            safe_minimum,
            safe_maximum,
            symbolic=True,
        )
    symbolic_identifiers = [path["id"] for path in symbolic_paths]
    if len(set(symbolic_identifiers)) != len(symbolic_identifiers):
        raise ValueError("symbolic path identifiers must be unique and non-empty")
    pose = data.get("pose", {})
    if not isinstance(pose, dict):
        raise ValueError("branding pose metadata must be an object")
    exact_fields(
        pose,
        {"species", "view", "gaze", "direct_eye_contact"},
        "branding pose",
    )
    if (
        pose.get("species") != "Buteo buteo"
        or pose.get("view") != "calm-three-quarter-front"
        or not isinstance(pose.get("gaze"), str)
        or not pose["gaze"]
        or pose.get("direct_eye_contact") is not False
    ):
        raise ValueError("the candidate must be a non-staring Buteo buteo portrait")
    palettes = data.get("palettes", {})
    if not isinstance(palettes, dict) or set(palettes) != PALETTE_NAMES:
        raise ValueError("branding palettes must contain the reviewed variants")
    used_roles = set(roles)
    for palette_name, palette in palettes.items():
        if not isinstance(palette, dict):
            raise ValueError(f"palette {palette_name!r} must be an object")
        expected_palette_fields = set(used_roles)
        if palette_name.startswith("icon_"):
            expected_palette_fields.add("background")
        exact_fields(palette, expected_palette_fields, f"palette {palette_name!r}")
        for color in palette.values():
            if not isinstance(color, str) or not COLOR.fullmatch(color):
                raise ValueError(f"invalid palette color: {color!r}")
    wallpaper = data.get("wallpaper", {})
    if not isinstance(wallpaper, dict):
        raise ValueError("wallpaper configuration must be an object")
    exact_fields(
        wallpaper,
        {"mark_short_side_fraction", "presets", "custom_solid"},
        "wallpaper configuration",
    )
    fraction = wallpaper.get("mark_short_side_fraction")
    if (
        isinstance(fraction, bool)
        or not isinstance(fraction, (int, Decimal))
        or Decimal(fraction) != Decimal("0.2")
    ):
        raise ValueError("wallpaper mark size must be the reviewed 0.2 fraction")
    presets = wallpaper.get("presets", [])
    if not isinstance(presets, list) or any(
        not isinstance(preset, dict) for preset in presets
    ):
        raise ValueError("wallpaper presets must be a list of objects")
    preset_ids = [preset.get("id") for preset in presets]
    if any(not isinstance(identifier, str) for identifier in preset_ids):
        raise ValueError("wallpaper preset identifiers must be strings")
    expected = set(WALLPAPER_PRESETS)
    if set(preset_ids) != expected or len(preset_ids) != len(expected):
        raise ValueError("wallpaper presets must be the four reviewed choices")
    for preset in presets:
        exact_fields(
            preset,
            {"id", "label", "background", "mark_palette"},
            "wallpaper preset",
        )
        expected_label, expected_palette = WALLPAPER_PRESETS[preset["id"]]
        if preset.get("label") != expected_label:
            raise ValueError("wallpaper preset label does not match its identifier")
        background = preset.get("background")
        if not isinstance(background, str) or not COLOR.fullmatch(background):
            raise ValueError("wallpaper presets must use literal #RRGGBB backgrounds")
        mark_palette = preset.get("mark_palette")
        if mark_palette != expected_palette:
            raise ValueError("wallpaper preset uses the wrong mark palette")
    presets_by_id = {preset["id"]: preset for preset in presets}
    if presets_by_id["dark-plain"]["background"] != presets_by_id["dark-logo"][
        "background"
    ]:
        raise ValueError("dark plain and logo wallpapers must share a background")
    if presets_by_id["light-plain"]["background"] != presets_by_id["light-logo"][
        "background"
    ]:
        raise ValueError("light plain and logo wallpapers must share a background")
    custom_solid = wallpaper.get("custom_solid")
    if not isinstance(custom_solid, dict):
        raise ValueError("custom solid wallpaper configuration must be an object")
    exact_fields(custom_solid, {"format", "recommended"}, "custom solid wallpaper")
    if custom_solid.get("format") != "#RRGGBB":
        raise ValueError("custom solid wallpaper format is invalid")
    recommended = custom_solid.get("recommended")
    expected_recommended = [
        presets_by_id["dark-plain"]["background"],
        presets_by_id["light-plain"]["background"],
    ]
    if recommended != expected_recommended:
        raise ValueError("custom solid recommendations must match built-in backgrounds")


def xml_attribute(value: str) -> str:
    """Escape an already validated value for a double-quoted XML attribute."""

    return escape(value, {'"': "&quot;", "'": "&apos;"})


def xml_path(path: dict[str, Any], fill: str, indent: str = "  ") -> str:
    return (
        f'{indent}<path id="{xml_attribute(path["id"])}" '
        f'fill="{xml_attribute(fill)}" '
        f'fill-rule="{xml_attribute(path["fill_rule"])}" '
        f'd="{xml_attribute(path["d"])}"/>'
    )


def svg_document(title: str, body: list[str]) -> str:
    return (
        SVG_HEADER
        + f"<!-- {GENERATED_NOTICE} -->\n"
        + '<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" '
        + 'viewBox="0 0 256 256">\n'
        + f"  <title>{escape(title)}</title>\n"
        + "\n".join(body)
        + "\n</svg>\n"
    )


def rendered_paths(data: dict[str, Any], palette_name: str) -> list[str]:
    palette = data["palettes"][palette_name]
    return [xml_path(path, palette[path["role"]]) for path in data["paths"]]


def render_icon(data: dict[str, Any], palette_name: str, title: str) -> str:
    palette = data["palettes"][palette_name]
    body = [
        f'  <rect width="256" height="256" rx="48" fill="{palette["background"]}"/>',
        *rendered_paths(data, palette_name),
    ]
    return svg_document(title, body)


def render_mark(data: dict[str, Any], palette_name: str, title: str) -> str:
    return svg_document(title, rendered_paths(data, palette_name))


def render_symbolic(data: dict[str, Any], title: str) -> str:
    body = [
        xml_path(path, "currentColor") for path in data["symbolic_paths"]
    ]
    return svg_document(title, body)


def preset_manifest(data: dict[str, Any]) -> str:
    document = {
        "schema": 1,
        "candidate_id": data["candidate_id"],
        "candidate_status": data["candidate_status"],
        "geometry_revision": data["geometry_revision"],
        "mark_short_side_fraction": float(
            data["wallpaper"]["mark_short_side_fraction"]
        ),
        "presets": data["wallpaper"]["presets"],
        "custom_solid": data["wallpaper"]["custom_solid"],
    }
    return json.dumps(document, indent=2, sort_keys=True) + "\n"


def expected_static_outputs(data: dict[str, Any]) -> dict[Path, str]:
    dark_icon = render_icon(data, "icon_dark", "Wild Buzzard")
    symbolic = render_symbolic(data, "Wild Buzzard symbolic mark")
    return {
        STATIC_OUTPUTS["host-icon-dark"]: dark_icon,
        STATIC_OUTPUTS["guest-icon-dark"]: dark_icon,
        STATIC_OUTPUTS["guest-settings-icon-dark"]: dark_icon.replace(
            "<title>Wild Buzzard</title>",
            "<title>Wild Buzzard Settings</title>",
        ),
        STATIC_OUTPUTS["guest-symbolic"]: symbolic,
        STATIC_OUTPUTS["guest-settings-symbolic"]: symbolic.replace(
            "<title>Wild Buzzard symbolic mark</title>",
            "<title>Wild Buzzard Settings symbolic mark</title>",
        ),
        STATIC_OUTPUTS["mark-dark"]: render_mark(
            data, "unboxed_dark", "Wild Buzzard dark unboxed mark"
        ),
        STATIC_OUTPUTS["mark-light"]: render_mark(
            data, "unboxed_light", "Wild Buzzard light unboxed mark"
        ),
        STATIC_OUTPUTS["icon-light"]: render_icon(
            data, "icon_light", "Wild Buzzard light icon"
        ),
        STATIC_OUTPUTS["wallpaper-presets"]: preset_manifest(data),
    }


def atomic_write(path: Path, contents: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent, prefix=f".{path.name}.", suffix=".tmp"
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(contents)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, 0o644)
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def write_static_outputs(data: dict[str, Any]) -> None:
    for path, contents in expected_static_outputs(data).items():
        atomic_write(path, contents)


def check_static_outputs(data: dict[str, Any]) -> int:
    stale: list[str] = []
    for path, expected in expected_static_outputs(data).items():
        try:
            actual = path.read_text(encoding="utf-8")
        except FileNotFoundError:
            stale.append(f"missing: {path.relative_to(REPOSITORY)}")
            continue
        if actual != expected:
            stale.append(f"stale: {path.relative_to(REPOSITORY)}")
    if stale:
        print("\n".join(stale), file=sys.stderr)
        print("run guest/branding/generate.py to regenerate", file=sys.stderr)
        return 1
    source_hash = hashlib.sha256(SOURCE.read_bytes()).hexdigest()
    print(f"branding assets are current (source sha256 {source_hash})")
    return 0


def decimal_text(value: Decimal) -> str:
    rendered = format(value.normalize(), "f")
    if "." in rendered:
        rendered = rendered.rstrip("0").rstrip(".")
    return rendered or "0"


def render_wallpaper(
    data: dict[str, Any], width: int, height: int, preset_id: str, color: str | None
) -> str:
    if (
        isinstance(width, bool)
        or isinstance(height, bool)
        or not isinstance(width, int)
        or not isinstance(height, int)
        or width <= 0
        or height <= 0
        or width > MAX_WALLPAPER_DIMENSION
        or height > MAX_WALLPAPER_DIMENSION
    ):
        raise ValueError(
            f"wallpaper dimensions must be integers from 1 to {MAX_WALLPAPER_DIMENSION}"
        )
    presets = {preset["id"]: preset for preset in data["wallpaper"]["presets"]}
    if preset_id == "solid":
        if color is None or not COLOR.fullmatch(color):
            raise ValueError("the solid preset requires a #RRGGBB color")
        background = color.upper()
        palette_name = None
        label = "Custom solid"
    else:
        if color is not None:
            raise ValueError("--color is valid only with the solid preset")
        try:
            preset = presets[preset_id]
        except KeyError as error:
            raise ValueError(f"unknown wallpaper preset: {preset_id}") from error
        background = preset["background"]
        palette_name = preset["mark_palette"]
        label = preset["label"]

    body = [f'  <rect width="{width}" height="{height}" fill="{background}"/>']
    if palette_name is not None:
        artboard = Decimal(data["artboard"]["width"])
        mark_pixels = Decimal(min(width, height)) * data["wallpaper"][
            "mark_short_side_fraction"
        ]
        scale = mark_pixels / artboard
        x = (Decimal(width) - artboard * scale) / Decimal(2)
        y = (Decimal(height) - artboard * scale) / Decimal(2)
        body.append(
            "  <g data-mark-short-side-fraction=\"0.2\" transform=\"translate("
            f"{decimal_text(x)} {decimal_text(y)}) scale({decimal_text(scale)})\">"
        )
        body.extend(rendered_paths(data, palette_name))
        body.append("  </g>")

    return (
        SVG_HEADER
        + f"<!-- {GENERATED_NOTICE} -->\n"
        + f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" '
        + f'height="{height}" viewBox="0 0 {width} {height}">\n'
        + f"  <title>{escape(label)} wallpaper</title>\n"
        + "\n".join(body)
        + "\n</svg>\n"
    )


def render_review_icon_group(
    data: dict[str, Any],
    palette_name: str,
    x: int,
    y: int,
    size: int,
) -> list[str]:
    """Render one icon at an exact physical size inside the review sheet."""

    palette = data["palettes"][palette_name]
    scale = Decimal(size) / Decimal(data["artboard"]["width"])
    body = [
        f'  <g data-review-size-px="{size}" '
        f'data-review-palette="{palette_name}" '
        f'transform="translate({x} {y}) scale({decimal_text(scale)})">',
        f'    <rect width="256" height="256" rx="48" '
        f'fill="{palette["background"]}"/>',
    ]
    body.extend(
        xml_path(path, palette[path["role"]], indent="    ")
        for path in data["paths"]
    )
    body.append("  </g>")
    return body


def render_review_mark_group(
    data: dict[str, Any],
    palette_name: str,
    x: int,
    y: int,
    size: int,
) -> list[str]:
    """Render one unboxed mark at an exact physical size."""

    scale = Decimal(size) / Decimal(data["artboard"]["width"])
    body = [
        f'  <g data-review-mark-size-px="{size}" '
        f'data-review-palette="{palette_name}" '
        f'transform="translate({x} {y}) scale({decimal_text(scale)})">'
    ]
    body.extend(
        xml_path(
            path,
            data["palettes"][palette_name][path["role"]],
            indent="    ",
        )
        for path in data["paths"]
    )
    body.append("  </g>")
    return body


def render_review_sheet(data: dict[str, Any]) -> str:
    """Return a deterministic dark/light thumbnail and wallpaper-mark sheet."""

    positions = ((36, 280), (84, 272), (140, 264), (204, 232), (316, 40))
    body = [
        f'  <rect width="{REVIEW_WIDTH}" height="{REVIEW_HEIGHT}" '
        'fill="#D9D5CF"/>',
        '  <rect x="24" y="24" width="976" height="320" rx="20" '
        'fill="#EEEAE4"/>',
        '  <rect x="24" y="376" width="976" height="320" rx="20" '
        'fill="#2A2D30"/>',
        '  <rect x="684" y="56" width="256" height="256" rx="16" '
        'fill="#202225"/>',
        '  <rect x="684" y="408" width="256" height="256" rx="16" '
        'fill="#F4F1EC"/>',
    ]
    for (x, top_y), size in zip(positions, REVIEW_SIZES, strict=True):
        body.extend(render_review_icon_group(data, "icon_dark", x, top_y, size))
        body.extend(
            render_review_icon_group(data, "icon_light", x, top_y + 352, size)
        )
    body.extend(render_review_mark_group(data, "unboxed_dark", 684, 56, 256))
    body.extend(render_review_mark_group(data, "unboxed_light", 684, 408, 256))
    return (
        SVG_HEADER
        + f"<!-- {GENERATED_NOTICE} -->\n"
        + f'<svg xmlns="http://www.w3.org/2000/svg" width="{REVIEW_WIDTH}" '
        + f'height="{REVIEW_HEIGHT}" viewBox="0 0 {REVIEW_WIDTH} {REVIEW_HEIGHT}">\n'
        + "  <title>Wild Buzzard exact-size vector review sheet</title>\n"
        + "\n".join(body)
        + "\n</svg>\n"
    )


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    action = parser.add_mutually_exclusive_group()
    action.add_argument(
        "--check", action="store_true", help="fail if outputs are stale"
    )
    action.add_argument(
        "--wallpaper-output",
        type=Path,
        metavar="PATH",
        help="write one exact-size SVG wallpaper instead of static assets",
    )
    action.add_argument(
        "--review-output",
        type=Path,
        metavar="PATH",
        help="write a deterministic exact-size dark/light review sheet",
    )
    parser.add_argument("--width", type=int)
    parser.add_argument("--height", type=int)
    parser.add_argument(
        "--preset",
        choices=["dark-plain", "dark-logo", "light-plain", "light-logo", "solid"],
    )
    parser.add_argument("--color", help="custom #RRGGBB color for --preset solid")
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    try:
        data = load_source()
        if arguments.check:
            if any(
                value is not None
                for value in (
                    arguments.width,
                    arguments.height,
                    arguments.preset,
                    arguments.color,
                )
            ):
                raise ValueError("wallpaper arguments cannot be combined with --check")
            return check_static_outputs(data)
        if arguments.wallpaper_output is not None:
            if (
                arguments.width is None
                or arguments.height is None
                or arguments.preset is None
            ):
                raise ValueError(
                    "--wallpaper-output requires --width, --height, and --preset"
                )
            wallpaper = render_wallpaper(
                data,
                arguments.width,
                arguments.height,
                arguments.preset,
                arguments.color,
            )
            atomic_write(arguments.wallpaper_output, wallpaper)
            return 0
        if arguments.review_output is not None:
            if any(
                value is not None
                for value in (
                    arguments.width,
                    arguments.height,
                    arguments.preset,
                    arguments.color,
                )
            ):
                raise ValueError(
                    "wallpaper arguments cannot be combined with --review-output"
                )
            atomic_write(arguments.review_output, render_review_sheet(data))
            return 0
        if any(
            value is not None
            for value in (
                arguments.width,
                arguments.height,
                arguments.preset,
                arguments.color,
            )
        ):
            raise ValueError("wallpaper arguments require --wallpaper-output")
        write_static_outputs(data)
        return 0
    except (OSError, ValueError) as error:
        print(f"branding generation failed: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
