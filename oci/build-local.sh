#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

oci_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(CDPATH= cd -- "$oci_dir/.." && pwd)
task_uid=$(id -u)
variant=${BUZZARDOS_OCI_VARIANT:-standard}
case "$variant" in
    standard)
        containerfile=Containerfile
        default_image=buzzardos-desktop:local
        expect_cuda=0
        ;;
    cuda)
        containerfile=Containerfile.cuda
        default_image=buzzardos-desktop-cuda:local
        expect_cuda=1
        ;;
    *)
        echo "unsupported BUZZARDOS_OCI_VARIANT: $variant (expected standard or cuda)" >&2
        exit 2
        ;;
esac
build_root=${BUZZARDOS_BUILD_ROOT:-"${TMPDIR:-/tmp}/buzzardos-oci-build-$task_uid"}
output_dir=${BUZZARDOS_OCI_OUTPUT_DIR:-"$build_root/output/$variant"}
deb_dir=${BUZZARDOS_GUEST_DEB_DIR:-"$build_root/debs"}
image=${BUZZARDOS_OCI_TAG:-$default_image}

for command_name in buildah dpkg-deb id install mktemp realpath sha256sum; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "local OCI build dependency missing: $command_name" >&2
        exit 1
    }
done

build_root=$(realpath -m -- "$build_root")
output_dir=$(realpath -m -- "$output_dir")
deb_dir=$(realpath -m -- "$deb_dir")
for generated_dir in "$build_root" "$output_dir"; do
    case "$generated_dir/" in
        "$project_dir/"*)
            echo "refusing to place generated OCI data inside the source repository: $generated_dir" >&2
            exit 1
            ;;
    esac
done
mkdir -p "$build_root" "$output_dir" "$deb_dir"

guest_version=$(tr -d '\n' <"$project_dir/guest/GUEST_VERSION")
desktop_version=$(tr -d '\n' <"$project_dir/guest/DESKTOP_VERSION")
cua_version=$(tr -d '\n' <"$project_dir/guest/BUZZARDOSCUA_VERSION")
guest_deb="$deb_dir/buzzardos-guest_${guest_version}_amd64.deb"
desktop_deb="$deb_dir/buzzardos-desktop_${desktop_version}_amd64.deb"
cua_deb="$deb_dir/buzzardoscua_${cua_version}_amd64.deb"

if [[ ! -s "$guest_deb" || ! -s "$desktop_deb" || ! -s "$cua_deb" ]]; then
    if [[ -n ${BUZZARDOS_GUEST_DEB_DIR:-} ]]; then
        echo "BUZZARDOS_GUEST_DEB_DIR does not contain all three expected guest packages" >&2
        exit 1
    fi
    BUZZARDOS_DEB_BUILD_ROOT="$build_root/deb-build" \
    BUZZARDOS_DEB_OUTPUT_DIR="$deb_dir" \
        "$project_dir/packaging/build-debs.sh" guest desktop cua
fi
for package in "$guest_deb" "$desktop_deb" "$cua_deb"; do
    test -s "$package"
    dpkg-deb --info "$package" >/dev/null
done

context=$(mktemp -d "$build_root/.oci-context.XXXXXX")
work=$(mktemp -d "$build_root/.buildah.XXXXXX")
storage="$work/storage"
runroot="$work/runroot"
container=
mkdir -m 0700 "$storage" "$runroot"

buildah_local() {
    buildah \
        --root "$storage" \
        --runroot "$runroot" \
        --storage-driver vfs \
        "$@"
}
cleanup() {
    if [[ -n "$container" ]]; then
        buildah_local rm "$container" >/dev/null 2>&1 || true
    fi
    buildah_local rm --all >/dev/null 2>&1 || true
    buildah_local rmi --all --force >/dev/null 2>&1 || true
    rm -rf -- "$context" "$work"
}
cleanup_on_exit() {
    status=$?
    if [[ $status -ne 0 && ${BUZZARDOS_KEEP_FAILED_BUILD:-0} == 1 ]]; then
        printf 'Preserving failed Buildah work directory for diagnosis: %s\n' "$work" >&2
        return
    fi
    cleanup
}
trap cleanup_on_exit EXIT HUP INT TERM

install -D -m 0644 "$oci_dir/desktop/$containerfile" "$context/Containerfile"
install -D -m 0755 "$oci_dir/desktop/provision-image.sh" "$context/provision-image.sh"
install -d -m 0755 "$context/apt" "$context/debs"
install -m 0644 "$oci_dir/desktop/apt/debian-sid-snapshot.sources" \
    "$oci_dir/desktop/apt/debian-sid-live.sources" \
    "$oci_dir/desktop/apt/99buzzardos-snapshot" \
    "$context/apt/"
install -m 0644 "$guest_deb" "$desktop_deb" "$cua_deb" "$context/debs/"

started_at=$(date +%s)
iidfile="$work/image.id"
buildah_local build \
    --format oci \
    --no-cache \
    --pull=always \
    --force-rm \
    --iidfile "$iidfile" \
    --tag "$image" \
    --file "$context/Containerfile" \
    "$context"
finished_at=$(date +%s)
build_seconds=$((finished_at - started_at))
image_id=$(tr -d '\n' <"$iidfile")
test -n "$image_id"

container=$(buildah_local from "$image_id")
BUZZARDOS_CONTAINER_ENGINE=buildah \
BUZZARDOS_BUILDAH_ROOT="$storage" \
BUZZARDOS_BUILDAH_RUNROOT="$runroot" \
BUZZARDOS_EXPECT_CUDA="$expect_cuda" \
    "$oci_dir/verify-image.sh" "$container"

inventory_tmp=$(mktemp "$output_dir/.dpkg-packages.tsv.XXXXXX")
buildah_local run "$container" -- \
    /usr/bin/dpkg-query --show '--showformat=${binary:Package}\t${Version}\n' |
    LC_ALL=C sort >"$inventory_tmp"
test -s "$inventory_tmp"
mv -f -- "$inventory_tmp" "$output_dir/dpkg-packages.tsv"
printf '%s\n' "$image_id" >"$output_dir/image-id.txt"
printf '%s\n' "$build_seconds" >"$output_dir/build-seconds.txt"

if [[ ${BUZZARDOS_EXPORT_ARCHIVE:-0} == 1 ]]; then
    archive="$output_dir/buzzardos-desktop-$variant-amd64.oci.tar"
    temporary="$archive.tmp"
    rm -f -- "$temporary"
    buildah_local push --format oci "$image_id" \
        "oci-archive:$temporary:buzzardos-desktop-$variant"
    mv -f -- "$temporary" "$archive"
    sha256sum "$archive" >"$archive.sha256"
    stat -c '%s' "$archive" >"$output_dir/archive-size.bytes"
    printf 'Exported %s (%s bytes)\n' \
        "$archive" "$(cat "$output_dir/archive-size.bytes")"
fi

printf 'Verified %s (%s variant); uncached Buildah assembly took %s seconds.\n' \
    "$image" "$variant" "$build_seconds"
printf 'All temporary Buildah images and storage will now be discarded from %s.\n' \
    "$work"
