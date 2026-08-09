#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: tools/build-release-rootfs.sh APPIMAGE OUTPUT_DIRECTORY

Build the pinned linux/amd64 reference image in the runner's local Docker
daemon, flatten it, verify a metadata-preserving round trip, and emit the
high-compression rootfs payload plus guest license/provenance evidence.

Nothing is pushed to a registry. OUTPUT_DIRECTORY must be outside the source
checkout. The caller needs passwordless sudo for root-owned rootfs metadata.
EOF
}

if [[ $# -ne 2 ]]; then
    usage >&2
    exit 2
fi

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
appimage_argument=$1
[[ -f "$appimage_argument" && ! -L "$appimage_argument" ]] || {
    echo "AppImage must be a regular non-symlink file: $appimage_argument" >&2
    exit 1
}
appimage=$(realpath -- "$appimage_argument")
output_dir=$(realpath -m -- "$2")
source_commit=$(git -C "$project_dir" rev-parse --verify HEAD^{commit})
short_commit=${source_commit:0:12}
runner_uid=$(id -u)
runner_gid=$(id -g)

case "$output_dir/" in
    "$project_dir/"*)
        echo "refusing to place release rootfs output inside the source repository" >&2
        exit 1
        ;;
esac

for command_name in docker findmnt git jq python3 realpath rsync sha256sum skopeo sudo tar zstd; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "release rootfs dependency missing: $command_name" >&2
        exit 1
    }
done
[[ -x "$appimage" ]] || chmod 0755 "$appimage"
docker info >/dev/null
sudo -n true

dirty=$(git -C "$project_dir" status --porcelain=v1 --untracked-files=all)
if [[ -n "$dirty" ]]; then
    echo "refusing to assemble release assets from a dirty source tree" >&2
    printf '%s\n' "$dirty" >&2
    exit 1
fi

if [[ -e "$output_dir" ]]; then
    [[ -d "$output_dir" && ! -L "$output_dir" ]] || {
        echo "release rootfs output must be a real directory: $output_dir" >&2
        exit 1
    }
    if find "$output_dir" -mindepth 1 -print -quit | grep -q .; then
        echo "release rootfs output directory must be empty: $output_dir" >&2
        exit 1
    fi
else
    mkdir -p "$output_dir"
fi

build_root=$(mktemp -d "${RUNNER_TEMP:-/tmp}/wildbuzzard-rootfs-release.XXXXXX")
image="wildbuzzard-desktop:release-$short_commit-${GITHUB_RUN_ID:-local}"
builder="wildbuzzard-release-${GITHUB_RUN_ID:-local}-${RANDOM}"
image_loaded=false
builder_created=false
cleanup() {
    if [[ "$image_loaded" == true ]]; then
        docker image rm --force "$image" >/dev/null 2>&1 || true
    fi
    if [[ "$builder_created" == true ]]; then
        docker buildx rm --force "$builder" >/dev/null 2>&1 || true
    fi
    sudo rm -rf -- "$build_root"
}
trap cleanup EXIT HUP INT TERM

layout="$build_root/oci-layout"
rootfs="$build_root/rootfs"
roundtrip="$build_root/roundtrip-rootfs"
work_dir="$build_root/apply-work"
mkdir -p "$layout" "$rootfs" "$roundtrip" "$work_dir"

docker buildx create \
    --name "$builder" \
    --driver docker-container >/dev/null
builder_created=true
docker buildx build \
    --builder "$builder" \
    --load \
    --platform linux/amd64 \
    --provenance=false \
    --sbom=false \
    --tag "$image" \
    --file "$project_dir/oci/desktop/Containerfile" \
    --progress plain \
    "$project_dir"
image_loaded=true
"$project_dir/oci/verify-image.sh" "$image"

skopeo copy \
    --override-os linux \
    --override-arch amd64 \
    "docker-daemon:$image" \
    "oci:$layout:wildbuzzard-desktop"
manifest_digest=$(jq -er '
    if .schemaVersion != 2 then error("OCI index schema is not 2")
    elif (.manifests | length) != 1 then
        error("OCI layout is not a single-platform image")
    else .manifests[0].digest
    end
' "$layout/index.json")

# The disposable runner no longer needs either the daemon image or BuildKit's
# layer cache once the verified OCI layout exists.  Pruning only this dedicated
# builder avoids touching unrelated local builders when the script is used by
# a developer.
docker image rm --force "$image" >/dev/null
image_loaded=false
docker buildx prune --builder "$builder" --all --force >/dev/null
docker buildx rm --force "$builder" >/dev/null
builder_created=false

sudo env APPIMAGE_EXTRACT_AND_RUN=1 \
    "$appimage" __apply-image \
    --archive "$layout" \
    --expected-digest "$manifest_digest" \
    --rootfs "$rootfs" \
    --work-dir "$work_dir"

sudo env \
    PATH="$PATH" \
    CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
    RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
    python3 "$project_dir/tools/license_audit.py" \
    --guest-rootfs "$rootfs" \
    --structural

if awk -v prefix="$rootfs/" '$5 == substr(prefix, 1, length(prefix)-1) || index($5, prefix) == 1 { found=1 } END { exit found ? 0 : 1 }' /proc/self/mountinfo; then
    echo "refusing to archive a rootfs containing active mounts" >&2
    exit 1
fi

runtime_dir="$output_dir/runtime"
guest_licenses="$output_dir/licenses/guest-rootfs"
guest_provenance="$output_dir/provenance/guest-rootfs"
mkdir -p \
    "$runtime_dir" \
    "$guest_licenses/usr-share-doc" \
    "$guest_licenses/usr-share-common-licenses" \
    "$guest_provenance"
archive="$runtime_dir/WildBuzzard-rootfs-linux-x86_64.tar.zst"
temporary=$(mktemp "$runtime_dir/.WildBuzzard-rootfs-linux-x86_64.tar.zst.XXXXXX")
archive_cleanup() {
    rm -f -- "$temporary"
}
trap 'archive_cleanup; cleanup' EXIT HUP INT TERM

sudo tar \
    --sort=name \
    --format=posix \
    --pax-option=delete=atime,delete=ctime \
    --numeric-owner \
    --acls \
    --selinux \
    --xattrs \
    --xattrs-include='*' \
    --sparse \
    --one-file-system \
    -C "$rootfs" \
    -cf - . |
    zstd -T0 -19 --long=27 --no-progress --force -o "$temporary"
zstd -t "$temporary"
mv -- "$temporary" "$archive"
chmod 0644 "$archive"
trap cleanup EXIT HUP INT TERM

sudo env PATH="$PATH" python3 "$project_dir/tools/release_metadata.py" rootfs \
    --rootfs "$rootfs" \
    --archive "$archive" \
    --oci-layout "$layout" \
    --source-commit "$source_commit" \
    --output "$runtime_dir/WildBuzzard-rootfs-linux-x86_64.json"
sudo chown "$runner_uid:$runner_gid" \
    "$runtime_dir/WildBuzzard-rootfs-linux-x86_64.json"
python3 "$project_dir/tools/release_metadata.py" verify \
    --archive "$archive" \
    --manifest "$runtime_dir/WildBuzzard-rootfs-linux-x86_64.json"

# Exercise the distributed AppImage's real first-machine path against the
# exact rootfs seed. This proves zstd/PAX/xattr handling, bundled helper
# resolution, and the recipient-style keep-id subordinate ownership map rather
# than verifying the archive only with a second GNU tar invocation.
subuid_start=$(awk -F: -v owner="$(id -un)" -v numeric="$runner_uid" \
    '($1 == owner || $1 == numeric) && $3 >= 65535 { print $2; exit }' \
    /etc/subuid)
subgid_start=$(awk -F: -v owner="$(id -un)" -v numeric="$runner_uid" \
    '($1 == owner || $1 == numeric) && $3 >= 65535 { print $2; exit }' \
    /etc/subgid)
if [[ -z "$subuid_start" || -z "$subgid_start" ]]; then
    echo "runner account needs a 65535-ID subordinate UID/GID range" >&2
    exit 1
fi
install -d -m0755 "$roundtrip/runtime"
cp --reflink=auto --preserve=mode,timestamps \
    "$archive" \
    "$runtime_dir/WildBuzzard-rootfs-linux-x86_64.json" \
    "$roundtrip/runtime/"
env APPIMAGE_EXTRACT_AND_RUN=1 "$appimage" \
    --storage-dir "$roundtrip" \
    create release-seed-smoke
mapped_rootfs="$roundtrip/vm/release-seed-smoke/rootfs"
roundtrip_diff="$build_root/roundtrip.diff"
# Linux rewrites this one xattr from VFS v2 to namespaced VFS v3. The
# verifier below compares its masks, flags, revision, and root ID directly.
sudo rsync \
    -aHAXnci \
    --delete \
    --filter='-x security.capability' \
    --numeric-ids \
    --no-owner \
    --no-group \
    "$rootfs/" "$mapped_rootfs/" >"$roundtrip_diff"
if [[ -s "$roundtrip_diff" ]]; then
    echo "AppImage first-machine extraction changed rootfs content or metadata" >&2
    cat "$roundtrip_diff" >&2
    exit 1
fi
sudo env PATH="$PATH" python3 "$project_dir/tools/release_metadata.py" \
    verify-idmapped-copy \
    --canonical "$rootfs" \
    --mapped "$mapped_rootfs" \
    --host-uid "$runner_uid" \
    --host-gid "$runner_gid" \
    --subuid-start "$subuid_start" \
    --subgid-start "$subgid_start"
sudo rm -rf -- "$roundtrip"

sudo rm -rf -- "$layout"

# Debian notice directories legitimately use internal symlinks. Materialize
# only targets proven to remain inside this evidence group; reject absolute or
# escaping links instead of accidentally copying from the runner host.
sudo env PATH="$PATH" python3 "$project_dir/tools/release_metadata.py" materialize \
    --source "$rootfs/usr/share/doc" \
    --destination "$guest_licenses/usr-share-doc"
sudo env PATH="$PATH" python3 "$project_dir/tools/release_metadata.py" materialize \
    --source "$rootfs/usr/share/common-licenses" \
    --destination "$guest_licenses/usr-share-common-licenses"
sudo chown -hR "$runner_uid:$runner_gid" "$guest_licenses/usr-share-doc"
sudo chown -hR "$runner_uid:$runner_gid" \
    "$guest_licenses/usr-share-common-licenses"
"$project_dir/tools/create-project-source-archive.sh" \
    "$guest_licenses/project-source"
install -m0644 "$project_dir/tools/release/guest-rootfs-licenses.README.md" \
    "$guest_licenses/README.md"

install -m0644 \
    "$project_dir/oci/base-images.lock.toml" \
    "$project_dir/oci/desktop/SWAY_UPSTREAM.toml" \
    "$project_dir/LICENSES/release-components.toml" \
    "$project_dir/LICENSES/generated/oci-packages.tsv" \
    "$guest_provenance/"
install -m0644 "$project_dir/guest/third_party/trycua-cua/UPSTREAM.toml" \
    "$guest_provenance/TRYCUA_UPSTREAM.toml"
install -m0644 "$project_dir/guest/third_party/trycua-cua/CHANGES.WILDBUZZARD.md" \
    "$guest_provenance/TRYCUA_CHANGES.WILDBUZZARD.md"
install -m0644 "$runtime_dir/WildBuzzard-rootfs-linux-x86_64.json" \
    "$guest_provenance/"

(
    cd "$output_dir"
    sha256sum \
        runtime/WildBuzzard-rootfs-linux-x86_64.tar.zst \
        runtime/WildBuzzard-rootfs-linux-x86_64.json \
        >ROOTFS_SHA256SUMS
)

printf 'Built verified flat rootfs payload: %s\n' "$archive"
