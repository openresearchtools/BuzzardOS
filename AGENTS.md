# Buzzard OS: Authoritative Product Specification

This file is the source of truth for every contributor and coding agent working
in this repository. Implementations, tests, documentation, packaging, and
design decisions must preserve the requirements below.

The approved guest Settings, desktop-file operations, AppImage registration,
updates, theming, sound, and scaling contract is incorporated by
reference from `docs/GUEST_SETTINGS_DESKTOP_INTEGRATION_PLAN.md`. Changes to
those features must update that plan and this specification together.

## Product

Buzzard OS is a rootless, persistent Linux desktop-machine manager installed
on Debian-family hosts as a normal, versioned Debian package. The host package
is named `buzzardos`; its application-menu name, window titles, metadata, and
other human-facing identity are `Buzzard OS`. It is updated by APT and is not
distributed as an AppImage or an extracted portable application folder.

Each machine is created in a user-selected directory. That directory contains
the complete machine metadata, cache, and mutable flat rootfs. A small JSON registry under
`$XDG_CONFIG_HOME/buzzardos/` records the registered machine UUID, display
name, and machine directory. The registry is an index, not
the source of machine state: a machine directory remains self-describing and
can be re-registered after moving it. `--machine-dir` selects the exact machine
directory for create/import/clone and remains an override for scripting and recovery.

Machine creation consumes an explicitly selected local or remote OCI source.
The reference guest image is assembled by installing Buzzard OS's two guest
Debian packages into a normal Debian rootfs. The guest runs systemd as PID 1
and a complete desktop session. The distribution-provided Sway and wlroots
packages, using wlroots' nested Wayland backend, composite the entire guest
desktop into exactly one native host Wayland window.

Buzzard OS is not an ephemeral application container:

- The rootfs is a durable machine disk, not a disposable container layer.
- Normal operation uses no overlay filesystem.
- Guest package installs, OS edits, user files, application state, and desktop
  configuration persist across restarts.
- The guest is a multi-process systemd system, not one foreground OCI process.
- Guest application windows are never forwarded as separate host surfaces.
- The nested compositor is the guest display, input, screenshot, and
  accessibility boundary.

All human-facing product names are `Buzzard OS`. This is a new application;
there is no released legacy installation, machine registry, or persistent
rootfs compatibility contract. New package, executable, desktop, D-Bus,
theme, and diagnostic identities use Buzzard OS naming directly. Upstream
names appear only where required for attribution.

## Repository and build boundaries

The source tree mirrors the three independently understandable deployment
parts:

```text
host/                 native host application and Debian package payload
guest/                guest desktop package and pinned Buzzard CUA fork
packaging/            reproducible binary-Debian-package assembly
oci/                  local Debian OCI assembly consuming guest outputs
tests/acceptance/     hardware/session/CUA journeys and fixtures
tools/                local tests and licensing gates
LICENSES/             machine-readable dependency and asset evidence
```

- `host/` and `guest/` are separate locked Cargo workspaces. Host crates do not
  belong to the OCI build context. Guest desktop and Buzzard CUA outputs enter
  the reference image only through their built `.deb` files.
- `guest/asset-manifest.tsv` is the authoritative mapping of guest source files
  to package destinations and modes. The guest desktop package and OCI
  installation must be contract-tested against it.
- `oci/compose.yaml` and `oci/build-local.sh` are local developer build entry
  points. They must not authenticate to or push a registry. The manually
  dispatched release-assets workflow builds the same OCI definition only as a
  disposable GitHub-runner intermediate and never publishes that image.
- Cargo targets, Debian packages, OCI archives, downloaded acceptance applications,
  screenshots, and other generated artifacts are built outside the repository
  by default and are never committed.
- Distribution assembly runs on a disposable GitHub-hosted Linux x86-64
  runner, never on a maintainer's workstation. The manually dispatched
  workflow is artifact-only and has no trigger for pushes or pull requests.
  It uploads its results for inspection and must never create a GitHub
  Release.
- The workflow never pushes an OCI image, GHCR image, GitHub Package, or APT
  repository. It builds the three `.deb` files and reference OCI locally in
  the runner for verification and discards intermediates with the runner.

## Debian packages and future APT publication

The build emits exactly three independently versioned amd64 binary packages:

```text
buzzardos_<version>_amd64.deb
buzzardos-guest-desktop_<version>_amd64.deb
buzzardcua_<version>_amd64.deb
```

- `buzzardos` contains the host manager, broker, display application, desktop
  file, AppStream metadata, icon, and Buzzard-owned helper payload. It declares
  its distro-owned runtime dependencies and must install and run on Ubuntu
  24.04 LTS, Debian 13, and Ubuntu 26.04.
- `buzzardos-guest-desktop` contains the Buzzard OS shell, Settings, clipboard
  agent, guest services, session glue, configuration, themes, and desktop
  integration. It depends on the distro `sway` package and compatible distro
  wlroots ABI package; it never ships a private compositor build.
- `buzzardcua` contains the reviewed Linux fork formerly described as TryCua
  Cua Driver. Its product, package, executable, service-facing, and diagnostic
  identity is `Buzzard CUA`; upstream attribution remains explicit.

The root `VERSION` file is authoritative for `buzzardos` and
`buzzardos-guest-desktop`. `guest/BUZZARDCUA_VERSION` is authoritative for
`buzzardcua`. A built package's `--version`, Debian control metadata,
AppStream release metadata, and artifact filename must agree. Package upgrades
must preserve registered machines and mutable rootfses; uninstalling a package
must not delete them.

During development the OCI definition installs the locally built guest `.deb`
files with APT/dpkg and then installs their declared dependencies from the
pinned Debian snapshot. Future publication may place the same packages in a
separately reviewed signed APT repository. Repository signing, key rotation,
release channels, rollback policy, and upload credentials are not inferred or
implemented by the current artifact workflow.

`.github/workflows/build-release-assets.yml` remains manually dispatched and
artifact-only. It has no push or pull-request trigger, no publisher job, and
no write permission. It must never create or modify a GitHub Release, tag,
environment, package, registry object, or APT repository.

GitHub Release or prerelease publishing may be designed only through a later,
separately reviewed explicit change. That future change must add its own strict
final licensing gate, tag/commit validation, approval boundary, and
least-privilege publication design; none of those future capabilities may be
inferred from the current artifact workflow. Artifact assembly currently
retains the under-2-GiB file guard so the output remains eligible for such
a future review.

## Host state and rootless contract

Installed application files follow Debian filesystem policy under `/usr` and
are owned by dpkg. Mutable machines are never stored under `/usr`, `/var/lib`,
Docker/Podman storage, or a hidden global Buzzard OS data directory. A machine
directory has this shape:

```text
<user-selected-machine-directory>/
├── machine.json
├── runtime.json
├── machine.lock
├── cache/
└── rootfs/
```

The application asks for the machine directory during each create/import
operation. Sharing is optional. A machine may configure zero or more explicit
host files or directories; each is mounted as a separate entry below
`/shared`. The GUI provides repeatable Add File, Add Folder, and Remove
controls, while CLI `--share` is repeatable. It never imposes one storage
location or one global shared directory for all machines. Removing the host
package leaves machine and shared data untouched.

Normal host prerequisites are limited to:

- Linux kernel support for the required unprivileged namespaces and mounts.
- Configured subordinate UID/GID ranges and trusted host
  `newuidmap`/`newgidmap` authorization gates.
- The host package uses the distro-owned `unshare`, `newuidmap`, and
  `newgidmap` gates to authorize the exact subordinate-ID map. Buzzard OS does
  not install or use LXC and never disables Ubuntu's global AppArmor policy.
  When Ubuntu's unprivileged-user-namespace AppArmor gate is active, the
  package may add only an exact executable-path `userns` profile for its
  installed namespace entry point. It never changes the global sysctl or
  grants a wildcard path.
- A host Wayland session.
- A working host GPU kernel driver and permission to selected devices.
- For optional audio, microphone, and camera integration, a working host
  PipeWire session, its standard PulseAudio-compatible recording service, and
  permission to the explicitly enabled device. Those host clients are normal
  declared Debian dependencies.
- No host FUSE or AppImage runtime is required for Buzzard OS itself. Guest
  AppImages remain supported inside the isolated guest.
- Machine export uses the distro GNU tar declared by the host package.
- The native host application is built on Ubuntu 24.04. Its package is
  install-tested on Ubuntu 24.04, Debian 13, and Ubuntu 26.04; a dependency or
  symbol floor unavailable on any of those targets fails release acceptance.

The product remains rootless. The Buzzard OS package installs no setuid helper,
privileged daemon, or system service. Distro packages such as `uidmap` remain
the authorization gates. Unsupported host security policy must produce a
precise diagnostic instead of weakening isolation.

## Machine portability and OCI exchange

- `BuzzardOS import SOURCE --name NAME --mode restore|clone` accepts a local OCI image-layout
  directory, an OCI tar/gzip/zstd archive, a Buzzard OS export, or a remote OCI
  reference. Multi-image indexes require an unambiguous native Linux manifest
  or explicit `--manifest` digest/reference selection.
- Import verifies index, manifest, config, and every layer digest; applies
  layers in order with OCI whiteouts; and materializes ownership through the
  destination host's subordinate-ID map. No source host UID is persisted as a
  portability requirement.
- Authenticated OCI environment values are applied to the machine boot
  environment. Entrypoint, command, user, working directory, labels, and stop
  signal are retained for a lossless OCI exchange, but do not replace the
  required desktop-machine boot: systemd remains guest PID 1 and the rootfs
  must satisfy the systemd desktop contract before the machine is committed.
- `BuzzardOS export NAME --output FILE` requires the machine to be fully
  stopped and exclusively locked. It enters the exact machine ID namespace,
  snapshots the flat rootfs as one canonical OCI layer, preserves numeric
  guest IDs, hardlinks, symlinks, modes, timestamps, xattrs, ACLs, capabilities
  and sparse files, excludes runtime mounts and every configured host share, writes all OCI blobs
  content-addressed, verifies the completed archive, and commits it atomically
  without replacing an existing file.
- Restore mode preserves a Buzzard OS export's guest machine identity and
  rejects a duplicate identity in the machine registry. Clone mode and the
  `BuzzardOS clone SOURCE NEW_NAME` convenience command assign a new host
  metadata UUID and remove `/etc/machine-id`, the random seed, and SSH host keys
  inside private staging before the atomic machine-directory commit. On first
  boot, guest init creates the machine ID and, when the distro `ssh-keygen` is
  present, creates only missing default host keys before systemd starts.
  Generic OCI images without a Buzzard OS identity annotation always receive
  fresh destination-local identity.
- OCI annotations retain machine intent but never pin destination-host
  GPU nodes, PipeWire node names, camera nodes, monitor details, runtime
  sockets, or active capture. Imported port rules start disabled and imported
  device sharing starts off until the destination user enables it.
- The implementation owns this OCI behavior and never requires Docker,
  Podman, containerd, or another container runtime on the end-user host.

## Components

### `buzzardos`

The launcher:

- Owns registered per-machine configuration and lifecycle.
- Pulls and digest-verifies OCI images with bundled functionality.
- Applies OCI layers correctly, including whiteouts, modes, links, xattrs, and
  ownership metadata.
- Creates new machines atomically and never replaces an existing machine
  implicitly.
- Starts the display gateway and broker and waits for systemd, Sway, the
  desktop shell, and the in-guest CUA driver.
- Exposes create, start, stop, status, list, doctor, and host-window controls.
- Provides a host-owned settings UI for machine lifecycle, initial window size,
  network mode, and explicit GPU selection. Settings that affect namespace or
  device construction clearly require a machine restart.
- Reports measured renderer, device, dmabuf, explicit-sync, presentation, and
  fallback diagnostics without inventing a zero-copy result.

### `buzzardos-broker`

The broker:

- Creates user, PID, mount, network, IPC, UTS, and cgroup namespaces.
- Makes guest systemd namespace PID 1.
- Mounts the persistent rootfs directly as `/` read/write.
- Creates an empty private `/shared` and mounts only the machine's explicit
  host file/folder shares below it with their configured access mode.
- Creates only ephemeral guest mounts such as `/proc`, `/run`, `/tmp`, and the
  required `/dev` view.
- Passes one filtered host Wayland connection to the nested compositor without
  exposing the host Wayland socket path.
- Provides private user-mode networking by default.
- Reconciles typed host-authorized port mappings live. Host-to-guest mappings
  use the bundled slirp4netns private API. Guest-to-host mappings terminate in
  machine-private relays that can reach only the configured host destination;
  they never re-enable unrestricted host-loopback access.
- Owns independent live media bridges for guest audio output, host microphone
  input, and host camera input. It never mounts the host PipeWire socket into
  the guest.
- Opens microphones only as named recording streams in the host desktop audio
  session, with Buzzard OS's application identity. It must never bypass host
  recording/privacy accounting by opening an ALSA capture endpoint directly.
- Injects every explicitly selected DRM/NVIDIA device plus matching host driver
  userspace, including multi-GPU selection and `all`.
- Supervises PID 1 and cleans ephemeral runtime state without discarding the
  rootfs.

It validates every path and machine identifier, rejects traversal and symlink
escapes, passes only explicit mounts/devices, and never accepts arbitrary
mounts or commands from mutable machine metadata.

### `buzzardos-display`

`buzzardos-display` is a complete native host Wayland application, not a
decoration strip retrofitted onto a guest-owned toplevel. It owns the only host
`xdg_toplevel` before the machine starts, while it boots, while it runs, after
it stops, and when startup fails. The guest compositor's output is an embedded
monitor surface inside that application window.

The native host application:

- Has an ordinary host-owned application frame with working titlebar drag,
  edge and corner resize, host minimize, maximize/restore, and close. On
  compositors without server-side decorations it supplies a complete,
  correctly scaled client-side frame; controls must never be clipped or
  positioned using negative child surfaces outside an unrelated guest
  toplevel.
- Uses one compact native header bar. `Machine`, `Ports`, `Devices`,
  `Clipboard`, and `Settings` are direct header-bar controls beside the machine
  title, lifecycle state, and native window controls; there is no second
  menu/toolbar row and no bottom informational banner consuming monitor space.
  `Machine` exposes Start,
  Stop, Restart, orderly Shut Down, machine state, and exit/close. `Settings`
  exposes initial monitor size, network mode, explicit GPU selection including
  `all`, and diagnostics. Start/Stop/Restart are host lifecycle actions, never
  guest taskbar or guest power buttons.
- Provides `Ports` and `Devices` controls. Port rows contain direction,
  protocol, host address/port, guest address/port, and enabled state. Device
  controls independently toggle guest audio to host speakers, host microphone
  to guest, and host camera to guest. These integrations apply live and report
  rejection or runtime failure without restarting PID 1.
- Provides a `Clipboard` control with exactly two explicit one-shot actions:
  `Send Host Clipboard to Guest` and `Copy Guest Clipboard to Host`. These
  actions transfer a bounded snapshot; they never connect or synchronize the
  host and guest clipboard services.
- Every new port row prepopulates the host address as `127.0.0.1` and resolves
  the current guest address automatically from the active machine network;
  users never have to discover the guest's private IP. An explicit
  `0.0.0.0` host bind exposes a guest service on every host IPv4 interface and
  must work for LAN clients when host routing/firewall policy permits it. The
  UI warns before enabling that wider exposure and never changes loopback to
  all interfaces implicitly.
- Shows explicit `Stopped`, `Starting`, `Running`, `Stopping`, and `Failed`
  monitor states in the same window. A failed boot leaves the host application
  open with the error and a retry action.
- Its native header labels every actively shared host microphone and camera for
  the complete capture interval. In addition, microphone capture must register
  as a host desktop recording stream so GNOME and other compatible shells show
  their normal global microphone/privacy indication. Startup fails rather than
  silently capturing if that host-visible registration cannot be observed.
- Keeps all lifecycle and settings controls outside the embedded guest monitor.
  Pointer or keyboard events over host chrome are consumed by the host
  application and are never forwarded into the guest. Events over the monitor
  are translated exactly once into guest-output coordinates.
- Proxies only the Wayland globals required by the nested compositor and
  prevents a replaced or compromised guest compositor from creating host
  toplevels, popups, decorations, menus, or controlling outer-window policy.
  The embedded compositor is a buffer/input producer, not the owner of the
  host window.
- Enforces the machine title and application ID and exposes exactly one host
  toplevel for the complete application.
- Translates resizing of the embedded monitor viewport into the guest virtual
  output's logical and physical mode without bitmap stretching. Toolbar,
  menu, titlebar, and borders are excluded from the guest output size.
- Keeps running long enough to supervise the display attachment independently
  of one guest-compositor connection. Guest disconnect changes the monitor to
  a stopped/failed state; it does not destroy the host application.
- Keeps the one guest virtual output live while the host toplevel is minimized,
  fully occluded, unfocused, or on another workspace. Real host frame callbacks
  pace visible presentation; when they stop, an internal refresh clock consumes
  the same scanout's newest dmabuf so in-guest applications, screenshots, CUA,
  and AT-SPI continue working. This is never implemented as a second display,
  VNC/RDP/video stream, or CPU copy, and background frames are never falsely
  reported as host-vblank presentations.
- Requests orderly systemd shutdown before allowing application exit while a
  machine is running. Persistent rootfs state is never destroyed by closing
  the host window.

The outer titlebar controls the whole native host application. A titlebar
inside the monitor controls only that guest application. Guest power actions
are labeled `Shut Down Machine` and never masquerade as host-window controls.
The former gateway-client-side negative-subsurface frame is explicitly not an
acceptable implementation of this contract.

### Explicit one-shot clipboard transfer

The host and guest clipboards remain separate by default. The guest must never
receive the host clipboard object, host data-device objects, a host clipboard
socket, a subscription, a history API, or any capability that lets guest code
decide when to inspect the host clipboard. Clipboard sharing happens only
after a human activates one of the two native host-header actions for one
machine:

- `Send Host Clipboard to Guest` causes the host application to read its own
  clipboard exactly once after that click. It copies the selected value into a
  bounded in-memory buffer, validates and canonicalizes it, sends only those
  bytes through the machine's typed clipboard channel, then closes the
  transaction and clears the host-side transfer buffer best-effort. The guest
  clipboard agent takes ownership of the copied value inside Sway so normal
  guest applications and agents can paste it. It receives no reference or
  continuing route back to the host clipboard.
- `Copy Guest Clipboard to Host` creates one fresh, unpredictable request with
  a short deadline. Only while that request is outstanding may the fixed guest
  clipboard agent return one snapshot of the current private guest clipboard.
  The host validates the returned value before replacing its clipboard. Guest
  messages sent without the matching live request are ignored and can never
  cause the host to read, disclose, or replace its clipboard.

Version 1 provides normal basic clipboard interoperability for valid UTF-8
plain text without embedded NUL characters and ordinary still images. PNG is
the canonical private wire format,
not a requirement on the application that originally placed an image on either
clipboard. The host/guest native clipboard API negotiates a supported still
image offering (PNG, JPEG, WebP, BMP, or TIFF), decodes it under limits, and
re-encodes it to `image/png` entirely in RAM. Native screenshot clipboard
objects and toolkit texture providers therefore work without a file or a PNG
source. Text aliases are canonicalized to `text/plain;charset=utf-8`. HTML,
RTF, SVG, animated images, URI/file lists, serialized objects, executable
formats, and arbitrary MIME types are rejected. Text is limited to 8 MiB. PNG
transport is limited to 64 MiB and, after safe decode, 8192 pixels on either
axis and 64 megapixels. Sources are consumed lazily only after the
corresponding host click; reads, writes, framing, conversion, and image decode
have bounded deadlines and memory.

The transport is a fixed, length-delimited, versioned protocol with
direction-specific messages, peer/provenance validation, a per-request nonce,
`CLOEXEC` descriptors, single-flight serialization, and no shell commands,
paths, file payloads, or mutable mount instructions. It uses no network port,
temporary file, host D-Bus, host PipeWire, host Wayland socket, or generic
guest-to-host RPC surface. Its Unix listener is guest-owned; the host connects
to it only after a native action, and there is no host listener that guest code
can call. Clipboard contents and content hashes are never
logged or persisted; diagnostics contain only direction, canonical MIME,
bounded byte count, timestamp, result, and non-content error category.

The actions are enabled only when that machine's interactive guest clipboard
agent is ready. They are disabled while Stopped, Starting, Stopping, or Failed.
Each native machine window has independent clipboard state, nonce space, and
channel. Closing, stopping, timing out, or starting another transfer cancels
the transaction and releases all transport buffers. A compromised guest may
offer hostile bytes only after the user explicitly requests a guest-to-host
copy; host-side type, size, structure, and image-decode validation happens
before those bytes become a host clipboard value.

### Complete desktop protocol boundary

The filtered display gateway is a complete virtual-monitor and input backend
for the distribution-provided wlroots Wayland backend. It is not a permanently minimal
allowlist that gains ordinary desktop capabilities only after applications
break.

Protocol responsibilities are classified explicitly:

- Guest applications connect only to Sway's normal private session socket.
  Sway provides guest-internal shell, window-management, screencopy,
  accessibility, activation, clipboard, drag-and-drop, input-method, relative
  pointer, pointer constraints, gestures, tablet, touch, presentation, dmabuf,
  explicit-sync, color-management, and Xwayland integration as supported by
  the distro-resolved guest stack. These protocols do not expose the host.
- The gateway translates every safe host capability required to make
  Sway's one nested output behave like a complete physical monitor and
  input seat. This includes native logical/physical modes, refresh and
  presentation timing, output transform and physical metadata, dmabuf
  feedback, explicit synchronization, active-output color descriptions and
  HDR metadata, ICC/color-space/primaries/transfer/luminance information,
  content-type hints, relative and locked pointer behavior, high-resolution
  scrolling, touchpad gestures, touch, tablet/stylus, keyboard compose and
  host IME text entry, and focus/capture transitions.
- Only operations that could escape the one-window boundary are denied:
  creating additional host toplevels or popups, enumerating or capturing the
  host desktop, observing host-global input, controlling host windows,
  accessing host clipboard or host drag-and-drop through the guest Wayland
  connection, leasing or reprogramming physical outputs, and binding arbitrary
  unclassified host globals. The explicit host-owned one-shot clipboard policy
  below is a separate typed byte-transfer capability; it never proxies a host
  clipboard protocol or object into the guest.

The implementation uses a version-negotiated capability table and modular
protocol handlers generated from the pinned Wayland protocol definitions.
Changing one protocol handler must not require restructuring the display
gateway. Startup and diagnostics record the host-advertised version, the
guest-facing version, whether the capability is translated, guest-internal,
intentionally isolated, or unsupported, and the reason for any downgrade.
Unknown or newly advertised globals are classified and reported; a capability
required by the selected desktop mode must fail clearly instead of silently
disappearing.

Moving the host window between monitors atomically renegotiates scale, physical
mode, refresh behavior, transform, subpixel hint, color description, HDR
metadata, and input-device capabilities for the same guest output. No
hardcoded RGB/BGR layout, SDR assumption, ICC profile, refresh rate, DPI, or
GPU identity is acceptable. Mixed-scale, mixed-refresh, mixed-color-space,
rotated, SDR/HDR, iGPU/dGPU, and hot-plugged monitor transitions must not
require restarting the machine or rebuilding the application.

The release protocol inventory is checked against the resolved distro Sway, wlroots,
Wayland, wayland-protocols, GTK, and host-compositor interfaces. Automated
contract tests fail when the nested backend begins requiring an unclassified
protocol or when a required translated capability lacks a handler. This is a
maintained compatibility boundary like a native compositor backend, not a
monthly ad-hoc refactor.

## Guest system

The reference image is a Debian-family desktop with:

- systemd as PID 1 and a normal UID 1000 interactive user.
- Passwordless sudo confined to the guest.
- A private system D-Bus and private interactive session D-Bus.
- Private PipeWire and WirePlumber services.
- The fixed GStreamer/PipeWire elements required by the private, host-gated
  media endpoints. These elements connect only to the guest PipeWire service.
- A private AT-SPI registry and accessibility tree.
- Unmodified Sway, wlroots, `libxkbcommon`, and `xkb-data` installed from the
  guest distribution's configured APT repositories. Buzzard OS does not fetch,
  fork, compile, or privately ship those projects. The reference-image build
  records their exact resolved Debian package versions.
- The guest uses the distro-owned `/usr/share/X11/xkb` and
  `libxkbcommon.so.0`. The host display uses its host distro equivalents. The
  paired keyboard protocol binds the canonical compiled keymap digest rather
  than requiring byte-identical host and guest package payloads.
- Human keyboard layout changes are paired host/guest transactions, never a
  Sway-only mutation. RMLVO validation is identical in Settings, output-sync,
  and the host parser: ASCII component syntax and byte limits are exact,
  layouts contain one to four non-empty groups, variants are globally empty or
  contain at most the matching number of comma-aligned slots (empty alignment
  slots are valid), and non-empty options contain no empty segment. The guest
  sends only that bounded RMLVO plus the canonical digest. Host Prepare queues
  only physical parent-keyboard events. For Sway, output-sync reads the
  user-owned recovery snapshot once through `O_NOFOLLOW`, verifies its digest,
  copies it into a write/grow/shrink/seal-protected memfd, and retains the fd
  while Sway consumes only `/proc/<output-sync-pid>/fd/<fd>`; the mutable
  recovery pathname is never passed to Sway. Host Commit activates the
  matching modifier/group state before replay. Failure restores the prior Sway
  map before Abort. A private durable in-session journal and a bounded-backoff
  supervisor reconcile Prepare/Commit response loss and process crashes
  through typed Status requests before physical input resumes.
- Xwayland for legacy X11 applications.
- Buzzard OS's native Rust desktop shell.
- Buzzard CUA running as the interactive user from the separately versioned
  `buzzardcua` package.
- GTK, Qt/KDE, Electron, Chromium, Vulkan, and OpenGL application support.
- Exactly four preinstalled, user-facing general applications: Firefox ESR,
  the customized Thunar file manager, Mousepad, and Foot. `ffmpeg` remains a
  non-menu runtime/codec utility.
- Native Type-2 AppImage support: `libfuse.so.2`, FUSE 3 utilities, the
  explicitly filtered `/dev/fuse` device, and automatic owner-execute
  authorization for genuine AppImage ELF files arriving in guest-owned
  storage. Ordinary files and symlinks must not gain execute permission.
- Noto Core, CJK, and Color Emoji fonts so normal multilingual Unicode text
  renders as glyphs instead of missing-character boxes.

The reference image contains no Plasma shell, KWin, XFCE shell, Wayfire, labwc,
Waybar, Fuzzel, patched compositor, compiler toolchain, Blender, Chromium,
Dolphin, Pavucontrol, `x11-apps`, XTerm/UXTerm, Mesa/Vulkan diagnostic tools,
Buzzard OS Electron demo, or private wlroots fork. Removing `x11-apps` and
XTerm/UXTerm does not remove Xwayland support. Removing Mesa/Vulkan diagnostic
tools does not remove the graphics runtime or drivers. Users may install or
replace desktop software inside their persistent machine. That cannot alter
the host, but replacing the reference compositor or boot assets may make Buzzard
OS integration diagnostics fail.

The persistent rootfs remains `nosuid` and the guest retains Linux
`no_new_privs`. Native AppImage execution must not solve FUSE authorization by
granting `CAP_SYS_ADMIN` (ambient or otherwise) to the interactive session,
AppImage runtime, or application tree. A narrowly scoped guest-root mount
broker may duplicate libfuse's private Unix descriptor and execute the pinned
distribution `fusermount3` with setuid-equivalent requester credentials. That
broker is inside the already authorized guest-root boundary and cannot access
the host mount namespace.

## Classic guest desktop

The Rust shell provides a simple classic XFCE/Openbox-style desktop, not a
full-screen launcher or a tile-grid dashboard:

- One desktop only; no numbered workspace buttons are shown.
- A persistent, compact bottom taskbar.
- A left-aligned `Applications` button.
- A narrow, textless `Show Desktop` button at the far right of the taskbar
  that minimizes every visible guest application through confirmed Sway IPC
  state changes without hiding the desktop icons or panel.
- Exactly one simple task button per running application.
- No pinned application launcher is duplicated beside a running-window task
  button.
- No minimize, maximize, close, or power buttons are duplicated in task
  buttons.
- A compact bottom-left Applications menu with vertically aligned rows,
  real FreeDesktop theme icons, scrolling for narrow/small outputs, installed
  `.desktop` discovery, each installed entry shown once, and a clearly
  separated `Shut Down Machine` item.
- The Applications menu measures installed labels, icons, header controls, and
  the current guest logical output instead of using a fixed package-time
  width. Its height is content-sized until it reaches the usable guest output;
  only overflowing rows scroll. A labelled, AT-SPI-visible close button sits
  at the right of the menu header.
- Desktop shortcuts for `Files` and `Shared`; `Shared` opens `/shared`.
- Desktop launcher icons display the launcher's localized FreeDesktop `Name=`
  rather than its on-disk `.desktop` filename. File managers continue to show
  the real filename.
- Appearance settings provide exactly Light/Dark toolkit theme selection and
  one accessible solid desktop background-colour picker. The product ships no
  logo wallpaper, wallpaper image, preview, gradient, or remote background.
- Guest Settings also provides a `Time & Location` page with a live automatic
  date/time display and a searchable installed IANA time-zone dropdown. The
  guest may change only its persistent timezone, never the shared kernel
  clock, and does not run a second NTP client.
- New `.desktop` files installed by the user appear without rebuilding the
  image.

Renaming a registered AppImage on the guest Desktop is one authoritative,
crash-recoverable shortcut-helper transaction. It uses a private durable
journal and an inter-process lock shared by registration reads and mutations,
verifies the source identity descriptor-relative to the XDG Desktop, performs
a same-directory no-replace rename, changes only the stable
registration's target path, and preserves its ID, launchers, icons, and target
bytes. Store startup finishes an interrupted transaction from the observed
inode and record state. Ambiguity or replacement fails closed and never
deletes a possible target. Because XDG Desktop and XDG state/data can reside
on different filesystems, the contract is ordered fsync plus deterministic
journal recovery, not an impossible cross-filesystem atomic rename.

The Sway session provides synchronized compositor-side guest decorations and
window operations for normal Wayland and Xwayland windows: title, drag,
edge/corner resize, minimize, maximize or restore, and close. Sway owns the
authoritative tree and every state/geometry mutation is issued and confirmed
through Sway IPC. Do not introduce a layer-shell titlebar overlay, geometry
polling, or taskbar window-control copies. The frame and application content
must move in the same compositor transaction.

Secondary-clicking a guest titlebar opens its window-control menu horizontally
at the actual guest pointer coordinate, clamped to the output; it must not
default to the window's left edge. Human host input and in-guest CUA input use
the same logical guest coordinate contract for this action.

The desktop must remain usable at small window sizes: task buttons page rather
than overflow, the menu scrolls rather than dropping entries, and the panel
remains aligned and clickable.

No compositor root context menu is configured in the reference session.
Clicking or right-clicking empty guest desktop space must never expose
`Terminal`, `Reconfigure`, or compositor `Exit` actions. Applications are
opened through the Buzzard OS Applications menu, desktop shortcuts, an
agent/CUA request, or a terminal intentionally opened by the user.

## Accessibility and in-guest computer use

The guest owns its complete computer-use environment:

```text
private guest D-Bus session
└── at-spi2-registryd
    ├── Buzzard OS desktop shell
    ├── GTK applications
    ├── Qt/KDE applications
    ├── Electron/Chromium applications
    └── in-guest agent and CUA/MCP drivers
```

- The Rust shell publishes its real desktop icons, Applications menu, every
  installed application, and every running-app task button through AT-SPI.
- Visual menu scrolling and taskbar paging never hide applications or running
  windows from AT-SPI: an agent can invoke any installed application or focus
  any running window directly without traversing visual pages.
- Applications publish their normal accessibility objects to the same private
  guest AT-SPI bus.
- The in-guest agent can take full-output screenshots, move/click/type through
  the guest compositor, inspect AT-SPI, invoke controls, open arbitrary
  installed apps, and automate Xwayland clients when those tools support them.
- The CUA service may remain running and a CUA session may remain open while a
  human types through the host window. Its persistent synthetic keyboard never
  grabs the seat. Named keys, chords, and Unicode text all use that same
  daemon-owned Wayland object rather than creating and destroying one-shot
  virtual keyboards. It returns every pressed key and modifier to neutral
  after success, error, cancellation, unwind, reconnect, session end, and
  graceful shutdown. Cancellation unconditionally restores the fixed keymap
  and completes a bounded same-client sync after releases and zero modifiers;
  session teardown fail-stops if that boundary cannot be proven. A failed roundtrip first drains the local pressed-key
  ledger on the same keyboard. If that connection is dead, it closes the
  virtual keyboard so wlroots releases its compositor-side pressed set
  on that same device before Sway removes it, then reconnects and publishes a
  zero modifier state. Recovery never replays a press on a replacement device.
  SDK shutdown resets but does not terminate the reusable process-global owner;
  abrupt process death uses that same compositor-side destruction path.
  Exactly simultaneous human and agent events may interleave like two physical
  keyboards on one Linux seat; an idle CUA must never suppress human input.
- Every operation above remains available when the one native host window is
  covered, unfocused, on another workspace, or minimized.
- Canvas/game/non-accessible surfaces remain operable by screenshot and input
  coordinates even when they expose no useful semantic nodes.
- Testing uses the in-guest CUA and AT-SPI interfaces or developer namespace
  entry. It does not add an SSH server or expose a guest control port to the
  host network.
- Keyboard-coexistence hardware acceptance must inject the human half through
  the host compositor, native Buzzard OS monitor, display gateway, and
  nested parent keyboard while the CUA session remains open. A guest-local
  `wtype` process is another synthetic guest keyboard and is never accepted as
  evidence of human-input coexistence. The host monitor must be deterministically
  focused and unsupported host compositors require an explicit harness input
  hook or clearly labelled interactive human step; the test fails rather than
  silently substituting guest input.

### Buzzard CUA fork

The repository carries the required Linux driver sources as an auditable fork
of [`trycua/cua`](https://github.com/trycua/cua), pinned to an exact upstream
commit. It is not downloaded unpinned during a release build.

- Preserve the upstream MIT license, copyright notices, source attribution,
  and a machine-readable record of the upstream repository and commit.
- Keep Buzzard OS modifications identifiable in source and changelog files.
  Do not claim upstream endorsement and do not remove third-party notices.
- Vendor only the packages and transitive source/assets actually required by
  the in-guest Buzzard CUA and MCP/CLI contract. Record and comply with every
  vendored third-party license; optional components with additional license
  obligations are not silently included.
- CUA product telemetry is removed from the fork: there is no telemetry
  endpoint/key, identity/config, sender, lifecycle/tool observer, or telemetry
  CLI. Vendor update checks, self-update, and remote skill downloads are also
  removed. Starting or using the bundled driver must not make automatic or
  user-invoked requests to CUA/trycua services; Buzzard OS owns updates.
- The supported execution target is the normal interactive guest session:
  stock Sway/wlroots plus Xwayland. No host automation socket, VNC/RDP
  indirection, host cursor injection, SSH daemon, or direct host compositor
  access is used.
- Implement full-output screenshots, window listing and state, pointer move,
  click, double-click, button down/up, drag, vertical/horizontal scroll,
  keyboard keys/chords, text entry, application launch, window focus,
  minimize, maximize/restore, close, and AT-SPI inspection/action.
- Wayland applications use compositor/session-native routes; Xwayland
  applications use the appropriate X11 route inside the same guest display.
  Raw input fallbacks must declare their limits instead of reporting success
  without an observable effect.
- Window enumeration, global frame geometry, focus, close, and exact frame
  configuration use Sway's stock IPC interface. That Unix socket exists only
  inside the interactive guest session. It does not reach the host compositor
  and is never exposed as a host-network control port.
- A screenshot and every absolute CUA coordinate share one canonical
  coordinate space: the guest output's native physical dmabuf pixels with
  origin `(0, 0)` at the monitor's top-left. The screenshot dimensions equal
  the current physical output mode and the image is never downscaled merely
  because the host uses fractional scale. Compositor, Xwayland, and AT-SPI
  logical geometry is transformed exactly once into that physical coordinate
  space; the current logical mode and scale remain separately reported. Host
  titlebar, toolbar, menu, border, and surrounding desktop pixels are never
  present in guest screenshots or guest coordinates.
- Window-relative coordinates are converted to the canonical desktop space
  using compositor-reported geometry. Resizing the host monitor viewport
  invalidates stale geometry and screenshot metadata atomically so an agent
  never clicks using a frame from the previous output mode.
- CUA methods return structured success/failure and post-action evidence where
  applicable. A successful input request means the compositor accepted the
  event and the expected target/state can be observed, not merely that a
  helper process exited zero.

KDE application compatibility must not create an unexpected Plasma-style
password-wallet prompt. The reference image disables and removes KWallet
D-Bus/portal auto-activation while retaining libraries required by installed
Qt/KDE applications. A normal Applications-menu launch must not start
`ksecretd` or `kwalletd`.

The host receives only the nested compositor's one surface. Guest AT-SPI,
application D-Bus, panels, internal windows, screenshots, and input tools are
not exported to the host. A guest agent can see and control the complete guest
display, but cannot screenshot, keylog, enumerate, or control the surrounding
host desktop.

## One-window display behavior

Sway connects through the filtered display gateway and creates one nested
output:

- Every guest panel, menu, dialog, notification, application, game, and
  Xwayland window is composited into that output.
- No guest application receives the host Wayland connection.
- No guest application appears as an independent host window.
- Host resize changes the guest virtual monitor mode and causes an immediate
  guest re-layout at the new native logical size.
- Fractional host scale produces the matching physical guest buffer and a
  correct logical desktop; do not stretch a low-resolution image.
- Keyboard, pointer, touch, tablet, focus, and relative-pointer events enter
  only through the machine window.
- The guest virtual output exists independently of host-window visibility.
  Minimizing, covering, unfocusing, or moving the host window to another
  workspace must not suspend or remove the guest display: applications,
  animations, CUA input, AT-SPI, and guest-only screenshots continue to work
  against the same output mode and coordinate space.
- Host presentation and guest scanout pacing are distinct clocks for that one
  output, not two displays or a streamed fallback. While visible, real host
  frame callbacks and presentation feedback pace scanout. If the host
  compositor withholds callbacks for a hidden window, a refresh-matched
  internal clock continues guest scanout without claiming physical
  presentation or vblank. Real host-vblank pacing resumes when the surface is
  presented again, without changing guest geometry or coordinates.
- Host visibility is not guest display existence: minimizing or occluding the
  native application stops physical presentation but never removes, suspends,
  or freezes Sway's one virtual monitor.

## GPU and zero-copy presentation

Applications and Sway render on selected physical GPU devices. The intended
active path is:

```text
guest Vulkan/OpenGL application
  -> dmabuf-backed GPU allocation
  -> Sway/wlroots composition
  -> dmabuf fd + modifier + explicit fence
  -> host Wayland compositor import
  -> host vblank presentation
```

When reported as zero-copy, there is no CPU copy, readback, texture upload, or
extra GPU image blit between Sway's final buffer and host presentation.
Dmabuf support alone is insufficient: renderer, DRM device identity,
format/modifier compatibility, feedback, explicit sync, and presentation
timing must all be measured. Any fallback is clearly reported.

NVIDIA support includes selected `/dev/nvidia*` nodes, capability devices, DRM
render nodes, matching host EGL/GLX/Vulkan/OpenGL metadata and libraries, CUDA,
NVENC/NVDEC, and multiple GPU selection. The host kernel driver is a
prerequisite and is never installed by Buzzard OS.

NVIDIA injection is implemented through a pinned, bundled NVIDIA Container
Toolkit/libnvidia-container CDI integration:

- Bundle the release-compatible `nvidia-ctk`, `nvidia-container-cli`, and
  required `libnvidia-container` userspace in the extracted `app/` payload with their upstream
  licenses and exact versions/checksums. End users must not install NVIDIA
  Container Toolkit, Docker, Podman, or a system CDI service.
- At every start, use the bundled toolkit against the current host driver to
  generate an ephemeral CDI description in Buzzard OS's private runtime
  directory. Never depend on `/var/run/cdi/nvidia.yaml`, a host
  `nvidia-cdi-refresh` service, or host `nvidia-ctk`/`nvidia-container-cli`
  binaries.
- Parse and validate the generated CDI edits, resolve every source
  canonically, and translate only the selected GPU's declared device nodes,
  driver libraries, firmware/capability nodes, ICD metadata, and environment
  into the fixed rootless broker mount model. CDI must not become an arbitrary
  mutable mount or command surface.
- `all`, index, UUID, multiple-GPU, and compatible MIG selection must produce
  the same device set reported by the bundled toolkit. Missing selected
  devices, driver/userspace mismatch, or incomplete CUDA/graphics/codec
  capability is a hard startup diagnostic rather than a silent CPU fallback.
- Status and diagnostics record toolkit version, generated CDI device names,
  resolved selected devices, mounted driver version, and the observable
  results of graphics, CUDA, NVENC, and NVDEC probes.

## Isolation

Default networking is a private network namespace with bundled user-mode
networking and host-loopback disabled. Explicit host and no-network modes may
be configured.

The host may explicitly authorize live bidirectional mappings while private
networking is active:

- Host-to-guest TCP and UDP mappings publish exactly one selected guest
  address and port on exactly one selected host bind address and port.
- Guest-to-host mappings publish exactly one selected listener inside the
  guest and terminate at exactly one selected host destination. Turning a
  mapping off closes its listener and active relay connections.
- Mapping configuration lives in host-owned machine metadata outside the
  rootfs. Guest processes cannot add mappings or change their destinations.

Media sharing is default-deny and independently revocable:

- Guest audio output may be connected to the host's default PipeWire output.
- Host microphone and host camera capture are separate opt-in toggles.
- Enabling microphone or camera starts a host-owned capture process, installs
  one machine-private internal mapping, and creates one corresponding source
  in the private guest PipeWire graph. Capture remains active for the complete
  enabled interval; the host UI states this explicitly and never presents the
  switch as dormant per-application permission.
- Microphone capture uses the host PipeWire-Pulse source-output path with a
  stable Buzzard OS application identity. The broker verifies a running,
  correctly targeted `Stream/Input/Audio` node before reporting the bridge
  active, allowing the host shell to expose its standard recording indicator.
- Disabling either input first terminates host capture, then removes the
  internal mapping and guest source. When disabled, the guest has no host
  device node, host PipeWire socket, capture process, reusable stream endpoint,
  or other route to that input.
- The host package declares the client libraries, GStreamer launcher, plugins,
  and installs Buzzard OS bridge code. It uses the already-running desktop PipeWire service in the
  same way as a native application; it never asks the user to install global
  bridge packages.

The guest receives no host filesystem or desktop-service access except:

- its persistent rootfs mounted as `/`;
- only the machine's explicitly configured optional file/folder shares mounted
  as separate entries below `/shared`;
- selected GPU/device and matching driver resources;
- the one filtered Wayland connection;
- the fixed per-machine clipboard-agent endpoint, which can receive only a
  host-authorized clipboard snapshot and can answer only a matching live
  host-created guest-snapshot request; and
- narrow read-only kernel/runtime mounts required to boot.

Never expose host home, host D-Bus, the host PipeWire socket, host SSH agent,
host AT-SPI, the host clipboard/data-device service, arbitrary `/dev`,
Docker/Podman sockets, or the real host Wayland socket.

## Lifecycle

Creation:

1. Validate the machine name, exact selected machine directory, and optional
   repeatable host file/folder shares.
2. Resolve and digest-verify the explicitly requested OCI image/layout/archive.
3. Apply the OCI layers on the same filesystem into a
   staging directory, using the recipient host's full subordinate-ID mapping.
4. Install versioned guest boot/session assets.
5. Atomically rename the staging tree to the exact user-selected machine
   directory, then register its UUID/name/path in the JSON index.
6. Never replace an existing machine implicitly.

Start:

1. Open or reuse the native host application window in `Stopped` state.
2. Lock the machine and repair stale runtime state.
3. Change the embedded monitor to `Starting` and create namespaces,
   networking, mounts, GPU injection, and the filtered display attachment.
4. Execute `/lib/systemd/systemd` as namespace PID 1.
5. Start private D-Bus/AT-SPI, Sway, shell, audio, portals, and CUA driver.
6. Attach Sway's output inside the existing native window and change state to
   `Running` only after the desktop and CUA readiness checks pass.
7. Keep the broker supervising PID 1 while the native application continues
   owning the window and lifecycle controls.

Stop:

1. Request orderly systemd poweroff.
2. Allow the desktop and services to save state.
3. Escalate only after a bounded timeout.
4. Tear down namespaces and ephemeral mounts.
5. Detach the guest output and return the same native application window to
   `Stopped`.
6. Leave every rootfs and external shared-path change intact.

Normal start never repulls the image. Rebase/update is explicit and never
discards local changes without informed user action.

## Release acceptance

A release and implementation handoff are incomplete until automated tests,
an agent-driven real Wayland/GPU hardware run, and visual inspection of the
captured artifacts demonstrate every requirement below. A coding agent must
not stop after compilation, unit tests, process-start checks, or API exit
status while any safe in-scope acceptance scenario remains untested.

- The three versioned `.deb` packages install cleanly and upgrade through dpkg/APT.
- The final OCI already contains every managed guest asset and both compiled
  guest executables as dpkg-owned files. The host launcher never injects,
  migrates, or overwrites guest package payloads during import or start. Its
  installed manifest, paths, modes, distro Sway/wlroots packages, CUA
  attribution, and required commands pass `oci/verify-image.sh`.
- Download the current official x86-64 LM Studio AppImage outside the
  repository, verify its vendor-published digest, copy it into `/shared` with
  mode `0644`, and prove the generic watcher adds owner execute permission.
  Launch it directly with no `--appimage-extract-and-run`, extracted cache, or
  vendor-specific command-line workaround; require the FUSE mount, Electron
  startup, a real Sway window, CUA focus/screenshot, and clean unmount to work
  while the application process has UID 1000 and no `CAP_SYS_ADMIN`.
- Live host-to-guest and guest-to-host port mappings can be added, changed,
  exercised with real TCP/UDP traffic, and removed without changing the
  container PID. Removal closes existing relays, conflicting binds fail with a
  precise diagnostic, and unrestricted host loopback remains unreachable.
- Guest audio reaches the physical host output with a generated signal and
  measured sample flow. The explicitly selected physical host microphone and
  camera are exercised through their real host-advertised backends, appear as
  guest-private sources only while enabled, carry verifiable non-placeholder
  samples/frames, and disappear after toggling off. Synthetic PipeWire sources
  are permitted only in unit/CI coverage and never satisfy hardware or release
  acceptance. While the microphone is enabled, the host graph must contain a
  running PipeWire-Pulse recording source-output with Buzzard OS's application
  ID, selected target, and capture-process PID; this must drive the standard
  host desktop recording/privacy indication and disappear on disable. After
  disablement, attempts from the guest to reconnect to the old endpoint fail
  and no host capture process remains.
- The installed host package passes the same media tests with only its declared
  Debian dependencies and dpkg-owned Buzzard OS helpers.
- Machine data is stored only in each user-selected machine directory; the XDG
  JSON file is an index and never contains rootfs data.
- Moving a stopped machine requires only re-registering its new directory;
  guest ownership is not rewritten.
- A guest file and installed package survive full stop/start.
- systemd is namespace PID 1 and the interactive user can run services and
  passwordless guest sudo.
- The host receives exactly one machine toplevel.
- Before the container starts, while it boots, while it runs, after it stops,
  and after a failed start, the same native host application remains usable.
- Every host application control is driven in a real Wayland session: titlebar
  drag, four-edge/four-corner resize, minimize, maximize, restore, close,
  Machine menu, Clipboard menu, Settings menu, Start, Stop, Restart, retry
  after failure, GPU selection, network selection, and initial monitor size.
  No action leaks a click or keystroke to the guest.
- `Machine`, `Ports`, `Devices`, `Clipboard`, and `Settings` remain in the
  native header bar at every tested size and scale. No separate toolbar or
  bottom confinement banner reduces the embedded guest monitor viewport.
- With no clipboard action, continuously mutate both clipboards and prove that
  neither side can enumerate, read, subscribe to, or overwrite the other.
  Activate `Send Host Clipboard to Guest` and prove exactly the clicked host
  text/still-image snapshot becomes the guest selection while subsequent host
  changes remain invisible. Activate `Copy Guest Clipboard to Host` and prove exactly
  one guest snapshot becomes the host selection while unsolicited, replayed,
  wrong-nonce, late, oversized, invalid-UTF-8, malformed-PNG, unsupported-MIME,
  and concurrent responses are rejected without changing the host clipboard.
  Repeat across two simultaneous machines and Stop/Start; require independent
  state, no transfer history/content logs, bounded memory/time, and disabled
  actions outside the Running/clipboard-ready state.
- The host frame and menu span the complete window at 100%, 125%, 133%, 150%,
  175%, and 200% scale; no titlebar or control is clipped.
- Host resize and fractional scale yield the exact guest logical/physical
  output and sharp screenshots.
- Guest titlebar drag and live edge resize have no detached/lagging overlay.
- The classic menu, desktop shortcuts, taskbar paging, installed-app discovery,
  and `/shared` shortcut work at normal and small sizes.
- CUA screenshots cover exactly the complete guest output and exclude all host
  chrome. Screenshot dimensions equal the compositor's physical output mode;
  reported logical mode/scale, transformed AT-SPI geometry, and pointer
  coordinates agree before and after host resize and fractional-scale changes.
- Type and read back multilingual text containing an em dash, an accented Latin
  word, CJK text, and emoji through a native AT-SPI editable. The exact Unicode
  string must survive insertion/readback and every character must render with a
  real glyph in the captured physical-resolution output.
- Repeat Applications-menu launch, application interaction, window close, and
  full-output screenshot checks with the native host window minimized and fully
  covered. Require observable guest frame changes while host-presented-frame
  counters remain unchanged, then require measured host-vblank presentation to
  resume when the window becomes visible.
- Drive every CUA operation through the installed in-guest CLI/MCP interface,
  including move/click/double-click/drag/scroll, key/chord/type, application
  launch, task focus, minimize/maximize/restore/close, full screenshot, window
  screenshot, and window enumeration.
- Exercise those operations against the Buzzard OS shell and real GTK,
  Qt/KDE, Electron/Chromium, native Wayland, and Xwayland applications. Assert
  visible post-action state and inspect screenshots, rather than accepting
  command exit status alone. Applications absent from the reference image are
  installed only into the dedicated disposable/persistent acceptance machine;
  they are never treated as preinstalled release content.
- In a dedicated persistent acceptance machine, use only the installed
  in-guest CUA/MCP/AT-SPI interfaces to perform a complete human-style desktop
  journey:
  - open the Applications menu, scroll it at normal and small output sizes,
    launch every user-visible installed application entry once, and verify
    that service/helper/`NoDisplay` entries remain hidden;
  - open, focus, minimize, restore, maximize, unmaximize, move, resize, and
    close application windows through guest titlebars and task buttons;
  - switch repeatedly between multiple open apps using the bottom taskbar and
    verify exactly one correctly titled task per window;
  - browse a real page in Firefox ESR, interact with page controls, scroll,
    type text, change tabs, and confirm its complete accessibility tree;
  - create, rename, copy, move, and delete test files in `Files` and every
    configured shared entry, and verify changes from both guest and the selected host path;
  - use a terminal to run normal commands and a passwordless guest
    `sudo apt install`, then prove the installed package persists after a full
    Stop/Start cycle;
  - install Blender into this acceptance machine (Blender remains absent from
    the reference image), launch it with GPU acceleration, navigate its menus,
    orbit/pan/zoom the viewport, select the default object, and drag a
    transform handle. Verify screenshots before and after each coordinate
    action and verify that resize/fractional-scale changes do not shift the
    target;
  - exercise a representative native Wayland client and Xwayland client with
    the same screenshot, pointer, keyboard, window-state, and AT-SPI checks.
- Capture timestamped full-output screenshots, relevant window screenshots,
  CUA request/result JSON, AT-SPI tree extracts, output-mode/scale state, and
  runtime diagnostics for the journey. Manually inspect those artifacts for
  clipping, stale coordinates, duplicated controls, click leakage, detached
  decorations, blur/stretching, and incorrect focus before declaring success.
- AT-SPI enumerates and invokes shell, GTK, Qt/KDE, Electron/Chromium, and
  compatible Xwayland controls.
- The vendored Buzzard CUA fork is pinned, reproducible, carries its upstream
  MIT license and
  upstream attribution, records local modifications, and passes license
  inventory checks.
- The default network and desktop-service isolation boundaries hold.
- Vulkan/OpenGL render on every selected GPU; NVIDIA CUDA and codec injection
  work when selected.
- NVIDIA acceptance is performed on a host that does not provide
  `nvidia-ctk`, `nvidia-container-cli`, or libnvidia-container through an
  undeclared `PATH` or runtime linker search path. The host package's declared
  NVIDIA helper payload must generate and apply its private CDI result
  successfully.
- Run `nvidia-smi -L` and a compiled CUDA compute probe inside the guest, then
  run the architecture-matching CUDA artifact from
  `openresearchtools/llama-cpp-arm64-builds` release `b10276`. On x86_64 the
  required artifact is
  `llama-b10276-bin-ubuntu-cuda13-x64.tar.gz`, SHA-256
  `4747dd212618ed5eecef318a3538b9e9ee4c3fb2808b226420aa8152a2fe0724`;
  on ARM64 use the corresponding ARM64 CUDA artifact and its published digest.
  Download it once into the acceptance machine's `cache/` for the hardware test only; do not
  bundle llama.cpp or a model in the released app/reference image.
- With a small test GGUF in the dedicated acceptance machine, require
  llama.cpp device enumeration to report the selected NVIDIA GPU, run a real
  `llama-bench` or short inference with all model layers requested on CUDA,
  assert from llama.cpp output that CUDA buffers/layers are active, and
  independently observe the guest process on the selected GPU. CPU-only
  success does not pass. Repeat device isolation for each explicit GPU choice
  and a multi-GPU choice when hardware is present.
- Zero-copy and explicit-sync status is measured and truthfully reported.
