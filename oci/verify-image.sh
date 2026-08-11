#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

image=${1:?usage: verify-image.sh IMAGE}

docker run --rm --entrypoint /bin/sh "$image" -ec '
    test "$(stat -c "%u:%g:%a" /home/wildbuzzard)" = "1000:1000:700"
    test "$(stat -c "%u:%g:%a" /home/wildbuzzard/.config)" = "1000:1000:700"
    for command in \
        Xwayland dbus-daemon dbus-run-session ffmpeg firefox-esr \
        foot fusermount3 \
        gsettings grim mako mousepad pipewire setpriv systemctl \
        unsquashfs \
        thunar wireplumber wtype
    do
        command -v "$command" >/dev/null
    done
    runtime_root=/opt/wildbuzzard/runtime
    runtime_revision=$(readlink "$runtime_root/current")
    runtime="$runtime_root/$runtime_revision"
    for required in \
        "$runtime/bin/sway" \
        "$runtime/bin/swaymsg" \
        "$runtime/bin/cua-driver" \
        "$runtime/libexec/wildbuzzard-init" \
        "$runtime/libexec/wildbuzzard-session" \
        "$runtime/libexec/wildbuzzard-sway-session" \
        "$runtime/libexec/wildbuzzard-shell" \
        "$runtime/libexec/wildbuzzard-settings" \
        "$runtime/libexec/wildbuzzard-shortcut-helper" \
        "$runtime/libexec/wildbuzzard-clipboard-agent" \
        "$runtime/libexec/wildbuzzard-updater" \
        "$runtime/libexec/wildbuzzard-appimage-ready" \
        "$runtime/libexec/wildbuzzard-fusermount" \
        "$runtime/libexec/wildbuzzard-fusermount-exec" \
        "$runtime/libexec/wildbuzzard-integration-agent" \
        "$runtime/lib/libxkbcommon.so.0" \
        "$runtime/share/X11/xkb/rules/evdev" \
        "$runtime/share/X11/xkb/rules/evdev.lst" \
        "$runtime/share/X11/xkb/symbols/us" \
        "$runtime/share/wildbuzzard/xkb-data.manifest.sha256" \
        "$runtime/share/wildbuzzard/xkb-data.version" \
        "$runtime/share/wildbuzzard/libxkbcommon0.manifest.sha256" \
        "$runtime/share/wildbuzzard/libxkbcommon0.version" \
        "$runtime/share/doc/xkb-data/copyright" \
        "$runtime/share/doc/libxkbcommon0/copyright" \
        /usr/libexec/wildbuzzard-shortcut-helper \
        /etc/wildbuzzard/sway-config \
        /usr/lib/wildbuzzard/guest-assets.manifest.json \
        /usr/lib/wildbuzzard/guest-assets.version
    do
        test -s "$required"
    done
	dpkg-query -W \
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
	test "$(cat "$runtime/share/wildbuzzard/xkb-data.version")" = \
	    "$(dpkg-query -W -f="\${Version}" xkb-data)"
	test "$(cat "$runtime/share/wildbuzzard/libxkbcommon0.version")" = \
	    "$(dpkg-query -W -f="\${Version}" libxkbcommon0)"
	/usr/bin/python3 -c "import apt, gi"
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
	            gsettings list-keys org.gnome.desktop.interface | grep -Fxq gtk-theme
	            gsettings list-keys org.gnome.desktop.interface | grep -Fxq icon-theme
	            gsettings list-keys org.gnome.desktop.interface | grep -Fxq color-scheme
	            gsettings set org.gnome.desktop.interface gtk-theme WildBuzzard
	            gsettings set org.gnome.desktop.interface icon-theme WildBuzzard
	            gsettings set org.gnome.desktop.interface color-scheme prefer-dark
	            gsettings get org.gnome.desktop.interface gtk-theme | grep -Fq WildBuzzard
	            gsettings get org.gnome.desktop.interface icon-theme | grep -Fq WildBuzzard
	            gsettings get org.gnome.desktop.interface color-scheme | grep -Fq prefer-dark
	        "
	rm -rf "$settings_root"
	case "$("$runtime/bin/sway" --version)" in
	    "sway version 1.12"*) ;;
	    *) echo "unexpected Sway version: $("$runtime/bin/sway" --version)" >&2; exit 1 ;;
	esac
	sway_relocations=$(LD_LIBRARY_PATH="$runtime/lib" ldd -r -- "$runtime/bin/sway" 2>&1)
	! printf '%s\n' "$sway_relocations" | grep -Eiq \
	    "not found|undefined symbol|relocation error|symbol lookup error"
	printf '%s\n' "$sway_relocations" | grep -F "$runtime/lib/libwlroots" >/dev/null
	printf '%s\n' "$sway_relocations" | grep -F "$runtime/lib/libxkbcommon.so.0" >/dev/null
	xkb_relocations=$(LD_LIBRARY_PATH="$runtime/lib" \
	    ldd -r -- "$runtime/lib/libxkbcommon.so.0" 2>&1)
	! printf '%s\n' "$xkb_relocations" | grep -Eiq \
	    "not found|undefined symbol|relocation error|symbol lookup error"
	ldd "$runtime/libexec/wildbuzzard-shell" | grep -F "not found" && exit 1 || true
	ldd "$runtime/libexec/wildbuzzard-settings" | grep -F "not found" && exit 1 || true
	ldd "$runtime/libexec/wildbuzzard-settings" | grep -F "libpulse.so.0" >/dev/null
	ldd "$runtime/libexec/wildbuzzard-shortcut-helper" | grep -F "not found" && exit 1 || true
	ldd "$runtime/libexec/wildbuzzard-clipboard-agent" | grep -F "not found" && exit 1 || true
	test -s /usr/local/share/applications/footclient.desktop
	test -s /usr/share/applications/org.openresearchtools.WildBuzzard.Settings1.desktop
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
    (root / "usr/lib/wildbuzzard/guest-assets.manifest.json").read_text()
)
assert manifest["schema"] == 1
assert len(manifest["assets"]) >= 47
for relative, record in manifest["assets"].items():
    path = root / relative
    assert path.is_file(), relative
    assert path.stat().st_mode & 0o7777 == record["mode"], relative
assert (root / "usr/lib/wildbuzzard/guest-assets.version").read_text().strip()

runtime_root = root / "opt/wildbuzzard/runtime"
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
    "bin/sway", "bin/swaymsg", "bin/cua-driver",
    "libexec/wildbuzzard-shell", "libexec/wildbuzzard-settings",
    "libexec/wildbuzzard-shortcut-helper", "libexec/wildbuzzard-clipboard-agent",
    "libexec/wildbuzzard-updater",
    "share/X11/xkb/rules/evdev", "share/X11/xkb/rules/evdev.lst",
    "share/X11/xkb/symbols/us",
    "share/doc/xkb-data/copyright",
    "lib/libxkbcommon.so.0", "share/doc/libxkbcommon0/copyright",
    "share/wildbuzzard/libxkbcommon0.manifest.sha256",
    "share/wildbuzzard/libxkbcommon0.version",
    "share/wildbuzzard/xkb-data.manifest.sha256",
    "share/wildbuzzard/xkb-data.version",
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
xkb_root = revision_dir / "share/X11/xkb"
xkb_manifest_path = revision_dir / "share/wildbuzzard/xkb-data.manifest.sha256"
recorded_xkb = {}
for line in xkb_manifest_path.read_text(encoding="utf-8").splitlines():
    digest, separator, relative = line.partition("  ")
    assert separator == "  "
    assert re.fullmatch(r"[0-9a-f]{64}", digest)
    assert re.fullmatch(r"[A-Za-z0-9._+/@~-]+", relative)
    assert ".." not in relative and relative not in recorded_xkb
    recorded_xkb[relative] = digest
observed_xkb = {}
for path in xkb_root.rglob("*"):
    metadata = path.lstat()
    assert not stat.S_ISLNK(metadata.st_mode)
    if stat.S_ISDIR(metadata.st_mode):
        continue
    assert stat.S_ISREG(metadata.st_mode)
    relative = path.relative_to(xkb_root).as_posix()
    observed_xkb[relative] = hashlib.sha256(path.read_bytes()).hexdigest()
assert recorded_xkb == observed_xkb
library = revision_dir / "lib/libxkbcommon.so.0"
library_manifest = (
    revision_dir / "share/wildbuzzard/libxkbcommon0.manifest.sha256"
).read_text(encoding="utf-8")
match = re.fullmatch(
    r"([0-9a-f]{64})  lib/libxkbcommon\.so\.0\n", library_manifest
)
assert match is not None
assert hashlib.sha256(library.read_bytes()).hexdigest() == match.group(1)
for listing in (root / "var/lib/dpkg/info").glob("*.list"):
    assert b"/opt/wildbuzzard/runtime/" not in listing.read_bytes(), listing
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
