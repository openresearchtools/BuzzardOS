#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

oci_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(CDPATH= cd -- "$oci_dir/.." && pwd)
task_uid=$(id -u)
build_root=${WILDBUZZARD_BUILD_ROOT:-"${TMPDIR:-/tmp}/wildbuzzard-build-$task_uid"}
output_dir=${WILDBUZZARD_OCI_OUTPUT_DIR:-"$build_root/oci"}
image=${WILDBUZZARD_OCI_TAG:-wildbuzzard-desktop:local}
output_dir=$(realpath -m -- "$output_dir")
case "$output_dir/" in
    "$project_dir/"*)
        echo "refusing to place OCI output inside the source repository: $output_dir" >&2
        exit 1
        ;;
esac

for command_name in docker gzip id realpath sha256sum; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "local OCI build dependency missing: $command_name" >&2
        exit 1
    }
done
docker info >/dev/null
mkdir -p "$output_dir"

export WILDBUZZARD_OCI_TAG=$image
docker compose --project-directory "$project_dir" \
    -f "$oci_dir/compose.yaml" \
    build desktop
"$oci_dir/verify-image.sh" "$image"

unpacked_size=$(docker image inspect --format '{{.Size}}' "$image")
image_id=$(docker image inspect --format '{{.Id}}' "$image")
printf '%s\n' "$unpacked_size" >"$output_dir/image-size.bytes"
printf '%s\n' "$image_id" >"$output_dir/image-id.txt"

package_inventory=$(mktemp "$output_dir/.dpkg-packages.tsv.XXXXXX")
cleanup_inventory() {
    rm -f -- "$package_inventory"
}
trap cleanup_inventory EXIT
docker run --rm --entrypoint /usr/bin/dpkg-query "$image" \
    --show \
    '--showformat=${binary:Package}\t${Version}\n' |
    LC_ALL=C sort >"$package_inventory"
test -s "$package_inventory"
mv -f -- "$package_inventory" "$output_dir/dpkg-packages.tsv"
trap - EXIT

printf 'Verified %s (unpacked image bytes: %s)\n' "$image" "$unpacked_size"
printf 'Recorded image identity and installed package inventory under %s\n' \
    "$output_dir"

if [[ ${WILDBUZZARD_EXPORT_ARCHIVE:-0} == 1 ]]; then
    archive="$output_dir/wildbuzzard-desktop-amd64.docker.tar.gz"
    temporary="$archive.tmp"
    trap 'rm -f -- "$temporary"' EXIT
    docker save "$image" | gzip -n -9 >"$temporary"
    mv -f -- "$temporary" "$archive"
    sha256sum "$archive" >"$archive.sha256"
    stat -c '%s' "$archive" >"$output_dir/archive-size.bytes"
    trap - EXIT
    printf 'Exported %s (%s bytes)\n' "$archive" "$(cat "$output_dir/archive-size.bytes")"
fi
