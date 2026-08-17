# Guest Settings and Desktop Integration Contract

- Status: authoritative implementation contract
- Parent specification: `../AGENTS.md`
- Applies to: the persistent guest desktop, Settings, package updates,
  desktop files, themes, AppImages, and host-authorized clipboard snapshots

This document records the user-facing contract. It deliberately excludes
host-machine configuration, display diagnostics, media-bridge diagnostics,
branding/logo controls, repair tools, and developer information from the
guest Settings application.

## 1. Settings application

`buzzardos-settings` is a standalone, adaptive Rust/GTK4 application. It is
an ordinary Sway-managed window and exposes native GTK accessibility objects
to the private guest AT-SPI bus. It uses no libadwaita, Electron, browser UI,
GNOME Control Center, or permanent GUI process.

The navigation contains exactly these six pages in this order:

```text
Settings
├── Display
│   └── Scaling
│
├── Sound
│   ├── Output volume
│   ├── Output mute
│   ├── Microphone input volume
│   └── Microphone mute
│
├── Keyboard
│   ├── Language
│   ├── Layout
│   └── Hardware
│
├── Time & Location
│   ├── Automatic date and time
│   ├── Current local date and time
│   └── Time zone [searchable IANA location dropdown]
│
├── Appearance
│   ├── Theme
│   │   ├── Light
│   │   └── Dark
│   └── Background colour
│
└── Updates
    ├── Check for updates
    ├── Available updates
    └── Install now
```

There is no About page, Applications/Desktop page, host-integration page,
diagnostics page, repair page, separate physical/logical-resolution report,
runtime readiness report, logo/background-image choice, or raw XKB editor.
The small-window layout scrolls instead of clipping controls. Each listed
control has a stable accessible name, value/state, and keyboard focus order.

## 2. Display

Scaling choices are Automatic, 100%, 125%, 150%, 175%, and 200%. Scaling is a
guest UI-density setting. The native host window continues to determine the
guest monitor's physical pixel dimensions, and the display path must not
bitmap-stretch the guest output.

The Display page presents one row labelled `Scaling`; it does not repeat a
second `Scaling` section heading above that row.

The setting is sent through the owner-only typed scale endpoint and persisted
only after the active Sway output confirms it. A rejected change restores the
last confirmed selection and shows a short user-facing error.

## 3. Sound

Sound presents exactly two devices: the default guest output and the default
guest microphone input. Each has one volume control and one mute control.
The implementation talks to the guest-private PipeWire/Pulse service; it does
not display host PipeWire details, bridge status, stream inventories, test
generators, diagnostics, camera controls, or device-routing controls.

## 4. Keyboard

Language selects a curated group of common XKB layouts. Layout selects the
actual layout inside that language. Hardware defaults to Generic 105-key PC
and also offers the standard 104-key and 101-key PC models for keyboards that
need them. The UI contains no free-form component fields.

One selection change is one complete typed keyboard transaction. The exact
RMLVO names are compiled against the protected pinned XKB data on both sides
of the nested physical-keyboard boundary. The guest and host activate the
same canonical digest atomically. CUA uses its own distinct virtual keyboard
on the same Sway seat; enabling CUA never disables ordinary human input.

## 4.1. Time and location

The guest shares the kernel clock, so the actual date/time remains automatic
and cannot be manually changed from the guest. The page shows a live local
date/time and an always-enabled automatic-clock state. It does not start a
second NTP client or expose a clock-setting action.

Time zone is guest-local persistent configuration. Its searchable dropdown is
generated from the installed IANA `zone.tab`; it has no free-form path field.
Selecting a validated location applies that exact zone through systemd
`timedatectl`. The guest timezone can change without changing the host clock.

## 5. Appearance

Theme contains only Light and Dark. Background colour is one native colour
picker for a uniform solid desktop colour. There are no wallpaper images,
logos, logo presets, previews, downloads, gradients, or branding controls.

Light and Dark share exactly the same geometry and interaction design. Only
palette tokens change. Both cover the Rust shell, taskbar, desktop items,
window decorations, Settings, GTK3, GTK4, Thunar, Mousepad, Foot, compatible
Qt applications, and compatible Electron/Chromium applications. Selection,
focus, sliders, switches, checks, radio buttons, folders, and unfocused views
use the accessible Cinnamon-orange accent and warm graphite/light neutrals;
no default blue selection is allowed. Unfocused Thunar content and menu bars
must retain the selected theme rather than reverting to white.

Theme and background colour persist in `~/.config/buzzardos/settings.json`.
Switching Light/Dark selects its recommended solid background; choosing a
custom colour then persists that explicit colour.

## 6. Debian updates

The Updates page manages packages inside the persistent Debian rootfs only,
including `buzzardos-guest-desktop` and `buzzardcua` when their configured APT
repository offers newer versions. The host `buzzardos` package is updated by
the host's own APT transaction and is never replaced from inside a guest.

`Check for updates` starts one fixed fresh updater worker. The worker refreshes
APT metadata and creates an exact candidate plan. The scrollable list shows
the package name, installed version, candidate version, and download size.
`Install now` installs only that opaque plan generation after revalidation.
There is no arbitrary command, package, path, repository, environment, or APT
argument surface.

`Check for updates` and `Install now` are visually complete Cinnamon-orange
buttons with dark high-contrast text, not label-like highlights. Active work
shows a native progress bar and a textual phase: repository refresh, plan
resolution, the current Debian archive with percentage/bytes and measured
download speed, the current package/install count, completion, or the bounded
failure reason.

The system-bus service is guest-root-owned and callable only by guest root or
the interactive UID 1000 user. It permits only the exact APT plan resolved
from configured repositories; there is no arbitrary package or command
surface. Buzzard OS guest files are dpkg-owned and updated only as versioned
packages. Updates are never installed automatically.

## 7. Desktop and application discovery

The shell discovers valid FreeDesktop launchers through XDG application
directories. Newly installed Debian applications appear without rebuilding
the image. The visual Applications list may scroll, but its complete model
and AT-SPI tree always expose every installed application.

The desktop always starts with Files and Shared. A newly created shortcut is
placed in the first available cell on the first visible desktop page, below
those built-ins when they occupy the leading cells. It immediately appears in
the current viewport. The shell uses the launcher's real FreeDesktop icon,
with a generic fallback only when no safe icon exists.

Owner-owned regular `.desktop` files in the guest Desktop directory are
automatically owner-executable after validation, so activating a shortcut
launches it without a redundant trust prompt. Symlinks, non-regular files,
foreign-owned files, and unsafe launchers are never auto-authorized.

The on-desktop visual and AT-SPI label of a valid launcher is its localized
FreeDesktop `Name=`, matching the Applications menu (for example,
`Firefox ESR`). The `.desktop` suffix and storage ID remain visible as the
real filename in file managers, but are never shown as the desktop icon label.

The desktop provides selection, Ctrl/Shift selection, rubber-band selection,
Cut, Copy, Paste, Rename, New Folder, Arrange, and confirmed Delete. Delete is
always a modal destructive action. Deleting a shortcut removes only the
shortcut, not its target. These actions are directly accessible through
AT-SPI as well as pointer/keyboard interaction.

## 8. AppImage integration

Add to Applications and Add Desktop Shortcut link to the original guest-
visible Type-2 AppImage. They do not copy it. If the target later moves, the
fixed launcher opens a native guest file chooser and atomically relinks the
registration after validating the replacement.

Thunar supplies exactly two fixed helper actions for an AppImage candidate:
Add to Applications and Add Desktop Shortcut. The helper validates the file;
the XML filter is not a security boundary. Generated launchers invoke only
the fixed helper with an opaque registration ID and never use `sh -c`.

Renaming a registered AppImage on the Desktop is one crash-recoverable helper
transaction. Its durable journal, descriptor-bound identity checks,
same-directory no-replace rename, registration update, fsync ordering, and
startup recovery preserve the stable registration, launchers, icon, bytes,
ownership, and mode.

## 9. Explicit clipboard snapshots

The host and guest clipboards remain separate. The native host header exposes
only `Send Host Clipboard to Guest` and `Copy Guest Clipboard to Host`. Each
click creates one bounded snapshot transaction. The guest never receives a
host clipboard object, host listener, subscription, history, socket path, or
authority to initiate a host clipboard read.

Version 1 accepts valid UTF-8 plain text and ordinary still images offered as
PNG, JPEG, WebP, BMP, or TIFF. Images are decoded under limits and carried as
canonical PNG bytes entirely in memory. HTML, RTF, SVG, animations, file
lists, objects, and executable formats are rejected. Peer credentials,
endpoint ownership/mode/inode, nonce, direction, size, timeouts, and
single-flight state are checked before a value reaches either clipboard.
Clipboard bytes and hashes are never logged or persisted.

## 10. Persistence, security, and acceptance

Settings and desktop state live only in the persistent guest rootfs. No guest
setting gains access to host D-Bus, host files outside explicit shares, host
clipboard, host window policy, or another machine. Every managed read/write
rejects unsafe types and symlink escapes and uses bounded data.

Acceptance must rebuild all three Debian packages, install `buzzardos` on the
host, install `buzzardos-guest-desktop` and `buzzardcua` in a reference image,
launch an actual persistent machine, and then verify at minimum:

- all five Settings pages at normal and small window sizes;
- scaling persistence and pixel-aligned input;
- output and microphone volume/mute;
- at least US and one non-US physical layout while CUA remains usable;
- Light and Dark screenshots of Settings, desktop, Thunar focused/unfocused,
  taskbar, menus, selection, and installed application windows;
- APT check, scrollable plan, and fixed-plan installation against a signed
  local fixture before any real update is accepted;
- AppImage registration, desktop icon/placement/trust, move/relink, and launch;
- text and screenshot clipboard snapshots in both directions; and
- AT-SPI names/actions plus CUA pointer, keyboard, screenshots, and windows.

No GitHub Release, package, registry image, tag, or other public object is
created by this acceptance work.
