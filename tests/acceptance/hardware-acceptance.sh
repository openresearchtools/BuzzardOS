#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -Eeuo pipefail
trap 'rc=$?; echo "hardware acceptance failed at line $LINENO: $BASH_COMMAND" >&2; exit "$rc"' ERR

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
launcher=${1:-${BUZZARDOS_LAUNCHER:-/usr/bin/buzzardos}}
machine=${2:-acceptance}
machine_dir=$(readlink -m -- "${3:-${BUZZARDOS_ACCEPT_MACHINE_DIR:?set BUZZARDOS_ACCEPT_MACHINE_DIR}}")
image=${4:-${BUZZARDOS_ACCEPT_IMAGE:-}}
artifact_dir=$(readlink -m -- "${5:-${TMPDIR:-/tmp}/buzzardos-acceptance-$(id -u)/artifacts-$machine}")
shared_dir=$(readlink -m -- "${BUZZARDOS_ACCEPT_SHARE_DIR:-${TMPDIR:-/tmp}/buzzardos-acceptance-$(id -u)/shared-$machine}")
password=${BUZZARDOS_ACCEPT_PASSWORD:-buzzard}
apt_package=${BUZZARDOS_ACCEPT_APT_PACKAGE:-hello}
runtime=$machine_dir/runtime.json
config=$machine_dir/machine.json

for command_name in jq podman readlink sha256sum; do
    command -v "$command_name" >/dev/null
done
[[ -x "$launcher" ]] || {
    echo "installed Buzzard OS launcher is missing: $launcher" >&2
    exit 2
}
install -d -m 0700 "$artifact_dir" "$shared_dir"

wb() {
    "$launcher" --machine-dir "$machine_dir" "$@"
}

wait_state() {
    local expected=$1
    local deadline=$((SECONDS + 120))
    while ((SECONDS < deadline)); do
        if [[ -f "$runtime" ]] && [[ $(jq -r '.state // empty' "$runtime") == "$expected" ]]; then
            return
        fi
        sleep 0.2
    done
    echo "machine did not reach state $expected" >&2
    jq . "$runtime" >&2 2>/dev/null || true
    exit 1
}

container_for() {
    jq -er '"buzzardos-" + (.id | gsub("-"; ""))' "$1/machine.json"
}

sudo_guest() {
    printf '%s\n' "$password" |
        podman exec --interactive --user user "$container" \
            sudo -S -p '' -- "$@"
}

if [[ ! -f "$config" ]]; then
    [[ -n "$image" ]] || {
        echo "machine is absent; provide an image as argument 4 or BUZZARDOS_ACCEPT_IMAGE" >&2
        exit 2
    }
    "$launcher" --machine-dir "$machine_dir" create "$machine" \
        --image "$image" --share "$shared_dir"
fi

container=$(container_for "$machine_dir")
wb start "$machine" --detach
wait_state running
container_id=$(podman inspect --format '{{.Id}}' "$container")
[[ $(podman inspect --format '{{.State.Running}}' "$container") == true ]]

podman inspect "$container" >"$artifact_dir/podman-inspect-running.json"
jq . "$config" >"$artifact_dir/machine.json"
jq . "$runtime" >"$artifact_dir/runtime-running.json"

# One complete desktop system runs behind the persistent Podman definition.
podman exec "$container" sh -ceu '
    test "$(cat /proc/1/comm)" = systemd
    test -d /run/systemd/system
    test -S /run/user/1000/bus
    pgrep -x sway >/dev/null
    pgrep -x buzzardos-deskt >/dev/null
    pgrep -x cua >/dev/null || command -v cua >/dev/null
'

# The guest handoff executes the real distro sudo on nosuid storage. Exercise authenticated non-TTY
# operation, package indexes, dependency resolution, maintainer scripts, and
# a persistent root-owned configuration write.
podman exec --user user "$container" sh -ceu '
    test -x /usr/bin/sudo
    test -x /usr/libexec/buzzardos-guest/sudo
    test -S /run/buzzardos/sudo.sock
    sudo -k
'
sudo_guest apt-get -o Dpkg::Use-Pty=0 update
sudo_guest env DEBIAN_FRONTEND=noninteractive \
    apt-get -o Dpkg::Use-Pty=0 install --yes "$apt_package"
sudo_guest sh -ceu 'printf "%s\n" buzzardos-native-sudo-acceptance >/etc/buzzardos-sudo-acceptance'
podman exec "$container" grep -qx buzzardos-native-sudo-acceptance /etc/buzzardos-sudo-acceptance

# CUA must still receive the private desktop, AT-SPI tree, screenshot and
# input endpoints through the Podman-managed machine.
"$project_dir/tests/acceptance/guest-cua.sh" \
    "$machine_dir" "$machine" get_desktop_state \
    '{"screenshot_out_file":"/tmp/buzzardos-cua-acceptance.png"}' \
    >"$artifact_dir/cua-desktop-state.json"
podman exec --user user "$container" test -s /tmp/buzzardos-cua-acceptance.png

# An unchanged restart must retain the same Podman container ID and rootfs.
marker="buzzardos-persistent-$(date +%s)-$$"
sudo_guest sh -ceu "printf '%s\\n' '$marker' >/var/lib/buzzardos-persistence-marker"
wb restart "$machine"
wait_state running
[[ $(podman inspect --format '{{.Id}}' "$container") == "$container_id" ]]
podman exec "$container" grep -qx "$marker" /var/lib/buzzardos-persistence-marker

# Stop, export, clone, and import-clone through the public launcher. Both new
# machines must receive new identities while preserving the exported data.
wb stop "$machine"
wait_state stopped
export_path=$artifact_dir/$machine.oci.tar
wb export "$machine" --output "$export_path"
sha256sum "$export_path" >"$export_path.sha256"

clone_name="$machine-clone-$$"
clone_dir=$(readlink -m -- "$artifact_dir/$clone_name")
"$launcher" --machine-dir "$clone_dir" clone "$machine" "$clone_name" --share "$shared_dir"
clone_container=$(container_for "$clone_dir")
"$launcher" --machine-dir "$clone_dir" start "$clone_name" --detach
clone_runtime=$clone_dir/runtime.json
deadline=$((SECONDS + 120))
while ((SECONDS < deadline)); do
    [[ $(jq -r '.state // empty' "$clone_runtime" 2>/dev/null || true) == running ]] && break
    sleep 0.2
done
[[ $(jq -r .state "$clone_runtime") == running ]]
podman exec "$clone_container" grep -qx "$marker" /var/lib/buzzardos-persistence-marker
[[ $(jq -r .id "$clone_dir/machine.json") != $(jq -r .id "$config") ]]
"$launcher" --machine-dir "$clone_dir" stop "$clone_name"
"$launcher" --machine-dir "$clone_dir" delete "$clone_name" --yes

import_name="$machine-import-$$"
import_dir=$(readlink -m -- "$artifact_dir/$import_name")
"$launcher" --machine-dir "$import_dir" import "$export_path" \
    --name "$import_name" --mode clone --share "$shared_dir"
import_container=$(container_for "$import_dir")
"$launcher" --machine-dir "$import_dir" start "$import_name" --detach
import_runtime=$import_dir/runtime.json
deadline=$((SECONDS + 120))
while ((SECONDS < deadline)); do
    [[ $(jq -r '.state // empty' "$import_runtime" 2>/dev/null || true) == running ]] && break
    sleep 0.2
done
[[ $(jq -r .state "$import_runtime") == running ]]
podman exec "$import_container" grep -qx "$marker" /var/lib/buzzardos-persistence-marker
[[ $(jq -r .id "$import_dir/machine.json") != $(jq -r .id "$config") ]]
"$launcher" --machine-dir "$import_dir" stop "$import_name"
"$launcher" --machine-dir "$import_dir" delete "$import_name" --yes

# Leave the original machine in its normal running state.
wb start "$machine" --detach
wait_state running
jq -n \
    --arg container "$container" \
    --arg container_id "$container_id" \
    --arg export "$export_path" \
    '{schema:1, container:$container, persistent_container_id:$container_id, export:$export, passed:true}' \
    >"$artifact_dir/result.json"

echo "hardware acceptance passed: $artifact_dir"
