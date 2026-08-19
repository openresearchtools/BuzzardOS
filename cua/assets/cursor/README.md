# Embedded Buzzard CUA cursor

`cua.default.lottie` is the inspected, editable dotLottie source archive from
the pinned TryCua revision. `cua.default.cua-theme` is its bounded runtime
artifact. Both are checked in so builds never fetch cursor code or assets.

The runtime embeds and decodes only `.cua-theme`. It never opens ZIP or JSON at
runtime. The artifact contains bounded vector geometry, paints, transforms,
and sampled animation frames; Tiny Skia rasterizes them at the live guest
output scale.

Regenerate the deterministic source archive with:

```bash
python3 cua/assets/cursor/build_default_theme.py
```

The binary artifact is pinned to that source and verified by the Rust test
`embedded_source_hash_matches_source_archive`. Any intentional source change
must also rebuild and inspect the artifact before commit.

The Inter font license and upstream MIT evidence are retained beside the
assets and under `cua/`.
