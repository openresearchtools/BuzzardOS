#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

image=${1:?usage: verify-image.sh IMAGE}
container_engine=${BUZZARDOS_CONTAINER_ENGINE:-docker}
case "$container_engine" in
    docker|podman) ;;
    *) echo "unsupported container engine: $container_engine" >&2; exit 2 ;;
esac
command -v "$container_engine" >/dev/null 2>&1 || {
    echo "container engine is unavailable: $container_engine" >&2
    exit 1
}

"$container_engine" run --rm --entrypoint /bin/sh "$image" -ec '
    test "$(stat -c "%u:%g:%a" /home/buzzard)" = "1000:1000:700"
    test "$(stat -c "%u:%g:%a" /home/buzzard/.config)" = "1000:1000:700"
    for command in \
        Xwayland dbus-daemon dbus-run-session ffmpeg firefox-esr \
        foot fusermount3 \
        buzzardcua gsettings grim mako mousepad pipewire setpriv sway swaymsg systemctl \
        unsquashfs \
        thunar wireplumber wtype
    do
        command -v "$command" >/dev/null
    done
    runtime_root=/opt/buzzardos/runtime
    runtime_revision=$(readlink "$runtime_root/current")
    runtime="$runtime_root/$runtime_revision"
    for required in \
        "$runtime/libexec/buzzardos-init" \
        "$runtime/libexec/buzzardos-session" \
        "$runtime/libexec/buzzardos-sway-session" \
        "$runtime/libexec/buzzardos-shell" \
        "$runtime/libexec/buzzardos-settings" \
        "$runtime/libexec/buzzardos-shortcut-helper" \
        "$runtime/libexec/buzzardos-clipboard-agent" \
        "$runtime/libexec/buzzardos-updater" \
        "$runtime/libexec/buzzardos-appimage-ready" \
        "$runtime/libexec/buzzardos-fusermount" \
        "$runtime/libexec/buzzardos-fusermount-exec" \
        "$runtime/libexec/buzzardos-integration-agent" \
        /usr/bin/buzzardcua \
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
	    buzzardcua buzzardos-guest-desktop sway \
	    at-spi2-core dconf-gsettings-backend ffmpeg firefox-esr foot fuse3 \
	    gsettings-desktop-schemas libfuse2t64 mousepad squashfs-tools \
	    fonts-noto-color-emoji fonts-noto-core fonts-noto-cjk \
	    libasound2t64 libatk-bridge2.0-0t64 libatk1.0-0t64 libcairo2 \
	    libcups2t64 libdbus-1-3 libexpat1 libgbm1 libglib2.0-0t64 \
	    libglib2.0-bin \
	    libgtk-3-0t64 libgtk-4-1 libnotify-bin libnspr4 libnss3 \
	    libpango-1.0-0 libpulse0 libx11-6 \
	    libxcb1 libxcomposite1 libxdamage1 libxext6 libxfixes3 libxi6 \
	    libxkbcommon0 libxrandr2 python3-apt python3-gi python3-pyatspi \
	    pipewire wireplumber xkb-data xwayland >/dev/null
	/usr/bin/python3 -c "import apt, gi"
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
	ldd "$runtime/libexec/buzzardos-shell" | grep -F "not found" && exit 1 || true
	ldd "$runtime/libexec/buzzardos-settings" | grep -F "not found" && exit 1 || true
	ldd "$runtime/libexec/buzzardos-settings" | grep -F "libpulse.so.0" >/dev/null
	ldd "$runtime/libexec/buzzardos-shortcut-helper" | grep -F "not found" && exit 1 || true
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
assert len(manifest["assets"]) >= 47
for relative, record in manifest["assets"].items():
    path = root / relative
    assert path.is_file(), relative
    assert path.stat().st_mode & 0o7777 == record["mode"], relative
assert (root / "usr/lib/buzzardos/guest-assets.version").read_text().strip()

runtime_root = root / "opt/buzzardos/runtime"
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
    "libexec/buzzardos-shell", "libexec/buzzardos-settings",
    "libexec/buzzardos-shortcut-helper", "libexec/buzzardos-clipboard-agent",
    "libexec/buzzardos-updater",
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
desktop_listing = root / "var/lib/dpkg/info/buzzardos-guest-desktop.list"
assert desktop_listing.is_file()
assert b"/opt/buzzardos/runtime/" in desktop_listing.read_bytes()
PY
    test -s /usr/share/doc/buzzardcua/LICENSE.trycua-cua.md
    grep -F 10279552e2bbe479e367a082f78b1b98ee85a697 \
        /usr/share/doc/buzzardcua/UPSTREAM.toml
	for forbidden in \
	    blender gcc g++ make meson ninja cargo rustc kwin_wayland \
	    wayfire labwc waybar fuzzel \
	    buzzardos-electron-demo
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
