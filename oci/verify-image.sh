#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

image=${1:?usage: verify-image.sh IMAGE}
container_engine=${BUZZARDOS_CONTAINER_ENGINE:-docker}
expect_cuda=${BUZZARDOS_EXPECT_CUDA:-0}
case "$expect_cuda" in
    0|1) ;;
    *) echo "BUZZARDOS_EXPECT_CUDA must be 0 or 1" >&2; exit 2 ;;
esac
case "$container_engine" in
    docker|podman|buildah) ;;
    *) echo "unsupported container engine: $container_engine" >&2; exit 2 ;;
esac
command -v "$container_engine" >/dev/null 2>&1 || {
    echo "container engine is unavailable: $container_engine" >&2
    exit 1
}

run_shell() {
    if [[ "$container_engine" == buildah ]]; then
        : "${BUZZARDOS_BUILDAH_ROOT:?Buildah verification requires BUZZARDOS_BUILDAH_ROOT}"
        : "${BUZZARDOS_BUILDAH_RUNROOT:?Buildah verification requires BUZZARDOS_BUILDAH_RUNROOT}"
        buildah \
            --root "$BUZZARDOS_BUILDAH_ROOT" \
            --runroot "$BUZZARDOS_BUILDAH_RUNROOT" \
            --storage-driver vfs \
            run "$image" -- env BUZZARDOS_EXPECT_CUDA="$expect_cuda" /bin/sh -ec "$1"
    else
        "$container_engine" run --rm \
            --env BUZZARDOS_EXPECT_CUDA="$expect_cuda" \
            --entrypoint /bin/sh "$image" -ec "$1"
    fi
}

run_shell '
    test "$(stat -c "%u:%g:%a" /home/buzzard)" = "1000:1000:700"
    test "$(stat -c "%u:%g:%a" /home/buzzard/.config)" = "1000:1000:700"
    test "$(stat -c "%u:%g:%a" /home/buzzard/.config/gtk-3.0/bookmarks)" = "1000:1000:600"
    test -d /home/buzzard/Documents
    test -d /home/buzzard/Downloads
    test -s /home/buzzard/.config/user-dirs.dirs
    test -s /home/buzzard/.config/Thunar/uca.xml
    test -x /usr/libexec/buzzardos-guest/sudo
    test -L /usr/local/bin/sudo
    test "$(readlink /usr/local/bin/sudo)" = /usr/libexec/buzzardos-guest/sudo
    test "$(cat /home/buzzard/.config/gtk-3.0/bookmarks)" = "$(printf "%s\n" \
        "file:///home/buzzard/Documents Documents" \
        "file:///home/buzzard/Downloads Downloads" \
        "file:///shared Shared")"
    ! grep -Fq xdg-user-dirs-update /usr/lib/buzzardos/runtime/current/libexec/buzzardos-session
    ! grep -Fq install-thunar-actions /usr/lib/buzzardos/runtime/current/libexec/buzzardos-session
    for command in \
        Xwayland dbus-daemon dbus-run-session ffmpeg firefox-esr \
        foot fusermount3 \
        cua cua1 cua2 buzzardoscua gsettings grim mako mousepad pipewire setpriv sway swaymsg systemctl \
        unsquashfs \
        thunar wireplumber wtype
    do
        command -v "$command" >/dev/null
    done
    runtime_root=/usr/lib/buzzardos/runtime
    runtime_revision=$(readlink "$runtime_root/current")
    runtime="$runtime_root/$runtime_revision"
    for required in \
        "$runtime/libexec/buzzardos-init" \
        "$runtime/libexec/buzzardos-session" \
        "$runtime/libexec/buzzardos-sway-session" \
        "$runtime/libexec/buzzardos-clipboard-agent" \
        "$runtime/libexec/buzzardos-appimage-ready" \
        "$runtime/libexec/buzzardos-fusermount" \
        "$runtime/libexec/buzzardos-fusermount-exec" \
        "$runtime/libexec/buzzardos-integration-agent" \
        /usr/bin/cua \
        /usr/bin/cua1 \
        /usr/bin/cua2 \
        /usr/bin/buzzardoscua \
        /usr/bin/buzzardos-desktop \
        /usr/bin/buzzardos-settings \
        /usr/libexec/buzzardos-desktop/buzzardos-shortcut-helper \
        /usr/bin/sway \
        /usr/bin/swaymsg \
        /usr/share/X11/xkb/rules/evdev \
        /usr/share/X11/xkb/rules/evdev.lst \
        /usr/share/X11/xkb/symbols/us \
        /usr/libexec/buzzardos-shortcut-helper \
        /etc/buzzardos/sway-config \
        /usr/lib/buzzardos/guest-assets.manifest.json \
        /usr/lib/buzzardos/guest-assets.version
    do
        test -s "$required"
    done
	dpkg-query -W \
	    buzzardoscua buzzardos-guest buzzardos-desktop sway \
	    at-spi2-core dconf-gsettings-backend ffmpeg firefox-esr foot fuse3 \
	    gsettings-desktop-schemas libfuse2t64 mousepad squashfs-tools \
	    fonts-noto-color-emoji fonts-noto-core fonts-noto-cjk \
	    libasound2t64 libatk-bridge2.0-0t64 libatk1.0-0t64 libcairo2 \
	    libcups2t64 libdbus-1-3 libexpat1 libgbm1 libglib2.0-0t64 \
	    libglib2.0-bin \
	    libgtk-3-0t64 libgtk-4-1 libnotify-bin libnspr4 libnss3 \
	    libpango-1.0-0 libpulse0 libx11-6 \
	    libxcb1 libxcomposite1 libxdamage1 libxext6 libxfixes3 libxi6 \
		    libxkbcommon0 libxrandr2 python3-apt \
		    pipewire wireplumber xkb-data xwayland >/dev/null
		guest_version=$(dpkg-query -W -f="\${Version}" buzzardos-guest)
		desktop_version=$(dpkg-query -W -f="\${Version}" buzzardos-desktop)
		cua_version=$(dpkg-query -W -f="\${Version}" buzzardoscua)
		test "$(cat /usr/share/buzzardos-guest/version)" = "$guest_version"
		test "$(cat /usr/share/buzzardos-desktop/version)" = "$desktop_version"
		test "$(cat /usr/share/buzzardoscua/version)" = "$cua_version"
		test "$(/usr/bin/buzzardos-desktop --version)" = "Buzzard OS Desktop $desktop_version"
		test "$(/usr/bin/buzzardos-settings --version)" = "Buzzard OS Settings $desktop_version"
		test "$(/usr/bin/cua --version)" = "Buzzard CUA $cua_version"
		test "$(/usr/bin/cua1 --version)" = "Buzzard CUA $cua_version"
		test "$(/usr/bin/cua2 --version)" = "Buzzard CUA $cua_version"
		test "$(/usr/bin/buzzardoscua --version)" = "Buzzard CUA $cua_version"
		test "$(readlink /usr/bin/cua1)" = cua
		test "$(readlink /usr/bin/cua2)" = cua
		test "$(readlink /usr/bin/buzzardoscua)" = cua
		test -s /usr/share/buzzardoscua/SKILL.md
		test ! -e /usr/lib/systemd/system/buzzardoscua.service
		/usr/bin/python3 -c "import apt"
	settings_root=/tmp/buzzardos-gsettings-verifier
	rm -rf "$settings_root"
	install -d -m 0700 -o 1000 -g 1000 \
	    "$settings_root/config" "$settings_root/runtime"
	setpriv --reuid=1000 --regid=1000 --clear-groups \
	    env \
	        HOME=/home/buzzard \
	        XDG_CONFIG_HOME="$settings_root/config" \
	        XDG_RUNTIME_DIR="$settings_root/runtime" \
	        dbus-run-session -- sh -ec "
	            gsettings list-keys org.gnome.desktop.interface | grep -Fxq gtk-theme
	            gsettings list-keys org.gnome.desktop.interface | grep -Fxq icon-theme
	            gsettings list-keys org.gnome.desktop.interface | grep -Fxq color-scheme
	            gsettings set org.gnome.desktop.interface gtk-theme BuzzardOS-Dark
	            gsettings set org.gnome.desktop.interface icon-theme BuzzardOS
	            gsettings set org.gnome.desktop.interface color-scheme prefer-dark
	            gsettings get org.gnome.desktop.interface gtk-theme | grep -Fq BuzzardOS-Dark
	            gsettings get org.gnome.desktop.interface icon-theme | grep -Fq BuzzardOS
	            gsettings get org.gnome.desktop.interface color-scheme | grep -Fq prefer-dark
	        "
	rm -rf "$settings_root"
	case "$(/usr/bin/sway --version)" in
	    "sway version 1.12"*) ;;
	    *) echo "unexpected distro Sway version: $(/usr/bin/sway --version)" >&2; exit 1 ;;
	esac
	sway_relocations=$(ldd -r -- /usr/bin/sway 2>&1)
	! printf '%s\n' "$sway_relocations" | grep -Eiq \
	    "not found|undefined symbol|relocation error|symbol lookup error"
	printf '%s\n' "$sway_relocations" | grep -F "libwlroots" >/dev/null
	printf '%s\n' "$sway_relocations" | grep -F "libxkbcommon.so.0" >/dev/null
	ldd /usr/bin/buzzardos-desktop | grep -F "not found" && exit 1 || true
	ldd /usr/bin/buzzardos-settings | grep -F "not found" && exit 1 || true
	ldd /usr/bin/buzzardos-settings | grep -F "libpulse.so.0" >/dev/null
	ldd /usr/libexec/buzzardos-desktop/buzzardos-shortcut-helper | grep -F "not found" && exit 1 || true
	ldd "$runtime/libexec/buzzardos-clipboard-agent" | grep -F "not found" && exit 1 || true
	test -s /usr/share/buzzardos/applications/footclient.desktop
	test -s /usr/share/applications/org.openresearchtools.BuzzardOS.Settings1.desktop
	test -s /usr/lib/systemd/system/buzzardos-desktop.service
    for library in \
        libasound.so.2 \
        libatk-1.0.so.0 \
        libatk-bridge-2.0.so.0 \
        libatspi.so.0 \
        libcairo.so.2 \
        libcups.so.2 \
        libdbus-1.so.3 \
        libdrm.so.2 \
        libexpat.so.1 \
        libfuse.so.2 \
        libgbm.so.1 \
        libgio-2.0.so.0 \
        libglib-2.0.so.0 \
        libgobject-2.0.so.0 \
        libgtk-3.so.0 \
        libgtk-4.so.1 \
        libnspr4.so \
        libnss3.so \
        libpango-1.0.so.0 \
        libpulse.so.0 \
        libudev.so.1 \
        libwayland-client.so.0 \
        libX11.so.6 \
        libXcomposite.so.1 \
        libXdamage.so.1 \
        libXext.so.6 \
        libXfixes.so.3 \
        libXi.so.6 \
        libxkbcommon.so.0 \
        libXrandr.so.2 \
        libxcb.so.1
    do
        ldconfig -p | grep -F "$library " >/dev/null
    done
    test -L /usr/bin/fusermount
    test "$(readlink -f /usr/bin/fusermount)" = /usr/bin/fusermount3
    python3 - <<"PY"
import hashlib
import json
import os
import re
import stat
from pathlib import Path

root = Path("/")
manifest = json.loads(
    (root / "usr/lib/buzzardos/guest-assets.manifest.json").read_text()
)
assert manifest["schema"] == 1
assert len(manifest["assets"]) >= 7
for relative, record in manifest["assets"].items():
    path = root / relative
    assert path.is_file(), relative
    assert path.stat().st_mode & 0o7777 == record["mode"], relative
assert (root / "usr/lib/buzzardos/guest-assets.version").read_text().strip()

runtime_root = root / "usr/lib/buzzardos/runtime"
root_metadata = runtime_root.lstat()
assert stat.S_ISDIR(root_metadata.st_mode) and not stat.S_ISLNK(root_metadata.st_mode)
assert root_metadata.st_uid == 0 and not (stat.S_IMODE(root_metadata.st_mode) & 0o022)
current = runtime_root / "current"
current_metadata = current.lstat()
assert stat.S_ISLNK(current_metadata.st_mode) and current_metadata.st_uid == 0
revision = os.readlink(current)
assert re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+~-]{0,127}", revision)
revision_dir = runtime_root / revision
revision_metadata = revision_dir.lstat()
assert stat.S_ISDIR(revision_metadata.st_mode) and not stat.S_ISLNK(revision_metadata.st_mode)
assert revision_metadata.st_uid == 0 and not (stat.S_IMODE(revision_metadata.st_mode) & 0o022)
runtime_manifest_path = revision_dir / "runtime.manifest.json"
runtime_manifest = json.loads(runtime_manifest_path.read_text())
assert set(runtime_manifest) == {"schema_version", "revision", "files"}
assert runtime_manifest["schema_version"] == 1
assert runtime_manifest["revision"] == revision
runtime_files = runtime_manifest["files"]
required_runtime = {
    "libexec/buzzardos-clipboard-agent", "libexec/buzzardos-init",
    "libexec/buzzardos-session", "libexec/buzzardos-sway-session",
    "libexec/buzzardos-output-sync", "libexec/buzzardos-desktop-services",
    "libexec/buzzardos-integration-agent",
}
assert required_runtime <= set(runtime_files)
seen = set()
for path in revision_dir.rglob("*"):
    relative = path.relative_to(revision_dir).as_posix()
    metadata = path.lstat()
    assert not stat.S_ISLNK(metadata.st_mode), relative
    assert metadata.st_uid == 0, relative
    assert not (stat.S_IMODE(metadata.st_mode) & 0o022), relative
    if stat.S_ISDIR(metadata.st_mode):
        continue
    assert stat.S_ISREG(metadata.st_mode), relative
    if relative in {"runtime.manifest.json", "readiness.json"}:
        continue
    record = runtime_files[relative]
    assert set(record) == {"sha256", "mode"}, relative
    assert stat.S_IMODE(metadata.st_mode) == record["mode"], relative
    assert hashlib.sha256(path.read_bytes()).hexdigest() == record["sha256"], relative
    seen.add(relative)
assert seen == set(runtime_files)
guest_listing = root / "var/lib/dpkg/info/buzzardos-guest.list"
desktop_listing = root / "var/lib/dpkg/info/buzzardos-desktop.list"
assert guest_listing.is_file() and desktop_listing.is_file()
assert b"/usr/lib/buzzardos/runtime/" in guest_listing.read_bytes()
assert b"/usr/bin/buzzardos-desktop" in desktop_listing.read_bytes()
PY
    test -s /usr/share/doc/buzzardoscua/LICENSE.trycua-cua.md
    grep -F 10279552e2bbe479e367a082f78b1b98ee85a697 \
        /usr/share/doc/buzzardoscua/UPSTREAM.toml
		test -s /etc/apt/apt.conf.d/20auto-upgrades || {
	    echo "standard APT automatic-update configuration is missing" >&2
	    exit 1
	}
		grep -Fq "APT::Periodic::Unattended-Upgrade \"1\";" /etc/apt/apt.conf.d/20auto-upgrades || {
	    echo "standard unattended-upgrades preset is missing" >&2
	    exit 1
	}
	test ! -e /usr/lib/systemd/system/buzzardos-updater.service || {
	    echo "obsolete Buzzard OS updater service leaked into the image" >&2
	    exit 1
	}
	for forbidden in \
	    blender gcc g++ make meson ninja cargo rustc kwin_wayland \
	    wayfire labwc waybar fuzzel \
	    buzzardos-electron-demo
    do
        if command -v "$forbidden" >/dev/null 2>&1; then
	        echo "forbidden runtime command is installed: $forbidden" >&2
	        exit 1
	    fi
    done
	test ! -e /opt/electron || { echo "private Electron payload is installed" >&2; exit 1; }
	test ! -e /usr/include/wlroots-0.20 || { echo "wlroots development headers are installed" >&2; exit 1; }
	test ! -e /usr/lib/x86_64-linux-gnu/pkgconfig/wlroots-0.20.pc || {
	    echo "wlroots development metadata is installed" >&2
	    exit 1
	}
	for build_command in cargo cc meson ninja rustc
    do
	    if command -v "$build_command" >/dev/null 2>&1; then
	        echo "build tool leaked into the image: $build_command" >&2
	        exit 1
	    fi
	done
	for forbidden_package in \
	    blender build-essential cargo chromium cmake dolphin fuzzel g++ gcc git \
	    kwin-wayland labwc make mesa-utils meson ninja-build pavucontrol pkg-config \
	    plasma-workspace rustc vulkan-tools waybar wayfire x11-apps xfce4 \
	    xfce4-panel xfdesktop4 xterm
	do
	    status=$(dpkg-query -W -f="\${db:Status-Abbrev}" "$forbidden_package" 2>/dev/null || true)
	    case "$status" in
	        ii*) echo "forbidden runtime package is installed: $forbidden_package" >&2; exit 1 ;;
	    esac
	done
	cuda_packages=$(dpkg-query -W -f="\${binary:Package}\n" 2>/dev/null | grep -E \
	    "^(cuda-|libcublas-|libcufft-|libcufile-|libcuobjclient-|libcurand-|libcusolver-|libcusparse-|libnccl2$|libnpp-|libnvfatbin-|libnvjitlink-|libnvjpeg-)" || true)
	if [ "$BUZZARDOS_EXPECT_CUDA" = 0 ]; then
	    test -z "$cuda_packages" || {
	        echo "standard image unexpectedly contains CUDA packages:" >&2
	        printf "%s\n" "$cuda_packages" >&2
	        exit 1
	    }
	    test ! -e /usr/local/cuda
	    test ! -e /etc/apt/sources.list.d/cuda-debian13-x86_64.list
	else
	    for cuda_package in \
	        cuda-keyring cuda-compat-13-3 cuda-cudart-13-3 \
	        cuda-libraries-13-3 cuda-nvrtc-13-3 cuda-nvtx-13-3 \
	        cuda-opencl-13-3 libcublas-13-3 libcufft-13-3 \
	        libcufile-13-3 libcuobjclient-13-3 libcurand-13-3 \
	        libcusolver-13-3 libcusparse-13-3 libnccl2 libnpp-13-3 \
	        libnvfatbin-13-3 libnvjitlink-13-3 libnvjpeg-13-3
	    do
	        status=$(dpkg-query -W -f="\${db:Status-Status}" "$cuda_package")
	        case "$status" in
	            installed) ;;
	            *) echo "CUDA runtime package is not installed: $cuda_package" >&2; exit 1 ;;
	        esac
	    done
	    test "$(dpkg-query -W -f="\${Version}" cuda-cudart-13-3)" = 13.3.29-1
	    test "$(dpkg-query -W -f="\${Version}" cuda-libraries-13-3)" = 13.3.1-1
	    test "$(dpkg-query -W -f="\${Version}" cuda-compat-13-3)" = 610.43.02-1
	    test "$(dpkg-query -W -f="\${Version}" libcublas-13-3)" = 13.6.0.2-1
	    test "$(dpkg-query -W -f="\${Version}" libnccl2)" = 2.30.7-1+cuda13.3
	    test -s /usr/share/doc/cuda-keyring/copyright
	    printf '%s  %s\n' \
	        be0f15ae130d46adb2c2aed7229518da353f28f1471d80b4dce62d909c6ceb2d \
	        /usr/share/doc/cuda-keyring/copyright | sha256sum --check --strict -
	    cmp -s /usr/share/doc/cuda-cudart-13-3/copyright \
	        /usr/share/doc/cuda-libraries-13-3/copyright
	    test -s /usr/share/doc/libcublas-13-3/copyright
	    test -L /usr/local/cuda
	    test -d /usr/local/cuda-13.3/compat
	    test -s /etc/apt/sources.list.d/cuda-debian13-x86_64.list
	    grep -Fq /usr/local/cuda/lib64 /etc/ld.so.conf.d/nvidia.conf
	    ldconfig -p | grep -F "libcudart.so" >/dev/null
	    ldconfig -p | grep -F "libcublas.so" >/dev/null
	    test "$CUDA_VERSION" = 13.3.1
	    test "$NVIDIA_VISIBLE_DEVICES" = all
	    test "$NVIDIA_DRIVER_CAPABILITIES" = compute,utility
	    test "$NVIDIA_PRODUCT_NAME" = CUDA
	    command -v nvcc >/dev/null 2>&1 && {
	        echo "CUDA developer compiler leaked into runtime image" >&2
	        exit 1
	    } || true
	    for forbidden_cuda_package in cuda-drivers cuda-toolkit-13-3 nvidia-driver; do
	        status=$(dpkg-query -W -f="\${db:Status-Abbrev}" \
	            "$forbidden_cuda_package" 2>/dev/null || true)
	        case "$status" in
	            ii*) echo "CUDA runtime image contains driver/devel package: $forbidden_cuda_package" >&2; exit 1 ;;
	        esac
	    done
	fi
	test ! -d /source || { echo "source tree leaked into the image" >&2; exit 1; }
    for unit in \
        sys-kernel-config.mount \
        sys-kernel-debug.mount \
        sys-kernel-tracing.mount
    do
	    target=$(readlink "/etc/systemd/system/$unit" || true)
	    if [ "$target" != /dev/null ]; then
	        echo "namespace-inapplicable unit is not masked: $unit -> $target" >&2
	        exit 1
	    fi
    done
'
