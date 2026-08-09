# Wild Buzzard

Wild Buzzard is a portable, rootless Linux desktop-machine launcher packaged
as one self-contained AppImage. It shares the host Linux kernel, but gives each
machine a persistent operating-system rootfs, systemd PID 1, private desktop
session, processes, network namespace, D-Bus, accessibility tree, audio
session, and selected GPU devices.

```text
WildBuzzard-x86_64.AppImage
  -> verified flat rootfs seed (or an explicit OCI source), materialized once
  -> persistent directly writable rootfs (no overlay)
  -> rootless namespaces with systemd as guest PID 1
  -> stock Sway/wlroots + Wild Buzzard's classic Rust desktop shell
  -> private D-Bus + AT-SPI + PipeWire + Xwayland + TryCua
  -> the complete guest desktop in one native host Wayland window
```

There is no KWin, Plasma shell, XFCE shell, labwc, Waybar, Fuzzel, patched
compositor, private wlroots fork, per-application host-window forwarding,
PipeWire display stream, VNC, or bundled Blender.

## Persistent and portable

The complete download includes an already-flattened, compressed rootfs seed.
First machine creation verifies and materializes it once; an explicit OCI
reference can instead be pulled and flattened once. Every later launch boots
the same directly writable rootfs. Packages installed with `apt`, user files,
agents, applications, services, and desktop settings survive stop/start.

All files live beside the AppImage:

```text
portable-folder/
├── WildBuzzard-x86_64.AppImage
├── runtime/               # complete-download seed; not the running rootfs
│   ├── WildBuzzard-rootfs-linux-x86_64.tar.zst
│   └── WildBuzzard-rootfs-linux-x86_64.json
├── vm/
│   └── <machine-name>/
│       ├── machine.json
│       ├── runtime.json
│       ├── machine.lock
│       └── rootfs/
├── shared/                # mounted read/write at /shared in every guest
└── cache/                 # disposable downloads and OCI staging
```

Wild Buzzard does not use `~/.wb`, XDG machine storage, Docker/Podman storage,
or hidden host state. Metadata does not store the folder's original absolute
path. Moving or copying the folder moves the AppImage, machines, shared files,
installed software, and agent state together.

When packaged, storage is derived from the original `$APPIMAGE` location,
never the transient `/tmp/.mount_*` mount. `--storage-dir PATH` explicitly
chooses another portable folder.

## Guest desktop

Stock, unmodified Sway 1.12 is the guest wlroots compositor. The reference
configuration places ordinary Wayland and Xwayland applications in floating
containers to provide a classic desktop instead of a tiling workflow. Window
geometry, focus, state, and exact-container actions come from Sway's private
in-guest IPC tree; there is no titlebar overlay or host-window forwarding.

Wild Buzzard's native Rust shell supplies a deliberately simple classic
XFCE/Openbox-style desktop:

- one desktop, with no numbered workspace buttons;
- `Files` and `Shared` desktop shortcuts;
- a compact bottom taskbar;
- an `Applications` button with no duplicate pinned application launchers;
- one aligned task button per running application, with paging when needed;
- no duplicate minimize/maximize/close buttons in the taskbar;
- a compact vertical Applications menu with real application icons, each
  installed application listed once, and no full-screen tile grid;
- automatic discovery of newly installed `.desktop` entries; and
- a separate, clearly labeled `Shut Down Machine` menu item.

The `Shared` shortcut opens `/shared`, backed by the sibling host `shared/`
folder.

The outer frame belongs to the host machine window. Its drag, edge resize,
host minimize/maximize/restore, close, and machine settings are separate from
guest application controls. Resizing the outer window changes Sway's virtual
monitor, so the guest re-lays out at the exact new logical and physical
resolution instead of stretching an image. The final Sway dmabuf always
matches the host viewport's native physical pixels. Guest desktop density is a
separate setting: Follow Host, 100%, 125%, 150%, 175%, or 200%.

The host-owned `Machine` control reports the running state and provides Start,
Stop, Restart, orderly Shut Down, and exit/close actions. The native window
controls own host maximize/restore. `Settings` owns the initial guest-monitor
size, network mode, explicit GPU selection (including `all`), desktop density,
and diagnostics. Settings that change namespace or device construction are
saved in portable `machine.json` and clearly require a machine restart.
Double-clicking the AppImage starts the machine; closing either the host frame
or `Shut Down Machine` powers the guest off cleanly before the window
disappears.

## Live ports and media

The native title/header bar contains `Machine`, `Ports`, `Devices`, and
`Settings` directly; there is no separate toolbar or bottom status banner. The
embedded monitor receives all remaining vertical space.

The host-owned `Ports` menu adds and removes TCP or UDP mappings in either
direction while the machine is running. `Host → Guest` publishes a chosen
guest service on a chosen host address and port. `Guest → Host` exposes only
one chosen host destination through a private guest listener. Applying a
change does not restart the machine or change its namespace PID.

The host-owned `Devices` menu independently controls guest audio output, host
microphone input, and host camera input. A normal Wayland desktop already runs
the per-user PipeWire service used by native applications such as browsers and
screen recorders. Wild Buzzard connects to that existing session and bundles
its own PipeWire/GStreamer client libraries, plugins, scanner, launcher, and
bridge code in the AppImage. The user installs no PipeWire bridge package,
creates no service, and edits no configuration file.

Microphone and camera access is off by default. Turning one on creates only its
machine-private capture bridge and guest PipeWire source. Capture is continuous
for the enabled interval, and the native Wild Buzzard header labels every
active input. Microphone capture also registers with the host desktop audio
session as an explicit `Wild Buzzard Microphone` recording stream, so GNOME and
compatible shells show their normal microphone/privacy indicator. Wild Buzzard
refuses to report the microphone active if that tracked host recording stream
cannot be observed, continuously rechecks the stream while sharing remains
enabled, and terminates an unaccounted bridge before retrying it. The media
helper is bound to the broker's lifetime and Wild Buzzard never bypasses host
accounting through direct ALSA capture. Turning it off kills
the host capture process, removes the relay, and removes the guest source. The
host PipeWire socket is never mounted into the guest. A host without a usable
PipeWire session can still run the desktop machine, but enabling a media bridge
returns a precise unsupported-host diagnostic.

## Full computer use inside the guest

```text
systemd (namespace PID 1)
└── interactive user session
    ├── private D-Bus
    │   └── private AT-SPI registry
    │       ├── Wild Buzzard desktop shell
    │       ├── GTK applications
    │       ├── Qt/KDE applications
    │       ├── Electron/Chromium applications
    │       └── in-guest agents
    ├── private PipeWire and WirePlumber
    ├── Sway/wlroots computer-use protocols
    ├── TryCua Cua Driver
    └── Xwayland
```

An agent installed inside the machine can screenshot the complete guest
display at its native physical dmabuf dimensions, click, drag, scroll, type,
use hotkeys, and inspect or invoke the guest's AT-SPI tree. Screenshots and
absolute input share that physical coordinate space; guest logical mode and
scale are reported separately. Canvas and game surfaces remain operable through
screenshot coordinates even when they expose no semantic accessibility nodes.
The shell exposes the complete installed-app list and every running guest
window semantically even when the human-facing menu scrolls or the taskbar
pages, so agents can launch or focus them directly.

The guest has one continuously live Sway scanout. When the native host window
is visible, host Wayland frame callbacks pace presentation at the monitor's
vblank. When it is minimized, covered, or on another workspace, an internal
refresh clock keeps that same guest scanout alive for applications, CUA input,
AT-SPI, and guest-only screenshots. Hidden frames are not reported as
host-presented or vblank-synchronized; the latest dmabuf is presented when the
host window becomes visible again.

The pinned Wild Buzzard CUA fork is cross-compiled to the guest glibc baseline
and carried in the AppImage as a managed guest asset. New and existing
persistent machines therefore use the audited driver without a runtime
download or a separately installed host helper.

Qt/KDE application support does not enable a KDE Wallet prompt. KWallet
auto-activation is removed from the reference session. The reference image
includes Noto Core, CJK, and Color Emoji fonts so
multilingual Unicode text renders as glyphs rather than missing-character
boxes.

The agent does not see the surrounding host desktop. Host screencopy, host
virtual input, host AT-SPI, host D-Bus, host PipeWire, and unrelated host
windows are not exposed. Development tests enter the namespace directly; no
SSH server or host-network control port is added.

The interactive user has passwordless sudo inside the rootless machine and can
install arbitrary packages and agents. Guest root is never host root.

Native Type-2 AppImages run inside the guest without extraction flags. The
reference image carries the conventional Electron runtime libraries,
`libfuse.so.2`, FUSE 3 utilities, and the narrowly exposed `/dev/fuse` device.
An in-guest watcher adds only the owner execute bit to genuine AppImage ELF
files arriving in `/home/wildbuzzard` or `/shared`; ordinary files and symlinks
are not authorized. Because the persistent rootfs remains `nosuid` and the
whole guest inherits Linux `no_new_privs`, a scoped guest-root broker performs
only libfuse's mount-helper exchange. It does not grant `CAP_SYS_ADMIN` to the
AppImage or its application tree.

## Display and isolation

`wildbuzzard-display` is the only component connected to the real host Wayland
socket. The guest compositor receives a private, filtered connection that
allows one nested output, input, dmabuf, synchronization, and presentation
protocols. The gateway rejects extra host windows and blocks the guest from
controlling the outer host toplevel.

`wildbuzzard-broker` creates the user, PID, mount, network, IPC, UTS, and cgroup
namespaces; mounts the rootfs as `/`; mounts portable `shared/` at `/shared`;
injects selected GPU resources; starts systemd; and supervises shutdown.

The guest does not receive host home, the real host Wayland socket, host D-Bus,
host PipeWire, host SSH agent, Docker/Podman sockets, arbitrary devices, or host
AT-SPI. Default networking uses a private namespace and bundled user-mode
networking with host loopback disabled.

Rootless namespaces share the host kernel and selected GPU kernel driver. They
are not a hard boundary against a kernel or GPU-driver vulnerability.

## GPU and presentation

The intended presentation path is:

```text
guest Vulkan/OpenGL application
  -> dmabuf-backed allocation
  -> Sway/wlroots composition
  -> dmabuf fd + modifier + explicit fence
  -> host Wayland compositor
  -> host-vblank presentation
```

Wild Buzzard reports zero-copy only when the active renderer, device identity,
dmabuf format/modifier, synchronization, native-resolution buffer, and host
presentation result have been measured. Dmabuf protocol support alone is not
called zero-copy. Any conversion, copy, scale, or presentation fallback is
reported.

The reference image pins the official Sway 1.12 and wlroots 0.20.2 source
commits. They are built in an isolated image stage with the GLES2 and Vulkan
renderers, Xwayland, dmabuf, explicit synchronization, presentation, and color
management support enabled. No compiler or compositor source is retained in
the final machine.

The clean reference desktop preinstalls only Firefox ESR, the customized
Thunar file manager, Mousepad, and Foot as general user-facing applications;
`ffmpeg` remains available as a runtime/codec utility. Chromium, Dolphin,
Pavucontrol, `x11-apps`, XTerm/UXTerm, Mesa/Vulkan diagnostic applications,
Blender, and any Wild Buzzard Electron demo are absent. Xwayland, Mesa/Vulkan
runtime support, generic Electron/AppImage support, PipeWire, CUA, and the
desktop integration remain included.

`--gpu all` selects every supported DRM and NVIDIA GPU. A DRM render node,
index, PCI identifier, or NVIDIA UUID can restrict selection. Matching host
Vulkan, OpenGL, EGL, GLX, CUDA, NVENC, and NVDEC userspace is injected
ephemerally; the host kernel driver remains a prerequisite.

## Self-contained AppImage

The release AppImage bundles the launcher, broker, display gateway, OCI
pull/extraction functionality, namespace helpers, user-mode networking, and GPU
integration. End users do not install Docker, Podman, an OCI runtime, `skopeo`,
`umoci`, `crane`, `bubblewrap`, `slirp4netns`, or NVIDIA Container Toolkit.

Host prerequisites are:

- a Linux kernel with the required unprivileged namespace/mount features;
- subordinate UID/GID ranges and trusted `newuidmap`/`newgidmap`;
- a Wayland desktop session;
- working GPU kernel drivers and device permissions; and
- for optional audio, microphone, or camera sharing, the normal per-user host
  PipeWire session and permission to the explicitly enabled input;
- AppImage execution, with extract-and-run when FUSE is unavailable.

Wild Buzzard installs no permanent daemon, setuid Wild Buzzard helper, system
service, or host package.

## Use

Double-click the AppImage to open the configured machine. Explicit commands are
also available:

```sh
./WildBuzzard-x86_64.AppImage doctor
./WildBuzzard-x86_64.AppImage create machine-1 --gpu all
./WildBuzzard-x86_64.AppImage start machine-1
./WildBuzzard-x86_64.AppImage status machine-1
./WildBuzzard-x86_64.AppImage stop machine-1
./WildBuzzard-x86_64.AppImage list
```

Without `--image`, creation uses the complete bundle's verified local seed.
`create NAME --image IMAGE_REFERENCE` instead pulls and flattens that explicit
OCI image. Either creation path runs once; starting never repulls. Updating or
rebasing is a separate explicit operation and never silently discards local
changes. A standalone AppImage can update an existing portable folder; a new
folder needs the complete bundle or an explicit OCI image reference.

## Development

The repository is split along its actual deployment boundaries:

```text
host/                    native launcher, broker, display, AppImage packaging
guest/                   shell, boot/session assets, CUA fork, guest installer
oci/                     Debian reference-image assembly and local Compose build
tests/acceptance/        real session, GPU, media, CUA, and visual journeys
tools/                   local source tests and dependency/license audit
LICENSES/                machine-readable release-component evidence
```

The host and guest are separate Cargo workspaces and have independent lock
files. OCI assembly consumes only `guest/` outputs and pinned compositor
sources; it does not compile or copy host runtime code. The AppImage build does
consume the guest shell/CUA payload because it migrates those managed assets
into existing persistent machines.

All generated targets, AppDirs, image archives, and acceptance artifacts are
placed under `${TMPDIR:-/tmp}/wildbuzzard-build-$(id -u)` by default, outside
the checkout. Override `WILDBUZZARD_BUILD_ROOT` or the component-specific
output variables when needed.

```sh
./tools/test-local.sh
./host/build-appimage.sh
./oci/build-local.sh

WILDBUZZARD_ELECTRON_APPIMAGE=/path/to/LM-Studio-x64.AppImage \
  WILDBUZZARD_ACCEPT_FULL_MATRIX=1 \
  ./tests/acceptance/hardware-acceptance.sh \
  /tmp/wildbuzzard-build-$(id -u)/out/WildBuzzard-x86_64.AppImage acceptance
```

`oci/build-local.sh` uses `oci/compose.yaml`, verifies the complete installed
guest contract, and keeps the image local. Set `WILDBUZZARD_EXPORT_ARCHIVE=1`
to export and checksum a compressed Docker archive outside the repository.
`host/build-appimage.sh` likewise creates no GitHub release by itself.

## Distribution assembly

Distribution builds run on disposable GitHub-hosted x86-64 runners through the
manually dispatched `Build release assets` workflow. They are not assembled on
a developer workstation. The workflow builds the reference OCI only inside
the runner, verifies it, flattens it into a compressed persistent-rootfs seed,
and discards the OCI intermediate. It does not use GHCR, publish a container
package, or authenticate to any registry for upload.

The workflow produces two primary files:

- `WildBuzzard-x86_64.AppImage` — the independently replaceable host
  application.
- `WildBuzzard-portable-x86_64.tar.zst` — the complete first-install folder,
  containing the same AppImage, the flat-rootfs seed, empty portable machine
  and sharing directories, checksums, provenance, and license evidence.

The runner validates the complete file by using the final AppImage to create a
temporary machine from the bundled seed through the real subordinate-ID path,
then compares its full content, metadata, and translated ownership with the
canonical flattened rootfs before upload.

Licensing is grouped by distribution boundary: AppImage/host notices and
source evidence are separate from guest/rootfs notices and source evidence.
The default `artifacts` mode retains both outputs as short-lived Actions
artifacts and cannot publish a Release. The `prerelease` and `release` modes
require explicit confirmation, an existing SemVer tag pointing at the selected
commit, and the strict license gate. Only the final publisher has permission to
create a GitHub Release; the mutually exclusive prerelease and production
publishers use separately protectable GitHub environments and do not execute
files from the checked-out build commit with their write token; only pinned
actions and the workflow's fixed inline publication commands run there. They
upload the two files above—not an OCI image. Assembly also fails early if
either primary file is not smaller than GitHub Releases' 2 GiB per-asset
limit.

Native Electron acceptance uses the official LM Studio AppImage as an external
test input. It is copied into `/shared` as mode `0644`, must become executable
through the generic guest policy, and is launched directly—never through
`--appimage-extract-and-run`. The vendor binary is not committed or bundled in
the reference image.

The live-integration hardware test selects and exercises real host microphone
and camera backends. It verifies TCP and UDP in both directions, physical
audio/video payloads, host-session recording registration, off-state
revocation, bridge recovery, and unchanged namespace PID. Synthetic sources
may cover deterministic unit/CI behavior but never count as hardware or
release acceptance. Full hardware acceptance runs
the physical test by default; set `WILDBUZZARD_ACCEPT_INTEGRATIONS=0` only on an
intentionally media-less CI worker.

[`AGENTS.md`](AGENTS.md) is the complete authoritative product and release
acceptance specification.
