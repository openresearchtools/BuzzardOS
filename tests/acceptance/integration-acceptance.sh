#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -Eeuo pipefail

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
task_uid=$(id -u)
default_launcher="${TMPDIR:-/tmp}/buzzardos-build-$task_uid/out/BuzzardOS/BuzzardOS"
launcher=${1:-${BUZZARDOS_LAUNCHER:-$default_launcher}}
machine=${2:-machine1}
portable_dir=$(CDPATH= cd -- "$(dirname -- "$launcher")" && pwd)
launcher="$portable_dir/$(basename -- "$launcher")"
machine_dir="$portable_dir/Machines/$machine"
config="$machine_dir/machine.json"
runtime="$machine_dir/runtime.json"
shared="$portable_dir/shared"
stamp=$(date -u +%Y%m%dT%H%M%SZ)
artifact_dir=${3:-"$portable_dir/acceptance/integrations-$stamp"}
fixture="$project_dir/tests/acceptance/integration-echo-fixture.py"
gnome_microphone_probe="$project_dir/tests/acceptance/gnome-microphone-indicator-probe.js"
guest_fixture="/shared/.wildbuzzard-integration-acceptance-$stamp-$$.py"
host_fixture="$shared/$(basename -- "$guest_fixture")"
original_config="$artifact_dir/machine.original.json"
settings_file="$artifact_dir/integrations.requested.json"

container_pid=
host_echo_pid=
guest_echo_pid=
guest_audio_generator_pid=
configuration_restored=0
gnome_microphone_accounting_checked=false
gnome_shell_libdir=
gnome_shell_indicator_actor_checked=false
gnome_shell_indicator_actor_skip_reason="host desktop is not GNOME"
gnome_shell_indicator_baseline='[]'
gnome_shell_indicator_enabled='[]'
gnome_shell_indicator_added='[]'

fail() {
    echo "integration acceptance: $*" >&2
    exit 1
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
        DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
        LD_LIBRARY_PATH=/run/wildbuzzard-host/driver/lib \
        "$@"
}

guest_background() {
    local log=$1
    shift
    guest sh -c '
        log=$1
        shift
        setsid "$@" </dev/null >"$log" 2>&1 &
        echo $!
    ' sh "$log" "$@"
}

bounded_guest() {
    local seconds=$1
    shift
    local status=0
    guest timeout --signal=TERM "$seconds" "$@" || status=$?
    if [[ "$status" != 0 && "$status" != 124 ]]; then
        fail "bounded guest probe exited with status $status: $*"
    fi
}

host_process_start() {
    awk '{print $22}' "/proc/$1/stat"
}

guest_process_start() {
    guest awk '{print $22}' "/proc/$1/stat"
}

same_host_process_alive() {
    local pid=$1
    local start=$2
    [[ -r "/proc/$pid/stat" ]] && [[ $(host_process_start "$pid") == "$start" ]]
}

same_guest_process_alive() {
    local pid=$1
    local start=$2
    guest test -r "/proc/$pid/stat" 2>/dev/null &&
        [[ $(guest_process_start "$pid") == "$start" ]]
}

stop_host_pid() {
    local pid=${1:-}
    [[ -n "$pid" ]] || return 0
    kill -TERM "$pid" 2>/dev/null || true
    for _attempt in $(seq 1 30); do
        kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.1
    done
    kill -KILL "$pid" 2>/dev/null || true
}

stop_guest_pid() {
    local pid=${1:-}
    [[ -n "$pid" && -n ${container_pid:-} ]] || return 0
    guest kill -TERM "$pid" 2>/dev/null || true
    for _attempt in $(seq 1 30); do
        guest kill -0 "$pid" 2>/dev/null || return 0
        sleep 0.1
    done
    guest kill -KILL "$pid" 2>/dev/null || true
}

restore_configuration() {
    [[ -f "$original_config" && -d "$machine_dir" ]] || return 0
    local temporary
    temporary=$(mktemp "$machine_dir/.machine.json.integration-restore.XXXXXX")
    cp -- "$original_config" "$temporary"
    chmod --reference="$config" "$temporary" 2>/dev/null || chmod 0644 "$temporary"
    mv -f -- "$temporary" "$config"
    configuration_restored=1
}

cleanup() {
    local saved_status=$?
    set +e
    restore_configuration
    stop_guest_pid "$guest_audio_generator_pid"
    stop_guest_pid "$guest_echo_pid"
    stop_host_pid "$host_echo_pid"
    rm -f -- "$host_fixture"
    if [[ -n ${container_pid:-} ]]; then
        guest rm -f \
            /tmp/wildbuzzard-integration-acceptance-echo.json \
            /tmp/wildbuzzard-integration-acceptance-audio.raw \
            /tmp/wildbuzzard-integration-acceptance-audio.log \
            /tmp/wildbuzzard-integration-acceptance-mic.raw \
            /tmp/wildbuzzard-integration-acceptance-camera.raw \
            /tmp/wildbuzzard-integration-acceptance-guest-echo.log \
            >/dev/null 2>&1 || true
    fi
    return "$saved_status"
}

on_error() {
    local status=$?
    echo "integration acceptance failed at line $1: $2" >&2
    exit "$status"
}

trap 'on_error "$LINENO" "$BASH_COMMAND"' ERR
trap cleanup EXIT

for command_name in awk cp date gst-launch-1.0 id install jq mktemp nsenter pw-dump python3 setpriv stat timeout; do
    command -v "$command_name" >/dev/null 2>&1 ||
        fail "host dependency is missing: $command_name"
done
[[ -x "$launcher" ]] || fail "portable Buzzard OS launcher is missing or not executable: $launcher"
[[ -f "$fixture" ]] || fail "echo fixture is missing: $fixture"
[[ -f "$gnome_microphone_probe" ]] || fail "GNOME microphone probe is missing: $gnome_microphone_probe"
[[ -f "$config" && -f "$runtime" ]] || fail "machine '$machine' does not exist"
[[ $(jq -r '.state // empty' "$runtime") == running ]] ||
    fail "machine '$machine' must already be running"
[[ $(jq -r '.network // "user"' "$config") == user ]] ||
    fail "live ports and media require the machine's private user network"

if [[ ${XDG_CURRENT_DESKTOP:-} == *GNOME* || ${XDG_CURRENT_DESKTOP:-} == *gnome* ]]; then
    command -v gjs >/dev/null 2>&1 ||
        fail "GNOME microphone accounting acceptance requires gjs"
    for candidate in /usr/lib/gnome-shell /usr/lib64/gnome-shell /usr/lib/*/gnome-shell; do
        if [[ -f "$candidate/Gvc-1.0.typelib" && -f "$candidate/libgvc.so" ]]; then
            gnome_shell_libdir=$candidate
            break
        fi
    done
    [[ -n "$gnome_shell_libdir" ]] ||
        fail "GNOME microphone accounting acceptance could not locate Gvc-1.0.typelib and libgvc.so"
    [[ -n ${XDG_RUNTIME_DIR:-} ]] ||
        fail "GNOME microphone accounting acceptance requires XDG_RUNTIME_DIR"
    gnome_microphone_accounting_checked=true
    command -v gsettings >/dev/null 2>&1 ||
        fail "GNOME microphone indicator acceptance requires gsettings"
    if [[ $(gsettings get org.gnome.desktop.interface toolkit-accessibility) == true ]]; then
        gnome_shell_indicator_actor_checked=true
        gnome_shell_indicator_actor_skip_reason=
    else
        gnome_shell_indicator_actor_skip_reason="GNOME host accessibility is disabled"
    fi
fi

mkdir -p -- "$artifact_dir" "$shared"
cp -- "$config" "$original_config"
install -m 0755 -- "$fixture" "$host_fixture"
container_pid=$(jq -er '.container_pid' "$runtime")
initial_container_pid=$container_pid

"$launcher" doctor >"$artifact_dir/doctor.txt"

wait_runtime() {
    local expression=$1
    local description=$2
    local deadline=$((SECONDS + 30))
    while ((SECONDS < deadline)); do
        if jq -e "$expression" "$runtime" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.1
    done
    jq '{state, container_pid, detail, integrations}' "$runtime" >&2 || true
    fail "timed out waiting for $description"
}

wait_guest_node() {
    local name=$1
    local wanted=$2
    local deadline=$((SECONDS + 15))
    while ((SECONDS < deadline)); do
        local present=false
        if guest pw-dump 2>/dev/null | jq -e --arg name "$name" \
            'any(.[]; .info.props?["node.name"]? == $name)' >/dev/null; then
            present=true
        fi
        [[ "$present" == "$wanted" ]] && return 0
        sleep 0.1
    done
    fail "guest PipeWire node '$name' did not reach present=$wanted"
}

wait_host_microphone_stream() {
    local pid=$1
    local target=$2
    local wanted=$3
    local evidence_name=${4:-}
    local deadline=$((SECONDS + 15))
    while ((SECONDS < deadline)); do
        local present=false
        local graph_valid=false
        local graph=
        if graph=$(pw-dump 2>/dev/null) &&
            jq -e 'type == "array"' <<<"$graph" >/dev/null; then
            graph_valid=true
            if jq -e --arg pid "$pid" --arg target "$target" '
                . as $graph |
                [$graph[] | select(
                    .type == "PipeWire:Interface:Node" and
                    .info.state == "running" and
                    ((.info.props["pulse.corked"] // false) as $corked |
                        $corked != true and $corked != "true" and
                        $corked != 1 and $corked != "1") and
                    .info.props["media.class"] == "Stream/Input/Audio" and
                    .info.props["client.api"] == "pipewire-pulse" and
                    .info.props["application.id"] == "org.openresearchtools.BuzzardOS" and
                    (.info.props["application.process.id"] | tostring) == $pid and
                    .info.props["target.object"] == $target
                )] as $streams |
                [$graph[] | select(
                    .type == "PipeWire:Interface:Node" and
                    .info.props["node.name"] == $target and
                    .info.props["media.class"] == "Audio/Source"
                )] as $sources |
                any($streams[]; . as $stream |
                    any($sources[]; . as $source |
                        any($graph[];
                            .type == "PipeWire:Interface:Link" and
                            .info.state == "active" and
                            (.info["output-node-id"] | tostring) == ($source.id | tostring) and
                            (.info["input-node-id"] | tostring) == ($stream.id | tostring))))
            ' <<<"$graph" >/dev/null; then
                present=true
            fi
        fi
        if [[ "$graph_valid" == true && "$present" == "$wanted" ]]; then
            if [[ -n "$evidence_name" ]]; then
                jq . <<<"$graph" >"$artifact_dir/$evidence_name"
            fi
            return 0
        fi
        sleep 0.1
    done
    fail "host microphone recording stream did not reach present=$wanted for PID $pid"
}

record_microphone_runtime_evidence() {
    local phase=$1
    local pid=$2
    local target=$3
    local process_alive=false
    local process_start=
    if process_start=$(host_process_start "$pid" 2>/dev/null); then
        process_alive=true
    else
        process_start=
    fi
    jq --arg phase "$phase" '{
        phase: $phase,
        state,
        container_pid,
        microphone: .integrations.host_microphone
    }' "$runtime" >"$artifact_dir/runtime-microphone-$phase.json"
    jq -n \
        --arg phase "$phase" \
        --argjson host_pid "$pid" \
        --arg target "$target" \
        --argjson process_alive "$process_alive" \
        --arg process_start "$process_start" \
        '{
            phase: $phase,
            host_pid: $host_pid,
            selected_target: $target,
            process_alive: $process_alive,
            process_start_ticks: (
                if $process_start == "" then null else ($process_start | tonumber) end
            )
        }' >"$artifact_dir/microphone-process-$phase.json"
}

wait_gnome_microphone_accounting() {
    local phase=$1
    local wanted=$2
    local evidence_name=$3

    if [[ "$gnome_microphone_accounting_checked" != true ]]; then
        jq -n \
            --arg phase "$phase" \
            --arg reason "$gnome_shell_indicator_actor_skip_reason" \
            '{
                acceptance: {
                    phase: $phase,
                    gnome_gvc_checked: false,
                    shell_indicator_actor_checked: false,
                    shell_indicator_actor_skip_reason: $reason
                }
            }' >"$artifact_dir/$evidence_name"
        return 0
    fi

    local evidence="$artifact_dir/$evidence_name"
    local stderr_evidence="$artifact_dir/$evidence_name.stderr"
    local deadline=$((SECONDS + 15))
    local payload=
    while ((SECONDS < deadline)); do
        if payload=$(env -i \
            HOME="${HOME:-/}" \
            USER="${USER:-$(id -un)}" \
            LOGNAME="${LOGNAME:-$(id -un)}" \
            PATH=/usr/local/bin:/usr/bin:/bin \
            LANG=C.UTF-8 \
            XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
            DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-}" \
            GI_TYPELIB_PATH="$gnome_shell_libdir" \
            LD_LIBRARY_PATH="$gnome_shell_libdir" \
            gjs "$gnome_microphone_probe" org.openresearchtools.BuzzardOS \
            2>"$stderr_evidence"); then
            local gvc_matches=false
            if [[ "$wanted" == true ]]; then
                jq -e '
                    .ready and
                    .wild_buzzard_tracked and
                    .wild_buzzard_privacy_indicator_expected
                ' <<<"$payload" >/dev/null && gvc_matches=true
            else
                jq -e '.ready and (.wild_buzzard_tracked | not)' \
                    <<<"$payload" >/dev/null && gvc_matches=true
            fi

            local actor_matches=true
            local current_indices='[]'
            local candidate_added='[]'
            if [[ "$gnome_shell_indicator_actor_checked" == true ]]; then
                actor_matches=false
                if jq -e '.shell_indicator.available == true' \
                    <<<"$payload" >/dev/null; then
                    current_indices=$(jq -c '.shell_indicator.showing_child_indices' \
                        <<<"$payload")
                    case "$phase" in
                        baseline)
                            actor_matches=true
                            ;;
                        enabled)
                            candidate_added=$(jq -cn \
                                --argjson baseline "$gnome_shell_indicator_baseline" \
                                --argjson current "$current_indices" \
                                '$current - $baseline')
                            if jq -en \
                                --argjson added "$candidate_added" \
                                '($added | length) == 1'; then
                                actor_matches=true
                            fi
                            ;;
                        recovered)
                            if jq -en \
                                --argjson added "$gnome_shell_indicator_added" \
                                --argjson current "$current_indices" \
                                '(($added - $current) | length) == 0'; then
                                actor_matches=true
                            fi
                            ;;
                        disabled)
                            if jq -en \
                                --argjson added "$gnome_shell_indicator_added" \
                                --argjson current "$current_indices" \
                                '($added - $current) == $added'; then
                                actor_matches=true
                            fi
                            ;;
                        *)
                            fail "unknown GNOME microphone indicator phase: $phase"
                            ;;
                    esac
                fi
            fi

            if [[ "$gvc_matches" == true && "$actor_matches" == true ]]; then
                case "$phase" in
                    baseline)
                        gnome_shell_indicator_baseline=$current_indices
                        ;;
                    enabled)
                        gnome_shell_indicator_enabled=$current_indices
                        gnome_shell_indicator_added=$candidate_added
                        ;;
                esac
                jq \
                    --arg phase "$phase" \
                    --argjson actor_checked "$gnome_shell_indicator_actor_checked" \
                    --arg actor_skip_reason "$gnome_shell_indicator_actor_skip_reason" \
                    --argjson baseline "$gnome_shell_indicator_baseline" \
                    --argjson enabled "$gnome_shell_indicator_enabled" \
                    --argjson added "$gnome_shell_indicator_added" \
                    '. + {
                        acceptance: {
                            phase: $phase,
                            gnome_gvc_checked: true,
                            shell_indicator_actor_checked: $actor_checked,
                            shell_indicator_actor_skip_reason: (
                                if $actor_checked then null else $actor_skip_reason end
                            ),
                            baseline_showing_child_indices: $baseline,
                            enabled_showing_child_indices: $enabled,
                            indicator_added_child_indices: $added
                        }
                    }' <<<"$payload" >"$evidence"
                return 0
            fi
            printf '%s\n' "$payload" >"$evidence"
        fi
        sleep 0.1
    done

    [[ -n "$payload" ]] && printf '%s\n' "$payload" >&2
    [[ ! -s "$stderr_evidence" ]] || cat -- "$stderr_evidence" >&2
    fail "GNOME microphone accounting/indicator did not reach phase=$phase tracked=$wanted"
}

apply_settings() {
    local temporary
    temporary=$(mktemp "$machine_dir/.machine.json.integration.XXXXXX")
    jq --slurpfile settings "$settings_file" \
        '.integrations = $settings[0]' "$config" >"$temporary"
    chmod --reference="$config" "$temporary" 2>/dev/null || chmod 0644 "$temporary"
    mv -f -- "$temporary" "$config"
}

write_settings() {
    local ports_json=$1
    local audio=$2
    local microphone=$3
    local camera=$4
    local microphone_target=${5:-}
    local camera_target=${6:-}
    jq -n \
        --argjson ports "$ports_json" \
        --argjson audio "$audio" \
        --argjson microphone "$microphone" \
        --argjson camera "$camera" \
        --arg microphone_target "$microphone_target" \
        --arg camera_target "$camera_target" \
        '{
            ports: $ports,
            media: {
                guest_audio_output: $audio,
                host_microphone: $microphone,
                host_camera: $camera,
                audio_target: null,
                microphone_target: (if $microphone_target == "" then null else $microphone_target end),
                camera_target: (if $camera_target == "" then null else $camera_target end)
            }
        }' >"$settings_file"
    apply_settings
}

assert_container_unchanged() {
    local observed
    observed=$(jq -er '.container_pid' "$runtime")
    [[ "$observed" == "$initial_container_pid" ]] ||
        fail "live integration restarted the machine ($initial_container_pid -> $observed)"
    kill -0 "$initial_container_pid" 2>/dev/null || fail "container PID exited"
}

uuid() {
    python3 -c 'import uuid; print(uuid.uuid4())'
}

free_host_port() {
    python3 - "$1" <<'PY'
import socket, sys
kind = socket.SOCK_STREAM if sys.argv[1] == "tcp" else socket.SOCK_DGRAM
with socket.socket(socket.AF_INET, kind) as endpoint:
    endpoint.bind(("127.0.0.1", 0))
    print(endpoint.getsockname()[1])
PY
}

free_guest_port() {
    guest python3 -c '
import socket, sys
kind = socket.SOCK_STREAM if sys.argv[1] == "tcp" else socket.SOCK_DGRAM
with socket.socket(socket.AF_INET, kind) as endpoint:
    endpoint.bind(("127.0.0.1", 0))
    print(endpoint.getsockname()[1])
' "$1"
}

wait_file() {
    local path=$1
    local deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)); do
        [[ -s "$path" ]] && return 0
        sleep 0.05
    done
    fail "fixture readiness file did not appear: $path"
}

wait_guest_file() {
    local path=$1
    local deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)); do
        guest test -s "$path" && return 0
        sleep 0.05
    done
    fail "guest fixture readiness file did not appear: $path"
}

assert_host_endpoint_closed() {
    local protocol=$1
    local port=$2
    if python3 "$fixture" client --protocol "$protocol" --host 127.0.0.1 \
        --port "$port" --message revoked --expect-prefix guest --timeout 1 \
        >/dev/null 2>&1; then
        fail "disabled host $protocol endpoint 127.0.0.1:$port still relays"
    fi
}

assert_guest_endpoint_closed() {
    local protocol=$1
    local port=$2
    if guest python3 "$guest_fixture" client --protocol "$protocol" --host 127.0.0.1 \
        --port "$port" --message revoked --expect-prefix host --timeout 1 \
        >/dev/null 2>&1; then
        fail "disabled guest $protocol endpoint 127.0.0.1:$port still relays"
    fi
}

assert_media_disabled() {
    wait_runtime '
        .integrations.guest_audio_output.enabled == false and
        .integrations.guest_audio_output.active == false and
        .integrations.guest_audio_output.host_pid == null and
        .integrations.guest_audio_output.guest_pid == null and
        .integrations.host_microphone.enabled == false and
        .integrations.host_microphone.active == false and
        .integrations.host_microphone.host_pid == null and
        .integrations.host_microphone.guest_pid == null and
        .integrations.host_camera.enabled == false and
        .integrations.host_camera.active == false and
        .integrations.host_camera.host_pid == null and
        .integrations.host_camera.guest_pid == null
    ' "all media bridges to be absent"
    wait_guest_node wildbuzzard_host_microphone false
    wait_guest_node wildbuzzard_host_camera false
    if pw-dump | jq -e '
        any(.[];
            ((.info.props?["application.name"]? // "") | contains("Buzzard OS Guest Audio")) or
            ((.info.props?["node.description"]? // "") | contains("Buzzard OS Guest Audio")))
    ' >/dev/null; then
        fail "disabled guest-audio bridge remains registered with host PipeWire"
    fi
    if pw-dump | jq -e '
        any(.[];
            .type == "PipeWire:Interface:Node" and
            .info.props["media.class"] == "Stream/Input/Audio" and
            .info.props["application.id"] == "org.openresearchtools.BuzzardOS")
    ' >/dev/null; then
        fail "disabled microphone bridge remains registered as a host recording stream"
    fi
}

raw_metrics() {
    guest python3 - "$1" <<'PY'
import json, pathlib, sys
payload = pathlib.Path(sys.argv[1]).read_bytes()
print(json.dumps({"bytes": len(payload), "nonzero_bytes": sum(byte != 0 for byte in payload)}))
PY
}

# A private guest PipeWire server exists, but it must not be the host server.
host_pipewire_identity=$(stat -Lc '%d:%i' "/run/user/$(id -u)/pipewire-0")
guest_pipewire_identity=$(guest stat -Lc '%d:%i' /run/user/1000/pipewire-0)
[[ "$host_pipewire_identity" != "$guest_pipewire_identity" ]] ||
    fail "guest is using the host PipeWire socket"
[[ -z $(guest find /dev -maxdepth 1 -name 'video*' -print -quit) ]] ||
    fail "a host camera device is visible in the guest while sharing is off"
guest test ! -e /dev/snd ||
    fail "host sound devices are visible in the guest while sharing is off"

write_settings '[]' false false false
assert_media_disabled
assert_container_unchanged

# Run one host and one guest target that serve TCP and UDP simultaneously.
host_ready="$artifact_dir/host-echo.json"
python3 "$fixture" server --address 127.0.0.1 --tcp-port 0 --udp-port 0 \
    --prefix host --ready "$host_ready" >"$artifact_dir/host-echo.log" 2>&1 &
host_echo_pid=$!
wait_file "$host_ready"
host_target_tcp=$(jq -er '.tcp_port' "$host_ready")
host_target_udp=$(jq -er '.udp_port' "$host_ready")

guest_ready=/tmp/wildbuzzard-integration-acceptance-echo.json
guest_echo_pid=$(guest_background /tmp/wildbuzzard-integration-acceptance-guest-echo.log \
    python3 "$guest_fixture" server --address 0.0.0.0 --tcp-port 0 --udp-port 0 \
    --prefix guest --ready "$guest_ready")
wait_guest_file "$guest_ready"
guest_ready_json=$(guest cat "$guest_ready")
guest_target_tcp=$(jq -er '.tcp_port' <<<"$guest_ready_json")
guest_target_udp=$(jq -er '.udp_port' <<<"$guest_ready_json")

host_forward_tcp=$(free_host_port tcp)
host_forward_udp=$(free_host_port udp)
guest_reverse_tcp=$(free_guest_port tcp)
guest_reverse_udp=$(free_guest_port udp)
id_host_tcp=$(uuid)
id_host_udp=$(uuid)
id_guest_tcp=$(uuid)
id_guest_udp=$(uuid)
ports=$(jq -n \
    --arg id_host_tcp "$id_host_tcp" --arg id_host_udp "$id_host_udp" \
    --arg id_guest_tcp "$id_guest_tcp" --arg id_guest_udp "$id_guest_udp" \
    --argjson host_forward_tcp "$host_forward_tcp" \
    --argjson host_forward_udp "$host_forward_udp" \
    --argjson guest_target_tcp "$guest_target_tcp" \
    --argjson guest_target_udp "$guest_target_udp" \
    --argjson guest_reverse_tcp "$guest_reverse_tcp" \
    --argjson guest_reverse_udp "$guest_reverse_udp" \
    --argjson host_target_tcp "$host_target_tcp" \
    --argjson host_target_udp "$host_target_udp" '
    [
        {id:$id_host_tcp,enabled:true,direction:"host-to-guest",protocol:"tcp",host_address:"127.0.0.1",host_port:$host_forward_tcp,guest_address:"10.0.2.100",guest_port:$guest_target_tcp},
        {id:$id_host_udp,enabled:true,direction:"host-to-guest",protocol:"udp",host_address:"127.0.0.1",host_port:$host_forward_udp,guest_address:"10.0.2.100",guest_port:$guest_target_udp},
        {id:$id_guest_tcp,enabled:true,direction:"guest-to-host",protocol:"tcp",host_address:"127.0.0.1",host_port:$host_target_tcp,guest_address:"127.0.0.1",guest_port:$guest_reverse_tcp},
        {id:$id_guest_udp,enabled:true,direction:"guest-to-host",protocol:"udp",host_address:"127.0.0.1",host_port:$host_target_udp,guest_address:"127.0.0.1",guest_port:$guest_reverse_udp}
    ]')
write_settings "$ports" false false false
wait_runtime '(.integrations.ports | length) == 4 and all(.integrations.ports[]; .enabled and .active)' \
    "all four live port mappings"
assert_container_unchanged

port_host_tcp=$(python3 "$fixture" client --protocol tcp --host 127.0.0.1 \
    --port "$host_forward_tcp" --message host-to-guest-tcp --expect-prefix guest)
port_host_udp=$(python3 "$fixture" client --protocol udp --host 127.0.0.1 \
    --port "$host_forward_udp" --message host-to-guest-udp --expect-prefix guest)
port_guest_tcp=$(guest python3 "$guest_fixture" client --protocol tcp --host 127.0.0.1 \
    --port "$guest_reverse_tcp" --message guest-to-host-tcp --expect-prefix host)
port_guest_udp=$(guest python3 "$guest_fixture" client --protocol udp --host 127.0.0.1 \
    --port "$guest_reverse_udp" --message guest-to-host-udp --expect-prefix host)
jq '{integrations}' "$runtime" >"$artifact_dir/ports-active.json"

write_settings '[]' false false false
wait_runtime '(.integrations.ports | length) == 0' "all port mappings to be removed"
assert_host_endpoint_closed tcp "$host_forward_tcp"
assert_host_endpoint_closed udp "$host_forward_udp"
assert_guest_endpoint_closed tcp "$guest_reverse_tcp"
assert_guest_endpoint_closed udp "$guest_reverse_udp"
for identifier in "$id_host_udp" "$id_guest_tcp" "$id_guest_udp"; do
    guest test ! -e "/run/wildbuzzard-host/reverse/forward-$identifier.sock"
    guest test ! -e "/run/wildbuzzard-host/reverse/reverse-$identifier.sock"
done
assert_container_unchanged

# Resolve the host's real default microphone and camera from the live PipeWire
# graph, then require their physical ALSA/V4L2/libcamera backends for hardware
# acceptance.  Current WirePlumber configurations commonly publish integrated
# cameras only through libcamera, with no parallel api.v4l2.path property.
host_graph=$(pw-dump)
host_mic_name=$(jq -er '
    first(.[]
        | select(.type == "PipeWire:Interface:Metadata")
        | select(.props["metadata.name"] == "default")
        | .metadata[]
        | select(.key == "default.audio.source")
        | .value.name)
' <<<"$host_graph")
host_camera_name=$(jq -er '
    first(.[]
        | select(.type == "PipeWire:Interface:Metadata")
        | select(.props["metadata.name"] == "default")
        | .metadata[]
        | select(.key == "default.video.source")
        | .value.name)
' <<<"$host_graph")
jq -e --arg name "$host_mic_name" '
    any(.[];
        .type == "PipeWire:Interface:Node" and
        .info.props["node.name"] == $name and
        .info.props["media.class"] == "Audio/Source" and
        ((.info.props["api.alsa.path"] // "") | startswith("hw:")))
' <<<"$host_graph" >/dev/null ||
    fail "default microphone is not a physical ALSA-backed PipeWire source"
jq -e --arg name "$host_camera_name" '
    any(.[];
        .type == "PipeWire:Interface:Node" and
        .info.props["node.name"] == $name and
        .info.props["media.class"] == "Video/Source" and
        (
            ((.info.props["api.v4l2.path"] // "") | startswith("/dev/video")) or
            (
                .info.props["device.api"] == "libcamera" and
                ((.info.props["api.libcamera.path"] // "") | length > 0)
            )
        ))
' <<<"$host_graph" >/dev/null ||
    fail "default camera is not a physical V4L2- or libcamera-backed PipeWire source"
printf '%s\n' "$host_mic_name" >"$artifact_dir/physical-microphone.txt"
printf '%s\n' "$host_camera_name" >"$artifact_dir/physical-camera.txt"

# Having host sources available must not make them visible before consent.
assert_media_disabled
if guest pw-dump | jq -e --arg mic "$host_mic_name" --arg camera "$host_camera_name" '
    any(.[]; .info.props?["node.name"]? == $mic or .info.props?["node.name"]? == $camera)
' >/dev/null; then
    fail "host media sources leaked into the guest before sharing was enabled"
fi
wait_gnome_microphone_accounting baseline false gnome-microphone-baseline.json

write_settings '[]' false true false "$host_mic_name"
wait_runtime '.integrations.host_microphone.enabled and .integrations.host_microphone.active and (.integrations.host_microphone.host_pid != null) and (.integrations.host_microphone.guest_pid != null)' \
    "microphone bridge activation"
wait_guest_node wildbuzzard_host_microphone true
mic_host_pid=$(jq -er '.integrations.host_microphone.host_pid' "$runtime")
mic_guest_pid=$(jq -er '.integrations.host_microphone.guest_pid' "$runtime")
wait_host_microphone_stream \
    "$mic_host_pid" "$host_mic_name" true pipewire-microphone-enabled.json
record_microphone_runtime_evidence enabled "$mic_host_pid" "$host_mic_name"
wait_gnome_microphone_accounting enabled true gnome-microphone-enabled.json
bounded_guest 4 gst-launch-1.0 -q \
    pipewiresrc target-object=wildbuzzard_host_microphone num-buffers=48 do-timestamp=true \
    ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=2 \
    ! filesink location=/tmp/wildbuzzard-integration-acceptance-mic.raw
mic_metrics=$(raw_metrics /tmp/wildbuzzard-integration-acceptance-mic.raw)
printf '%s\n' "$mic_metrics" | jq . >"$artifact_dir/microphone-samples.json"
jq -e '.bytes > 4096 and .nonzero_bytes > 1024' <<<"$mic_metrics" >/dev/null ||
    fail "microphone bridge produced no measurable guest samples: $mic_metrics"

# A dead host bridge is reconciled live without restarting the container.
kill -TERM "$mic_host_pid"
wait_runtime ".integrations.host_microphone.active and .integrations.host_microphone.host_pid != $mic_host_pid" \
    "microphone bridge recovery"
recovered_mic_host_pid=$(jq -er '.integrations.host_microphone.host_pid' "$runtime")
wait_host_microphone_stream \
    "$recovered_mic_host_pid" "$host_mic_name" true pipewire-microphone-recovered.json
record_microphone_runtime_evidence recovered "$recovered_mic_host_pid" "$host_mic_name"
wait_gnome_microphone_accounting recovered true gnome-microphone-recovered.json
recovered_mic_host_start=$(host_process_start "$recovered_mic_host_pid")
recovered_mic_guest_pid=$(jq -er '.integrations.host_microphone.guest_pid' "$runtime")
recovered_mic_guest_start=$(guest_process_start "$recovered_mic_guest_pid")
assert_container_unchanged

write_settings '[]' false false false
assert_media_disabled
wait_host_microphone_stream \
    "$recovered_mic_host_pid" "$host_mic_name" false pipewire-microphone-disabled.json
record_microphone_runtime_evidence disabled "$recovered_mic_host_pid" "$host_mic_name"
wait_gnome_microphone_accounting disabled false gnome-microphone-disabled.json
same_host_process_alive "$recovered_mic_host_pid" "$recovered_mic_host_start" &&
    fail "disabled host microphone capture process is still alive"
same_guest_process_alive "$recovered_mic_guest_pid" "$recovered_mic_guest_start" &&
    fail "disabled guest microphone source process is still alive"
assert_container_unchanged

write_settings '[]' false false true "" "$host_camera_name"
wait_runtime '.integrations.host_camera.enabled and .integrations.host_camera.active and (.integrations.host_camera.host_pid != null) and (.integrations.host_camera.guest_pid != null)' \
    "camera bridge activation"
wait_guest_node wildbuzzard_host_camera true
guest busctl --user get-property \
    org.freedesktop.portal.Desktop \
    /org/freedesktop/portal/desktop \
    org.freedesktop.portal.Camera \
    IsCameraPresent | grep -Fx 'b true' >/dev/null ||
    fail "guest camera portal does not classify the shared source as a camera"
camera_host_pid=$(jq -er '.integrations.host_camera.host_pid' "$runtime")
camera_guest_pid=$(jq -er '.integrations.host_camera.guest_pid' "$runtime")
camera_host_start=$(host_process_start "$camera_host_pid")
camera_guest_start=$(guest_process_start "$camera_guest_pid")
bounded_guest 4 gst-launch-1.0 -q \
    pipewiresrc target-object=wildbuzzard_host_camera num-buffers=3 do-timestamp=true \
    ! videoconvert ! video/x-raw,format=BGRA \
    ! filesink location=/tmp/wildbuzzard-integration-acceptance-camera.raw
camera_metrics=$(guest python3 - /tmp/wildbuzzard-integration-acceptance-camera.raw <<'PY'
import json, pathlib, sys
payload = pathlib.Path(sys.argv[1]).read_bytes()
rgb = [payload[index] for index in range(len(payload)) if index % 4 != 3]
print(json.dumps({
    "bytes": len(payload),
    "rgb_nonzero_bytes": sum(byte != 0 for byte in rgb),
    "rgb_minimum": min(rgb) if rgb else None,
    "rgb_maximum": max(rgb) if rgb else None,
}))
PY
)
jq -e '
    .bytes > 1000000 and
    .rgb_nonzero_bytes > 100000 and
    .rgb_maximum > .rgb_minimum
' <<<"$camera_metrics" >/dev/null ||
    fail "physical camera produced blank or placeholder guest frames: $camera_metrics"

write_settings '[]' false false false
assert_media_disabled
same_host_process_alive "$camera_host_pid" "$camera_host_start" &&
    fail "disabled host camera capture process is still alive"
same_guest_process_alive "$camera_guest_pid" "$camera_guest_start" &&
    fail "disabled guest camera source process is still alive"
assert_container_unchanged

write_settings '[]' true false false
wait_runtime '.integrations.guest_audio_output.enabled and .integrations.guest_audio_output.active and (.integrations.guest_audio_output.host_pid != null) and (.integrations.guest_audio_output.guest_pid != null)' \
    "guest audio output activation"
audio_host_pid=$(jq -er '.integrations.guest_audio_output.host_pid' "$runtime")
audio_guest_pid=$(jq -er '.integrations.guest_audio_output.guest_pid' "$runtime")
audio_host_start=$(host_process_start "$audio_host_pid")
audio_guest_start=$(guest_process_start "$audio_guest_pid")
guest_audio_generator_pid=$(guest_background /tmp/wildbuzzard-integration-acceptance-audio.log \
    gst-launch-1.0 -q audiotestsrc is-live=true wave=sine volume=0.2 \
    ! audioconvert ! audio/x-raw,format=S16LE,rate=48000,channels=2 \
    ! pipewiresink sync=false)
bounded_guest 4 gst-launch-1.0 -q \
    pipewiresrc num-buffers=48 do-timestamp=true \
    stream-properties=props,stream.capture.sink=true,stream.monitor=true \
    ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=2 \
    ! filesink location=/tmp/wildbuzzard-integration-acceptance-audio.raw
audio_metrics=$(raw_metrics /tmp/wildbuzzard-integration-acceptance-audio.raw)
jq -e '.bytes > 4096 and .nonzero_bytes > 1024' <<<"$audio_metrics" >/dev/null ||
    fail "guest output monitor produced no measurable samples: $audio_metrics"
pw-dump | jq -e '
    any(.[];
        ((.info.props?["application.name"]? // "") | contains("Buzzard OS Guest Audio")) or
        ((.info.props?["node.description"]? // "") | contains("Buzzard OS Guest Audio")))
' >/dev/null || fail "guest audio bridge is absent from host PipeWire"

stop_guest_pid "$guest_audio_generator_pid"
guest_audio_generator_pid=
write_settings '[]' false false false
assert_media_disabled
same_host_process_alive "$audio_host_pid" "$audio_host_start" &&
    fail "disabled guest-audio host process is still alive"
same_guest_process_alive "$audio_guest_pid" "$audio_guest_start" &&
    fail "disabled guest-audio source process is still alive"
assert_container_unchanged

# Final revocation audit while the physical host devices remain present.
[[ -z $(guest find /dev -maxdepth 1 -name 'video*' -print -quit) ]] ||
    fail "camera device appeared in the guest after revocation"
guest test ! -e /dev/snd || fail "sound device appeared in the guest after revocation"
assert_media_disabled
assert_container_unchanged

jq '{state, container_pid, integrations}' "$runtime" >"$artifact_dir/runtime-before-restore.json"
restore_configuration
original_integrations=$(jq -cS '.integrations // {ports:[],media:{}}' "$original_config")
deadline=$((SECONDS + 30))
while ((SECONDS < deadline)); do
    current_integrations=$(jq -cS '.integrations // {ports:[],media:{}}' "$config")
    [[ "$current_integrations" == "$original_integrations" ]] && break
    sleep 0.1
done
[[ "$current_integrations" == "$original_integrations" ]] ||
    fail "original machine configuration was not restored"

stop_host_pid "$host_echo_pid"
host_echo_pid=
stop_guest_pid "$guest_echo_pid"
guest_echo_pid=
rm -f -- "$host_fixture"
assert_container_unchanged

jq -n \
    --arg machine "$machine" \
    --argjson container_pid "$initial_container_pid" \
    --arg host_pipewire_identity "$host_pipewire_identity" \
    --arg guest_pipewire_identity "$guest_pipewire_identity" \
    --arg port_host_tcp "$port_host_tcp" \
    --arg port_host_udp "$port_host_udp" \
    --arg port_guest_tcp "$port_guest_tcp" \
    --arg port_guest_udp "$port_guest_udp" \
    --argjson microphone "$mic_metrics" \
    --argjson camera "$camera_metrics" \
    --argjson audio "$audio_metrics" \
    --argjson mic_recovered_from "$mic_host_pid" \
    --argjson mic_recovered_to "$recovered_mic_host_pid" \
    --argjson gnome_microphone_accounting_checked "$gnome_microphone_accounting_checked" \
    --argjson gnome_shell_indicator_actor_checked "$gnome_shell_indicator_actor_checked" \
    --arg gnome_shell_indicator_actor_skip_reason "$gnome_shell_indicator_actor_skip_reason" \
    --argjson gnome_shell_indicator_added "$gnome_shell_indicator_added" '
    {
        result: "passed",
        machine: $machine,
        container_pid_before_and_after: $container_pid,
        isolation: {
            host_pipewire_socket_identity: $host_pipewire_identity,
            guest_pipewire_socket_identity: $guest_pipewire_identity,
            host_socket_not_shared: ($host_pipewire_identity != $guest_pipewire_identity),
            no_camera_device_when_disabled: true,
            no_sound_device_when_disabled: true
        },
        ports: {
            host_to_guest_tcp: $port_host_tcp,
            host_to_guest_udp: $port_host_udp,
            guest_to_host_tcp: $port_guest_tcp,
            guest_to_host_udp: $port_guest_udp,
            endpoints_removed_after_disable: true
        },
        media: {
            microphone_samples: $microphone,
            camera_frames: $camera,
            guest_audio_samples: $audio,
            microphone_host_bridge_recovery: {old_pid:$mic_recovered_from,new_pid:$mic_recovered_to},
            gnome_microphone_source_output_accounting_checked: $gnome_microphone_accounting_checked,
            gnome_shell_microphone_indicator_actor_checked: $gnome_shell_indicator_actor_checked,
            gnome_shell_microphone_indicator_actor_skip_reason: (
                if $gnome_shell_indicator_actor_checked
                then null
                else $gnome_shell_indicator_actor_skip_reason
                end
            ),
            gnome_shell_microphone_indicator_added_child_indices: $gnome_shell_indicator_added,
            all_processes_and_guest_sources_removed_after_disable: true
        },
        machine_restarted: false,
        original_configuration_restored: true
    }' >"$artifact_dir/result.json"

jq '{state, container_pid, integrations}' "$runtime" >"$artifact_dir/runtime-final.json"
trap - EXIT
echo "integration acceptance passed: $artifact_dir/result.json"
