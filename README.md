# Buzzard OS

Buzzard OS is a rootless manager for persistent Linux desktop machines. Each
machine boots systemd and a complete stock-Sway desktop inside Linux
namespaces, while the entire guest monitor remains inside one native host
Wayland window.

This is not an ephemeral application container. A machine has one directly
writable rootfs; guest packages, files, settings, and application state survive
restarts. The host owns lifecycle, window chrome, ports, devices, and explicit
one-shot clipboard transfers.

## Debian packages

Buzzard OS is built as four independently versioned packages:

```text
buzzardos_<version>_amd64.deb
buzzardos-guest_<version>_amd64.deb
buzzardos-desktop_<version>_amd64.deb
buzzardoscua_<version>_amd64.deb
```

- `buzzardos` installs the host manager, broker, native display application,
  desktop-menu entry, AppStream metadata, icons, and helpers.
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

The same operations are fully scriptable. `--machine-dir` is the exact machine
directory, not a global parent:

```sh
buzzardos --machine-dir /data/projects/research-vm create research \
  --image docker.io/example/research:latest \
  --share /data/datasets \
  --share /home/me/notes.txt

buzzardos --machine-dir /fast-disk/imported import ./machine.oci.tar.zst \
  --name imported --mode clone

buzzardos --machine-dir /fast-disk/pulled pull pulled \
  docker.io/openresearchtools/example:latest --keep-oci-archive

buzzardos --machine-dir /fast-disk/local-build build local-build \
  --context ./my-image --file Containerfile

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

Buildah is the only end-user OCI tool dependency. Pull and build use isolated
temporary Buildah storage beside the selected destination, never the user's
normal Buildah cache; builds use `--no-cache`, and temporary images/layers are
deleted after rootfs import. Export requires a stopped, exclusively locked
machine. It emits an identity-free canonical
OCI archive with numeric guest IDs, hardlinks, symlinks, modes, timestamps,
xattrs, ACLs, capabilities, and sparse files. Runtime mounts and configured
host shares are excluded. Docker and Podman are not end-user dependencies.

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
unprivileged namespaces, configured subordinate UID/GID ranges, the distro
`newuidmap`/`newgidmap` gates, and working permissions for any selected GPU or
media devices. Buzzard OS installs no privileged daemon, custom setuid helper,
kernel module, or LXC dependency.

## Licensing

Project-authored code is AGPL-3.0-or-later. Dependencies and the Buzzard CUA
fork retain their own notices, source provenance, and attribution under
[`LICENSES/`](LICENSES/) and the corresponding package documentation paths.
