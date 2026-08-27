# Buzzard OS packaging handoff

## Decided architecture

Buzzard OS is a new Debian-packaged application. There is no released portable
folder, AppImage, old registry, or rootfs migration contract to preserve.

The deployment boundary is four binary packages:

- `buzzardos`: host manager, broker, native display, helpers, icons, desktop
  entry, and AppStream metadata;
- `buzzardos-guest`: guest session, integration agents, systemd assets, and
  stock Sway/wlroots mechanics;
- `buzzardos-desktop`: optional shell, Settings, themes, icons, applications,
  and Thunar integration; and
- `buzzardoscua`: the separately versioned in-guest computer-use service.

The four version files are `VERSION`, `guest/GUEST_VERSION`,
`guest/DESKTOP_VERSION`, and `cua/VERSION`. Package updates will
eventually be served from a separately designed signed APT repository. The
current workflow does not publish packages or an OCI image.

## Machine storage

Each create, import, or clone receives one exact destination machine directory.
Its `machine.json`, runtime state, lock, cache, and flat mutable `rootfs/` live
together. Machines may be spread across different disks.

`$XDG_CONFIG_HOME/buzzardos/machines.json` is a private atomic JSON index of
machine UUID, display name, and absolute directory. It is not a database or
the source of machine state. A moved self-describing directory can be
registered again with `buzzardos --machine-dir PATH register`.

Shares are opt-in per machine. Metadata contains zero or more validated host
regular-file or real-directory paths. The broker creates a private `/shared`
and mounts only those entries. The manager has repeatable Add File, Add Folder,
and Remove controls; the CLI has repeatable `--share PATH` flags.

## Reference guest

`oci/desktop/Containerfile` and `Containerfile.cuda` are distributed in the
host package as guest-building recipes. The manager copies the selected recipe
and three separately supplied Buzzard guest package artifacts into a temporary
Buildah context; it does not bundle those guest packages into the host package.
A future release recipe can instead install those same packages from the
Buzzard APT repository. It assembles a Debian-snapshot rootfs on the builder's
machine.
Buzzard does not publish that resulting Debian rootfs or an OCI image. Package
compilation is separate from the recipe. Sway and wlroots come exclusively
from the Debian package set. No Sway/wlroots source checkout, Meson build,
private fork, or compositor toolchain is present.

The host launcher does not inject source assets into an existing OCI input or
running machine. Built-in recipe creation is an explicit Buildah build step;
pulled and imported OCI input must already contain the installed guest packages
and systemd desktop contract. This keeps guest updates under dpkg/APT ownership.

## Developer environment

The ignored `build/podman-dev/` directory contains the rootless Podman service,
graphroot, caches, persistent checkout, and outputs on the data disk. Its Ubuntu
24.04 image includes the broad native/Rust/Node/Python/container build toolset
and passwordless sudo. Runs have no explicit CPU, memory, or PID limit.

Use only one `run.sh` invocation at a time because each invocation refreshes
the shared persistent checkout:

```sh
./build/podman-dev/run.sh buzzardos-build test
./build/podman-dev/run.sh buzzardos-build debs
./build/podman-dev/run.sh buzzardos-build oci
```

## Acceptance targets

Before a package revision is handed off:

1. run Rust format, Clippy, unit, Python contract, shell syntax, and structural
   licensing checks;
2. build and inspect all four `.deb` files;
3. time a cold local Containerfile build and run `oci/verify-image.sh`; and
4. install the host package on each supported host and all three guest packages
   in the reference guest; verify versions, stock Sway, desktop files, and
   package ownership on the existing Ubuntu 24.04, Debian 13, and Ubuntu 26.04
   amd64 VMs.

## Next publication work

APT publication is intentionally future work. It needs repository layout,
Release/InRelease signing, offline/key-rotation procedure, channels, rollback,
retention, CI approval, tag/commit validation, and least-privilege upload
credentials. None of that authority belongs to the current artifact-only
workflow.
