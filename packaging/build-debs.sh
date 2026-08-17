#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

packaging_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(CDPATH= cd -- "$packaging_dir/.." && pwd)
task_uid=$(id -u)
build_root=${BUZZARDOS_DEB_BUILD_ROOT:-${TMPDIR:-/tmp}/buzzardos-deb-build-$task_uid}
output_dir=${BUZZARDOS_DEB_OUTPUT_DIR:-$build_root/output}
host_target=${BUZZARDOS_HOST_TARGET_DIR:-$build_root/target-host}
guest_target=${BUZZARDOS_GUEST_TARGET_DIR:-$build_root/target-guest}
cua_target=${BUZZARDOS_CUA_TARGET_DIR:-$build_root/target-cua}
version=$(tr -d '\n' <"$project_dir/VERSION")
cua_version=$(tr -d '\n' <"$project_dir/guest/BUZZARDCUA_VERSION")

case "$version:$cua_version" in
    *[!A-Za-z0-9.+:~_-]*) echo 'invalid Debian package version' >&2; exit 1 ;;
esac

requested=("${@:-all}")
want() {
    local candidate=$1 item
    for item in "${requested[@]}"; do
        [[ "$item" == all || "$item" == "$candidate" ]] && return 0
    done
    return 1
}
for item in "${requested[@]}"; do
    case "$item" in all|host|guest|cua) ;; *) echo "unknown package selection: $item" >&2; exit 2 ;; esac
done

mkdir -p "$build_root" "$output_dir"
if [[ "$output_dir/" == "$project_dir/"* ]]; then
    echo "refusing to place generated Debian packages inside the repository: $output_dir" >&2
    exit 1
fi
for command_name in cargo dpkg-deb install sha256sum strip; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "Debian package build dependency missing: $command_name" >&2
        exit 1
    }
done

workspace_version() {
    sed -n 's/^version = "\([^"]*\)"/\1/p' "$1" | head -n1
}
if want host; then
    [[ "$(workspace_version "$project_dir/host/Cargo.toml")" == "$version" ]]
fi
if want guest; then
    [[ "$(workspace_version "$project_dir/guest/Cargo.toml")" == "$version" ]]
fi

write_control() {
    local root=$1 package=$2 package_version=$3 depends=$4 description=$5
    install -d -m 0755 "$root/DEBIAN"
    cat >"$root/DEBIAN/control" <<EOF
Package: $package
Version: $package_version
Architecture: amd64
Maintainer: Open Research Tools <maintainers@openresearchtools.org>
Section: utils
Priority: optional
Depends: $depends
Homepage: https://github.com/openresearchtools/BuzzardOS
Description: $description
EOF
    chmod 0644 "$root/DEBIAN/control"
}

finish_package() {
    local root=$1 package=$2 package_version=$3 output
    output="$output_dir/${package}_${package_version}_amd64.deb"
    find "$root" -type d -exec chmod 0755 {} +
    dpkg-deb --root-owner-group --build "$root" "$output"
    dpkg-deb --info "$output" >/dev/null
    dpkg-deb --contents "$output" >/dev/null
    sha256sum "$output" >"$output.sha256"
    printf 'Built %s\n' "$output"
}

build_host() {
    CARGO_TARGET_DIR="$host_target" cargo build \
        --locked --release --manifest-path "$project_dir/host/Cargo.toml" --workspace
    local root="$build_root/root-buzzardos"
    rm -rf -- "$root"
    install -D -m 0755 "$host_target/release/buzzardos" "$root/usr/bin/buzzardos"
    ln -s buzzardos "$root/usr/bin/BuzzardOS"
    install -D -m 0755 "$host_target/release/buzzardos-broker" \
        "$root/usr/libexec/buzzardos/buzzardos-broker"
    install -D -m 0755 "$host_target/release/buzzardos-display" \
        "$root/usr/libexec/buzzardos/buzzardos-display"
    install -D -m 0755 "$packaging_dir/helpers/crane" \
        "$root/usr/libexec/buzzardos/crane"
    install -D -m 0644 "$project_dir/host/packaging/BuzzardOS.desktop" \
        "$root/usr/share/applications/org.openresearchtools.buzzardos.desktop"
    install -D -m 0644 "$project_dir/host/packaging/org.openresearchtools.BuzzardOS.metainfo.xml" \
        "$root/usr/share/metainfo/org.openresearchtools.buzzardos.metainfo.xml"
    local icon size
    for icon in "$project_dir"/host/packaging/icons/buzzardos-*.png; do
        size=${icon##*-}
        size=${size%.png}
        install -D -m 0644 "$icon" \
            "$root/usr/share/icons/hicolor/${size}x${size}/apps/buzzardos.png"
    done
    install -D -m 0644 "$project_dir/LICENSE" "$root/usr/share/doc/buzzardos/copyright"
    install -D -m 0644 "$project_dir/NOTICE" "$root/usr/share/doc/buzzardos/NOTICE"
    install -D -m 0644 "$project_dir/THIRD_PARTY_NOTICES.md" \
        "$root/usr/share/doc/buzzardos/THIRD_PARTY_NOTICES.md"
    install -D -m 0644 "$project_dir/LICENSES/generated/RUST_DEPENDENCY_LICENSES.txt" \
        "$root/usr/share/doc/buzzardos/RUST_DEPENDENCY_LICENSES.txt"
    install -D -m 0644 "$project_dir/LICENSES/generated/cargo-host.tsv" \
        "$root/usr/share/doc/buzzardos/cargo-host.tsv"
    install -d -m 0755 "$root/usr/share/buzzardos"
    printf '%s\n' "$version" >"$root/usr/share/buzzardos/version"
    write_control "$root" buzzardos "$version" \
        'bubblewrap, gstreamer1.0-pipewire, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-tools, libglib2.0-0t64 | libglib2.0-0, libgtk-4-1 (>= 4.14), libwayland-client0, libxkbcommon0, pipewire-bin, skopeo, slirp4netns, tar, uidmap, util-linux, xkb-data' \
        'Buzzard OS rootless persistent desktop-machine manager'
    cat >"$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q /usr/share/icons/hicolor || true
exit 0
EOF
    chmod 0755 "$root/DEBIAN/postinst"
    finish_package "$root" buzzardos "$version"
}

build_guest() {
    CARGO_TARGET_DIR="$guest_target" cargo build \
        --locked --release --manifest-path "$project_dir/guest/Cargo.toml" --workspace
    local root="$build_root/root-buzzardos-guest-desktop"
    rm -rf -- "$root"
    install -d -m 0755 "$root"
    "$project_dir/guest/install-rootfs-assets.sh" \
        "$root" \
        "$guest_target/release/buzzardos-shell" \
        "$guest_target/release/buzzardos-settings" \
        "$guest_target/release/buzzardos-shortcut-helper" \
        "$guest_target/release/buzzardos-clipboard-agent"
    install -D -m 0644 "$project_dir/LICENSE" \
        "$root/usr/share/doc/buzzardos-guest-desktop/copyright"
    install -D -m 0644 "$project_dir/NOTICE" \
        "$root/usr/share/doc/buzzardos-guest-desktop/NOTICE"
    install -D -m 0644 "$project_dir/THIRD_PARTY_NOTICES.md" \
        "$root/usr/share/doc/buzzardos-guest-desktop/THIRD_PARTY_NOTICES.md"
    install -D -m 0644 "$project_dir/LICENSES/generated/RUST_DEPENDENCY_LICENSES.txt" \
        "$root/usr/share/doc/buzzardos-guest-desktop/RUST_DEPENDENCY_LICENSES.txt"
    install -D -m 0644 "$project_dir/LICENSES/generated/cargo-guest.tsv" \
        "$root/usr/share/doc/buzzardos-guest-desktop/cargo-guest.tsv"
    install -d -m 0755 "$root/usr/share/buzzardos"
    printf '%s\n' "$version" >"$root/usr/share/buzzardos/package-version"
    write_control "$root" buzzardos-guest-desktop "$version" \
        'at-spi2-core, buzzardcua, dbus, dbus-user-session, dbus-x11, dconf-gsettings-backend, foot, fuse3, libfuse2t64 | libfuse2, libglib2.0-0t64 | libglib2.0-0, libgtk-4-1, libpulse0, libwayland-client0, libxkbcommon0, mako-notifier, mousepad, pipewire, pipewire-pulse, polkitd, python3, python3-apt, python3-gi, sway (>= 1.9), systemd, thunar, wireplumber, xdg-desktop-portal, xdg-desktop-portal-gtk, xdg-desktop-portal-wlr, xkb-data, xwayland' \
        'Buzzard OS guest desktop shell, Settings, services, and integration'
    cat >"$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload >/dev/null 2>&1 || true
exit 0
EOF
    chmod 0755 "$root/DEBIAN/postinst"
    finish_package "$root" buzzardos-guest-desktop "$version"
}

build_cua() {
    CARGO_TARGET_DIR="$cua_target" cargo build \
        --locked --release \
        --manifest-path "$project_dir/guest/third_party/trycua-cua/cua-driver/rust/Cargo.toml" \
        --package cua-driver
    local root="$build_root/root-buzzardcua"
    rm -rf -- "$root"
    install -D -m 0755 "$cua_target/release/cua-driver" "$root/usr/bin/buzzardcua"
    install -D -m 0644 "$project_dir/guest/third_party/trycua-cua/LICENSE.md" \
        "$root/usr/share/doc/buzzardcua/LICENSE.trycua-cua.md"
    install -D -m 0644 "$project_dir/LICENSE" \
        "$root/usr/share/doc/buzzardcua/copyright"
    install -D -m 0644 "$project_dir/NOTICE" \
        "$root/usr/share/doc/buzzardcua/NOTICE"
    install -D -m 0644 "$project_dir/LICENSES/generated/RUST_DEPENDENCY_LICENSES.txt" \
        "$root/usr/share/doc/buzzardcua/RUST_DEPENDENCY_LICENSES.txt"
    install -D -m 0644 "$project_dir/LICENSES/generated/cargo-cua.tsv" \
        "$root/usr/share/doc/buzzardcua/cargo-cua.tsv"
    install -D -m 0644 "$project_dir/guest/third_party/trycua-cua/UPSTREAM.toml" \
        "$root/usr/share/doc/buzzardcua/UPSTREAM.toml"
    install -D -m 0644 "$project_dir/guest/third_party/trycua-cua/CHANGES.BUZZARDOS.md" \
        "$root/usr/share/doc/buzzardcua/CHANGES.BUZZARDOS.md"
    install -D -m 0644 "$project_dir/guest/third_party/trycua-cua/CITATION.cff" \
        "$root/usr/share/doc/buzzardcua/CITATION.cff"
    install -D -m 0644 \
        "$project_dir/guest/third_party/trycua-cua/cua-driver/rust/crates/cursor-overlay/assets/Inter-OFL.txt" \
        "$root/usr/share/doc/buzzardcua/Inter-OFL.txt"
    install -D -m 0644 \
        "$project_dir/guest/third_party/trycua-cua/cua-driver/rust/crates/platform-linux/protocol/virtual-keyboard-unstable-v1.xml" \
        "$root/usr/share/doc/buzzardcua/virtual-keyboard-unstable-v1.xml"
    install -d -m 0755 "$root/usr/share/buzzardcua"
    printf '%s\n' "$cua_version" >"$root/usr/share/buzzardcua/version"
    write_control "$root" buzzardcua "$cua_version" \
        'libc6, libgcc-s1, libx11-6, libxfixes3, libxi6, libxkbcommon0, libxrandr2, libxtst6' \
        'Buzzard CUA in-guest computer-use automation service'
    finish_package "$root" buzzardcua "$cua_version"
}

want host && build_host
want guest && build_guest
want cua && build_cua
