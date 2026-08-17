#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

if (($# < 3 || $# > 4)); then
    echo "usage: $0 MACHINE-DIR MACHINE TOOL [JSON-ARGUMENTS]" >&2
    exit 2
fi

machine_input=$1
machine=$2
tool=$3
arguments=${4:-\{\}}

if [[ ! -d "$machine_input" ]]; then
    echo "machine directory does not exist: $machine_input" >&2
    exit 2
fi
machine_dir=$(readlink -f -- "$machine_input")

if [[ ! "$machine" =~ ^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$ ]]; then
    echo "invalid machine name: $machine" >&2
    exit 2
fi

runtime=$machine_dir/runtime.json
container_pid=$(jq -er '
    select(.state == "running") |
    .container_pid |
    select(type == "number" and . > 1)
' "$runtime")

nsenter -t "$container_pid" -U -n -p -m -u -i -- \
    setpriv --reuid=0 --regid=0 --clear-groups \
    setpriv --reuid=1000 --regid=1000 --clear-groups \
    env -i \
    HOME=/home/buzzard \
    USER=buzzard \
    LOGNAME=buzzard \
    PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    XDG_RUNTIME_DIR=/run/user/1000 \
    XDG_CONFIG_HOME=/home/buzzard/.config \
    XDG_DATA_HOME=/home/buzzard/.local/share \
    XDG_CACHE_HOME=/home/buzzard/.cache \
    XDG_CONFIG_DIRS=/etc/buzzardos/xdg:/etc/xdg \
    XDG_DATA_DIRS=/usr/local/share:/usr/share \
    XDG_SESSION_TYPE=wayland \
    XDG_CURRENT_DESKTOP=sway \
    XDG_SESSION_DESKTOP=sway \
    DISPLAY=:0 \
    DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
    LD_LIBRARY_PATH=/run/buzzardos-host/driver/lib \
    "QT_QPA_PLATFORM=wayland;xcb" \
    QT_QPA_PLATFORMTHEME=gtk3 \
    QT_ACCESSIBILITY=1 \
    GTK_MODULES=gail:atk-bridge \
    NO_AT_BRIDGE=0 \
    CUA_DRIVER_RS_ENABLE_WAYLAND=1 \
    sh -lc '
        session_pid=
        WAYLAND_DISPLAY=
        SWAYSOCK=
        shell_observed=0
        attempt=0
        while [ "$attempt" -lt 150 ]; do
            candidate=$(pgrep -xo buzzardos-deskt 2>/dev/null || true)
            if [ -n "$candidate" ] && [ -r "/proc/$candidate/environ" ]; then
                shell_observed=1
                wayland_display=$(
                    tr "\0" "\n" <"/proc/$candidate/environ" |
                        sed -n "s/^WAYLAND_DISPLAY=//p" |
                        head -1
                )
                sway_socket=$(
                    tr "\0" "\n" <"/proc/$candidate/environ" |
                        sed -n "s/^SWAYSOCK=//p" |
                        head -1
                )
                if [ -n "$wayland_display" ] && [ -n "$sway_socket" ]; then
                    session_pid=$candidate
                    WAYLAND_DISPLAY=$wayland_display
                    SWAYSOCK=$sway_socket
                    break
                fi
            fi
            attempt=$((attempt + 1))
            sleep 0.1
        done
        if [ "$shell_observed" -eq 0 ]; then
            echo "Buzzard OS shell session is unavailable" >&2
            exit 1
        fi
        if [ -z "$session_pid" ]; then
            echo "private Sway application endpoints are unavailable" >&2
            exit 1
        fi
        export WAYLAND_DISPLAY SWAYSOCK
        exec "$@"
    ' sh /usr/bin/buzzardoscua "$tool" "$arguments"
