#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -Eeuo pipefail

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
manifest=${WILDBUZZARD_MPL_SOURCE_MANIFEST:-"$project_dir/LICENSES/mpl-sources.tsv"}
destination=${1:?usage: fetch-mpl-sources.sh DESTINATION}

for command_name in curl install mkdir mktemp mv sha256sum; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "MPL source fetch dependency is missing: $command_name" >&2
        exit 1
    }
done
[[ -f "$manifest" ]] || {
    echo "MPL source manifest is missing: $manifest" >&2
    exit 1
}

mkdir -p -- "$destination"
temporary=$(mktemp "$destination/.mpl-source.XXXXXX")
cleanup() {
    rm -f -- "$temporary"
}
trap cleanup EXIT

tab=$'\t'
while IFS="$tab" read -r name version expected url; do
    case "$name" in
        ''|'#'*) continue ;;
    esac
    [[ "$name" =~ ^[A-Za-z0-9_-]+$ && "$version" =~ ^[A-Za-z0-9.+-]+$ ]] || {
        echo "invalid MPL source record: $name $version" >&2
        exit 1
    }
    [[ "$expected" =~ ^[0-9a-f]{64}$ && "$url" == https://static.crates.io/crates/* ]] || {
        echo "invalid MPL source pin: $name $version" >&2
        exit 1
    }
    target="$destination/$name-$version.crate"
    if [[ -f "$target" ]] &&
        printf '%s  %s\n' "$expected" "$target" | sha256sum --check --status; then
        continue
    fi
    curl --fail --location --retry 3 --output "$temporary" "$url"
    printf '%s  %s\n' "$expected" "$temporary" | sha256sum --check --status || {
        echo "MPL source checksum mismatch: $name $version" >&2
        exit 1
    }
    install -m 0644 "$temporary" "$target.new"
    mv -f -- "$target.new" "$target"
done <"$manifest"
