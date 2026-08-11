# Guest Settings and Desktop Integration Plan

- Status: approved implementation contract
- Applies to: Wild Buzzard guest desktop, reference rootfs, packaging,
  migration, CUA, and release acceptance
- Parent specification: `../AGENTS.md`

This document turns the agreed Settings, desktop, AppImage, update, sound,
display-scale, branding, and explicit clipboard-transfer decisions into one
implementation plan. It is not a speculative design note. Implementations and
tests must satisfy this
contract. If this file and `AGENTS.md` ever disagree, the stricter requirement
applies and both files must be corrected in the same change.

## 1. Product outcome

The guest gains a small native Settings application and a normal persistent
desktop without turning the Rust shell into a general-purpose widget toolkit.
The finished experience must provide:

- a native, accessible Settings window;
- persistent light and dark themes;
- guest UI-scale controls that never reduce the physical output resolution;
- persistent keyboard language/layout, variant, Compose, and layout-switching
  controls for the private Sway seat;
- controls for the guest's private PipeWire audio graph;
- visible Debian package-update status and an explicit Update Now workflow;
- automatic discovery of applications installed through normal Debian
  packages;
- link-in-place registration of Type-2 AppImages;
- relinking through a file chooser when a registered AppImage moves;
- desktop shortcuts for installed applications and AppImages;
- normal desktop cut, copy, paste, rename, and confirmed delete operations;
- two host-authorized, one-shot text/still-image clipboard snapshot actions
  without a shared or continuously synchronized host/guest clipboard;
- a distinctive common-buzzard logo and resolution-independent wallpaper; and
- full AT-SPI and in-guest CUA access to every control and desktop item.

The host launcher and the guest remain separate products at the trust
boundary. Guest Settings must never gain access to the host desktop, host
filesystem beyond explicit shares, host D-Bus, host clipboard service, or host
device controls. A host-authorized clipboard snapshot gives the guest only the
copied bytes, never access to the source clipboard.

## 2. Decisions that are final

### 2.1 Settings framework

`wildbuzzard-settings` is a standalone Rust application built with plain
`gtk4-rs` plus `gio` and `glib`. It does not use libadwaita, Electron, a
WebView, a browser UI, GNOME Control Center, or a permanent GUI daemon.

GTK4 is used because it supplies a normal Wayland `xdg_toplevel`, keyboard and
IME handling, lists, sliders, dialogs, focus behavior, and native AT-SPI. The
existing layer-shell desktop process remains deliberately small and is not
expanded into a hand-written form toolkit.

### 2.2 AppImages are linked in place

Add to Applications and Add Desktop Shortcut do **not** copy the AppImage.
They register the original guest-visible path. The file may live in
`/shared`, the guest home directory, or another guest-visible location.

Moving, renaming, unmounting, or deleting that original file is allowed to
break the link temporarily. When the user activates a launcher whose target
is missing, Wild Buzzard opens a native guest file chooser and asks the user
to locate the AppImage. Selecting a valid replacement atomically relinks every
launcher backed by that registration. Cancelling leaves the registration
unchanged and reports that the target is unavailable.

A future explicit advanced Copy into Machine action may be considered, but it
is not part of this implementation and must never become the default.

### 2.3 Desktop deletion is explicit and confirmed

Delete is destructive. It always opens a modal with the affected item name,
an unambiguous explanation, and exactly two primary choices: Delete and
Cancel. Keyboard activation and CUA invocation follow the same confirmation
path.

- Deleting a desktop shortcut removes only the shortcut. It never deletes the
  application, AppImage, or file referenced by that shortcut.
- Deleting a symbolic link removes only the link.
- Deleting a regular desktop file deletes that file.
- Deleting a folder deletes the folder and its contents only after the modal
  states that the contents will also be deleted.
- Cancelling makes no filesystem or registration change.

There is no silent permanent delete, no deletion triggered merely by removing
an Applications-menu registration, and no path traversal through a shortcut
target.

### 2.4 Updates are checked automatically and installed manually

The guest may check for Debian package updates on a timer and at explicit user
request. It never installs updates automatically. Update Now requires a user
action, shows the exact plan, and reports progress and failures.

Guest package updates and Wild Buzzard host-AppImage updates are separate:

- the Updates page manages packages inside the persistent guest rootfs;
- replacing the host AppImage is a host action and is never performed by the
  guest updater; and
- Wild Buzzard-managed guest integration assets are migrated by the launcher,
  not owned by `apt`.

### 2.5 Physical pixels and guest UI scale are independent

The host window determines the guest monitor's physical pixel dimensions.
Settings may change guest UI density, but may not resize or resample the final
physical framebuffer. A 1595 by 940 guest monitor remains a 1595 by 940 dmabuf
and CUA screenshot regardless of the selected UI scale.

## 3. Repository architecture

Add these guest workspace components:

```text
guest/
├── desktop-core/       shared schemas, validation, desktop entries and files
├── settings/           wildbuzzard-settings GTK4 application
├── shortcut-helper/    registration, safe launch, relink and file actions
├── updater/            fixed-operation privileged update service
├── shell/              existing panel/menu/desktop, extended through core
└── assets/             themes, branding, service units and integrations
```

`desktop-core` is the only implementation of:

- the versioned Settings schema;
- XDG application and desktop-directory discovery;
- `.desktop` parsing and generation;
- AppImage registration records and stable IDs;
- atomic user-file operations;
- theme tokens;
- update-state parsing; and
- canonical logical-to-physical display metadata.

The shell, Settings application, shortcut helper, and tests consume this
crate. They must not grow separate parsers or subtly different validation
rules.

### 3.1 Processes and buses

`wildbuzzard-settings` runs as the normal interactive user and owns the
session-bus name:

```text
org.openresearchtools.WildBuzzard.Settings1
```

The name provides single-instance activation and typed notifications for
settings changes. It does not provide privileged methods.

The updater owns a separate private guest system-bus name:

```text
org.openresearchtools.WildBuzzard.Updater1
```

Only the fixed methods described in the Updates section are exposed. Neither
interface accepts arbitrary commands, shell fragments, filesystem paths,
package-manager flags, or environment variables.

Guest UI-scale changes use a separate, narrowly typed session interface
provided by the display runtime. Its only mutating request is conceptually:

```text
SetGuestScale {
    preset,
    current_geometry_generation
}
```

The display application validates the enumerated preset and generation, then
atomically returns the resulting physical and logical geometry. This interface
cannot resize or move the host window, edit machine configuration, select
devices, access another machine, or issue an arbitrary host command.

Keyboard changes use a second owner-only session interface whose only
mutating request is conceptually:

```text
SetGuestKeyboard {
    model,
    layout,
    variant,
    options
}
```

Every field is length-bounded and restricted to the same ASCII XKB
component-name syntax enforced by desktop-core and the host parser. Layout has
one to four non-empty comma-separated groups. Variant is globally empty or has
at most the matching number of comma-aligned slots; an empty aligned slot such
as `,nodeadkeys` is valid. A non-empty options list has no empty segment.
The service compiles the complete keymap with the protected pinned
libxkbcommon and XKB data before coordinating one paired host/Sway
transaction. The host receives only the same bounded RMLVO fields, a random
transaction token, and the canonical keymap digest; it never receives a guest
path or serialized keymap. Prepare queues only parent physical-keyboard
events, the guest applies and confirms one sealed keymap on the exact
nested keyboard, and Commit atomically activates the matching native
modifier/group state before replay. A failed apply restores the prior guest
map before Abort releases queued input. The interface does not accept a
command, shell fragment, caller-selected path, environment variable, or host
keyboard object. Invalid combinations are rejected without changing the
persisted setting. CUA keeps its own persistent synthetic keyboard on the
same private Sway seat so semantic agent typing and human layout selection
coexist.

### 3.2 Persistent state

User-owned state lives in the persistent guest home:

```text
~/.config/wildbuzzard/settings.json
~/.local/share/wildbuzzard/appimages/<id>.json
~/.local/share/applications/wildbuzzard-appimage-<id>.desktop
~/.local/share/icons/hicolor/<size>/apps/wildbuzzard-appimage-<id>.png
~/.local/state/wildbuzzard/
$XDG_DESKTOP_DIR (falling back to ~/Desktop)
```

Root-owned updater state lives at:

```text
/var/lib/wildbuzzard-updater/state.json
/var/lib/wildbuzzard-updater/plan.json
/var/log/wildbuzzard-updater/
/run/lock/wildbuzzard-updater.lock
```

Every managed write uses a same-directory temporary file, validated content,
`fsync`, atomic rename, and directory `fsync`. Schemas carry a version.
Unknown newer schema versions produce a visible diagnostic and preserve the
file; they are never silently reset.

No state in this feature is written to the host home or outside the portable
machine rootfs and explicit `shared/` mount.

## 4. Settings application

The application appears once in the Applications menu as `Settings` with the
Buzzard Settings icon. It opens as a normal Sway-managed window and supports
titlebar move, edge resize, minimize, maximize/restore, close, taskbar focus,
and all existing CUA window actions.

The window is adaptive:

- a compact single-column layout is used at small guest sizes;
- navigation and content may use two columns when space permits;
- no fixed pixel width assumes one host resolution or scale;
- all pages scroll when their natural content exceeds the viewport;
- keyboard traversal follows visual order; and
- every label, button, switch, slider, list row, status, and dialog is exposed
  through AT-SPI with a stable accessible name and current state.

Pages are:

1. Appearance
2. Display
3. Keyboard
4. Sound
5. Updates
6. Applications & Desktop
7. About

There is no Apply button for changes that can be safely committed
independently. Keyboard model/layout/variant/options are one grouped keymap
transaction and therefore have one explicit Apply action after local
validation. Changes requiring a session restart say so before confirmation.
Every failed change restores the last confirmed state and shows the actual
error.

## 5. Appearance and theming

### 5.1 User controls

Appearance provides:

- Dark;
- Light; and
- a preview containing text, controls, selection, folders, the taskbar, and
  the Buzzard mark.

Desktop Background provides four built-in suggestions plus a custom solid
colour:

- Dark Plain;
- Dark + Logo;
- Light Plain;
- Light + Logo; and
- Custom Solid Colour.

The plain and logo variants of each theme use the same recommended background
colour, so adding the mark never changes the surrounding desktop colour. The
custom option is a native accessible colour control and always produces one
uniform colour: it does not introduce gradients, images, remote downloads, or
per-monitor bitmap scaling. Background choice and theme mode persist
independently, allowing a user to retain an explicitly chosen background while
changing application colours.

The selected mode persists across Stop/Start. Session startup reads the
setting instead of unconditionally forcing dark mode.

### 5.2 Theme coverage

Ship complete `WildBuzzard-Dark` and `WildBuzzard-Light` themes and propagate
the selection to:

- the Rust desktop shell and wallpaper;
- GTK3 and GTK4;
- the guest color-scheme portal used by compatible Electron/Chromium apps;
- Qt/KDE color configuration without starting Plasma or KWallet;
- Foot;
- Mako; and
- the Settings application itself.

The graphite/cinnamon visual language remains consistent. Blue selection,
focus, window-frame, taskbar, and file-manager accents are replaced with the
theme's accessible cinnamon accent. Selection must remain distinguishable
from hover and focus in both themes.

Light mode is a conservative color translation of Dark mode, not a second
visual design. It keeps exactly the same widget geometry, spacing, padding,
border widths, corner treatment, titlebar and taskbar dimensions, typography,
icon geometry, shadows, and interaction states. Only shared palette tokens
change from dark graphite neutrals to restrained warm-light neutrals while
retaining the cinnamon accent. It must not introduce bright blue accents,
stark-white expanses, different control shapes, or larger “light theme”
metrics.

Applications that do not support live recoloring may require reopening. The
UI states that limitation instead of claiming the application changed.

### 5.3 Theme implementation

Theme values are typed tokens in `desktop-core`, not independently copied
color literals. The shell reloads its palette and redraws surfaces on a
settings-generation change. Toolkit and application configuration is written
atomically, then the appropriate guest-only settings notifications are sent.
Dark and Light consume one shared geometry stylesheet and two palette maps so
their layout cannot drift.

## 6. Display and internal UI scaling

Display offers:

- Automatic;
- 100%;
- 125%;
- 150%;
- 175%; and
- 200%.

Automatic follows the effective scale supplied by the host display path,
including values such as 133%. Manual values alter guest logical UI density.
They do not lower the physical monitor mode.

The runtime contract is split into:

```text
physical_width
physical_height
host_surface_scale_120
guest_ui_scale_120
logical_width
logical_height
geometry_generation
```

Requirements:

- `physical_width` and `physical_height` equal the monitor viewport's actual
  host physical pixels;
- Sway produces a final dmabuf of exactly those dimensions;
- no low-resolution complete frame is stretched to the viewport;
- a scale change updates Sway output state, input conversion, shell layout,
  AT-SPI geometry, runtime diagnostics, and CUA metadata in one generation;
- stale screenshots and window geometry from the preceding generation are
  rejected;
- absolute CUA coordinates remain in native physical-output pixels;
- the screenshot dimensions always equal the physical output mode; and
- the Settings UI reports physical mode, logical mode, UI scale, and effective
  scale separately.

This requires replacing the current single `scale_120` behavior in
`wildbuzzard-output-sync`. It is not acceptable to issue an isolated
`swaymsg output scale` command from Settings while the gateway, input, and CUA
still use the old transform.

If an individual third-party application lacks the relevant fractional-scale
protocol support, diagnostics may identify that application limitation. The
Wild Buzzard final monitor surface itself must still remain native-resolution
and must never hide whole-output stretching.

### 6.1 Keyboard language, layout, and Compose

Keyboard Settings provides:

- a common language/layout chooser plus an editable XKB layout code for every
  installed XKB layout, including comma-separated layout groups;
- the XKB keyboard model, defaulting to `pc105`;
- optional comma-aligned variants;
- optional comma-separated XKB options for Compose and layout switching;
- the confirmed active Sway layout name; and
- an ordinary GTK editable for testing characters, Backspace, modifiers,
  Compose, and layout-specific symbols immediately.

The selected keymap is guest state, exactly as on a physical Linux desktop.
Raw evdev keycodes from the host window are forwarded unchanged and
interpreted once by the configured nested physical keyboard in stock Sway.
The pinned wlroots Wayland backend intentionally ignores the parent's keymap
payload, so Sway remains authoritative for guest symbols and shortcuts; no US
key-remapping table may be introduced in the host input path. That backend
does consume the parent keyboard's serialized modifier and layout-group
state, however. The native gateway must therefore compile the byte-identical
canonical map and change that state in the same transaction as Sway, or AltGr,
Compose modifiers, and `grp:*` switching would disagree with guest symbols.

Keymap compilation is distribution-stable, not dependent on whichever
`xkb-data` happens to be installed on the host or later installed by a guest
user. The OCI Sway builder resolves one `xkb-data` package from its immutable
Debian snapshot, normalizes it to regular files, and emits a canonical
file/hash manifest, exact package version, and copyright. That same payload is
used at `/opt/wildbuzzard/runtime/current/share/X11/xkb` in the protected guest
runtime and at `$APPDIR/usr/share/wildbuzzard/xkb` in the host application.
Both sides also load the byte-identical `libxkbcommon.so.0` copied from that
same pinned builder, rather than separately serializing `TEXT_V1` with the
build host's library. Packaging and artifact audits reject symlinks, special
files, hash drift, version drift, a missing notice, or any host/guest data or
library byte mismatch. Runtime keymap compilation is given these roots and
libraries explicitly and must not add default host or mutable guest paths as a
fallback.

The settings schema stores only the exact bounded XKB contract above. The
session-private output-sync service compiles each request and keeps a unique
owner-only `0600` disk snapshot solely as durable journal/restart evidence; it
does not describe that file mode as immutable and never passes that pathname
to Sway. Immediately before each Sway apply, output-sync opens the snapshot
once with `O_NOFOLLOW`, validates the opened regular file and canonical digest,
copies those exact bytes into an `MFD_ALLOW_SEALING` memfd, applies
`F_SEAL_WRITE`, `F_SEAL_GROW`, `F_SEAL_SHRINK`, and `F_SEAL_SEAL`, and retains
the descriptor for the complete active/prior lifetime. Stock Sway receives
only `/proc/<output-sync-pid>/fd/<fd>`, so replacing or chmodding the user
pathname cannot change the bytes Sway opens. The service requires exactly one
input named `wayland-keyboard-wildbuzzard-seat`, applies the sealed map through
fixed Sway IPC, and confirms Sway's inventory. Before native Prepare it durably
journals the token, phase, prior/requested RMLVO, canonical digests, and managed
snapshot paths in the private session runtime. Commit and Abort response loss
is reconciled through Status rather than guessed. The supervised service uses
that journal after a crash: it revalidates and reseals the snapshot, restores
the prior Sway map before aborting a prepared transaction, or starts a new
complete transaction back to still-authoritative persisted Settings when an
unacknowledged commit completed. Settings persists only after the full paired
Commit is confirmed. A failed persistence step requests a new paired
transaction back to the prior setting and must report truthfully if that
rollback fails.

The CUA daemon may remain running and a CUA session may remain open while a
human types. CUA does not grab the seat. Its virtual keyboard is persistent,
serializes each short synthetic transaction, and handles named keys, chords,
and Unicode text without a one-shot client. It tracks every down event, drains
releases in reverse order, and publishes a zero modifier mask on success,
error, unwind, session end, reconnect, and shutdown. Cancellation also
unconditionally restores the fixed keymap and completes a bounded sync on that
same Wayland client. If this barrier cannot be proven, session teardown
fail-stops; a new-client sync is never accepted as proof. If a roundtrip fails after
a down event, it first retries cleanup on the same keyboard. If the connection
is dead, closing it makes pinned wlroots emit releases for its compositor-side
pressed set on that same device before Sway removes it; only then does CUA
reconnect and publish a zero modifier state. It never replays a press on a
replacement keyboard, because that could execute a binding or insert text.
SDK/runtime shutdown explicitly resets the process-global keyboard owner but
keeps it reusable by later SDK instances; process exit and abrupt death use the
same compositor-side destruction path. Human and CUA events
sent at the exact same moment may interleave like two physical keyboards on
one seat, but an idle CUA must never block or alter human input.

## 7. Sound

Sound controls the guest's private PipeWire graph through
`libpulse-binding` and `pipewire-pulse`. It does not parse human-formatted
`wpctl` output.

Expose:

- default output device;
- output volume and mute;
- default input device;
- input volume and mute;
- available sinks and sources;
- active playback and recording streams; and
- speaker-test and microphone-level feedback that run only when requested.

WirePlumber owns persisted routing and levels. Settings reflects server-side
changes made by applications while the page is open.

Host media permissions remain host-owned:

- Settings cannot enable host speakers, microphone, or camera;
- it clearly shows when the corresponding host bridge is Disabled,
  Unavailable, or Connected;
- opening Sound does not activate a microphone;
- a level meter or test activates capture only while visible/running and
  releases it immediately afterward; and
- device use must continue to trigger the host's normal PipeWire/portal
  privacy indication.

Camera permission remains in the host Devices control and is not duplicated
as a guest Settings page in this milestone.

### 7.1 Host port-sharing behavior

The existing host `Ports` control remains the owner of port forwarding. Every
new rule is usable without researching namespace addresses:

- the host address is prepopulated as `127.0.0.1`;
- the guest address is prepopulated from the active machine network and is
  refreshed automatically when that runtime address changes;
- both directions show plain-language endpoint labels in addition to the
  technical Host → Guest or Guest → Host direction; and
- advanced address fields remain editable, but never start blank.

For exposing a guest service through the host, `127.0.0.1` remains the safe
default. The user may explicitly change the host listener to `0.0.0.0` to
listen on every host IPv4 interface. That rule must then be reachable from a
different machine on the local network when host routing and firewall policy
allow it. The UI warns that `0.0.0.0` can expose the service on every reachable
host interface, not only a trusted LAN. It never widens a listener
automatically.

TCP and UDP rules apply live. Bind conflicts, unavailable guest addresses,
firewall/routing failures, and disabled listeners report their real state.
Acceptance tests cover host-loopback access, a separate LAN client,
host-to-guest and guest-to-host traffic, automatic guest-address resolution,
rule disable/re-enable, and Stop/Start persistence.

## 8. Application discovery

The shell continues discovering visible FreeDesktop applications from:

```text
~/.local/share/applications
/usr/local/share/applications
/usr/share/applications
```

An entry appears once when it has `Type=Application`, a valid name and
executable action, and is not hidden by `Hidden`, `NoDisplay`, desktop
visibility, or helper/service classification. A command-line-only Debian
package does not appear.

Filesystem changes are observed without rebuilding the image or restarting
the machine. The implementation should use file monitoring with a bounded
debounce rather than permanent rapid polling. Rescans produce one atomic
application model shared by visual menu rows and AT-SPI.

## 9. Link-in-place AppImage registration

### 9.1 Entry points

For a genuine Type-2 AppImage, shipped Thunar exposes secondary-click actions:

- Add to Applications;
- Add Desktop Shortcut;
- Remove from Applications, when registered; and
- Remove Desktop Shortcut, when present.

Thunar integration uses its supported custom-action mechanism. The underlying
helper is file-manager-independent so other file managers may integrate later,
but Wild Buzzard does not claim to inject menus into every third-party file
manager.

Thunar's pattern/range filter is only a fast context-menu prefilter:
`*.AppImage;*.appimage`, one selected regular-file candidate. The fixed helper
still opens the single `%f` argument without following a final symlink and is
the authority that accepts only a genuine x86-64 Type-2 AppImage. The UCA
command contains no user-controlled shell text, `sh -c`, command substitution,
or multi-file field code.

Because Thunar resolves one `Thunar/uca.xml` through XDG instead of merging
the system and user files, guest login performs an idempotent user-owned merge.
It replaces only actions carrying Wild Buzzard's fixed unique IDs and preserves
every other user action and byte. The resulting file is bounded, parsed before
modification, written atomically with mode `0600`, and never follows a symlink.
A malformed, oversized, or unsafe existing file is preserved unchanged and
produces an actionable guest-session diagnostic without blocking desktop boot.

Settings > Applications & Desktop lists all AppImage registrations and offers
Launch, Relink, Add/Remove Desktop Shortcut, Remove from Applications, and
Reveal Target.

### 9.2 Registration record

Each registration has a stable random identifier and stores:

- the guest-visible target path;
- sanitized display name;
- sanitized icon metadata;
- last-observed file identity and size for diagnostics;
- whether an Applications launcher exists;
- whether a desktop shortcut exists;
- creation and last-successful-launch times; and
- schema version.

The original AppImage is never copied into the registration directory.
Replacing a valid AppImage at the same path is allowed. The target is
revalidated on every launch; a stored checksum is evidence, not a permanent
pin that prevents normal application updates.

Generated `.desktop` files never embed the untrusted pathname in a shell
command. Their `Exec=` invokes the fixed helper with the opaque registration
ID. There is no `sh -c`.

### 9.3 Safe inspection

Registration:

1. opens the path without following a final symlink unless the user explicitly
   selected that resolved regular file;
2. verifies a regular x86-64 Type-2 AppImage and bounded ELF/AppImage metadata;
3. never executes the AppImage to obtain its name, icon, or desktop file;
4. extracts only bounded metadata through a sandboxed read-only parser;
5. rejects absolute archive paths, `..`, device nodes, oversized fields,
   malformed images, and decompression bombs;
6. sanitizes FreeDesktop field codes and icons;
7. falls back to a built-in AppImage icon when metadata is absent; and
8. writes the registration, icon, and launchers atomically and idempotently.

The existing narrow AppImage execution authorization is reused. It may add
only the owner's execute bit to a validated Type-2 AppImage after an explicit
registration/launch action. If the target filesystem forbids that change,
Wild Buzzard reports the problem and does not silently copy the file, grant
`CAP_SYS_ADMIN`, or loosen the guest sandbox.

### 9.4 Missing-target relink flow

Activation through the Applications menu, desktop, Settings, AT-SPI, or CUA
always enters the same launch helper.

When the stored target is missing or no longer a valid regular Type-2
AppImage:

1. show a native GTK4 file chooser inside the guest;
2. explain which registered application is missing and show its last path;
3. filter for candidate AppImages without hiding an explicit All Files
   fallback;
4. validate the selected file before changing state;
5. warn and request confirmation if its application identity materially
   differs from the registered application;
6. atomically replace the target path in the registration;
7. refresh extracted metadata and icons;
8. keep the opaque ID, Applications launcher, and desktop shortcut stable; and
9. launch only after the new record is committed successfully.

Cancel returns a structured `target_missing` result and changes nothing.
Selecting another missing, unreadable, incompatible, or malformed file keeps
the chooser open with a precise error.

Paths with spaces, quotes, percent signs, newlines, non-ASCII characters, and
long components are supported without shell interpretation.

## 10. Applications-menu desktop actions

Secondary-clicking an Applications-menu row opens a dedicated context surface
at the current guest pointer coordinate, clamped to the output. It contains:

- Open;
- Add Desktop Shortcut, when absent; or
- Remove Desktop Shortcut, when present.

The context surface is not implemented by moving or reusing the main
Applications-menu surface. It has its own input region and lifecycle so it
does not flicker, leave a stale grey menu, rearrange task buttons, or capture
clicks after dismissal.

Adding a shortcut writes a valid launcher into the XDG Desktop directory.
Removing it removes only that launcher. The action is available directly
through AT-SPI, so agents do not need to navigate visual menu pages.

## 11. Desktop contents and layout

Resolve `XDG_DESKTOP_DIR` and fall back to `~/Desktop`. Display:

- valid `.desktop` launchers;
- regular files;
- folders;
- symbolic links with an explicit link emblem;
- Type-2 AppImages; and
- the required built-in Files and Shared shortcuts.

Files and folders open through a fixed, argument-safe helper and `GFile`/GIO,
not a shell command. AppImages use the registration/launch helper.

Icons:

- use FreeDesktop theme icons or file thumbnails where safe;
- auto-flow within the usable desktop excluding the taskbar;
- reflow after monitor resize or UI-scale change;
- support pointer drag to reposition on the grid;
- persist positions by stable file identity where possible;
- page or scroll when the usable output cannot show all icons; and
- remain fully enumerable and invokable in AT-SPI even when off the current
  visual page.

The desktop background remains free of Sway's Terminal/Reconfigure/Exit menu.
Wild Buzzard's own desktop context menu may contain only desktop file actions
defined here.

## 12. Desktop file operations

### 12.1 Context menus and shortcuts

Normal click selects one item. `Ctrl+click` toggles items, `Shift+click`
extends a range in visual order, and pointer-drag on empty space creates a
rubber-band selection. Cut, Copy, and Delete operate on the complete
selection. Rename is enabled only for one selected item. Visual selection and
the AT-SPI selected state must always agree.

Secondary-clicking a desktop item opens at the pointer:

- Open;
- Cut;
- Copy;
- Rename;
- Delete;
- Add to Applications, for an unregistered valid AppImage; and
- Remove from Applications, for a registered AppImage.

Secondary-clicking empty desktop space exposes:

- Paste, enabled only when the private guest clipboard has supported content;
- New Folder; and
- Arrange Icons.

Keyboard equivalents are supported:

- `Ctrl+C` Copy;
- `Ctrl+X` Cut;
- `Ctrl+V` Paste;
- `F2` Rename;
- `Delete` confirmed Delete; and
- `Escape` dismiss menu/dialog.

These operations use the guest Wayland clipboard only. Host clipboard access
remains isolated.

### 12.2 Copy and cut semantics

Copy and Cut place a typed guest-local URI list and an internal operation
record on the private guest clipboard.

- Copy duplicates files, directories, or symbolic links without following a
  symbolic link into its target.
- Cut moves only after the destination write succeeds.
- A failed paste leaves the cut source intact.
- Cross-filesystem moves use copy, durability checks, then confirmed source
  removal.
- Name collisions open Replace, Keep Both, and Cancel choices.
- Permission, space, unsupported type, and partial-copy errors identify the
  affected path and never report success for incomplete work.
- Clipboard state survives a shell redraw but need not survive logout.

### 12.3 Rename semantics

Rename uses an inline editor or native dialog with the current basename
selected appropriately. It rejects empty names, `.`, `..`, path separators,
NUL, and collisions unless the user explicitly resolves the conflict.
Renaming a registered AppImage through Wild Buzzard updates its registration
as one crash-recoverable transaction; an external rename is handled by the
normal missing-target relink flow.

The Wild Buzzard rename path is one shortcut-helper transaction, not a shell
file rename followed by a best-effort registration rollback. It holds a
private inter-process lock shared by registration reads and mutations,
durably writes one bounded strict journal, verifies the source by
device/inode/size through the already-open XDG Desktop descriptor,
uses a same-directory no-replace rename, fsyncs the Desktop directory, changes
only the stable registration's target path, and then removes and fsyncs the
journal. Launchers, icon projections, registration ID, and the AppImage bytes
remain unchanged. Every RegistrationStore startup recovers an interrupted
transaction forward from the observed inode location and registration target;
both/neither locations, a replaced inode, a symlink, a removed registration,
or an unrelated target path is ambiguity and blocks mutation without deleting
either file. The Desktop rename itself is necessarily one-filesystem and
atomic. XDG Desktop and XDG data/state may be different filesystems, so no
false cross-filesystem atomic-commit claim is made: ordered fsync plus the
durable journal provides deterministic power-loss recovery to one committed
state.

### 12.4 Delete modal

The modal uses destructive styling for Delete and safe default focus on
Cancel. It states one of:

- “This removes only the shortcut. The target will not be deleted.”
- “This permanently deletes ‘name’.”
- “This permanently deletes ‘folder’ and everything inside it.”
- “This removes the link only. Its target will not be deleted.”

The dialog exposes its item count and consequence through AT-SPI. Enter must
not activate Delete unless the user or agent has explicitly focused it.
Closing the dialog is equivalent to Cancel.

Deletion uses descriptor-relative operations rooted at the resolved XDG
Desktop directory, rejects traversal and mount-boundary surprises, and does
not follow symlinks. Failures leave the model synchronized with the actual
filesystem and display a concrete error.

### 12.5 Host-authorized clipboard snapshots

The native host header contains a direct `Clipboard` menu alongside
`Machine`, `Ports`, `Devices`, and `Settings`. It exposes exactly:

- `Send Host Clipboard to Guest`; and
- `Copy Guest Clipboard to Host`.

This is not clipboard sharing in the Wayland data-device sense. The guest never
binds or proxies the host data-device manager, never receives a host clipboard
handle, and cannot subscribe to or trigger a host clipboard read. The two
ordinary clipboards remain independent before and after every transaction.

For host-to-guest, activation of the native host action is the authorization
event. Only then does the GTK host application lazily read one supported value
from its clipboard. It copies that value into bounded process RAM, validates
and canonicalizes it, sends only the resulting bytes in one typed transaction,
and best-effort clears the transport buffer. The in-guest agent stores the
value and becomes the owner of a normal private Sway clipboard selection so it
can continue serving guest paste requests after the transport closes. It has
no continuing reference or route to the host source.

For guest-to-host, the native action creates a cryptographically unpredictable
single-use nonce and a short deadline. The host sends one snapshot request to
the fixed in-guest agent and accepts at most one matching response. It ignores
unsolicited, replayed, wrong-direction, wrong-nonce, concurrent, and late
messages. A guest response never causes a host clipboard read; after host-side
validation succeeds, the host itself replaces its clipboard with the response
bytes.

The version-1 allowlist is deliberately small:

- valid UTF-8 text without embedded NUL characters, canonical MIME
  `text/plain;charset=utf-8`, maximum 8 MiB;
  and
- an ordinary still image offered through the native clipboard as PNG, JPEG,
  WebP, BMP, TIFF, or an equivalent toolkit texture that serializes to one of
  those formats. It is decoded under limits and canonicalized entirely in RAM
  to `image/png` for transport. The canonical encoded size is at most 64 MiB,
  width or height at most 8192 pixels, and decoded area at most 64 megapixels.

`text/plain` is accepted as an input alias. PNG is only the private canonical
representation; a native screenshot or copied image does not need to originate
as PNG. HTML, RTF, SVG, animated images, URI/file lists, serialized arbitrary
objects, executable formats, paths, and all unclassified MIME types are
rejected. Text and image sources are read only after the host click. Source
image structure, decoded geometry, and canonical PNG are checked before a
guest-provided image is installed into the host clipboard. All reads, writes,
decodes, conversions, and ownership handoffs have bounded memory and
deadlines.

The control plane is a fixed, versioned, length-delimited protocol. Messages
contain only version, direction, request nonce, canonical MIME, byte length,
and payload. Descriptors are `CLOEXEC`; message length is checked before
allocation; transfers are serialized per machine. There are no arbitrary
commands, paths, mounts, environment edits, network listeners, temporary
files, or generic RPC calls. The endpoint is a guest-owned Unix listener at a
fixed per-machine runtime path; there is no host listener for the guest to
call. The host never services a guest-initiated host-read request, and it reads
a guest response only while its own matching action is pending. A replaced or
compromised guest agent can receive a host snapshot only after the explicit
host-to-guest click, or offer hostile bytes only after the explicit
guest-to-host click; it gains no independent host clipboard capability.

Clipboard content and content hashes are never persisted, added to crash
reports, or logged. Diagnostics may record only time, machine, direction,
canonical MIME, bounded byte count, result, and a content-free error category.
Buffers are best-effort zeroed and dropped on success, failure, timeout,
machine stop, or agent disconnect. Each machine has an independent endpoint,
nonce space, and pending transaction. Actions are disabled unless the machine
is Running and its guest clipboard agent has completed readiness.

## 13. Guest package updates

### 13.1 Prerequisite: protect the pinned desktop runtime

The current image places pinned Sway/wlroots payloads in dpkg-owned `/usr`
locations while dpkg believes Debian packages own those files. A normal
`apt upgrade` could overwrite them. The Update Now UI must remain disabled
until this is fixed.

Move Wild Buzzard's pinned compositor runtime and managed integration
executables into a versioned private prefix:

```text
/opt/wildbuzzard/runtime/<asset-revision>/
/opt/wildbuzzard/runtime/current -> <asset-revision>
```

Use explicit service paths and embedded runtime-library search paths so apt
does not own or replace these files. The launcher installs a new revision
atomically during its existing managed-asset migration and retains the
previous complete revision until the new session passes readiness. Guest apt
does not modify this prefix.

This is managed-asset fallback, not an assertion that arbitrary apt
transactions are reversible.

### 13.2 Checking

A fixed systemd timer and an explicit Check Now action call the root updater.
The checker uses Debian's structured package API, refreshes configured
repositories, and writes:

- check time;
- repository errors;
- exact installed and candidate versions;
- download size;
- update count;
- security origin when reliably known; and
- an opaque plan generation.

States are:

```text
never_checked
checking
up_to_date
available
installing
failed
restart_recommended
```

The shell shows a cinnamon badge on Settings when updates are available. Its
accessible label includes the count, for example “Settings, 14 updates
available.” Mako emits one guest-local notification for each new plan
generation and records the last-notified generation.

### 13.3 Installing

Update Now:

1. shows the exact packages, current/candidate versions, and download size;
2. requires confirmation;
3. sends only the opaque plan generation to the root service;
4. refuses stale plans whose candidates changed;
5. streams structured progress to Settings;
6. serializes against apt/dpkg and reports lock ownership or timeout;
7. retains apt/dpkg logs and the attempted plan;
8. never reboots or powers off automatically; and
9. reports when logout or restart is recommended.

Allowed system-bus methods are conceptually:

```text
Check()
GetState()
InstallPlan(generation)
RetryRepair(generation)
CancelDownload(generation)
```

No method accepts a package name, repository URL, command, arbitrary path, or
apt argument. Repository management and arbitrary package installation remain
normal explicit guest administration tasks.

There is no truthful transactional rollback for apt on the flat mutable
rootfs. On failure the UI preserves evidence, explains the actual dpkg state,
and offers Retry/Repair where safe. It never claims that rollback occurred.
Machine snapshot/backup is a separate future host feature.

## 14. Branding and Buzzard logo

### 14.1 Species and silhouette

The logo represents a European common buzzard (`Buteo buteo`), not a generic
eagle, falcon, kite, vulture, or owl. Authoritative visual references include
the [RSPB Common buzzard guide](https://www.rspb.org.uk/birds-and-wildlife/buzzard),
the [RSPB bird-of-prey identification guide](https://www.rspb.org.uk/birds-and-wildlife/identifying-birds/whats-that-bird-of-prey),
and the [BTO Buzzard profile](https://www.bto.org/learn/about-birds/birdfacts/buzzard).

The previous generated flying/underside and direct-staring concepts are
rejected and must not be used as production artwork. The new direction is an
original **near-front three-quarter common buzzard**: a balanced portrait
showing the head, upper chest, and folded-wing shoulders. The head and gaze
turn slightly off-axis so the bird remains recognizable and alert without
making direct eye contact with the viewer. It must read as a whole bird
portrait rather than a detached mascot eye or abstract wing symbol.

Preserve common-buzzard traits visible from the near-front angle: a compact
broad head, short neck, substantial chest, modest hooked beak, natural raptor
eyes, and rounded folded shoulders. Avoid direct-staring mascot expressions,
perfectly mirrored eyes, eagle heraldry, a giant eagle beak, a bald vulture
head, an owl facial disc or oversized owl eyes, a falcon helmet shape, shields,
letters, and a central cyclops-eye motif.

### 14.2 Vector construction

The production logo is a hand-audited original SVG, not a traced reference
photograph and not a raw generated raster:

- 256 by 256 master artboard;
- centered near-front three-quarter portrait with safe space on every side;
- recognizable head, chest, and folded-wing shoulder silhouette before
  plumage details are added;
- slight natural asymmetry is allowed, but the optical weight remains
  balanced;
- two to four filled paths;
- no filters, blur, gradients, photographic texture, or scale-dependent
  strokes; and
- a separately simplified symbolic variant that remains recognizable at
  16–24 pixels.

Generate several genuinely different near-front three-quarter concepts for comparison,
then manually reconstruct the selected direction as vector geometry.
Generated concepts may guide anatomy and composition only. They are not
production assets until manually reconstructed, simplified, visually audited,
and checked for originality and licensing. None of the previously generated
concept images is an approved candidate.

### 14.3 Similarity and trademark screening

Every shortlisted vector candidate and the final mark must be checked for
confusing similarity before it is accepted:

1. render clean color and monochrome images at high resolution;
2. run each through Google Lens/Google Images reverse-image search and at
   least one independent reverse/similarity search service;
3. perform ordinary image searches for front-facing buzzard, hawk, eagle,
   raptor, Linux, software, security, AI, and technology logos;
4. search relevant public trademark/logo databases in intended publication
   territories;
5. record the date, candidate hash, services, queries, closest results, URLs,
   and a written comparison in a committed clearance report without copying
   third-party artwork into the repository; and
6. reject or substantially redesign a candidate whose overall silhouette,
   face/beak construction, negative space, eye treatment, color lockup, or
   composition is materially close to an existing organization or product.

Repeat the searches after every material redesign and once more immediately
before publication. Reverse-image and database searches reduce risk but do
not guarantee legal trademark clearance; obtain professional trademark review
before treating the public brand as legally cleared.

### 14.4 Palette and placement

```text
Dark icon background     #181818
Dark main mark           #82766D
Dark secondary mark      #F3ECE4
Dark detail              #24272A
Dark Cinnamon accent     #D9683A
Dark unboxed main        #93867C
Light icon background    #F4F1EC
Light main mark          #71665F
Light secondary mark     #FFFDFC
Light detail             #24272A
Light Cinnamon accent    #C9572D
Symbolic                 currentColor
Dark wallpaper           #202225
Light wallpaper          #F4F1EC
```

The neutral brown-grey main mass is intentionally subordinate to the
graphite, off-white, and Cinnamon-orange system. Cinnamon is a restrained
cere/feather accent rather than a full orange bird silhouette. Dark and light
variants change palette only; their audited geometry is identical.

The four built-in wallpaper presets are deterministic: Dark Plain is solid
`#202225`, Dark + Logo uses that same solid with the dark-theme mark, Light
Plain is solid `#F4F1EC`, and Light + Logo uses that same solid with the
light-theme mark. A custom background is one user-selected solid colour.

For logo presets, the wallpaper is generated or rendered from the SVG at the
guest output's current physical resolution and centers the mark at 18–22% of
the output's shorter dimension. It never stretches a fixed-resolution bitmap.
Plain presets render only the selected solid colour. Idle wallpaper is static;
an optional boot animation may use the same paths without changing the logo
geometry or remaining active after desktop readiness.

Use one reviewed source of geometry for the host icon, guest Applications
icon, Settings/About, symbolic icons, and wallpaper variants.

## 15. Accessibility and CUA contract

Every new function is usable by a human and by the installed in-guest agent.

- Settings publishes normal GTK4 AT-SPI roles, names, descriptions, values,
  selection, checked state, progress, and dialogs.
- The shell publishes every desktop item and every item action, including
  off-page items.
- Applications-menu context actions are directly invokable without visually
  scrolling to the row.
- File-operation confirmations expose consequence, item count, target, and
  Delete/Cancel actions.
- The missing-AppImage chooser is a guest window included in screenshots and
  normal window enumeration.
- CUA success requires an observable post-action filesystem, registration,
  settings, audio, window, or update-state change.
- Host chrome never appears in screenshots or coordinates.
- All interactions continue while the host machine window is covered,
  unfocused, on another workspace, or minimized.

## 16. Security boundaries

- Settings runs unprivileged.
- The updater is the only new privileged guest component and exposes fixed
  operations over the private guest system bus.
- AppImage metadata is parsed without executing the AppImage.
- Launcher paths are looked up by opaque IDs; no shell interpolation is used.
- Desktop file operations are descriptor-relative, reject traversal, and do
  not follow symlinks for destructive actions.
- Desktop file Cut/Copy/Paste and all in-guest CUA clipboard operations remain
  on the private guest Wayland session. The separate host-header clipboard
  actions transfer only one validated byte snapshot under Section 12.5; they
  expose no host clipboard object or guest-triggerable host read.
- Sound controls cannot grant host media permissions.
- No host D-Bus, host home, host clipboard service/data-device, SSH server,
  VNC/RDP, or guest control port is introduced. The fixed clipboard endpoint
  is not a host clipboard socket and supports no operation beyond its typed,
  host-authorized snapshot protocol.
- Registered files in `/shared` remain host-visible user files; registration
  grants no access beyond the existing explicit `/shared` mount.

## 17. Packaging and migration

The reference OCI adds only the runtime dependencies needed by these features:

- GTK4 and the exact `gtk4-rs` runtime closure;
- `pipewire-pulse` client support used by Settings;
- the structured Debian package API used by the updater;
- bounded AppImage metadata inspection support;
- the fixed guest clipboard agent and its private-session Wayland client
  closure;
- the Wild Buzzard settings, helper, updater, themes, and logo assets; and
- their complete license and provenance records.

No compiler toolchain or source tree remains in the rootfs. Build dependencies
stay in disposable builder stages.

Guest assets are added to `guest/asset-manifest.tsv` and installed identically
by OCI construction and persistent-rootfs migration. Migration:

- preserves user settings, registrations, desktop files, package state, and
  arbitrary user data;
- adds new defaults only when no user choice exists;
- converts older schemas atomically;
- moves the pinned desktop runtime out of dpkg-owned paths before enabling
  Update Now; and
- can resume safely after interruption.

## 18. Implementation workstreams

The root agent acts as orchestrator. Independent agents implement and review
bounded workstreams; the orchestrator owns contract integration, conflicts,
end-to-end testing, and final evidence.

### Phase 0: preserve and verify the baseline

- Finish the current native viewport-size acceptance defect.
- Require an exact native physical framebuffer and aligned input before
  layering new Settings work onto the display path.
- Keep GitHub builds artifact-only: no Release, package, OCI push, tag, or
  publication.

### Phase 1: shared core and schemas

- Add `desktop-core`.
- Define versioned Settings and AppImage registration schemas.
- Implement atomic persistence, XDG discovery, desktop-entry validation, and
  typed theme/display/update state.
- Add hostile-path and schema-migration tests.

### Phase 2: branding and theming

- Create multiple calm near-front three-quarter common-buzzard concepts,
  reject the previous flying and direct-staring concepts, and construct the
  selected design as an original SVG.
- Complete and document reverse-image, visual-similarity, and trademark
  database screening before accepting the mark.
- Produce dark, light, symbolic, icon, and wallpaper variants from one
  geometry source.
- Keep Dark and Light geometry identical through one shared layout stylesheet;
  change only conservative palette tokens.
- Implement runtime shell palette reload and toolkit/application propagation.
- Remove unconditional dark-mode startup behavior.

### Phase 3: native Settings

- Build the adaptive GTK4 Settings application.
- Implement Appearance, Display, Keyboard, Sound, Applications & Desktop,
  Updates, and About pages.
- Add the FreeDesktop launcher, icon, single-instance activation, and complete
  AT-SPI labels.

### Phase 4: desktop and AppImage integration

- Add XDG Desktop discovery and icon layout to the shell.
- Implement link-in-place registration, safe metadata extraction, launch,
  missing-target relink, and removal.
- Add Thunar custom actions and Applications-menu context actions.
- Implement Cut, Copy, Paste, Rename, and confirmed Delete.

### Phase 5: display scale and audio

- Split host surface scale from guest UI scale throughout the gateway,
  output-sync, runtime, shell, input, and CUA.
- Implement private PipeWire sound controls and host-permission status.
- Verify exact physical screenshots and coordinate alignment at every scale.
- Implement the two native host-header clipboard actions, typed per-machine
  transport, fixed Sway clipboard agent, content validation, cancellation,
  readiness, and isolation diagnostics specified in Section 12.5.

### Phase 6: safe updates

- Move pinned Sway/wlroots and managed runtime assets into the private prefix.
- Implement the fixed-operation updater, timer, plan generation, progress,
  badge, notification, logs, and recovery states.
- Enable Update Now only after the pinned-runtime protection gate passes.

### Phase 7: independent acceptance and artifacts

- Run unit, integration, schema, security, migration, and packaging tests.
- Rebuild the OCI, AppImage, and complete portable archive.
- Download the GitHub Actions artifacts and test those exact bytes.
- Drive the complete journey through the installed guest CUA/AT-SPI interface.
- Inspect physical-resolution screenshots and structured evidence.
- Do not publish packages, OCI images, GitHub Releases, or tags.

No phase is complete merely because it compiles. Agents must return concrete
test evidence and must not silently weaken this contract to make tests pass.

## 19. Acceptance matrix

### 19.1 Settings and persistence

- Open Settings from Applications, desktop shortcut, AT-SPI, and CUA.
- Navigate every control by mouse and keyboard at normal and small outputs.
- Stop and start the machine; verify every setting persists.
- Feed invalid and newer-schema configuration; verify preservation and an
  actionable diagnostic.

### 19.2 Appearance and branding

- Switch Light to Dark and Dark to Light.
- Exercise Dark Plain, Dark + Logo, Light Plain, Light + Logo, and Custom Solid
  Colour; verify exact persistence across Stop/Start and native-resolution
  regeneration after output resize.
- Verify shell, wallpaper, Settings, Thunar, Mousepad, Foot, GTK4, Qt, and a
  representative Electron application.
- Verify selected, hover, focused, disabled, warning, and destructive states
  in both themes.
- Compare widget allocations and shell surface geometry between Dark and
  Light; require them to be identical apart from color values.
- Inspect the logo at 16, 24, 32, 64, 256 pixels and on multiple wallpaper
  aspect ratios. Require a calm near-front three-quarter common-buzzard
  portrait with an off-axis gaze and reject direct-staring mascot treatment,
  blur, gradients, wrong bird anatomy, and stretched raster output.
- Review the recorded reverse-image, similarity, and trademark searches for
  every shortlisted candidate and the final asset.

### 19.3 Scale

- Exercise Automatic, 100, 125, 150, 175, and 200% guest UI scale across host
  scale 100, 125, 133, 150, 175, and 200%.
- Resize and move the host window between mixed-scale monitors.
- Require exact physical screenshot dimensions, correct logical dimensions,
  unchanged dmabuf fast-path truth, sharp text, and aligned CUA clicks.
- Reject stale geometry immediately after each scale generation.

### 19.3.1 Keyboard and CUA coexistence

- Apply and persist at least `us`, `gb`, `de`, and `us`/`intl` with a Compose
  option; stop/start and verify the same active Sway layout returns.
- Reject malformed, oversized, command-shaped, missing, or unsupported XKB
  component combinations without replacing the last confirmed setting.
- In Settings, Mousepad, Foot, Firefox, a GTK application, a Qt application,
  an Electron application, and an Xwayland application, test ordinary text,
  shifted characters, layout-specific symbols, Backspace, Delete, arrows,
  Enter, Tab, shortcuts, Caps Lock, Compose, and configured layout switching.
- Keep CUA running. Execute Ctrl+L, text, Enter, an interrupted modifier
  action, and a cancelled session; after each case require a neutral CUA
  modifier ledger and immediately type and Backspace through the human host
  input route without restarting Sway or the application.
- In the hardware journey, issue host-origin Backspace/text/Enter immediately
  after CUA `type_text` and before the next CUA key while the same CUA session
  remains open. The input source must traverse the host compositor, native GTK
  monitor, display gateway, and nested parent keyboard. GNOME acceptance uses
  a bounded Mutter RemoteDesktop session that never enables clipboard and is
  stopped on every exit. Other compositors require a declared harness hook or
  an explicit interactive human step; guest `wtype` is not valid evidence.
- Hold the Settings input-test field open while alternating CUA and human
  actions. An idle CUA must never suppress a human key. Exact simultaneous
  actions may interleave but must return to a neutral seat afterward.
- Kill output-sync after journal creation, after host Prepare, after Sway
  apply, after Commit is sent, and after a Commit response is lost. Require
  bounded supervised restart, strict Status reconciliation, prior-Sway-before-
  Abort ordering, no permanently queued physical keys, and persisted Settings
  as the final active layout.
- Exercise AltGr/level-three symbols and `grp:*` group switching through the
  physical host-window input path, proving the native and Sway canonical
  keymap digests and modifier/group state stay paired.

### 19.4 Sound

- Enumerate real guest sinks, sources, and streams.
- Change default devices, volume, and mute and observe the server-side state.
- Test with host speaker/microphone permission off and on.
- Verify opening Settings does not activate the microphone.
- Verify level tests release devices and host privacy indication remains
  truthful.

### 19.4.1 Port sharing

- Create both forwarding directions and verify the host and guest address
  fields are already populated with correct runtime values.
- Expose a guest TCP and UDP service on host `127.0.0.1` and verify it remains
  unreachable from a separate LAN client.
- Explicitly change the host bind to `0.0.0.0` and verify the same service is
  reachable from a separate LAN client when the test firewall permits it.
- Disable the rule and verify both loopback and LAN listeners close
  immediately; re-enable it and verify recovery without restarting PID 1.
- Stop/Start the machine and verify the rule persists while the guest address
  is resolved again rather than relying on a stale hardcoded value.

### 19.4.2 Explicit clipboard transfer

- With no host clipboard action pending, repeatedly mutate host and guest
  clipboards and prove neither side observes, subscribes to, or changes the
  other. From a compromised-guest test process, prove no protocol operation can
  request the current host clipboard.
- Put multilingual text containing newlines, an em dash, accented Latin, CJK,
  and emoji on the host clipboard; activate `Send Host Clipboard to Guest`;
  paste/read it through a real guest GTK editable and CUA; require exact bytes.
  Change the host clipboard afterward and prove the guest selection remains
  the authorized snapshot.
- Repeat host-to-guest with a native screenshot clipboard object and separate
  PNG, JPEG, WebP, BMP, and TIFF sources; verify the canonical image's decoded
  dimensions/content in a real guest application. Reject unsupported MIME,
  invalid UTF-8, oversized text, oversized PNG, decompression-bomb geometry,
  malformed PNG, timeout, and source cancellation without changing the guest
  clipboard.
- Put supported text and native screenshot/JPEG/WebP/BMP/TIFF/PNG still-image
  values on the private guest clipboard and activate
  `Copy Guest Clipboard to Host`; require exactly one matching snapshot to
  replace the host clipboard. Prove unsolicited, replayed, wrong-nonce,
  wrong-direction, late, oversized, malformed, and concurrent guest responses
  do not modify the host clipboard.
- Prove the two actions are disabled outside Running/agent-ready, cancel safely
  during Stop/Restart/disconnect, preserve no content or hash in logs/state,
  and use bounded RAM with no temporary files.
- Run two machines simultaneously. Transfer different values in both
  directions and prove their channels, nonces, ownership, and diagnostics do
  not cross. Confirm host-header clicks never reach the guest input stream.

### 19.5 Debian applications and updates

- Install a real `.deb` with a visible `.desktop` entry and verify menu/AT-SPI
  appearance without restart.
- Install a command-line-only package and verify it remains absent from the
  menu.
- Use a deterministic local signed APT fixture to test no updates, available
  updates, badge, notification, stale plan, lock contention, download failure,
  interrupted install, repair, logs, and restart recommendation.
- Verify apt cannot overwrite the pinned Sway/wlroots runtime.
- Stop/Start and prove successful package updates persist.

### 19.6 AppImages

- Register a real official Type-2 AppImage from Thunar without copying it.
- Verify its original path, size, and digest remain unchanged.
- Add/remove Applications and desktop launchers repeatedly without duplicates.
- Launch through native FUSE before and after Stop/Start.
- Rename or move the original outside Wild Buzzard, activate the launcher,
  select the new path in the guest chooser, and verify atomic relink.
- Cancel relink and verify no mutation.
- Replace the file at the same path with a valid newer image and verify safe
  revalidation and metadata refresh.
- Reject fake ELF, fake AppImage, directory, device, final symlink, malformed
  metadata, archive traversal, oversized metadata, and incompatible
  architecture.
- Test spaces, quotes, percent signs, newlines, Unicode, and maximum supported
  path lengths.

### 19.7 Desktop operations

- Create folders and files, open them, drag/reposition icons, and survive
  resize and Stop/Start.
- Copy and cut/paste files, folders, and symlinks.
- Exercise same-filesystem and cross-filesystem moves between the guest home,
  Desktop, and `/shared`.
- Resolve Replace, Keep Both, and Cancel collisions.
- Rename ordinary items and registered AppImages.
- Verify Delete always opens the correct modal.
- Cancel every deletion case and prove nothing changed.
- Delete a shortcut and prove its target survives.
- Delete a symlink and prove its target survives.
- Delete a regular file and non-empty folder only after explicit confirmation.
- Repeat through direct AT-SPI actions and require observable filesystem
  evidence.

### 19.8 Hidden-window operation

Repeat Settings changes, AppImage relink, application launch, desktop file
operations, update checks, and screenshots while the native host window is
covered and minimized. Guest output and CUA must continue running without
claiming hidden frames were physically presented.

## 20. Completion gate

This feature set is complete only when:

- all phases above are implemented;
- the reference image contains no development-only applications or toolchain;
- licensing/provenance checks pass;
- automated tests pass;
- an independent review finds no boundary or destructive-operation defect;
- the real hardware/CUA journey passes with inspected screenshots;
- the final downloaded AppImage and portable archive pass the same journey;
  and
- the GitHub workflow remains artifact-only with no publication side effect.

Until then, the Settings and desktop integration work is in development and
must not be described as production-complete.
