# Third-party notices and release obligations

Buzzard OS is Copyright (C) 2026 Open Research Tools contributors and is
licensed under AGPL-3.0-or-later. Third-party components keep their own
licenses and copyright notices; no third-party author or project endorses
Buzzard OS. Machine-readable component and asset records are under
`LICENSES/`.

## Source-vendored components

- **Buzzard CUA 0.17.0+buzzard1** is an auditable Linux fork of `trycua/cua`,
  tag `cua-driver-rs-v0.17.0`, commit
  `10279552e2bbe479e367a082f78b1b98ee85a697`, under the MIT License. Its
  preserved notice, source record, reviewed Linux inventory, and downstream
  changelog are in `guest/third_party/trycua-cua/`.
- **Inter** is bundled by the Buzzard CUA cursor overlay under OFL-1.1. Its
  complete license is preserved beside the source asset.
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

## Reference OCI image

- **Sway** and **wlroots** are installed only from the pinned Debian snapshot.
  Buzzard OS does not build or ship a private compositor fork. Exact package
  versions appear in the generated OCI inventory and distro copyright notices
  remain under `/usr/share/doc/`.
- All other Debian packages likewise retain their package copyright files.
  The image pins the Debian amd64 base manifest and resolves build-time
  packages from dated snapshots. The finished persistent guest restores the
  live Debian repository so its owner can request normal updates.
- NVIDIA CUDA Runtime 13.1.80-1 and cuBLAS 13.2.2.2-1 use the NVIDIA CUDA EULA
  plus bundled third-party notices. Their package copyright/EULA files must
  remain in the guest. Inclusion in NVIDIA's redistributable-file list does
  not replace a project-level distribution-rights review.

## Native Debian packages

- Host runtime tools such as `bubblewrap`, `skopeo`, `slirp4netns`, `uidmap`,
  GStreamer, GTK, and PipeWire are normal Debian dependencies. They are not
  copied into or statically repackaged by `buzzardos`; their distro packages
  retain the authoritative copyright and source records.
- The OCI command adapter installed by `buzzardos` is project-authored shell
  code which delegates only the required inspect/copy operations to distro
  `skopeo`. No Crane or other downloaded Go executable is shipped.
- `buzzardos-guest-desktop` and `buzzardcua` carry the project, dependency, and
  upstream-fork evidence for their own payloads. Sway/wlroots remain owned by
  their distro packages rather than either Buzzard OS package.

## Current release gates

The statuses in `LICENSES/release-components.toml` are authoritative. The
following gates remain and must not be inferred away:

1. Public redistribution of the CUDA Runtime and cuBLAS payloads needs a
   project-level review against the NVIDIA CUDA EULA. Checksums, installed
   EULA files, and redistributable-file-list entries are evidence, not the
   conclusion of that review.
2. Every candidate OCI archive must audit its exact manifest, size, installed
   package/version inventory, and package copyright closure. Every candidate
   `.deb` must be audited as the exact package to be published. An earlier
   successful build is not evidence for a later artifact.

`NOASSERTION` means unknown, never public domain or probably compatible. The
licensing audit is evidence collection and is not legal advice.
