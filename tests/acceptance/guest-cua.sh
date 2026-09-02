#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -Eeuo pipefail

if (($# < 3 || $# > 4)); then
    echo "usage: $0 MACHINE-DIR MACHINE TOOL [JSON-ARGUMENTS]" >&2
    exit 2
fi

machine_dir=$(readlink -f -- "$1")
machine=$2
tool=$3
arguments=${4:-\{\}}

[[ -d "$machine_dir" ]] || {
    echo "machine directory does not exist: $machine_dir" >&2
    exit 2
}
[[ "$machine" =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$ ]] || {
    echo "invalid machine name: $machine" >&2
    exit 2
}

machine_id=$(jq -er '.id | gsub("-"; "")' "$machine_dir/machine.json")
container="buzzardos-$machine_id"
[[ $(podman container inspect --format '{{.State.Running}}' "$container") == true ]] || {
    echo "machine is not running: $machine" >&2
    exit 1
}

podman exec --user user \
    --env HOME=/home/user \
    --env USER=user \
    --env LOGNAME=user \
    --env XDG_RUNTIME_DIR=/run/user/1000 \
    --env XDG_CONFIG_HOME=/home/user/.config \
    --env XDG_DATA_HOME=/home/user/.local/share \
    --env XDG_CACHE_HOME=/home/user/.cache \
    --env XDG_CONFIG_DIRS=/etc/buzzardos/xdg:/etc/xdg \
    --env XDG_DATA_DIRS=/usr/local/share:/usr/share \
    --env XDG_SESSION_TYPE=wayland \
    --env XDG_CURRENT_DESKTOP=sway \
    --env XDG_SESSION_DESKTOP=sway \
    --env DISPLAY=:0 \
    --env DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
    --env 'QT_QPA_PLATFORM=wayland;xcb' \
    --env QT_QPA_PLATFORMTHEME=gtk3 \
    --env QT_ACCESSIBILITY=1 \
    --env GTK_MODULES=gail:atk-bridge \
    --env NO_AT_BRIDGE=0 \
    "$container" sh -lc '
        attempt=0
        while [ "$attempt" -lt 150 ]; do
            candidate=$(pgrep -xo buzzardos-deskt 2>/dev/null || true)
            if [ -n "$candidate" ] && [ -r "/proc/$candidate/environ" ]; then
                WAYLAND_DISPLAY=$(tr "\0" "\n" <"/proc/$candidate/environ" | sed -n "s/^WAYLAND_DISPLAY=//p" | head -1)
                SWAYSOCK=$(tr "\0" "\n" <"/proc/$candidate/environ" | sed -n "s/^SWAYSOCK=//p" | head -1)
                if [ -n "$WAYLAND_DISPLAY" ] && [ -n "$SWAYSOCK" ]; then
                    export WAYLAND_DISPLAY SWAYSOCK
                    exec "$@"
                fi
            fi
            attempt=$((attempt + 1))
            sleep 0.1
        done
        echo "private Sway/CUA endpoints are unavailable" >&2
        exit 1
    ' sh /usr/bin/cua "$tool" "$arguments"
