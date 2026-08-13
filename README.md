# Buzzard OS

Buzzard OS is a rootless, persistent Linux desktop-machine launcher. It boots
systemd and a complete Sway desktop inside Linux namespaces while presenting
the whole guest monitor in one native host Wayland window.

It is intentionally closer to a portable desktop VM than an ephemeral
application container:

- each machine has one flat mutable rootfs that persists across restarts;
- `apt install`, user files, application state, and desktop settings survive;
- the guest has its own systemd, D-Bus, PipeWire, AT-SPI, Xwayland, and CUA
  services;
- every guest window stays inside one nested Sway output;
- the host application owns its normal titlebar, machine controls, devices,
  ports, settings, and explicit one-shot clipboard actions.

## Portable distribution

The Linux download is one high-compression archive:

`BuzzardOS-portable-linux-x86_64.tar.xz`

Extract it and run `BuzzardOS/BuzzardOS`. Buzzard OS itself is not an AppImage
and does not use FUSE. Its layout follows ordinary extracted applications such
as Blender. Its OCI exporter uses a pinned GLIBC-2.31-compatible GNU tar
runtime rather than inheriting the build machine's newer tar ABI:

```text
BuzzardOS/
├── BuzzardOS                         executable entry point
├── Install-Dependencies              Debian/Ubuntu uidmap setup
├── app/
│   ├── AppRun                        internal dependency environment
│   ├── usr/bin/                      launcher, broker, display
│   ├── usr/lib/                      private native libraries
│   ├── usr/libexec/                  bundled helpers
│   ├── runtime/
│   │   ├── default-rootfs.oci.tar.zst
│   │   └── default-rootfs.oci.json
│   ├── licenses/{host,guest}/
│   └── provenance/
├── Machines/
└── shared/
```

All paths are resolved relative to the top-level `BuzzardOS` executable. No
machine state is written to `~/.wb`, XDG data directories, Docker/Podman
storage, or a system directory. Copying the entire folder moves the install
media, machines, and shared data together.

On first launch, the digest-verified OCI seed is imported once into
`Machines/default/rootfs/`. The compressed seed is install media; it is not the
running filesystem and no overlay is used. `shared/` is mounted read/write at
`/shared` in every machine and remains ordinary host-owned storage.

## Host prerequisites

Buzzard OS bundles its application dependencies and helpers. A supported host
needs:

- x86-64 Linux with unprivileged user, PID, mount, IPC, UTS, network, and
  cgroup namespaces;
- a host Wayland session;
- configured subordinate UID/GID ranges and the distribution's trusted
  `newuidmap`/`newgidmap` authorization helpers;
- working host GPU drivers and permissions for selected devices;
- a normal host PipeWire session only for optional audio, microphone, and
  camera integration.

On Debian or Ubuntu, `./Install-Dependencies` installs `uidmap` and verifies
the authorization helpers and subordinate ranges. On Ubuntu systems enforcing
AppArmor's unprivileged-user-namespace policy, it also installs the distro's
root-owned, AppArmor-profiled `lxc-usernsexec` entry point; Buzzard OS uses
that executable only to establish its explicit UID/GID map and never uses LXC
services, storage, networking, or machine management. It does not disable the
host's AppArmor policy. Buzzard OS remains rootless and installs no daemon,
service, setuid helper, kernel module, or package of its own.

## Machines and OCI exchange

Running `./BuzzardOS` opens the native machine manager. The first launch creates
the bundled `default` machine. The manager can Create, Import, Export, Clone,
Start, Stop, and Delete machines. Every running machine gets a separate native
window and exclusive lock; separate launcher instances can run different
machines simultaneously.

The same operations are available from the command line:

```sh
./BuzzardOS
./BuzzardOS create work
./BuzzardOS start work
./BuzzardOS stop work
./BuzzardOS import SOURCE --name restored --mode restore
./BuzzardOS import SOURCE --name independent-copy --mode clone
./BuzzardOS export work --output shared/work.oci.tar.zst
./BuzzardOS clone work work-copy
./BuzzardOS delete work-copy --yes
./BuzzardOS list
./BuzzardOS doctor
```

Import accepts a local OCI image-layout directory, a tar/gzip/zstd OCI archive,
a Buzzard OS export, or a remote OCI reference. A multi-platform/multi-image
local layout requires an unambiguous native Linux entry or `--manifest`
selection. `--mode restore` preserves the identity carried by a Buzzard OS
export and rejects a duplicate in the same portable folder. `--mode clone`
regenerates the host metadata UUID and removes the guest machine ID, random
seed, and SSH host keys while the new machine is still private staging; a
failed reset never commits a partially cloned machine. On first boot, guest
init creates the new machine ID and asks the distro `ssh-keygen`, when present,
to create only missing default host keys before systemd starts. Generic OCI
images have no portable Buzzard OS identity annotation and therefore always
receive fresh local identity.
Layers are applied in order with OCI whiteouts and preserved ownership, modes,
times, links, xattrs, ACLs, and file capabilities.

Authenticated OCI environment values and descriptive process metadata are
retained and round-trip through export. Buzzard OS is a desktop-machine
runtime, so an imported image must provide systemd and always boots systemd as
PID 1; an image's foreground `Entrypoint`, `Cmd`, `User`, or `WorkingDir` is
preserved as OCI metadata rather than replacing the machine boot contract.

Export requires a stopped, exclusively locked machine. Buzzard OS enters the
same subordinate-ID namespace used by that machine, archives the canonical
guest IDs directly, and writes a standards-compliant OCI layout containing a
config, manifest, index, content-addressed blobs, and portable machine
annotation. It excludes `/shared` and ephemeral mounts. Importing the export on
another host remaps canonical guest IDs to that host's ranges. Clone preserves
the filesystem but resets machine identity and host keys for first-boot
regeneration.

Docker and Podman are not runtime dependencies. They are used only by
developers and the disposable Actions runner to build the reference image from
`oci/desktop/Containerfile`. Buzzard OS implements import and export itself.

## Guest desktop

The reference Debian guest contains unmodified pinned Sway/wlroots, systemd,
private D-Bus and PipeWire services, Xwayland, AT-SPI, and the Linux Wayland CUA
driver. The shipped general applications are Firefox ESR, customized Thunar,
Mousepad, and Foot. Users can install other Debian packages and native Type-2
AppImages inside their persistent machine.

The lightweight classic shell provides one desktop, desktop shortcuts, an
adaptive Applications menu, a bottom taskbar, one task button per running
application, Show Desktop, compositor-owned move/resize/minimize/maximize/close
operations, and accessible AT-SPI actions. Guest Settings contains only:

- Display scaling;
- output and microphone volume/mute;
- keyboard language/layout/hardware model;
- Light/Dark appearance plus solid background colour;
- Debian package updates;
- time and installed IANA time zone.

## Isolation and explicit sharing

Guest applications see only the nested Sway display and private guest services.
They cannot enumerate or capture the host desktop, observe host-global input,
create host windows, or access the host clipboard.

The host header exposes exactly two one-shot clipboard actions: send the
current host clipboard snapshot to the guest, or copy one requested guest
snapshot to the host. Text and bounded still images are validated and copied;
the clipboards are never synchronized and no persistent guest-to-host clipboard
capability exists.

Ports, custom shares, audio output, microphone input, camera input, and GPU
devices are separately host-authorized capabilities. The default network is
private user-mode networking. A host bind on `127.0.0.1` remains local; an
explicit `0.0.0.0` bind warns before exposing the port to the LAN.

## Building locally

OCI development entry points never publish an image:

```sh
./oci/build-local.sh
```

`host/build-portable-app.sh` builds the dependency-complete `app/` payload
outside the repository. It requires the pinned compositor runtime artifact via
`WILDBUZZARD_GUEST_RUNTIME_PAYLOAD`. `tools/build-release-rootfs.sh` builds and
verifies the OCI seed. `tools/assemble-release-assets.sh` creates the final
`tar.xz` archive with maximum multithreaded xz compression.

Generated Cargo targets, OCI layouts, downloaded applications, screenshots,
and release artifacts stay outside the source tree.

## GitHub Actions

`.github/workflows/build-release-assets.yml` is manual and artifact-only. It
has read-only repository permission, uses a disposable local Docker builder,
never pushes an OCI image, and never creates a Release, tag, environment,
Package, or registry object. It uploads exactly one short-lived Actions
artifact named `BuzzardOS-portable-linux-x86_64.tar.xz`, containing that
archive and `BuzzardOS-portable-linux-x86_64.tar.xz.sha256`.

## Licensing

Project-authored code is AGPL-3.0-or-later. Bundled dependencies keep their own
licenses and corresponding-source/provenance records. The portable archive
separates native host notices under `app/licenses/host/` from guest/rootfs
notices under `app/licenses/guest/` and binds them to the exact source commit,
OCI seed, package inventories, and payload hashes under `app/provenance/`.
