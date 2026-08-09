#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -Eeuo pipefail

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
manifest=${WILDBUZZARD_SLIRP_SOURCE_MANIFEST:-"$project_dir/LICENSES/slirp4netns-sources.tsv"}
destination=${1:?usage: fetch-slirp4netns-sources.sh DESTINATION}
cache=${WILDBUZZARD_SLIRP_SOURCE_CACHE:-"${TMPDIR:-/tmp}/wildbuzzard-slirp-source-cache-$(id -u)"}

for command_name in curl install mkdir mktemp mv realpath sha256sum tar; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "slirp4netns source helper is missing '$command_name'" >&2
        exit 1
    }
done
[[ -f "$manifest" ]] || {
    echo "slirp4netns source manifest is missing: $manifest" >&2
    exit 1
}

destination=$(realpath -m -- "$destination")
cache=$(realpath -m -- "$cache")
mkdir -p -- "$destination" "$cache"
checksums="$destination/SHA256SUMS"
: >"$checksums"

tab=$'\t'
while IFS="$tab" read -r archive_name url expected_sha256; do
    case "$archive_name" in
        ''|'#'*) continue ;;
    esac
    [[ "$archive_name" =~ ^slirp4netns_[A-Za-z0-9.+-]+\.(dsc|tar\.gz|tar\.xz)$ ]] || {
        echo "unsafe slirp4netns source archive name: $archive_name" >&2
        exit 1
    }
    [[ "$url" == https://archive.ubuntu.com/ubuntu/pool/universe/s/slirp4netns/* ]] || {
        echo "unexpected slirp4netns source URL: $url" >&2
        exit 1
    }
    [[ "$expected_sha256" =~ ^[0-9a-f]{64}$ ]] || {
        echo "invalid slirp4netns source checksum for $archive_name" >&2
        exit 1
    }

    cached="$cache/$archive_name"
    if [[ ! -f "$cached" ]] ||
        ! printf '%s  %s\n' "$expected_sha256" "$cached" | sha256sum --check --status; then
        temporary=$(mktemp "$cache/.${archive_name}.XXXXXX")
        curl --fail --location --retry 3 --output "$temporary" "$url"
        printf '%s  %s\n' "$expected_sha256" "$temporary" |
            sha256sum --check --status || {
                echo "checksum mismatch for slirp4netns source $archive_name" >&2
                exit 1
            }
        chmod 0644 "$temporary"
        mv -f -- "$temporary" "$cached"
    fi

    case "$archive_name" in
        *.tar.gz) listing=$(tar -tzf "$cached") ;;
        *.tar.xz) listing=$(tar -tJf "$cached") ;;
        *.dsc) listing= ;;
    esac
    while IFS= read -r member; do
        case "$member" in
            '') continue ;;
            /*|../*|*/../*|*/..)
                echo "unsafe member in $archive_name: $member" >&2
                exit 1
                ;;
        esac
    done <<<"$listing"

    install -m 0644 "$cached" "$destination/$archive_name"
    printf '%s  %s\n' "$expected_sha256" "$archive_name" >>"$checksums"
done <"$manifest"

LC_ALL=C sort -k2,2 "$checksums" -o "$checksums"
