# Buzzard OS: authoritative product specification

This file is the source of truth for contributors and coding agents working in
this repository. Implementation, tests, documentation, packaging, and design
decisions must preserve it. When code and this document disagree, treat the
code as work still to be brought into conformance; do not reinterpret this
specification around the current implementation.

Detailed plans may refine a subsystem but may not contradict this file.
Changes to guest Settings, desktop-file operations, guest AppImage support,
updates, theming, sound, or scaling must also keep
`docs/GUEST_SETTINGS_DESKTOP_INTEGRATION_PLAN.md` consistent.

## Product invariants

Buzzard OS is a rootless, persistent Linux desktop-machine manager installed
on Debian-family hosts through APT as a normal versioned package. Its
human-facing name is `Buzzard OS`; new executable, application, D-Bus, theme,
package, diagnostic, and runtime identities use Buzzard naming. This is a new
application with no released legacy-data compatibility requirement.

Buzzard OS is not distributed as an AppImage or extracted portable folder.
There is no global Buzzard machine-storage directory. Each machine is created
in the exact directory selected by the user and is independently movable,
deletable, exportable, and re-registerable.

A machine is a persistent, directly writable, flat root filesystem:

- The rootfs is durable machine state, not a disposable container layer.
- Normal operation uses no overlay and does not run from an OCI archive.
- Guest package installations, files, configuration, and application state
  survive complete stop/start cycles.
- Guest systemd is namespace PID 1 and supervises a complete multi-process
  desktop session.
- Stock distro Sway/wlroots composites the guest into exactly one native host
  Buzzard OS window through one fixed host-facing guest output. Additional
  numbered CUA/manual outputs are guest-internal, active, off-screen virtual
  outputs and never become additional host surfaces.
- Guest applications are never forwarded as separate host surfaces.
- The nested compositor is the guest display, input, screenshot, and
  accessibility boundary.

## Repository and component boundaries

The target layout makes all four distributable components independently
understandable and licensed:

```text
host/                 buzzardos host manager, broker, and display
guest/                buzzardos-guest mechanics and integration runtime
desktop/              optional official buzzardos-desktop environment
cua/                  buzzardoscua source and upstream attribution
packaging/            reproducible Debian package assembly
oci/                  reference OCI assembly and one-time setup
tests/acceptance/     machine, session, hardware, and CUA journeys
tools/                local verification and licensing gates
LICENSES/             package and third-party licensing evidence
```

Move current sources toward these boundaries where practical. Do not keep
unrelated components together merely because they were historically in one
workspace. Host and guest workspaces remain independently locked; host source
does not enter the OCI build context. Guest outputs enter the reference image
only through built `.deb` files.

Each component has its own authoritative version file, changelog, Debian
metadata, and AGPL-3.0-or-later license evidence. Every package carries its
copyright/license material under normal Debian documentation paths.
Third-party code keeps its original license and notices; the Buzzard CUA fork
preserves upstream MIT attribution alongside the AGPL-3.0-or-later terms for
Buzzard-authored work. License inventory and provenance gates are mandatory.

Generated Cargo targets, packages, OCI layouts/archives, downloaded test
applications, logs, and screenshots live outside tracked source by default.

Production Buzzard components create no log files, retain no input/activity
telemetry, and create no automatic diagnostic screenshots. Background host and
guest services discard their standard output and error streams. Operational
JSON files are bounded, atomic current-state/IPC records required to run the
machine; they are not append-only history. Explicit CLI output and screenshots
requested by a CUA caller remain direct results of that invocation.

## Exactly four Debian packages

The build emits exactly four independently versioned packages:

```text
buzzardos_<version>_<arch>.deb
buzzardos-guest_<version>_<arch>.deb
buzzardos-desktop_<version>_<arch>.deb
buzzardoscua_<version>_<arch>.deb
```

### `buzzardos`

The host package contains the manager GUI/CLI, rootless broker, native display
and filtered Wayland gateway, application-menu/AppStream/icon metadata,
diagnostics, and required host helpers. It declares distro runtime dependencies
and must install on Ubuntu 24.04 LTS, Debian 13, and Ubuntu 26.04. It installs
no privileged Buzzard daemon, system service, or setuid binary. Upgrade or
uninstall never deletes or rewrites machines.

### `buzzardos-guest`

The guest mechanics package contains the desktop-independent runtime:

- guest systemd/session integration and readiness;
- a minimal usable stock-Sway session configuration;
- output, scale, and keyboard synchronization;
- private clipboard agent and typed endpoint;
- guest AppImage detection/runtime integration;
- guest-side audio, microphone, camera, and port integration agent;
- private D-Bus, AT-SPI, PipeWire/WirePlumber, portals, notifications, and
  runtime glue required by the integration boundary.

It depends on unmodified Sway, wlroots ABI, Wayland, `libxkbcommon`, `xkb-data`,
PipeWire, WirePlumber, GStreamer, and other packages from the guest distro's
configured APT repositories. Buzzard OS never fetches, forks, compiles, or
privately ships Sway or wlroots.

It works without the official desktop. The user still has a functional
stock-Sway machine and may install another desktop provider. Runtime readiness
must not require the official Buzzard shell or Settings application.

### `buzzardos-desktop`

The optional official desktop package depends on `buzzardos-guest` and owns
only the opinionated Buzzard desktop experience:

- Rust desktop shell, bottom taskbar, menu, and desktop icons;
- Settings UI that calls runtime APIs but does not own their mechanics;
- themes, icons, toolkit defaults, menus, application defaults, desktop files;
- Thunar presentation, actions, and integration;
- official desktop-provider readiness and UI assets.

Changing taskbar, Settings, icons, themes, Thunar, or other UI updates this
package without forcing a mechanics update. Another desktop package may depend
on `buzzardos-guest` and replace it.

### `buzzardoscua`

The separate CUA package contains one reviewed Rust CLI crate derived from the
Linux functionality formerly called the TryCua Cua Driver. Its product,
package, executable, and diagnostic identity is `Buzzard CUA`. It runs as the
interactive guest user and depends on compatible guest-runtime interfaces;
neither guest package is collapsed into it. It contains no CUA daemon, session
manager, MCP server, browser-specific automation layer, recording subsystem,
telemetry, vendor service, or start/end-session protocol.

The reference image installs `buzzardos-guest`, `buzzardos-desktop`, and
`buzzardoscua`, but their versions
and update cadence remain independent. Package metadata expresses explicit
protocol compatibility ranges rather than coincident version numbers.

## Package installation and updates

Packages deliver and own updateable files; they do not repeatedly provision a
machine. Package-owned binaries, libraries, units, vendor defaults, themes,
icons, and assets may be replaced by upgrade. User home configuration and
machine state may not be reset.

Maintainer scripts must not create UID/GID 1000, reseed `/home`, reset Sway or
PipeWire, clear machine identity, or rerun image setup. Normal narrow Debian
hooks such as `daemon-reload`, cache refresh, or an explicit versioned data
migration are allowed. A persistent-data migration exists only when a schema
actually changes, is idempotent, is tested from every supported schema, and
preserves user changes. Routine code/asset updates need no compatibility layer.

Guest updates use distro APT and `unattended-upgrades`/APT timers. There is no
custom long-running Buzzard root updater, check service, or updater timer. The
four package artifacts are published by versioned Buzzard OS GitHub releases
and indexed by the separately signed Open Research Tools APT repository. Local
builds infer no publication or signing authority.

## Reference image and one-time provisioning

The reference OCI is a normal Debian-family rootfs. Its Containerfile installs
`buzzardos-guest`, `buzzardos-desktop`, and `buzzardoscua` through the signed
Open Research Tools APT repository, allowing APT to resolve their stock
dependencies and retain ordinary package upgrades in the persistent machine.

Image provisioning is not package-update behavior. After package installation,
the Containerfile explicitly invokes one auditable setup script exactly once.
It owns only construction-time presets:

- create the canonical interactive UID/GID 1000 user and home;
- leave the canonical guest account locked in install media so the manager can
  set the user-selected password while committing each new machine;
- install a guest-only socket handoff that executes the real distro `sudo`
  with ordinary password authentication despite the persistent rootfs being
  `nosuid` and the desktop session retaining `no_new_privs`;
- seed initial user configuration from package templates once;
- enable/mask systemd units appropriate to a namespace guest;
- configure initial graphical/session and standard APT update presets;
- leave `/etc/machine-id` empty for first-boot generation;
- perform deterministic validation and cleanup.

Conceptually:

```Dockerfile
FROM debian:stable
RUN apt-get update \
 && apt-get install -y ca-certificates wget \
 && wget -qO /tmp/openresearchtools-keyring.deb \
      https://keyring.openresearchtools.com \
 && apt-get install -y /tmp/openresearchtools-keyring.deb \
 && apt-get update \
 && apt-get install -y buzzardos-guest buzzardos-desktop buzzardoscua \
 && /usr/libexec/buzzardos/setup-reference-image \
 && apt-get clean
```

The exact script name may change but not its boundary. It is invoked only by
image construction or an explicit image-builder workflow, never at guest boot
or package install/upgrade. A machine boots an already configured persistent
filesystem; launch supplies only dynamic runtime state.

After construction switches from immutable build inputs to the live signed
distribution repositories, it refreshes and retains their authenticated APT
package indexes. A newly created machine can therefore resolve a local `.deb`'s
repository dependencies immediately. Normal distro APT timers refresh those
indexes thereafter; Buzzard startup performs no package-manager work or
polling. Package archive caches may be cleaned, but the final package indexes
must not be deleted.

Buzzard OS does not install or use SSH. The image contains no SSH server,
control path, generated host keys, or exposed SSH port. Testing/control uses
owned namespaces and typed local runtime interfaces.

## Per-machine state and registry

Every create, pull, import, and clone requires an exact user-selected directory:

```text
<machine-directory>/
├── machine.json
├── runtime.json
├── machine.lock
├── cache/
│   └── source.oci.tar.zst    # only when explicitly retained
└── rootfs/
```

`machine.json` is the durable self-describing source of machine intent.
`runtime.json` and runtime sockets are reconstructible. Cache is not running
state. `rootfs/` is the flat, directly writable disk. Moving a stopped
directory and re-registering it requires no rootfs rewrite.

A JSON index at `$XDG_CONFIG_HOME/buzzardos/machines.json` stores only machine
UUID, display name, and directory. It is not a database or source of machine
state. `--machine-dir` supports scripts/recovery without the index. No rootfs
lives under `/usr`, `/var/lib`, Docker/Podman storage, or a hidden global
Buzzard directory.

Sharing is optional and per machine. Configure zero or more exact host files
or directories with explicit `ro`/`rw` mode. GUI supplies repeatable **Add
File**, **Add Folder**, and **Remove** controls. CLI `--share` is repeatable.
The broker creates an empty `/shared` and mounts only those entries under
distinct validated names.

Screen size, shares, port rules, network mode, GPU/device selection, and media
toggles are dynamic host settings. Packages and guest boot never bake or
re-provision them.

## OCI create, pull, import, export, and retention

Supported sources are a remote OCI reference (including authenticated and
explicit localhost registries), local OCI layout directory, OCI tar/gzip/zstd
archive, or Buzzard OS OCI export. End users need no Docker, Podman,
containerd, or other runtime.

All indexes, manifests, configs, and blobs are digest-verified. Layers apply in
order with whiteouts, modes, links, xattrs, ACLs, capabilities, sparse files,
and numeric guest ownership materialized through the destination host's
subordinate-ID map.

Canonical CLI behavior:

```text
buzzardos create --name NAME --machine-dir DIR --image SOURCE
                 --password-stdin [--share PATH[:ro|rw] ...]
                 [--keep-oci-archive]

buzzardos pull OCI_REFERENCE --name NAME --machine-dir DIR
               --password-stdin [--share PATH[:ro|rw] ...]
               [--keep-oci-archive]

buzzardos import SOURCE --name NAME --machine-dir DIR
                 [--mode restore|clone] [--manifest DIGEST]
                 --password-stdin [--share PATH[:ro|rw] ...]
                 [--keep-oci-archive]

buzzardos export MACHINE --output FILE
buzzardos clone SOURCE --name NAME --machine-dir DIR
                --password-stdin [--share PATH[:ro|rw] ...]
buzzardos password MACHINE --machine-dir DIR --password-stdin
```

`create` accepts any explicit supported OCI source. `pull` is the convenient
remote form and creates a machine after pulling; it is not hidden global cache.
`import` handles layouts/archives, remote sources, and Buzzard exports with
restore/clone semantics. The GUI exposes **Create from OCI**, **Pull OCI**, and
**Import**, asks for exact destination, supports repeated shares, and offers a
**Keep verified OCI archive** toggle.

All paths use one transaction:

1. validate destination, name, shares, source type, and manifest choice;
2. acquire and finalize a verified OCI representation in private staging;
3. flatten it on the destination filesystem into a staged rootfs;
4. validate the systemd/Sway guest-runtime contract;
5. optionally retain one verified canonical archive under machine `cache/`;
6. atomically commit the self-describing directory and registry entry.

Default behavior discards source archive and temporary blobs after rootfs
commit. `--keep-oci-archive` retains the verified archive for that machine
only. Retention is never required for boot, update, export, move, or recovery;
the archive is install media, never the running rootfs. Deleting it later does
not affect the machine.

Creation never replaces an existing destination. Partial acquisition or
expansion never registers a machine. OCI config environment, entrypoint,
command, user, working directory, labels, and stop signal are retained as
metadata, but systemd remains guest PID 1.

Export requires a stopped, exclusively locked machine. It snapshots flat
rootfs as canonical content-addressed OCI, excludes runtime mounts and shares,
verifies output, and commits atomically without overwriting. Export snapshots a
private copy with `/etc/machine-id`, the guest random seed, and any stale SSH
host keys cleared; it never mutates the source rootfs. Restore preserves a
Buzzard export's host metadata identity and rejects duplicates, while first
boot generates destination-local guest identity. Clone assigns a new host
metadata UUID and independently verifies the same identity clearing in staging.
Generic OCI sources receive fresh host metadata and guest identity.

Imported port rules start disabled; devices/media start off. Annotations never
pin host GPU nodes, monitor data, PipeWire/camera nodes, sockets, or capture.

## Host components

### Manager

The manager owns GUI/CLI, JSON registry, metadata, lifecycle,
create/pull/import/export/clone, source verification, and host settings. It
provides commands suitable for people, scripts, and agents. GUI actions call
the same typed operations as CLI.

It never injects current guest assets into an existing rootfs. Guest software
enters images as `.deb` packages and existing machines receive it via APT.

### Broker

The broker:

- creates user, PID, mount, network, IPC, UTS, and cgroup namespaces;
- maps canonical guest IDs through authorized subordinate IDs;
- mounts flat rootfs directly as `/` read/write;
- creates ephemeral `/proc`, `/run`, `/tmp`, `/dev`, integrations and shares;
- starts/supervises guest systemd as namespace PID 1;
- supplies private user networking and typed live port mappings;
- injects only selected devices/GPU and matching driver userspace;
- owns host media bridges without exposing the host PipeWire socket;
- rejects traversal, symlink escape, and arbitrary commands/mounts from guest
  metadata.

The product stays rootless, using distro `newuidmap`, `newgidmap`, `unshare`,
and kernel gates. It does not install LXC, weaken global user-namespace policy,
or add privileged Buzzard helpers. Where Ubuntu needs an AppArmor exception,
installation may add only an exact-path profile for the namespace entry point.

### Display

`buzzardos-display` is a native host Wayland app and filtered gateway. It owns
the one `xdg_toplevel` before start, during boot/run, during an in-place
restart, and after failure until the user closes it. A complete stop or clean
guest poweroff closes the native window and exits its broker. Exactly one fixed
guest output is host-facing for the lifetime of the machine window. The guest compositor is a buffer/input producer embedded as
that one monitor surface; it never owns host window policy. Guest-internal
off-screen outputs never create another host toplevel or host-control channel.

The window has ordinary drag, edge/corner resize, minimize, maximize/restore,
and close. One compact native header exposes `Machine`, `Ports`, `Devices`,
`Clipboard`, and `Settings`; there is no second toolbar or bottom host banner.
Host chrome consumes its own input. Only viewport events translate exactly
once to guest coordinates.

The gateway prevents extra host toplevels/popups, host-global input, host
window control, host clipboard/drag-and-drop, physical-output control, or
unclassified host globals. Resize changes logical/physical guest mode without
stretching and excludes host chrome. Monitor transitions negotiate scale,
refresh, transform, subpixel, color/HDR, input, dmabuf, sync, and presentation;
missing required capability fails clearly.

The fixed host-facing output stays live while minimized, occluded, unfocused,
or on another host workspace. Host callbacks pace visible presentation; an
internal clock continues the same guest scanout when callbacks stop, without
claiming vblank, creating a second host display/stream, or using a CPU-copy
fallback. Guest workspace selection and numbered virtual-output management are
performed only through guest Sway IPC and never send a host-control command.

Closing requests orderly guest shutdown and never discards rootfs. An
unexpected compositor disconnect changes the existing window to Failed rather
than silently closing it. An orderly complete stop closes the window; an
in-place restart keeps it.

## Guest runtime, desktop, and CUA

### Stock Sway/wlroots boundary

Sway is Wayland server for guest applications and nested client of the display
gateway. Wlroots is Sway's library, not a daemon. Apps connect only to Sway's
private socket. Sway presents one fixed host-facing output through the filtered
parent socket and may also maintain active guest-only off-screen virtual
outputs for numbered CUA and manual workspaces.

The gateway keeps a versioned capability table and modular handlers.
Guest-internal protocols stay inside Sway, safe parent monitor/input features
are translated, and escape-capable operations are isolated. On-demand
diagnostics report current advertised/facing versions and downgrade/denial
state without retaining an event log. Tests fail when a distro Sway/wlroots
update needs an unclassified parent protocol.

### Session and integration

`buzzardos-guest` starts separate supervised processes for Sway, output-sync,
clipboard, media/network integration, private D-Bus/AT-SPI,
PipeWire/WirePlumber, portals, and notifications. Narrow process boundaries
mean a media failure cannot kill display synchronization; one package versions
them because they implement one runtime protocol.

Output-sync owns Sway IPC coordination for size, scale, and paired keyboard
changes. RMLVO validation is identical on both sides; transitions queue
physical parent-keyboard input and commit/rollback atomically. Recovery
snapshots use `O_NOFOLLOW`, digest verification, and sealed memory, never a
mutable path passed to Sway.

The integration agent owns only guest halves of fixed media/port protocols and
pipelines. Broker remains authority for namespaces, listeners, host targets,
capture, shares, and devices. Guest cannot request arbitrary host mounts,
commands, ports, or capture targets.

### Guest workspace, output, and seat model

The guest always has a canonical `Desktop` workspace; it is the initial human
workspace shown by the fixed host-facing output. The host-facing output itself
is never recreated when the selected guest workspace changes. There is exactly
one human input seat, `seat0`. Host keyboard, pointer, touch, and tablet events
enter only through `seat0`. Synthetic CUA input never uses or impersonates
`seat0`.

Numbered CUA workspaces are created lazily:

```text
cua  == cua1  -> seat1 -> workspace CUA  -> numbered virtual output 1
cua2          -> seat2 -> workspace CUA2 -> numbered virtual output 2
cuaN          -> seatN -> workspace CUAN -> numbered virtual output N
```

Invoking `cuaN` ensures that exact seat/workspace/off-screen output exists and
is active; unused higher-numbered workspaces are not pre-created. Every CUA
seat is permanently scoped to its matching numbered workspace/output. CUA
coordinate transforms, output mode, screenshot metadata, pressed-input state,
and mutation lock are per numbered output/seat, not global. Operations from
different numbered callers may proceed independently; mutating operations for
one seat serialize through that seat's lock.

The official desktop exposes a thin guest-internal top bar with `Desktop`,
`CUA`, `CUA2`, ... selectors for existing workspaces and a `+` control. The
selectors change what the human views using only typed Sway IPC; they do not
resize, replace, or command the native host window. `+` creates a manual guest
workspace/output independently of CUA invocation and never allocates a CUA
seat or turns it into a CUA workspace.

Closing a numbered CUA or manual workspace is lossless: first move every
window on it to `Desktop` through confirmed Sway IPC, then disable and remove
its guest-only virtual output and selector. Failure to confirm all window moves
leaves the workspace/output intact. `Desktop`, `seat0`, and the fixed
host-facing output cannot be closed.

Taskbars enumerate only windows on their own current workspace. A window never
appears simultaneously in `Desktop` and CUA/manual taskbars. Every normal
window titlebar menu offers a Move action targeting `Desktop` and every
existing numbered CUA workspace; moving is confirmed against Sway's tree.

Global compact window listing remains available to CUA, but each row includes
the current output and workspace along with stable window identity, title,
application ID, state, and geometry. When `cuaN` focuses a window currently on
another workspace/output, it first moves that window to caller `N`'s workspace
and output through Sway IPC, confirms the move, and only then focuses it.
Focus never silently redirects the caller to another seat/output.

### Official desktop

`buzzardos-desktop` is classic XFCE/Openbox-style, not a full-screen launcher.
It provides the thin `Desktop`/`CUA`/`CUA2`.../`+` workspace bar described
above, a compact bottom taskbar scoped to the currently displayed guest
workspace, Applications menu, one task button per current-workspace window,
Show Desktop, `Files` and `/shared` shortcuts, and a clearly separated
lifecycle boundary: machine shutdown remains in the native host window and
never appears as a redundant Applications-menu row inside the guest.

Task buttons are contiguous, borderless, and have no visual gaps. Capped task
buttons are on by default, use a 260-pixel maximum and a 96-pixel minimum, and
expose adjacent `Applications`, `<`, `>` controls in that order only when every
window cannot fit at the minimum; paging controls never bracket the task list.
Each paging action moves the visible range by exactly five windows. Applications has immediate
case-insensitive search which clears whenever the menu closes, persistent
pin/unpin actions, and a full-output transparent click-away surface so any
click outside the visible menu closes it without reaching the covered client.
Every shell-owned surface restores the default pointer on entry so a client
resize/move cursor cannot persist over the empty desktop, panel, or
Applications click-away surface.

A secondary click on an application titlebar opens that window's controls at
the pointer's horizontal position. The titlebar binding sends only the opaque
window identity; after the transient click-away surface opens, its ordinary
Wayland pointer-enter event supplies the position. One stock-Sway zero-distance
cursor-focus refresh after mapping triggers that enter without moving the
cursor or returning its coordinates through IPC. Buzzard OS never writes a
host-input or CUA-input click-coordinate file. The surface consumes the first
outside click, closes the controls without activating the covered client, and
has an empty input region while closed.

FreeDesktop discovery hides helper/`NoDisplay` entries, responds to newly
installed entries, and presents each app once. Menus/taskbar work at small
sizes through measured layout, scrolling, and paging. Desktop supplies
Light/Dark, solid background color, time/location, official themes/icons, and
Thunar integration; it supplies no logo wallpaper or remote background.
Thunar's GTK3 status bar remains palette-correct in focused and backdrop
states; no inactive white strip is permitted.

Thunar exposes exactly five fixed, single-path helper actions for validated
Type-2 AppImages: Run AppImage, persistent source-adjacent Extract and Run,
Extract and Run `--no-sandbox`, Add AppImage to Applications, and Add AppImage
to Desktop. Removal is never a file-manager action. A managed AppImage's
Applications secondary-click menu owns exactly Open, Extract and Run, Extract
and Run `--no-sandbox`, Pin/Unpin, Add to Desktop, Rename, and Delete from
Applications. Deleting from Applications removes only that projection and
never deletes the source AppImage or extraction; an explicitly created Desktop
shortcut remains usable.

Every Applications, generated Desktop-shortcut, raw Desktop-AppImage, Thunar,
AT-SPI, and CUA launch enters the same fixed helper. Normal launch checks
`<AppImage>.extracted` first and otherwise executes the original AppImage. A
real, guest-user-owned extraction must contain an `AppRun` resolving inside
it. Extracted launch retains literal fixed arguments from the first safe
top-level desktop entry, discards FreeDesktop field-code arguments, and removes
an embedded `--no-sandbox` unless approved by an exact guest-user-owned,
regular, zero-byte, mode-0600 `.no-sandbox` marker. The explicit no-sandbox
action creates that marker. No action embeds a shell command.

Sway owns geometry/state. Drag, resize, minimize, maximize/restore, close,
focus, move-to-Desktop/CUA-workspace, and titlebar context actions use confirmed
Sway IPC. Do not add a layer-shell titlebar overlay or duplicate window
controls in task items. Human seat0 and each numbered CUA seat use the same
coordinate definitions, scoped to their respective output.

### Buzzard CUA

Buzzard CUA is a daemonless one-crate CLI. `cua` is exactly the `cua1` caller;
`cuaN` selects numbered seat/output/workspace `N` and lazily creates it when
needed. Every command performs its bounded operation and exits. There is no
start-session, end-session, attach-session, session token, resident CUA daemon,
MCP server, browser-specialized API, screen/action recording, telemetry,
self-update, or remote skill download.

The CLI provides raw visual and accessibility tools: screenshot, compact
global window listing/state, AT-SPI inspection/action, application launch,
window focus/move/state, pointer move/click/drag/scroll, and keyboard
keys/chords/text for Wayland and Xwayland. Each raw visual/input command binds
to the invoking `cuaN` seat and numbered output. It cannot inject through human
`seat0` or operate in another CUA caller's coordinate space.

Each numbered CUA seat uses Sway's normal native cursor on its own numbered
output. Buzzard CUA has no layer-shell cursor overlay, animated/vector cursor
theme, cursor registry, or cursor-theme/motion configuration tools. Human
`seat0` retains its independent normal Sway cursor.

Canonical coordinates are physical dmabuf pixels of the caller's numbered
output from `(0,0)` at its top-left. Screenshots contain exactly that output,
exclude host chrome and all other guest outputs, and are not downscaled for
fractional scale. Logical geometry transforms exactly once. Output resize or
workspace/output destruction atomically invalidates stale screenshot and
geometry metadata for that caller.

CUA returns structured success/failure and observable evidence. A helper exit
code is not success. Per-seat input cleanup never leaves pressed keys after
success, failure, cancellation, or process exit. Independent CLI invocations
coordinate only through minimal per-seat/output state and locks; this state is
not a session or daemon and grants no generic RPC surface.

The fork pins an exact upstream commit, preserves MIT notices, records Buzzard
changes, and contacts no upstream service. It exposes no host automation
socket, VNC/RDP, network control port, direct host compositor access, or host
window/workspace command.

## Clipboard boundary

Host and guest clipboards are separate. Guest never receives host clipboard
objects, socket, subscription, history, or inspection route. Host header has
exactly two one-shot actions:

- **Send Host Clipboard to Guest** reads once after click, validates,
  canonicalizes, transfers bounded bytes, and closes the transaction.
- **Copy Guest Clipboard to Host** creates an unpredictable short request;
  only its matching response may replace host clipboard. Unsolicited, replayed,
  wrong-nonce, late, or concurrent responses are rejected.

Version 1 accepts UTF-8 plain text without NUL (8 MiB) and still images. PNG is
canonical wire format (64 MiB, 8192 pixels/axis, 64 megapixels after decode);
native APIs may decode JPEG/WebP/BMP/TIFF and re-encode in RAM. HTML, RTF, SVG,
animations, URI/file lists, serialized/executable data, and arbitrary MIME are
rejected.

Transport is fixed, versioned, length-delimited, direction-specific, and uses
peer validation, nonce, `CLOEXEC`, deadlines, bounded memory, and single-flight
serialization. It carries no paths, commands, files, mutable mounts, generic
RPC, network, temporary file, host D-Bus/PipeWire/Wayland. Contents/hashes are
never logged or persisted.

Actions enable only while that machine agent is ready. Each machine has
independent state/nonces. Stop, timeout, close, or new transfer cancels prior
transaction and releases buffers.

## Networking, media, devices, and isolation

Default network is private user-mode networking with host-loopback disabled.
No-network or host-network modes are explicit. Host-to-guest and
guest-to-host TCP/UDP rules are typed, live, revocable, stored in host metadata,
and exact-address/port limited. New binds default `127.0.0.1`; `0.0.0.0`
requires warning. Guest cannot create/retarget rules.

Audio output, microphone, and camera are independent default-off bridges. Guest
never receives host PipeWire socket. Microphone capture is a named desktop
PipeWire recording stream with Buzzard identity and host privacy accounting,
never direct ALSA bypass. Disable terminates capture and private endpoint.

GPU injection includes only authorized DRM/NVIDIA nodes and matching userspace.
Multiple selections and `all` are explicit. Diagnostics measure device,
renderer, dmabuf format/modifier, explicit sync, presentation, CUDA, and codec;
they never label fallback zero-copy. Host kernel driver is prerequisite.

Guest receives only rootfs, explicit shares, selected devices/userspace, one
filtered Wayland connection, fixed private integration endpoints, and narrow
boot mounts. Never expose host home, D-Bus, AT-SPI, clipboard, PipeWire socket,
agent credentials, arbitrary `/dev`, runtime sockets, or real Wayland socket.

Rootfs is `nosuid` and session retains `no_new_privs`. Guest Type-2 AppImage
support uses filtered `/dev/fuse` and narrow guest-root mount mediation; it
never grants `CAP_SYS_ADMIN` to session/apps. Ordinary files/symlinks never
gain execution through AppImage detection.

## Lifecycle

Create/pull/import uses the verified staged transaction above. It never boots
source through Podman and never modifies host OS.

Start:

1. reuse native window in `Stopped` and lock machine;
2. repair only stale ephemeral runtime state;
3. enter `Starting`, construct namespaces/mounts/network/devices/display;
4. execute guest systemd as namespace PID 1;
5. start private session, Sway, integrations, and selected desktop; Buzzard CUA
   remains an already installed on-demand CLI and no CUA daemon is started;
6. report `Running` only after runtime readiness; require desktop readiness
   only when a provider is selected.

Stop requests orderly systemd poweroff, allows state save, escalates after a
bounded timeout, tears down ephemeral state, and returns same window to
`Stopped`. Normal start never repulls OCI, expands layers, provisions image,
installs packages, or rewrites UID ownership.

Testing/control needs no SSH. Acceptance tooling may enter owned namespaces
with `nsenter` and run as guest root or UID 1000. Product control uses direct
supervision and narrow typed Unix sockets/files; no general remote shell or
arbitrary host-to-guest exec API exists.

## Build and release authority

`oci/build-local.sh` is the local developer entry point and never
pushes/authenticates. The GitHub workflow builds and audits exactly four
packages; a pushed version tag publishes all four `.deb` files and their
checksums to that tag's GitHub release. Manual workflow runs verify the
published-APT reference OCI on a disposable runner and discard it.

The separate `openresearchtools/apt` repository indexes stable GitHub release
assets, signs its Release metadata in its protected `apt-signing` environment,
and publishes only indexes, signatures, public keys, and the archive-keyring
package. Buzzard OS publishes no OCI registry image.

## Minimum release acceptance

A handoff is incomplete until automation plus real Wayland/GPU journeys prove:

- four independently versioned packages build/install/upgrade and carry AGPL
  plus third-party license evidence;
- guest packages enter OCI only as dpkg-owned payloads and one-time setup runs
  only during construction, never upgrade/start;
- image uses only distro Sway/wlroots and records resolved package versions;
- create, localhost/remote pull, layout/archive import, clone, export, shares,
  exact destinations, archive discard, and retention pass atomic failure tests;
- retained archive deletion cannot affect rootfs; restart performs no OCI work;
- moved machines re-register without ownership rewrite;
- identities differ across creation/clone, persist across move/restart, and do
  not derive from host identity;
- guest files, packages, and UI customization survive restart; automatic APT
  updates do not rerun provisioning;
- systemd is namespace PID 1; desktop/Sway/PipeWire and every invoked CUA CLI
  run as UID 1000; sudo is confined to guest root;
- runtime works without `buzzardos-desktop` and supports another provider;
- one host toplevel remains usable before/during/after runtime/failure; all
  controls work without guest input leakage;
- resize, fractional scale, monitor change, hidden-window scanout, dmabuf,
  explicit sync, and truthful fallback reporting work;
- clipboard isolation and two one-shot transfers pass hostile, replay,
  concurrency, multi-machine, timeout, MIME, and size tests;
- live port/media/share/device toggles work on real hardware and fully revoke;
- the fixed host-facing output and single host window remain unchanged while
  lazy CUA/CUA2/manual outputs are created, switched, kept active off-screen,
  and closed entirely through guest Sway IPC;
- workspace close moves every window to Desktop before output removal;
  taskbars remain workspace-scoped and titlebar Move targets are correct;
- concurrent `cua`/`cuaN` invocations bind their own seat/output, lock per
  seat, produce output-scoped screenshots/coordinates, and never use seat0;
- compact global window listings report output/workspace, and CUA focus moves
  a selected foreign-workspace window into the caller workspace before focus;
- daemonless CUA/AT-SPI performs screenshot, input, window, application,
  Wayland, Xwayland, accessibility, and off-screen journeys with visible
  evidence, with no session/MCP/browser/recording/telemetry subsystem;
- guest AppImages work through constrained FUSE without `CAP_SYS_ADMIN` and
  ordinary files never gain execute permission;
- namespace, filesystem, service, clipboard, media, network, and device
  isolation holds; and
- neither image nor tests install, start, generate identity for, or rely on SSH.

Compilation, unit tests, process start, and API exit status alone are not
sufficient release evidence.
