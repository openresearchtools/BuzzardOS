#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
fork_dir="$project_dir/third_party/trycua-cua"
appdir="${1:-$project_dir/build/appimage/WildBuzzard.AppDir}"
guest_rootfs="${2:-}"

for required in \
    "$fork_dir/LICENSE.md" \
    "$fork_dir/UPSTREAM.toml" \
    "$fork_dir/CHANGES.WILDBUZZARD.md" \
    "$fork_dir/README.WILDBUZZARD.md"; do
    test -s "$required"
done

grep -Eq '^upstream_repository = "https://github.com/trycua/cua"$' \
    "$fork_dir/UPSTREAM.toml"
grep -Eq '^upstream_commit = "[0-9a-f]{40}"$' "$fork_dir/UPSTREAM.toml"
grep -Eq '^upstream_tag = "[^"]+"$' "$fork_dir/UPSTREAM.toml"
grep -Eq '^license = "MIT"$' "$fork_dir/UPSTREAM.toml"
grep -Eq '^upstream_endorsement = false$' "$fork_dir/UPSTREAM.toml"
grep -q '^MIT License$' "$fork_dir/LICENSE.md"
grep -q 'not endorsed' "$fork_dir/CHANGES.WILDBUZZARD.md"

check_cargo_licenses() {
    local manifest=$1
    cargo metadata --locked --format-version 1 --manifest-path "$manifest" |
        jq -e '
            [.packages[]
             | select(.source != null)
             | select((.license // "") == "" and (.license_file // "") == "")]
            | length == 0
        ' >/dev/null
}

check_cargo_licenses "$project_dir/Cargo.toml"
check_cargo_licenses "$fork_dir/cua-driver/rust/Cargo.toml"

if [[ -d "$appdir" ]]; then
    for package in \
        libnvidia-container-tools \
        libnvidia-container1 \
        nvidia-container-toolkit-base; do
        test -s "$appdir/usr/share/doc/$package/copyright"
    done
fi

if [[ -n "$guest_rootfs" ]]; then
    guest_doc="$guest_rootfs/usr/share/doc/wildbuzzard-cua"
    cmp "$fork_dir/LICENSE.md" "$guest_doc/LICENSE.trycua-cua.md"
    cmp "$fork_dir/UPSTREAM.toml" "$guest_doc/UPSTREAM.toml"
    cmp "$fork_dir/CHANGES.WILDBUZZARD.md" \
        "$guest_doc/CHANGES.WILDBUZZARD.md"
fi

echo "Wild Buzzard source, CUA fork, guest, and bundled-toolkit license inventory passed"
