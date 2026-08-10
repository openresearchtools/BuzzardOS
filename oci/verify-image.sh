#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

image=${1:?usage: verify-image.sh IMAGE}

docker run --rm --entrypoint /bin/sh "$image" -ec '
    test "$(stat -c "%u:%g:%a" /home/wildbuzzard)" = "1000:1000:700"
    test "$(stat -c "%u:%g:%a" /home/wildbuzzard/.config)" = "1000:1000:700"
    for command in \
        Xwayland cua-driver dbus-daemon dbus-run-session ffmpeg firefox-esr \
        foot fusermount3 \
        gsettings grim mako mousepad pipewire setpriv sway swaymsg systemctl \
        thunar wireplumber wtype
    do
        command -v "$command" >/dev/null
    done
    for required in \
        /usr/libexec/wildbuzzard-init \
        /usr/libexec/wildbuzzard-session \
        /usr/libexec/wildbuzzard-sway-session \
        /usr/libexec/wildbuzzard-shell \
        /usr/libexec/wildbuzzard-appimage-ready \
        /usr/libexec/wildbuzzard-fusermount \
        /usr/libexec/wildbuzzard-fusermount-exec \
        /usr/libexec/wildbuzzard-integration-agent \
        /usr/local/bin/cua-driver \
        /etc/wildbuzzard/sway-config \
        /usr/lib/wildbuzzard/guest-assets.manifest.json \
        /usr/lib/wildbuzzard/guest-assets.version
    do
        test -s "$required"
    done
	dpkg-query -W \
	    at-spi2-core dconf-gsettings-backend ffmpeg firefox-esr foot fuse3 \
	    gsettings-desktop-schemas libfuse2t64 mousepad \
	    fonts-noto-color-emoji fonts-noto-core fonts-noto-cjk \
	    libasound2t64 libatk-bridge2.0-0t64 libatk1.0-0t64 libcairo2 \
	    libcups2t64 libdbus-1-3 libexpat1 libgbm1 libglib2.0-0t64 \
	    libglib2.0-bin \
	    libgtk-3-0t64 libnspr4 libnss3 libpango-1.0-0 libx11-6 \
	    libxcb1 libxcomposite1 libxdamage1 libxext6 libxfixes3 libxi6 \
	    libxkbcommon0 libxrandr2 python3-pyatspi \
	    pipewire wireplumber xwayland >/dev/null
	settings_root=/tmp/wildbuzzard-gsettings-verifier
	rm -rf "$settings_root"
	install -d -m 0700 -o 1000 -g 1000 \
	    "$settings_root/config" "$settings_root/runtime"
	setpriv --reuid=1000 --regid=1000 --clear-groups \
	    env \
	        HOME=/home/wildbuzzard \
	        XDG_CONFIG_HOME="$settings_root/config" \
	        XDG_RUNTIME_DIR="$settings_root/runtime" \
	        dbus-run-session -- sh -ec "
	            gsettings set org.gnome.desktop.interface gtk-theme WildBuzzard
	            gsettings set org.gnome.desktop.interface icon-theme WildBuzzard
	            gsettings set org.gnome.desktop.interface color-scheme prefer-dark
	            gsettings get org.gnome.desktop.interface gtk-theme | grep -Fq WildBuzzard
	            gsettings get org.gnome.desktop.interface icon-theme | grep -Fq WildBuzzard
	            gsettings get org.gnome.desktop.interface color-scheme | grep -Fq prefer-dark
	        "
	rm -rf "$settings_root"
	case "$(sway --version)" in
	    "sway version 1.12"*) ;;
	    *) echo "unexpected Sway version: $(sway --version)" >&2; exit 1 ;;
	esac
	ldd "$(command -v sway)" | grep -F "not found" && exit 1 || true
	ldd /usr/libexec/wildbuzzard-shell | grep -F "not found" && exit 1 || true
	test -s /usr/local/share/applications/footclient.desktop
	test -s /usr/lib/systemd/system/wildbuzzard-desktop.service
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
        libnspr4.so \
        libnss3.so \
        libpango-1.0.so.0 \
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
import json
from pathlib import Path

root = Path("/")
manifest = json.loads(
    (root / "usr/lib/wildbuzzard/guest-assets.manifest.json").read_text()
)
assert manifest["schema"] == 1
assert len(manifest["assets"]) >= 47
for relative, record in manifest["assets"].items():
    path = root / relative
    assert path.is_file(), relative
    assert path.stat().st_mode & 0o7777 == record["mode"], relative
assert (root / "usr/lib/wildbuzzard/guest-assets.version").read_text().strip()
PY
    test -s /usr/share/doc/wildbuzzard-cua/LICENSE.trycua-cua.md
    grep -F 10279552e2bbe479e367a082f78b1b98ee85a697 \
        /usr/share/doc/wildbuzzard-cua/UPSTREAM.toml
    grep -F 88869399f421d9180dd8b6ed0b5a1f4a3585d252 \
        /usr/share/doc/wildbuzzard-sway/UPSTREAM.toml
    grep -F d783533489e1f75d6886c2ab5c5960090ef268f8 \
        /usr/share/doc/wildbuzzard-sway/UPSTREAM.toml
    test -s /usr/share/doc/wildbuzzard-sway/LICENSE.sway
    test -s /usr/share/doc/wildbuzzard-sway/LICENSE.wlroots
	for forbidden in \
	    blender gcc g++ make meson ninja cargo rustc kwin_wayland \
	    wayfire labwc waybar fuzzel \
	    wildbuzzard-electron-demo
    do
        ! command -v "$forbidden" >/dev/null 2>&1
    done
    test ! -e /opt/electron
    test ! -e /usr/include/wlroots-0.20
    test ! -e /usr/lib/x86_64-linux-gnu/pkgconfig/wlroots-0.20.pc
	for build_command in cargo cc meson ninja rustc
    do
        ! command -v "$build_command" >/dev/null 2>&1
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
	test ! -d /source
    for unit in \
        sys-kernel-config.mount \
        sys-kernel-debug.mount \
        sys-kernel-tracing.mount
    do
        test "$(readlink "/etc/systemd/system/$unit")" = /dev/null
    done
'
