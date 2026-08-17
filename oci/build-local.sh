#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

oci_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(CDPATH= cd -- "$oci_dir/.." && pwd)
task_uid=$(id -u)
build_root=${BUZZARDOS_BUILD_ROOT:-"${TMPDIR:-/tmp}/buzzardos-build-$task_uid"}
output_dir=${BUZZARDOS_OCI_OUTPUT_DIR:-"$build_root/oci"}
image=${BUZZARDOS_OCI_TAG:-buzzardos-desktop:local}
container_engine=${BUZZARDOS_CONTAINER_ENGINE:-auto}
output_dir=$(realpath -m -- "$output_dir")
case "$output_dir/" in
    "$project_dir/"*)
        echo "refusing to place OCI output inside the source repository: $output_dir" >&2
        exit 1
        ;;
esac

if [[ "$container_engine" == auto ]]; then
    if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
        container_engine=docker
    elif command -v podman >/dev/null 2>&1 && podman info >/dev/null 2>&1; then
        container_engine=podman
    else
        echo 'no usable local Docker or Podman build engine is available' >&2
        exit 1
    fi
fi
case "$container_engine" in
    docker|podman) ;;
    *) echo "BUZZARDOS_CONTAINER_ENGINE must be auto, docker, or podman" >&2; exit 2 ;;
esac
for command_name in "$container_engine" id realpath sha256sum; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "local OCI build dependency missing: $command_name" >&2
        exit 1
    }
done
"$container_engine" info >/dev/null
mkdir -p "$output_dir"

if [[ "$container_engine" == docker ]]; then
    export BUZZARDOS_OCI_TAG=$image
    docker compose --project-directory "$project_dir" \
        -f "$oci_dir/compose.yaml" \
        build desktop
else
    podman build --tag "$image" \
        --file "$oci_dir/desktop/Containerfile" "$project_dir"
fi
BUZZARDOS_CONTAINER_ENGINE="$container_engine" "$oci_dir/verify-image.sh" "$image"

unpacked_size=$("$container_engine" image inspect --format '{{.Size}}' "$image")
image_id=$("$container_engine" image inspect --format '{{.Id}}' "$image")
printf '%s\n' "$unpacked_size" >"$output_dir/image-size.bytes"
printf '%s\n' "$image_id" >"$output_dir/image-id.txt"

package_inventory=$(mktemp "$output_dir/.dpkg-packages.tsv.XXXXXX")
cleanup_inventory() {
    rm -f -- "$package_inventory"
}
trap cleanup_inventory EXIT
"$container_engine" run --rm --entrypoint /usr/bin/dpkg-query "$image" \
    --show \
    '--showformat=${binary:Package}\t${Version}\n' |
    LC_ALL=C sort >"$package_inventory"
test -s "$package_inventory"
mv -f -- "$package_inventory" "$output_dir/dpkg-packages.tsv"
trap - EXIT

printf 'Verified %s (unpacked image bytes: %s)\n' "$image" "$unpacked_size"
printf 'Recorded image identity and installed package inventory under %s\n' \
    "$output_dir"

if [[ ${BUZZARDOS_EXPORT_ARCHIVE:-0} == 1 ]]; then
    archive="$output_dir/buzzardos-desktop-amd64.oci.tar"
    temporary="$archive.tmp"
    trap 'rm -f -- "$temporary"' EXIT
    if [[ "$container_engine" == podman ]]; then
        podman save --quiet --format oci-archive --output "$temporary" "$image"
    else
        command -v skopeo >/dev/null 2>&1 || {
            echo 'OCI archive export with Docker requires skopeo' >&2
            exit 1
        }
        skopeo copy "docker-daemon:$image" "oci-archive:$temporary:buzzardos-desktop"
    fi
    mv -f -- "$temporary" "$archive"
    sha256sum "$archive" >"$archive.sha256"
    stat -c '%s' "$archive" >"$output_dir/archive-size.bytes"
    trap - EXIT
    printf 'Exported %s (%s bytes)\n' "$archive" "$(cat "$output_dir/archive-size.bytes")"
fi
