# Third-party notices and release obligations

Buzzard OS is Copyright (C) 2026 Open Research Tools contributors and is
licensed under AGPL-3.0-or-later. Third-party components keep their own
licenses and copyright notices; no third-party author or project endorses
Buzzard OS. Machine-readable component and asset records are under
`LICENSES/`.

## Source-vendored components

- **Buzzard CUA** contains an auditable Linux fork of `trycua/cua`,
  tag `cua-driver-rs-v0.17.0`, commit
  `10279552e2bbe479e367a082f78b1b98ee85a697`, under the MIT License. Its
  preserved notice, source record, reviewed reduction notes, and downstream
  changelog are in `cua/`.
- The vendored **virtual-keyboard-unstable-v1** protocol XML preserves its MIT
  copyright and permission notice inline.
- Locked Cargo graphs record registry checksums, license expressions,
  repository URLs, and hashes of shipped license/notice files for the host,
  guest desktop, and Buzzard CUA packages.
- Seven selected Rust packages are MPL-2.0 (`option-ext` and six UniFFI
  packages). Their exact Source Code Form archives are identified by the
  versions and registry checksums in the generated inventories.
- Rust 1.96.0 is pinned by `rust-toolchain.toml`.
  `LICENSES/rust-runtime.toml` records the exact standard-library notice
  bundle which package and OCI audits require.

## Guest-building Containerfiles

- The Containerfiles are recipes; Buzzard OS does not distribute their
  resulting Debian root filesystem or an OCI image.
- **Sway**, **wlroots**, and all other non-Buzzard Debian packages are resolved
  by APT when the person building a machine runs the recipe. Buzzard OS does
  not build or ship a private compositor fork, copy those packages into a
  Buzzard `.deb`, or replace their package-owned notices.
- The optional CUDA recipe downloads NVIDIA packages directly from NVIDIA's
  authenticated repository during the user's build. Buzzard does not convey
  the CUDA or cuBLAS package payloads. Their terms and notices remain those of
  the packages selected by the builder.

## Native Debian packages

- Podman, Buildah, their native OCI runtime and networking dependencies,
  GStreamer, GTK, and PipeWire are normal Debian dependencies. They are not
  copied into or statically repackaged by `buzzardos`; their distro packages
  retain the authoritative copyright and source records.
- OCI lifecycle, pull, build, import, export, networking, devices, CDI, and
  user-namespace behavior use the distribution's stock Podman and Buildah.
  Buzzard OS ships no copied container runtime or downloaded Go executable.
- `buzzardos`, `buzzardos-guest`, `buzzardos-desktop`, and `buzzardoscua` each
  carry only the project, embedded dependency, asset, and upstream-fork
  evidence applicable to that package. Their notice bundles are intentionally
  separate. Sway/wlroots remain owned by their distro packages.

## Current release gates

The statuses in `LICENSES/release-components.toml` are authoritative. The
following gates remain and must not be inferred away:

1. Every candidate `.deb` must be audited as the exact package to be
   published. An earlier successful build is not evidence for a later
   artifact.
2. If Buzzard ever starts distributing a prebuilt machine or OCI archive, that
   becomes a new release surface and requires a separate complete license and
   redistribution review before publication.

`NOASSERTION` means unknown, never public domain or probably compatible. The
licensing audit is evidence collection and is not legal advice.
