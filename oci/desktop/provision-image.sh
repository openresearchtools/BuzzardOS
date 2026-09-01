#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
# One-time reference-image provisioning. This script is copied into the OCI
# build, executed once, and removed. Debian package upgrades never rerun it.
set -eu

if getent passwd 1000 >/dev/null; then
    test "$(getent passwd 1000 | cut -d: -f1)" = buzzard
else
    useradd --create-home --uid 1000 --user-group --shell /bin/bash buzzard
fi

install -d -o buzzard -g buzzard -m 0700 \
    /home/buzzard \
    /home/buzzard/.cache \
    /home/buzzard/.config \
    /home/buzzard/.config/gtk-3.0 \
    /home/buzzard/.config/sway \
    /home/buzzard/.config/sway/config.d \
    /home/buzzard/.local \
    /home/buzzard/.local/share \
    /home/buzzard/.local/state

# The package owns this wrapper under /usr. Reference-image setup places one
# deliberate link ahead of distro sudo without making a Debian package own
# anything under /usr/local.
install -d -m 0755 /usr/local/bin
ln -sfn /usr/libexec/buzzardos-guest/sudo /usr/local/bin/sudo
ln -sfn /usr/libexec/buzzardos-guest/sudo /usr/local/bin/sudoedit

# Construct the initial desktop home exactly once. These are image defaults,
# not login work: package upgrades and machine starts never recreate folders,
# bookmarks, or a user-modified Thunar action file.
setpriv --reuid=1000 --regid=1000 --clear-groups \
    env \
        HOME=/home/buzzard \
        USER=buzzard \
        LOGNAME=buzzard \
        LANG=C.UTF-8 \
        XDG_CONFIG_HOME=/home/buzzard/.config \
        XDG_DATA_HOME=/home/buzzard/.local/share \
        XDG_STATE_HOME=/home/buzzard/.local/state \
        /bin/sh -ec '
            /usr/bin/xdg-user-dirs-update
            printf "%s\n" \
                "file:///home/buzzard/Documents Documents" \
                "file:///home/buzzard/Downloads Downloads" \
                "file:///shared Shared" \
                >"$XDG_CONFIG_HOME/gtk-3.0/bookmarks"
            chmod 0600 "$XDG_CONFIG_HOME/gtk-3.0/bookmarks"
            /usr/libexec/buzzardos-shortcut-helper install-thunar-actions >/dev/null
        '

# Let distro APT perform normal security/package updates. Buzzard OS does not
# install a private root D-Bus updater, check service, or update timer.
install -d -m 0755 /etc/apt/apt.conf.d
install -m 0644 /dev/null /etc/apt/apt.conf.d/20auto-upgrades
printf '%s\n' \
    'APT::Periodic::Update-Package-Lists "1";' \
    'APT::Periodic::Unattended-Upgrade "1";' \
    > /etc/apt/apt.conf.d/20auto-upgrades

systemctl enable apt-daily.timer apt-daily-upgrade.timer >/dev/null
systemctl mask \
    console-getty.service \
    dev-hugepages.mount \
    getty@.service \
    systemd-logind.service \
    systemd-modules-load.service \
    systemd-remount-fs.service \
    systemd-udevd.service \
    systemd-udevd-control.socket \
    systemd-udevd-kernel.socket \
    sys-kernel-config.mount \
    sys-kernel-debug.mount \
    sys-kernel-tracing.mount

# The committed OCI is install media, not a running machine. Each new machine
# receives its local identity on first boot. SSH is deliberately not installed
# or configured by Buzzard OS.
rm -f /etc/machine-id /var/lib/dbus/machine-id
install -m 0444 /dev/null /etc/machine-id
rm -f /etc/ssh/ssh_host_* 2>/dev/null || true
