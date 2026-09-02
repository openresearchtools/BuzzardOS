#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -Eeuo pipefail
trap 'rc=$?; echo "integration acceptance failed at line $LINENO: $BASH_COMMAND" >&2; exit "$rc"' ERR

launcher=${1:-${BUZZARDOS_LAUNCHER:-/usr/bin/buzzardos}}
machine=${2:-machine1}
machine_dir=$(readlink -m -- "${3:-${BUZZARDOS_ACCEPT_MACHINE_DIR:?set BUZZARDOS_ACCEPT_MACHINE_DIR}}")
artifact_dir=$(readlink -m -- "${4:-$machine_dir/cache/acceptance/integrations-$(date -u +%Y%m%dT%H%M%SZ)}")
runtime=$machine_dir/runtime.json
config=$machine_dir/machine.json

for command_name in jq podman readlink; do
    command -v "$command_name" >/dev/null
done
[[ -x "$launcher" && -f "$config" ]] || {
    echo "installed launcher or machine metadata is missing" >&2
    exit 2
}
install -d -m 0700 "$artifact_dir"

wb() {
    "$launcher" --machine-dir "$machine_dir" "$@"
}

machine_id=$(jq -er '.id | gsub("-"; "")' "$config")
container="buzzardos-$machine_id"

if [[ $(podman container inspect --format '{{.State.Running}}' "$container" 2>/dev/null || true) != true ]]; then
    wb start "$machine" --detach
fi

container_id=$(podman container inspect --format '{{.Id}}' "$container")
[[ -n "$container_id" ]]
[[ $(podman container inspect --format '{{.State.Running}}' "$container") == true ]]
[[ $(podman container inspect --format '{{index .Config.Labels "org.openresearchtools.buzzardos.managed"}}' "$container") == true ]]
[[ $(podman container inspect --format '{{index .Config.Labels "org.openresearchtools.buzzardos.machine-id"}}' "$container") == "$(jq -r .id "$config")" ]]

podman container inspect "$container" >"$artifact_dir/podman-inspect-before.json"
jq . "$runtime" >"$artifact_dir/runtime-before.json"

# The guest integration is entered only through Podman's native exec path.
podman exec --user user "$container" sh -ceu '
    test "$(cat /proc/1/comm)" = systemd
    test -S /run/user/1000/bus
    pgrep -x sway >/dev/null
    pgrep -x buzzardos-deskt >/dev/null
'

# Every enabled host-to-guest rule must exist in Podman's own published-port
# state. Guest-to-host rules are native pasta arguments and are recorded by
# Podman in the persistent container definition.
while IFS=$'\t' read -r protocol host_port guest_port; do
    [[ -n "$protocol" ]] || continue
    podman port "$container" "$guest_port/$protocol" |
        grep -Eq ":${host_port}$"
done < <(
    jq -r '.integrations.ports[]? |
        select(.enabled and .direction == "host-to-guest") |
        [.protocol, (.host_port|tostring), (.guest_port|tostring)] | @tsv' "$config"
)

if jq -e '.integrations.media | .guest_audio_output or .host_microphone or .host_camera' "$config" >/dev/null; then
    endpoint_file=$(find "${XDG_RUNTIME_DIR:?}/buzzardos/machines/$machine_id/host-status" -maxdepth 1 -name media-endpoints.json -print -quit)
    [[ -n "$endpoint_file" ]]
    jq -e --arg container "$container" '.schema == 1 and .container == $container' "$endpoint_file" >/dev/null
    cp -- "$endpoint_file" "$artifact_dir/media-endpoints.json"
fi

# With no changed definition, Restart must target the same persistent Podman
# object; the container ID and external rootfs are unchanged.
wb restart "$machine"
[[ $(podman container inspect --format '{{.Id}}' "$container") == "$container_id" ]]
[[ $(podman container inspect --format '{{.State.Running}}' "$container") == true ]]

podman container inspect "$container" >"$artifact_dir/podman-inspect-after.json"
jq . "$runtime" >"$artifact_dir/runtime-after.json"
jq -n \
    --arg container "$container" \
    --arg container_id "$container_id" \
    '{schema:1, container:$container, persistent_container_id:$container_id, passed:true}' \
    >"$artifact_dir/result.json"

echo "integration acceptance passed: $artifact_dir"
