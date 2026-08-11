#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later

"""Security and schema tests for the deterministic branding generator."""

from __future__ import annotations

import copy
import importlib.util
from pathlib import Path
import tempfile
import unittest
import xml.etree.ElementTree as ET


REPOSITORY = Path(__file__).resolve().parents[2]
GENERATOR_PATH = REPOSITORY / "guest/branding/generate.py"
SPEC = importlib.util.spec_from_file_location("wildbuzzard_branding_generator", GENERATOR_PATH)
assert SPEC is not None and SPEC.loader is not None
GENERATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GENERATOR)
SVG_NAMESPACE = "http://www.w3.org/2000/svg"


class BrandingGeneratorSecurityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.source = GENERATOR.load_source()

    def mutated_path(self, field: str, value: object) -> dict[str, object]:
        source = copy.deepcopy(self.source)
        source["paths"][0][field] = value
        return source

    def test_reviewed_source_passes_strict_validation(self) -> None:
        GENERATOR.validate_source(copy.deepcopy(self.source))

    def test_duplicate_json_keys_are_rejected_before_validation(self) -> None:
        source_text = GENERATOR.SOURCE.read_text(encoding="utf-8")
        source_text = source_text.replace(
            '  "schema": 1,',
            '  "schema": 1,\n  "schema": 1,',
            1,
        )
        with tempfile.TemporaryDirectory() as directory:
            source_path = Path(directory) / "duplicate.json"
            source_path.write_text(source_text, encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "duplicate JSON key"):
                GENERATOR.load_source(source_path)

    def test_unknown_and_missing_schema_fields_are_rejected(self) -> None:
        source = copy.deepcopy(self.source)
        source["unexpected"] = True
        with self.assertRaisesRegex(ValueError, "unknown=.*unexpected"):
            GENERATOR.validate_source(source)

        source = copy.deepcopy(self.source)
        del source["candidate_id"]
        with self.assertRaisesRegex(ValueError, "missing=.*candidate_id"):
            GENERATOR.validate_source(source)

        source = copy.deepcopy(self.source)
        source["paths"][0]["onclick"] = "alert(1)"
        with self.assertRaisesRegex(ValueError, "branding path fields"):
            GENERATOR.validate_source(source)

    def test_path_data_rejects_xml_and_script_injection(self) -> None:
        payloads = (
            'M0 0"/><script>alert(1)</script><path d="M0 0',
            "M0 0&external;Z",
            "M0 0 url(https://example.invalid/mark.svg) Z",
            "M0 0 R 10 10 Z",
            " , \t\n",
            ["M0 0Z"],
        )
        for payload in payloads:
            with self.subTest(payload=payload):
                with self.assertRaisesRegex(ValueError, "path data"):
                    GENERATOR.validate_source(self.mutated_path("d", payload))

    def test_paths_are_normalized_closed_and_inside_safe_space(self) -> None:
        invalid_paths = (
            ("m25 224L40 40Z", "normalized absolute"),
            ("M25 224L40 40", "must be closed"),
            ("M25 224Z", "empty or unmatched"),
            ("M25 224C40 40Z", "wrong number"),
            ("M23 224L40 40Z", "safe area"),
        )
        for path_data, message in invalid_paths:
            with self.subTest(path_data=path_data):
                with self.assertRaisesRegex(ValueError, message):
                    GENERATOR.validate_source(self.mutated_path("d", path_data))

    def test_symbolic_geometry_is_separate_and_strict(self) -> None:
        source = copy.deepcopy(self.source)
        source["symbolic_paths"] = []
        with self.assertRaisesRegex(ValueError, "one or two paths"):
            GENERATOR.validate_source(source)

        source = copy.deepcopy(self.source)
        source["symbolic_paths"][0]["role"] = "main"
        with self.assertRaisesRegex(ValueError, "symbolic path fields"):
            GENERATOR.validate_source(source)

    def test_fill_rule_rejects_attribute_injection(self) -> None:
        for fill_rule in (
            'evenodd" onload="alert(1)',
            "url(https://example.invalid)",
            "inherit",
            ["evenodd"],
        ):
            with self.subTest(fill_rule=fill_rule):
                with self.assertRaisesRegex(ValueError, "fill rule"):
                    GENERATOR.validate_source(
                        self.mutated_path("fill_rule", fill_rule)
                    )

    def test_identifier_and_role_reject_markup(self) -> None:
        with self.assertRaisesRegex(ValueError, "identifiers"):
            GENERATOR.validate_source(
                self.mutated_path("id", 'portrait"/><script/>')
            )
        with self.assertRaisesRegex(ValueError, "palette role"):
            GENERATOR.validate_source(
                self.mutated_path("role", 'main" onload="alert(1)')
            )

    def test_palette_rejects_non_literal_color(self) -> None:
        source = copy.deepcopy(self.source)
        source["palettes"]["icon_dark"]["main"] = "url(https://example.invalid/x)"
        with self.assertRaisesRegex(ValueError, "invalid palette color"):
            GENERATOR.validate_source(source)

    def test_wallpaper_schema_rejects_unsafe_shape_and_fraction(self) -> None:
        source = copy.deepcopy(self.source)
        source["wallpaper"]["presets"][0]["id"] = ["dark-plain"]
        with self.assertRaisesRegex(ValueError, "identifiers"):
            GENERATOR.validate_source(source)

        source = copy.deepcopy(self.source)
        source["wallpaper"]["mark_short_side_fraction"] = "0.2"
        with self.assertRaisesRegex(ValueError, "mark size"):
            GENERATOR.validate_source(source)

        source = copy.deepcopy(self.source)
        source["wallpaper"]["presets"][0]["label"] = "Darkish"
        with self.assertRaisesRegex(ValueError, "label"):
            GENERATOR.validate_source(source)

        source = copy.deepcopy(self.source)
        source["wallpaper"]["presets"][1]["background"] = "#000000"
        with self.assertRaisesRegex(ValueError, "share a background"):
            GENERATOR.validate_source(source)

        source = copy.deepcopy(self.source)
        source["wallpaper"]["custom_solid"]["recommended"].reverse()
        with self.assertRaisesRegex(ValueError, "recommendations"):
            GENERATOR.validate_source(source)

    def test_wallpaper_dimensions_are_bounded(self) -> None:
        for width, height in (
            (0, 100),
            (100, -1),
            (GENERATOR.MAX_WALLPAPER_DIMENSION + 1, 100),
            (True, 100),
        ):
            with self.subTest(width=width, height=height):
                with self.assertRaisesRegex(ValueError, "dimensions"):
                    GENERATOR.render_wallpaper(
                        self.source,
                        width,
                        height,
                        "dark-plain",
                        None,
                    )

    def test_generation_is_byte_deterministic(self) -> None:
        first = GENERATOR.expected_static_outputs(self.source)
        second = GENERATOR.expected_static_outputs(copy.deepcopy(self.source))
        self.assertEqual(first, second)
        for width, height in ((16, 16), (256, 256), (3840, 2160), (7680, 4320)):
            with self.subTest(width=width, height=height):
                first_wallpaper = GENERATOR.render_wallpaper(
                    self.source,
                    width,
                    height,
                    "dark-logo",
                    None,
                )
                second_wallpaper = GENERATOR.render_wallpaper(
                    copy.deepcopy(self.source),
                    width,
                    height,
                    "dark-logo",
                    None,
                )
                self.assertEqual(first_wallpaper, second_wallpaper)

        first_review = GENERATOR.render_review_sheet(self.source)
        second_review = GENERATOR.render_review_sheet(copy.deepcopy(self.source))
        self.assertEqual(first_review, second_review)

    def test_review_sheet_contains_every_exact_icon_size_in_both_palettes(
        self,
    ) -> None:
        root = ET.fromstring(GENERATOR.render_review_sheet(self.source))
        groups = [
            element
            for element in root.iter()
            if element.tag.rsplit("}", 1)[-1] == "g"
            and "data-review-size-px" in element.attrib
        ]
        observed = [
            (
                int(group.attrib["data-review-size-px"]),
                group.attrib["data-review-palette"],
            )
            for group in groups
        ]
        expected = [
            (size, palette)
            for size in GENERATOR.REVIEW_SIZES
            for palette in ("icon_dark", "icon_light")
        ]
        self.assertCountEqual(observed, expected)
        self.assertEqual(root.attrib["width"], str(GENERATOR.REVIEW_WIDTH))
        self.assertEqual(root.attrib["height"], str(GENERATOR.REVIEW_HEIGHT))
        self.assertNotIn("href=", GENERATOR.render_review_sheet(self.source))

    def test_xml_path_escapes_attributes_defensively(self) -> None:
        path = {
            "id": 'mark" onload="alert(1)',
            "fill_rule": 'evenodd"/><script>alert(2)</script>',
            "d": 'M0 0Z"/><script>alert(3)</script>',
        }
        fragment = GENERATOR.xml_path(
            path,
            '#000000"/><script>alert(4)</script>',
        )
        root = ET.fromstring(f'<svg xmlns="{SVG_NAMESPACE}">{fragment}</svg>')
        children = list(root)
        self.assertEqual(len(children), 1)
        element = children[0]
        self.assertEqual(element.tag, f"{{{SVG_NAMESPACE}}}path")
        self.assertEqual(element.attrib["id"], path["id"])
        self.assertEqual(element.attrib["fill-rule"], path["fill_rule"])
        self.assertEqual(element.attrib["d"], path["d"])
        self.assertFalse(any(child.tag.endswith("script") for child in root.iter()))

    def test_generated_documents_have_a_closed_local_svg_inventory(self) -> None:
        allowed_elements = {"svg", "title", "rect", "g", "path"}
        allowed_attributes = {
            "svg": {"width", "height", "viewBox"},
            "title": set(),
            "rect": {"width", "height", "rx", "fill"},
            "g": {"data-mark-short-side-fraction", "transform"},
            "path": {"id", "fill", "fill-rule", "d"},
        }
        documents = [
            document
            for path, document in GENERATOR.expected_static_outputs(self.source).items()
            if path.suffix == ".svg"
        ]
        documents.extend(
            GENERATOR.render_wallpaper(self.source, 731, 487, preset, None)
            for preset in ("dark-plain", "dark-logo", "light-plain", "light-logo")
        )
        documents.append(
            GENERATOR.render_wallpaper(self.source, 731, 487, "solid", "#123456")
        )

        for document in documents:
            with self.subTest(title=document.splitlines()[3]):
                root = ET.fromstring(document)
                for element in root.iter():
                    local_name = element.tag.rsplit("}", 1)[-1]
                    self.assertIn(local_name, allowed_elements)
                    self.assertLessEqual(
                        set(element.attrib), allowed_attributes[local_name]
                    )
                    for value in element.attrib.values():
                        lowered = value.lower()
                        self.assertNotIn("url(", lowered)
                        self.assertNotIn("javascript:", lowered)
                self.assertFalse(any("script" in node.tag.lower() for node in root.iter()))


if __name__ == "__main__":
    unittest.main()
