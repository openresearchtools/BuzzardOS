# Wild Buzzard: Authoritative Product Specification

This file is the source of truth for every contributor and coding agent working
in this repository. Implementations, tests, documentation, packaging, and
design decisions must preserve the requirements below.

The approved guest Settings, desktop-file operations, AppImage registration,
updates, theming, sound, scaling, and branding contract is incorporated by
reference from `docs/GUEST_SETTINGS_DESKTOP_INTEGRATION_PLAN.md`. Changes to
those features must update that plan and this specification together.

## Product

Wild Buzzard is a rootless, persistent Linux desktop-machine launcher
distributed as one native AppImage.

The complete portable download carries a digest-verified, already-flattened
rootfs seed. First machine creation materializes that seed once into a flat
mutable root filesystem using the recipient host's subordinate-ID mapping, and
every later launch boots that same rootfs. An explicitly configured OCI image
remains an alternative creation source and is pulled and flattened once. The
guest runs systemd as PID 1 and a complete desktop session. Stock Sway, using
wlroots' nested Wayland backend, composites the entire guest desktop into
exactly one native host Wayland window.

Wild Buzzard is not an ephemeral application container:

- The rootfs is a durable machine disk, not a disposable container layer.
- Normal operation uses no overlay filesystem.
- Guest package installs, OS edits, user files, application state, and desktop
  configuration persist across restarts.
- The guest is a multi-process systemd system, not one foreground OCI process.
- Guest application windows are never forwarded as separate host surfaces.
- The nested compositor is the guest display, input, screenshot, and
  accessibility boundary.

## Repository and build boundaries

The source tree mirrors the three independently understandable deployment
parts:

```text
host/                 native host application and AppImage packaging
guest/                guest shell, managed rootfs assets, and pinned CUA fork
oci/                  local Debian OCI assembly consuming guest outputs
tests/acceptance/     hardware/session/CUA journeys and fixtures
tools/                local tests and licensing gates
LICENSES/             machine-readable dependency and asset evidence
```

- `host/` and `guest/` are separate locked Cargo workspaces. Host crates do not
  belong to the OCI build context. The AppImage packager may build guest
  outputs only because it carries them as managed migration assets for
  persistent rootfses.
- `guest/asset-manifest.tsv` is the authoritative mapping of guest source files
  to rootfs destinations and modes. OCI installation and the host migration
  table must be contract-tested against it.
- `oci/compose.yaml` and `oci/build-local.sh` are local developer build entry
  points. They must not authenticate to or push a registry. The manually
  dispatched release-assets workflow builds the same OCI definition only as a
  disposable GitHub-runner intermediate and never publishes that image.
- Cargo targets, AppDirs, OCI archives, downloaded acceptance applications,
  screenshots, and other generated artifacts are built outside the repository
  by default and are never committed.
- Distribution assembly runs on a disposable GitHub-hosted Linux x86-64
  runner, never on a maintainer's workstation. The manually dispatched
  workflow is artifact-only and has no trigger for pushes or pull requests.
  It uploads its results for inspection and must never create a GitHub
  Release.
- The workflow never pushes an OCI image, GHCR image, or GitHub Package. It
  builds the reference OCI locally in the runner, verifies and flattens it,
  and discards the OCI intermediate when the runner is destroyed.

## Distribution artifacts and future publication

The checked-in artifact workflow emits exactly two primary files:

```text
WildBuzzard-x86_64.AppImage
WildBuzzard-portable-x86_64.tar.zst
```

- The standalone AppImage is independently downloadable and replaceable so an
  existing portable folder can update the host application without replacing
  a persistent machine.
- The complete portable archive contains that same AppImage, a verified
  high-compression flat-rootfs seed, initial `vm/`, `shared/`, and `cache/`
  directories, checksums, provenance, and licenses. On first machine creation
  the seed is materialized into the normal mutable `vm/<name>/rootfs/`; the
  compressed seed is not the running rootfs and no overlay is introduced.

The bundle keeps licensing evidence in two explicit groups: host/AppImage
payload evidence and guest/rootfs payload evidence. Each group contains the
notices and corresponding-source/provenance records for the exact payload it
describes. The runner-generated manifest binds the AppImage, flat-rootfs
archive, source commit, OCI source descriptors, package inventory, and hashes.

`.github/workflows/build-release-assets.yml` is manually dispatched and
artifact-only. It has no push or pull-request trigger, no release/prerelease
mode, no publisher job, and no write permission. A successful run uploads
exactly two short-lived Actions artifacts named for the two files above. It
must never create or modify a GitHub Release, tag, environment, package, or
registry object.

GitHub Release or prerelease publishing may be designed only through a later,
separately reviewed explicit change. That future change must add its own strict
final licensing gate, tag/commit validation, approval boundary, and
least-privilege publication design; none of those future capabilities may be
inferred from the current artifact workflow. Artifact assembly currently
retains the under-2-GiB per-file guard so both outputs remain eligible for such
a future review.

## Portable on-disk layout

All Wild Buzzard files live beside the AppImage by default:

```text
portable-folder/
├── WildBuzzard-x86_64.AppImage
├── runtime/                         # present in the complete first-install bundle
│   ├── WildBuzzard-rootfs-linux-x86_64.tar.zst
│   └── WildBuzzard-rootfs-linux-x86_64.json
├── vm/
│   └── <machine-name>/
│       ├── machine.json
│       ├── runtime.json
│       ├── machine.lock
│       └── rootfs/
├── shared/
└── cache/
```

- `vm/<machine-name>/rootfs/` is the complete, flat, directly writable guest
  operating system.
- `runtime/` contains the verified canonical-ID seed used to create new
  machines offline. It is install media, never the mounted or running rootfs,
  and is absent when only the independently replaceable AppImage is supplied.
- `shared/` is user-managed host/guest storage mounted read/write at `/shared`
  in every guest.
- `cache/` contains only disposable downloads and OCI intermediates.
- Machine metadata stores portable relative references and never embeds the
  portable folder's original absolute path.

When running as an AppImage, derive `portable-folder/` from the original
`$APPIMAGE` path. Never derive it from `/tmp/.mount_*`. `--storage-dir` may
explicitly select another portable folder.

Do not use `~/.wb`, XDG data directories, Docker/Podman storage, hidden host
state, or system-wide machine directories. Copying the portable folder must
move the AppImage, machines, shared files, and cache together without rewriting
metadata.

## Self-contained AppImage and rootless host contract

The release AppImage is the complete user-facing application. End users must
not install Wild Buzzard's pull, extraction, namespace, network, GPU-injection,
or packaging helpers themselves. Release helpers are bundled and resolved
relative to the mounted AppDir, never from host `PATH`.

Normal host prerequisites are limited to:

- Linux kernel support for the required unprivileged namespaces and mounts.
- Configured subordinate UID/GID ranges and trusted host
  `newuidmap`/`newgidmap` authorization gates.
- A host Wayland session.
- A working host GPU kernel driver and permission to selected devices.
- For optional audio, microphone, and camera integration, a working host
  PipeWire session, its standard PulseAudio-compatible recording service, and
  permission to the explicitly enabled device. Wild Buzzard bundles its
  PipeWire/GStreamer/PulseAudio client stack; users do not install or configure
  Wild Buzzard-specific host services or helpers.
- Standard facilities required to execute an AppImage, with extract-and-run
  support when FUSE is unavailable.

The product remains rootless. It does not install a setuid helper, daemon,
package, or system service. Unsupported host security policy must produce a
precise diagnostic instead of weakening isolation.

## Components

### `wildbuzzard`

The launcher:

- Owns portable machine configuration and lifecycle.
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

### `wildbuzzard-broker`

The broker:

- Creates user, PID, mount, network, IPC, UTS, and cgroup namespaces.
- Makes guest systemd namespace PID 1.
- Mounts the persistent rootfs directly as `/` read/write.
- Mounts sibling `shared/` at `/shared` read/write.
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
  session, with Wild Buzzard's application identity. It must never bypass host
  recording/privacy accounting by opening an ALSA capture endpoint directly.
- Injects every explicitly selected DRM/NVIDIA device plus matching host driver
  userspace, including multi-GPU selection and `all`.
- Supervises PID 1 and cleans ephemeral runtime state without discarding the
  rootfs.

It validates every path and machine identifier, rejects traversal and symlink
escapes, passes only explicit mounts/devices, and never accepts arbitrary
mounts or commands from mutable machine metadata.

### `wildbuzzard-display`

`wildbuzzard-display` is a complete native host Wayland application, not a
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
- Uses one compact native header bar. `Machine`, `Ports`, `Devices`, and
  `Settings` are direct header-bar controls beside the machine title, lifecycle
  state, and native window controls; there is no second menu/toolbar row and no
  bottom informational banner consuming monitor space. `Machine` exposes Start,
  Stop, Restart, orderly Shut Down, machine state, and exit/close. `Settings`
  exposes initial monitor size, network mode, explicit GPU selection including
  `all`, and diagnostics. Start/Stop/Restart are host lifecycle actions, never
  guest taskbar or guest power buttons.
- Provides `Ports` and `Devices` controls. Port rows contain direction,
  protocol, host address/port, guest address/port, and enabled state. Device
  controls independently toggle guest audio to host speakers, host microphone
  to guest, and host camera to guest. These integrations apply live and report
  rejection or runtime failure without restarting PID 1.
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

### Complete desktop protocol boundary

The filtered display gateway is a complete virtual-monitor and input backend
for the pinned wlroots Wayland backend. It is not a permanently minimal
allowlist that gains ordinary desktop capabilities only after applications
break.

Protocol responsibilities are classified explicitly:

- Guest applications connect only to Sway's normal private session socket.
  Sway provides guest-internal shell, window-management, screencopy,
  accessibility, activation, clipboard, drag-and-drop, input-method, relative
  pointer, pointer constraints, gestures, tablet, touch, presentation, dmabuf,
  explicit-sync, color-management, and Xwayland integration as supported by
  the pinned guest stack. These protocols do not expose the host.
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
  accessing host clipboard or drag-and-drop without an explicit future sharing
  policy, leasing or reprogramming physical outputs, and binding arbitrary
  unclassified host globals.

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

The release protocol inventory is checked against the pinned Sway, wlroots,
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
- Unmodified upstream Sway 1.12, pinned to source commit
  `88869399f421d9180dd8b6ed0b5a1f4a3585d252`, and upstream wlroots 0.20.2,
  pinned to source commit `d783533489e1f75d6886c2ab5c5960090ef268f8`.
  The final image
  contains their licenses but no compositor source or build toolchain.
- Xwayland for legacy X11 applications.
- Wild Buzzard's native Rust desktop shell.
- TryCua Cua Driver running as the interactive user.
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
Wild Buzzard Electron demo, or private wlroots fork. Removing `x11-apps` and
XTerm/UXTerm does not remove Xwayland support. Removing Mesa/Vulkan diagnostic
tools does not remove the graphics runtime or drivers. Users may install or
replace desktop software inside their persistent machine. That cannot alter
the host, but replacing the reference compositor or boot assets may make Wild
Buzzard integration diagnostics fail.

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
- New `.desktop` files installed by the user appear without rebuilding the
  image.

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
opened through the Wild Buzzard Applications menu, desktop shortcuts, an
agent/CUA request, or a terminal intentionally opened by the user.

## Accessibility and in-guest computer use

The guest owns its complete computer-use environment:

```text
private guest D-Bus session
└── at-spi2-registryd
    ├── Wild Buzzard desktop shell
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
- Every operation above remains available when the one native host window is
  covered, unfocused, on another workspace, or minimized.
- Canvas/game/non-accessible surfaces remain operable by screenshot and input
  coordinates even when they expose no useful semantic nodes.
- Testing uses the in-guest CUA and AT-SPI interfaces or developer namespace
  entry. It does not add an SSH server or expose a guest control port to the
  host network.

### Wild Buzzard CUA Driver fork

The repository carries the required Linux driver sources as an auditable fork
of [`trycua/cua`](https://github.com/trycua/cua), pinned to an exact upstream
commit. It is not downloaded unpinned during a release build.

- Preserve the upstream MIT license, copyright notices, source attribution,
  and a machine-readable record of the upstream repository and commit.
- Keep Wild Buzzard modifications identifiable in source and changelog files.
  Do not claim upstream endorsement and do not remove third-party notices.
- Vendor only the packages and transitive source/assets actually required by
  the in-guest Cua Driver and MCP/CLI contract. Record and comply with every
  vendored third-party license; optional components with additional license
  obligations are not silently included.
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
prerequisite and is never installed by Wild Buzzard.

NVIDIA injection is implemented through a pinned, bundled NVIDIA Container
Toolkit/libnvidia-container CDI integration:

- Bundle the release-compatible `nvidia-ctk`, `nvidia-container-cli`, and
  required `libnvidia-container` userspace in the AppImage with their upstream
  licenses and exact versions/checksums. End users must not install NVIDIA
  Container Toolkit, Docker, Podman, or a system CDI service.
- At every start, use the bundled toolkit against the current host driver to
  generate an ephemeral CDI description in Wild Buzzard's private runtime
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
  stable Wild Buzzard application identity. The broker verifies a running,
  correctly targeted `Stream/Input/Audio` node before reporting the bridge
  active, allowing the host shell to expose its standard recording indicator.
- Disabling either input first terminates host capture, then removes the
  internal mapping and guest source. When disabled, the guest has no host
  device node, host PipeWire socket, capture process, reusable stream endpoint,
  or other route to that input.
- The AppImage bundles the client libraries, GStreamer launcher, plugins, and
  bridge code. It uses the already-running desktop PipeWire service in the
  same way as a native application; it never asks the user to install global
  bridge packages.

The guest receives no host filesystem or desktop-service access except:

- its persistent rootfs mounted as `/`;
- portable `shared/` mounted at `/shared`;
- selected GPU/device and matching driver resources;
- the one filtered Wayland connection;
- narrow read-only kernel/runtime mounts required to boot.

Never expose host home, host D-Bus, the host PipeWire socket, host SSH agent, host AT-SPI,
arbitrary `/dev`, Docker/Podman sockets, or the real host Wayland socket.

## Lifecycle

Creation:

1. Validate the machine name and portable paths.
2. Select the verified bundled flat-rootfs seed by default, or resolve, pull,
   and digest-verify an explicitly requested OCI image.
3. Materialize the seed or apply the OCI layers on the same filesystem into a
   staging directory, using the recipient host's full subordinate-ID mapping.
4. Install versioned guest boot/session assets.
5. Atomically rename into `vm/<name>/`.
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
6. Leave every rootfs and `shared/` change intact.

Normal start never repulls the image. Rebase/update is explicit and never
discards local changes without informed user action.

## Release acceptance

A release and implementation handoff are incomplete until automated tests,
an agent-driven real Wayland/GPU hardware run, and visual inspection of the
captured artifacts demonstrate every requirement below. A coding agent must
not stop after compilation, unit tests, process-start checks, or API exit
status while any safe in-scope acceptance scenario remains untested.

- The AppImage runs without separately installed Wild Buzzard helpers.
- The final OCI already contains every managed guest asset and both compiled
  guest executables before the launcher performs any persistent-rootfs
  migration. Its installed manifest, paths, modes, Sway/wlroots pins, CUA
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
  running PipeWire-Pulse recording source-output with Wild Buzzard's application
  ID, selected target, and capture-process PID; this must drive the standard
  host desktop recording/privacy indication and disappear on disable. After
  disablement, attempts from the guest to reconnect to the old endpoint fail
  and no host capture process remains.
- The release AppImage passes the same media tests with host PATH stripped;
  its GStreamer/PipeWire executables, plugins, libraries, and licenses resolve
  only from the mounted AppDir.
- All machine state is beside the AppImage, including `shared/`, never
  `~/.wb`.
- Copying the portable folder needs no metadata rewrite.
- A guest file and installed package survive full stop/start.
- systemd is namespace PID 1 and the interactive user can run services and
  passwordless guest sudo.
- The host receives exactly one machine toplevel.
- Before the container starts, while it boots, while it runs, after it stops,
  and after a failed start, the same native host application remains usable.
- Every host application control is driven in a real Wayland session: titlebar
  drag, four-edge/four-corner resize, minimize, maximize, restore, close,
  Machine menu, Settings menu, Start, Stop, Restart, retry after failure, GPU
  selection, network selection, and initial monitor size. No action leaks a
  click or keystroke to the guest.
- `Machine`, `Ports`, `Devices`, and `Settings` remain in the native header bar
  at every tested size and scale. No separate toolbar or bottom confinement
  banner reduces the embedded guest monitor viewport.
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
- Exercise those operations against the Wild Buzzard shell and real GTK,
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
  - create, rename, copy, move, and delete test files in `Files` and `Shared`,
    and verify `/shared` changes from both guest and portable host folder;
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
- The vendored TryCua fork is pinned, reproducible, carries its MIT license and
  upstream attribution, records local modifications, and passes license
  inventory checks.
- The default network and desktop-service isolation boundaries hold.
- Vulkan/OpenGL render on every selected GPU; NVIDIA CUDA and codec injection
  work when selected.
- NVIDIA acceptance is performed on a host that does not provide
  `nvidia-ctk`, `nvidia-container-cli`, or libnvidia-container through `PATH`
  or runtime linker search paths. The AppImage's pinned bundled toolkit must
  generate and apply its private CDI result successfully.
- Run `nvidia-smi -L` and a compiled CUDA compute probe inside the guest, then
  run the architecture-matching CUDA artifact from
  `openresearchtools/llama-cpp-arm64-builds` release `b10276`. On x86_64 the
  required artifact is
  `llama-b10276-bin-ubuntu-cuda13-x64.tar.gz`, SHA-256
  `4747dd212618ed5eecef318a3538b9e9ee4c3fb2808b226420aa8152a2fe0724`;
  on ARM64 use the corresponding ARM64 CUDA artifact and its published digest.
  Download it once into portable `cache/` for the hardware test only; do not
  bundle llama.cpp or a model in the release AppImage/reference image.
- With a small test GGUF in the dedicated acceptance machine, require
  llama.cpp device enumeration to report the selected NVIDIA GPU, run a real
  `llama-bench` or short inference with all model layers requested on CUDA,
  assert from llama.cpp output that CUDA buffers/layers are active, and
  independently observe the guest process on the selected GPU. CPU-only
  success does not pass. Repeat device isolation for each explicit GPU choice
  and a multi-GPU choice when hardware is present.
- Zero-copy and explicit-sync status is measured and truthfully reported.
