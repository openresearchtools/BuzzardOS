#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
target_root=${1:?usage: install-rootfs-assets.sh ROOTFS SHELL_BINARY CUA_BINARY}
shell_binary=${2:?usage: install-rootfs-assets.sh ROOTFS SHELL_BINARY CUA_BINARY}
cua_binary=${3:?usage: install-rootfs-assets.sh ROOTFS SHELL_BINARY CUA_BINARY}
manifest="$script_dir/asset-manifest.tsv"

test -d "$target_root"
test ! -L "$target_root"
test -x "$shell_binary"
test -x "$cua_binary"

tab=$(printf '\t')
while IFS="$tab" read -r mode source destination; do
    case "$mode" in
        ''|'#'*) continue ;;
    esac
    case "$source:$destination" in
        /*:*|*:/|*..*:*|*:*..*)
            echo "unsafe guest asset mapping: $source -> $destination" >&2
            exit 1
            ;;
    esac
    test -f "$script_dir/$source"
    test ! -L "$script_dir/$source"
    install -D -m "$mode" "$script_dir/$source" "$target_root/$destination"
done <"$manifest"

install -D -m 0755 "$shell_binary" "$target_root/usr/libexec/wildbuzzard-shell"
install -D -m 0755 "$cua_binary" "$target_root/usr/local/bin/cua-driver"

# KDE libraries remain available, but their wallet/portal auto-activation must
# never turn into a surprise password prompt in the reference desktop.
for retired in \
    usr/share/applications/org.kde.ksecretd.desktop \
    usr/share/dbus-1/services/org.freedesktop.impl.portal.desktop.kwallet.service \
    usr/share/dbus-1/services/org.kde.kwalletd5.service \
    usr/share/dbus-1/services/org.kde.kwalletd6.service \
    usr/share/dbus-1/services/org.kde.secretservicecompat.service \
    usr/share/xdg-desktop-portal/portals/kwallet.portal; do
    rm -f -- "$target_root/$retired"
done

install -d -m 0755 "$target_root/usr/lib/wildbuzzard"
json="$target_root/usr/lib/wildbuzzard/guest-assets.manifest.json"
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
emit_record usr/libexec/wildbuzzard-shell 0755 \
    "$target_root/usr/libexec/wildbuzzard-shell"
emit_record usr/local/bin/cua-driver 0755 \
    "$target_root/usr/local/bin/cua-driver"
printf '\n  }\n}\n' >>"$tmp"
chmod 0644 "$tmp"
mv -f -- "$tmp" "$json"
install -m 0644 "$script_dir/ASSET_REVISION" \
    "$target_root/usr/lib/wildbuzzard/guest-assets.version"
