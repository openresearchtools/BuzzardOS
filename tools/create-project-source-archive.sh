#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: tools/create-project-source-archive.sh OUTPUT_DIRECTORY

Create a deterministic, checksum-addressed archive of the exact clean Git
commit used to build Wild Buzzard. The archive is corresponding-source and
provenance evidence for binary artifacts; generated files and Git history are
not included.
EOF
}

if [[ $# -ne 1 ]]; then
    usage >&2
    exit 2
fi

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
output_dir=$(realpath -m -- "$1")

case "$output_dir/" in
    "$project_dir/"*)
        echo "refusing to place corresponding-source output inside the source repository" >&2
        exit 1
        ;;
esac

for command_name in git python3 realpath sha256sum zstd; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "project source archive dependency missing: $command_name" >&2
        exit 1
    }
done

git -C "$project_dir" rev-parse --is-inside-work-tree >/dev/null
source_commit=$(git -C "$project_dir" rev-parse --verify HEAD^{commit})
source_date_epoch=$(git -C "$project_dir" show -s --format=%ct "$source_commit")
source_repository=$(git -C "$project_dir" config --get remote.origin.url || true)
if [[ -z "$source_repository" ]]; then
    source_repository=https://github.com/openresearchtools/BuzzardOS
fi

dirty=$(git -C "$project_dir" status --porcelain=v1 --untracked-files=all)
if [[ -n "$dirty" ]]; then
    echo "refusing to package binary artifacts from a dirty source tree" >&2
    printf '%s\n' "$dirty" >&2
    exit 1
fi

mkdir -p "$output_dir"
archive_name="BuzzardOS-source-$source_commit.tar.zst"
archive="$output_dir/$archive_name"
provenance="$output_dir/source-provenance.json"
checksums="$output_dir/SHA256SUMS"
for destination in "$archive" "$provenance" "$checksums"; do
    if [[ -e "$destination" || -L "$destination" ]]; then
        echo "refusing to replace existing source evidence: $destination" >&2
        exit 1
    fi
done
temporary=$(mktemp "$output_dir/.${archive_name}.XXXXXX")
cleanup() {
    rm -f -- "$temporary" "$archive" "$provenance" "$checksums"
}
trap cleanup EXIT HUP INT TERM

git -C "$project_dir" archive \
    --format=tar \
    --prefix="BuzzardOS-$source_commit/" \
    "$source_commit" |
    zstd -T0 -19 --long=27 --no-progress --force -o "$temporary"
zstd -t "$temporary"
mv -- "$temporary" "$archive"
chmod 0644 "$archive"

archive_sha256=$(sha256sum -- "$archive" | awk '{print $1}')
archive_size=$(stat -c '%s' -- "$archive")
read -r uncompressed_sha256 uncompressed_size < <(
    zstd -T0 -dc -- "$archive" |
        python3 -c '
import hashlib
import sys
digest = hashlib.sha256()
size = 0
while block := sys.stdin.buffer.read(1024 * 1024):
    digest.update(block)
    size += len(block)
print(digest.hexdigest(), size)
'
)
printf '%s  %s\n' "$archive_sha256" "$archive_name" >"$checksums"

SOURCE_COMMIT=$source_commit \
SOURCE_DATE_EPOCH=$source_date_epoch \
SOURCE_REPOSITORY=$source_repository \
ARCHIVE_NAME=$archive_name \
ARCHIVE_SHA256=$archive_sha256 \
ARCHIVE_SIZE=$archive_size \
UNCOMPRESSED_SHA256=$uncompressed_sha256 \
UNCOMPRESSED_SIZE=$uncompressed_size \
python3 - "$provenance" <<'PY'
import json
import os
import sys
from pathlib import Path

destination = Path(sys.argv[1])
record = {
    "schema": 1,
    "repository": os.environ["SOURCE_REPOSITORY"],
    "commit": os.environ["SOURCE_COMMIT"],
    "source_date_epoch": int(os.environ["SOURCE_DATE_EPOCH"]),
    "archive": {
        "name": os.environ["ARCHIVE_NAME"],
        "sha256": os.environ["ARCHIVE_SHA256"],
        "size": int(os.environ["ARCHIVE_SIZE"]),
        "format": "tar+zstd",
        "uncompressed_sha256": os.environ["UNCOMPRESSED_SHA256"],
        "uncompressed_size": int(os.environ["UNCOMPRESSED_SIZE"]),
    },
    "build_recipes": [
        "host/build-appimage.sh",
        "oci/desktop/Containerfile",
        "tools/build-release-rootfs.sh",
        "tools/assemble-release-assets.sh",
    ],
}
destination.write_text(
    json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8"
)
PY

python3 "$project_dir/tools/release_metadata.py" verify-source \
    --directory "$output_dir" \
    --source-commit "$source_commit"

trap - EXIT HUP INT TERM
printf 'Created %s (%s bytes)\n' "$archive" "$archive_size"
