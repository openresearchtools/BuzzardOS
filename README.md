# Wild Buzzard

Wild Buzzard is a portable, rootless Linux desktop-machine launcher packaged
as one self-contained AppImage. It shares the host Linux kernel, but gives each
machine a persistent operating-system rootfs, systemd PID 1, private desktop
session, processes, network namespace, D-Bus, accessibility tree, audio
session, and selected GPU devices.

```text
WildBuzzard-x86_64.AppImage
  -> persistent flat OCI rootfs (pulled once, no overlay)
  -> rootless namespaces with systemd as guest PID 1
  -> stock Sway/wlroots + Wild Buzzard's classic Rust desktop shell
  -> private D-Bus + AT-SPI + PipeWire + Xwayland + TryCua
  -> the complete guest desktop in one native host Wayland window
```

There is no KWin, Plasma shell, XFCE shell, labwc, Waybar, Fuzzel, patched
compositor, private wlroots fork, per-application host-window forwarding,
PipeWire display stream, VNC, or bundled Blender.

## Persistent and portable

The OCI image is downloaded and flattened once. Every later launch boots the
same directly writable rootfs. Packages installed with `apt`, user files,
agents, applications, services, and desktop settings survive stop/start.

All files live beside the AppImage:

```text
portable-folder/
├── WildBuzzard-x86_64.AppImage
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

The host-owned `Machine` menu reports the running state and provides GPU
passthrough selection, the initial guest-monitor size, maximize/restore, and
orderly machine shutdown. GPU, initial-size, and desktop-scale changes are
saved in portable `machine.json` and clearly take effect on the next start.
Double-clicking the AppImage starts the machine; closing either the host frame
or `Shut Down Machine` powers the guest off cleanly before the window
disappears.

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
auto-activation is removed from the reference session, and Chromium uses its
guest-local basic password store while retaining its complete accessibility
tree. The reference image includes Noto Core, CJK, and Color Emoji fonts so
multilingual Unicode text renders as glyphs rather than missing-character
boxes.

The agent does not see the surrounding host desktop. Host screencopy, host
virtual input, host AT-SPI, host D-Bus, host PipeWire, and unrelated host
windows are not exposed. Development tests enter the namespace directly; no
SSH server or host-network control port is added.

The interactive user has passwordless sudo inside the rootless machine and can
install arbitrary packages and agents. Guest root is never host root.

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

Creating pulls and extracts once. Starting does not repull. Updating or rebasing
is a separate explicit operation and never silently discards local changes.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
./scripts/build-appimage.sh
./scripts/hardware-acceptance.sh ./dist/WildBuzzard-x86_64.AppImage acceptance
```

[`AGENTS.md`](AGENTS.md) is the complete authoritative product and release
acceptance specification.
