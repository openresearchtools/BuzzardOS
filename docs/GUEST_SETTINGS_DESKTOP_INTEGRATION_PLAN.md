# Guest Settings and Desktop Integration Plan

- Status: approved implementation contract
- Applies to: Wild Buzzard guest desktop, reference rootfs, packaging,
  migration, CUA, and release acceptance
- Parent specification: `../AGENTS.md`

This document turns the agreed Settings, desktop, AppImage, update, sound,
display-scale, and branding decisions into one implementation plan. It is not
a speculative design note. Implementations and tests must satisfy this
contract. If this file and `AGENTS.md` ever disagree, the stricter requirement
applies and both files must be corrected in the same change.

## 1. Product outcome

The guest gains a small native Settings application and a normal persistent
desktop without turning the Rust shell into a general-purpose widget toolkit.
The finished experience must provide:

- a native, accessible Settings window;
- persistent light and dark themes;
- guest UI-scale controls that never reduce the physical output resolution;
- controls for the guest's private PipeWire audio graph;
- visible Debian package-update status and an explicit Update Now workflow;
- automatic discovery of applications installed through normal Debian
  packages;
- link-in-place registration of Type-2 AppImages;
- relinking through a file chooser when a registered AppImage moves;
- desktop shortcuts for installed applications and AppImages;
- normal desktop cut, copy, paste, rename, and confirmed delete operations;
- a distinctive common-buzzard logo and resolution-independent wallpaper; and
- full AT-SPI and in-guest CUA access to every control and desktop item.

The host launcher and the guest remain separate products at the trust
boundary. Guest Settings must never gain access to the host desktop, host
filesystem beyond explicit shares, host D-Bus, or host device controls.

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
3. Sound
4. Updates
5. Applications & Desktop
6. About

There is no Apply button for changes that can be safely committed
immediately. Changes requiring a session restart say so before confirmation.
Every failed change restores the last confirmed state and shows the actual
error.

## 5. Appearance and theming

### 5.1 User controls

Appearance provides:

- Dark;
- Light; and
- a preview containing text, controls, selection, folders, the taskbar, and
  the Buzzard mark.

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

Applications that do not support live recoloring may require reopening. The
UI states that limitation instead of claiming the application changed.

### 5.3 Theme implementation

Theme values are typed tokens in `desktop-core`, not independently copied
color literals. The shell reloads its palette and redraws surfaces on a
settings-generation change. Toolkit and application configuration is written
atomically, then the appropriate guest-only settings notifications are sent.

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
atomically; an external rename is handled by the normal missing-target relink
flow.

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

Use an original underside-view “Thermal Buzzard” in a shallow soaring V:

- stocky body and compressed neck;
- modest, broad head with little forward projection;
- exceptionally broad, rounded wings;
- five visible fingered primaries per wing at normal sizes; and
- a short, broad, slightly fanned, non-forked tail.

Avoid heraldic shields, bald heads, owl faces, central eye symbols, giant
beaks, long hawk tails, forked kite tails, and pointed falcon wings.

### 14.2 Vector construction

The production logo is a hand-audited original SVG, not a traced reference
photograph and not a raw generated raster:

- 256 by 256 master artboard;
- mark aspect ratio 2.1–2.35:1;
- at least 9% horizontal clear margin;
- body length 42–47% of wingspan;
- head width 8–10% of wingspan;
- head projection no more than 5% of wingspan;
- tail width 18–22% and length 13–16% of wingspan;
- wing tips 6–8% of wingspan above the shoulder line;
- two to four filled paths;
- no filters, blur, gradients, photographic texture, or scale-dependent
  strokes; and
- a simplified four-notch symbolic variant at 16–24 pixels.

Generated concepts may guide anatomy and composition only. They are not
production assets until manually reconstructed, simplified, visually audited,
and checked for originality and licensing.

### 14.3 Palette and placement

```text
Dark icon background     #181818
Dark main mark           #FF7139
Dark secondary mark      #FFD0BF
Dark unboxed mark        #E6E6E6 + #FF7139
Light icon background    #F4F1EC
Light main mark          #24272A
Light accent             #BD4218
Symbolic                 currentColor
Dark wallpaper           #202225
```

The wallpaper is generated or rendered from the SVG at the guest output's
current physical resolution. It uses the existing plain background and
centers the mark at 18–22% of the output's shorter dimension. It never
stretches a fixed-resolution bitmap. Idle wallpaper is static; an optional
boot animation may use the same paths without changing the logo geometry or
remaining active after desktop readiness.

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
- Clipboard operations remain on the private guest Wayland session.
- Sound controls cannot grant host media permissions.
- No host socket, host D-Bus, host home, host clipboard, SSH server, VNC/RDP,
  or guest control port is introduced.
- Registered files in `/shared` remain host-visible user files; registration
  grants no access beyond the existing explicit `/shared` mount.

## 17. Packaging and migration

The reference OCI adds only the runtime dependencies needed by these features:

- GTK4 and the exact `gtk4-rs` runtime closure;
- `pipewire-pulse` client support used by Settings;
- the structured Debian package API used by the updater;
- bounded AppImage metadata inspection support;
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

- Construct and independently audit the final buzzard SVG.
- Produce dark, light, symbolic, icon, and wallpaper variants from one
  geometry source.
- Implement runtime shell palette reload and toolkit/application propagation.
- Remove unconditional dark-mode startup behavior.

### Phase 3: native Settings

- Build the adaptive GTK4 Settings application.
- Implement Appearance, Display, Sound, Applications & Desktop, Updates, and
  About pages.
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
- Verify shell, wallpaper, Settings, Thunar, Mousepad, Foot, GTK4, Qt, and a
  representative Electron application.
- Verify selected, hover, focused, disabled, warning, and destructive states
  in both themes.
- Inspect the logo at 16, 24, 32, 64, 256 pixels and on multiple wallpaper
  aspect ratios. Reject blur, gradients, wrong bird anatomy, and stretched
  raster output.

### 19.3 Scale

- Exercise Automatic, 100, 125, 150, 175, and 200% guest UI scale across host
  scale 100, 125, 133, 150, 175, and 200%.
- Resize and move the host window between mixed-scale monitors.
- Require exact physical screenshot dimensions, correct logical dimensions,
  unchanged dmabuf fast-path truth, sharp text, and aligned CUA clicks.
- Reject stale geometry immediately after each scale generation.

### 19.4 Sound

- Enumerate real guest sinks, sources, and streams.
- Change default devices, volume, and mute and observe the server-side state.
- Test with host speaker/microphone permission off and on.
- Verify opening Settings does not activate the microphone.
- Verify level tests release devices and host privacy indication remains
  truthful.

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
