# Buzzard OS: authoritative product specification

This file is the source of truth for contributors and coding agents working in
this repository. Implementation, tests, documentation, packaging, and design
decisions must preserve it. Buzzard OS is unreleased; there is no legacy
runtime or machine format to preserve.

The approved guest Settings, desktop-file, AppImage, update, theming, sound,
scaling, shell, workspace, and CUA behavior remains defined in
`docs/GUEST_SETTINGS_DESKTOP_INTEGRATION_PLAN.md`. Runtime changes must keep
that behavior intact and update both documents when their boundary changes.

## Product

Buzzard OS is a rootless, persistent Linux desktop-machine manager installed
on Debian-family hosts through APT. Every machine has one persistent rootless
Podman container and one persistent, external, flat, directly writable rootfs
in the exact directory selected by the user.

Buzzard OS is not an ephemeral application container:

- the external rootfs is the durable machine disk, not a disposable layer;
- normal operation uses no overlay and never boots an OCI archive;
- guest packages, configuration, files, and desktop state survive stop/start;
- guest systemd is container PID 1 and supervises the complete desktop;
- stock distro Sway/wlroots composites the guest into exactly one native host
  Buzzard OS window;
- guest applications never become separate host surfaces; and
- the nested compositor remains the display, input, screenshot, and
  accessibility boundary.

Human-facing identities use `Buzzard OS`. New executable, application,
package, D-Bus, theme, diagnostic, and runtime identities use Buzzard naming.

For local side-by-side testing, the explicitly requested reversible host build
identity `buzzardos-pod` is supported. It must not conflict with the installed
`buzzardos` host package, registry, runtime paths, or desktop identity. Guest
components keep their normal identities. See `docs/PODMAN_SIDE_BY_SIDE_BUILD.md`.

## One runtime: native rootless Podman

Podman owns the machine container, namespaces, cgroups, seccomp, capabilities,
networking, ports, devices, CDI, and supported runtime flags. Buildah or Podman
builds Containerfiles. Buzzard OS does not implement a second container
runtime.

The final source tree contains no Bubblewrap backend, custom namespace
construction, slirp4netns controller, custom cgroup controller, copied NVIDIA
toolkit, private CDI generator, legacy runtime mode, compatibility branch,
transition shim, fallback backend, commented-out replacement, or dead code.

Buzzard OS delegates isolation policy to native rootless Podman, no more and no
less:

- it supplies no Buzzard-authored seccomp, capability, AppArmor, SELinux,
  `no-new-privileges`, privileged-mode, or syscall policy;
- it never adds `--privileged` or weakens Podman defaults automatically;
- the user's normal Podman configuration remains effective;
- a single unrestricted custom Podman-arguments field accepts arbitrary
  arguments without filtering, rewriting, categorizing, or policy UI;
- custom arguments are parsed into argv and passed directly to Podman without
  a shell; and
- tests assert the exact argv and inspect the stored Podman configuration so
  hidden security flags cannot appear.

Buzzard OS does not hard-code a user-namespace mode. With no custom namespace
argument, Podman's configured rootless default applies. The unrestricted
arguments field supports Podman's native `--userns` modes (including `host`,
`keep-id`, `auto`, and `nomap`) and explicit `--uidmap`/`--gidmap` arguments.
Buzzard neither emulates these modes nor rewrites one into another. Rootfs
materialization and every later container definition use the same effective
native Podman mapping selected for that machine, so same-host-UID and fully
subordinate-ID machines are both supported without a second mapping layer.
Buzzard does not force Podman's `rootfs:idmap` extension: native rootless
Podman and the host kernel/filesystem decide whether an explicitly requested
idmapped mount is supported.

Each ordinary lifecycle action targets the existing persistent container:

```text
Start    -> podman start
Stop     -> podman stop
Restart  -> podman restart
```

Ordinary start or restart never recreates the container. Settings whose Podman
definition cannot be updated in place are saved as pending intent. On the next
explicit lifecycle boundary, and only when the effective definition changed,
Buzzard replaces the stopped Podman container definition while preserving the
same external rootfs and machine identity. With no changed definition,
`podman start`, `podman stop`, and `podman restart` are direct calls.

The Podman container name and ID are reconstructible runtime state. The
self-describing machine metadata and external rootfs remain authoritative.
Removing or rebuilding Podman metadata must never remove or rewrite the
machine rootfs.

## Repository and package boundaries

```text
host/                 manager, Podman runtime adapter, native display, bridges
guest/                buzzardos-guest mechanics and integration runtime
desktop/              optional official buzzardos-desktop environment
cua/                  buzzardoscua source and upstream attribution
packaging/            reproducible Debian package assembly
oci/                  reference Containerfiles and one-time setup
tests/acceptance/     machine, session, hardware, and CUA journeys
tools/                verification and licensing gates
LICENSES/             package and third-party licensing evidence
```

Host and guest Cargo workspaces remain independently locked. Host source never
enters the OCI build context. Guest outputs enter reference images only through
built `.deb` packages.

The build emits exactly four independently versioned packages:

```text
buzzardos_<version>_<arch>.deb
buzzardos-guest_<version>_<arch>.deb
buzzardos-desktop_<version>_<arch>.deb
buzzardoscua_<version>_<arch>.deb
```

`buzzardos` contains the manager GUI/CLI, Podman adapter, native display,
filtered Wayland gateway, media and clipboard bridges, metadata, icons, and
diagnostics. It declares distro Podman and Buildah dependencies and does not
bundle their binaries. It installs no privileged Buzzard daemon, system
service, setuid binary, copied container runtime, copied NVIDIA toolkit, or
Buzzard AppArmor policy. Upgrade and uninstall never delete machines.

`buzzardos-guest` contains desktop-independent session, systemd, stock-Sway,
output/scale/keyboard synchronization, clipboard agent, AppImage integration,
private D-Bus/AT-SPI/PipeWire/WirePlumber/portal glue, and guest halves of the
typed Buzzard integrations. Native sudo inside the Podman container is the
distro sudo; there is no Buzzard sudo transport or privilege bridge.

`buzzardos-desktop` owns only the official shell, Settings, themes, icons,
Thunar integration, application defaults, and desktop assets.

`buzzardoscua` remains the separate daemonless Rust CLI with preserved upstream
MIT attribution and AGPL terms for Buzzard work. It contains no resident
daemon, MCP server, recording, telemetry, browser specialization, or host
automation surface.

Every package carries Debian copyright material and exact dependency notices
for what it ships. Distro-provided Podman, Buildah, conmon, crun/runc,
Netavark/Aardvark or pasta, seccomp policy, CDI support, and their dependencies
remain independently installed host packages whose own package records are
authoritative. License inventories and tests must contain no notice for a
removed bundled helper.

## Machine storage and metadata

Every create, pull, import, and clone requires an exact user-selected
directory:

```text
<machine-directory>/
├── machine.json
├── runtime.json
├── machine.lock
├── cache/
│   └── source.oci.tar       # only when explicitly retained
└── rootfs/
```

`machine.json` is durable intent. It stores the machine UUID, display name,
source, external-rootfs contract, desired Podman definition, display settings,
shares, port mappings, devices, media choices, and unrestricted custom Podman
argv. `runtime.json` is bounded reconstructible current state. There is no
migration code for prior unreleased schemas.

The registry at `$XDG_CONFIG_HOME/buzzardos/machines.json` stores only UUID,
name, and exact machine directory. No rootfs lives in Podman image storage or a
hidden Buzzard storage directory. Moving a stopped machine and re-registering
it may replace its Podman definition but requires no rootfs ownership rewrite.

Shares are ordinary Podman bind mounts. The GUI supports repeatable Add File,
Add Folder, Remove, and read-only/read-write controls. They mount at distinct
validated names below `/shared`. Podman performs the mount; Buzzard adds no
parallel share mechanism.

## OCI and Containerfile operations

Podman/Buildah owns registry authentication, pull, build, import, export, save,
load, layer handling, digest validation, and supported transports. Buzzard
orchestrates those native operations and performs atomic machine-directory
commit; it does not contain a second OCI parser or layer extractor.

Supported creation paths are:

- build the Standard or CUDA-capable reference Containerfile;
- build a user-selected Containerfile/Dockerfile;
- pull an OCI image reference;
- import a Podman-supported local archive or image source; and
- clone or restore a Buzzard machine export.

Creation materializes the selected image once into the new external flat
rootfs, creates the persistent Podman container definition against that rootfs,
then atomically registers the machine. It never replaces an existing
destination. Temporary Podman objects are removed after success or failure.
Retaining source install media is optional and never required to run.

Export requires a stopped machine and exclusive lock. It uses Podman-native
filesystem/image operations, excludes runtime mounts and shares, commits the
output atomically, and never mutates the source rootfs. Clone creates a new
machine UUID and clears destination-local guest identity in private staging.
Restore rejects duplicate machine identity. Imported ports and devices start
disabled.

The reference Containerfiles install `buzzardos-guest`,
`buzzardos-desktop`, and `buzzardoscua` through the signed Open Research Tools
APT repository and invoke the one-time setup command during image construction.
Package installation or guest boot never reruns provisioning. The final image
retains authenticated APT sources and indexes so normal guest APT updates work.

## Native manager and machine window

The manager preserves the finalized native GTK4 design and is the only machine
settings editor. Every GUI action calls the same typed operation used by the
CLI.

The manager list provides Start/Stop, Settings, Export, Clone, and confirmed
Delete. Add Machine exposes Standard, CUDA-capable, custom Containerfile,
remote OCI pull, and import flows with native file/folder pickers and explicit
destination selection.

Manager Settings contains:

- immutable machine location with Open Folder;
- initial display size and guest scale;
- Podman network mode and port mappings;
- GPU/CDI selection;
- file and folder shares;
- complete audio, microphone, and camera switches;
- Automatic plus current detected-device dropdowns for audio output,
  microphone, and camera; and
- one unrestricted custom Podman-arguments field.

Settings that alter Podman's stored container definition clearly say they
apply at the next start/restart. Media endpoints remain Buzzard-owned
integrations but their configuration lives in manager Settings.

The machine window contains only these direct controls:

- one state-dependent Start/Stop button;
- Restart;
- Settings, which opens this machine's page in the existing manager window;
- Copy to Host; and
- Copy to VM.

It contains no duplicate settings, ports, devices, media, lifecycle menus, or
secondary settings window. A complete stop or orderly guest poweroff closes
the machine window. Restart keeps the window and reconnects the same display
boundary.

## Display, input, integrations, and guest desktop

`buzzardos-display` remains one native GTK4 host Wayland application and one
filtered gateway. It owns exactly one host toplevel and embeds the one fixed
guest Sway output. Extra CUA/manual outputs remain active guest-internal
off-screen outputs and never create host surfaces.

Podman receives only the exact private Unix endpoints and state mounts required
for the selected integrations. It never receives the real host Wayland,
PipeWire, D-Bus, AT-SPI, clipboard, credential, or agent sockets. Buzzard's
display/input/media/clipboard code remains outside Podman's frame and input
paths except for those private endpoints.

The gateway retains resizing, fractional scale, monitor transitions, dmabuf,
explicit synchronization, hidden-window pacing, keyboard state balancing,
pointer/touch/tablet translation, and one-host-window guarantees already
covered by tests and the guest integration plan.

Host and guest clipboards remain separate. The window exposes exactly the two
one-shot actions Copy to VM and Copy to Host. Existing bounded MIME, image,
nonce, deadline, validation, and no-persistence behavior remains unchanged.

Audio output, microphone, and camera remain independent Buzzard bridges using
the selected host devices and private guest endpoints. The host PipeWire socket
is never mounted into the container. Microphone/camera authorization and host
privacy indication remain explicit.

All guest shell, workspace, taskbar, AppImage, Settings, Thunar, theme, CUA,
and accessibility behavior in
`docs/GUEST_SETTINGS_DESKTOP_INTEGRATION_PLAN.md` remains unchanged unless this
specification explicitly changes its host/runtime boundary.

## Lifecycle and state

Start acquires the machine lock, reconciles a changed stopped definition when
necessary, starts the existing Podman container, launches/reuses the native
window and private integration endpoints, and reports Running only after
systemd/Sway/runtime readiness.

Stop calls Podman's stop operation, waits for the container to stop, tears down
only Buzzard's ephemeral integration state, preserves the rootfs/container
definition, records Stopped, and closes the machine window.

Restart calls Podman's restart operation when the definition is unchanged. If
saved settings require a new Podman definition, restart performs the one
stopped definition replacement and starts it with the same rootfs. It never
rebuilds, repulls, or reprovisions the machine.

Runtime state is derived from `podman inspect` and Podman events/status, not a
stale host process PID. Closing a machine window requests orderly stop; an
unexpected display disconnect marks the current window Failed.

## Required verification

Completion requires evidence for all of the following:

- no Bubblewrap/custom namespace/slirp/CDI/sudo-transport source, package,
  test, notice, or dependency remains;
- exact generated argv contains no hidden security policy and custom argv is
  forwarded byte-for-byte as parsed, without a shell;
- ordinary start/stop/restart targets the same persistent Podman container;
- changed definition settings replace only the stopped definition and preserve
  rootfs contents and identity;
- Standard, CUDA-capable, and custom Containerfiles build through
  Podman/Buildah;
- pull/import/export/clone operate through Podman and survive complete
  stop/start cycles;
- exact selected machine directories and external drives work without moving
  rootfs state into Podman storage;
- native Podman networking, port publishing, devices, GPU/CDI, and arbitrary
  supported custom flags work;
- manager and machine-window controls match the exact UI contract above;
- display, resize, scale, input, CUA seats/outputs, clipboard, audio,
  microphone, and camera acceptance journeys pass;
- four independently versioned Debian packages build, install, upgrade, and
  carry correct current licensing; and
- APT-installed host package and APT-built reference images pass on Ubuntu
  24.04 LTS, Debian 13, and Ubuntu 26.04 test environments, with real host
  Wayland/GPU acceptance where hardware is required.

Compilation, unit tests, command exit status, or a narrow mock alone is not
release evidence.
