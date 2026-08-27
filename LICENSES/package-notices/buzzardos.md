# Buzzard OS host package notices

This notice covers only the payload of the `buzzardos` host package.

## Shipped by this package

- Buzzard OS host manager, broker, display gateway, desktop metadata, and
  artwork are Copyright (C) 2026 Open Research Tools contributors. Except for
  the AppStream metadata noted below, they are licensed under
  AGPL-3.0-or-later.
- `org.openresearchtools.BuzzardOS.metainfo.xml` is offered under CC0-1.0.
- Locked Rust crates and the Rust standard library are linked into the host
  executables. Their exact versions, license expressions, source checksums,
  and complete retained notice texts are shipped beside this file.
- The unmodified NVIDIA Container Toolkit 1.19.1 helpers used to describe
  explicitly selected NVIDIA GPUs are shipped as separate executables under
  their upstream Apache-2.0/BSD/MIT and conditional LGPL terms. Their pinned
  package notices, Go module inventory, source archives, checksums, and
  retained notice texts are shipped under this package's documentation
  directory. NVIDIA does not sponsor, endorse, or support Buzzard OS.

Buzzard OS does not currently carry a separately forked third-party host
component. The NVIDIA helpers above are unmodified upstream programs, not a
Buzzard fork.

## Not bundled

`bubblewrap`, `buildah`, GTK, GStreamer, PipeWire, `slirp4netns`, `tar`,
`uidmap`, Wayland, XKB data, and the other packages named by the package
manager are installed independently by APT. Their files and license texts are
not copied into the `buzzardos` package; their own package metadata remains
authoritative.

This host notice does not cover a machine image or root filesystem, software
installed inside a machine, or the separately distributed `buzzardos-guest`,
`buzzardos-desktop`, and `buzzardoscua` packages.
