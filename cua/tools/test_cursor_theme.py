# SPDX-License-Identifier: AGPL-3.0-or-later
import importlib.util
from pathlib import Path
import struct
import unittest

spec = importlib.util.spec_from_file_location("cursor_theme", Path(__file__).with_name("build-cursor-theme.py"))
theme = importlib.util.module_from_spec(spec)
spec.loader.exec_module(theme)


class CursorThemeTests(unittest.TestCase):
    def test_native_file_sizes_hotspot_and_no_animation(self):
        data = theme.xcursor(theme.SHAPES["default"])
        self.assertEqual(struct.unpack_from("<4I", data), (0x72756358, 16, 0x10000, len(theme.SIZES)))
        for n, size in enumerate(theme.SIZES):
            kind, nominal, offset = struct.unpack_from("<3I", data, 16 + n * 12)
            self.assertEqual((kind, nominal), (theme.IMAGE_TYPE, size))
            header, kind, nominal, version, w, h, x, y, delay = struct.unpack_from("<9I", data, offset)
            self.assertEqual((header, version, w, h, delay), (36, 1, size, size, 0))
            self.assertEqual((x, y), (round(3 * size / 24),) * 2)
            rgba = data[offset + 36:offset + 36 + w * h * 4]
            self.assertIn(bytes((53, 57, 229, 255)), rgba)
            self.assertEqual(len(rgba), w * h * 4)
            self.assertTrue(all(max(rgba[i:i+3]) <= rgba[i+3] for i in range(0, len(rgba), 4)))

    def test_all_shapes_have_bounded_hotspots_and_no_numbered_variants(self):
        aliases = set()
        for name, (hotspot, polygons, names) in theme.SHAPES.items():
            self.assertTrue(all(0 <= value < 24 for value in hotspot))
            self.assertTrue(all(len(polygon) >= 3 for polygon in polygons))
            self.assertFalse(any(character.isdigit() for character in name))
            for alias in [name, *names]:
                self.assertNotIn(alias, aliases)
                aliases.add(alias)

    def test_supported_fractional_scales_have_exact_native_rasters(self):
        for scale in (120, 150, 160, 180, 210, 240):
            self.assertIn(24 * scale // 120, theme.SIZES)


if __name__ == "__main__":
    unittest.main()
