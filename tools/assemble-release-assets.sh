#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: tools/assemble-release-assets.sh APPIMAGE ROOTFS_STAGE OUTPUT_DIRECTORY

Create the two Wild Buzzard release payloads from an audited AppImage and the
output of tools/build-release-rootfs.sh:

  WildBuzzard-x86_64.AppImage
  WildBuzzard-portable-x86_64.tar.zst

OUTPUT_DIRECTORY also receives SHA256SUMS for those two primary assets. The
portable archive contains the AppImage, the flat rootfs seed, empty portable
state directories, separate AppImage/guest license groups, and provenance.
EOF
}

if [[ $# -ne 3 ]]; then
    usage >&2
    exit 2
fi

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
appimage_argument=$1
rootfs_stage_argument=$2
output_dir=$(realpath -m -- "$3")

[[ -f "$appimage_argument" && ! -L "$appimage_argument" ]] || {
    echo "AppImage must be a regular non-symlink file: $appimage_argument" >&2
    exit 1
}
[[ -d "$rootfs_stage_argument" && ! -L "$rootfs_stage_argument" ]] || {
    echo "rootfs stage must be a real directory: $rootfs_stage_argument" >&2
    exit 1
}
appimage=$(realpath -- "$appimage_argument")
rootfs_stage=$(realpath -- "$rootfs_stage_argument")
source_commit=$(git -C "$project_dir" rev-parse --verify HEAD^{commit})
source_date_epoch=$(git -C "$project_dir" show -s --format=%ct "$source_commit")

for generated_path in "$output_dir"; do
    case "$generated_path/" in
        "$project_dir/"*)
            echo "refusing to place release assets inside the source repository" >&2
            exit 1
            ;;
    esac
done

for command_name in cmp cp find git install python3 realpath sha256sum stat tar unsquashfs zstd; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "release assembly dependency missing: $command_name" >&2
        exit 1
    }
done

# GitHub Releases reject files at or above 2 GiB. Enforce that contract during
# artifact-only builds as well, so an inspected artifact cannot later fail only
# when the identical bytes reach the publisher.
github_release_asset_limit=$((2 * 1024 * 1024 * 1024))
appimage_size=$(stat -c '%s' -- "$appimage")
if (( appimage_size >= github_release_asset_limit )); then
    echo "standalone AppImage is not smaller than GitHub's 2 GiB asset limit" >&2
    exit 1
fi

dirty=$(git -C "$project_dir" status --porcelain=v1 --untracked-files=all)
if [[ -n "$dirty" ]]; then
    echo "refusing to assemble release assets from a dirty source tree" >&2
    printf '%s\n' "$dirty" >&2
    exit 1
fi

if [[ -e "$output_dir" ]]; then
    [[ -d "$output_dir" && ! -L "$output_dir" ]] || {
        echo "release output must be a real directory: $output_dir" >&2
        exit 1
    }
    if find "$output_dir" -mindepth 1 -print -quit | grep -q .; then
        echo "release output directory must be empty: $output_dir" >&2
        exit 1
    fi
else
    mkdir -p "$output_dir"
fi

if find "$rootfs_stage" -type l -print -quit | grep -q .; then
    echo "rootfs stage contains a symlink; notice links must be materialized" >&2
    exit 1
fi
special=$(find "$rootfs_stage" \( ! -type d ! -type f \) -print -quit)
if [[ -n "$special" ]]; then
    echo "rootfs stage contains a socket/device/FIFO: $special" >&2
    exit 1
fi

mapfile -t rootfs_stage_entries < <(
    find "$rootfs_stage" -mindepth 1 -maxdepth 1 -printf '%f\n' | LC_ALL=C sort
)
# Keep this strict: an unexpected runner output must never silently enter or be
# silently omitted from a release bundle.
expected_rootfs_stage=(ROOTFS_SHA256SUMS licenses provenance runtime)
if [[ "${rootfs_stage_entries[*]}" != "${expected_rootfs_stage[*]}" ]]; then
    echo "rootfs stage has missing or extra top-level entries" >&2
    printf 'expected: %s\nactual:   %s\n' \
        "${expected_rootfs_stage[*]}" "${rootfs_stage_entries[*]}" >&2
    exit 1
fi

runtime_archive="$rootfs_stage/runtime/WildBuzzard-rootfs-linux-x86_64.tar.zst"
runtime_manifest="$rootfs_stage/runtime/WildBuzzard-rootfs-linux-x86_64.json"
for required_file in \
    "$runtime_archive" \
    "$runtime_manifest" \
    "$rootfs_stage/ROOTFS_SHA256SUMS" \
    "$rootfs_stage/licenses/guest-rootfs/README.md" \
    "$rootfs_stage/provenance/guest-rootfs/WildBuzzard-rootfs-linux-x86_64.json"; do
    [[ -f "$required_file" && ! -L "$required_file" ]] || {
        echo "rootfs stage file missing or not regular: $required_file" >&2
        exit 1
    }
done
expected_rootfs_checksums=$(mktemp)
checksum_cleanup() {
    rm -f -- "$expected_rootfs_checksums"
}
trap checksum_cleanup EXIT HUP INT TERM
(
    cd "$rootfs_stage"
    sha256sum \
        runtime/WildBuzzard-rootfs-linux-x86_64.tar.zst \
        runtime/WildBuzzard-rootfs-linux-x86_64.json
) >"$expected_rootfs_checksums"
cmp -- "$expected_rootfs_checksums" "$rootfs_stage/ROOTFS_SHA256SUMS"
python3 "$project_dir/tools/release_metadata.py" verify \
    --archive "$runtime_archive" \
    --manifest "$runtime_manifest"
cmp -- \
    "$runtime_manifest" \
    "$rootfs_stage/provenance/guest-rootfs/WildBuzzard-rootfs-linux-x86_64.json"

"$project_dir/tools/check-licenses.sh" --appimage "$appimage" --structural

assembly_root=$(mktemp -d "${RUNNER_TEMP:-/tmp}/wildbuzzard-release-assembly.XXXXXX")
bundle="$assembly_root/WildBuzzard"
appdir="$assembly_root/AppDir"
roundtrip="$assembly_root/roundtrip"
temporary_appimage=$(mktemp "$output_dir/.WildBuzzard-x86_64.AppImage.XXXXXX")
temporary_bundle=$(mktemp "$output_dir/.WildBuzzard-portable-x86_64.tar.zst.XXXXXX")
temporary_checksums=$(mktemp "$output_dir/.SHA256SUMS.XXXXXX")
cleanup() {
    rm -rf -- "$assembly_root"
    rm -f -- \
        "$temporary_appimage" \
        "$temporary_bundle" \
        "$temporary_checksums" \
        "$expected_rootfs_checksums"
}
trap cleanup EXIT HUP INT TERM

runtime_size=$(python3 -c \
    'import pathlib,tomllib; print(tomllib.loads(pathlib.Path(__import__("sys").argv[1]).read_text())["runtime_binary_size"])' \
    "$project_dir/LICENSES/appimage-runtime-dependencies.toml")
unsquashfs \
    -no-progress \
    -offset "$runtime_size" \
    -dest "$appdir" \
    "$appimage" >/dev/null

install -d -m0755 \
    "$bundle/runtime" \
    "$bundle/licenses/appimage/usr-share-doc" \
    "$bundle/licenses/guest-rootfs" \
    "$bundle/provenance/appimage" \
    "$bundle/provenance/guest-rootfs" \
    "$bundle/vm" \
    "$bundle/shared" \
    "$bundle/cache"
install -m0755 "$appimage" "$bundle/WildBuzzard-x86_64.AppImage"
install -m0644 "$project_dir/tools/release/portable-bundle.README.md" \
    "$bundle/README.md"
install -m0644 "$project_dir/tools/release/appimage-licenses.README.md" \
    "$bundle/licenses/appimage/README.md"

# Resolve only links proven to stay within their evidence group. The resulting
# outer bundle is an intentionally simple directories-and-regular-files tree.
python3 "$project_dir/tools/release_metadata.py" materialize \
    --source "$appdir/usr/share/doc" \
    --destination "$bundle/licenses/appimage/usr-share-doc"
cp --reflink=auto --preserve=mode,timestamps \
    "$runtime_archive" "$runtime_manifest" "$bundle/runtime/"
python3 "$project_dir/tools/release_metadata.py" materialize \
    --source "$rootfs_stage/licenses/guest-rootfs" \
    --destination "$bundle/licenses/guest-rootfs"
cp -RL --preserve=mode,timestamps \
    "$rootfs_stage/provenance/guest-rootfs/." \
    "$bundle/provenance/guest-rootfs/"
install -m0644 "$rootfs_stage/ROOTFS_SHA256SUMS" \
    "$bundle/provenance/guest-rootfs/ROOTFS_SHA256SUMS"

python3 "$project_dir/tools/release_metadata.py" appimage \
    --appimage "$appimage" \
    --appdir "$appdir" \
    --source-commit "$source_commit" \
    --output "$bundle/provenance/appimage/WildBuzzard-AppImage-linux-x86_64.json"
python3 "$project_dir/tools/release_metadata.py" bundle \
    --root "$bundle" \
    --source-commit "$source_commit" \
    --output "$bundle/provenance/bundle-files.json"
python3 "$project_dir/tools/release_metadata.py" checksums --root "$bundle"
python3 "$project_dir/tools/release_metadata.py" verify-bundle --root "$bundle"

tar \
    --sort=name \
    --format=posix \
    --pax-option=delete=atime,delete=ctime \
    --mtime="@$source_date_epoch" \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    -C "$assembly_root" \
    -cf - WildBuzzard |
    zstd -T0 -19 --long=27 --no-progress --force -o "$temporary_bundle"
zstd -q -t -- "$temporary_bundle"
chmod 0644 "$temporary_bundle"
bundle_size=$(stat -c '%s' -- "$temporary_bundle")
if (( bundle_size >= github_release_asset_limit )); then
    echo "portable bundle is not smaller than GitHub's 2 GiB asset limit" >&2
    exit 1
fi

install -d -m0755 "$roundtrip"
zstd -T0 -dc -- "$temporary_bundle" |
    tar --no-same-owner --same-permissions -xf - -C "$roundtrip"
mapfile -t roundtrip_entries < <(
    find "$roundtrip" -mindepth 1 -maxdepth 1 -printf '%f\n' | LC_ALL=C sort
)
if [[ "${roundtrip_entries[*]}" != "WildBuzzard" ]]; then
    echo "portable archive must contain exactly one WildBuzzard root directory" >&2
    exit 1
fi
python3 "$project_dir/tools/release_metadata.py" verify-bundle \
    --root "$roundtrip/WildBuzzard"

install -m0755 "$appimage" "$temporary_appimage"
final_appimage="$output_dir/WildBuzzard-x86_64.AppImage"
final_bundle="$output_dir/WildBuzzard-portable-x86_64.tar.zst"
final_checksums="$output_dir/SHA256SUMS"
mv -- "$temporary_appimage" "$final_appimage"
mv -- "$temporary_bundle" "$final_bundle"
(
    cd "$output_dir"
    sha256sum \
        WildBuzzard-x86_64.AppImage \
        WildBuzzard-portable-x86_64.tar.zst
) >"$temporary_checksums"
chmod 0644 "$temporary_checksums"
mv -- "$temporary_checksums" "$final_checksums"

cmp -- "$appimage" "$final_appimage"
(
    cd "$output_dir"
    sha256sum --check --strict SHA256SUMS
)

trap - EXIT HUP INT TERM
cleanup
printf 'Built %s\n' "$final_appimage"
printf 'Built %s\n' "$final_bundle"
