#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
set -eu

usage='usage: install-rootfs-assets.sh ROOTFS CLIPBOARD_AGENT_BINARY'
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
target_root=${1:?$usage}
clipboard_agent_binary=${2:?$usage}
asset_manifest="$script_dir/runtime-asset-manifest.tsv"
revision=$(tr -d '\n' <"$script_dir/ASSET_REVISION")

test -d "$target_root"
test ! -L "$target_root"
test -x "$clipboard_agent_binary"

case "$revision" in
    ''|*[!A-Za-z0-9._+~-]*|.*|*/*)
        echo "invalid protected runtime revision: $revision" >&2
        exit 1
        ;;
esac
if [ "${#revision}" -gt 128 ]; then
    echo "protected runtime revision is too long" >&2
    exit 1
fi

tab=$(printf '\t')
runtime_root="$target_root/opt/buzzardos/runtime"
revision_dir="$runtime_root/$revision"
for protected_dir in \
    "$target_root/opt" \
    "$target_root/opt/buzzardos" \
    "$runtime_root"; do
    if [ -L "$protected_dir" ] || { [ -e "$protected_dir" ] && [ ! -d "$protected_dir" ]; }; then
        echo "protected runtime parent is not a real directory: $protected_dir" >&2
        exit 1
    fi
    if [ ! -d "$protected_dir" ]; then
        mkdir -- "$protected_dir"
    fi
    chmod 0755 "$protected_dir"
done
if [ "$(id -u)" -eq 0 ]; then
    chown 0:0 "$target_root/opt" "$target_root/opt/buzzardos" "$runtime_root"
fi
stage=$(mktemp -d "$runtime_root/.$revision.staging.XXXXXX")
cleanup_stage() {
    if [ -n "${stage:-}" ] && [ -d "$stage" ] && [ ! -L "$stage" ]; then
        rm -rf -- "$stage"
    fi
}
trap cleanup_stage EXIT HUP INT TERM
chmod 0755 "$stage"

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
    case "$destination" in
        @runtime/*)
            relative=${destination#@runtime/}
            install -D -m "$mode" "$script_dir/$source" "$stage/$relative"
            ;;
        @runtime*)
            echo "invalid protected runtime mapping: $destination" >&2
            exit 1
            ;;
        *)
            install -D -m "$mode" "$script_dir/$source" "$target_root/$destination"
            ;;
    esac
done <"$asset_manifest"

install -D -m 0755 "$clipboard_agent_binary" \
    "$stage/libexec/buzzardos-clipboard-agent"
for required in \
    libexec/buzzardos-clipboard-agent \
    libexec/buzzardos-init \
    libexec/buzzardos-session \
    libexec/buzzardos-sway-session \
    libexec/buzzardos-output-sync \
    libexec/buzzardos-desktop-services \
    libexec/buzzardos-integration-agent; do
    test -f "$stage/$required"
    test ! -L "$stage/$required"
done

if find "$stage" -mindepth 1 -type l -print -quit | grep -q .; then
    echo "protected runtime staging contains a symbolic link" >&2
    exit 1
fi
if find "$stage" -mindepth 1 ! -type d ! -type f -print -quit | grep -q .; then
    echo "protected runtime staging contains a special file" >&2
    exit 1
fi
find "$stage" -type d -exec chmod 0755 {} +
if [ "$(id -u)" -eq 0 ]; then
    chown -R 0:0 "$stage" "$runtime_root"
fi

# Emit the canonical manifest used by the package and startup integrity gates.
# Trusted runtime names are restricted to an ASCII subset, avoiding a second
# JSON escaping implementation in this bootstrap shell.
runtime_manifest="$stage/runtime.manifest.json"
file_list=$(mktemp "$runtime_root/.$revision.files.XXXXXX")
cleanup_files() {
    rm -f -- "$file_list"
    cleanup_stage
}
trap cleanup_files EXIT HUP INT TERM
(
    cd "$stage"
    LC_ALL=C find . -type f ! -name runtime.manifest.json ! -name readiness.json \
        -printf '%P\n' | LC_ALL=C sort
) >"$file_list"
printf '{"files":{' >"$runtime_manifest"
first=1
while IFS= read -r relative; do
    case "$relative" in
        ''|/*|*..*|*[!A-Za-z0-9._+/@~-]*)
            echo "unsafe protected runtime path: $relative" >&2
            exit 1
            ;;
    esac
    file="$stage/$relative"
    test -f "$file"
    test ! -L "$file"
    digest=$(sha256sum "$file" | cut -d' ' -f1)
    octal_mode=$(stat -c '%a' "$file")
    numeric_mode=$(printf '%d' "0$octal_mode")
    if [ "$first" -eq 0 ]; then
        printf ',' >>"$runtime_manifest"
    fi
    first=0
    printf '"%s":{"mode":%s,"sha256":"%s"}' \
        "$relative" "$numeric_mode" "$digest" >>"$runtime_manifest"
done <"$file_list"
printf '},"revision":"%s","schema_version":1}' "$revision" >>"$runtime_manifest"
chmod 0644 "$runtime_manifest"
if [ "$(id -u)" -eq 0 ]; then
    chown 0:0 "$runtime_manifest"
fi

current_target=
if [ -L "$runtime_root/current" ]; then
    current_target=$(readlink "$runtime_root/current")
    case "$current_target" in
        ''|*[!A-Za-z0-9._+~-]*|.*|*/*)
            echo "existing protected runtime current target is invalid" >&2
            exit 1
            ;;
    esac
    test -d "$runtime_root/$current_target"
    test ! -L "$runtime_root/$current_target"
elif [ -e "$runtime_root/current" ]; then
    echo "protected runtime current is not a symbolic link" >&2
    exit 1
fi

if [ -e "$revision_dir" ]; then
    test -d "$revision_dir"
    test ! -L "$revision_dir"
    existing_matches=1
    if [ ! -f "$revision_dir/runtime.manifest.json" ] || \
        [ -L "$revision_dir/runtime.manifest.json" ] || \
        ! cmp -s "$runtime_manifest" "$revision_dir/runtime.manifest.json" || \
        find "$revision_dir" -mindepth 1 -type l -print -quit | grep -q . || \
        find "$revision_dir" -mindepth 1 ! -type d ! -type f -print -quit | grep -q .; then
        existing_matches=0
    fi
    if [ "$existing_matches" -eq 1 ]; then
        existing_file_list=$(mktemp "$runtime_root/.$revision.existing-files.XXXXXX")
        (
            cd "$revision_dir"
            LC_ALL=C find . -type f ! -name runtime.manifest.json ! -name readiness.json \
                -printf '%P\n' | LC_ALL=C sort
        ) >"$existing_file_list"
        if ! cmp -s "$file_list" "$existing_file_list"; then
            existing_matches=0
        else
            while IFS= read -r relative; do
                if ! cmp -s "$stage/$relative" "$revision_dir/$relative" || \
                    [ "$(stat -c '%a' "$stage/$relative")" != \
                      "$(stat -c '%a' "$revision_dir/$relative")" ]; then
                    existing_matches=0
                    break
                fi
            done <"$file_list"
        fi
        rm -f -- "$existing_file_list"
    fi
    if [ "$existing_matches" -eq 0 ]; then
        if [ "$current_target" = "$revision" ]; then
            echo "active protected runtime revision is incomplete or differs; bump ASSET_REVISION" >&2
            exit 1
        fi
        incomplete="$runtime_root/.$revision.incomplete.$$"
        test ! -e "$incomplete"
        test ! -L "$incomplete"
        mv -- "$revision_dir" "$incomplete"
        mv -- "$stage" "$revision_dir"
        stage=
        rm -rf -- "$incomplete"
    else
        # A completed retry reuses its existing readiness record.  The matching
        # canonical manifest proves this revision has the same managed bytes.
        rm -rf -- "$stage"
        stage=
    fi
else
    mv -- "$stage" "$revision_dir"
    stage=
fi

if [ "$current_target" != "$revision" ]; then
    if [ -n "$current_target" ]; then
        previous_tmp="$runtime_root/.previous.$$"
        rm -f -- "$previous_tmp"
        ln -s "$current_target" "$previous_tmp"
        mv -Tf -- "$previous_tmp" "$runtime_root/previous"
    fi
    current_tmp="$runtime_root/.current.$$"
    rm -f -- "$current_tmp"
    ln -s "$revision" "$current_tmp"
    mv -Tf -- "$current_tmp" "$runtime_root/current"
fi

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

# This manifest covers non-versioned managed integration.  The independently
# canonical runtime.manifest.json owns every private runtime byte.
install -d -m 0755 "$target_root/usr/lib/buzzardos"
json="$target_root/usr/lib/buzzardos/guest-assets.manifest.json"
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
    case "$destination" in
        @runtime/*) continue ;;
    esac
    emit_record "$destination" "$mode" "$target_root/$destination"
done <"$asset_manifest"
printf '\n  }\n}\n' >>"$tmp"
chmod 0644 "$tmp"
mv -f -- "$tmp" "$json"
install -m 0644 "$script_dir/ASSET_REVISION" \
    "$target_root/usr/lib/buzzardos/guest-assets.version"

rm -f -- "$file_list"
trap - EXIT HUP INT TERM
