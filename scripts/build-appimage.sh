#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
build_dir=${WILDBUZZARD_BUILD_DIR:-"$project_dir/build/appimage"}
appdir="$build_dir/WildBuzzard.AppDir"
tools_dir="$build_dir/tools"
output_dir="$project_dir/dist"
final_output="$output_dir/WildBuzzard-x86_64.AppImage"
gtk_sdk=${WILDBUZZARD_GTK_SDK:-"$project_dir/build/gtk-sdk"}
gtk_sdk_pkgconfig="$gtk_sdk/usr/lib/x86_64-linux-gnu/pkgconfig"
gtk_sdk_lib="$gtk_sdk/usr/lib/x86_64-linux-gnu"

crane_version=v0.21.8
crane_sha256=59b59f68ee37aba51f5523d69ec779ee925d9be4e279f9220eca357267f2ee67
slirp_version=v1.3.4
slirp_sha256=e8d0440de8d8c87072138883bc27cfa02f8b0e8a504badbf335c41f794788cc2
linuxdeploy_version=1-alpha-20251107-1
linuxdeploy_sha256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
appimage_runtime_sha256=1cc49bcf1e2ccd593c379adb17c9f85a36d619088296504de95b1d06215aebbf
zig_version=0.14.1
zig_sha256=24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c
cargo_zigbuild_version=0.21.8
nvidia_toolkit_version=1.19.1-1
nvidia_toolkit_base_sha256=b6c5b4e77a28cde0197cc0e64edf75538604775d9f8aea502cef667e7e5b2132
nvidia_container_tools_sha256=5642763d51961a2295dff09990048a5dcee81edbea2a8c5084e47b09ccf17268
nvidia_container_library_sha256=d73bb582af893135198ef81cb22135c790a75d2ad72910446477c6c4430f3e6b

case "$(uname -m)" in
    x86_64) ;;
    *)
        echo "AppImage packaging currently supports x86_64" >&2
        exit 1
        ;;
esac

for command_name in cargo curl readelf sha256sum tar bwrap unshare dpkg-deb pkg-config; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
        echo "build dependency missing: $command_name" >&2
        exit 1
    fi
done

cargo_pkg_config_path=${PKG_CONFIG_PATH:-}
cargo_rustflags=${RUSTFLAGS:-}
if ! pkg-config --exists 'gtk4 >= 4.18' 'graphene-gobject-1.0 >= 1.10'; then
    if [[ -f "$gtk_sdk_pkgconfig/gtk4.pc" ]] &&
        [[ -f "$gtk_sdk_pkgconfig/graphene-gobject-1.0.pc" ]] &&
        [[ -f "$gtk_sdk_lib/libgtk-4.so.1" ]] &&
        [[ -f "$gtk_sdk_lib/libgraphene-1.0.so.0" ]]; then
        cargo_pkg_config_path="$gtk_sdk_pkgconfig${cargo_pkg_config_path:+:$cargo_pkg_config_path}"
        cargo_rustflags="-L native=$gtk_sdk_lib${cargo_rustflags:+ $cargo_rustflags}"
        printf 'Using staged GTK SDK: %s\n' "$gtk_sdk"
    else
        echo "build dependency missing: GTK >= 4.18 development files" >&2
        echo "install them or set WILDBUZZARD_GTK_SDK to a staged /usr tree" >&2
        exit 1
    fi
fi

download_verified() {
    local url=$1
    local destination=$2
    local expected=$3
    if [[ ! -f "$destination" ]] ||
        [[ "$(sha256sum "$destination" | cut -d' ' -f1)" != "$expected" ]]; then
        curl --fail --location --retry 3 --output "$destination.tmp" "$url"
        printf '%s  %s\n' "$expected" "$destination.tmp" | sha256sum --check -
        mv "$destination.tmp" "$destination"
    fi
}

rm -rf "$appdir"
mkdir -p \
    "$appdir/usr/bin" \
    "$appdir/usr/lib" \
    "$appdir/usr/libexec/wildbuzzard" \
    "$tools_dir" \
    "$output_dir"
staged_output=$(mktemp "$output_dir/.WildBuzzard-x86_64.AppImage.XXXXXX")
cleanup_staged_output() {
    rm -f -- "$staged_output"
}
trap cleanup_staged_output EXIT

PKG_CONFIG_PATH="$cargo_pkg_config_path" \
RUSTFLAGS="$cargo_rustflags" \
cargo build \
    --manifest-path "$project_dir/Cargo.toml" \
    --workspace \
    --exclude wildbuzzard-shell \
    --release \
    --locked

# The guest shell is copied into persistent OCI rootfses, so it must not inherit
# the build host's glibc baseline. Compile it with Zig's versioned GNU target;
# the resulting binary runs against the guest's own libxkbcommon and glibc.
zig_archive="$tools_dir/zig-x86_64-linux-$zig_version.tar.xz"
download_verified \
    "https://ziglang.org/download/$zig_version/zig-x86_64-linux-$zig_version.tar.xz" \
    "$zig_archive" \
    "$zig_sha256"
zig_dir="$tools_dir/zig-x86_64-linux-$zig_version"
if [[ ! -x "$zig_dir/zig" ]]; then
    zig_extract=$(mktemp -d "$build_dir/zig-extract.XXXXXX")
    tar -xJf "$zig_archive" -C "$zig_extract"
    rm -rf -- "$zig_dir"
    mv "$zig_extract/zig-x86_64-linux-$zig_version" "$zig_dir"
    rmdir "$zig_extract"
fi
cargo_zigbuild_root="$tools_dir/cargo-zigbuild-$cargo_zigbuild_version"
if [[ ! -x "$cargo_zigbuild_root/bin/cargo-zigbuild" ]]; then
    cargo install \
        --root "$cargo_zigbuild_root" \
        --version "$cargo_zigbuild_version" \
        --locked \
        cargo-zigbuild
fi
PATH="$zig_dir:$cargo_zigbuild_root/bin:$PATH" \
    cargo zigbuild \
        --manifest-path "$project_dir/Cargo.toml" \
        --package wildbuzzard-shell \
        --release \
        --locked \
        --target x86_64-unknown-linux-gnu.2.31
guest_shell="$project_dir/target/x86_64-unknown-linux-gnu/release/wildbuzzard-shell"
maximum_glibc=$(
    readelf --version-info "$guest_shell" |
        sed -n 's/.*Name: \(GLIBC_[0-9.]*\).*/\1/p' |
        sort -V |
        tail -n1
)
if [[ -n "$maximum_glibc" ]] &&
    [[ "$(printf '%s\n' GLIBC_2.31 "$maximum_glibc" | sort -V | tail -n1)" != GLIBC_2.31 ]]; then
    echo "guest shell requires $maximum_glibc, newer than supported GLIBC_2.31" >&2
    exit 1
fi

# The patched in-guest CUA driver is a managed guest asset. Building it into
# the AppImage makes fixes available to both newly extracted and existing
# persistent machines without downloading an unpinned binary at startup.
PATH="$zig_dir:$cargo_zigbuild_root/bin:$PATH" \
    cargo zigbuild \
        --manifest-path \
        "$project_dir/third_party/trycua-cua/cua-driver/rust/Cargo.toml" \
        --package cua-driver \
        --release \
        --locked \
        --target x86_64-unknown-linux-gnu.2.31
guest_cua_driver="$project_dir/third_party/trycua-cua/cua-driver/rust/target/x86_64-unknown-linux-gnu/release/cua-driver"
maximum_glibc=$(
    readelf --version-info "$guest_cua_driver" |
        sed -n 's/.*Name: \(GLIBC_[0-9.]*\).*/\1/p' |
        sort -V |
        tail -n1
)
if [[ -n "$maximum_glibc" ]] &&
    [[ "$(printf '%s\n' GLIBC_2.31 "$maximum_glibc" | sort -V | tail -n1)" != GLIBC_2.31 ]]; then
    echo "guest CUA driver requires $maximum_glibc, newer than supported GLIBC_2.31" >&2
    exit 1
fi

install -m755 "$project_dir/target/release/wildbuzzard" "$appdir/usr/bin/wildbuzzard"
install -m755 "$project_dir/target/release/wildbuzzard-broker" "$appdir/usr/bin/wildbuzzard-broker"
install -m755 "$project_dir/target/release/wildbuzzard-display" "$appdir/usr/bin/wildbuzzard-display"
install -m755 "$guest_shell" "$appdir/usr/bin/wildbuzzard-shell"
install -m755 "$(command -v bwrap)" "$appdir/usr/libexec/wildbuzzard/bwrap"
install -m755 "$(command -v unshare)" "$appdir/usr/libexec/wildbuzzard/unshare"

crane_archive="$tools_dir/crane-$crane_version.tar.gz"
download_verified \
    "https://github.com/google/go-containerregistry/releases/download/$crane_version/go-containerregistry_Linux_x86_64.tar.gz" \
    "$crane_archive" \
    "$crane_sha256"
tar -xzf "$crane_archive" -C "$appdir/usr/libexec/wildbuzzard" crane
chmod 755 "$appdir/usr/libexec/wildbuzzard/crane"

slirp_binary="$tools_dir/slirp4netns-$slirp_version-x86_64"
download_verified \
    "https://github.com/rootless-containers/slirp4netns/releases/download/$slirp_version/slirp4netns-x86_64" \
    "$slirp_binary" \
    "$slirp_sha256"
install -m755 "$slirp_binary" "$appdir/usr/libexec/wildbuzzard/slirp4netns"

# Wild Buzzard generates and validates NVIDIA CDI itself. Bundle the pinned
# toolkit base and libnvidia-container payload so runtime behavior never
# depends on host nvidia-ctk/nvidia-container-cli packages or PATH.
nvidia_packages="$tools_dir/nvidia-container-toolkit-$nvidia_toolkit_version"
mkdir -p "$nvidia_packages"
nvidia_toolkit_base_deb="$nvidia_packages/nvidia-container-toolkit-base_${nvidia_toolkit_version}_amd64.deb"
nvidia_container_tools_deb="$nvidia_packages/libnvidia-container-tools_${nvidia_toolkit_version}_amd64.deb"
nvidia_container_library_deb="$nvidia_packages/libnvidia-container1_${nvidia_toolkit_version}_amd64.deb"
download_verified \
    "https://nvidia.github.io/libnvidia-container/stable/deb/amd64/$(basename "$nvidia_toolkit_base_deb")" \
    "$nvidia_toolkit_base_deb" \
    "$nvidia_toolkit_base_sha256"
download_verified \
    "https://nvidia.github.io/libnvidia-container/stable/deb/amd64/$(basename "$nvidia_container_tools_deb")" \
    "$nvidia_container_tools_deb" \
    "$nvidia_container_tools_sha256"
download_verified \
    "https://nvidia.github.io/libnvidia-container/stable/deb/amd64/$(basename "$nvidia_container_library_deb")" \
    "$nvidia_container_library_deb" \
    "$nvidia_container_library_sha256"
nvidia_extract=$(mktemp -d "$build_dir/nvidia-toolkit-extract.XXXXXX")
for package in \
    "$nvidia_toolkit_base_deb" \
    "$nvidia_container_tools_deb" \
    "$nvidia_container_library_deb"; do
    dpkg-deb --extract "$package" "$nvidia_extract"
done
install -m755 "$nvidia_extract/usr/bin/nvidia-ctk" \
    "$appdir/usr/libexec/wildbuzzard/nvidia-ctk"
install -m755 "$nvidia_extract/usr/bin/nvidia-cdi-hook" \
    "$appdir/usr/libexec/wildbuzzard/nvidia-cdi-hook"
install -m755 "$nvidia_extract/usr/bin/nvidia-container-cli" \
    "$appdir/usr/libexec/wildbuzzard/nvidia-container-cli"
install -m755 "$nvidia_extract/usr/lib/x86_64-linux-gnu/libnvidia-container.so.1.19.1" \
    "$appdir/usr/lib/libnvidia-container.so.1.19.1"
install -m755 "$nvidia_extract/usr/lib/x86_64-linux-gnu/libnvidia-container-go.so.1.19.1" \
    "$appdir/usr/lib/libnvidia-container-go.so.1.19.1"
ln -s libnvidia-container.so.1.19.1 "$appdir/usr/lib/libnvidia-container.so.1"
ln -s libnvidia-container-go.so.1.19.1 "$appdir/usr/lib/libnvidia-container-go.so.1"
for package in \
    nvidia-container-toolkit-base \
    libnvidia-container-tools \
    libnvidia-container1; do
    mkdir -p "$appdir/usr/share/doc/$package"
    cp "$nvidia_extract/usr/share/doc/$package/copyright" \
        "$appdir/usr/share/doc/$package/copyright"
done
rm -rf -- "$nvidia_extract"

install -m755 "$project_dir/packaging/AppRun" "$appdir/AppRun"
install -m644 \
    "$project_dir/packaging/WildBuzzard.desktop" \
    "$appdir/org.openresearchtools.wildbuzzard.desktop"
install -m644 "$project_dir/packaging/wildbuzzard.svg" "$appdir/wildbuzzard.svg"
mkdir -p "$appdir/usr/share/applications" "$appdir/usr/share/icons/hicolor/scalable/apps"
mkdir -p "$appdir/usr/share/metainfo"
cp "$appdir/org.openresearchtools.wildbuzzard.desktop" "$appdir/usr/share/applications/"
cp "$appdir/wildbuzzard.svg" "$appdir/usr/share/icons/hicolor/scalable/apps/"
install -m644 \
    "$project_dir/packaging/org.openresearchtools.WildBuzzard.metainfo.xml" \
    "$appdir/usr/share/metainfo/org.openresearchtools.wildbuzzard.appdata.xml"

linuxdeploy="$tools_dir/linuxdeploy-x86_64.AppImage"
download_verified \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/$linuxdeploy_version/linuxdeploy-x86_64.AppImage" \
    "$linuxdeploy" \
    "$linuxdeploy_sha256"
chmod 755 "$linuxdeploy"

appimage_runtime="$tools_dir/runtime-x86_64"
download_verified \
    "https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64" \
    "$appimage_runtime" \
    "$appimage_runtime_sha256"

export APPIMAGE_EXTRACT_AND_RUN=1
"$linuxdeploy" \
    --appdir "$appdir" \
    --executable "$appdir/usr/bin/wildbuzzard" \
    --executable "$appdir/usr/bin/wildbuzzard-broker" \
    --executable "$appdir/usr/bin/wildbuzzard-display" \
    --executable "$appdir/usr/bin/wildbuzzard-shell" \
    --executable "$appdir/usr/libexec/wildbuzzard/bwrap" \
    --executable "$appdir/usr/libexec/wildbuzzard/unshare" \
    --executable "$appdir/usr/libexec/wildbuzzard/nvidia-ctk" \
    --executable "$appdir/usr/libexec/wildbuzzard/nvidia-cdi-hook" \
    --executable "$appdir/usr/libexec/wildbuzzard/nvidia-container-cli" \
    --desktop-file "$appdir/org.openresearchtools.wildbuzzard.desktop" \
    --icon-file "$appdir/wildbuzzard.svg"

# Keep the guest driver out of linuxdeploy's host dependency/RPATH rewrite.
# The launcher copies this exact guest-targeted executable into the persistent
# rootfs through the subordinate-ID migration namespace.
install -m755 "$guest_cua_driver" "$appdir/usr/bin/wildbuzzard-cua-driver"

linuxdeploy_extract=$(mktemp -d "$build_dir/linuxdeploy-extract.XXXXXX")
cleanup_linuxdeploy_extract() {
    rm -rf -- "$linuxdeploy_extract"
}
trap 'cleanup_linuxdeploy_extract; cleanup_staged_output' EXIT
(
    cd "$linuxdeploy_extract"
    "$linuxdeploy" --appimage-extract >/dev/null
)
appimagetool="$linuxdeploy_extract/squashfs-root/plugins/linuxdeploy-plugin-appimage/usr/bin/appimagetool"
[[ -x "$appimagetool" ]] || {
    echo "verified linuxdeploy bundle does not contain appimagetool" >&2
    exit 1
}
ARCH=x86_64 "$appimagetool" \
    --runtime-file "$appimage_runtime" \
    "$appdir" \
    "$staged_output"

chmod 755 "$staged_output"
mv -f -- "$staged_output" "$final_output"
cleanup_linuxdeploy_extract
trap - EXIT
(cd "$output_dir" && sha256sum "$(basename "$final_output")") > "$final_output.sha256"
printf 'Built %s\n' "$final_output"
