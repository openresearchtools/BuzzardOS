#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

package=${1:?usage: test-host-package-matrix.sh /path/to/buzzardos_VERSION_ARCH.deb}
package=$(realpath -- "$package")
test -s "$package"

container_runtime=${BUZZARDOS_CONTAINER_RUNTIME:-}
if [[ -z "$container_runtime" ]]; then
    if command -v podman >/dev/null 2>&1; then
        container_runtime=podman
    elif command -v docker >/dev/null 2>&1; then
        container_runtime=docker
    else
        echo "host package matrix requires Podman or Docker" >&2
        exit 1
    fi
fi
command -v "$container_runtime" >/dev/null 2>&1 || {
    echo "container runtime is unavailable: $container_runtime" >&2
    exit 1
}

for image in \
    docker.io/library/ubuntu:24.04 \
    docker.io/library/debian:13 \
    docker.io/library/ubuntu:26.04; do
    echo "Installing Buzzard OS host package in $image"
    "$container_runtime" run --rm \
        --volume "$package:/tmp/buzzardos.deb:ro" \
        "$image" \
        sh -euc '
            export DEBIAN_FRONTEND=noninteractive
            apt-get -qq update >/tmp/apt.log 2>&1 || { cat /tmp/apt.log >&2; exit 1; }
            apt-get install --yes --no-install-recommends -qq \
                -o Dpkg::Use-Pty=0 /tmp/buzzardos.deb >>/tmp/apt.log 2>&1 || {
                cat /tmp/apt.log >&2
                exit 1
            }
            version=$(dpkg-query -W -f="\${Version}" buzzardos)
            test "$(buzzardos --version)" = "Buzzard OS $version"
            /usr/libexec/buzzardos/buzzardos-display --help >/dev/null
            podman --version >/dev/null
            buildah --version >/dev/null
            test -s /usr/share/doc/buzzardos/copyright
            test -s /usr/share/applications/org.openresearchtools.buzzardos.desktop
            test -s /usr/share/metainfo/org.openresearchtools.buzzardos.metainfo.xml
            test -s /usr/share/icons/hicolor/256x256/apps/buzzardos.png
            echo "Buzzard OS $version package smoke passed"
        '
done
