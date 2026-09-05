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
- A package-private, unmodified crun 1.29.1 is built from commit
  `f0d911de5587342cfeb16473bf32ecdfeaf25957`. Its executable is GPL-2.0-or-later;
  libcrun is LGPL-2.1-or-later. The libocispec generator retains GPL-3.0-or-later
  and its parser-skeleton exception, OCI schemas retain Apache-2.0, and the
  embedded portable BLAKE3 code retains CC0-1.0 OR Apache-2.0. Exact recursive
  commits and complete notices are under `crun/`; corresponding source and
  build scripts are in `sources/crun-source.tar.gz` beside this file.

## Not bundled

Podman, Buildah, the host's system crun/runc, networking dependencies, GTK,
GStreamer, PipeWire, Wayland, XKB data, and the other packages named by the
package manager are installed independently by APT. Their files and license
texts are not copied into the `buzzardos` package; their own package metadata
remains authoritative.

This host notice does not cover a machine image or root filesystem, software
installed inside a machine, or the separately distributed `buzzardos-guest`,
`buzzardos-desktop`, and `buzzardoscua` packages.
