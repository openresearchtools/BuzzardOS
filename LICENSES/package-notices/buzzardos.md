# Buzzard OS host package notices

This notice covers only the payload of the `buzzardos` host package.

## Shipped by this package

- Buzzard OS host manager, Podman adapter, display gateway, desktop metadata, and
  artwork are Copyright (C) 2026 Open Research Tools contributors. Except for
  the AppStream metadata noted below, they are licensed under
  AGPL-3.0-or-later.
- `org.openresearchtools.BuzzardOS.metainfo.xml` is offered under CC0-1.0.
- Locked Rust crates and the Rust standard library are linked into the host
  executables. Their exact versions, license expressions, source checksums,
  and complete retained notice texts are shipped beside this file.
Buzzard OS does not currently carry a separately forked third-party host
component.

## Not bundled

Podman, Buildah, their native OCI runtime and networking dependencies, GTK,
GStreamer, PipeWire, Wayland, XKB data, and the other packages named by the
package manager are installed independently by APT. Their files and license
texts are not copied into the `buzzardos` package; their own package metadata
remains authoritative.

This host notice does not cover a machine image or root filesystem, software
installed inside a machine, or the separately distributed `buzzardos-guest`,
`buzzardos-desktop`, and `buzzardoscua` packages.
