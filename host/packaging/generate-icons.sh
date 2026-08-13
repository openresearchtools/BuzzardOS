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
command -v python3 >/dev/null 2>&1 || {
    echo "python3 is required to generate the rounded Buzzard OS icon mask" >&2
    exit 1
}

work_dir=$(mktemp -d)
trap 'rm -rf -- "$work_dir"' EXIT

# The approved artwork is a wide 16:9 composition. Crop only its empty side
# margins, keep the complete bird, then fit it into a square graphite tile.
# No generative redraw, tracing, sharpening, or colour replacement is used.
for size in 512 256 128 64 48 32; do
    inset=$((size * 15 / 16))
    supersampled=$((size * 4))
    radius=$((supersampled / 8))
    base="$work_dir/base-${size}.png"
    mask="$work_dir/mask-${size}.pgm"
    output="$packaging_dir/icons/buzzardos-${size}.png"
    ffmpeg -hide_banner -loglevel error -y \
        -i "$source_image" \
        -vf "crop=1280:941:196:0,scale=${inset}:-2:flags=lanczos,pad=${size}:${size}:(ow-iw)/2:(oh-ih)/2:color=0x1e2024,format=rgba" \
        -frames:v 1 -compression_level 9 -pred mixed "$base"
    python3 - "$mask" "$supersampled" "$radius" <<'PY'
import sys

path, size_text, radius_text = sys.argv[1:]
size = int(size_text)
radius = int(radius_text)
centre = (size - 1) / 2
inner = size / 2 - radius
with open(path, "wb") as output:
    output.write(f"P5\n{size} {size}\n255\n".encode("ascii"))
    row = bytearray(size)
    for y in range(size):
        qy = max(abs(y - centre) - inner, 0)
        for x in range(size):
            qx = max(abs(x - centre) - inner, 0)
            row[x] = 255 if qx * qx + qy * qy <= radius * radius else 0
        output.write(row)
PY
    ffmpeg -hide_banner -loglevel error -y \
        -i "$base" -i "$mask" \
        -filter_complex "[1:v]scale=${size}:${size}:flags=lanczos[mask];[0:v][mask]alphamerge,format=rgba" \
        -frames:v 1 -compression_level 9 -pred mixed "$output"
done
