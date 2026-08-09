#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail
trap 'rc=$?; echo "hardware acceptance failed at line $LINENO: $BASH_COMMAND" >&2; exit "$rc"' ERR

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
task_uid=$(id -u)
default_appimage="${TMPDIR:-/tmp}/wildbuzzard-build-$task_uid/out/WildBuzzard-x86_64.AppImage"
appimage=${1:-${WILDBUZZARD_APPIMAGE:-$default_appimage}}
machine=${2:-acceptance}
install_package=${WILDBUZZARD_ACCEPT_INSTALL_PACKAGE:-0}
full_matrix=${WILDBUZZARD_ACCEPT_FULL_MATRIX:-0}
integration_acceptance=${WILDBUZZARD_ACCEPT_INTEGRATIONS:-1}
accept_image=${WILDBUZZARD_ACCEPT_IMAGE:-}
relocation_active=0
relocation_original=
relocation_target=
electron_acceptance_host_path=
electron_acceptance_log_path=
electron_acceptance_guest_session=

restore_interrupted_relocation() {
    if [[ -n "${electron_acceptance_guest_session:-}" ]] &&
        [[ "${container_pid:-}" =~ ^[1-9][0-9]*$ ]] &&
        [[ -e "/proc/$container_pid/ns/pid" ]]; then
        guest pkill -TERM -s "$electron_acceptance_guest_session" \
            >/dev/null 2>&1 || true
    fi
    if [[ "$relocation_active" == 1 ]] &&
        [[ -n "$relocation_original" ]] &&
        [[ -n "$relocation_target" ]] &&
        [[ -d "$relocation_target" ]] &&
        [[ ! -e "$relocation_original" ]]; then
        if [[ -x "$relocation_target/$(basename -- "$appimage")" ]]; then
            APPIMAGE_EXTRACT_AND_RUN=1 \
                "$relocation_target/$(basename -- "$appimage")" \
                stop "$machine" >/dev/null 2>&1 || true
        fi
        mv -- "$relocation_target" "$relocation_original"
    fi
    if [[ -n "${electron_acceptance_host_path:-}" ]]; then
        rm -f -- "$electron_acceptance_host_path"
    fi
    if [[ -n "${electron_acceptance_log_path:-}" ]]; then
        rm -f -- "$electron_acceptance_log_path"
    fi
}
trap restore_interrupted_relocation EXIT

for command_name in awk jq nsenter python3 readlink; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "hardware acceptance dependency missing: $command_name" >&2
        exit 1
    }
done
[[ -x "$appimage" ]] || {
    echo "AppImage is missing or not executable: $appimage" >&2
    exit 1
}

portable_dir=$(CDPATH= cd -- "$(dirname -- "$appimage")" && pwd)
appimage="$portable_dir/$(basename -- "$appimage")"
runtime="$portable_dir/vm/$machine/runtime.json"
marker="wildbuzzard-acceptance-$(date +%s)-$$"

wb() {
    APPIMAGE_EXTRACT_AND_RUN=1 "$appimage" "$@"
}

wb_without_host_path() {
    env PATH=/definitely-not-a-host-helper-path \
        APPIMAGE_EXTRACT_AND_RUN=1 \
        "$appimage" "$@"
}

wait_running() {
    local deadline=$((SECONDS + 120))
    while ((SECONDS < deadline)); do
        if [[ -f "$runtime" ]] &&
            [[ $(jq -r '.state // empty' "$runtime") == running ]] &&
            [[ $(jq -r '.display.window.toplevels // 0' "$runtime") == 1 ]]; then
            return
        fi
        sleep 1
    done
    echo "machine did not reach one-window readiness" >&2
    exit 1
}

wait_stopped() {
    local deadline=$((SECONDS + 30))
    while ((SECONDS < deadline)); do
        if [[ $(jq -r '.state // empty' "$runtime" 2>/dev/null || true) == stopped ]]; then
            return
        fi
        sleep 1
    done
    echo "machine did not complete orderly window-close shutdown" >&2
    exit 1
}

appdir_for_process() {
    local pid=$1
    python3 - "$pid" <<'PY'
from pathlib import Path
import sys

values = [
    entry.removeprefix(b"APPDIR=")
    for entry in Path(f"/proc/{sys.argv[1]}/environ").read_bytes().split(b"\0")
    if entry.startswith(b"APPDIR=")
]
if len(values) != 1 or not values[0]:
    raise SystemExit(f"process {sys.argv[1]} does not expose exactly one APPDIR")
print(values[0].decode("utf-8"))
PY
}

process_start_time() {
    local pid=$1
    python3 - "$pid" <<'PY'
from pathlib import Path
import sys

payload = Path(f"/proc/{sys.argv[1]}/stat").read_text(encoding="ascii")
_prefix, separator, suffix = payload.rpartition(")")
if not separator:
    raise SystemExit(f"invalid process stat record for {sys.argv[1]}")
fields = suffix.lstrip().split()
if len(fields) < 20:
    raise SystemExit(f"short process stat record for {sys.argv[1]}")
print(fields[19])
PY
}

wait_process_identity_gone() {
    local pid=$1
    local start_time=$2
    local deadline=$((SECONDS + 30))
    local current_start_time=
    while ((SECONDS < deadline)); do
        if [[ -r "/proc/$pid/stat" ]]; then
            current_start_time=$(process_start_time "$pid" 2>/dev/null || true)
        else
            current_start_time=
        fi
        if [[ "$current_start_time" != "$start_time" ]]; then
            return
        fi
        sleep 0.1
    done
    echo "process identity remained live: pid=$pid start_time=$start_time" >&2
    exit 1
}

wait_appdir_lease_released() {
    local pid=$1
    local start_time=$2
    local appdir=$3
    local deadline=$((SECONDS + 30))
    local current_start_time=
    while ((SECONDS < deadline)); do
        if [[ -r "/proc/$pid/stat" ]]; then
            current_start_time=$(process_start_time "$pid" 2>/dev/null || true)
        else
            current_start_time=
        fi
        if [[ "$current_start_time" != "$start_time" && ! -e "$appdir" ]]; then
            return
        fi
        sleep 0.1
    done
    echo "AppImage broker lease was not released: pid=$pid appdir=$appdir" >&2
    exit 1
}

wait_maximized() {
    local expected=$1
    local deadline=$((SECONDS + 15))
    while ((SECONDS < deadline)); do
        if [[ $(jq -r '.display.window.maximized // false' "$runtime") == "$expected" ]]; then
            return
        fi
        sleep 0.25
    done
    echo "host window did not reach maximized=$expected" >&2
    exit 1
}

wait_native_window_frame() {
    local deadline=$((SECONDS + 30))
    while ((SECONDS < deadline)); do
        if jq -e '
            .display.presentation.native_resolution == true and
            .display.presentation.scale_120 >= 120 and
            .display.presentation.width ==
                ((.display.window.width * .display.presentation.scale_120 + 119) / 120 | floor) and
            .display.presentation.height ==
                ((.display.window.height * .display.presentation.scale_120 + 119) / 120 | floor) and
            .display.presentation.viewport_width == .display.window.width and
            .display.presentation.viewport_height == .display.window.height
        ' "$runtime" >/dev/null; then
            return
        fi
        sleep 0.25
    done
    echo "guest output did not catch up to the host window at native resolution" >&2
    exit 1
}

wait_scaled_window_frame() {
    local scale_120=$1
    local deadline=$((SECONDS + 20))
    while ((SECONDS < deadline)); do
        if jq -e --argjson scale "$scale_120" '
            .display.presentation.scale_120 == $scale and
            .display.presentation.width ==
                ((.display.window.width * $scale + 119) / 120 | floor) and
            .display.presentation.height ==
                ((.display.window.height * $scale + 119) / 120 | floor) and
            .display.presentation.viewport_width == .display.window.width and
            .display.presentation.viewport_height == .display.window.height and
            .display.presentation.native_resolution == true
        ' "$runtime" >/dev/null; then
            return
        fi
        sleep 0.1
    done
    echo "guest output did not reach native ${scale_120}/120 fractional scale" >&2
    exit 1
}

wait_sway_output_matches_runtime() {
    local required_scale_120=${1:-}
    local deadline=$((SECONDS + 15))
    local expected outputs
    while ((SECONDS < deadline)); do
        # The guest scale can be configured independently of the host surface
        # scale. Read the display gateway's atomic guest-output record rather
        # than inferring guest logical dimensions from host-window diagnostics.
        expected=$(guest jq -c '{
            scale_120,
            logical_width: .guest_logical_width,
            logical_height: .guest_logical_height,
            physical_width,
            physical_height
        }' /run/wildbuzzard-display-state/output-state.json)
        if [[ -n "$required_scale_120" ]] &&
            [[ $(jq -r '.scale_120' <<<"$expected") != "$required_scale_120" ]]; then
            sleep 0.1
            continue
        fi
        outputs=$(guest wlr-randr --json)
        if jq -e --argjson expected "$expected" '
            any(.[];
                .enabled == true and
                # wlr-output-management represents scale as wl_fixed (24.8),
                # so recurring fractions such as 4/3 are rounded to 1/256.
                (((.scale * 120) - $expected.scale_120) | fabs) < 0.2 and
                any(.modes[];
                    .current == true and
                    .width == $expected.physical_width and
                    .height == $expected.physical_height) and
                (((($expected.physical_width / .scale) - $expected.logical_width) | fabs) < 2.1) and
                (((($expected.physical_height / .scale) - $expected.logical_height) | fabs) < 2.1))
        ' <<<"$outputs" >/dev/null; then
            return
        fi
        sleep 0.1
    done
    echo "Sway output did not converge on the host window's native scale and resolution" >&2
    exit 1
}

wait_cua_capture_matches_runtime() {
    local deadline=$((SECONDS + 15))
    local expected capture
    while ((SECONDS < deadline)); do
        expected=$(jq -c '{
            width: .display.presentation.width,
            height: .display.presentation.height
        }' "$runtime")
        capture=$(guest cua-driver get_desktop_state '{}')
        if jq -e --argjson expected "$expected" '
            .screenshot_mime_type == "image/png" and
            .screenshot_width == $expected.width and
            .screenshot_height == $expected.height and
            (.screenshot_png_b64 | length) > 0
        ' <<<"$capture" >/dev/null; then
            return
        fi
        sleep 0.1
    done
    echo "Cua full-output capture did not converge on the native physical guest output size" >&2
    exit 1
}

refresh_pid() {
    container_pid=$(jq -er '.container_pid' "$runtime")
}

guest() {
    nsenter -t "$container_pid" -U -n -p -m -u -i -- \
        setpriv --reuid=0 --regid=0 --clear-groups \
        setpriv --reuid=1000 --regid=1000 --clear-groups \
        env -i \
        HOME=/home/wildbuzzard \
        USER=wildbuzzard \
        LOGNAME=wildbuzzard \
        PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
        XDG_RUNTIME_DIR=/run/user/1000 \
        XDG_CONFIG_HOME=/home/wildbuzzard/.config \
        XDG_DATA_HOME=/home/wildbuzzard/.local/share \
        XDG_CACHE_HOME=/home/wildbuzzard/.cache \
        XDG_CONFIG_DIRS=/etc/wildbuzzard/xdg:/etc/xdg \
        XDG_DATA_DIRS=/usr/local/share:/usr/share \
        XDG_SESSION_TYPE=wayland \
        XDG_CURRENT_DESKTOP=sway \
        XDG_SESSION_DESKTOP=sway \
        DISPLAY=:0 \
        DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
        LD_LIBRARY_PATH=/run/wildbuzzard-host/driver/lib \
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
                candidate=$(pgrep -xo wildbuzzard-she 2>/dev/null || true)
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
                echo "Wild Buzzard shell session is unavailable" >&2
                exit 1
            fi
            if [ -z "$session_pid" ]; then
                echo "private Sway application endpoints are unavailable" >&2
                exit 1
            fi
            export WAYLAND_DISPLAY SWAYSOCK
            exec "$@"
        ' sh "$@"
}

guest_spawn() {
    guest sh -c \
        'exec setsid --fork "$@" </dev/null >/tmp/wildbuzzard-acceptance-app.log 2>&1' \
        sh "$@"
}

guest_spawn_logged() {
    local log=$1
    shift
    guest sh -c \
        'log=$1; shift; exec setsid --fork "$@" </dev/null >"$log" 2>&1' \
        sh "$log" "$@"
}

guest_session_for_pid() {
    local pid=$1
    guest python3 - "$pid" <<'PY'
import pathlib
import sys

payload = pathlib.Path(f"/proc/{int(sys.argv[1])}/stat").read_text(encoding="ascii")
_prefix, separator, suffix = payload.rpartition(")")
if not separator:
    raise SystemExit("invalid process stat record")
fields = suffix.lstrip().split()
if len(fields) < 4:
    raise SystemExit("short process stat record")
print(fields[3])
PY
}

assert_appimage_session_unprivileged() {
    local session=$1
    guest python3 - "$session" <<'PY'
import pathlib
import sys

session = int(sys.argv[1])
members = []
for process in pathlib.Path("/proc").iterdir():
    if not process.name.isdigit():
        continue
    try:
        stat_payload = (process / "stat").read_text(encoding="ascii")
        _prefix, separator, suffix = stat_payload.rpartition(")")
        if not separator:
            continue
        stat_fields = suffix.lstrip().split()
        if len(stat_fields) < 4 or int(stat_fields[3]) != session:
            continue
        status = {}
        for line in (process / "status").read_text(encoding="ascii").splitlines():
            name, separator, value = line.partition(":")
            if separator:
                status[name] = value.split()
    except (FileNotFoundError, PermissionError, ProcessLookupError, ValueError):
        continue
    if status.get("Uid") != ["1000"] * 4:
        raise SystemExit(f"AppImage session process {process.name} has unexpected UIDs")
    if status.get("Gid") != ["1000"] * 4:
        raise SystemExit(f"AppImage session process {process.name} has unexpected GIDs")
    if status.get("NoNewPrivs") != ["1"]:
        raise SystemExit(f"AppImage session process {process.name} lost no_new_privs")
    for field in ("CapInh", "CapPrm", "CapEff", "CapAmb"):
        values = status.get(field)
        if len(values or ()) != 1:
            raise SystemExit(f"AppImage session process {process.name} lacks {field}")
        if int(values[0], 16) & (1 << 21):
            raise SystemExit(
                f"AppImage session process {process.name} holds CAP_SYS_ADMIN in {field}"
            )
    members.append(int(process.name))
if not members:
    raise SystemExit("AppImage session has no live processes")
PY
}

appimage_fuse_mount_for_pid() {
    local pid=$1
    local executable mount_record mountpoint filesystem options
    executable=$(guest readlink "/proc/$pid/exe")
    mount_record=$(guest findmnt -rn -o TARGET,FSTYPE,OPTIONS -T "$executable")
    read -r mountpoint filesystem options <<<"$mount_record"
    case "$mountpoint" in
        /tmp/.mount_* | /run/user/1000/.mount_* | \
            /home/wildbuzzard/.mount_* | /shared/.mount_*) ;;
        *)
            echo "AppImage executable is outside an approved runtime mount: $mount_record" >&2
            return 1
            ;;
    esac
    [[ "$executable" == "$mountpoint/"* ]]
    [[ "$filesystem" == fuse || "$filesystem" == fuse.squashfuse ]]
    case ",$options," in *,ro,*) ;; *) return 1 ;; esac
    case ",$options," in *,nosuid,*) ;; *) return 1 ;; esac
    case ",$options," in *,nodev,*) ;; *) return 1 ;; esac
    printf '%s\n' "$mountpoint"
}

wait_appimage_session_cleanup() {
    local pid=$1
    local session=$2
    local mountpoint=$3
    local deadline=$((SECONDS + 20))
    guest kill -TERM "$pid" >/dev/null 2>&1 || true
    while ((SECONDS < deadline)); do
        if ! guest pgrep -s "$session" >/dev/null 2>&1 &&
            ! guest findmnt -rn -M "$mountpoint" >/dev/null 2>&1; then
            return
        fi
        sleep 0.1
    done
    guest pkill -TERM -s "$session" >/dev/null 2>&1 || true
    sleep 1
    guest pkill -KILL -s "$session" >/dev/null 2>&1 || true
    echo "AppImage session or FUSE mount did not stop cleanly: session=$session mount=$mountpoint" >&2
    return 1
}

assert_cua_ok() {
    local tool=$1
    local arguments=$2
    local result
    if ! result=$(guest cua-driver "$tool" "$arguments"); then
        echo "Cua tool $tool failed: $result" >&2
        exit 1
    fi
    jq -e . <<<"$result" >/dev/null 2>&1 || {
        echo "Cua tool $tool returned invalid JSON: $result" >&2
        exit 1
    }
    jq -e 'has("code") | not' <<<"$result" >/dev/null || {
        echo "Cua tool $tool failed: $result" >&2
        exit 1
    }
}

assert_cua_confirmed() {
    local tool=$1
    local arguments=$2
    local result
    if ! result=$(guest cua-driver "$tool" "$arguments"); then
        echo "Cua tool $tool failed: $result" >&2
        exit 1
    fi
    jq -e '
        (has("code") | not) and
        .effect == "confirmed" and
        any(.evidence[]?; .kind == "value_readback")
    ' <<<"$result" >/dev/null || {
        echo "Cua tool $tool lacked confirmed compositor readback: $result" >&2
        exit 1
    }
}

wait_for_window() {
    local needle=$1
    local deadline=$((SECONDS + 30))
    local windows
    while ((SECONDS < deadline)); do
        windows=$(guest cua-driver list_windows '{}')
        if jq -e --arg needle "${needle,,}" \
            '.windows[] |
             select(((.app_name // "") | ascii_downcase | contains($needle)) or
                    ((.title // "") | ascii_downcase | contains($needle)))' \
            <<<"$windows" >/dev/null; then
            jq -c --arg needle "${needle,,}" \
                '.windows[] |
                 select(((.app_name // "") | ascii_downcase | contains($needle)) or
                        ((.title // "") | ascii_downcase | contains($needle)))' \
                <<<"$windows" | head -1
            return
        fi
        sleep 1
    done
    echo "guest window containing '$needle' did not appear" >&2
    return 1
}

window_frame_for_pid() {
    local pid=$1
    guest cua-driver list_windows "{\"pid\":$pid}" |
        jq -ce --argjson pid "$pid" '
            [.windows[] | select(.pid == $pid and .is_on_screen)][0] |
            {x, y, width, height}
        '
}

sway_window_state_for_pid() {
    local pid=$1
    guest swaymsg -r -t get_tree |
        jq -ce --argjson pid "$pid" '
            [
                .. | objects |
                select(.type? == "workspace") as $workspace |
                (
                    $workspace.floating_nodes[]? |
                    recurse(.nodes[]?, .floating_nodes[]?)
                ) as $window |
                select($window.pid? == $pid) |
                {
                    frame: {
                        x: ($workspace.rect.x + $window.deco_rect.x),
                        y: ($workspace.rect.y + $window.deco_rect.y),
                        width: $window.rect.width,
                        height: ($window.rect.height + $window.deco_rect.height)
                    },
                    workspace: $workspace.rect,
                    workspace_name: $workspace.name,
                    marks: ($window.marks // []),
                    scratchpad_state: $window.scratchpad_state,
                    fullscreen_mode: $window.fullscreen_mode,
                    shell: $window.shell,
                    border: $window.border,
                    border_width: $window.current_border_width
                }
            ][0]
        '
}

drag_guest_frame_edge() {
    local pid=$1
    local edge=$2
    local before points after
    before=$(window_frame_for_pid "$pid")
    points=$(jq -c --arg edge "$edge" '
        (.x + (.width / 2 | floor)) as $cx |
        (.y + (.height / 2 | floor)) as $cy |
        if $edge == "left" then
            {from_x: .x + 1, from_y: $cy, to_x: .x - 15, to_y: $cy}
        elif $edge == "right" then
            {from_x: .x + .width - 2, from_y: $cy,
             to_x: .x + .width + 15, to_y: $cy}
        elif $edge == "top" then
            {from_x: $cx, from_y: .y + 1, to_x: $cx, to_y: .y - 15}
        elif $edge == "bottom" then
            {from_x: $cx, from_y: .y + .height - 2,
             to_x: $cx, to_y: .y + .height + 15}
        elif $edge == "top-left" then
            {from_x: .x + 1, from_y: .y + 1,
             to_x: .x - 15, to_y: .y - 15}
        elif $edge == "top-right" then
            {from_x: .x + .width - 2, from_y: .y + 1,
             to_x: .x + .width + 15, to_y: .y - 15}
        elif $edge == "bottom-left" then
            {from_x: .x + 1, from_y: .y + .height - 2,
             to_x: .x - 15, to_y: .y + .height + 15}
        else
            {from_x: .x + .width - 2, from_y: .y + .height - 2,
             to_x: .x + .width + 15, to_y: .y + .height + 15}
        end
    ' <<<"$before")
    assert_cua_ok drag "$(jq -c \
        '. + {scope:"desktop", duration_ms:180, steps:12}' <<<"$points")"
    after=$(window_frame_for_pid "$pid")
    jq -e -n --arg edge "$edge" --argjson before "$before" --argjson after "$after" '
        def near($a; $b): (($a - $b) | fabs) <= 4;
        ($before.x + $before.width) as $before_right |
        ($before.y + $before.height) as $before_bottom |
        ($after.x + $after.width) as $after_right |
        ($after.y + $after.height) as $after_bottom |
        if $edge == "left" then
            $after.x < $before.x - 8 and near($after_right; $before_right)
        elif $edge == "right" then
            near($after.x; $before.x) and $after_right > $before_right + 8
        elif $edge == "top" then
            $after.y < $before.y - 8 and near($after_bottom; $before_bottom)
        elif $edge == "bottom" then
            near($after.y; $before.y) and $after_bottom > $before_bottom + 8
        elif $edge == "top-left" then
            $after.x < $before.x - 8 and $after.y < $before.y - 8 and
            near($after_right; $before_right) and near($after_bottom; $before_bottom)
        elif $edge == "top-right" then
            $after.y < $before.y - 8 and $after_right > $before_right + 8 and
            near($after.x; $before.x) and near($after_bottom; $before_bottom)
        elif $edge == "bottom-left" then
            $after.x < $before.x - 8 and $after_bottom > $before_bottom + 8 and
            near($after_right; $before_right) and near($after.y; $before.y)
        else
            $after_right > $before_right + 8 and
            $after_bottom > $before_bottom + 8 and
            near($after.x; $before.x) and near($after.y; $before.y)
        end
    ' >/dev/null
}

wb doctor
if [[ ! -f "$portable_dir/vm/$machine/machine.json" ]]; then
    create_arguments=(create "$machine" --gpu all)
    if [[ -n "$accept_image" ]]; then
        create_arguments+=(--image "$accept_image")
    fi
    wb "${create_arguments[@]}"
fi
# Stop deliberately preserves the native host window and its supervising
# broker. Close any live supervisor instead, including one already in Stopped
# state, so the lease test below unequivocally exercises this AppImage/runtime
# rather than reusing an older build's process and AppDir.
existing_supervisor_pid=$(jq -r '.launcher_pid // empty' "$runtime" 2>/dev/null || true)
if [[ "$existing_supervisor_pid" =~ ^[1-9][0-9]*$ ]] &&
    [[ -r "/proc/$existing_supervisor_pid/stat" ]]; then
    existing_supervisor_start_time=$(process_start_time "$existing_supervisor_pid")
    wb window "$machine" close
    wait_stopped
    wait_process_identity_gone \
        "$existing_supervisor_pid" "$existing_supervisor_start_time"
fi
wb start "$machine" --detach
wait_running
refresh_pid
wait_native_window_frame
configured_width=$(jq -er '.width' "$portable_dir/vm/$machine/machine.json")
configured_height=$(jq -er '.height' "$portable_dir/vm/$machine/machine.json")
[[ $(jq -r '.display.window.width' "$runtime") == "$configured_width" ]]
[[ $(jq -r '.display.window.height' "$runtime") == "$configured_height" ]]

# `wb` deliberately forces the AppImage runtime's FUSE-less path. Its direct
# AppRun/launcher process returns after readiness, but the detached broker must
# retain a lease on that private AppDir because Stop keeps the same native host
# window alive and a later Start can need bundled helpers. This is a behavioral
# assertion against the real broker rather than an assumption about a magic fd
# number. It prevents extract-and-run from deleting gst-launch and the media
# plugins immediately after `start --detach` returns.
extract_lease_broker_pid=$(jq -er '.launcher_pid' "$runtime")
extract_lease_broker_start_time=$(process_start_time "$extract_lease_broker_pid")
extract_lease_appdir=$(appdir_for_process "$extract_lease_broker_pid")
case "$extract_lease_appdir" in
    "${TMPDIR:-/tmp}"/appimage_extracted_*) ;;
    *)
        echo "forced extract-and-run used an unexpected APPDIR: $extract_lease_appdir" >&2
        exit 1
        ;;
esac
[[ -d "$extract_lease_appdir" ]]
[[ $(stat -c %u "$extract_lease_appdir") == "$task_uid" ]]
[[ $(stat -c %a "$extract_lease_appdir") == 700 ]]
[[ -x "$extract_lease_appdir/usr/libexec/wildbuzzard/gst-launch-1.0" ]]

wb stop "$machine"
wait_stopped
[[ $(process_start_time "$extract_lease_broker_pid") == \
    "$extract_lease_broker_start_time" ]]
[[ -x "$extract_lease_appdir/usr/libexec/wildbuzzard/gst-launch-1.0" ]]
wb start "$machine" --detach
wait_running
refresh_pid
[[ $(jq -er '.launcher_pid' "$runtime") == "$extract_lease_broker_pid" ]]
[[ $(process_start_time "$extract_lease_broker_pid") == \
    "$extract_lease_broker_start_time" ]]
[[ $(appdir_for_process "$extract_lease_broker_pid") == "$extract_lease_appdir" ]]
wait_native_window_frame

# Exercise live TCP/UDP mappings in both directions and all three separately
# authorized media bridges against this already-running namespace. The helper
# snapshots and restores machine.json and asserts that the container PID never
# changes, so this also guards the no-restart live-reconciliation contract.
if [[ "$integration_acceptance" == 1 ]]; then
    "$project_dir/tests/acceptance/integration-acceptance.sh" "$appimage" "$machine"
    refresh_pid
fi

# Namespace, PID 1, portable layout, private network, and explicit data share.
[[ $(guest cat /proc/1/comm) == systemd ]]
[[ $(guest hostname) == "$machine" ]]
# Bubblewrap must construct POSIX message queues before systemd starts. If PID
# 1 has to invoke a mount helper inside this rootless namespace,
# dev-mqueue.mount can fail even though the kernel supports mqueue.
[[ $(guest findmnt -rn -M /dev/mqueue -o FSTYPE) == mqueue ]]
[[ $(guest systemctl is-active dev-mqueue.mount) == active ]]
[[ -z "$(guest systemctl --failed --no-legend --plain --no-pager)" ]]
for namespace in user pid mnt net ipc uts cgroup; do
    host_namespace=$(readlink "/proc/self/ns/$namespace")
    guest_namespace=$(readlink "/proc/$container_pid/ns/$namespace")
    [[ "$host_namespace" != "$guest_namespace" ]]
done
[[ $(guest find /sys/class/net -mindepth 1 -maxdepth 1 -printf '%f\n' | sort | paste -sd,) == lo,tap0 ]]

# A real host-loopback listener must not be reachable through slirp's host
# gateway in the default private network mode.
loopback_probe=$(mktemp "$portable_dir/cache/loopback-probe.XXXXXX")
python3 - "$loopback_probe" <<'PY' &
import pathlib
import socket
import sys

listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen()
pathlib.Path(sys.argv[1]).write_text(str(listener.getsockname()[1]), encoding="ascii")
listener.settimeout(10)
try:
    listener.accept()
except TimeoutError:
    pass
PY
loopback_listener=$!
deadline=$((SECONDS + 5))
while ((SECONDS < deadline)) && [[ ! -s "$loopback_probe" ]]; do
    sleep 0.05
done
[[ -s "$loopback_probe" ]]
loopback_port=$(<"$loopback_probe")
guest python3 - "$loopback_port" <<'PY'
import socket
import sys

client = socket.socket()
client.settimeout(2)
raise SystemExit(
    0 if client.connect_ex(("10.0.2.2", int(sys.argv[1]))) != 0 else 1
)
PY
kill "$loopback_listener" 2>/dev/null || true
wait "$loopback_listener" 2>/dev/null || true
rm -f -- "$loopback_probe"

[[ $(guest stat -c %a /run/wildbuzzard-host/wayland-0) == 0 ]]
! guest test -e /run/wildbuzzard-host/window-control
! test -e "$portable_dir/vm/$machine/.window-control.sock"
! guest python3 -c \
    'import socket; client = socket.socket(socket.AF_UNIX); client.connect("/run/wildbuzzard-host/wayland-0")' \
    2>/dev/null
! guest test -e /run/user/1000/wayland-0
! guest test -S /run/docker.sock
! guest test -S /var/run/docker.sock
[[ $(guest sudo -n id -u) == 0 ]]
rootfs_host_uid=$(stat -c %u "$portable_dir/vm/$machine/rootfs")
[[ "$rootfs_host_uid" != 0 ]]
[[ "$rootfs_host_uid" != "$(id -u)" ]]
guest findmnt -T / -n -o OPTIONS | grep -q 'nosuid'
guest findmnt -T / -n -o OPTIONS | grep -q 'nodev'
[[ $(jq -r '.display.window.toplevels' "$runtime") == 1 ]]
[[ $(jq -r '.display.presentation.transport' "$runtime") == dmabuf ]]
[[ $(jq -r '.display.presentation.presented' "$runtime") == true ]]
[[ $(jq -r '.display.presentation.vsync' "$runtime") == true ]]
[[ $(jq -r '.display.presentation.native_resolution' "$runtime") == true ]]
[[ $(jq -r '.display.render_nodes | length' "$runtime") -gt 0 ]]
[[ $(jq -r '.display.renderer' "$runtime") == gles2 ]]
[[ $(jq -r '.display.selected_render_device_identity | length' "$runtime") -gt 0 ]]
[[ $(jq -r '.display.render_device_identities | length' "$runtime") -gt 0 ]]
[[ $(jq -r '.display.host_device_identity | length' "$runtime") -gt 0 ]]
for host_gpu_device in \
    /dev/dri/* \
    /dev/nvidia[0-9]* \
    /dev/nvidiactl \
    /dev/nvidia-modeset \
    /dev/nvidia-uvm \
    /dev/nvidia-uvm-tools \
    /dev/nvidia-caps/*; do
    [[ -c "$host_gpu_device" ]] || continue
    jq -e --arg device "$host_gpu_device" \
        '.display.exposed_devices | index($device) != null' \
        "$runtime" >/dev/null
done
! jq -e '.. | strings | select(startswith("/"))' \
    "$portable_dir/vm/$machine/machine.json" >/dev/null

# The private desktop sockets may use familiar names, but they must be
# different kernel socket objects from the host session.
host_bus_inode=$(stat -Lc %i /run/user/"$(id -u)"/bus)
guest_bus_inode=$(guest stat -Lc %i /run/user/1000/bus)
[[ "$host_bus_inode" != "$guest_bus_inode" ]]
if [[ -S /run/user/"$(id -u)"/pipewire-0 ]]; then
    host_pipewire_inode=$(stat -Lc %i /run/user/"$(id -u)"/pipewire-0)
    guest_pipewire_inode=$(guest stat -Lc %i /run/user/1000/pipewire-0)
    [[ "$host_pipewire_inode" != "$guest_pipewire_inode" ]]
fi
if [[ -S /run/user/"$(id -u)"/at-spi/bus_0 ]]; then
    host_atspi_inode=$(stat -Lc %i /run/user/"$(id -u)"/at-spi/bus_0)
    guest_atspi_inode=$(guest stat -Lc %i /run/user/1000/at-spi/bus_0)
    [[ "$host_atspi_inode" != "$guest_atspi_inode" ]]
fi
compositor_pid=$(guest pgrep -xo sway)
session_environment=$(guest sh -c 'tr "\0" "\n" <"/proc/$1/environ"' sh "$compositor_pid")
! grep -Eq \
    '^(APPIMAGE|ELECTRON_RUN_AS_NODE|SNAP|SSH_AUTH_SOCK|XDG_DATA_HOME=/home/)' \
    <<<"$session_environment"
grep -Fx 'WLR_RENDERER=gles2' <<<"$session_environment" >/dev/null
if guest test -e /dev/nvidiactl; then
    grep -Fx 'LD_LIBRARY_PATH=/run/wildbuzzard-host/driver/lib' \
        <<<"$session_environment" >/dev/null
    grep -E '^WLR_RENDER_DRM_DEVICE=/dev/dri/renderD[0-9]+$' \
        <<<"$session_environment" >/dev/null
fi

printf '%s\n' "$marker" >"$portable_dir/shared/.wildbuzzard-acceptance"
[[ $(guest cat /shared/.wildbuzzard-acceptance) == "$marker" ]]
guest sh -c 'printf "%s\n" "$1" > /shared/.wildbuzzard-guest-created' sh "$marker"
[[ $(stat -c %u "$portable_dir/shared/.wildbuzzard-guest-created") == "$(id -u)" ]]
[[ $(stat -c %g "$portable_dir/shared/.wildbuzzard-guest-created") == "$(id -g)" ]]
printf '%s-host-edit\n' "$marker" >"$portable_dir/shared/.wildbuzzard-guest-created"
[[ $(guest cat /shared/.wildbuzzard-guest-created) == "$marker-host-edit" ]]
guest mkdir -p /shared/.wildbuzzard-guest-directory
printf '%s-host-created\n' "$marker" \
    >"$portable_dir/shared/.wildbuzzard-guest-directory/host-file"
[[ $(guest cat /shared/.wildbuzzard-guest-directory/host-file) == "$marker-host-created" ]]
guest sh -c "printf '%s\\n' '$marker' > /home/wildbuzzard/.wildbuzzard-persistence"
guest sh -c "printf '%s\\n' '$marker' > /home/wildbuzzard/.config/wildbuzzard-acceptance.setting"
guest install -d -m 0700 /home/wildbuzzard/.config/sway
guest sh -c 'printf "%s\n" "$1" \
    > /home/wildbuzzard/.config/sway/wildbuzzard-acceptance.marker' \
    sh "$marker"
# Integration assets are installed when the machine is created, but normal
# starts must not silently restore them over guest-root changes. This harmless
# comment makes that durable-rootfs invariant observable across the restart
# below.
guest sudo -n sh -c \
    'printf "%s\n" "# persistent guest OS edit: $1" >> /etc/wildbuzzard/sway-config' \
    sh "$marker"
guest kill -HUP "$compositor_pid"
wait_native_window_frame
guest install -d -m 0700 /home/wildbuzzard/.local/bin
guest sh -c 'cat > /home/wildbuzzard/.local/bin/wildbuzzard-acceptance-agent' <<'AGENT'
#!/bin/sh
set -eu
capture=${XDG_RUNTIME_DIR:-/run/user/1000}/wildbuzzard-arbitrary-agent.png
grim "$capture"
test -s "$capture"
python3 -c 'import pyatspi; assert pyatspi.Registry.getDesktopCount() > 0; assert pyatspi.Registry.getDesktop(0).childCount > 0'
cat /home/wildbuzzard/.wildbuzzard-persistence
AGENT
guest chmod 0700 /home/wildbuzzard/.local/bin/wildbuzzard-acceptance-agent
[[ $(guest /home/wildbuzzard/.local/bin/wildbuzzard-acceptance-agent) == "$marker" ]]

# The reference image uses pinned, unmodified upstream Sway 1.12 and wlroots
# 0.20.2 commits, and deliberately omits the discarded
# compositor/desktop stack, Blender, and build toolchain.
guest dpkg-query -W libwlroots-0.20 >/dev/null
[[ $(guest readlink -f "$(guest sh -c 'command -v sway')") == /usr/bin/sway ]]
guest sway --version 2>&1 | grep -E '^sway version 1\.12' >/dev/null
guest grep -Fxq \
    'commit = "88869399f421d9180dd8b6ed0b5a1f4a3585d252"' \
    /usr/share/doc/wildbuzzard-sway/UPSTREAM.toml
guest grep -Fxq \
    'commit = "d783533489e1f75d6886c2ab5c5960090ef268f8"' \
    /usr/share/doc/wildbuzzard-sway/UPSTREAM.toml
guest test -f /usr/share/doc/wildbuzzard-sway/LICENSE.sway
guest test -f /usr/share/doc/wildbuzzard-sway/LICENSE.wlroots
for forbidden in blender gcc kwin_wayland labwc make wayfire waybar fuzzel; do
    ! guest sh -c 'command -v "$1"' sh "$forbidden" >/dev/null 2>&1
done
for forbidden_package in blender kwin-wayland labwc plasma-workspace wayfire waybar fuzzel; do
    ! guest dpkg-query -W "$forbidden_package" >/dev/null 2>&1
done
for wallet_activation in \
    /usr/share/applications/org.kde.ksecretd.desktop \
    /usr/share/dbus-1/services/org.freedesktop.impl.portal.desktop.kwallet.service \
    /usr/share/dbus-1/services/org.kde.kwalletd5.service \
    /usr/share/dbus-1/services/org.kde.kwalletd6.service \
    /usr/share/dbus-1/services/org.kde.secretservicecompat.service \
    /usr/share/xdg-desktop-portal/portals/kwallet.portal; do
    ! guest test -e "$wallet_activation"
done
guest grep -q -- '--password-store=basic' /etc/chromium.d/wildbuzzard
guest grep -q -- '--force-renderer-accessibility=complete' \
    /etc/chromium.d/wildbuzzard
! guest pgrep -x ksecretd >/dev/null 2>&1
! guest pgrep -x kwalletd6 >/dev/null 2>&1

# Private session D-Bus, AT-SPI, Sway output control, full-output native
# capture, and a
# representative GTK accessibility tree.
guest dbus-send --session --dest=org.a11y.Bus --type=method_call --print-reply \
    /org/a11y/bus org.a11y.Bus.GetAddress >/dev/null
wait_sway_output_matches_runtime
guest cua-driver health_report '{}' | jq -e '.overall == "ok"' >/dev/null
wait_cua_capture_matches_runtime
guest pgrep -x sway >/dev/null
guest pgrep -x wildbuzzard-she >/dev/null
guest pgrep -x mako >/dev/null
guest pgrep -x pipewire >/dev/null
guest pgrep -x pipewire-pulse >/dev/null
guest pgrep -x wireplumber >/dev/null
guest pgrep -f '^/usr/bin/python3 /usr/libexec/wildbuzzard-output-sync$' >/dev/null
guest pgrep -f '^/usr/libexec/at-spi2-registryd ' >/dev/null
guest python3 - <<'PY'
import pyatspi
import time

desktop = pyatspi.Registry.getDesktop(0)
def walk(node, depth=0):
    yield node
    if depth < 10:
        try:
            for child in node:
                yield from walk(child, depth + 1)
        except Exception:
            pass

nodes = list(walk(desktop))
labels = [node.name for node in nodes if node.name]
assert "Wild Buzzard Desktop" in labels
assert "Applications" in labels

button = next(node for node in nodes if node.name == "Applications")
actions = button.queryAction()
action_names = [actions.getName(index) for index in range(actions.nActions)]
assert "click" in action_names
assert actions.doAction(action_names.index("click"))
time.sleep(1)

nodes = list(walk(pyatspi.Registry.getDesktop(0)))
labels = [node.name for node in nodes if node.name]
for expected in [
    "Chromium Web Browser",
    "Dolphin",
    "Firefox ESR",
    "Foot",
    "Thunar File Manager",
    "Volume Control",
    "Wild Buzzard Electron",
    "Shut Down Machine",
]:
    assert expected in labels
chromium = next(node for node in nodes if node.name == "Chromium Web Browser")
chromium_actions = chromium.queryAction()
assert "click" in [
    chromium_actions.getName(index)
    for index in range(chromium_actions.nActions)
]

# Return the human-facing menu to its original closed state.
button = next(node for node in nodes if node.name == "Applications")
actions = button.queryAction()
action_names = [actions.getName(index) for index in range(actions.nActions)]
assert actions.doAction(action_names.index("click"))
PY
for guest_command in wtype Xwayland; do
    guest sh -c 'command -v "$1"' sh "$guest_command" >/dev/null
done
! guest sh -c 'command -v wildbuzzard-window-control' >/dev/null

# The native Rust shell is functional, not merely installed, and advertises
# semantic AT-SPI actions while a D-Bus notification reaches mako.
guest test -x /usr/libexec/wildbuzzard-shell
guest notify-send --app-name=wildbuzzard-acceptance \
    "Wild Buzzard acceptance" "Notification is visible"
deadline=$((SECONDS + 10))
while ((SECONDS < deadline)) &&
    ! guest makoctl list | grep -q "Wild Buzzard acceptance"; do
    sleep 0.1
done
guest makoctl list | grep -q "Wild Buzzard acceptance"
guest makoctl dismiss --all

for process_name in chromium dolphin electron foot glxgears thunar vkcube xev xeyes; do
    guest pkill -x "$process_name" >/dev/null 2>&1 || true
done

guest pkill -x thunar >/dev/null 2>&1 || true
guest_spawn thunar
sleep 2
windows=$(guest cua-driver list_windows '{}')
thunar_pid=$(jq -er '.windows[] | select(.app_name == "thunar") | .pid' <<<"$windows" | head -1)
thunar_window=$(jq -er '.windows[] | select(.app_name == "thunar") | .window_id' <<<"$windows" | head -1)
thunar_state=$(guest cua-driver get_window_state \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window,\"include_screenshot\":false}")
jq -e '.element_count > 10 and (.tree_markdown | length) > 100' \
    <<<"$thunar_state" >/dev/null

# Stock Sway owns one synchronized normal frame for every managed application.
# Drive its titlebar and all four edges/corners with desktop-absolute CUA input;
# including pid/window_id here would select window-local coordinates instead.
guest grep -Fxq 'for_window [all] floating enable, border normal 8' \
    /etc/wildbuzzard/sway-config
guest grep -Fxq 'show_marks no' /etc/wildbuzzard/sway-config
guest swaymsg -r -t get_tree | jq -e --argjson pid "$thunar_pid" '
    .. | objects |
    select(.pid? == $pid) |
    .floating != "auto_off" and .floating != "user_off" and
    .border == "normal" and .deco_rect.height > 0
' >/dev/null
thunar_before_drag=$(window_frame_for_pid "$thunar_pid")
titlebar_drag=$(jq -c '
    {
        scope: "desktop",
        from_x: (.x + (.width / 2 | floor)),
        from_y: (.y + 10),
        to_x: (.x + (.width / 2 | floor) + 36),
        to_y: (.y + 34),
        duration_ms: 220,
        steps: 14
    }
' <<<"$thunar_before_drag")
assert_cua_ok drag "$titlebar_drag"
thunar_after_drag=$(window_frame_for_pid "$thunar_pid")
jq -e -n --argjson before "$thunar_before_drag" --argjson after "$thunar_after_drag" '
    $after.x > $before.x + 20 and $after.y > $before.y + 12 and
    (($after.width - $before.width) | fabs) <= 4 and
    (($after.height - $before.height) | fabs) <= 4
' >/dev/null
for guest_frame_edge in \
    left right top bottom \
    top-left top-right bottom-left bottom-right; do
    drag_guest_frame_edge "$thunar_pid" "$guest_frame_edge"
done

assert_cua_ok bring_to_front \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window}"
thunar_action=$(jq -er \
    '.elements[] |
     select(.enabled and .role == "button" and .label == "Home") |
     .element_token' \
    <<<"$thunar_state" | head -1)
assert_cua_ok click \
    "{\"pid\":$thunar_pid,\"element_token\":\"$thunar_action\"}"
assert_cua_ok press_key \
    "{\"scope\":\"desktop\",\"pid\":$thunar_pid,\"window_id\":$thunar_window,\"key\":\"escape\",\"delivery_mode\":\"foreground\"}"

# Native guest-global pointer, click, double-click, scroll, and drag routes.
# Coordinates stay inside the dedicated acceptance machine's Thunar window.
assert_cua_ok move_cursor '{"scope":"desktop","x":900,"y":500}'
assert_cua_ok click \
    "{\"scope\":\"desktop\",\"pid\":$thunar_pid,\"window_id\":$thunar_window,\"x\":900,\"y\":500,\"delivery_mode\":\"foreground\"}"
assert_cua_ok double_click \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window,\"x\":900,\"y\":500,\"delivery_mode\":\"foreground\"}"
assert_cua_ok scroll \
    "{\"scope\":\"desktop\",\"pid\":$thunar_pid,\"window_id\":$thunar_window,\"x\":900,\"y\":500,\"direction\":\"down\",\"amount\":1,\"delivery_mode\":\"foreground\"}"
assert_cua_ok drag \
    "{\"scope\":\"desktop\",\"pid\":$thunar_pid,\"window_id\":$thunar_window,\"from_x\":900,\"from_y\":500,\"to_x\":920,\"to_y\":500,\"duration_ms\":100,\"delivery_mode\":\"foreground\"}"

# Verifiable typing, hotkey, and key delivery in a terminal wholly inside the
# guest. Ctrl+U clears the first text before the expected marker is submitted.
guest sh -c 'printf "%s\n" "#!/bin/bash" "IFS= read -e -r value" \
    "printf \"%s\" \"\$value\" > /home/wildbuzzard/.wildbuzzard-cua-input" \
    "sleep 10" > /tmp/wildbuzzard-input-test; chmod 700 /tmp/wildbuzzard-input-test'
guest_spawn foot --app-id wildbuzzard-acceptance /tmp/wildbuzzard-input-test
wait_for_window wildbuzzard-acceptance >/dev/null
assert_cua_ok type_text \
    '{"scope":"desktop","text":"wrong","delivery_mode":"foreground"}'
assert_cua_ok hotkey \
    '{"scope":"desktop","keys":["ctrl","u"],"delivery_mode":"foreground"}'
assert_cua_ok type_text \
    "{\"scope\":\"desktop\",\"text\":\"$marker\",\"delivery_mode\":\"foreground\"}"
assert_cua_ok press_key \
    '{"scope":"desktop","key":"enter","delivery_mode":"foreground"}'
sleep 1
[[ $(guest cat /home/wildbuzzard/.wildbuzzard-cua-input) == "$marker" ]]
guest pkill -x foot >/dev/null 2>&1 || true

# Classic state changes remain compositor-owned even though stock Sway has no
# titlebar buttons. Maximize fills the usable workspace (the output minus the
# shell's exclusive bottom taskbar), its restore geometry survives minimize,
# normal minimize preserves its frame, and close is confirmed by disappearance
# of the exact opaque CUA window id. The requested canonical-physical values
# are multiples of 420, so every accepted 100/125/133/150/175/200% guest scale
# has an exact integer-logical representation and readback cannot pass by
# rounding tolerance.
assert_cua_confirmed set_window_frame \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window,\"x\":420,\"y\":420,\"width\":840,\"height\":420}"
assert_cua_confirmed maximize_window \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window}"
thunar_maximized=$(sway_window_state_for_pid "$thunar_pid")
guest_logical_width=$(guest jq -er \
    '.guest_logical_width' /run/wildbuzzard-display-state/output-state.json)
guest_logical_height=$(guest jq -er \
    '.guest_logical_height' /run/wildbuzzard-display-state/output-state.json)
jq -e -n \
    --argjson state "$thunar_maximized" \
    --argjson width "$guest_logical_width" \
    --argjson height "$guest_logical_height" '
        $state.frame == $state.workspace and
        $state.workspace == {x:0, y:0, width:$width, height:($height - 42)} and
        $state.fullscreen_mode == 0 and
        any($state.marks[]; startswith("__wildbuzzard_restore_v1_"))
    ' >/dev/null

# Stock Sway emits no IPC window event for floating resize motion. Shrink the
# maximized frame through its real left border while it is already focused,
# then open the task context menu. The shell must synchronously refresh the
# tree, expose Maximize (not stale Restore), and maximize the live resized
# frame when that semantic action is invoked.
thunar_max_before_resize=$(window_frame_for_pid "$thunar_pid")
assert_cua_ok drag "$(jq -c '
    {
        scope: "desktop",
        from_x: (.x + 1),
        from_y: (.y + (.height / 2 | floor)),
        to_x: (.x + 48),
        to_y: (.y + (.height / 2 | floor)),
        duration_ms: 220,
        steps: 14
    }
' <<<"$thunar_max_before_resize")"
thunar_after_max_resize=$(window_frame_for_pid "$thunar_pid")
thunar_resized_normal=$(sway_window_state_for_pid "$thunar_pid")
jq -e -n \
    --argjson before "$thunar_max_before_resize" \
    --argjson after "$thunar_after_max_resize" '
        def near($a; $b): (($a - $b) | fabs) <= 4;
        $after.x > $before.x + 30 and
        $after.width < $before.width - 30 and
        near($after.x + $after.width; $before.x + $before.width)
    ' >/dev/null
thunar_title=$(guest swaymsg -r -t get_tree |
    jq -er --argjson pid "$thunar_pid" \
        '.. | objects | select(.pid? == $pid) | .name' | head -1)
read -r task_logical_x task_logical_y < <(
    guest python3 - "$thunar_title" <<'PY'
import pyatspi
import sys

def walk(node, depth=0):
    yield node
    if depth < 12:
        try:
            for child in node:
                yield from walk(child, depth + 1)
        except Exception:
            pass

target = f"Switch to {sys.argv[1]}"
desktop = pyatspi.Registry.getDesktop(0)
shell = next(node for node in walk(desktop) if node.name == "Wild Buzzard Desktop")
button = next(node for node in walk(shell) if node.name == target)
extents = button.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
print(extents.x + extents.width // 2, extents.y + extents.height // 2)
PY
)
read -r logical_width logical_height physical_width physical_height < <(
    guest jq -r \
        '[.guest_logical_width, .guest_logical_height,
          .physical_width, .physical_height] | @tsv' \
        /run/wildbuzzard-display-state/output-state.json
)
task_x=$(((task_logical_x * physical_width + logical_width / 2) / logical_width))
task_y=$(((task_logical_y * physical_height + logical_height / 2) / logical_height))
assert_cua_ok click \
    "{\"scope\":\"desktop\",\"x\":$task_x,\"y\":$task_y,\"button\":\"right\"}"
guest python3 - "$thunar_title" <<'PY'
import pyatspi
import sys
import time

def walk(node, depth=0):
    yield node
    if depth < 12:
        try:
            for child in node:
                yield from walk(child, depth + 1)
        except Exception:
            pass

maximize_label = f"Maximize {sys.argv[1]}"
restore_label = f"Restore {sys.argv[1]}"
for _ in range(100):
    desktop = pyatspi.Registry.getDesktop(0)
    shell = next(node for node in walk(desktop) if node.name == "Wild Buzzard Desktop")
    nodes = list(walk(shell))
    labels = [node.name for node in nodes]
    if maximize_label in labels:
        break
    time.sleep(0.05)
assert maximize_label in labels
assert restore_label not in labels
maximize = next(node for node in nodes if node.name == maximize_label)
actions = maximize.queryAction()
names = [actions.getName(index) for index in range(actions.nActions)]
assert "click" in names
assert actions.doAction(names.index("click"))
PY
deadline=$((SECONDS + 5))
while ((SECONDS < deadline)); do
    thunar_maximized=$(sway_window_state_for_pid "$thunar_pid")
    if jq -e '.frame == .workspace and
        any(.marks[]; startswith("__wildbuzzard_restore_v1_"))' \
        <<<"$thunar_maximized" >/dev/null; then
        break
    fi
    sleep 0.05
done
jq -e '.frame == .workspace and
    any(.marks[]; startswith("__wildbuzzard_restore_v1_"))' \
    <<<"$thunar_maximized" >/dev/null

assert_cua_confirmed minimize_window \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window}"
thunar_minimized=$(sway_window_state_for_pid "$thunar_pid")
jq -e '
    .workspace_name == "__i3_scratch" and
    .scratchpad_state == "fresh" and
    any(.marks[]; startswith("__wildbuzzard_restore_v1_"))
' <<<"$thunar_minimized" >/dev/null
assert_cua_confirmed restore_window \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window}"
thunar_restored=$(sway_window_state_for_pid "$thunar_pid")
jq -e -n --argjson before "$thunar_resized_normal" --argjson after "$thunar_restored" '
    $after.frame == $before.frame and
    $after.workspace_name != "__i3_scratch" and
    ($after.marks | map(select(startswith("__wildbuzzard_restore_v1_"))) | length) == 0
' >/dev/null
assert_cua_confirmed minimize_window \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window}"
assert_cua_confirmed restore_window \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window}"
jq -e -n \
    --argjson before "$thunar_restored" \
    --argjson after "$(sway_window_state_for_pid "$thunar_pid")" \
    '$after.frame == $before.frame' >/dev/null
assert_cua_confirmed close_window \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window}"
guest cua-driver list_windows '{}' |
    jq -e --argjson id "$thunar_window" \
        'all(.windows[]; .window_id != $id)' >/dev/null

if [[ "$full_matrix" == 1 ]]; then
    # Representative native Wayland Qt/KDE plus Electron, Chromium, and legacy
    # Xwayland clients must all remain inside Sway's one output and publish
    # their normal accessibility objects where the toolkit supports them.
    for guest_command in dolphin chromium xeyes; do
        guest sh -c 'command -v "$1"' sh "$guest_command" >/dev/null
    done
    guest pkill -x dolphin >/dev/null 2>&1 || true
    # Start from a known non-Home location so the semantic action below must
    # produce an observable navigation, rather than passing because Dolphin
    # restored an already-Home session.
    guest_spawn dolphin /home/wildbuzzard/Downloads
    dolphin=$(wait_for_window dolphin)
    dolphin_pid=$(jq -er '.pid' <<<"$dolphin")
    dolphin_window=$(jq -er '.window_id' <<<"$dolphin")
    dolphin_state=$(guest cua-driver get_window_state \
        "{\"pid\":$dolphin_pid,\"window_id\":$dolphin_window,\"include_screenshot\":false}")
    jq -e '.element_count > 10 and (.tree_markdown | length) > 100' \
        <<<"$dolphin_state" >/dev/null
    # Qt 6 exposes Dolphin's current location as a page tab, the Places
    # shortcuts as selectable list items, and the Go menu commands as semantic
    # Press actions. Invoke the exact Go -> Home command: toggling the Places
    # row can be accepted by AT-SPI without navigating on some Dolphin builds.
    dolphin_action=$(jq -er \
        '.elements[] |
         select(.enabled and .role == "menu item" and .label == "Home") |
         .element_token' \
        <<<"$dolphin_state" | head -1)
    dolphin_click=$(guest cua-driver click \
        "{\"pid\":$dolphin_pid,\"element_token\":\"$dolphin_action\"}")
    jq -e '
        (has("code") | not) and
        .effect == "confirmed" and
        any(.evidence[]?; .kind == "screenshot_change")
    ' <<<"$dolphin_click" >/dev/null
    deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)); do
        dolphin_title=$(guest cua-driver list_windows '{}' | jq -r \
            --argjson pid "$dolphin_pid" \
            '.windows[] | select(.pid == $pid) | .title' | head -1)
        [[ "$dolphin_title" == Home* ]] && break
        sleep 0.1
    done
    [[ "$dolphin_title" == Home* ]]
    assert_cua_ok press_key \
        "{\"scope\":\"desktop\",\"pid\":$dolphin_pid,\"window_id\":$dolphin_window,\"key\":\"escape\",\"delivery_mode\":\"foreground\"}"
    guest pkill -x dolphin >/dev/null 2>&1 || true

    # Exercise a real vendor-distributed Electron AppImage. It is copied with
    # mode 0644 on purpose: the guest watcher must recognize its AppImage magic
    # and authorize owner execution, after which this is an ordinary direct
    # exec/FUSE launch (never --appimage-extract-and-run).
    electron_appimage=${WILDBUZZARD_ELECTRON_APPIMAGE:-}
    [[ -f "$electron_appimage" ]] || {
        echo "full matrix requires WILDBUZZARD_ELECTRON_APPIMAGE" >&2
        exit 1
    }
    electron_name="wildbuzzard-electron-acceptance-$$.AppImage"
    electron_log_name="wildbuzzard-electron-acceptance-$$.log"
    electron_acceptance_host_path="$portable_dir/shared/$electron_name"
    electron_acceptance_log_path="$portable_dir/shared/$electron_log_name"
    if [[ -e "$electron_acceptance_host_path" ||
        -e "$electron_acceptance_log_path" ]]; then
        echo "refusing to replace a pre-existing AppImage acceptance artifact" >&2
        exit 1
    fi
    cp -- "$electron_appimage" "$electron_acceptance_host_path"
    chmod 0644 "$electron_acceptance_host_path"
    deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)); do
        guest test -x "/shared/$electron_name" && break
        sleep 0.05
    done
    guest test -x "/shared/$electron_name"
    guest_spawn_logged "/shared/$electron_log_name" \
        "/shared/$electron_name"
    deadline=$((SECONDS + 45))
    electron=
    while ((SECONDS < deadline)); do
        electron=$(guest cua-driver list_windows '{}' | jq -c \
            '[.windows[] |
              select((.title | ascii_downcase) | contains("lm studio"))][0] // empty')
        [[ -n "$electron" ]] && break
        sleep 0.25
    done
    if [[ -z "$electron" ]]; then
        guest tail -n 80 "/shared/$electron_log_name" >&2 || true
        echo "LM Studio AppImage did not publish a guest window" >&2
        exit 1
    fi
    electron_pid=$(jq -er '.pid' <<<"$electron")
    electron_window=$(jq -er '.window_id' <<<"$electron")
    electron_acceptance_guest_session=$(guest_session_for_pid "$electron_pid")
    [[ "$electron_acceptance_guest_session" =~ ^[1-9][0-9]*$ ]]
    assert_appimage_session_unprivileged "$electron_acceptance_guest_session"
    electron_mountpoint=$(appimage_fuse_mount_for_pid "$electron_pid")
    assert_cua_ok bring_to_front \
        "{\"pid\":$electron_pid,\"window_id\":$electron_window}"
    wait_cua_capture_matches_runtime
    wait_appimage_session_cleanup \
        "$electron_pid" "$electron_acceptance_guest_session" "$electron_mountpoint"
    electron_acceptance_guest_session=
    rm -f -- "$electron_acceptance_host_path" "$electron_acceptance_log_path"
    electron_acceptance_host_path=
    electron_acceptance_log_path=

    guest pkill -x chromium >/dev/null 2>&1 || true
    guest_spawn chromium --no-sandbox \
        --ozone-platform=wayland \
        --user-data-dir=/tmp/wildbuzzard-chromium about:blank
    chromium=$(wait_for_window chromium)
    chromium_pid=$(jq -er '.pid' <<<"$chromium")
    chromium_window=$(jq -er '.window_id' <<<"$chromium")
    guest sh -c \
        'grep -zq -- "--password-store=basic" "/proc/$1/cmdline"' \
        sh "$chromium_pid"
    guest sh -c \
        'grep -zq -- "--force-renderer-accessibility=complete" "/proc/$1/cmdline"' \
        sh "$chromium_pid"
    guest cua-driver get_window_state \
        "{\"pid\":$chromium_pid,\"window_id\":$chromium_window,\"include_screenshot\":false}" |
        jq -e '.element_count > 4 and (.tree_markdown | length) > 100' >/dev/null
    ! guest pgrep -x ksecretd >/dev/null 2>&1
    ! guest pgrep -x kwalletd6 >/dev/null 2>&1
    guest cua-driver list_windows '{}' |
        jq -e '
            [.windows[] |
             select(
                (.app_name | ascii_downcase | contains("wallet")) or
                (.app_name | ascii_downcase | contains("secret")) or
                (.title | ascii_downcase | contains("wallet"))
             )] |
            length == 0
        ' >/dev/null
    guest pkill -x chromium >/dev/null 2>&1 || true

    guest pkill -x xeyes >/dev/null 2>&1 || true
    guest_spawn xeyes
    deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)) && ! guest pgrep -x xeyes >/dev/null; do
        sleep 0.1
    done
    guest pgrep -x xeyes >/dev/null
    # xeyes is a canvas-like X11 client with no useful semantic control tree;
    # prove its stock Xwayland frame has the same title drag, exact border hit
    # tests, and all eight live resize paths as the native Wayland window.
    xeyes=$(wait_for_window xeyes)
    xeyes_pid=$(jq -er '.pid' <<<"$xeyes")
    jq -e '.shell == "xwayland" and .border == "normal" and .border_width == 10' \
        <<<"$(sway_window_state_for_pid "$xeyes_pid")" >/dev/null
    xeyes_before_drag=$(window_frame_for_pid "$xeyes_pid")
    assert_cua_ok drag "$(jq -c '
        {
            scope: "desktop",
            from_x: (.x + (.width / 2 | floor)),
            from_y: (.y + 10),
            to_x: (.x + (.width / 2 | floor) + 36),
            to_y: (.y + 34),
            duration_ms: 220,
            steps: 14
        }
    ' <<<"$xeyes_before_drag")"
    xeyes_after_drag=$(window_frame_for_pid "$xeyes_pid")
    jq -e -n --argjson before "$xeyes_before_drag" --argjson after "$xeyes_after_drag" '
        $after.x > $before.x + 20 and $after.y > $before.y + 12 and
        (($after.width - $before.width) | fabs) <= 4 and
        (($after.height - $before.height) | fabs) <= 4
    ' >/dev/null
    for guest_frame_edge in \
        left right top bottom \
        top-left top-right bottom-left bottom-right; do
        drag_guest_frame_edge "$xeyes_pid" "$guest_frame_edge"
    done

    # It also remains observable and operable through global capture/input.
    guest cua-driver get_desktop_state '{}' |
        jq -e '(.screenshot_png_b64 | length) > 0' >/dev/null

    # Prove that screenshot-driven input reaches a canvas-like Xwayland client
    # with no useful semantic controls. xev records the real button event,
    # turning the Cua input route into an observable assertion.
    guest sh -c 'cat > /tmp/wildbuzzard-xev-canvas' <<'XEV'
#!/bin/sh
exec xev -event mouse >/tmp/wildbuzzard-xev-canvas.log 2>&1
XEV
    guest chmod 0700 /tmp/wildbuzzard-xev-canvas
    guest rm -f /tmp/wildbuzzard-xev-canvas.log
    guest_spawn /tmp/wildbuzzard-xev-canvas
    canvas_info=
    deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)); do
        canvas_info=$(guest xwininfo -name 'Event Tester' 2>/dev/null || true)
        [[ -n "$canvas_info" ]] && break
        sleep 0.1
    done
    [[ -n "$canvas_info" ]]
    canvas_logical_x=$(awk '/Absolute upper-left X:/ {print $4}' <<<"$canvas_info")
    canvas_logical_y=$(awk '/Absolute upper-left Y:/ {print $4}' <<<"$canvas_info")
    canvas_width=$(awk '/^[[:space:]]*Width:/ {print $2}' <<<"$canvas_info")
    canvas_height=$(awk '/^[[:space:]]*Height:/ {print $2}' <<<"$canvas_info")
    # Xwayland exposes compositor-logical coordinates, while desktop-scope CUA
    # uses the native physical dmabuf pixels returned in its screenshots.
    # Convert the logical target once at this API boundary.
    canvas_logical_x=$((canvas_logical_x + canvas_width / 2))
    canvas_logical_y=$((canvas_logical_y + canvas_height / 2))
    read -r guest_logical_width guest_logical_height physical_width physical_height < <(
        guest jq -r \
            '[.guest_logical_width, .guest_logical_height,
              .physical_width, .physical_height] | @tsv' \
            /run/wildbuzzard-display-state/output-state.json
    )
    canvas_x=$(((canvas_logical_x * physical_width + guest_logical_width / 2) / guest_logical_width))
    canvas_y=$(((canvas_logical_y * physical_height + guest_logical_height / 2) / guest_logical_height))
    assert_cua_ok click \
        "{\"scope\":\"desktop\",\"x\":$canvas_x,\"y\":$canvas_y,\"delivery_mode\":\"foreground\"}"
    deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)) &&
        ! guest grep -q 'ButtonPress event' /tmp/wildbuzzard-xev-canvas.log; do
        sleep 0.1
    done
    guest grep -q 'ButtonPress event' /tmp/wildbuzzard-xev-canvas.log
    guest pkill -x xev >/dev/null 2>&1 || true

    # Start real GLX and Vulkan workloads and require the broker to observe an
    # application-open selected render node rather than only Sway's renderer.
    guest_spawn glxgears
    guest_spawn vkcube --wsi wayland
    deadline=$((SECONDS + 20))
    while ((SECONDS < deadline)); do
        if [[ $(jq -r '.display.application_devices | length' "$runtime") -gt 0 ]]; then
            break
        fi
        sleep 1
    done
    [[ $(jq -r '.display.application_devices | length' "$runtime") -gt 0 ]]

    if guest test -e /dev/nvidiactl; then
        # The NVIDIA ICD is staged in ephemeral runtime state so the broker
        # never creates a mount placeholder in the persistent rootfs. Prove
        # that the session's additive Vulkan manifest exposes the selected
        # NVIDIA GPU while retaining the Mesa devices.
        guest test -s /run/wildbuzzard-host/driver/nvidia_icd.json
        guest env \
            VK_ADD_DRIVER_FILES=/run/wildbuzzard-host/driver/nvidia_icd.json \
            vulkaninfo --summary |
            grep -F 'deviceName' |
            grep -F 'NVIDIA' >/dev/null
        guest sh -c 'cat > /tmp/wildbuzzard-desktop-gpu-test.py <<'"'"'PY'"'"'
import ctypes
cuda = ctypes.CDLL("libcuda.so.1")
assert cuda.cuInit(0) == 0
count = ctypes.c_int()
assert cuda.cuDeviceGetCount(ctypes.byref(count)) == 0
assert count.value > 0
ctypes.CDLL("libnvidia-encode.so.1")
ctypes.CDLL("libnvcuvid.so.1")
with open("/tmp/wildbuzzard-desktop-gpu-test.ok", "w", encoding="utf-8") as result:
    result.write(str(count.value))
PY'
        guest rm -f /tmp/wildbuzzard-desktop-gpu-test.ok
        guest_spawn python3 /tmp/wildbuzzard-desktop-gpu-test.py
        deadline=$((SECONDS + 20))
        while ((SECONDS < deadline)) &&
            ! guest test -s /tmp/wildbuzzard-desktop-gpu-test.ok; do
            sleep 0.1
        done
        guest grep -Eq '^[1-9][0-9]*$' /tmp/wildbuzzard-desktop-gpu-test.ok
        guest rm -f /tmp/wildbuzzard-desktop-ffmpeg-encoders
        guest sh -c \
            'ffmpeg -hide_banner -encoders > /tmp/wildbuzzard-desktop-ffmpeg-encoders 2>&1 &'
        deadline=$((SECONDS + 20))
        while ((SECONDS < deadline)) &&
            ! guest test -s /tmp/wildbuzzard-desktop-ffmpeg-encoders; do
            sleep 0.1
        done
        guest grep nvenc /tmp/wildbuzzard-desktop-ffmpeg-encoders >/dev/null
        guest rm -f \
            /tmp/wildbuzzard-desktop-codec.log \
            /tmp/wildbuzzard-desktop-codec.mp4 \
            /tmp/wildbuzzard-desktop-codec.ok
        guest sh -c \
            'ffmpeg -hide_banner -loglevel error -f lavfi -i color=size=256x256:rate=1 -frames:v 1 -c:v h264_nvenc -y /tmp/wildbuzzard-desktop-codec.mp4 >>/tmp/wildbuzzard-desktop-codec.log 2>&1 && ffmpeg -hide_banner -loglevel error -hwaccel cuda -i /tmp/wildbuzzard-desktop-codec.mp4 -f null - >>/tmp/wildbuzzard-desktop-codec.log 2>&1 && touch /tmp/wildbuzzard-desktop-codec.ok &'
        deadline=$((SECONDS + 20))
        while ((SECONDS < deadline)) &&
            ! guest test -e /tmp/wildbuzzard-desktop-codec.ok; do
            sleep 0.1
        done
        guest test -s /tmp/wildbuzzard-desktop-codec.mp4
        guest test -e /tmp/wildbuzzard-desktop-codec.ok
    fi
fi

if [[ "$install_package" == 1 ]]; then
    guest sudo -n apt-get update
    guest sudo -n apt-get install --yes hello
fi

# Host-only launcher controls prove maximize/restore resize negotiation,
# native-resolution relayout, minimize request delivery, and the same orderly
# shutdown path used by an xdg_toplevel close event.
wb window "$machine" restore
wait_maximized false
wait_native_window_frame
wb window "$machine" maximize
wait_maximized true
wait_native_window_frame
wb window "$machine" restore
wait_maximized false
wait_native_window_frame
wb window "$machine" minimize
sleep 1
[[ $(jq -r '.state' "$runtime") == running ]]
[[ -d "$extract_lease_appdir" ]]
[[ -x "$extract_lease_appdir/usr/libexec/wildbuzzard/gst-launch-1.0" ]]
wb window "$machine" close
wait_stopped
wait_appdir_lease_released \
    "$extract_lease_broker_pid" \
    "$extract_lease_broker_start_time" \
    "$extract_lease_appdir"

# A full orderly close/start proves the same mutable rootfs and shared
# directory return.
wb start "$machine" --detach
wait_running
refresh_pid
[[ $(guest hostname) == "$machine" ]]
[[ -z "$(guest systemctl --failed --no-legend --plain --no-pager)" ]]
[[ $(guest cat /home/wildbuzzard/.wildbuzzard-persistence) == "$marker" ]]
[[ $(guest cat /shared/.wildbuzzard-acceptance) == "$marker" ]]
[[ $(guest cat /home/wildbuzzard/.config/wildbuzzard-acceptance.setting) == "$marker" ]]
guest grep -Fxq "$marker" \
    /home/wildbuzzard/.config/sway/wildbuzzard-acceptance.marker
guest grep -Fxq "# persistent guest OS edit: $marker" \
    /etc/wildbuzzard/sway-config
[[ $(guest /home/wildbuzzard/.local/bin/wildbuzzard-acceptance-agent) == "$marker" ]]
guest sh -c 'command -v wtype' >/dev/null
if [[ "$install_package" == 1 ]]; then
    guest dpkg-query -W hello >/dev/null
fi

# Move the complete stopped portable folder, boot the full machine from its new
# location without rewriting metadata, verify persistent state, then return it
# to the original path and boot it once more. This proves real portability,
# rather than merely testing path construction or listing copied metadata.
wb window "$machine" close
wait_stopped
machine_config_hash=$(sha256sum "$portable_dir/vm/$machine/machine.json" | cut -d' ' -f1)
appimage_name=$(basename -- "$appimage")
relocation_original=$portable_dir
relocation_target="${portable_dir}.wildbuzzard-relocation-$$"
[[ ! -e "$relocation_target" ]]
mv -- "$relocation_original" "$relocation_target"
relocation_active=1
portable_dir=$relocation_target
appimage="$portable_dir/$appimage_name"
runtime="$portable_dir/vm/$machine/runtime.json"

wb_without_host_path list | grep "^$machine"$'\t' >/dev/null
wb_without_host_path start "$machine" --detach
wait_running
refresh_pid
[[ $(guest cat /home/wildbuzzard/.wildbuzzard-persistence) == "$marker" ]]
[[ $(guest cat /shared/.wildbuzzard-acceptance) == "$marker" ]]
[[ $(guest /home/wildbuzzard/.local/bin/wildbuzzard-acceptance-agent) == "$marker" ]]
relocated_machine_config_hash=$(
    sha256sum "$portable_dir/vm/$machine/machine.json" | cut -d' ' -f1
)
[[ "$relocated_machine_config_hash" == "$machine_config_hash" ]]
wb status "$machine" |
    grep -Fx "rootfs: $portable_dir/vm/$machine/rootfs" >/dev/null
wb window "$machine" close
wait_stopped

mv -- "$relocation_target" "$relocation_original"
relocation_active=0
portable_dir=$relocation_original
appimage="$portable_dir/$appimage_name"
runtime="$portable_dir/vm/$machine/runtime.json"
wb start "$machine" --detach
wait_running
refresh_pid
[[ $(guest cat /home/wildbuzzard/.wildbuzzard-persistence) == "$marker" ]]
[[ $(guest /home/wildbuzzard/.local/bin/wildbuzzard-acceptance-agent) == "$marker" ]]

# `stop` must not return while its detached broker is still cleaning up; an
# immediate start is the regression test for that lifecycle boundary.
wb stop "$machine"
wb start "$machine" --detach
wait_running
refresh_pid
[[ $(guest cat /home/wildbuzzard/.wildbuzzard-persistence) == "$marker" ]]

# A guest-local poweroff stops Sway before namespace PID 1; the broker must
# recognize that orderly sequence rather than report the display disconnect as
# a crash.
guest sudo -n systemctl --no-block start poweroff.target
wait_stopped
wb start "$machine" --detach
wait_running
refresh_pid
[[ $(guest cat /home/wildbuzzard/.wildbuzzard-persistence) == "$marker" ]]

# Exercise the native fractional-scale bridge around unmodified Sway/wlroots
# without mutating the host monitor configuration. The test override replaces
# only the host's preferred-scale value; Sway still renders and submits the
# resulting dmabuf through the real host compositor.
wb stop "$machine"
WILDBUZZARD_TEST_FRACTIONAL_SCALE_120=180 \
    APPIMAGE_EXTRACT_AND_RUN=1 \
    "$appimage" start "$machine" --detach
wait_running
refresh_pid
wait_scaled_window_frame 180
guest pgrep -f '^/usr/bin/python3 /usr/libexec/wildbuzzard-output-sync$' >/dev/null
wait_sway_output_matches_runtime 180
guest grim -t ppm /tmp/wildbuzzard-fractional-scale.ppm
capture_dimensions=$(guest python3 -c \
    'with open("/tmp/wildbuzzard-fractional-scale.ppm", "rb") as stream:
         assert stream.readline().strip() == b"P6"
         print(stream.readline().decode("ascii").strip())')
[[ "$capture_dimensions" == \
    "$(jq -r '.display.presentation.width' "$runtime") $(jq -r '.display.presentation.height' "$runtime")" ]]
guest rm -f /tmp/wildbuzzard-fractional-scale.ppm
wb window "$machine" maximize
wait_maximized true
wait_scaled_window_frame 180
wb window "$machine" restore
wait_maximized false
wait_scaled_window_frame 180
wb stop "$machine"
wb start "$machine" --detach
wait_running
refresh_pid
wait_native_window_frame

rm -f -- "$portable_dir/shared/.wildbuzzard-acceptance"
rm -f -- "$portable_dir/shared/.wildbuzzard-guest-created"
rm -f -- "$portable_dir/shared/.wildbuzzard-guest-directory/host-file"
rmdir -- "$portable_dir/shared/.wildbuzzard-guest-directory"
echo "Wild Buzzard hardware acceptance passed for '$machine'"
