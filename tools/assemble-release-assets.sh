#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

usage() {
    printf '%s\n' \
        'Usage: tools/assemble-release-assets.sh PORTABLE_APP_ROOT ROOTFS_STAGE OUTPUT_DIRECTORY' \
        '' \
        'Create BuzzardOS-portable-linux-x86_64.tar.xz. The archive root is' \
        'BuzzardOS/ and contains the launcher, app/, Machines/, and shared/.'
}

[[ $# -eq 3 ]] || { usage >&2; exit 2; }
project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
portable_app_root=$(realpath -- "$1")
rootfs_stage=$(realpath -- "$2")
output_dir=$(realpath -m -- "$3")
source_commit=$(git -C "$project_dir" rev-parse --verify HEAD^{commit})
source_date_epoch=$(git -C "$project_dir" show -s --format=%ct "$source_commit")

for required in \
    "$portable_app_root/BuzzardOS" \
    "$portable_app_root/Install-Dependencies" \
    "$portable_app_root/app/AppRun" \
    "$portable_app_root/app/usr/bin/buzzardos" \
    "$rootfs_stage/runtime/default-rootfs.oci.tar.zst" \
    "$rootfs_stage/runtime/default-rootfs.oci.json" \
    "$rootfs_stage/ROOTFS_SHA256SUMS"; do
    [[ -f "$required" && ! -L "$required" ]] || {
        echo "portable assembly input is missing or unsafe: $required" >&2
        exit 1
    }
done
for command_name in cmp find git install jq python3 readelf realpath sha256sum stat tar xz zstd; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "portable assembly dependency missing: $command_name" >&2
        exit 1
    }
done
python3 "$project_dir/tools/verify-portable-release-inputs.py" \
    --rootfs-stage "$rootfs_stage" \
    --project-root "$project_dir" \
    --source-commit "$source_commit"
python3 "$project_dir/tools/verify-elf-glibc-floor.py" \
    --root "$portable_app_root/app" \
    --maximum 2.39
case "$output_dir/" in
    "$project_dir/"*) echo 'refusing to place release output inside the source tree' >&2; exit 1 ;;
esac
if [[ -e "$output_dir" ]]; then
    [[ -d "$output_dir" && ! -L "$output_dir" ]] || {
        echo "release output must be a real directory: $output_dir" >&2
        exit 1
    }
    find "$output_dir" -mindepth 1 -print -quit | grep -q . && {
        echo "release output directory must be empty: $output_dir" >&2
        exit 1
    }
else
    mkdir -p "$output_dir"
fi

assembly_root=$(mktemp -d "${RUNNER_TEMP:-/tmp}/buzzardos-assembly.XXXXXX")
bundle="$assembly_root/BuzzardOS"
roundtrip="$assembly_root/roundtrip"
final_archive="$output_dir/BuzzardOS-portable-linux-x86_64.tar.xz"
final_checksum="$final_archive.sha256"
temporary_archive=$(mktemp "$output_dir/.BuzzardOS-portable-linux-x86_64.tar.xz.XXXXXX")
temporary_checksum=$(mktemp "$output_dir/.BuzzardOS-portable-linux-x86_64.tar.xz.sha256.XXXXXX")
completed=false
cleanup() {
    rm -rf -- "$assembly_root"
    rm -f -- "$temporary_archive" "$temporary_checksum"
    if [[ "$completed" != true ]]; then
        rm -f -- "$final_archive" "$final_checksum"
    fi
}
trap cleanup EXIT HUP INT TERM

install -d -m0755 "$bundle" "$bundle/Machines" "$bundle/shared"
install -m0755 "$portable_app_root/BuzzardOS" "$bundle/BuzzardOS"
install -m0755 "$portable_app_root/Install-Dependencies" "$bundle/Install-Dependencies"
cp -a -- "$portable_app_root/app" "$bundle/app"
install -d -m0755 \
    "$bundle/app/runtime" \
    "$bundle/app/licenses/host" \
    "$bundle/app/licenses/host/usr-share-doc" \
    "$bundle/app/licenses/guest" \
    "$bundle/app/provenance/host" \
    "$bundle/app/provenance/guest"
install -m0644 \
    "$rootfs_stage/runtime/default-rootfs.oci.tar.zst" \
    "$rootfs_stage/runtime/default-rootfs.oci.json" \
    "$bundle/app/runtime/"
cmp "$rootfs_stage/runtime/default-rootfs.oci.tar.zst" \
    "$bundle/app/runtime/default-rootfs.oci.tar.zst"
cmp "$rootfs_stage/runtime/default-rootfs.oci.json" \
    "$bundle/app/runtime/default-rootfs.oci.json"

# Resolve notice symlinks only within their own source tree. The executable app
# keeps its runtime symlinks, while the two evidence groups are plain files.
python3 "$project_dir/tools/release_metadata.py" materialize \
    --source "$bundle/app/usr/share/doc" \
    --destination "$bundle/app/licenses/host/usr-share-doc"
python3 "$project_dir/tools/release_metadata.py" materialize \
    --source "$rootfs_stage/licenses/guest" \
    --destination "$bundle/app/licenses/guest"
install -m0644 "$project_dir/tools/release/host-app-licenses.README.md" \
    "$bundle/app/licenses/host/README.md"
cp -a -- "$rootfs_stage/provenance/guest/." "$bundle/app/provenance/guest/"
install -m0644 "$rootfs_stage/ROOTFS_SHA256SUMS" \
    "$bundle/app/provenance/guest/ROOTFS_SHA256SUMS"
install -m0644 \
    "$project_dir/LICENSES/release-components.toml" \
    "$project_dir/LICENSES/package-inputs.toml" \
    "$project_dir/host/packaging/icons/README.md" \
    "$bundle/app/provenance/host/"
install -m0644 "$project_dir/host/packaging/icons/low-glide-source.png" \
    "$bundle/app/provenance/host/low-glide-source.png"
"$project_dir/tools/create-project-source-archive.sh" \
    "$bundle/app/licenses/host/project-source"

install -m0644 "$project_dir/tools/release/portable-bundle.README.md" "$bundle/README.md"
(
    cd "$bundle"
    find BuzzardOS Install-Dependencies app -type f \
        ! -path 'app/runtime/*' \
        ! -path 'app/licenses/*' \
        ! -path 'app/provenance/*' \
        -print0 |
        LC_ALL=C sort -z |
        xargs -0 sha256sum >app/provenance/host/PAYLOAD_SHA256SUMS
    sha256sum --check --strict app/provenance/host/PAYLOAD_SHA256SUMS
)
jq -n \
    --arg source_commit "$source_commit" \
    --arg archive "app/runtime/default-rootfs.oci.tar.zst" \
    --arg archive_sha256 "$(sha256sum "$bundle/app/runtime/default-rootfs.oci.tar.zst" | awk '{print $1}')" \
    --argjson archive_size "$(stat -c '%s' "$bundle/app/runtime/default-rootfs.oci.tar.zst")" \
    --arg metadata_sha256 "$(sha256sum "$bundle/app/runtime/default-rootfs.oci.json" | awk '{print $1}')" \
    --arg manifest_digest "$(jq -er '.manifest_digest' "$bundle/app/runtime/default-rootfs.oci.json")" \
    --arg source_manifest_digest "$(jq -er '.source_manifest_digest' "$bundle/app/runtime/default-rootfs.oci.json")" \
    --arg payload_sha256 "$(sha256sum "$bundle/app/provenance/host/PAYLOAD_SHA256SUMS" | awk '{print $1}')" \
    --arg base_images_sha256 "$(sha256sum "$bundle/app/provenance/guest/base-images.lock.toml" | awk '{print $1}')" \
    --arg sway_sha256 "$(sha256sum "$bundle/app/provenance/guest/SWAY_UPSTREAM.toml" | awk '{print $1}')" \
    --arg trycua_sha256 "$(sha256sum "$bundle/app/provenance/guest/TRYCUA_UPSTREAM.toml" | awk '{print $1}')" \
    --arg package_inventory_sha256 "$(sha256sum "$bundle/app/provenance/guest/oci-packages.tsv" | awk '{print $1}')" \
    --arg icon_sha256 "$(sha256sum "$bundle/app/provenance/host/low-glide-source.png" | awk '{print $1}')" \
    '{
      schema:1,
      kind:"buzzardos-portable",
      platform:{os:"linux",architecture:"x86_64"},
      source_commit:$source_commit,
      host_application:{
        checksums:{path:"app/provenance/host/PAYLOAD_SHA256SUMS",sha256:$payload_sha256}
      },
      rootfs:{
        archive:{path:$archive,size:$archive_size,sha256:$archive_sha256},
        metadata:{path:"app/runtime/default-rootfs.oci.json",sha256:$metadata_sha256},
        manifest_digest:$manifest_digest,
        source_manifest_digest:$source_manifest_digest,
        source_descriptors:[
          {path:"app/provenance/guest/base-images.lock.toml",sha256:$base_images_sha256},
          {path:"app/provenance/guest/SWAY_UPSTREAM.toml",sha256:$sway_sha256},
          {path:"app/provenance/guest/TRYCUA_UPSTREAM.toml",sha256:$trycua_sha256}
        ],
        package_inventory:{path:"app/provenance/guest/oci-packages.tsv",sha256:$package_inventory_sha256}
      },
      icon:{source:"app/provenance/host/low-glide-source.png",sha256:$icon_sha256}
    }' \
    >"$bundle/app/provenance/manifest.json"

(
    cd "$bundle"
    find . -type f ! -path './SHA256SUMS' -print0 |
        LC_ALL=C sort -z |
        xargs -0 sha256sum >SHA256SUMS
    sha256sum --check --strict SHA256SUMS
)

tar \
    --sort=name \
    --format=posix \
    --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
    --mtime="@$source_date_epoch" \
    --owner=0 --group=0 --numeric-owner \
    -C "$assembly_root" -cf - BuzzardOS |
    xz -9e -T0 >"$temporary_archive"
xz -t -- "$temporary_archive"

archive_size=$(stat -c '%s' "$temporary_archive")
if (( archive_size >= 2 * 1024 * 1024 * 1024 )); then
    echo 'portable archive is not smaller than the 2 GiB Actions/Release guard' >&2
    exit 1
fi
mkdir -p "$roundtrip"
xz -dc -- "$temporary_archive" | tar --no-same-owner -xf - -C "$roundtrip"
mapfile -t roots < <(find "$roundtrip" -mindepth 1 -maxdepth 1 -printf '%f\n' | LC_ALL=C sort)
[[ "${roots[*]}" == BuzzardOS ]] || {
    echo 'portable archive must contain exactly one BuzzardOS root directory' >&2
    exit 1
}
(
    cd "$roundtrip/BuzzardOS"
    mapfile -t entries < <(find . -mindepth 1 -maxdepth 1 -printf '%f\n' | LC_ALL=C sort)
    [[ "${entries[*]}" == \
        'BuzzardOS Install-Dependencies Machines README.md SHA256SUMS app shared' ]]
    ! find Machines shared -mindepth 1 -print -quit | grep -q .
    sha256sum --check --strict SHA256SUMS
    test -x ./BuzzardOS
    test -x ./app/AppRun
    test -x ./app/usr/bin/buzzardos
    test -f ./app/runtime/default-rootfs.oci.tar.zst
    test -f ./app/runtime/default-rootfs.oci.json
    test -f ./app/licenses/host/README.md
    test -f ./app/licenses/guest/README.md
    test -f ./app/provenance/manifest.json
    test -f ./app/provenance/host/PAYLOAD_SHA256SUMS
    test -f ./app/provenance/guest/ROOTFS_SHA256SUMS
    cmp ./app/runtime/default-rootfs.oci.json \
        ./app/provenance/guest/default-rootfs.oci.json
    sha256sum --check --strict ./app/provenance/host/PAYLOAD_SHA256SUMS
)

mv -- "$temporary_archive" "$final_archive"
(
    cd "$output_dir"
    sha256sum "$(basename -- "$final_archive")" >"$temporary_checksum"
    sha256sum --check --strict "$temporary_checksum"
)
mv -- "$temporary_checksum" "$final_checksum"
completed=true
trap - EXIT HUP INT TERM
cleanup
printf 'Built %s (%s bytes)\n' "$final_archive" "$archive_size"
