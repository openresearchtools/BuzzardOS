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
guest_version=$(tr -d '\n' <"$project_dir/guest/GUEST_VERSION")
desktop_version=$(tr -d '\n' <"$project_dir/guest/DESKTOP_VERSION")
cua_version=$(tr -d '\n' <"$project_dir/cua/VERSION")

case "$version:$guest_version:$desktop_version:$cua_version" in
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
    case "$item" in all|host|guest|desktop|cua) ;; *) echo "unknown package selection: $item" >&2; exit 2 ;; esac
done

mkdir -p "$build_root" "$output_dir"
if [[ "$output_dir/" == "$project_dir/"* ]]; then
    echo "refusing to place generated Debian packages inside the repository: $output_dir" >&2
    exit 1
fi
for command_name in cargo curl dpkg-deb install rustc sha256sum strip; do
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
    [[ "$(workspace_version "$project_dir/guest/Cargo.toml")" == "$guest_version" ]]
fi
if want desktop; then
    [[ "$(workspace_version "$project_dir/guest/Cargo.toml")" == "$desktop_version" ]]
fi
if want cua; then
    [[ "$(workspace_version "$project_dir/cua/Cargo.toml")" == "$cua_version" ]]
fi

write_control() {
    local root=$1 package=$2 package_version=$3 depends=$4 description=$5 recommends=${6:-}
    install -d -m 0755 "$root/DEBIAN"
    cat >"$root/DEBIAN/control" <<EOF
Package: $package
Version: $package_version
Architecture: amd64
Maintainer: Open Research Tools <maintainers@openresearchtools.org>
Section: utils
Priority: optional
Depends: $depends
EOF
    if [[ -n "$recommends" ]]; then
        printf 'Recommends: %s\n' "$recommends" >>"$root/DEBIAN/control"
    fi
    cat >>"$root/DEBIAN/control" <<EOF
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

install_rust_licensing() {
    local root=$1 package=$2 sysroot notice expected mpl_cache
    sysroot=$(rustc --print sysroot)
    notice="$sysroot/share/doc/rust/COPYRIGHT-library.html"
    expected=$(sed -n 's/^standard_library_notice_sha256 = "\([0-9a-f]*\)"/\1/p' \
        "$project_dir/LICENSES/rust-runtime.toml")
    [[ -s "$notice" && "$expected" =~ ^[0-9a-f]{64}$ ]] || {
        echo 'Rust standard-library licensing evidence is unavailable' >&2
        exit 1
    }
    printf '%s  %s\n' "$expected" "$notice" | sha256sum --check --status || {
        echo 'Rust standard-library notice differs from LICENSES/rust-runtime.toml' >&2
        exit 1
    }
    install -D -m 0644 "$notice" \
        "$root/usr/share/doc/$package/rust/COPYRIGHT-library.html"

    mpl_cache="$build_root/mpl-sources"
    "$project_dir/tools/fetch-mpl-sources.sh" "$mpl_cache"
    install -d -m 0755 "$root/usr/share/doc/$package/sources/mpl"
    install -m 0644 "$mpl_cache"/*.crate \
        "$root/usr/share/doc/$package/sources/mpl/"
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
    install -D -m 0644 "$project_dir/host/LICENSE" \
        "$root/usr/share/doc/buzzardos/COMPONENT-LICENSE"
    install -D -m 0644 "$project_dir/host/CHANGELOG.md" \
        "$root/usr/share/doc/buzzardos/changelog"
    install -D -m 0644 "$project_dir/NOTICE" "$root/usr/share/doc/buzzardos/NOTICE"
    install -D -m 0644 "$project_dir/THIRD_PARTY_NOTICES.md" \
        "$root/usr/share/doc/buzzardos/THIRD_PARTY_NOTICES.md"
    install -D -m 0644 "$project_dir/LICENSES/generated/RUST_DEPENDENCY_LICENSES.txt" \
        "$root/usr/share/doc/buzzardos/RUST_DEPENDENCY_LICENSES.txt"
    install -D -m 0644 "$project_dir/LICENSES/generated/cargo-host.tsv" \
        "$root/usr/share/doc/buzzardos/cargo-host.tsv"
    install_rust_licensing "$root" buzzardos
    install -d -m 0755 "$root/usr/share/buzzardos"
    printf '%s\n' "$version" >"$root/usr/share/buzzardos/version"
    write_control "$root" buzzardos "$version" \
        'bubblewrap, buildah, gstreamer1.0-pipewire, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-tools, libglib2.0-0t64 | libglib2.0-0, libgtk-4-1 (>= 4.14), libwayland-client0, libxkbcommon0, pipewire-bin, slirp4netns, tar, uidmap, util-linux, xkb-data' \
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
        --locked --release --manifest-path "$project_dir/guest/Cargo.toml" \
        --package buzzardos-clipboard-agent
    local root="$build_root/root-buzzardos-guest"
    rm -rf -- "$root"
    install -d -m 0755 "$root"
    "$project_dir/guest/install-rootfs-assets.sh" \
        "$root" \
        "$guest_target/release/buzzardos-clipboard-agent"
    install -D -m 0644 "$project_dir/LICENSE" \
        "$root/usr/share/doc/buzzardos-guest/copyright"
    install -D -m 0644 "$project_dir/guest/packages/buzzardos-guest/LICENSE" \
        "$root/usr/share/doc/buzzardos-guest/COMPONENT-LICENSE"
    install -D -m 0644 "$project_dir/guest/packages/buzzardos-guest/README.md" \
        "$root/usr/share/doc/buzzardos-guest/README"
    install -D -m 0644 "$project_dir/guest/packages/buzzardos-guest/CHANGELOG.md" \
        "$root/usr/share/doc/buzzardos-guest/changelog"
    install -D -m 0644 "$project_dir/NOTICE" \
        "$root/usr/share/doc/buzzardos-guest/NOTICE"
    install -D -m 0644 "$project_dir/THIRD_PARTY_NOTICES.md" \
        "$root/usr/share/doc/buzzardos-guest/THIRD_PARTY_NOTICES.md"
    install -D -m 0644 "$project_dir/LICENSES/generated/RUST_DEPENDENCY_LICENSES.txt" \
        "$root/usr/share/doc/buzzardos-guest/RUST_DEPENDENCY_LICENSES.txt"
    install -D -m 0644 "$project_dir/LICENSES/generated/cargo-guest.tsv" \
        "$root/usr/share/doc/buzzardos-guest/cargo-guest.tsv"
    install_rust_licensing "$root" buzzardos-guest
    install -d -m 0755 "$root/usr/share/buzzardos-guest"
    printf '%s\n' "$guest_version" >"$root/usr/share/buzzardos-guest/version"
    write_control "$root" buzzardos-guest "$guest_version" \
        'at-spi2-core, dbus, dbus-user-session, dbus-x11, ffmpeg, fuse3, grim, gstreamer1.0-pipewire, gstreamer1.0-plugins-bad, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-tools, libfuse2t64 | libfuse2, libgbm1, libgl1, libgl1-mesa-dri, libglib2.0-bin, libgtk-3-0t64 | libgtk-3-0, libnotify-bin, libnss3, libpulse0, libwayland-client0, libxkbcommon0, mesa-vulkan-drivers, pipewire, pipewire-alsa, pipewire-pulse, pkexec, polkitd, python3, qt6-gtk-platformtheme, qt6-svg-plugins, qt6-wayland, slurp, squashfs-tools, sudo, sway (>= 1.9), systemd, systemd-sysv, unattended-upgrades, util-linux, wireplumber, wlr-randr, wtype, xdg-desktop-portal, xdg-desktop-portal-gtk, xdg-desktop-portal-wlr, xkb-data, xwayland' \
        'Buzzard OS guest session, integration, and persistent-machine mechanics'
    cat >"$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
command -v systemctl >/dev/null 2>&1 && systemctl daemon-reload >/dev/null 2>&1 || true
exit 0
EOF
    chmod 0755 "$root/DEBIAN/postinst"
    finish_package "$root" buzzardos-guest "$guest_version"
}

build_desktop() {
    CARGO_TARGET_DIR="$guest_target" cargo build \
        --locked --release --manifest-path "$project_dir/guest/Cargo.toml" \
        --package buzzardos-desktop \
        --package buzzardos-settings \
        --package buzzardos-shortcut-helper
    local root="$build_root/root-buzzardos-desktop"
    rm -rf -- "$root"
    install -d -m 0755 "$root"
    "$project_dir/guest/install-desktop-assets.sh" \
        "$root" \
        "$guest_target/release/buzzardos-desktop" \
        "$guest_target/release/buzzardos-settings" \
        "$guest_target/release/buzzardos-shortcut-helper"
    install -D -m 0644 "$project_dir/LICENSE" \
        "$root/usr/share/doc/buzzardos-desktop/copyright"
    install -D -m 0644 "$project_dir/guest/packages/buzzardos-desktop/LICENSE" \
        "$root/usr/share/doc/buzzardos-desktop/COMPONENT-LICENSE"
    install -D -m 0644 "$project_dir/guest/packages/buzzardos-desktop/README.md" \
        "$root/usr/share/doc/buzzardos-desktop/README"
    install -D -m 0644 "$project_dir/guest/packages/buzzardos-desktop/CHANGELOG.md" \
        "$root/usr/share/doc/buzzardos-desktop/changelog"
    install -D -m 0644 "$project_dir/NOTICE" \
        "$root/usr/share/doc/buzzardos-desktop/NOTICE"
    install -D -m 0644 "$project_dir/THIRD_PARTY_NOTICES.md" \
        "$root/usr/share/doc/buzzardos-desktop/THIRD_PARTY_NOTICES.md"
    install -D -m 0644 "$project_dir/LICENSES/generated/RUST_DEPENDENCY_LICENSES.txt" \
        "$root/usr/share/doc/buzzardos-desktop/RUST_DEPENDENCY_LICENSES.txt"
    install -D -m 0644 "$project_dir/LICENSES/generated/cargo-guest.tsv" \
        "$root/usr/share/doc/buzzardos-desktop/cargo-guest.tsv"
    install_rust_licensing "$root" buzzardos-desktop
    install -d -m 0755 "$root/usr/share/buzzardos-desktop"
    printf '%s\n' "$desktop_version" >"$root/usr/share/buzzardos-desktop/version"
    write_control "$root" buzzardos-desktop "$desktop_version" \
        "buzzardos-guest (>= $guest_version), dconf-gsettings-backend, firefox-esr, fonts-dejavu-core, fonts-noto-cjk, fonts-noto-color-emoji, fonts-noto-core, foot, gsettings-desktop-schemas, libglib2.0-0t64 | libglib2.0-0, libgtk-4-1, mako-notifier, mousepad, thunar, xdg-utils" \
        'Buzzard OS optional classic desktop, Settings, themes, and file-manager integration'
    cat >"$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q /usr/share/icons/BuzzardOS || true
exit 0
EOF
    chmod 0755 "$root/DEBIAN/postinst"
    finish_package "$root" buzzardos-desktop "$desktop_version"
}

build_cua() {
    CARGO_TARGET_DIR="$cua_target" cargo build \
        --locked --release \
        --manifest-path "$project_dir/cua/Cargo.toml"
    local root="$build_root/root-buzzardoscua"
    rm -rf -- "$root"
    install -D -m 0755 "$cua_target/release/cua" "$root/usr/bin/cua"
    ln -s cua "$root/usr/bin/cua1"
    local index
    for index in $(seq 2 64); do
        ln -s cua "$root/usr/bin/cua$index"
    done
    ln -s cua "$root/usr/bin/buzzardoscua"
    install -D -m 0644 "$project_dir/cua/LICENSE.trycua.md" \
        "$root/usr/share/doc/buzzardoscua/LICENSE.trycua-cua.md"
    install -D -m 0644 "$project_dir/LICENSE" \
        "$root/usr/share/doc/buzzardoscua/copyright"
    install -D -m 0644 "$project_dir/guest/packages/buzzardoscua/LICENSE" \
        "$root/usr/share/doc/buzzardoscua/COMPONENT-LICENSE"
    install -D -m 0644 "$project_dir/guest/packages/buzzardoscua/README.md" \
        "$root/usr/share/doc/buzzardoscua/README"
    install -D -m 0644 "$project_dir/guest/packages/buzzardoscua/CHANGELOG.md" \
        "$root/usr/share/doc/buzzardoscua/changelog"
    install -D -m 0644 "$project_dir/NOTICE" \
        "$root/usr/share/doc/buzzardoscua/NOTICE"
    install -D -m 0644 "$project_dir/LICENSES/generated/RUST_DEPENDENCY_LICENSES.txt" \
        "$root/usr/share/doc/buzzardoscua/RUST_DEPENDENCY_LICENSES.txt"
    install -D -m 0644 "$project_dir/LICENSES/generated/cargo-cua.tsv" \
        "$root/usr/share/doc/buzzardoscua/cargo-cua.tsv"
    install_rust_licensing "$root" buzzardoscua
    install -D -m 0644 "$project_dir/cua/UPSTREAM.toml" \
        "$root/usr/share/doc/buzzardoscua/UPSTREAM.toml"
    install -D -m 0644 "$project_dir/cua/CHANGES.BUZZARDOS.md" \
        "$root/usr/share/doc/buzzardoscua/CHANGES.BUZZARDOS.md"
    install -D -m 0644 "$project_dir/cua/CITATION.cff" \
        "$root/usr/share/doc/buzzardoscua/CITATION.cff"
    install -D -m 0644 \
        "$project_dir/cua/assets/cursor/Inter-OFL.txt" \
        "$root/usr/share/doc/buzzardoscua/Inter-OFL.txt"
    install -D -m 0644 \
        "$project_dir/cua/protocol/virtual-keyboard-unstable-v1.xml" \
        "$root/usr/share/doc/buzzardoscua/virtual-keyboard-unstable-v1.xml"
    install -D -m 0644 "$project_dir/cua/Skills/buzzard-cua/SKILL.md" \
        "$root/usr/share/buzzardoscua/SKILL.md"
    install -d -m 0755 "$root/usr/share/buzzardoscua"
    printf '%s\n' "$cua_version" >"$root/usr/share/buzzardoscua/version"
    write_control "$root" buzzardoscua "$cua_version" \
        'at-spi2-core, grim, libc6, libgcc-s1, libx11-6, libxfixes3, libxi6, libxkbcommon0, libxrandr2, libxtst6, sway (>= 1.9), xdg-utils' \
        'Buzzard CUA daemonless computer-use command for numbered Sway workspaces'
    finish_package "$root" buzzardoscua "$cua_version"
}

if want host; then build_host; fi
if want guest; then build_guest; fi
if want desktop; then build_desktop; fi
if want cua; then build_cua; fi
