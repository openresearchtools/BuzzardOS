#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

usage() {
    printf '%s\n' \
        'Usage: tools/build-release-rootfs.sh PORTABLE_ROOT OUTPUT_DIRECTORY' \
        '' \
        'Build the reference OCI image in a disposable local Docker builder,' \
        'verify it, and emit the compressed OCI install seed plus guest notices.'
}

[[ $# -eq 2 ]] || { usage >&2; exit 2; }
project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
portable_root=$(realpath -- "$1")
output_dir=$(realpath -m -- "$2")
launcher="$portable_root/BuzzardOS"
source_commit=$(git -C "$project_dir" rev-parse --verify HEAD^{commit})
source_date_epoch=$(git -C "$project_dir" show -s --format=%ct "$source_commit")
short_commit=${source_commit:0:12}
runner_uid=$(id -u)
runner_gid=$(id -g)

[[ -x "$launcher" && ! -L "$launcher" ]] || {
    echo "portable BuzzardOS launcher is missing: $launcher" >&2
    exit 1
}
case "$output_dir/" in
    "$project_dir/"*) echo 'refusing to build release data inside the source tree' >&2; exit 1 ;;
esac
container_engine=${BUZZARDOS_CONTAINER_ENGINE:-auto}
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

for command_name in "$container_engine" findmnt git jq python3 realpath sha256sum tar zstd; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "rootfs build dependency missing: $command_name" >&2
        exit 1
    }
done
"$container_engine" info >/dev/null
if [[ "$container_engine" == docker ]]; then
    command -v skopeo >/dev/null 2>&1 || {
        echo 'rootfs build dependency missing: skopeo' >&2
        exit 1
    }
    docker buildx version >/dev/null
fi
if [[ -e "$output_dir" ]]; then
    [[ -d "$output_dir" && ! -L "$output_dir" ]] || {
        echo "output must be a real directory: $output_dir" >&2
        exit 1
    }
    find "$output_dir" -mindepth 1 -print -quit | grep -q . && {
        echo "output directory must be empty: $output_dir" >&2
        exit 1
    }
else
    mkdir -p "$output_dir"
fi

build_root=$(mktemp -d "${RUNNER_TEMP:-/tmp}/buzzardos-rootfs.XXXXXX")
roundtrip_root="$build_root/import-roundtrip"
image="buzzardos-desktop:artifact-$short_commit-${GITHUB_RUN_ID:-local}"
builder="buzzardos-artifact-${GITHUB_RUN_ID:-local}-${RANDOM}"
image_loaded=false
builder_created=false
cleanup() {
    if [[ "$image_loaded" == true ]]; then
        "$container_engine" image rm --force "$image" >/dev/null 2>&1 || true
    fi
    if [[ "$container_engine" == docker && "$builder_created" == true ]]; then
        docker buildx rm --force "$builder" >/dev/null 2>&1 || true
    fi
    if [[ -x "$launcher" && -f "$roundtrip_root/Machines/imported/machine.json" ]]; then
        "$launcher" --storage-dir "$roundtrip_root" delete imported --yes \
            >/dev/null 2>&1 || true
    fi
    find "$build_root" -depth -delete 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

layout="$build_root/oci-layout"
mkdir -p "$layout" "$roundtrip_root"

if [[ "$container_engine" == docker ]]; then
    docker buildx create --name "$builder" --driver docker-container >/dev/null
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
else
    podman build \
        --platform linux/amd64 \
        --tag "$image" \
        --file "$project_dir/oci/desktop/Containerfile" \
        "$project_dir"
fi
image_loaded=true
BUZZARDOS_CONTAINER_ENGINE="$container_engine" \
    "$project_dir/oci/verify-image.sh" "$image"
if [[ "$container_engine" == docker ]]; then
    skopeo copy --override-os linux --override-arch amd64 \
        "docker-daemon:$image" "oci:$layout:buzzardos-desktop"
else
    podman save --quiet --format oci-dir --output "$layout" "$image"
fi

source_manifest_digest=$(jq -er '
  if .schemaVersion != 2 or (.manifests | length) != 1 then
    error("reference OCI index must contain exactly one manifest")
  else .manifests[0].digest end
' "$layout/index.json")
[[ "$source_manifest_digest" =~ ^sha256:[0-9a-f]{64}$ ]]
source_manifest="$layout/blobs/sha256/${source_manifest_digest#sha256:}"
source_config_digest=$(jq -er '.config.digest' "$source_manifest")
[[ "$source_config_digest" =~ ^sha256:[0-9a-f]{64}$ ]]
source_config="$layout/blobs/sha256/${source_config_digest#sha256:}"
[[ -f "$source_config" && ! -L "$source_config" ]]

"$container_engine" image rm --force "$image" >/dev/null
image_loaded=false
if [[ "$container_engine" == docker ]]; then
    docker buildx prune --builder "$builder" --all --force >/dev/null
    docker buildx rm --force "$builder" >/dev/null
    builder_created=false
fi

runtime_dir="$output_dir/runtime"
guest_licenses="$output_dir/licenses/guest"
guest_provenance="$output_dir/provenance/guest"
mkdir -p \
    "$runtime_dir" \
    "$guest_licenses/usr-share-doc" \
    "$guest_licenses/usr-share-common-licenses" \
    "$guest_provenance"
archive="$runtime_dir/default-rootfs.oci.tar.zst"
# Materialize the verified source image with the exact distributed importer.
# The mapped rootfs is then snapshotted from its canonical guest-ID namespace
# by the launcher's pinned export tar. This creates one identity-free layer;
# host-side tar must never interpret subordinate owners or security metadata.
"$launcher" --storage-dir "$roundtrip_root" import "$layout" --name imported
mapped_rootfs="$roundtrip_root/Machines/imported/rootfs"
subuid_start=$(awk -F: -v owner="$(id -un)" -v numeric="$runner_uid" \
    '($1 == owner || $1 == numeric) && $3 >= 65535 { print $2; exit }' /etc/subuid)
subgid_start=$(awk -F: -v owner="$(id -un)" -v numeric="$runner_gid" \
    '($1 == owner || $1 == numeric) && $3 >= 65535 { print $2; exit }' /etc/subgid)
[[ -n "$subuid_start" && -n "$subgid_start" ]] || {
    echo 'runner account needs subordinate UID/GID ranges of at least 65535 IDs' >&2
    exit 1
}
[[ "$(stat -c %u "$mapped_rootfs/etc/passwd")" == "$subuid_start" ]] || {
    echo 'rootless import did not map guest root to the configured subordinate UID' >&2
    exit 1
}
[[ "$(stat -c %u "$mapped_rootfs/home/wildbuzzard")" == "$runner_uid" ]] || {
    echo 'rootless import did not keep guest UID 1000 as the desktop host user' >&2
    exit 1
}
if findmnt -rn -o TARGET | awk -v root="$mapped_rootfs" '$0 == root || index($0, root "/") == 1 { found=1 } END { exit found ? 0 : 1 }'; then
    echo 'refusing to inspect an imported rootfs containing active mounts' >&2
    exit 1
fi

"$launcher" --storage-dir "$roundtrip_root" __export-generic-seed imported \
    --output "$archive" \
    --source-date-epoch "$source_date_epoch"
zstd -q -t -- "$archive"
chmod 0644 "$archive"

# Independently unpack the flattened seed and verify every content-addressed
# blob, the one-layer invariant, and the absence of portable machine identity.
seed_check="$build_root/seed-check"
mkdir -p "$seed_check"
zstd -q -dc -- "$archive" | tar --no-same-owner -xf - -C "$seed_check"
cmp "$layout/oci-layout" "$seed_check/oci-layout"
manifest_digest=$(jq -er '
  if .schemaVersion != 2 or (.manifests | length) != 1 then
    error("flattened OCI index must contain exactly one manifest")
  else .manifests[0].digest end
' "$seed_check/index.json")
[[ "$manifest_digest" =~ ^sha256:[0-9a-f]{64}$ ]]
while IFS= read -r blob; do
    digest=${blob##*/}
    [[ "$(sha256sum "$blob" | awk '{print $1}')" == "$digest" ]] || {
        echo "OCI seed contains a blob whose name does not match its digest: $blob" >&2
        exit 1
    }
done < <(find "$seed_check/blobs/sha256" -type f -print | LC_ALL=C sort)
flattened_manifest="$seed_check/blobs/sha256/${manifest_digest#sha256:}"
jq -e \
    '(.schemaVersion == 2) and (.layers | length == 1) and
     (.annotations["org.openresearchtools.buzzardos.machine-config.v1"] == null)' \
    "$flattened_manifest" >/dev/null
flattened_config_digest=$(jq -er '.config.digest' "$flattened_manifest")
[[ "$flattened_config_digest" =~ ^sha256:[0-9a-f]{64}$ ]]
flattened_config="$seed_check/blobs/sha256/${flattened_config_digest#sha256:}"
python3 - "$source_config" "$flattened_config" <<'PY'
import json
import sys
from pathlib import Path

source = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8")).get("config") or {}
flattened = json.loads(Path(sys.argv[2]).read_text(encoding="utf-8")).get("config") or {}
for key in ("Env", "Entrypoint", "Cmd", "WorkingDir", "User", "StopSignal"):
    if key in source and flattened.get(key) != source[key]:
        raise SystemExit(f"flattened OCI seed changed authenticated process field {key}")
source_labels = source.get("Labels")
flattened_labels = flattened.get("Labels")
if source_labels is not None:
    if not isinstance(source_labels, dict) or not isinstance(flattened_labels, dict):
        raise SystemExit("flattened OCI seed changed authenticated process field Labels")
    for key, value in source_labels.items():
        if flattened_labels.get(key) != value:
            raise SystemExit(f"flattened OCI seed changed authenticated label {key}")
PY

archive_sha256=$(sha256sum "$archive" | awk '{print $1}')
archive_size=$(stat -c '%s' "$archive")
jq -n \
    --arg source_commit "$source_commit" \
    --arg archive_sha256 "$archive_sha256" \
    --arg manifest_digest "$manifest_digest" \
    --arg source_manifest_digest "$source_manifest_digest" \
    --argjson archive_size "$archive_size" \
    '{schema:1,kind:"buzzardos-oci-seed",platform:{os:"linux",architecture:"amd64"},archive:{name:"default-rootfs.oci.tar.zst",size:$archive_size,sha256:$archive_sha256},manifest_digest:$manifest_digest,source_manifest_digest:$source_manifest_digest,source_commit:$source_commit}' \
    >"$runtime_dir/default-rootfs.oci.json"

# Exercise the final flattened archive, not only its pre-flattening source.
"$launcher" --storage-dir "$roundtrip_root" delete imported --yes
"$launcher" --storage-dir "$roundtrip_root" import "$archive" --name imported
mapped_rootfs="$roundtrip_root/Machines/imported/rootfs"
[[ "$(stat -c %u "$mapped_rootfs/etc/passwd")" == "$subuid_start" ]] || {
    echo 'flattened seed did not map guest root to the configured subordinate UID' >&2
    exit 1
}
[[ "$(stat -c %u "$mapped_rootfs/home/wildbuzzard")" == "$runner_uid" ]] || {
    echo 'flattened seed did not keep guest UID 1000 as the desktop host user' >&2
    exit 1
}
if findmnt -rn -o TARGET | awk -v root="$mapped_rootfs" '$0 == root || index($0, root "/") == 1 { found=1 } END { exit found ? 0 : 1 }'; then
    echo 'refusing to inspect a flattened rootfs containing active mounts' >&2
    exit 1
fi

# Guest UID 1000 maps to the runner account. Package metadata remains readable
# from the mapped rootfs while every generated evidence file is host-user
# owned. No host-root extraction or sudo cleanup path is needed.
env PATH="$PATH" CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
    RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
    python3 "$project_dir/tools/license_audit.py" \
    --guest-rootfs "$mapped_rootfs" --structural
python3 "$project_dir/tools/release_metadata.py" materialize \
    --source "$mapped_rootfs/usr/share/doc" \
    --destination "$guest_licenses/usr-share-doc"
python3 "$project_dir/tools/release_metadata.py" materialize \
    --source "$mapped_rootfs/usr/share/common-licenses" \
    --destination "$guest_licenses/usr-share-common-licenses"
"$launcher" --storage-dir "$roundtrip_root" delete imported --yes
install -m0644 "$project_dir/tools/release/guest-rootfs-licenses.README.md" \
    "$guest_licenses/README.md"
"$project_dir/tools/create-project-source-archive.sh" "$guest_licenses/project-source"

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
install -m0644 "$runtime_dir/default-rootfs.oci.json" "$guest_provenance/"
(
    cd "$output_dir"
    sha256sum runtime/default-rootfs.oci.tar.zst runtime/default-rootfs.oci.json >ROOTFS_SHA256SUMS
)

printf 'Built verified OCI install seed: %s\n' "$archive"
