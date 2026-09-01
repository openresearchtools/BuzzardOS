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
nvidia_toolkit_version=1.19.1-1
nvidia_toolkit_base_sha256=b6c5b4e77a28cde0197cc0e64edf75538604775d9f8aea502cef667e7e5b2132
nvidia_container_tools_sha256=5642763d51961a2295dff09990048a5dcee81edbea2a8c5084e47b09ccf17268
nvidia_container_library_sha256=d73bb582af893135198ef81cb22135c790a75d2ad72910446477c6c4430f3e6b

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

download_verified() {
    local url=$1 destination=$2 expected=$3
    install -d -m 0755 "$(dirname -- "$destination")"
    if [[ ! -f "$destination" ]] ||
        ! printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --status; then
        curl --fail --location --retry 3 --output "$destination.tmp" "$url"
        printf '%s  %s\n' "$expected" "$destination.tmp" | sha256sum --check --strict
        mv -f -- "$destination.tmp" "$destination"
    fi
}

mkdir -p "$build_root" "$output_dir"
if [[ "$output_dir/" == "$project_dir/"* ]]; then
    echo "refusing to place generated Debian packages inside the repository: $output_dir" >&2
    exit 1
fi
for command_name in cargo curl dpkg-deb gzip install md5sum rustc sha256sum strip; do
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
    [[ "$(workspace_version "$project_dir/guest/shell/Cargo.toml")" == "$desktop_version" ]]
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
 This package is one independently versioned component of Buzzard OS.
EOF
    chmod 0644 "$root/DEBIAN/control"
}

finish_package() {
    local root=$1 package=$2 package_version=$3 output filename
    local changelog="$root/usr/share/doc/$package/changelog.Debian"
    install -d -m 0755 "$(dirname -- "$changelog")"
    cat >"$changelog" <<EOF
$package ($package_version) stable; urgency=medium

  * Publish the independently versioned Buzzard OS package.

 -- Open Research Tools <maintainers@openresearchtools.org>  Sat, 29 Aug 2026 00:00:00 +0000
EOF
    gzip -n -9 "$changelog"
    if [[ -d "$root/etc" ]]; then
        (
            cd "$root"
            find etc -type f -printf '/%p\n' | LC_ALL=C sort
        ) >"$root/DEBIAN/conffiles"
        [[ -s "$root/DEBIAN/conffiles" ]] || rm -f "$root/DEBIAN/conffiles"
    fi
    (
        cd "$root"
        find . -type f ! -path './DEBIAN/*' -print0 \
            | LC_ALL=C sort -z \
            | xargs -0 md5sum
    ) >"$root/DEBIAN/md5sums"
    output="$output_dir/${package}_${package_version}_amd64.deb"
    filename=${output##*/}
    find "$root" -type d -exec chmod 0755 {} +
    dpkg-deb --root-owner-group --build "$root" "$output"
    dpkg-deb --info "$output" >/dev/null
    dpkg-deb --contents "$output" >/dev/null
    (cd "$output_dir" && sha256sum "$filename") >"$output.sha256"
    printf 'Built %s\n' "$output"
}

install_upstream_changelog() {
    local root=$1 package=$2 source=$3 destination
    destination="$root/usr/share/doc/$package/changelog.gz"
    install -d -m 0755 "$(dirname -- "$destination")"
    gzip -n -9 -c "$source" >"$destination"
    chmod 0644 "$destination"
}

install_rust_licensing() {
    local root=$1 package=$2 inventory=$3 sysroot notice expected mpl_cache
    [[ -s "$inventory" ]] || {
        echo "Rust dependency inventory is unavailable: $inventory" >&2
        exit 1
    }
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

    if awk -F '\t' 'NR > 2 && $6 ~ /(^| OR | AND )MPL-2\.0($| OR | AND )/ { found=1 } END { exit !found }' \
        "$inventory"; then
        mpl_cache="$build_root/mpl-sources"
        "$project_dir/tools/fetch-mpl-sources.sh" "$mpl_cache"
        install -d -m 0755 "$root/usr/share/doc/$package/sources/mpl"
        while IFS=$'\t' read -r name crate_version _source _checksum _license normalized _rest; do
            [[ "$name" != name && "$name" != \#* ]] || continue
            [[ "$normalized" == *MPL-2.0* ]] || continue
            install -m 0644 "$mpl_cache/$name-$crate_version.crate" \
                "$root/usr/share/doc/$package/sources/mpl/"
        done <"$inventory"
    fi
}

stage_nvidia_toolkit() {
    local root=$1
    local package_dir="$build_root/package-inputs/nvidia-container-toolkit-$nvidia_toolkit_version"
    local base_deb="$package_dir/nvidia-container-toolkit-base_${nvidia_toolkit_version}_amd64.deb"
    local tools_deb="$package_dir/libnvidia-container-tools_${nvidia_toolkit_version}_amd64.deb"
    local library_deb="$package_dir/libnvidia-container1_${nvidia_toolkit_version}_amd64.deb"
    download_verified \
        "https://nvidia.github.io/libnvidia-container/stable/deb/amd64/$(basename "$base_deb")" \
        "$base_deb" "$nvidia_toolkit_base_sha256"
    download_verified \
        "https://nvidia.github.io/libnvidia-container/stable/deb/amd64/$(basename "$tools_deb")" \
        "$tools_deb" "$nvidia_container_tools_sha256"
    download_verified \
        "https://nvidia.github.io/libnvidia-container/stable/deb/amd64/$(basename "$library_deb")" \
        "$library_deb" "$nvidia_container_library_sha256"

    local extract="$build_root/nvidia-toolkit-extract"
    rm -rf -- "$extract"
    install -d -m 0755 "$extract"
    local package
    for package in "$base_deb" "$tools_deb" "$library_deb"; do
        dpkg-deb --extract "$package" "$extract"
    done

    install -D -m 0755 "$extract/usr/bin/nvidia-ctk" \
        "$root/usr/libexec/buzzardos/nvidia-ctk"
    install -D -m 0755 "$extract/usr/bin/nvidia-cdi-hook" \
        "$root/usr/libexec/buzzardos/nvidia-cdi-hook"
    install -D -m 0755 "$extract/usr/bin/nvidia-container-cli" \
        "$root/usr/libexec/buzzardos/nvidia-container-cli.real"
    install -D -m 0755 "$project_dir/host/packaging/buzzardos-nvidia-container-cli" \
        "$root/usr/libexec/buzzardos/nvidia-container-cli"
    install -D -m 0644 \
        "$extract/usr/lib/x86_64-linux-gnu/libnvidia-container.so.1.19.1" \
        "$root/usr/libexec/buzzardos/nvidia-libs/libnvidia-container.so.1.19.1"
    install -D -m 0644 \
        "$extract/usr/lib/x86_64-linux-gnu/libnvidia-container-go.so.1.19.1" \
        "$root/usr/libexec/buzzardos/nvidia-libs/libnvidia-container-go.so.1.19.1"
    ln -s libnvidia-container.so.1.19.1 \
        "$root/usr/libexec/buzzardos/nvidia-libs/libnvidia-container.so.1"
    ln -s libnvidia-container-go.so.1.19.1 \
        "$root/usr/libexec/buzzardos/nvidia-libs/libnvidia-container-go.so.1"

    local documentation="$root/usr/share/doc/buzzardos/licenses/nvidia"
    for package in \
        nvidia-container-toolkit-base \
        libnvidia-container-tools \
        libnvidia-container1; do
        install -D -m 0644 "$extract/usr/share/doc/$package/copyright" \
            "$documentation/$package.copyright"
        install -D -m 0644 "$extract/usr/share/doc/$package/changelog.Debian.gz" \
            "$documentation/$package.changelog.Debian.gz"
    done
    install -D -m 0644 "$project_dir/LICENSES/nvidia-go-dependencies.toml" \
        "$root/usr/share/doc/buzzardos/nvidia-go-dependencies.toml"
    install -D -m 0644 "$project_dir/LICENSES/go-runtime.toml" \
        "$root/usr/share/doc/buzzardos/go-runtime.toml"
    install -D -m 0644 "$project_dir/LICENSES/go-source-archives.tsv" \
        "$root/usr/share/doc/buzzardos/go-source-archives.tsv"
    local go_sources="$build_root/go-sources"
    rm -rf -- "$go_sources"
    "$project_dir/tools/fetch-go-source-archives.sh" "$go_sources"
    install -d -m 0755 "$root/usr/share/doc/buzzardos/sources/go"
    cp -a "$go_sources/." "$root/usr/share/doc/buzzardos/sources/go/"
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
    stage_nvidia_toolkit "$root"
    install -D -m 0644 "$project_dir/host/packaging/BuzzardOS.desktop" \
        "$root/usr/share/applications/org.openresearchtools.buzzardos.desktop"
    install -D -m 0644 "$project_dir/host/packaging/org.openresearchtools.BuzzardOS.metainfo.xml" \
        "$root/usr/share/metainfo/org.openresearchtools.buzzardos.metainfo.xml"
    install -D -m 0644 "$project_dir/host/packaging/buzzardos.apparmor" \
        "$root/etc/apparmor.d/usr.libexec.buzzardos"
    local icon size
    for icon in "$project_dir"/host/packaging/icons/buzzardos-*.png; do
        size=${icon##*-}
        size=${size%.png}
        install -D -m 0644 "$icon" \
            "$root/usr/share/icons/hicolor/${size}x${size}/apps/buzzardos.png"
    done
    install -D -m 0644 "$project_dir/packaging/copyright/buzzardos" \
        "$root/usr/share/doc/buzzardos/copyright"
    install -D -m 0644 "$project_dir/LICENSE" \
        "$root/usr/share/doc/buzzardos/LICENSE"
    install -D -m 0644 "$project_dir/host/LICENSE" \
        "$root/usr/share/doc/buzzardos/COMPONENT-LICENSE"
    install_upstream_changelog "$root" buzzardos "$project_dir/host/CHANGELOG.md"
    install -D -m 0644 "$project_dir/NOTICE" "$root/usr/share/doc/buzzardos/NOTICE"
    install -D -m 0644 "$project_dir/LICENSES/package-notices/buzzardos.md" \
        "$root/usr/share/doc/buzzardos/THIRD_PARTY_NOTICES.md"
    install -D -m 0644 \
        "$project_dir/LICENSES/generated/RUST_DEPENDENCY_LICENSES.buzzardos.txt" \
        "$root/usr/share/doc/buzzardos/RUST_DEPENDENCY_LICENSES.txt"
    install -D -m 0644 "$project_dir/LICENSES/generated/cargo-host.tsv" \
        "$root/usr/share/doc/buzzardos/cargo-host.tsv"
    install_rust_licensing "$root" buzzardos \
        "$project_dir/LICENSES/generated/cargo-host.tsv"
    install -d -m 0755 "$root/usr/share/buzzardos"
    printf '%s\n' "$version" >"$root/usr/share/buzzardos/version"
    # Ship only Buzzard's guest-building recipes with the host manager. Guest
    # packages remain separate release artifacts and are never folded into the
    # host package or its licensing inventory.
    install -D -m 0644 "$project_dir/oci/desktop/Containerfile" \
        "$root/usr/share/buzzardos/containerfiles/desktop/Containerfile"
    install -D -m 0644 "$project_dir/oci/desktop/Containerfile.cuda" \
        "$root/usr/share/buzzardos/containerfiles/desktop/Containerfile.cuda"
    install -D -m 0755 "$project_dir/oci/desktop/provision-image.sh" \
        "$root/usr/share/buzzardos/containerfiles/desktop/provision-image.sh"
    install -D -m 0644 "$project_dir/oci/desktop/apt/debian-sid-snapshot.sources" \
        "$root/usr/share/buzzardos/containerfiles/desktop/apt/debian-sid-snapshot.sources"
    install -D -m 0644 "$project_dir/oci/desktop/apt/debian-sid-live.sources" \
        "$root/usr/share/buzzardos/containerfiles/desktop/apt/debian-sid-live.sources"
    install -D -m 0644 "$project_dir/oci/desktop/apt/99buzzardos-snapshot" \
        "$root/usr/share/buzzardos/containerfiles/desktop/apt/99buzzardos-snapshot"
    install -d -m 0755 "$root/usr/share/lintian/overrides"
    cat >"$root/usr/share/lintian/overrides/buzzardos" <<'EOF'
# Lintian's byte signature identifies Rust's separately inventoried
# miniz_oxide implementation as embedded C zlib. No zlib source or library is
# copied into this package; the exact Rust crate/license closure is shipped in
# /usr/share/doc/buzzardos.
buzzardos: embedded-library zlib [usr/libexec/buzzardos/buzzardos-display]
EOF
    write_control "$root" buzzardos "$version" \
        'apparmor, bubblewrap, buildah, gstreamer1.0-pipewire, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-tools, libcap2, libc6, libgcc-s1, libglib2.0-0t64 | libglib2.0-0, libgtk-4-1 (>= 4.14), libseccomp2, libwayland-client0, libxkbcommon0, passt, pipewire-bin, slirp4netns, uidmap, xkb-data' \
        'Buzzard OS rootless persistent desktop-machine manager'
    cat >"$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database -q || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q /usr/share/icons/hicolor || true
if [ -d /sys/kernel/security/apparmor ]; then
    apparmor_parser -r -T -W /etc/apparmor.d/usr.libexec.buzzardos
fi
exit 0
EOF
    chmod 0755 "$root/DEBIAN/postinst"
    cat >"$root/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
case "$1" in
    remove|deconfigure)
        if [ -d /sys/kernel/security/apparmor ]; then
            apparmor_parser -R /etc/apparmor.d/usr.libexec.buzzardos || true
        fi
        ;;
esac
exit 0
EOF
    chmod 0755 "$root/DEBIAN/prerm"
    finish_package "$root" buzzardos "$version"
}

build_guest() {
    CARGO_TARGET_DIR="$guest_target" cargo build \
        --locked --release --manifest-path "$project_dir/guest/Cargo.toml" \
        --package buzzardos-clipboard-agent \
        --package buzzardos-sudo-bridge
    local root="$build_root/root-buzzardos-guest"
    rm -rf -- "$root"
    install -d -m 0755 "$root"
    "$project_dir/guest/install-rootfs-assets.sh" \
        "$root" \
        "$guest_target/release/buzzardos-clipboard-agent" \
        "$guest_target/release/buzzardos-sudo"
    install -D -m 0644 "$project_dir/packaging/copyright/buzzardos-guest" \
        "$root/usr/share/doc/buzzardos-guest/copyright"
    install -D -m 0644 "$project_dir/LICENSE" \
        "$root/usr/share/doc/buzzardos-guest/LICENSE"
    install -D -m 0644 "$project_dir/guest/packages/buzzardos-guest/LICENSE" \
        "$root/usr/share/doc/buzzardos-guest/COMPONENT-LICENSE"
    install -D -m 0644 "$project_dir/guest/packages/buzzardos-guest/README.md" \
        "$root/usr/share/doc/buzzardos-guest/README"
    install_upstream_changelog "$root" buzzardos-guest \
        "$project_dir/guest/packages/buzzardos-guest/CHANGELOG.md"
    install -D -m 0644 "$project_dir/NOTICE" \
        "$root/usr/share/doc/buzzardos-guest/NOTICE"
    install -D -m 0644 "$project_dir/LICENSES/package-notices/buzzardos-guest.md" \
        "$root/usr/share/doc/buzzardos-guest/THIRD_PARTY_NOTICES.md"
    install -D -m 0644 \
        "$project_dir/LICENSES/generated/RUST_DEPENDENCY_LICENSES.buzzardos-guest.txt" \
        "$root/usr/share/doc/buzzardos-guest/RUST_DEPENDENCY_LICENSES.txt"
    install -D -m 0644 "$project_dir/LICENSES/generated/cargo-buzzardos-guest.tsv" \
        "$root/usr/share/doc/buzzardos-guest/cargo-dependencies.tsv"
    install_rust_licensing "$root" buzzardos-guest \
        "$project_dir/LICENSES/generated/cargo-buzzardos-guest.tsv"
    install -d -m 0755 "$root/usr/share/buzzardos-guest"
    printf '%s\n' "$guest_version" >"$root/usr/share/buzzardos-guest/version"
    write_control "$root" buzzardos-guest "$guest_version" \
        'at-spi2-core, dbus, dbus-user-session, dbus-x11, ffmpeg, fuse3, grim, gstreamer1.0-pipewire, gstreamer1.0-plugins-bad, gstreamer1.0-plugins-base, gstreamer1.0-plugins-good, gstreamer1.0-tools, libc6, libfuse2t64 | libfuse2, libgbm1, libgcc-s1, libgl1, libgl1-mesa-dri, libglib2.0-bin, libgtk-3-0t64 | libgtk-3-0, libnotify-bin, libnss3, libpulse0, libwayland-client0, libxkbcommon0, mesa-vulkan-drivers, pipewire, pipewire-alsa, pipewire-pulse, python3, qt6-gtk-platformtheme, qt6-svg-plugins, qt6-wayland, slurp, squashfs-tools, sudo, sway (>= 1.9), systemd, systemd-sysv, unattended-upgrades, wireplumber, wlr-randr, wtype, xdg-desktop-portal, xdg-desktop-portal-gtk, xdg-desktop-portal-wlr, xkb-data, xwayland' \
        'Buzzard OS guest session, integration, and persistent-machine mechanics'
    cat >"$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
rm -f /etc/polkit-1/rules.d/49-buzzardos-root.rules
if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload >/dev/null 2>&1 || true
    if [ -d /run/systemd/system ] && getent passwd user >/dev/null 2>&1; then
        systemctl start buzzardos-sudo.socket buzzardos-fusermount.socket
    fi
fi
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
    install -D -m 0644 "$project_dir/packaging/copyright/buzzardos-desktop" \
        "$root/usr/share/doc/buzzardos-desktop/copyright"
    install -D -m 0644 "$project_dir/LICENSE" \
        "$root/usr/share/doc/buzzardos-desktop/LICENSE"
    install -D -m 0644 "$project_dir/guest/packages/buzzardos-desktop/LICENSE" \
        "$root/usr/share/doc/buzzardos-desktop/COMPONENT-LICENSE"
    install -D -m 0644 "$project_dir/guest/packages/buzzardos-desktop/README.md" \
        "$root/usr/share/doc/buzzardos-desktop/README"
    install_upstream_changelog "$root" buzzardos-desktop \
        "$project_dir/guest/packages/buzzardos-desktop/CHANGELOG.md"
    install -D -m 0644 "$project_dir/NOTICE" \
        "$root/usr/share/doc/buzzardos-desktop/NOTICE"
    install -D -m 0644 "$project_dir/LICENSES/package-notices/buzzardos-desktop.md" \
        "$root/usr/share/doc/buzzardos-desktop/THIRD_PARTY_NOTICES.md"
    install -D -m 0644 \
        "$project_dir/LICENSES/generated/RUST_DEPENDENCY_LICENSES.buzzardos-desktop.txt" \
        "$root/usr/share/doc/buzzardos-desktop/RUST_DEPENDENCY_LICENSES.txt"
    install -D -m 0644 "$project_dir/LICENSES/generated/cargo-buzzardos-desktop.tsv" \
        "$root/usr/share/doc/buzzardos-desktop/cargo-dependencies.tsv"
    install_rust_licensing "$root" buzzardos-desktop \
        "$project_dir/LICENSES/generated/cargo-buzzardos-desktop.tsv"
    install -d -m 0755 "$root/usr/share/buzzardos-desktop"
    printf '%s\n' "$desktop_version" >"$root/usr/share/buzzardos-desktop/version"
    write_control "$root" buzzardos-desktop "$desktop_version" \
        "buzzardos-guest (>= $guest_version), dconf-gsettings-backend, firefox-esr, fonts-dejavu-core, fonts-noto-cjk, fonts-noto-color-emoji, fonts-noto-core, foot, gsettings-desktop-schemas, libc6, libgcc-s1, libglib2.0-0t64 | libglib2.0-0, libgtk-4-1, mako-notifier, mousepad, thunar, xdg-user-dirs, xdg-utils" \
        'Buzzard OS optional classic desktop and Settings'
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
    strip --strip-unneeded "$root/usr/bin/cua"
    ln -s cua "$root/usr/bin/cua1"
    local index
    for index in $(seq 2 64); do
        ln -s cua "$root/usr/bin/cua$index"
    done
    ln -s cua "$root/usr/bin/buzzardoscua"
    install -D -m 0644 "$project_dir/cua/LICENSE.trycua.md" \
        "$root/usr/share/doc/buzzardoscua/LICENSE.trycua-cua.md"
    install -D -m 0644 "$project_dir/packaging/copyright/buzzardoscua" \
        "$root/usr/share/doc/buzzardoscua/copyright"
    install -D -m 0644 "$project_dir/LICENSE" \
        "$root/usr/share/doc/buzzardoscua/LICENSE"
    install -D -m 0644 "$project_dir/guest/packages/buzzardoscua/LICENSE" \
        "$root/usr/share/doc/buzzardoscua/COMPONENT-LICENSE"
    install -D -m 0644 "$project_dir/guest/packages/buzzardoscua/README.md" \
        "$root/usr/share/doc/buzzardoscua/README"
    install_upstream_changelog "$root" buzzardoscua \
        "$project_dir/guest/packages/buzzardoscua/CHANGELOG.md"
    install -D -m 0644 "$project_dir/NOTICE" \
        "$root/usr/share/doc/buzzardoscua/NOTICE"
    install -D -m 0644 "$project_dir/LICENSES/package-notices/buzzardoscua.md" \
        "$root/usr/share/doc/buzzardoscua/THIRD_PARTY_NOTICES.md"
    install -D -m 0644 \
        "$project_dir/LICENSES/generated/RUST_DEPENDENCY_LICENSES.buzzardoscua.txt" \
        "$root/usr/share/doc/buzzardoscua/RUST_DEPENDENCY_LICENSES.txt"
    install -D -m 0644 "$project_dir/LICENSES/generated/cargo-cua.tsv" \
        "$root/usr/share/doc/buzzardoscua/cargo-cua.tsv"
    install_rust_licensing "$root" buzzardoscua \
        "$project_dir/LICENSES/generated/cargo-cua.tsv"
    install -D -m 0644 "$project_dir/cua/UPSTREAM.toml" \
        "$root/usr/share/doc/buzzardoscua/UPSTREAM.toml"
    install -D -m 0644 "$project_dir/cua/CHANGES.BUZZARDOS.md" \
        "$root/usr/share/doc/buzzardoscua/CHANGES.BUZZARDOS.md"
    install -D -m 0644 "$project_dir/cua/CITATION.cff" \
        "$root/usr/share/doc/buzzardoscua/CITATION.cff"
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
