#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -Eeuo pipefail
trap 'rc=$?; echo "hardware acceptance failed at line $LINENO: $BASH_COMMAND" >&2; exit "$rc"' ERR

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
task_uid=$(id -u)
default_launcher=/usr/bin/buzzardos
launcher=${1:-${BUZZARDOS_LAUNCHER:-$default_launcher}}
machine=${2:-acceptance}
machine_dir=${3:-${BUZZARDOS_ACCEPT_MACHINE_DIR:-"${TMPDIR:-/tmp}/buzzardos-acceptance-$task_uid/$machine"}}
shared_dir=${BUZZARDOS_ACCEPT_SHARE_DIR:-"${TMPDIR:-/tmp}/buzzardos-acceptance-$task_uid/shared-$machine"}
machine_dir=$(readlink -m -- "$machine_dir")
shared_dir=$(readlink -m -- "$shared_dir")
install_package=${BUZZARDOS_ACCEPT_INSTALL_PACKAGE:-0}
full_matrix=${BUZZARDOS_ACCEPT_FULL_MATRIX:-0}
integration_acceptance=${BUZZARDOS_ACCEPT_INTEGRATIONS:-1}
accept_image=${BUZZARDOS_ACCEPT_IMAGE:-}
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
        "$launcher" --machine-dir "$relocation_target" \
            stop "$machine" >/dev/null 2>&1 || true
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
[[ -x "$launcher" ]] || {
    echo "installed Buzzard OS launcher is missing or not executable: $launcher" >&2
    exit 1
}

launcher=$(readlink -f -- "$launcher")
runtime="$machine_dir/runtime.json"
marker="buzzardos-acceptance-$(date +%s)-$$"

wb() {
    "$launcher" --machine-dir "$machine_dir" "$@"
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

wait_configured_initial_window_frame() {
    local configured_width=$1
    local configured_height=$2
    local deadline=$((SECONDS + 30))
    while ((SECONDS < deadline)); do
        if jq -e \
            --argjson configured_width "$configured_width" \
            --argjson configured_height "$configured_height" '
            def gcd(a; b):
                if b == 0 then a else gcd(b; a % b) end;
            def aligned_extent(extent; scale_120):
                (120 / gcd(scale_120; 120)) as $denominator |
                (((extent + $denominator - 1) / $denominator | floor) * $denominator);
            (aligned_extent($configured_width; .display.presentation.scale_120)) as $expected_width |
            (aligned_extent($configured_height; .display.presentation.scale_120)) as $expected_height |
            .display.presentation.scale_120 >= 120 and
            .display.window.width == $expected_width and
            .display.window.height == $expected_height and
            .display.presentation.viewport_width == $expected_width and
            .display.presentation.viewport_height == $expected_height and
            .display.presentation.width ==
                (($expected_width * .display.presentation.scale_120 + 119) / 120 | floor) and
            .display.presentation.height ==
                (($expected_height * .display.presentation.scale_120 + 119) / 120 | floor) and
            .display.presentation.native_resolution == true
        ' "$runtime" >/dev/null; then
            return
        fi
        sleep 0.1
    done
    echo "native monitor did not reach the pixel-aligned viewport for configured ${configured_width}x${configured_height}" >&2
    exit 1
}

wait_native_window_frame_after() {
    local previous=$1
    local deadline=$((SECONDS + 30))
    while ((SECONDS < deadline)); do
        if jq -e --argjson previous "$previous" '
            .display.presentation.native_resolution == true and
            .display.presentation.scale_120 >= 120 and
            .display.presentation.width ==
                ((.display.window.width * .display.presentation.scale_120 + 119) / 120 | floor) and
            .display.presentation.height ==
                ((.display.window.height * .display.presentation.scale_120 + 119) / 120 | floor) and
            .display.presentation.viewport_width == .display.window.width and
            .display.presentation.viewport_height == .display.window.height and
            .display.presentation.submitted_frames > $previous.submitted_frames and
            .display.presentation.painted_frames > $previous.painted_frames
        ' "$runtime" >/dev/null; then
            return
        fi
        sleep 0.25
    done
    echo "guest output did not submit and paint a native frame after Sway reload" >&2
    exit 1
}

wait_sway_config_contains() {
    local expected=$1
    local deadline=$((SECONDS + 15))
    local current_config
    while ((SECONDS < deadline)); do
        if current_config=$(guest swaymsg -r -t get_config 2>/dev/null) &&
            jq -e --arg expected "$expected" \
                '.config | contains($expected)' \
                <<<"$current_config" >/dev/null; then
            return
        fi
        sleep 0.1
    done
    echo "Sway did not finish loading the requested configuration" >&2
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
    local expected output_state outputs
    while ((SECONDS < deadline)); do
        # The guest scale can be configured independently of the host surface
        # scale. Read the display gateway's atomic guest-output record rather
        # than inferring guest logical dimensions from host-window diagnostics.
        output_state=$(guest cat \
            /run/buzzardos-display-state/output-state.json)
        expected=$(jq -c '{
            guest_ui_scale_120,
            logical_width,
            logical_height,
            physical_width,
            physical_height
        }' <<<"$output_state")
        if [[ -n "$required_scale_120" ]] &&
            [[ $(jq -r '.guest_ui_scale_120' <<<"$expected") != "$required_scale_120" ]]; then
            sleep 0.1
            continue
        fi
        outputs=$(guest wlr-randr --json)
        if jq -e --argjson expected "$expected" '
            any(.[];
                .enabled == true and
                # wlr-output-management represents scale as wl_fixed (24.8),
                # so recurring fractions such as 4/3 are rounded to 1/256.
                (((.scale * 120) - $expected.guest_ui_scale_120) | fabs) < 0.2 and
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
        HOME=/home/buzzard \
        USER=buzzard \
        LOGNAME=buzzard \
        PATH=/opt/buzzardos/runtime/current/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
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
        sh -c '
            session_pid=
            WAYLAND_DISPLAY=
            SWAYSOCK=
            shell_observed=0
            attempt=0
            while [ "$attempt" -lt 150 ]; do
                candidate=$(pgrep -xo buzzardos-she 2>/dev/null || true)
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
        ' sh "$@"
}

guest_spawn() {
    guest sh -c \
        'exec setsid --fork "$@" </dev/null >/tmp/buzzardos-acceptance-app.log 2>&1' \
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
            /home/buzzard/.mount_* | /shared/.mount_*) ;;
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
                    decoration: {
                        x: ($workspace.rect.x + $window.deco_rect.x),
                        y: ($workspace.rect.y + $window.deco_rect.y),
                        width: $window.deco_rect.width,
                        height: $window.deco_rect.height
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

titlebar_drag_for_pid() {
    local pid=$1
    local sway_state output_state
    sway_state=$(sway_window_state_for_pid "$pid")
    output_state=$(guest cat \
        /run/buzzardos-display-state/output-state.json)
    jq -ce -n \
        --argjson state "$sway_state" \
        --argjson output "$output_state" '
            def physical_x($logical):
                (($logical * $output.physical_width /
                    $output.logical_width) | round);
            def physical_y($logical):
                (($logical * $output.physical_height /
                    $output.logical_height) | round);

            ($state.decoration) as $decoration |
            ($state.border_width) as $border |
            if ($output.logical_width <= 0 or
                $output.logical_height <= 0 or
                $output.physical_width <= 0 or
                $output.physical_height <= 0) then
                error("guest output has no usable logical/physical mapping")
            elif ($border <= 0 or
                  $decoration.width <= (2 * $border) or
                  $decoration.height <= $border) then
                error("Sway decoration has no titlebar interior outside its resize border")
            else
                ($decoration.width - (2 * $border)) as $interior_width |
                ($decoration.height - $border) as $interior_height |
                # Sway current_border_width is the authoritative edge-resize
                # hit region along the top and sides. Aim halfway between the
                # bottom of that top hit region and the decoration bottom,
                # then map the logical point into the CUA canonical physical
                # output space. This remains a titlebar drag at every scale.
                ($decoration.x + $border + ($interior_width / 2 | floor)) as $from_x |
                ($decoration.y + $border + ($interior_height / 2 | floor)) as $from_y |
                # Derive a visible move from the same live decoration instead
                # of coupling the test to one package-time pixel height.
                ($decoration.height + $border) as $delta_x |
                ([($decoration.height - $border), (2 * $border)] | max) as $delta_y |
                {
                    scope: "desktop",
                    from_x: physical_x($from_x),
                    from_y: physical_y($from_y),
                    to_x: physical_x($from_x + $delta_x),
                    to_y: physical_y($from_y + $delta_y),
                    duration_ms: 220,
                    steps: 14
                }
            end
        '
}

drag_guest_frame_edge() {
    local pid=$1
    local edge=$2
    local before points after
    # Give every edge/corner an identical centered baseline. Cumulative
    # outward resizes eventually hit an output boundary, where Sway correctly
    # clamps motion and a later corner can no longer exercise both axes.
    guest swaymsg -r "[pid=$pid] move position 320 180" |
        jq -e 'all(.[]; .success)' >/dev/null
    guest swaymsg -r "[pid=$pid] resize set width 640 px height 420 px" |
        jq -e 'all(.[]; .success)' >/dev/null
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
    if ! jq -e -n --arg edge "$edge" --argjson before "$before" --argjson after "$after" '
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
    ' >/dev/null; then
        printf 'guest frame resize failed: edge=%s before=%s after=%s\n' \
            "$edge" "$before" "$after" >&2
        return 1
    fi
}

wb doctor
if [[ ! -f "$machine_dir/machine.json" ]]; then
    [[ -n "$accept_image" ]] || {
        echo "BUZZARDOS_ACCEPT_IMAGE is required to create the acceptance machine" >&2
        exit 1
    }
    mkdir -p -- "$shared_dir"
    create_arguments=(create "$machine" --gpu all --image "$accept_image" --share "$shared_dir")
    wb "${create_arguments[@]}"
fi
[[ -d "$shared_dir" ]]
jq -e --arg shared "$shared_dir" \
    'any(.shares[]; .host_path == $shared)' \
    "$machine_dir/machine.json" >/dev/null
configured_width=$(jq -er '.width' "$machine_dir/machine.json")
configured_height=$(jq -er '.height' "$machine_dir/machine.json")
# Stop deliberately preserves the native host window and its supervising
# broker. Close any live supervisor instead, including one already in Stopped
# state, so this run unequivocally exercises the supplied installed package
# rather than reusing an older build's process.
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
wait_configured_initial_window_frame "$configured_width" "$configured_height"

# The installed launcher returns after readiness. Stop keeps the same native
# host window and supervising broker alive; a later Start must reuse that
# process and the dpkg-owned helper payload.
package_broker_pid=$(jq -er '.launcher_pid' "$runtime")
package_broker_start_time=$(process_start_time "$package_broker_pid")
wb stop "$machine"
wait_stopped
[[ $(process_start_time "$package_broker_pid") == \
    "$package_broker_start_time" ]]
[[ -x /usr/libexec/buzzardos/buzzardos-broker ]]
wb start "$machine" --detach
wait_running
refresh_pid
[[ $(jq -er '.launcher_pid' "$runtime") == "$package_broker_pid" ]]
[[ $(process_start_time "$package_broker_pid") == \
    "$package_broker_start_time" ]]
wait_native_window_frame

# Exercise live TCP/UDP mappings in both directions and all three separately
# authorized media bridges against this already-running namespace. The helper
# snapshots and restores machine.json and asserts that the container PID never
# changes, so this also guards the no-restart live-reconciliation contract.
if [[ "$integration_acceptance" == 1 ]]; then
    "$project_dir/tests/acceptance/integration-acceptance.sh" \
        "$launcher" "$machine" "$machine_dir"
    refresh_pid
fi

# Namespace, PID 1, installed-package layout, private network, and explicit data share.
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
loopback_probe=$(mktemp "$machine_dir/cache/loopback-probe.XXXXXX")
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

[[ $(guest stat -c %a /run/buzzardos-host/wayland-0) == 0 ]]
! guest test -e /run/buzzardos-host/window-control
! test -e "$machine_dir/.window-control.sock"
! guest python3 -c \
    'import socket; client = socket.socket(socket.AF_UNIX); client.connect("/run/buzzardos-host/wayland-0")' \
    2>/dev/null
! guest test -e /run/user/1000/wayland-0
! guest test -S /run/docker.sock
! guest test -S /var/run/docker.sock
[[ $(guest sudo -n id -u) == 0 ]]
rootfs_host_uid=$(stat -c %u "$machine_dir/rootfs")
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
# Only explicitly authorized shares may persist host filesystem paths in the
# otherwise destination-independent machine metadata.
jq -e --arg shared "$shared_dir" '
    [.shares[].host_path] == [$shared] and
    ([.. | strings | select(startswith("/") and . != $shared)] | length) == 0
' "$machine_dir/machine.json" >/dev/null

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
    grep -Fx 'LD_LIBRARY_PATH=/run/buzzardos-host/driver/lib' \
        <<<"$session_environment" >/dev/null
    grep -E '^WLR_RENDER_DRM_DEVICE=/dev/dri/renderD[0-9]+$' \
        <<<"$session_environment" >/dev/null
fi

printf '%s\n' "$marker" >"$shared_dir/.buzzardos-acceptance"
[[ $(guest cat /shared/.buzzardos-acceptance) == "$marker" ]]
guest sh -c 'printf "%s\n" "$1" > /shared/.buzzardos-guest-created' sh "$marker"
[[ $(stat -c %u "$shared_dir/.buzzardos-guest-created") == "$(id -u)" ]]
[[ $(stat -c %g "$shared_dir/.buzzardos-guest-created") == "$(id -g)" ]]
printf '%s-host-edit\n' "$marker" >"$shared_dir/.buzzardos-guest-created"
[[ $(guest cat /shared/.buzzardos-guest-created) == "$marker-host-edit" ]]
guest mkdir -p /shared/.buzzardos-guest-directory
printf '%s-host-created\n' "$marker" \
    >"$shared_dir/.buzzardos-guest-directory/host-file"
[[ $(guest cat /shared/.buzzardos-guest-directory/host-file) == "$marker-host-created" ]]
guest sh -c "printf '%s\\n' '$marker' > /home/buzzard/.buzzardos-persistence"
guest sh -c "printf '%s\\n' '$marker' > /home/buzzard/.config/buzzardos-acceptance.setting"
guest install -d -m 0700 /home/buzzard/.config/sway
guest sh -c 'printf "%s\n" "$1" \
    > /home/buzzard/.config/sway/buzzardos-acceptance.marker' \
    sh "$marker"
# Integration assets are installed when the machine is created, but normal
# starts must not silently restore them over guest-root changes. This harmless
# comment makes that durable-rootfs invariant observable across the restart
# below.
guest sudo -n sh -c \
    'printf "%s\n" "# persistent guest OS edit: $1" >> /etc/buzzardos/sway-config' \
    sh "$marker"
compositor_start_time=$(guest awk '{print $22}' "/proc/$compositor_pid/stat")
reload_output_state=$(guest cat \
    /run/buzzardos-display-state/output-state.json)
reload_output_before=$(jq -ce '{
    host_surface_scale_120,
    guest_ui_scale_120,
    logical_width,
    logical_height,
    physical_width,
    physical_height,
    geometry_generation
}' <<<"$reload_output_state")
reload_frame_counters=$(jq -ce '{
    submitted_frames: .display.presentation.submitted_frames,
    painted_frames: .display.presentation.painted_frames
}' "$runtime")
reload_result=$(guest swaymsg -r reload)
jq -e '
    type == "array" and
    length == 1 and
    .[0].success == true
' <<<"$reload_result" >/dev/null
wait_sway_config_contains "# persistent guest OS edit: $marker"
[[ $(guest pgrep -xo sway) == "$compositor_pid" ]]
[[ $(guest awk '{print $22}' "/proc/$compositor_pid/stat") == \
    "$compositor_start_time" ]]
wait_sway_output_matches_runtime
reload_output_state=$(guest cat \
    /run/buzzardos-display-state/output-state.json)
reload_output_after=$(jq -ce '{
    host_surface_scale_120,
    guest_ui_scale_120,
    logical_width,
    logical_height,
    physical_width,
    physical_height,
    geometry_generation
}' <<<"$reload_output_state")
[[ "$reload_output_after" == "$reload_output_before" ]]
wait_native_window_frame_after "$reload_frame_counters"
[[ $(guest pgrep -xo sway) == "$compositor_pid" ]]
[[ $(guest awk '{print $22}' "/proc/$compositor_pid/stat") == \
    "$compositor_start_time" ]]
wait_cua_capture_matches_runtime
guest install -d -m 0700 /home/buzzard/.local/bin
guest sh -c 'cat > /home/buzzard/.local/bin/buzzardos-acceptance-agent' <<'AGENT'
#!/bin/sh
set -eu
capture=${XDG_RUNTIME_DIR:-/run/user/1000}/buzzardos-arbitrary-agent.png
grim "$capture"
test -s "$capture"
python3 -c 'import pyatspi; assert pyatspi.Registry.getDesktopCount() > 0; assert pyatspi.Registry.getDesktop(0).childCount > 0'
cat /home/buzzard/.buzzardos-persistence
AGENT
guest chmod 0700 /home/buzzard/.local/bin/buzzardos-acceptance-agent
[[ $(guest /home/buzzard/.local/bin/buzzardos-acceptance-agent) == "$marker" ]]

# The reference image uses the distribution's stock Sway and matching wlroots
# dependency. Keep application compatibility in the runtime, but do not
# confuse full-matrix test fixtures with applications shipped by the image.
guest dpkg-query -W sway >/dev/null
[[ $(guest sh -c 'command -v sway') == \
    /usr/bin/sway ]]
guest sway --version 2>&1 | grep -E '^sway version [0-9]+' >/dev/null
guest test -f /usr/share/doc/sway/copyright
for required_command in \
    ffmpeg firefox-esr foot mousepad sway thunar wtype Xwayland; do
    guest sh -c 'command -v "$1"' sh "$required_command" >/dev/null
done
for required_package in \
    ffmpeg firefox-esr foot fuse3 libfuse2t64 mesa-vulkan-drivers mousepad \
    pipewire pipewire-pulse thunar wireplumber xwayland; do
    [[ $(guest dpkg-query -W -f='${db:Status-Status}' "$required_package") == \
        installed ]]
done
for forbidden in \
    blender chromium dolphin gcc glxgears kwin_wayland labwc make \
    pavucontrol uxterm vkcube vulkaninfo wayfire waybar fuzzel \
    buzzardos-electron-demo xeyes xterm; do
    ! guest sh -c 'command -v "$1"' sh "$forbidden" >/dev/null 2>&1
done
for forbidden_package in \
    blender chromium dolphin kwin-wayland labwc mesa-utils pavucontrol \
    plasma-workspace vulkan-tools wayfire waybar fuzzel x11-apps xterm; do
    ! guest dpkg-query -W "$forbidden_package" >/dev/null 2>&1
done
for forbidden_desktop_entry in \
    /usr/share/applications/chromium.desktop \
    /usr/share/applications/org.kde.dolphin.desktop \
    /usr/share/applications/pavucontrol.desktop \
    /usr/share/applications/buzzardos-electron-demo.desktop \
    /usr/share/applications/debian-uxterm.desktop \
    /usr/share/applications/debian-xterm.desktop; do
    ! guest test -e "$forbidden_desktop_entry"
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
guest pgrep -x buzzardos-she >/dev/null
guest pgrep -x mako >/dev/null
guest pgrep -x pipewire >/dev/null
guest pgrep -x pipewire-pulse >/dev/null
guest pgrep -x wireplumber >/dev/null
guest pgrep -f \
    '^/usr/bin/python3 /opt/buzzardos/runtime/current/libexec/buzzardos-output-sync$' \
    >/dev/null
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
assert "Buzzard OS Desktop" in labels
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
    "Firefox ESR",
    "Foot",
    "Mousepad",
    "Thunar File Manager",
    "Shut Down Machine",
]:
    assert expected in labels
for forbidden in [
    "Chromium Web Browser",
    "Dolphin",
    "PulseAudio Volume Control",
    "UXTerm",
    "Volume Control",
    "Buzzard OS Electron",
    "XTerm",
]:
    assert forbidden not in labels

# Return the human-facing menu to its original closed state.
button = next(node for node in nodes if node.name == "Applications")
actions = button.queryAction()
action_names = [actions.getName(index) for index in range(actions.nActions)]
assert actions.doAction(action_names.index("click"))
PY
! guest sh -c 'command -v buzzardos-window-control' >/dev/null

# The native Rust shell is functional, not merely installed, and advertises
# semantic AT-SPI actions while a D-Bus notification reaches mako.
guest test -x /opt/buzzardos/runtime/current/libexec/buzzardos-shell
guest notify-send --app-name=buzzardos-acceptance \
    "Buzzard OS acceptance" "Notification is visible"
deadline=$((SECONDS + 10))
while ((SECONDS < deadline)) &&
    ! guest makoctl list | grep -q "Buzzard OS acceptance"; do
    sleep 0.1
done
guest makoctl list | grep -q "Buzzard OS acceptance"
guest makoctl dismiss --all

for process_name in foot thunar; do
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
    /etc/buzzardos/sway-config
guest grep -Fxq 'show_marks no' /etc/buzzardos/sway-config
guest swaymsg -r -t get_tree | jq -e --argjson pid "$thunar_pid" '
    .. | objects |
    select(.pid? == $pid) |
    .floating != "auto_off" and .floating != "user_off" and
    .border == "normal" and .deco_rect.height > 0
' >/dev/null
thunar_before_drag=$(window_frame_for_pid "$thunar_pid")
titlebar_drag=$(titlebar_drag_for_pid "$thunar_pid")
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

# Exercise the guest-private CUA keyboard without changing host focus or using
# a host RemoteDesktop/input-injection API. Physical host-keyboard coexistence
# is deliberately a manual observation: automated acceptance must never seize,
# focus, type on, or otherwise interfere with the operator's host keyboard.
guest sh -c 'printf "%s\n" "#!/bin/bash" "IFS= read -e -r cua_value" \
    "printf \"%s\" \"\$cua_value\" > /home/buzzard/.buzzardos-cua-input" \
    "sleep 10" > /tmp/buzzardos-input-test; chmod 700 /tmp/buzzardos-input-test'
guest rm -f /home/buzzard/.buzzardos-cua-input
guest_spawn foot --app-id buzzardos-acceptance /tmp/buzzardos-input-test
wait_for_window buzzardos-acceptance >/dev/null
cua_keyboard_session="buzzardos-keyboard-$marker"
assert_cua_ok start_session \
    "{\"session\":\"$cua_keyboard_session\",\"capture_scope\":\"desktop\"}"
assert_cua_ok hotkey \
    "{\"session\":\"$cua_keyboard_session\",\"scope\":\"desktop\",\"keys\":[\"ctrl\",\"l\"],\"delivery_mode\":\"foreground\"}"
assert_cua_ok type_text \
    "{\"session\":\"$cua_keyboard_session\",\"scope\":\"desktop\",\"text\":\"$marker\",\"delivery_mode\":\"foreground\"}"
assert_cua_ok press_key \
    "{\"session\":\"$cua_keyboard_session\",\"scope\":\"desktop\",\"key\":\"backspace\",\"delivery_mode\":\"foreground\"}"
assert_cua_ok type_text \
    "{\"session\":\"$cua_keyboard_session\",\"scope\":\"desktop\",\"text\":\"z\",\"delivery_mode\":\"foreground\"}"
assert_cua_ok press_key \
    "{\"session\":\"$cua_keyboard_session\",\"scope\":\"desktop\",\"key\":\"enter\",\"delivery_mode\":\"foreground\"}"
deadline=$((SECONDS + 5))
while ((SECONDS < deadline)) &&
    ! guest test -e /home/buzzard/.buzzardos-cua-input; do
    sleep 0.1
done
[[ $(guest cat /home/buzzard/.buzzardos-cua-input) == "${marker%?}z" ]]
assert_cua_ok end_session \
    "{\"session\":\"$cua_keyboard_session\"}"
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
thunar_output_state=$(guest cat \
    /run/buzzardos-display-state/output-state.json)
guest_logical_width=$(jq -er '.logical_width' <<<"$thunar_output_state")
guest_logical_height=$(jq -er '.logical_height' <<<"$thunar_output_state")
jq -e -n \
    --argjson state "$thunar_maximized" \
    --argjson width "$guest_logical_width" \
    --argjson height "$guest_logical_height" '
        $state.frame == $state.workspace and
        $state.workspace == {x:0, y:0, width:$width, height:($height - 42)} and
        $state.fullscreen_mode == 0 and
        any($state.marks[]; startswith("__buzzardos_restore_v1_"))
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
shell = next(node for node in walk(desktop) if node.name == "Buzzard OS Desktop")
button = next(node for node in walk(shell) if node.name == target)
extents = button.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
print(extents.x + extents.width // 2, extents.y + extents.height // 2)
PY
)
task_output_state=$(guest cat \
    /run/buzzardos-display-state/output-state.json)
task_output_dimensions=$(jq -er \
    '[.logical_width, .logical_height,
      .physical_width, .physical_height] | @tsv' \
    <<<"$task_output_state")
read -r logical_width logical_height physical_width physical_height \
    <<<"$task_output_dimensions"
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
    shell = next(node for node in walk(desktop) if node.name == "Buzzard OS Desktop")
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
        any(.marks[]; startswith("__buzzardos_restore_v1_"))' \
        <<<"$thunar_maximized" >/dev/null; then
        break
    fi
    sleep 0.05
done
jq -e '.frame == .workspace and
    any(.marks[]; startswith("__buzzardos_restore_v1_"))' \
    <<<"$thunar_maximized" >/dev/null

assert_cua_confirmed minimize_window \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window}"
thunar_minimized=$(sway_window_state_for_pid "$thunar_pid")
jq -e '
    .workspace_name == "__i3_scratch" and
    .scratchpad_state == "fresh" and
    any(.marks[]; startswith("__buzzardos_restore_v1_"))
' <<<"$thunar_minimized" >/dev/null
assert_cua_confirmed restore_window \
    "{\"pid\":$thunar_pid,\"window_id\":$thunar_window}"
thunar_restored=$(sway_window_state_for_pid "$thunar_pid")
jq -e -n --argjson before "$thunar_resized_normal" --argjson after "$thunar_restored" '
    $after.frame == $before.frame and
    $after.workspace_name != "__i3_scratch" and
    ($after.marks | map(select(startswith("__buzzardos_restore_v1_"))) | length) == 0
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
    # These are dedicated acceptance-machine fixtures, not reference-image
    # contents.  Install them only after the baseline absence checks above so
    # a passing full-matrix run cannot silently claim that they ship in the
    # OCI.  The acceptance machine is persistent by design and disposable;
    # use a newly created machine for each clean-reference certification.
    guest sudo -n apt-get update
    guest sudo -n env DEBIAN_FRONTEND=noninteractive \
        apt-get install --yes --no-install-recommends \
        dolphin mesa-utils vulkan-tools x11-apps x11-utils

    # Representative native Wayland Qt/KDE plus an external vendor Electron
    # AppImage and legacy Xwayland clients must all remain inside Sway's one
    # output and publish their normal accessibility objects where supported.
    for guest_command in dolphin glxgears vkcube vulkaninfo xev xeyes; do
        guest sh -c 'command -v "$1"' sh "$guest_command" >/dev/null
    done
    guest pkill -x dolphin >/dev/null 2>&1 || true
    # Start from a known non-Home location so the semantic action below must
    # produce an observable navigation, rather than passing because Dolphin
    # restored an already-Home session.
    guest_spawn dolphin /home/buzzard/Downloads
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
    electron_appimage=${BUZZARDOS_ELECTRON_APPIMAGE:-}
    [[ -f "$electron_appimage" ]] || {
        echo "full matrix requires BUZZARDOS_ELECTRON_APPIMAGE" >&2
        exit 1
    }
    electron_name="buzzardos-electron-acceptance-$$.AppImage"
    electron_log_name="buzzardos-electron-acceptance-$$.log"
    electron_acceptance_host_path="$shared_dir/$electron_name"
    electron_acceptance_log_path="$shared_dir/$electron_log_name"
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
    jq -e '.shell == "xwayland" and .border == "normal" and .border_width == 8' \
        <<<"$(sway_window_state_for_pid "$xeyes_pid")" >/dev/null
    xeyes_before_drag=$(window_frame_for_pid "$xeyes_pid")
    assert_cua_ok drag "$(titlebar_drag_for_pid "$xeyes_pid")"
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
    guest sh -c 'cat > /tmp/buzzardos-xev-canvas' <<'XEV'
#!/bin/sh
exec xev -event mouse >/tmp/buzzardos-xev-canvas.log 2>&1
XEV
    guest chmod 0700 /tmp/buzzardos-xev-canvas
    guest rm -f /tmp/buzzardos-xev-canvas.log
    guest_spawn /tmp/buzzardos-xev-canvas
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
    canvas_output_state=$(guest cat \
        /run/buzzardos-display-state/output-state.json)
    canvas_output_dimensions=$(jq -er \
        '[.logical_width, .logical_height,
          .physical_width, .physical_height] | @tsv' \
        <<<"$canvas_output_state")
    read -r guest_logical_width guest_logical_height physical_width physical_height \
        <<<"$canvas_output_dimensions"
    canvas_x=$(((canvas_logical_x * physical_width + guest_logical_width / 2) / guest_logical_width))
    canvas_y=$(((canvas_logical_y * physical_height + guest_logical_height / 2) / guest_logical_height))
    assert_cua_ok click \
        "{\"scope\":\"desktop\",\"x\":$canvas_x,\"y\":$canvas_y,\"delivery_mode\":\"foreground\"}"
    deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)) &&
        ! guest grep -q 'ButtonPress event' /tmp/buzzardos-xev-canvas.log; do
        sleep 0.1
    done
    guest grep -q 'ButtonPress event' /tmp/buzzardos-xev-canvas.log
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
        guest test -s /run/buzzardos-host/driver/nvidia_icd.json
        guest env \
            VK_ADD_DRIVER_FILES=/run/buzzardos-host/driver/nvidia_icd.json \
            vulkaninfo --summary |
            grep -F 'deviceName' |
            grep -F 'NVIDIA' >/dev/null
        guest sh -c 'cat > /tmp/buzzardos-desktop-gpu-test.py <<'"'"'PY'"'"'
import ctypes
cuda = ctypes.CDLL("libcuda.so.1")
assert cuda.cuInit(0) == 0
count = ctypes.c_int()
assert cuda.cuDeviceGetCount(ctypes.byref(count)) == 0
assert count.value > 0
ctypes.CDLL("libnvidia-encode.so.1")
ctypes.CDLL("libnvcuvid.so.1")
with open("/tmp/buzzardos-desktop-gpu-test.ok", "w", encoding="utf-8") as result:
    result.write(str(count.value))
PY'
        guest rm -f /tmp/buzzardos-desktop-gpu-test.ok
        guest_spawn python3 /tmp/buzzardos-desktop-gpu-test.py
        deadline=$((SECONDS + 20))
        while ((SECONDS < deadline)) &&
            ! guest test -s /tmp/buzzardos-desktop-gpu-test.ok; do
            sleep 0.1
        done
        guest grep -Eq '^[1-9][0-9]*$' /tmp/buzzardos-desktop-gpu-test.ok
        guest rm -f /tmp/buzzardos-desktop-ffmpeg-encoders
        guest sh -c \
            'ffmpeg -hide_banner -encoders > /tmp/buzzardos-desktop-ffmpeg-encoders 2>&1 &'
        deadline=$((SECONDS + 20))
        while ((SECONDS < deadline)) &&
            ! guest test -s /tmp/buzzardos-desktop-ffmpeg-encoders; do
            sleep 0.1
        done
        guest grep nvenc /tmp/buzzardos-desktop-ffmpeg-encoders >/dev/null
        guest rm -f \
            /tmp/buzzardos-desktop-codec.log \
            /tmp/buzzardos-desktop-codec.mp4 \
            /tmp/buzzardos-desktop-codec.ok
        guest sh -c \
            'ffmpeg -hide_banner -loglevel error -f lavfi -i color=size=256x256:rate=1 -frames:v 1 -c:v h264_nvenc -y /tmp/buzzardos-desktop-codec.mp4 >>/tmp/buzzardos-desktop-codec.log 2>&1 && ffmpeg -hide_banner -loglevel error -hwaccel cuda -i /tmp/buzzardos-desktop-codec.mp4 -f null - >>/tmp/buzzardos-desktop-codec.log 2>&1 && touch /tmp/buzzardos-desktop-codec.ok &'
        deadline=$((SECONDS + 20))
        while ((SECONDS < deadline)) &&
            ! guest test -e /tmp/buzzardos-desktop-codec.ok; do
            sleep 0.1
        done
        guest test -s /tmp/buzzardos-desktop-codec.mp4
        guest test -e /tmp/buzzardos-desktop-codec.ok
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
[[ -x /usr/libexec/buzzardos/buzzardos-broker ]]
wb window "$machine" close
wait_stopped
wait_process_identity_gone "$package_broker_pid" "$package_broker_start_time"

# A full orderly close/start proves the same mutable rootfs and shared
# directory return.
wb start "$machine" --detach
wait_running
refresh_pid
[[ $(guest hostname) == "$machine" ]]
[[ -z "$(guest systemctl --failed --no-legend --plain --no-pager)" ]]
[[ $(guest cat /home/buzzard/.buzzardos-persistence) == "$marker" ]]
[[ $(guest cat /shared/.buzzardos-acceptance) == "$marker" ]]
[[ $(guest cat /home/buzzard/.config/buzzardos-acceptance.setting) == "$marker" ]]
guest grep -Fxq "$marker" \
    /home/buzzard/.config/sway/buzzardos-acceptance.marker
guest grep -Fxq "# persistent guest OS edit: $marker" \
    /etc/buzzardos/sway-config
[[ $(guest /home/buzzard/.local/bin/buzzardos-acceptance-agent) == "$marker" ]]
guest sh -c 'command -v wtype' >/dev/null
if [[ "$install_package" == 1 ]]; then
    guest dpkg-query -W hello >/dev/null
fi

# Move the complete stopped machine directory, boot it from its new location
# through the explicit recovery override, verify persistent state, then return
# it to the registered path and boot it once more. The dpkg-owned application
# stays installed while the self-describing mutable machine moves independently.
relocation_outbound_broker_pid=$(jq -er '.launcher_pid' "$runtime")
relocation_outbound_broker_start_time=$(
    process_start_time "$relocation_outbound_broker_pid"
)
wb window "$machine" close
wait_stopped
wait_process_identity_gone \
    "$relocation_outbound_broker_pid" "$relocation_outbound_broker_start_time"
machine_config_hash=$(sha256sum "$machine_dir/machine.json" | cut -d' ' -f1)
relocation_original=$machine_dir
relocation_target="${machine_dir}.buzzardos-relocation-$$"
[[ ! -e "$relocation_target" ]]
mv -- "$relocation_original" "$relocation_target"
relocation_active=1
machine_dir=$relocation_target
runtime="$machine_dir/runtime.json"

wb status "$machine" | grep -Fx "rootfs: $machine_dir/rootfs" >/dev/null
wb start "$machine" --detach
wait_running
refresh_pid
[[ $(guest cat /home/buzzard/.buzzardos-persistence) == "$marker" ]]
[[ $(guest cat /shared/.buzzardos-acceptance) == "$marker" ]]
[[ $(guest /home/buzzard/.local/bin/buzzardos-acceptance-agent) == "$marker" ]]
relocated_machine_config_hash=$(
    sha256sum "$machine_dir/machine.json" | cut -d' ' -f1
)
[[ "$relocated_machine_config_hash" == "$machine_config_hash" ]]
wb status "$machine" |
    grep -Fx "rootfs: $machine_dir/rootfs" >/dev/null
relocation_return_broker_pid=$(jq -er '.launcher_pid' "$runtime")
relocation_return_broker_start_time=$(
    process_start_time "$relocation_return_broker_pid"
)
wb window "$machine" close
wait_stopped
wait_process_identity_gone \
    "$relocation_return_broker_pid" "$relocation_return_broker_start_time"

mv -- "$relocation_target" "$relocation_original"
relocation_active=0
machine_dir=$relocation_original
runtime="$machine_dir/runtime.json"
wb start "$machine" --detach
wait_running
refresh_pid
[[ $(guest cat /home/buzzard/.buzzardos-persistence) == "$marker" ]]
[[ $(guest /home/buzzard/.local/bin/buzzardos-acceptance-agent) == "$marker" ]]

# `stop` must not return while its detached broker is still cleaning up; an
# immediate start is the regression test for that lifecycle boundary.
wb stop "$machine"
wb start "$machine" --detach
wait_running
refresh_pid
[[ $(guest cat /home/buzzard/.buzzardos-persistence) == "$marker" ]]

# A guest-local poweroff stops Sway before namespace PID 1; the broker must
# recognize that orderly sequence rather than report the display disconnect as
# a crash.
guest sudo -n systemctl --no-block start poweroff.target
wait_stopped
wb start "$machine" --detach
wait_running
refresh_pid
[[ $(guest cat /home/buzzard/.buzzardos-persistence) == "$marker" ]]

# Exercise the native fractional-scale bridge around unmodified Sway/wlroots
# without mutating the host monitor configuration. The test override replaces
# only the host's preferred-scale value; Sway still renders and submits the
# resulting dmabuf through the real host compositor. The override is consumed
# only when the display process starts, while Stop deliberately keeps that
# process alive. Close the normal display so this start cannot silently reuse
# a process that never received the override.
fractional_baseline_broker_pid=$(jq -er '.launcher_pid' "$runtime")
fractional_baseline_broker_start_time=$(
    process_start_time "$fractional_baseline_broker_pid"
)
wb window "$machine" close
wait_stopped
wait_process_identity_gone \
    "$fractional_baseline_broker_pid" "$fractional_baseline_broker_start_time"
BUZZARDOS_TEST_FRACTIONAL_SCALE_120=180 \
    "$launcher" start "$machine" --detach
wait_running
refresh_pid
fractional_override_broker_pid=$(jq -er '.launcher_pid' "$runtime")
fractional_override_broker_start_time=$(
    process_start_time "$fractional_override_broker_pid"
)
wait_scaled_window_frame 180
guest pgrep -f \
    '^/usr/bin/python3 /opt/buzzardos/runtime/current/libexec/buzzardos-output-sync$' \
    >/dev/null
wait_sway_output_matches_runtime 180
guest grim -t ppm /tmp/buzzardos-fractional-scale.ppm
capture_dimensions=$(guest python3 -c \
    'with open("/tmp/buzzardos-fractional-scale.ppm", "rb") as stream:
         assert stream.readline().strip() == b"P6"
         print(stream.readline().decode("ascii").strip())')
[[ "$capture_dimensions" == \
    "$(jq -r '.display.presentation.width' "$runtime") $(jq -r '.display.presentation.height' "$runtime")" ]]
guest rm -f /tmp/buzzardos-fractional-scale.ppm
wb window "$machine" maximize
wait_maximized true
wait_scaled_window_frame 180
wb window "$machine" restore
wait_maximized false
wait_scaled_window_frame 180
# Close the overridden display before starting normally; otherwise Stop would
# preserve the startup-only test setting.
wb window "$machine" close
wait_stopped
wait_process_identity_gone \
    "$fractional_override_broker_pid" "$fractional_override_broker_start_time"
wb start "$machine" --detach
wait_running
refresh_pid
wait_native_window_frame

rm -f -- "$shared_dir/.buzzardos-acceptance"
rm -f -- "$shared_dir/.buzzardos-guest-created"
rm -f -- "$shared_dir/.buzzardos-guest-directory/host-file"
rmdir -- "$shared_dir/.buzzardos-guest-directory"
echo "Buzzard OS hardware acceptance passed for '$machine'"
