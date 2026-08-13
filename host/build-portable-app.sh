#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

host_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
project_dir=$(CDPATH= cd -- "$host_dir/.." && pwd)
task_uid=$(id -u)
build_root=${WILDBUZZARD_BUILD_ROOT:-"${TMPDIR:-/tmp}/wildbuzzard-build-$task_uid"}
build_dir=${WILDBUZZARD_BUILD_DIR:-"$build_root/portable-app"}
appdir="$build_dir/BuzzardOS.app"
tools_dir="$build_dir/tools"
output_dir=${WILDBUZZARD_OUTPUT_DIR:-"$build_root/out"}
final_output="$output_dir/app"
gtk_sdk=${WILDBUZZARD_GTK_SDK:-"$build_root/gtk-sdk"}
gtk_sdk_pkgconfig="$gtk_sdk/usr/lib/x86_64-linux-gnu/pkgconfig"
gtk_sdk_lib="$gtk_sdk/usr/lib/x86_64-linux-gnu"
host_target_dir="$build_dir/cargo-host"
guest_target_dir="$build_dir/cargo-guest"
cua_target_dir="$build_dir/cargo-cua"
guest_compositor_runtime=${WILDBUZZARD_GUEST_RUNTIME_PAYLOAD:-}

crane_version=v0.21.8
crane_sha256=59b59f68ee37aba51f5523d69ec779ee925d9be4e279f9220eca357267f2ee67
slirp_package_version=1.3.3-1
slirp_deb_sha256=dda3ca5101c58e9585bfd6e7b9d26831090327120cfb5092172ead355f968dd4
slirp_binary_sha256=20581c54ee53ae32e908c9b318481e5a71b72a13f850ce41722e402cb524b325
tar_package_version=1.34+dfsg-1+deb11u1
tar_deb_sha256=41c9c31f67a76b3532036f09ceac1f40a9224f1680395d120a8b24eae60dd54a
tar_binary_sha256=8498b0a43e820b0f8ed5cc61accfdfadffc7bd43ff6b0a91256a09ffc19dad38
tar_libacl_version=2.2.53-10
tar_libacl_deb_sha256=aa18d721be8aea50fbdb32cd9a319cb18a3f111ea6ad17399aa4ba9324c8e26a
tar_libacl_sha256=f99dd63f622af240ea7779bc2b21c7dc197d5d8dd7a865a3b0f6281a39768bee
tar_libselinux_version=3.1-3
tar_libselinux_deb_sha256=339f5ede10500c16dd7192d73169c31c4b27ab12130347275f23044ec8c7d897
tar_libselinux_sha256=1500423209a91f2f7787103b79ce823ceccf42c1883aa372c71112c688dc4d16
tar_libpcre2_version=10.36-2+deb11u1
tar_libpcre2_deb_sha256=ee192c8d22624eb9d0a2ae95056bad7fb371e5abc17e23e16b1de3ddb17a1064
tar_libpcre2_sha256=bedb7d14699797f65a30cbfa84f16681ffed436ea98111817b7d3ebbfbca334e
linuxdeploy_version=1-alpha-20251107-1
linuxdeploy_sha256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
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
        echo "Buzzard OS portable packaging currently supports x86_64" >&2
        exit 1
        ;;
esac

for command_name in cargo cmp curl dd file find git id install make mksquashfs ninja patch python3 readelf realpath sha256sum sort tar touch unzip zstd bwrap unshare dpkg-deb pkg-config gst-launch-1.0 gst-inspect-1.0 pw-dump; do
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
appdir="$build_dir/BuzzardOS.app"
tools_dir="$build_dir/tools"
final_output="$output_dir/app"
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

write_xkb_manifest() {
    local root=$1
    local output=$2
    python3 - "$root" "$output" <<'PY'
import hashlib
import os
import re
import stat
import sys
from pathlib import Path

root = Path(sys.argv[1])
output = Path(sys.argv[2])
if not root.is_dir() or root.is_symlink():
    raise SystemExit(f"XKB root is not a real directory: {root}")
rows = []
for directory, directory_names, file_names in os.walk(root, followlinks=False):
    directory_names.sort()
    file_names.sort()
    directory_path = Path(directory)
    for name in directory_names:
        path = directory_path / name
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode) or not stat.S_ISDIR(mode):
            raise SystemExit(f"XKB tree contains a non-directory entry: {path}")
    for name in file_names:
        path = directory_path / name
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode) or not stat.S_ISREG(mode):
            raise SystemExit(f"XKB tree contains a non-regular file: {path}")
        relative = path.relative_to(root).as_posix()
        if re.fullmatch(r"[A-Za-z0-9._+/@~-]+", relative) is None or ".." in relative:
            raise SystemExit(f"XKB tree contains an unsafe path: {relative}")
        rows.append((relative, hashlib.sha256(path.read_bytes()).hexdigest()))
contents = "".join(f"{digest}  {relative}\n" for relative, digest in sorted(rows))
if not contents:
    raise SystemExit("XKB tree is empty")
output.write_text(contents, encoding="utf-8", newline="\n")
PY
}

verify_xkb_payload() {
    local xkb_root=$1
    local manifest=$2
    local version_file=$3
    local copyright_file=$4
    local observed
    [[ -d "$xkb_root" && ! -L "$xkb_root" ]] || {
        echo "pinned XKB root is not a real directory: $xkb_root" >&2
        return 1
    }
    for required in \
        compat/complete \
        keycodes/evdev \
        rules/evdev \
        rules/evdev.lst \
        symbols/us \
        types/complete; do
        [[ -f "$xkb_root/$required" && ! -L "$xkb_root/$required" ]] || {
            echo "pinned XKB root is missing regular file $required" >&2
            return 1
        }
    done
    for metadata in "$manifest" "$version_file" "$copyright_file"; do
        [[ -s "$metadata" && ! -L "$metadata" ]] || {
            echo "pinned XKB metadata is missing or unsafe: $metadata" >&2
            return 1
        }
    done
    python3 - "$version_file" <<'PY'
import re
import sys
from pathlib import Path

if re.fullmatch(
    r"[A-Za-z0-9.+:~_-]+\n",
    Path(sys.argv[1]).read_text(encoding="utf-8"),
) is None:
    raise SystemExit("pinned XKB package version is invalid")
PY
    observed=$(mktemp "${TMPDIR:-/tmp}/wildbuzzard-xkb-manifest.XXXXXX")
    if ! write_xkb_manifest "$xkb_root" "$observed" ||
        ! cmp -s -- "$observed" "$manifest"; then
        rm -f -- "$observed"
        echo "pinned XKB tree differs from its deterministic manifest" >&2
        return 1
    fi
    rm -f -- "$observed"
}

verify_elf_relocation_closure() {
    local object=$1
    local label=$2
    local library_dir=$3
    local closure
    if ! closure=$(LD_LIBRARY_PATH="$library_dir" ldd -r -- "$object" 2>&1); then
        echo "$label failed relocation-closure validation" >&2
        printf '%s\n' "$closure" >&2
        return 1
    fi
    if grep -Eiq \
        'not found|undefined symbol|relocation error|symbol lookup error' \
        <<<"$closure"; then
        echo "$label has an incomplete relocation closure" >&2
        printf '%s\n' "$closure" >&2
        return 1
    fi
}

verify_pinned_libxkbcommon() {
    local library=$1
    local manifest=$2
    local version_file=$3
    local copyright_file=$4
    [[ -f "$library" && ! -L "$library" ]] || {
        echo "pinned libxkbcommon is missing or unsafe: $library" >&2
        return 1
    }
    for metadata in "$manifest" "$version_file" "$copyright_file"; do
        [[ -s "$metadata" && ! -L "$metadata" ]] || {
            echo "pinned libxkbcommon metadata is missing or unsafe: $metadata" >&2
            return 1
        }
    done
    readelf -d "$library" | grep -Fq 'Library soname: [libxkbcommon.so.0]' || {
        echo "pinned libxkbcommon has an unexpected SONAME" >&2
        return 1
    }
    python3 - "$library" "$manifest" "$version_file" <<'PY'
import hashlib
import re
import sys
from pathlib import Path

library, manifest, version_file = map(Path, sys.argv[1:])
match = re.fullmatch(
    r"([0-9a-f]{64})  lib/libxkbcommon\.so\.0\n",
    manifest.read_text(encoding="utf-8"),
)
if match is None:
    raise SystemExit("pinned libxkbcommon manifest is invalid")
if hashlib.sha256(library.read_bytes()).hexdigest() != match.group(1):
    raise SystemExit("pinned libxkbcommon differs from its manifest")
if re.fullmatch(
    r"[A-Za-z0-9.+:~_-]+\n",
    version_file.read_text(encoding="utf-8"),
) is None:
    raise SystemExit("pinned libxkbcommon package version is invalid")
PY
    verify_elf_relocation_closure \
        "$library" \
        "pinned libxkbcommon" \
        "$(dirname -- "$library")"
}

[[ -n "$guest_compositor_runtime" ]] || {
    echo "WILDBUZZARD_GUEST_RUNTIME_PAYLOAD must name the pinned sway-runtime-artifact directory" >&2
    exit 1
}
guest_compositor_runtime=$(realpath -- "$guest_compositor_runtime")
[[ -d "$guest_compositor_runtime" && ! -L "$guest_compositor_runtime" ]] || {
    echo "guest compositor runtime must be a real directory: $guest_compositor_runtime" >&2
    exit 1
}
for required in bin/sway bin/swaymsg; do
    [[ -f "$guest_compositor_runtime/$required" && \
        ! -L "$guest_compositor_runtime/$required" ]] || {
        echo "guest compositor runtime is missing a regular $required" >&2
        exit 1
    }
done
if find "$guest_compositor_runtime" -mindepth 1 -type l -print -quit | grep -q .; then
    echo "guest compositor runtime contains a symbolic link" >&2
    exit 1
fi
if find "$guest_compositor_runtime" -mindepth 1 ! -type d ! -type f -print -quit | grep -q .; then
    echo "guest compositor runtime contains a special file" >&2
    exit 1
fi
verify_xkb_payload \
    "$guest_compositor_runtime/share/X11/xkb" \
    "$guest_compositor_runtime/share/wildbuzzard/xkb-data.manifest.sha256" \
    "$guest_compositor_runtime/share/wildbuzzard/xkb-data.version" \
    "$guest_compositor_runtime/share/doc/xkb-data/copyright"
verify_pinned_libxkbcommon \
    "$guest_compositor_runtime/lib/libxkbcommon.so.0" \
    "$guest_compositor_runtime/share/wildbuzzard/libxkbcommon0.manifest.sha256" \
    "$guest_compositor_runtime/share/wildbuzzard/libxkbcommon0.version" \
    "$guest_compositor_runtime/share/doc/libxkbcommon0/copyright"

cargo_pkg_config_path=${PKG_CONFIG_PATH:-}
cargo_rustflags=${RUSTFLAGS:-}
if pkg-config --exists 'gtk4 >= 4.18' 'graphene-gobject-1.0 >= 1.10'; then
    # cargo-zigbuild does not reliably retain pkg-config's native search
    # directory when a GTK-using binary reaches those libraries through a
    # same-workspace Rust library (the shortcut helper is such a binary).
    # Keep the audited builder library directory explicit for every guest
    # cross-link instead of relying on the linker's host-default paths.
    gtk_builder_lib=$(pkg-config --variable=libdir gtk4)
    [[ "$gtk_builder_lib" == /* && -d "$gtk_builder_lib" ]] || {
        echo "GTK pkg-config returned an unsafe library directory: $gtk_builder_lib" >&2
        exit 1
    }
    cargo_rustflags="-L native=$gtk_builder_lib${cargo_rustflags:+ $cargo_rustflags}"
else
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
    local project_destination="$appdir/usr/share/doc/wildbuzzard/sources/project"

    # Snapshot licensing inputs only after linuxdeploy has finished mutating the
    # AppDir. Remove the previous snapshot first so a deleted source record can
    # never survive from an earlier staging pass.
    rm -rf -- \
        "$license_destination" \
        "$mpl_destination" \
        "$go_destination" \
        "$slirp_destination" \
        "$project_destination"
    install -d -m755 \
        "$appdir/usr/share/doc/wildbuzzard" \
        "$appdir/usr/share/doc/wildbuzzard-cua" \
        "$appdir/usr/share/doc/wildbuzzard/rust" \
        "$license_destination" \
        "$mpl_destination" \
        "$go_destination" \
        "$slirp_destination" \
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

# The guest clipboard endpoint is part of the protected runtime. It talks only
# to Sway's private clipboard and the machine-private framed Unix endpoint;
# host clipboard access remains in the native display process.
PATH="$zig_dir:$cargo_zigbuild_root/bin:$PATH" \
CARGO_TARGET_DIR="$guest_target_dir" \
    cargo zigbuild \
        --manifest-path "$project_dir/guest/Cargo.toml" \
        --package wildbuzzard-clipboard-agent \
        --release \
        --locked \
        --target x86_64-unknown-linux-gnu.2.31
guest_clipboard_agent="$guest_target_dir/x86_64-unknown-linux-gnu/release/wildbuzzard-clipboard-agent"
maximum_glibc=$(
    readelf --version-info "$guest_clipboard_agent" |
        sed -n 's/.*Name: \(GLIBC_[0-9.]*\).*/\1/p' |
        sort -V |
        tail -n1
)
if [[ -n "$maximum_glibc" ]] &&
    [[ "$(printf '%s\n' GLIBC_2.31 "$maximum_glibc" | sort -V | tail -n1)" != GLIBC_2.31 ]]; then
    echo "guest clipboard agent requires $maximum_glibc, newer than supported GLIBC_2.31" >&2
    exit 1
fi

# Settings is another managed guest executable. Build against the staged GTK4
# development ABI, but keep the resulting guest binary and its guest runtime
# libraries out of linuxdeploy's host dependency rewrite.
PATH="$zig_dir:$cargo_zigbuild_root/bin:$PATH" \
PKG_CONFIG_PATH="$cargo_pkg_config_path" \
PKG_CONFIG_ALLOW_CROSS=1 \
RUSTFLAGS="$cargo_rustflags" \
CARGO_TARGET_DIR="$guest_target_dir" \
    cargo zigbuild \
        --manifest-path "$project_dir/guest/Cargo.toml" \
        --package wildbuzzard-settings \
        --release \
        --locked \
        --target x86_64-unknown-linux-gnu.2.31
guest_settings="$guest_target_dir/x86_64-unknown-linux-gnu/release/wildbuzzard-settings"
maximum_glibc=$(
    readelf --version-info "$guest_settings" |
        sed -n 's/.*Name: \(GLIBC_[0-9.]*\).*/\1/p' |
        sort -V |
        tail -n1
)
if [[ -n "$maximum_glibc" ]] &&
    [[ "$(printf '%s\n' GLIBC_2.31 "$maximum_glibc" | sort -V | tail -n1)" != GLIBC_2.31 ]]; then
    echo "guest Settings requires $maximum_glibc, newer than supported GLIBC_2.31" >&2
    exit 1
fi

# The shortcut helper uses the same staged GTK4/GIO ABI as Settings for its
# in-guest relink chooser. It remains a managed guest executable rather than
# a host application dependency.
PATH="$zig_dir:$cargo_zigbuild_root/bin:$PATH" \
PKG_CONFIG_PATH="$cargo_pkg_config_path" \
PKG_CONFIG_ALLOW_CROSS=1 \
RUSTFLAGS="$cargo_rustflags" \
CARGO_TARGET_DIR="$guest_target_dir" \
    cargo zigbuild \
        --manifest-path "$project_dir/guest/Cargo.toml" \
        --package wildbuzzard-shortcut-helper \
        --release \
        --locked \
        --target x86_64-unknown-linux-gnu.2.31
guest_shortcut_helper="$guest_target_dir/x86_64-unknown-linux-gnu/release/wildbuzzard-shortcut-helper"
maximum_glibc=$(
    readelf --version-info "$guest_shortcut_helper" |
        sed -n 's/.*Name: \(GLIBC_[0-9.]*\).*/\1/p' |
        sort -V |
        tail -n1
)
if [[ -n "$maximum_glibc" ]] &&
    [[ "$(printf '%s\n' GLIBC_2.31 "$maximum_glibc" | sort -V | tail -n1)" != GLIBC_2.31 ]]; then
    echo "guest shortcut helper requires $maximum_glibc, newer than supported GLIBC_2.31" >&2
    exit 1
fi

# The patched in-guest CUA driver is a managed guest asset. Building it into
# the portable host application makes fixes available to both newly created and existing
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
install -m755 "$(command -v bwrap)" "$appdir/usr/libexec/wildbuzzard/bwrap"
install -m755 "$(command -v unshare)" "$appdir/usr/libexec/wildbuzzard/unshare"

# Export must work on the oldest supported glibc rather than inheriting the
# disposable builder's GNU tar ABI.  Keep tar and its three libraries in a
# private closure so linuxdeploy cannot replace them with newer host copies.
tar_packages="$tools_dir/tar-$tar_package_version"
mkdir -p "$tar_packages"
tar_deb="$tar_packages/tar_${tar_package_version}_amd64.deb"
tar_libacl_deb="$tar_packages/libacl1_${tar_libacl_version}_amd64.deb"
tar_libselinux_deb="$tar_packages/libselinux1_${tar_libselinux_version}_amd64.deb"
tar_libpcre2_deb="$tar_packages/libpcre2-8-0_${tar_libpcre2_version}_amd64.deb"
download_verified \
    "https://deb.debian.org/debian/pool/main/t/tar/$(basename "$tar_deb")" \
    "$tar_deb" \
    "$tar_deb_sha256"
download_verified \
    "https://deb.debian.org/debian/pool/main/a/acl/$(basename "$tar_libacl_deb")" \
    "$tar_libacl_deb" \
    "$tar_libacl_deb_sha256"
download_verified \
    "https://deb.debian.org/debian/pool/main/libs/libselinux/$(basename "$tar_libselinux_deb")" \
    "$tar_libselinux_deb" \
    "$tar_libselinux_deb_sha256"
download_verified \
    "https://deb.debian.org/debian/pool/main/p/pcre2/$(basename "$tar_libpcre2_deb")" \
    "$tar_libpcre2_deb" \
    "$tar_libpcre2_deb_sha256"
tar_extract=$(mktemp -d "$build_dir/tar-runtime-extract.XXXXXX")
for tar_component in tar libacl1 libselinux1 libpcre2-8-0; do
    dpkg-deb --extract "$tar_packages/$tar_component"*.deb "$tar_extract/$tar_component"
done
tar_runtime_dir="$appdir/usr/libexec/wildbuzzard"
tar_library_dir="$tar_runtime_dir/tar-libs"
install -d -m755 "$tar_library_dir"
install -m755 "$tar_extract/tar/bin/tar" "$tar_runtime_dir/tar.real"
install -m755 "$host_dir/packaging/buzzardos-tar" "$tar_runtime_dir/tar"
install -m755 \
    "$(readlink -f "$tar_extract/libacl1/usr/lib/x86_64-linux-gnu/libacl.so.1")" \
    "$tar_library_dir/libacl.so.1"
install -m755 \
    "$(readlink -f "$tar_extract/libselinux1/lib/x86_64-linux-gnu/libselinux.so.1")" \
    "$tar_library_dir/libselinux.so.1"
install -m755 \
    "$(readlink -f "$tar_extract/libpcre2-8-0/usr/lib/x86_64-linux-gnu/libpcre2-8.so.0")" \
    "$tar_library_dir/libpcre2-8.so.0"
printf '%s  %s\n' "$tar_binary_sha256" "$tar_runtime_dir/tar.real" | sha256sum --check --status
printf '%s  %s\n' "$tar_libacl_sha256" "$tar_library_dir/libacl.so.1" | sha256sum --check --status
printf '%s  %s\n' "$tar_libselinux_sha256" "$tar_library_dir/libselinux.so.1" | sha256sum --check --status
printf '%s  %s\n' "$tar_libpcre2_sha256" "$tar_library_dir/libpcre2-8.so.0" | sha256sum --check --status
for tar_component in tar libacl1 libselinux1 libpcre2-8-0; do
    install -d -m755 "$appdir/usr/share/doc/wildbuzzard/tar-runtime/$tar_component"
    install -m644 "$tar_extract/$tar_component/usr/share/doc/$tar_component/copyright" \
        "$appdir/usr/share/doc/wildbuzzard/tar-runtime/$tar_component/copyright"
done
tar_source_cache="$tar_packages/sources"
tar_source_destination="$appdir/usr/share/doc/wildbuzzard/sources/tar-runtime"
install -d -m755 "$tar_source_cache" "$tar_source_destination"
while IFS=$'\t' read -r source_package filename url checksum; do
    [[ -n "$source_package" && "${source_package:0:1}" != '#' ]] || continue
    [[ "$filename" != */* && "$filename" != .* ]] || {
        echo "unsafe tar runtime source filename: $filename" >&2
        exit 1
    }
    download_verified "$url" "$tar_source_cache/$filename" "$checksum"
    install -m644 "$tar_source_cache/$filename" "$tar_source_destination/$filename"
done < "$project_dir/LICENSES/tar-runtime-sources.tsv"
awk -F '\t' '!/^#/ && NF == 4 {print $4 "  " $2}' \
    "$project_dir/LICENSES/tar-runtime-sources.tsv" \
    > "$tar_source_destination/SHA256SUMS"
rm -rf -- "$tar_extract"
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
    "$host_dir/packaging/BuzzardOS.desktop" \
    "$appdir/org.openresearchtools.buzzardos.desktop"
install -m644 "$host_dir/packaging/icons/buzzardos-512.png" "$appdir/buzzardos.png"
mkdir -p "$appdir/usr/share/applications"
mkdir -p "$appdir/usr/share/metainfo"
cp "$appdir/org.openresearchtools.buzzardos.desktop" "$appdir/usr/share/applications/"
for icon_size in 512 256 128 64 48 32; do
    icon_dir="$appdir/usr/share/icons/hicolor/${icon_size}x${icon_size}/apps"
    mkdir -p "$icon_dir"
    install -m644 \
        "$host_dir/packaging/icons/buzzardos-${icon_size}.png" \
        "$icon_dir/buzzardos.png"
done
install -m644 \
    "$host_dir/packaging/org.openresearchtools.BuzzardOS.metainfo.xml" \
    "$appdir/usr/share/metainfo/org.openresearchtools.buzzardos.appdata.xml"
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
    --executable "$appdir/usr/libexec/wildbuzzard/bwrap" \
    --executable "$appdir/usr/libexec/wildbuzzard/unshare" \
    --executable "$appdir/usr/libexec/wildbuzzard/gst-launch-1.0" \
    --executable "$appdir/usr/libexec/wildbuzzard/pw-dump" \
    --executable "$appdir/usr/libexec/wildbuzzard/gst-plugin-scanner" \
    --executable "$appdir/usr/libexec/wildbuzzard/slirp4netns" \
    --executable "$appdir/usr/libexec/wildbuzzard/nvidia-ctk" \
    --executable "$appdir/usr/libexec/wildbuzzard/nvidia-cdi-hook" \
    --executable "$appdir/usr/libexec/wildbuzzard/nvidia-container-cli" \
    --desktop-file "$appdir/org.openresearchtools.buzzardos.desktop" \
    --icon-file "$appdir/buzzardos.png"
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
# portable application carries the client ABI while still connecting to the user's running
# host service. Install the pinned build-environment client library explicitly.
install -m755 /usr/lib/x86_64-linux-gnu/libpipewire-0.3.so.0 \
    "$appdir/usr/lib/libpipewire-0.3.so.0"

# The host gateway and the nested guest keyboard must compile against the
# same immutable xkeyboard-config files.  Stage a stable AppDir path directly
# from the already-verified Sway runtime artifact; never consult host
# /usr/share/X11/xkb at application runtime.
host_xkb_root="$appdir/usr/share/wildbuzzard/xkb"
install -d -m755 \
    "$appdir/usr/share/wildbuzzard" \
    "$host_xkb_root" \
    "$appdir/usr/share/doc/xkb-data"
cp -a -- "$guest_compositor_runtime/share/X11/xkb/." "$host_xkb_root/"
install -m644 \
    "$guest_compositor_runtime/share/wildbuzzard/xkb-data.manifest.sha256" \
    "$appdir/usr/share/wildbuzzard/xkb-data.manifest.sha256"
install -m644 \
    "$guest_compositor_runtime/share/wildbuzzard/xkb-data.version" \
    "$appdir/usr/share/wildbuzzard/xkb-data.version"
install -m644 \
    "$guest_compositor_runtime/share/doc/xkb-data/copyright" \
    "$appdir/usr/share/doc/xkb-data/copyright"
verify_xkb_payload \
    "$host_xkb_root" \
    "$appdir/usr/share/wildbuzzard/xkb-data.manifest.sha256" \
    "$appdir/usr/share/wildbuzzard/xkb-data.version" \
    "$appdir/usr/share/doc/xkb-data/copyright"

# linuxdeploy resolved the host build's libxkbcommon. Replace every such copy
# with the exact Debian-snapshot library used by the pinned Sway build, so the
# host and guest serialize identical TEXT_V1 keymaps.
find "$appdir/usr/lib" -type f -name 'libxkbcommon.so*' -delete
find "$appdir/usr/lib" -type l -name 'libxkbcommon.so*' -delete
install -m755 \
    "$guest_compositor_runtime/lib/libxkbcommon.so.0" \
    "$appdir/usr/lib/libxkbcommon.so.0"
install -m644 \
    "$guest_compositor_runtime/share/wildbuzzard/libxkbcommon0.manifest.sha256" \
    "$appdir/usr/share/wildbuzzard/libxkbcommon0.manifest.sha256"
install -m644 \
    "$guest_compositor_runtime/share/wildbuzzard/libxkbcommon0.version" \
    "$appdir/usr/share/wildbuzzard/libxkbcommon0.version"
install -d -m755 "$appdir/usr/share/doc/libxkbcommon0"
install -m644 \
    "$guest_compositor_runtime/share/doc/libxkbcommon0/copyright" \
    "$appdir/usr/share/doc/libxkbcommon0/copyright"
verify_pinned_libxkbcommon \
    "$appdir/usr/lib/libxkbcommon.so.0" \
    "$appdir/usr/share/wildbuzzard/libxkbcommon0.manifest.sha256" \
    "$appdir/usr/share/wildbuzzard/libxkbcommon0.version" \
    "$appdir/usr/share/doc/libxkbcommon0/copyright"
verify_elf_relocation_closure \
    "$appdir/usr/bin/wildbuzzard-display" \
    "wildbuzzard-display" \
    "$appdir/usr/lib"
display_ldd=$(LD_LIBRARY_PATH="$appdir/usr/lib" \
    ldd -r -- "$appdir/usr/bin/wildbuzzard-display")
grep -Fq \
    "libxkbcommon.so.0 => $appdir/usr/lib/libxkbcommon.so.0" \
    <<<"$display_ldd" || {
    echo "wildbuzzard-display did not resolve the pinned AppDir libxkbcommon" >&2
    exit 1
}

# Assemble the exact guest runtime only after linuxdeploy has finished so its
# host dependency collector cannot rewrite guest binaries or their private
# $ORIGIN wlroots search path. The launcher migrates this complete revision
# atomically inside the subordinate-ID namespace.
guest_runtime_root="$build_dir/guest-runtime-root"
rm -rf -- "$guest_runtime_root"
install -d -m755 "$guest_runtime_root"
"$project_dir/guest/install-rootfs-assets.sh" \
    "$guest_runtime_root" \
    "$guest_shell" \
    "$guest_settings" \
    "$guest_shortcut_helper" \
    "$guest_clipboard_agent" \
    "$guest_cua_driver" \
    "$guest_compositor_runtime"
guest_revision=$(tr -d '\n' <"$project_dir/guest/ASSET_REVISION")
guest_revision_source="$guest_runtime_root/opt/wildbuzzard/runtime/$guest_revision"
[[ -d "$guest_revision_source" && ! -L "$guest_revision_source" ]] || {
    echo "guest runtime assembler did not create revision $guest_revision" >&2
    exit 1
}
guest_runtime_destination="$appdir/usr/bin/wildbuzzard-guest-runtime"
install -d -m755 "$guest_runtime_destination"
cp -a -- "$guest_revision_source" "$guest_runtime_destination/$guest_revision"
if find "$guest_runtime_destination" -mindepth 1 -type l -print -quit | grep -q .; then
    echo "packaged guest runtime contains a symbolic link" >&2
    exit 1
fi
verify_xkb_payload \
    "$guest_runtime_destination/$guest_revision/share/X11/xkb" \
    "$guest_runtime_destination/$guest_revision/share/wildbuzzard/xkb-data.manifest.sha256" \
    "$guest_runtime_destination/$guest_revision/share/wildbuzzard/xkb-data.version" \
    "$guest_runtime_destination/$guest_revision/share/doc/xkb-data/copyright"
verify_pinned_libxkbcommon \
    "$guest_runtime_destination/$guest_revision/lib/libxkbcommon.so.0" \
    "$guest_runtime_destination/$guest_revision/share/wildbuzzard/libxkbcommon0.manifest.sha256" \
    "$guest_runtime_destination/$guest_revision/share/wildbuzzard/libxkbcommon0.version" \
    "$guest_runtime_destination/$guest_revision/share/doc/libxkbcommon0/copyright"
cmp -s -- \
    "$appdir/usr/share/wildbuzzard/xkb-data.manifest.sha256" \
    "$guest_runtime_destination/$guest_revision/share/wildbuzzard/xkb-data.manifest.sha256" || {
    echo "host and guest pinned XKB manifests differ" >&2
    exit 1
}
cmp -s -- \
    "$appdir/usr/share/doc/xkb-data/copyright" \
    "$guest_runtime_destination/$guest_revision/share/doc/xkb-data/copyright" || {
    echo "host and guest pinned XKB notices differ" >&2
    exit 1
}
cmp -s -- \
    "$appdir/usr/lib/libxkbcommon.so.0" \
    "$guest_runtime_destination/$guest_revision/lib/libxkbcommon.so.0" || {
    echo "host and guest pinned libxkbcommon payloads differ" >&2
    exit 1
}
cmp -s -- \
    "$appdir/usr/share/doc/libxkbcommon0/copyright" \
    "$guest_runtime_destination/$guest_revision/share/doc/libxkbcommon0/copyright" || {
    echo "host and guest pinned libxkbcommon notices differ" >&2
    exit 1
}
install -d -m755 "$appdir/usr/share/doc/wildbuzzard-sway"
install -m644 "$project_dir/LICENSES/upstream/sway-1.12-LICENSE" \
    "$appdir/usr/share/doc/wildbuzzard-sway/LICENSE.sway"
install -m644 "$project_dir/LICENSES/upstream/wlroots-0.20.2-LICENSE" \
    "$appdir/usr/share/doc/wildbuzzard-sway/LICENSE.wlroots"
install -m644 "$project_dir/oci/desktop/SWAY_UPSTREAM.toml" \
    "$appdir/usr/share/doc/wildbuzzard-sway/UPSTREAM.toml"

stage_release_license_payload
python3 "$project_dir/tools/license_audit.py" \
    --stage-appdir-host-notices "$appdir"
"$project_dir/tools/check-licenses.sh" --appdir "$appdir" --structural

rm -rf -- "$final_output"
mv -- "$appdir" "$final_output"
find "$final_output" -type d -exec chmod 0755 {} +
test -x "$final_output/AppRun"
test -x "$final_output/usr/bin/wildbuzzard"
printf 'Built dependency-complete portable application directory: %s\n' "$final_output"
