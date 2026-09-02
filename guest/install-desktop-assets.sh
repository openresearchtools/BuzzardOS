#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
set -eu

usage='usage: install-desktop-assets.sh ROOTFS DESKTOP_BINARY SETTINGS_BINARY SHORTCUT_HELPER_BINARY'
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
target_root=${1:?$usage}
desktop_binary=${2:?$usage}
settings_binary=${3:?$usage}
shortcut_helper_binary=${4:?$usage}
manifest="$script_dir/desktop-asset-manifest.tsv"

test -d "$target_root"
test ! -L "$target_root"
test -x "$desktop_binary"
test -x "$settings_binary"
test -x "$shortcut_helper_binary"

tab=$(printf '\t')
while IFS="$tab" read -r mode source destination; do
    case "$mode" in
        ''|'#'*) continue ;;
    esac
    case "$source:$destination" in
        /*:*|*:/|*..*:*|*:*..*|*:@runtime*)
            echo "unsafe desktop asset mapping: $source -> $destination" >&2
            exit 1
            ;;
    esac
    test -f "$script_dir/$source"
    test ! -L "$script_dir/$source"
    install -D -m "$mode" "$script_dir/$source" "$target_root/$destination"
done <"$manifest"

install -D -m 0755 "$desktop_binary" "$target_root/usr/bin/buzzardos-desktop"
install -D -m 0755 "$settings_binary" "$target_root/usr/bin/buzzardos-settings"
install -D -m 0755 "$shortcut_helper_binary" \
    "$target_root/usr/libexec/buzzardos-desktop/buzzardos-shortcut-helper"

install -d -m 0755 "$target_root/usr/lib/buzzardos-desktop"
json="$target_root/usr/lib/buzzardos-desktop/assets.manifest.json"
tmp="$json.tmp"
printf '{\n  "schema": 1,\n  "assets": {\n' >"$tmp"
first=1
emit_record() {
    relative=$1
    mode=$2
    file=$3
    digest=$(sha256sum "$file" | cut -d' ' -f1)
    numeric_mode=$(printf '%d' "0$mode")
    if [ "$first" -eq 0 ]; then
        printf ',\n' >>"$tmp"
    fi
    first=0
    printf '    "%s": {"sha256": "%s", "mode": %s}' \
        "$relative" "$digest" "$numeric_mode" >>"$tmp"
}
while IFS="$tab" read -r mode source destination; do
    case "$mode" in
        ''|'#'*) continue ;;
    esac
    emit_record "$destination" "$mode" "$target_root/$destination"
done <"$manifest"
emit_record usr/bin/buzzardos-desktop 0755 "$target_root/usr/bin/buzzardos-desktop"
emit_record usr/bin/buzzardos-settings 0755 "$target_root/usr/bin/buzzardos-settings"
emit_record usr/libexec/buzzardos-desktop/buzzardos-shortcut-helper 0755 \
    "$target_root/usr/libexec/buzzardos-desktop/buzzardos-shortcut-helper"
printf '\n  }\n}\n' >>"$tmp"
chmod 0644 "$tmp"
mv -f -- "$tmp" "$json"
