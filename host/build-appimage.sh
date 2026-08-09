#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

host_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(CDPATH= cd -- "$host_dir/.." && pwd)
task_uid=$(id -u)
build_root=${WILDBUZZARD_BUILD_ROOT:-"${TMPDIR:-/tmp}/wildbuzzard-build-$task_uid"}
build_dir=${WILDBUZZARD_BUILD_DIR:-"$build_root/appimage"}
appdir="$build_dir/WildBuzzard.AppDir"
tools_dir="$build_dir/tools"
output_dir=${WILDBUZZARD_OUTPUT_DIR:-"$build_root/out"}
final_output="$output_dir/WildBuzzard-x86_64.AppImage"
gtk_sdk=${WILDBUZZARD_GTK_SDK:-"$build_root/gtk-sdk"}
gtk_sdk_pkgconfig="$gtk_sdk/usr/lib/x86_64-linux-gnu/pkgconfig"
gtk_sdk_lib="$gtk_sdk/usr/lib/x86_64-linux-gnu"
host_target_dir="$build_dir/cargo-host"
guest_target_dir="$build_dir/cargo-guest"
cua_target_dir="$build_dir/cargo-cua"

crane_version=v0.21.8
crane_sha256=59b59f68ee37aba51f5523d69ec779ee925d9be4e279f9220eca357267f2ee67
slirp_package_version=1.3.3-1
slirp_deb_sha256=dda3ca5101c58e9585bfd6e7b9d26831090327120cfb5092172ead355f968dd4
slirp_binary_sha256=20581c54ee53ae32e908c9b318481e5a71b72a13f850ce41722e402cb524b325
linuxdeploy_version=1-alpha-20251107-1
linuxdeploy_sha256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
appimage_runtime_sha256=a861c1b4c90ea8a3968753db768c647b068f563929992dc97ffdbce90247a7e6
appimage_runtime_relink_manifest_sha256=a956b20085c7ff0b0019a531e51db3dfdf174f6fa9c0c4183baba2c93a0dd772
appimage_runtime_metadata_sha256=b2182090c84f5cab0b6345d447d54bc39cb31dd348d32b24b90eb8b2c7de55db
zig_version=0.14.1
zig_sha256=24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c
cargo_zigbuild_version=0.21.8
rust_toolchain_version=1.96.0
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

for command_name in cargo curl dd file find git id install make mksquashfs ninja patch python3 readelf realpath sha256sum sort tar touch zstd bwrap unshare dpkg-deb pkg-config gst-launch-1.0 gst-inspect-1.0 pw-dump; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
        echo "build dependency missing: $command_name" >&2
        exit 1
    fi
done

actual_rust_toolchain=$(rustc --version | awk '{print $2}')
if [[ "$actual_rust_toolchain" != "$rust_toolchain_version" ]]; then
    echo "Rust toolchain mismatch: expected $rust_toolchain_version, found $actual_rust_toolchain" >&2
    exit 1
fi
rust_notice="$(rustc --print sysroot)/share/doc/rust/COPYRIGHT-library.html"
[[ -f "$rust_notice" ]] || {
    echo "Rust standard-library notice is missing: $rust_notice" >&2
    exit 1
}

build_dir=$(realpath -m -- "$build_dir")
output_dir=$(realpath -m -- "$output_dir")
gtk_sdk=$(realpath -m -- "$gtk_sdk")
gtk_sdk_pkgconfig="$gtk_sdk/usr/lib/x86_64-linux-gnu/pkgconfig"
gtk_sdk_lib="$gtk_sdk/usr/lib/x86_64-linux-gnu"
appdir="$build_dir/WildBuzzard.AppDir"
tools_dir="$build_dir/tools"
final_output="$output_dir/WildBuzzard-x86_64.AppImage"
host_target_dir="$build_dir/cargo-host"
guest_target_dir="$build_dir/cargo-guest"
cua_target_dir="$build_dir/cargo-cua"
for generated_path in "$build_dir" "$output_dir"; do
    case "$generated_path/" in
        "$project_dir/"*)
            echo "refusing to place build output inside the source repository: $generated_path" >&2
            exit 1
            ;;
    esac
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

stage_release_license_payload() {
    local license_destination="$appdir/usr/share/doc/wildbuzzard/licenses"
    local mpl_destination="$appdir/usr/share/doc/wildbuzzard/sources/mpl"
    local go_destination="$appdir/usr/share/doc/wildbuzzard/sources/go"
    local slirp_destination="$appdir/usr/share/doc/wildbuzzard/sources/slirp4netns"
    local runtime_destination="$appdir/usr/share/doc/wildbuzzard/sources/appimage-runtime"
    local project_destination="$appdir/usr/share/doc/wildbuzzard/sources/project"

    # Snapshot licensing inputs only after linuxdeploy has finished mutating the
    # AppDir. Remove the previous snapshot first so a deleted source record can
    # never survive from an earlier staging pass.
    rm -rf -- \
        "$license_destination" \
        "$mpl_destination" \
        "$go_destination" \
        "$slirp_destination" \
        "$runtime_destination" \
        "$project_destination"
    install -d -m755 \
        "$appdir/usr/share/doc/wildbuzzard" \
        "$appdir/usr/share/doc/wildbuzzard-cua" \
        "$appdir/usr/share/doc/wildbuzzard/rust" \
        "$license_destination" \
        "$mpl_destination" \
        "$go_destination" \
        "$slirp_destination" \
        "$runtime_destination" \
        "$runtime_destination/relink-kit" \
        "$project_destination"
    install -m644 "$project_dir/LICENSE" \
        "$appdir/usr/share/doc/wildbuzzard/LICENSE"
    install -m644 "$project_dir/NOTICE" \
        "$appdir/usr/share/doc/wildbuzzard/NOTICE"
    install -m644 "$project_dir/THIRD_PARTY_NOTICES.md" \
        "$appdir/usr/share/doc/wildbuzzard/THIRD_PARTY_NOTICES.md"
    cp -a "$project_dir/LICENSES/." "$license_destination/"

    # Fetch against the manifest inside the snapshot, not the mutable source
    # path. The subsequent artifact audit also rejects a source-tree change
    # that races this build.
    WILDBUZZARD_MPL_SOURCE_MANIFEST="$license_destination/mpl-sources.tsv" \
        "$project_dir/tools/fetch-mpl-sources.sh" "$mpl_destination"
    install -d -m755 "$build_dir/go-source-tmp" "$tools_dir/go-source-cache"
    LC_ALL=C \
    TMPDIR="$build_dir/go-source-tmp" \
    WILDBUZZARD_GO_SOURCE_MANIFEST="$license_destination/go-source-archives.tsv" \
    WILDBUZZARD_GO_SOURCE_CACHE="$tools_dir/go-source-cache" \
        "$project_dir/tools/fetch-go-source-archives.sh" "$go_destination"
    install -d -m755 "$tools_dir/slirp-source-cache"
    WILDBUZZARD_SLIRP_SOURCE_MANIFEST="$license_destination/slirp4netns-sources.tsv" \
    WILDBUZZARD_SLIRP_SOURCE_CACHE="$tools_dir/slirp-source-cache" \
        "$project_dir/tools/fetch-slirp4netns-sources.sh" "$slirp_destination"

    cp -a "$appimage_runtime_relink_kit/." "$runtime_destination/relink-kit/"
    install -m644 "$appimage_runtime_metadata" \
        "$runtime_destination/runtime-metadata.toml"

    rm -rf -- "$build_dir/project-source"
    "$project_dir/tools/create-project-source-archive.sh" \
        "$build_dir/project-source"
    cp -a "$build_dir/project-source/." "$project_destination/"

    install -m644 "$project_dir/guest/third_party/trycua-cua/LICENSE.md" \
        "$appdir/usr/share/doc/wildbuzzard-cua/LICENSE.trycua-cua.md"
    install -m644 "$project_dir/guest/third_party/trycua-cua/UPSTREAM.toml" \
        "$appdir/usr/share/doc/wildbuzzard-cua/UPSTREAM.toml"
    install -m644 "$project_dir/guest/third_party/trycua-cua/CHANGES.WILDBUZZARD.md" \
        "$appdir/usr/share/doc/wildbuzzard-cua/CHANGES.WILDBUZZARD.md"
    install -m644 "$project_dir/guest/third_party/trycua-cua/CITATION.cff" \
        "$appdir/usr/share/doc/wildbuzzard-cua/CITATION.cff"
    install -m644 \
        "$project_dir/guest/third_party/trycua-cua/cua-driver/rust/crates/cursor-overlay/assets/Inter-OFL.txt" \
        "$appdir/usr/share/doc/wildbuzzard-cua/Inter-OFL.txt"
    install -m644 \
        "$project_dir/guest/third_party/trycua-cua/cua-driver/rust/crates/platform-linux/protocol/virtual-keyboard-unstable-v1.xml" \
        "$appdir/usr/share/doc/wildbuzzard-cua/virtual-keyboard-unstable-v1.xml"
    install -m644 "$rust_notice" \
        "$appdir/usr/share/doc/wildbuzzard/rust/COPYRIGHT-library.html"
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
CARGO_TARGET_DIR="$host_target_dir" \
cargo build \
    --manifest-path "$host_dir/Cargo.toml" \
    --workspace \
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

# Build the Type-2 runtime from the exact audited source set. The resulting
# static PIE and its LGPL relink kit are deterministic and independent of the
# build host's libc, GCC, and mutable distribution package repository.
appimage_runtime_output="$build_dir/appimage-runtime-output"
appimage_runtime_build="$build_dir/appimage-runtime-build"
appimage_runtime_source_cache="$tools_dir/appimage-runtime-sources"
"$project_dir/tools/build-appimage-runtime.sh" \
    --zig "$zig_dir/zig" \
    --zig-archive "$zig_archive" \
    --source-cache "$appimage_runtime_source_cache" \
    --build-dir "$appimage_runtime_build" \
    --output-dir "$appimage_runtime_output" \
    --self-test
appimage_runtime="$appimage_runtime_output/runtime-x86_64"
appimage_runtime_relink_kit="$appimage_runtime_output/relink-kit"
appimage_runtime_metadata="$appimage_runtime_output/runtime-metadata.toml"
printf '%s  %s\n' "$appimage_runtime_sha256" "$appimage_runtime" | sha256sum --check -
printf '%s  %s\n' \
    "$appimage_runtime_relink_manifest_sha256" \
    "$appimage_runtime_relink_kit/BUILD-INPUTS.sha256" | sha256sum --check -
printf '%s  %s\n' \
    "$appimage_runtime_metadata_sha256" \
    "$appimage_runtime_metadata" | sha256sum --check -

cargo_zigbuild_root="$tools_dir/cargo-zigbuild-$cargo_zigbuild_version"
if [[ ! -x "$cargo_zigbuild_root/bin/cargo-zigbuild" ]]; then
    cargo install \
        --root "$cargo_zigbuild_root" \
        --version "$cargo_zigbuild_version" \
        --locked \
        cargo-zigbuild
fi
PATH="$zig_dir:$cargo_zigbuild_root/bin:$PATH" \
CARGO_TARGET_DIR="$guest_target_dir" \
    cargo zigbuild \
        --manifest-path "$project_dir/guest/Cargo.toml" \
        --package wildbuzzard-shell \
        --release \
        --locked \
        --target x86_64-unknown-linux-gnu.2.31
guest_shell="$guest_target_dir/x86_64-unknown-linux-gnu/release/wildbuzzard-shell"
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
CARGO_TARGET_DIR="$cua_target_dir" \
    cargo zigbuild \
        --manifest-path \
        "$project_dir/guest/third_party/trycua-cua/cua-driver/rust/Cargo.toml" \
        --package cua-driver \
        --release \
        --locked \
        --target x86_64-unknown-linux-gnu.2.31
guest_cua_driver="$cua_target_dir/x86_64-unknown-linux-gnu/release/cua-driver"
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

install -m755 "$host_target_dir/release/wildbuzzard" "$appdir/usr/bin/wildbuzzard"
install -m755 "$host_target_dir/release/wildbuzzard-broker" "$appdir/usr/bin/wildbuzzard-broker"
install -m755 "$host_target_dir/release/wildbuzzard-display" "$appdir/usr/bin/wildbuzzard-display"
install -m755 "$guest_shell" "$appdir/usr/bin/wildbuzzard-shell"
install -m755 "$(command -v bwrap)" "$appdir/usr/libexec/wildbuzzard/bwrap"
install -m755 "$(command -v unshare)" "$appdir/usr/libexec/wildbuzzard/unshare"
install -m755 "$(command -v gst-launch-1.0)" \
    "$appdir/usr/libexec/wildbuzzard/gst-launch-1.0"
install -m755 "$(command -v pw-dump)" \
    "$appdir/usr/libexec/wildbuzzard/pw-dump"
gst_plugin_scanner=/usr/lib/x86_64-linux-gnu/gstreamer1.0/gstreamer-1.0/gst-plugin-scanner
[[ -x "$gst_plugin_scanner" ]] || {
    echo "build dependency missing: $gst_plugin_scanner" >&2
    exit 1
}
install -m755 "$gst_plugin_scanner" \
    "$appdir/usr/libexec/wildbuzzard/gst-plugin-scanner"

# Host audio/microphone/camera bridges run entirely outside the guest. Bundle
# the GStreamer launcher, the exact plugins used by the fixed pipelines, and
# their PipeWire dependencies; release behavior must not depend on host PATH
# or globally installed GStreamer plugins.
gst_plugin_dir="$appdir/usr/lib/gstreamer-1.0"
mkdir -p "$gst_plugin_dir"
gst_plugins=(
    libgstaudioconvert.so
    libgstaudioresample.so
    libgstcoreelements.so
    libgstgdp.so
    libgstjpeg.so
    libgstpipewire.so
    libgstpulseaudio.so
    libgsttcp.so
    libgstvideo4linux2.so
    libgstvideoconvertscale.so
    libgstvideorate.so
)
gst_plugin_sources=()
for plugin in "${gst_plugins[@]}"; do
    source_path="/usr/lib/x86_64-linux-gnu/gstreamer-1.0/$plugin"
    [[ -f "$source_path" ]] || {
        echo "build dependency missing: $source_path" >&2
        exit 1
    }
    install -m755 "$source_path" "$gst_plugin_dir/$plugin"
    gst_plugin_sources+=("$gst_plugin_dir/$plugin")
done
spa_plugin_sources=()
for relative in \
    libspa.so \
    support/libspa-support.so \
    support/libspa-dbus.so \
    audioconvert/libspa-audioconvert.so \
    videoconvert/libspa-videoconvert.so; do
    source_path="/usr/lib/x86_64-linux-gnu/spa-0.2/$relative"
    [[ -f "$source_path" ]] || {
        echo "build dependency missing: $source_path" >&2
        exit 1
    }
    destination="$appdir/usr/lib/spa-0.2/$relative"
    mkdir -p "$(dirname -- "$destination")"
    install -m755 "$source_path" "$destination"
    spa_plugin_sources+=("$destination")
done
for package in \
    bubblewrap \
    util-linux \
    gstreamer1.0-tools \
    gstreamer1.0-pipewire \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    libpulse0 \
    libpipewire-0.3-0t64 \
    libspa-0.2-modules \
    pipewire-bin; do
    copyright="/usr/share/doc/$package/copyright"
    [[ -f "$copyright" ]] || {
        echo "host payload copyright is missing: $copyright" >&2
        exit 1
    }
    mkdir -p "$appdir/usr/share/doc/$package"
    install -m644 "$copyright" "$appdir/usr/share/doc/$package/copyright"
done

crane_archive="$tools_dir/crane-$crane_version.tar.gz"
download_verified \
    "https://github.com/google/go-containerregistry/releases/download/$crane_version/go-containerregistry_Linux_x86_64.tar.gz" \
    "$crane_archive" \
    "$crane_sha256"
tar -xzf "$crane_archive" -C "$appdir/usr/libexec/wildbuzzard" crane
chmod 755 "$appdir/usr/libexec/wildbuzzard/crane"

slirp_packages="$tools_dir/slirp4netns-$slirp_package_version"
mkdir -p "$slirp_packages"
slirp_deb="$slirp_packages/slirp4netns_${slirp_package_version}_amd64.deb"
download_verified \
    "https://archive.ubuntu.com/ubuntu/pool/universe/s/slirp4netns/$(basename "$slirp_deb")" \
    "$slirp_deb" \
    "$slirp_deb_sha256"
slirp_extract=$(mktemp -d "$build_dir/slirp4netns-extract.XXXXXX")
dpkg-deb --extract "$slirp_deb" "$slirp_extract"
slirp_binary="$slirp_extract/usr/bin/slirp4netns"
printf '%s  %s\n' "$slirp_binary_sha256" "$slirp_binary" |
    sha256sum --check --status || {
        echo "slirp4netns package payload differs from the audited binary" >&2
        exit 1
    }
install -m755 "$slirp_binary" "$appdir/usr/libexec/wildbuzzard/slirp4netns"
install -d -m755 "$appdir/usr/share/doc/slirp4netns"
install -m644 "$slirp_extract/usr/share/doc/slirp4netns/copyright" \
    "$appdir/usr/share/doc/slirp4netns/copyright"
rm -rf -- "$slirp_extract"

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

install -m755 "$host_dir/packaging/AppRun" "$appdir/AppRun"
install -m644 \
    "$host_dir/packaging/WildBuzzard.desktop" \
    "$appdir/org.openresearchtools.wildbuzzard.desktop"
install -m644 "$host_dir/packaging/wildbuzzard.svg" "$appdir/wildbuzzard.svg"
mkdir -p "$appdir/usr/share/applications" "$appdir/usr/share/icons/hicolor/scalable/apps"
mkdir -p "$appdir/usr/share/metainfo"
cp "$appdir/org.openresearchtools.wildbuzzard.desktop" "$appdir/usr/share/applications/"
cp "$appdir/wildbuzzard.svg" "$appdir/usr/share/icons/hicolor/scalable/apps/"
install -m644 \
    "$host_dir/packaging/org.openresearchtools.WildBuzzard.metainfo.xml" \
    "$appdir/usr/share/metainfo/org.openresearchtools.wildbuzzard.appdata.xml"
linuxdeploy="$tools_dir/linuxdeploy-x86_64.AppImage"
download_verified \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/$linuxdeploy_version/linuxdeploy-x86_64.AppImage" \
    "$linuxdeploy" \
    "$linuxdeploy_sha256"
chmod 755 "$linuxdeploy"

export APPIMAGE_EXTRACT_AND_RUN=1
linuxdeploy_args=(
    --appdir "$appdir" \
    --executable "$appdir/usr/bin/wildbuzzard" \
    --executable "$appdir/usr/bin/wildbuzzard-broker" \
    --executable "$appdir/usr/bin/wildbuzzard-display" \
    --executable "$appdir/usr/bin/wildbuzzard-shell" \
    --executable "$appdir/usr/libexec/wildbuzzard/bwrap" \
    --executable "$appdir/usr/libexec/wildbuzzard/unshare" \
    --executable "$appdir/usr/libexec/wildbuzzard/gst-launch-1.0" \
    --executable "$appdir/usr/libexec/wildbuzzard/pw-dump" \
    --executable "$appdir/usr/libexec/wildbuzzard/gst-plugin-scanner" \
    --executable "$appdir/usr/libexec/wildbuzzard/slirp4netns" \
    --executable "$appdir/usr/libexec/wildbuzzard/nvidia-ctk" \
    --executable "$appdir/usr/libexec/wildbuzzard/nvidia-cdi-hook" \
    --executable "$appdir/usr/libexec/wildbuzzard/nvidia-container-cli" \
    --desktop-file "$appdir/org.openresearchtools.wildbuzzard.desktop" \
    --icon-file "$appdir/wildbuzzard.svg"
)
for library in "${gst_plugin_sources[@]}" "${spa_plugin_sources[@]}"; do
    linuxdeploy_args+=(--library "$library")
done
# The pinned NVIDIA binaries are extracted directly into the AppDir rather
# than installed into the disposable build host.  Make that staged library
# directory visible to linuxdeploy's ldd-based dependency resolver so it can
# trace libnvidia-container and its closure without a host toolkit install.
LD_LIBRARY_PATH="$appdir/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
    "$linuxdeploy" "${linuxdeploy_args[@]}"

# linuxdeploy may infer and copy GStreamer's direct ALSA plugin while walking
# transitive launcher dependencies, even though it is intentionally absent
# from `gst_plugins`. Wild Buzzard microphone capture must have no packaged
# raw-device bypass around the host desktop's PipeWire-Pulse recording
# accounting path. Remove both locations linuxdeploy has used, then fail the
# build if a future layout reintroduces the plugin anywhere in the AppDir.
rm -f \
    "$appdir/usr/lib/gstreamer-1.0/libgstalsa.so" \
    "$appdir/usr/lib/libgstalsa.so"
unexpected_alsa_plugin=$(find "$appdir/usr" -name libgstalsa.so -print -quit)
if [[ -n "$unexpected_alsa_plugin" ]]; then
    echo "forbidden direct-ALSA GStreamer plugin was packaged: $unexpected_alsa_plugin" >&2
    exit 1
fi

# linuxdeploy intentionally treats libpipewire as a host integration library
# and refuses to deploy it. Wild Buzzard's release contract is stricter: the
# AppImage carries the client ABI while still connecting to the user's running
# host service. Install the pinned build-environment client library explicitly.
install -m755 /usr/lib/x86_64-linux-gnu/libpipewire-0.3.so.0 \
    "$appdir/usr/lib/libpipewire-0.3.so.0"

# Keep the guest driver out of linuxdeploy's host dependency/RPATH rewrite.
# The launcher copies this exact guest-targeted executable into the persistent
# rootfs through the subordinate-ID migration namespace.
install -m755 "$guest_cua_driver" "$appdir/usr/bin/wildbuzzard-cua-driver"

stage_release_license_payload
python3 "$project_dir/tools/license_audit.py" \
    --stage-appdir-host-notices "$appdir"
"$project_dir/tools/check-licenses.sh" --appdir "$appdir" --structural

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
