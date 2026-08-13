#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

packaging_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source_image="$packaging_dir/icons/low-glide-source.png"
expected_source_sha256=58557adcbf860124bffe30655e36b8df9607fff58279414f5e43c5c90a38f2ea

[[ "$(sha256sum "$source_image" | cut -d' ' -f1)" == "$expected_source_sha256" ]] || {
    echo "Low Glide source artwork differs from its approved digest" >&2
    exit 1
}
command -v ffmpeg >/dev/null 2>&1 || {
    echo "ffmpeg is required to regenerate the Buzzard OS desktop icons" >&2
    exit 1
}

# The approved artwork is a wide 16:9 composition. Crop only its empty side
# margins, keep the complete bird, then fit it into a square graphite tile.
# No generative redraw, tracing, sharpening, or colour replacement is used.
for size in 512 256 128 64 48 32; do
    inset=$((size * 15 / 16))
    output="$packaging_dir/icons/buzzardos-${size}.png"
    ffmpeg -hide_banner -loglevel error -y \
        -i "$source_image" \
        -vf "crop=1280:941:196:0,scale=${inset}:-2:flags=lanczos,pad=${size}:${size}:(ow-iw)/2:(oh-ih)/2:color=0x1e2024,format=rgba" \
        -frames:v 1 -compression_level 9 -pred mixed "$output"
done

