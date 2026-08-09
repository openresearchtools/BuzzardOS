#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
manifest=${WILDBUZZARD_GO_SOURCE_MANIFEST:-"$project_dir/LICENSES/go-source-archives.tsv"}
destination=${1:?usage: fetch-go-source-archives.sh DESTINATION}
cache=${WILDBUZZARD_GO_SOURCE_CACHE:-"${TMPDIR:-/tmp}/wildbuzzard-go-source-cache-$(id -u)"}

for command_name in curl find id install mktemp realpath sha256sum tar unzip; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "Go source archive helper is missing '$command_name'" >&2
        exit 1
    }
done

destination=$(realpath -m -- "$destination")
cache=$(realpath -m -- "$cache")
mkdir -p -- "$destination/archives" "$destination/notices" "$cache"
license_index="$destination/LICENSES.tsv"
printf '# id\tarchive\tlicense-expression\n' >"$license_index"

validate_archive_members() {
    local archive=$1
    local listing
    if [[ "$archive" == *.zip ]]; then
        listing=$(unzip -Z1 "$archive")
    else
        listing=$(tar -tzf "$archive")
    fi
    while IFS= read -r member; do
        case "$member" in
            ''|/*|../*|*/../*|*/..)
                echo "unsafe member in checksum-pinned archive '$archive': $member" >&2
                exit 1
                ;;
        esac
    done <<<"$listing"
}

tab=$'\t'
while IFS="$tab" read -r archive_id archive_name url expected_sha256 license_expression; do
    case "$archive_id" in
        ''|'#'*) continue ;;
    esac
    case "$archive_id:$archive_name:$url:$expected_sha256" in
        *[!A-Za-z0-9._@+:/=\!~-]*)
            echo "unsafe Go source archive record: $archive_id" >&2
            exit 1
            ;;
    esac
    [[ "$archive_name" != */* && "$archive_name" != .* ]] || {
        echo "unsafe Go source archive name: $archive_name" >&2
        exit 1
    }
    [[ "$expected_sha256" =~ ^[0-9a-f]{64}$ ]] || {
        echo "invalid Go source archive checksum for $archive_id" >&2
        exit 1
    }
    [[ -n "$license_expression" && "$license_expression" != *$'\t'* ]] || {
        echo "missing or invalid license expression for $archive_id" >&2
        exit 1
    }

    cached="$cache/$archive_name"
    if [[ ! -f "$cached" ]] || ! printf '%s  %s\n' "$expected_sha256" "$cached" | sha256sum --check --status; then
        temporary=$(mktemp "$cache/.${archive_name}.XXXXXX")
        curl --fail --location --retry 3 --output "$temporary" "$url"
        printf '%s  %s\n' "$expected_sha256" "$temporary" | sha256sum --check --status || {
            echo "checksum mismatch for Go source archive $archive_id" >&2
            exit 1
        }
        chmod 0644 "$temporary"
        mv -f -- "$temporary" "$cached"
    fi
    validate_archive_members "$cached"
    install -m 0644 "$cached" "$destination/archives/$archive_name"

    extracted=$(mktemp -d "${TMPDIR:-/tmp}/wildbuzzard-go-source.XXXXXX")
    if [[ "$archive_name" == *.zip ]]; then
        unzip -q "$cached" -d "$extracted"
    else
        tar -xzf "$cached" -C "$extracted"
    fi
    notice_count=0
    while IFS= read -r -d '' notice; do
        relative=${notice#"$extracted/"}
        install -D -m 0644 "$notice" "$destination/notices/$archive_id/$relative"
        notice_count=$((notice_count + 1))
    done < <(find "$extracted" -type f \( \
        -iname 'LICENSE*' -o -iname 'COPYING*' -o \
        -iname 'NOTICE*' -o -iname 'PATENTS*' \
    \) -print0)
    if ((notice_count == 0)); then
        echo "Go source archive $archive_id contains no discoverable license notice" >&2
        exit 1
    fi
    printf '%s\t%s\t%s\n' \
        "$archive_id" "$archive_name" "$license_expression" >>"$license_index"
    find "$extracted" -depth -delete
done <"$manifest"

(
    cd "$destination/archives"
    sha256sum -- *
) >"$destination/SHA256SUMS"
