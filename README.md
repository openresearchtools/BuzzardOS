# Buzzard OS

Buzzard OS is a rootless manager for persistent Linux desktop machines built
around stock Podman. Each machine boots systemd and a complete stock-Sway
desktop in one persistent Podman container, while the entire guest monitor
remains inside one native host Wayland window.

This is not an ephemeral application container. A machine has one directly
writable external rootfs; guest packages, files, settings, and application
state survive restarts. Podman owns the container lifecycle, namespaces,
cgroups, seccomp, capabilities, networking, ports, devices, CDI, and supported
runtime options. Buzzard owns its manager, native machine window, and the
explicit display, input, media, and one-shot clipboard bridges.

## Debian packages

Buzzard OS is built as four independently versioned packages:

```text
buzzardos_<version>_amd64.deb
buzzardos-guest_<version>_amd64.deb
buzzardos-desktop_<version>_amd64.deb
buzzardoscua_<version>_amd64.deb
```

- `buzzardos` installs the host manager, native Podman adapter, native display
  application, desktop-menu entry, AppStream metadata, icons, and helpers.
- `buzzardos-guest` installs guest mechanics: systemd/session integration,
  clipboard and media integration, AppImage support, and the distribution's
  normal `sway`/wlroots stack.
- `buzzardos-desktop` installs the optional Buzzard OS desktop shell,
  Settings, themes, icons, Thunar integration, and reference applications.
- `buzzardoscua` installs the reviewed, daemonless in-guest `cua`/`cuaN`
  computer-use commands while retaining upstream attribution.

The versions come from [`VERSION`](VERSION),
[`guest/GUEST_VERSION`](guest/GUEST_VERSION),
[`guest/DESKTOP_VERSION`](guest/DESKTOP_VERSION), and
[`cua/VERSION`](cua/VERSION). Version tags publish all four packages to the
Buzzard OS GitHub release, and the signed Open Research Tools APT repository
indexes them for normal installation and upgrades.

`cua/VERSION` is Buzzard CUA's own product version. The TryCua tag and commit
from which its reviewed Linux subset originated are retained only in the CUA
license and provenance records; they do not participate in package updates.

## Per-machine storage

There is no global Buzzard OS storage root. Every create, import, or clone
chooses the complete destination machine directory:

```text
<chosen-machine-directory>/
├── machine.json
├── runtime.json
├── machine.lock
├── cache/
└── rootfs/
```

The small JSON index at `$XDG_CONFIG_HOME/buzzardos/machines.json` records each
machine's UUID, name, and directory. It is only an index. Machine directories
are self-describing, may live on different disks, and can be moved and
registered again.

Sharing is optional. A machine can have zero or more selected host files or
folders, each exposed as a separate entry below `/shared` in that guest. The
GUI has repeatable **Add File**, **Add Folder**, and **Remove** controls. It
does not create or mount a mandatory global shared directory.

## GUI and CLI

Run `buzzardos` from the application menu or a terminal. The native manager
shows every registered machine in a conventional list with Start/Stop,
Settings, and a compact menu for open, export, clone, and confirmed deletion.
**Add Machine** offers an official build with a CUDA-support/Standard variant
dropdown (CUDA recommended), OCI pull, custom Containerfile/Dockerfile build,
and OCI import. Creation always
asks for the exact destination folder and provides optional share pickers.
Built-in recipes consume the three separately distributed Buzzard guest `.deb`
artifacts; they are not embedded in the host package.

Every creation flow and Machine Settings includes one unrestricted **Native
Podman create arguments** field. Buzzard parses normal shell-style quoting into
an argument vector and passes every argument directly to `podman create`; it
does not filter, categorize, translate, or replace the arguments. Leaving the
field blank uses the user's configured rootless Podman defaults. Examples
include:

```text
--userns=keep-id                         # retain the caller's host UID/GID identity
--userns=auto                            # Podman-managed subordinate IDs
--userns=nomap                           # exclude the caller's host identity
--userns=host                            # Podman's host user-namespace mode
--uidmap=0:100000:65536 --gidmap=0:100000:65536
```

These are native Podman modes, not Buzzard implementations. Devices, CDI,
mounts, security options, and any other arguments supported by the installed
Podman version may be supplied in the same field. Definition-changing settings
take effect at the next explicit start or restart; unchanged Start, Stop, and
Restart target the same persistent Podman container.

For hardware rendering, select a device and a native UID mapping that can open
it. One tested Intel configuration on an external LUKS/ext4 `nosuid` drive is:

```text
--userns=keep-id:uid=1000,gid=1000 --device=/dev/dri/renderD128
```

This is an explicit configuration example, not a hidden default or a guarantee
for every GPU. Buzzard's private stock crun mounts the chosen disk directly at
`/`, using reconstructible runtime metadata as the native rootfs anchor. No
machine data is copied into that anchor, no overlay is added to the running
disk, and no mount helper stays resident. Native Podman `exec --user` lookups
use the anchor's metadata; use numeric guest IDs (for example `1000:1000`) when
entering the canonical guest account through Podman. Guest-local account
lookups continue to use the real guest filesystem.

The same operations are fully scriptable. `--machine-dir` is the exact machine
directory, not a global parent:

```sh
buzzardos --machine-dir /data/projects/research-vm create research \
  --image docker.io/example/research:latest \
  --share /data/datasets \
  --share /home/me/notes.txt

buzzardos --machine-dir /fast-disk/imported import ./machine.oci.tar \
  --name imported --mode clone

buzzardos --machine-dir /fast-disk/pulled pull pulled \
  docker.io/openresearchtools/example:latest --keep-oci-archive

buzzardos --machine-dir /fast-disk/local-build build local-build \
  --context ./my-image --file Containerfile \
  --podman-arguments '--userns=keep-id'

buzzardos start research
buzzardos stop research
buzzardos status research
buzzardos list

buzzardos --machine-dir /moved/research-vm register
buzzardos unregister research
```

Import accepts a local OCI image-layout directory, tar/gzip/zstd OCI archive,
Buzzard OS export, or remote OCI reference. OCI indexes with multiple matching
images require `--manifest`. Restore retains a Buzzard OS export's portable
host metadata identity, while every exported rootfs has its guest machine ID,
random seed, and any stale SSH host keys cleared in a private copy. The source
machine is never modified. Clone also assigns a fresh host metadata identity
before committing the destination. Pulled and built install media is discarded by default;
`--keep-oci-archive` retains a verified OCI archive in the machine's cache.

Podman owns persistent container definitions, pull, import, export, save/load,
and runtime inspection. Podman or Buildah owns Containerfile builds. Buzzard
orchestrates those native operations, materializes the selected image once into
the chosen external flat rootfs, and atomically commits the machine directory.
Temporary Podman objects are removed after completion. Export requires a
stopped, exclusively locked machine and excludes runtime mounts and configured
host shares. Buzzard contains no second OCI parser, layer extractor, namespace
runtime, network controller, or container-security policy.

## Guest desktop

The repository distributes a Containerfile recipe for building a Debian-family
guest containing systemd, distro Sway and wlroots, Xwayland, private D-Bus and
PipeWire services, AT-SPI, Buzzard OS Guest Desktop, and Buzzard CUA. Buzzard OS
does not distribute the resulting Debian rootfs or OCI image. Debian and other
non-Buzzard packages are obtained by the builder from their own repositories
and retain their own package licenses. The four preinstalled general
applications selected by the recipe are Firefox ESR, Thunar, Mousepad, and
Foot. Guest applications may also use native Type-2 AppImages; Buzzard OS
itself is not an AppImage.

All guest windows remain inside one nested Sway output. Guest applications
cannot create host windows, enumerate or capture the host desktop, observe
host-global input, or access the host clipboard. Clipboard transfer happens
only after one of the two explicit host actions is clicked.

## Local development

The broad Ubuntu 24.04 development image is stored under the gitignored
`build/podman-dev/` data-disk directory. It uses rootless Podman, passwordless
sudo inside the build container, and applies no CPU, memory, or PID limit:

```sh
./build/podman-dev/build-image.sh
./build/podman-dev/run.sh buzzardos-build test
./build/podman-dev/run.sh buzzardos-build debs
./build/podman-dev/run.sh buzzardos-build oci
```

The direct package entry point is:

```sh
BUZZARDOS_DEB_OUTPUT_DIR=/path/on/data-disk/debs \
  packaging/build-debs.sh all
```

The OCI build consumes the locally built guest `.deb` files and installs stock
Sway/wlroots with APT:

```sh
./oci/build-local.sh
```

Generated Cargo targets, packages, OCI archives, downloads, screenshots, and
other build artifacts remain outside the tracked source tree.

## Supported hosts

The host package is built on Ubuntu 24.04 and install-tested on Ubuntu 24.04,
Debian 13, and Ubuntu 26.04. Runtime prerequisites include a Wayland session,
stock rootless Podman and Buildah, and working permissions for any selected GPU
or media devices. Podman owns namespaces, UID/GID mappings, container security,
networking, ports, devices, CDI, and lifecycle. Buzzard OS does not replace or
weaken those native policies and installs no privileged daemon, custom setuid
helper, kernel module, or LXC dependency.

## Licensing

Project-authored code is AGPL-3.0-or-later. Dependencies and the Buzzard CUA
fork retain their own notices, source provenance, and attribution under
[`LICENSES/`](LICENSES/) and the corresponding package documentation paths.
