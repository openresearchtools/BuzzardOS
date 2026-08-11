# Wild Buzzard vector source

`buzzard-mark.json` is the only production-candidate geometry source. It is an
original, 256 by 256, four-path portrait of a European common buzzard (`Buteo
buteo`) in a calm near-front three-quarter pose with its single visible eye
looking slightly off axis. It is a candidate, not a cleared or approved public
mark.

Regenerate every checked-in variant:

```sh
python3 guest/branding/generate.py
python3 guest/branding/generate.py --check
```

Generate the deterministic exact-size visual review sheet outside the source
tree, then rasterize it with the distribution's `ffmpeg`/librsvg stack:

```sh
python3 guest/branding/generate.py \
  --review-output /tmp/wildbuzzard-vector-review.svg
ffmpeg -v error -i /tmp/wildbuzzard-vector-review.svg \
  -frames:v 1 /tmp/wildbuzzard-vector-review.png
```

The sheet contains dark and light icons at actual 16, 24, 32, 64, and 256
physical pixels plus both 256-pixel unboxed wallpaper marks. It is review
evidence, not a shipped runtime asset.

Generate an exact-size, resolution-independent wallpaper outside the source
tree:

```sh
python3 guest/branding/generate.py \
  --wallpaper-output /tmp/wildbuzzard-wallpaper.svg \
  --width 1920 --height 1080 --preset dark-logo
```

The four stable preset IDs are `dark-plain`, `dark-logo`, `light-plain`, and
`light-logo`. Arbitrary single-colour output uses `--preset solid --color
'#RRGGBB'`. Dark `#202225` and light `#F4F1EC` are the recommended solid
defaults because they match the corresponding Wild Buzzard backgrounds.

The logo presets center the unchanged 256-unit geometry at exactly 20% of the
output's shorter physical dimension. The generator writes a new SVG for the
requested dimensions; it never stretches a fixed-size raster.

Generated files carry a warning that similarity and trademark clearance is
pending. Do not edit them directly. See
[`docs/branding/PROVENANCE.md`](../../docs/branding/PROVENANCE.md) and
[`docs/branding/CLEARANCE_REPORT.md`](../../docs/branding/CLEARANCE_REPORT.md).
