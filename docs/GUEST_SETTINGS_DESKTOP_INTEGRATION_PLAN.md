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
an ordinary Sway-managed window, paints an explicit opaque root and page stack
over a palette-derived solid drawing layer in focused and backdrop states, and
exposes native GTK accessibility objects
to the private guest AT-SPI bus. It uses no libadwaita, Electron, browser UI,
GNOME Control Center, or permanent GUI process.

The navigation contains exactly these seven pages in this order:

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
│   └── Capped task buttons [on by default]
│
├── Security
│   ├── Change machine password
│   └── Passwordless sudo [off by default]
│
└── Updates
    └── Standard APT/unattended-upgrades status and manual-control guidance
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

The primary guest output uses hardware GLES2 and DMA-BUF transport; there is
no Pixman or CPU-copied shared-memory primary-frame fallback. The renderer
selection reaches the systemd-managed desktop through its runtime environment
file, not only Podman's initial PID 1 environment. Ordinary shared-memory
cursor surfaces remain supported independently of primary-frame transport.

Hardware/rootfs integration (2026-09-05): the APT-installed host now supplies
the external disk as a native root bind through private stock crun, using a
reconstructible runtime anchor. A fresh machine on LUKS/ext4 `nosuid,nodev`
storage booted the actual nested desktop with explicit native
`--userns=keep-id:uid=1000,gid=1000 --device=/dev/dri/renderD128`. Guest UID 1000
rendered through Intel GLES; Firefox on CUA1 used Wayland, hardware WebRender
and WebGL2. The host presentation record confirmed DMA-BUF and explicit sync.
Complete stop/start and restart retained the same container and rootfs.
These results do not cover every GPU, NVIDIA CDI, or other host distributions.
No hidden namespace selection or host device-permission change is permitted;
the selected native mapping must have access to the selected render device.

The Display page presents one row labelled `Scaling`; it does not repeat a
second `Scaling` section heading above that row.

The setting is sent through the owner-only typed scale endpoint and persisted
only after every active Sway output confirms the physical mode, UI scale, and
non-overlapping layout. Native window resize uses that same convergence path,
so the fixed host-facing output and all active guest-only workspace outputs
remain the same size and are repacked without overlap. A rejected change
restores the last confirmed selection and shows a short user-facing error.
After that layout settles, the official desktop re-clamps existing floating
windows into their own resized workspace without focusing them, so stale
absolute frames cannot leak across an adjacent output boundary.

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
same canonical digest atomically. Human input belongs exclusively to `seat0`.
Each `cuaN` has its own `seatN`, keyboard, pointer, focus, pressed state and
output coordinates. CUA creation, input, cancellation and teardown never
replace, release, reset or borrow seat0's input objects or interaction state.
Shell callbacks are dispatched by the exact owning Wayland seat and device,
not registry order, default-seat selection or the last active device. A
released device's queued callbacks cannot affect its replacement or another
seat. Restarts, polling and periodic rebinding are not input recovery.

The human can select and control any CUA workspace through the fixed visible
output. Its selected agent's native cursor is visible to the human. Numbered
CUA screenshots include the normal compositor cursors on the captured output;
the human cursor may appear alongside the agent cursor when sharing that view.
Capture still excludes other outputs and host chrome. Cursor appearance does
not change input ownership: each numbered caller still controls only seatN.
Agents use a compact, contrasting red native cursor theme with no numbers,
labels, trails, animation or overlay surfaces. Application-defined tool cursors
remain usable. Human cursor textures retain their original guest physical
pixels. GTK's scale-aware cursor API receives logical dimensions and hotspot
separately; no downsample-then-upsample cursor rendering is permitted. Native
size and sharpness require acceptance on every supported host GTK version.
When the compositor unmaps its exported cursor surface, the
host must hide that cursor immediately, including null-buffer commits, so it
cannot duplicate a cursor already composited inside the guest frame.
CUA operations do not select the human's visible workspace. Lifecycle commands
are provided by the host manager CLI, using the same operations as the GUI;
machine start, stop, restart and status require no guest-side restart scripts.

CUA focus switches out of an obstructing fullscreen window on its own workspace
using stock Sway state commands, then activates the requested window through
the caller's numbered seat. It never uses default-seat IPC focus or clears
fullscreen windows on unrelated workspaces. Focusing the fullscreen application
itself, a child inside its fullscreen container, or an allowed transient dialog
does not exit fullscreen. Foreign-workspace targets are moved into the caller's
workspace before activation; no intermediate keyboard focus is sent to their
source workspace. This changes neither the renderer nor the host window mode.

Acceptance limitation (2026-09-05): fullscreen focus recovery passes on both
numbered seats, but this does not complete the input-isolation requirement.
Stock Sway 1.12 focuses every seat already on a workspace when a window enters
fullscreen there. The live test reproduced this when seat0 was viewing CUA2;
the subsequent CUA focus recovery itself preserved seat0's focus. Arbitrary
new-window activation and shared-Xwayland focus are separate unresolved cases.
No focus-reset watcher, compositor patch, or hidden launch-staging rule is an
accepted substitute for the required behavior.

The GPU-backed production-compositor regression now separately checks distinct
simultaneous CUA text streams, distinct pointer positions and output screenshots
before launching windows during typing. The existing-window case passed with
6.23 seconds of overlapping exact text, independent pointers and unchanged
seat0 focus. The launch phase still failed with `input_focus_changed`; hardware
rendering does not repair stock Sway's all-seat activation behavior. The
acceptance test continues to fail overall, rather than treating the passing
baseline as complete multi-CUA isolation.

## 4.1. Time and location

The guest shares the kernel clock, so the actual date/time remains automatic
and cannot be manually changed from the guest. The page shows a live local
date/time and an always-enabled automatic-clock state. It does not start a
second NTP client or expose a clock-setting action.

Time zone is guest-local persistent configuration. Its searchable dropdown is
generated from the installed IANA `zone.tab`; it has no free-form path field.
Selecting a validated location applies that exact zone through systemd
`timedatectl`. Settings asks for the machine-local password and invokes the
real distro `sudo`; it does not use Polkit or a privileged Settings service.
The guest timezone can change without changing the host clock.

## 4.2. Security

The official reference image creates the canonical interactive account
`user` at UID/GID 1000 with the documented initial password `buzzard`. This is
a one-time image-construction preset. The host never receives a guest password
and never edits `/etc/passwd`, `/etc/shadow`, PAM, or sudoers during create,
pull, import, clone, export, start, or stop. Imported and cloned root filesystems
therefore keep their existing credential and keyring state unchanged.

Security changes are guest-local. **Change password** presents current, new,
and confirmation password fields and runs the distro `chpasswd` through
authenticated native distro sudo. **Passwordless sudo** is off by default.
Enabling it requires the current machine password and creates only the exact
root-owned `/etc/sudoers.d/91-buzzardos-passwordless` policy for `user` after
`visudo` validation. Disabling it removes only that exact verified policy. No
Polkit rule, host helper, host socket, or host filesystem path participates.

On nosuid machine storage, the existing private guest socket handoff invokes
the unmodified distro sudo with the caller's arguments, descriptors and
terminal context. Sudo still owns authentication, sudoers and command
execution. This is not a host-side elevation service.

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
must retain the selected theme rather than reverting to white. The Thunar
status bar has explicit active and `:backdrop` palette states and must never
become a white strip when the window loses focus.

The Light Thunar hierarchy is explicit in focused and backdrop states:
titlebar and Places navigation use the panel tone, menu/toolbar/status use the
window tone, the file view uses the field tone, and borders remain neutral.
Home, Desktop, and every conventional XDG user directory use Buzzard's orange
place icons rather than inherited blue artwork. One-time reference-image
provisioning initializes the standard FreeDesktop user directories and seeds
Thunar's Places sidebar with Documents and Downloads alongside the
Buzzard-specific `/shared` bookmark. Machine start and desktop login perform
no folder or bookmark setup. Existing `user-dirs.dirs` and GTK bookmarks are
therefore preserved, and a place removed later by the user stays removed.

Light mode must preserve visible depth rather than flattening the desktop into
white-on-white planes. Its desktop, top and bottom panels, navigation rails,
window canvas, raised Settings groups, fields, controls, hover states, borders,
and backdrops use distinct neutral tokens. Settings presents grouped controls
as bordered raised cards on a quieter page canvas, with the orange accent
reserved for the selected navigation row and interaction state.
The reference Light palette uses `#ebebeb` for both shell panels and the
Settings navigation rail, and `#fafafa` for both the solid desktop background
and the non-sidebar Settings window canvas. Active shell segments remain
`#ebebeb` with the existing Cinnamon focus underline; Light hover states use
the darker neutral tone `#dadada`. Accent orange remains reserved for selected
and focused state. These exact values do not alter the independent Dark
palette.

Theme and background colour persist in `~/.config/buzzardos/settings.json`.
Switching Light/Dark selects its recommended solid background; choosing a
custom colour then persists that explicit colour.

## 6. Debian updates

Package updates inside the persistent guest use the distribution's standard
APT and unattended-upgrades mechanisms. This includes `buzzardos-guest`,
`buzzardos-desktop`, and `buzzardoscua` through the signed Open Research Tools
APT source installed by the reference recipe. The host `buzzardos` package is updated by the host's own APT
transaction and is never replaced from inside a guest.

Buzzard OS installs no custom updater daemon, timer, D-Bus API, privileged APT
broker, candidate-plan format, or package transaction UI. The Updates page is
informational: it identifies standard APT as the owner of guest updates and
directs a user who wants manual control to `apt` in Foot. One-time image
provisioning enables the distro's normal periodic APT and unattended-upgrades
units; later package upgrades do not re-run provisioning or rewrite user
policy.

## 7. Desktop and application discovery

The shell discovers valid FreeDesktop launchers through XDG application
directories. Newly installed Debian applications appear without rebuilding
the image. The visual Applications list may scroll, but its complete model
and AT-SPI tree always expose every installed application.

Task buttons are contiguous and borderless without gaps. `Capped task buttons`
defaults on; when enabled each button is at most 260 logical pixels and never
shrinks below 96 logical pixels. `<` and `>` appear together directly after
`Applications`, in that order, only when all running windows cannot fit at that
minimum; each moves the visible window range by exactly five per click. They do
not bracket the task list. When disabled all task buttons share the available
width.

Applications provides case-insensitive search across application name,
generic name, and categories. Search is immediately keyboard-active and is
cleared each time the menu closes. The menu owns a transparent click-away
surface while open, so a click anywhere outside its visible bounds closes it
and is not forwarded into the covered guest application. Context actions pin
or unpin an application persistently; pinned applications remain searchable
and are visibly identified in the menu.

The shell restores the normal pointer whenever it receives pointer entry on
the desktop, taskbar, menu, or transparent click-away surface. A resize or move
cursor selected by an application must not persist over shell-owned empty
space.

Every workspace selector and the `+` button uses its complete rendered
rectangle as the click target, including its lower edge and fractional-scale
positions. The right side of the top bar hosts application system trays using
the guest-private StatusNotifierItem/StatusNotifierWatcher and DBusMenu
protocols. Tray entries react to registration, property and owner-change
signals without periodic process or bus polling; they support their normal
activation and menu actions. They never connect to a host session bus.

An application-titlebar secondary click sends only the target window identity
to the shell. The transient full-output menu surface obtains the current
horizontal position from its normal Wayland pointer-enter event after one
stock-Sway zero-distance cursor-focus refresh, anchors the window controls
there, and consumes the first outside click before closing. The refresh moves
neither axis and returns no coordinates. Neither host input nor Buzzard CUA
writes a last-click coordinate file, so guest processes cannot poll human
input through desktop integration state.

The desktop always starts with Files and Shared. A newly created shortcut is
placed in the first available cell on the first visible desktop page, below
those built-ins when they occupy the leading cells. It immediately appears in
the current viewport. The shell uses the launcher's real FreeDesktop icon,
with a generic fallback only when no safe icon exists.

The shell watches the actual XDG Desktop directory with inotify and rebuilds
only its in-memory desktop model when that directory changes. Successful
Buzzard helper mutations also send the shell one typed `desktop_changed`
notification, so Thunar actions and helper-created folders, renames,
shortcuts, and deletions appear immediately without login, machine restart,
or a periodic directory scan. Files uses the themed `user-home` icon, Shared
uses `folder-publicshare`, and ordinary directories use `folder`.

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

Add AppImage to Applications and Add AppImage to Desktop link to the original
guest-visible Type-2 AppImage. They do not copy it. If the target later moves, the
fixed launcher opens a native guest file chooser and atomically relinks the
registration after validating the replacement.

Thunar supplies exactly five fixed helper actions for one AppImage candidate:
Run AppImage, Extract and Run AppImage (Persistent), Extract and Run
`--no-sandbox`, Add AppImage to Applications, and Add AppImage to Desktop.
Remove from Applications and Remove Desktop Shortcut are not Thunar actions.
The helper validates the file; the XML filter is not a security boundary.
Generated launchers invoke only the fixed helper with an opaque registration
ID and never use `sh -c`.

A managed AppImage's Applications secondary-click menu contains, in order:
Open, Extract and Run, Extract and Run `--no-sandbox`, Pin/Unpin, Add to
Desktop, Rename, and Delete from Applications. Ordinary distribution
applications contain Open, Pin/Unpin, and Add to Desktop, without AppImage-only
operations. Rename updates the managed Applications and Desktop projections,
not the original AppImage filename. Delete from Applications unpins and removes
only the Applications projection. It does not delete the original AppImage,
its extraction, or an explicitly requested Desktop shortcut.

Persistent extraction is atomic and source-adjacent at
`<AppImage>.extracted`. A validated `AppRun` must resolve inside that real,
guest-user-owned directory. The explicit no-sandbox action creates a private
zero-byte, mode-0600 `.no-sandbox` marker inside the extraction. Every normal
launch route checks for the extraction first; when it is absent the original
AppImage runs normally. When present, the helper runs the validated `AppRun`,
retains literal fixed arguments from the first safe top-level desktop entry,
discards arguments containing FreeDesktop field codes, and suppresses any
embedded `--no-sandbox` unless the marker is valid. Applications, generated
Desktop shortcuts, raw Desktop AppImages, Thunar, AT-SPI, and CUA therefore
inherit the same selected persistent mode. The original AppImage remains the
registered identity and is never replaced or deleted by extraction.

Renaming a registered AppImage on the Desktop is one crash-recoverable helper
transaction. Its durable journal, descriptor-bound identity checks,
same-directory no-replace rename, registration update, fsync ordering, and
startup recovery preserve the stable registration, launchers, icon, bytes,
ownership, and mode.

Type-2 FUSE mounting uses one private guest systemd socket whose root half
accepts only the pinned runtime's exact mount/unmount argument shapes, UID/GID
1000 peers, allowed `.mount_*` paths, and the caller's validated libfuse
communication descriptor. It exposes neither a generic root command nor a
Polkit authorization. The mounted filesystem is always read-only, `nosuid`,
and `nodev`; extraction remains the non-FUSE fallback.

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

The complete guest desktop service uses `TasksMax=infinity`; this setting is
owned by the guest mechanics package and delivered by APT, not a boot-time
repair script. Machine definitions default to native Podman `--pids-limit=-1`
before unrestricted user arguments, so explicit per-machine limits still win.
The effective ceiling remains subject to ancestor/host limits and available
resources. This changes no renderer, browser sandbox, namespace, capability,
or other guest service policy. Acceptance must launch Firefox through the
actual Sway desktop service, load ordinary pages, and check the service and
container `pids.events` before and after, including a complete machine restart.

Settings and desktop state live only in the persistent guest rootfs. No guest
setting gains access to host D-Bus, host files outside explicit shares, host
clipboard, host window policy, or another machine. Every managed read/write
rejects unsafe types and symlink escapes and uses bounded data.

Acceptance must rebuild all four Debian packages, install `buzzardos` on the
host, install `buzzardos-guest`, `buzzardos-desktop`, and `buzzardoscua` in a reference image,
launch an actual persistent machine, and then verify at minimum:

- all seven Settings pages at normal and small window sizes;
- the documented `user` / `buzzard` initial credential, authenticated sudo,
  password change, and passwordless-sudo enable/disable round trip;
- scaling persistence and pixel-aligned input;
- output and microphone volume/mute;
- at least US and one non-US physical layout while CUA remains usable;
- Light and Dark screenshots of Settings, desktop, Thunar focused/unfocused,
  taskbar, menus, selection, and installed application windows;
- standard APT/unattended-upgrades configuration with no Buzzard-owned updater
  service, timer, or D-Bus policy;
- AppImage direct launch, persistent extraction, remembered no-sandbox launch,
  registration, add/remove menu and desktop entries, placement, move/relink;
- text and screenshot clipboard snapshots in both directions; and
- AT-SPI names/actions plus CUA pointer, keyboard, screenshots, and windows.

No GitHub Release, package, registry image, tag, or other public object is
created by this acceptance work.
