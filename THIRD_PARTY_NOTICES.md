# Third-party notices and release obligations

Buzzard OS is Copyright (C) 2026 Open Research Tools contributors and is
licensed under AGPL-3.0-or-later. Third-party components keep
their own licenses and copyright notices; no third-party author or project
endorses Buzzard OS. The machine-readable component and asset records are
in `LICENSES/`. The project copyright declaration is also preserved in
`NOTICE`.

## Source-vendored components

- **Cua Driver 0.17.0** is an auditable fork of `trycua/cua`, tag
  `cua-driver-rs-v0.17.0`, commit
  `10279552e2bbe479e367a082f78b1b98ee85a697`, under the MIT License.  Its
  preserved notice, source record, reviewed Linux-only inventory, and
  downstream changelog are in `guest/third_party/trycua-cua/`. The inventory
  is regression-tested against the selected Cargo packages, package license
  metadata, Linux skill files, and removed platform-only paths.
- **Inter** is bundled by the Cua Driver cursor overlay under OFL-1.1.  The
  copyright and complete license are preserved beside `Inter.ttf`.
- The vendored **virtual-keyboard-unstable-v1** protocol XML preserves its
  MIT copyright and permission notice inline.
- The exact Linux release Cargo graphs are recorded separately from the
  vendored fork itself.  Registry checksums, license expressions, repository
  URLs, and the hashes of shipped license/notice files must match the locked
  inventories before release.
- Seven selected Rust packages are MPL-2.0 (`option-ext` and six UniFFI
  packages).  Their exact Source Code Form is identified by the crate version
  and registry checksum in the generated inventories.  A release must tell
  recipients how to obtain that exact source and keep a durable source
  availability procedure; reproducing the MPL text alone is not enough.
- Cargo inventories do not cover the **Rust standard library** or compiler
  runtime linked into each executable. Rust 1.96.0 is pinned by
  `rust-toolchain.toml`; `LICENSES/rust-runtime.toml` records the exact
  `COPYRIGHT-library.html` size and checksum. Host-app and OCI builds install
  that verified notice bundle and their artifact audits require it.

## Reference OCI image

- **Sway 1.12** (commit
  `88869399f421d9180dd8b6ed0b5a1f4a3585d252`) and **wlroots 0.20.2**
  (commit `d783533489e1f75d6886c2ab5c5960090ef268f8`) are built from unmodified
  upstream source under the MIT License.  Their licenses and the source record
  are installed under `/usr/share/doc/wildbuzzard-sway/` in the guest.
- Debian packages retain their package copyright files under
  `/usr/share/doc/`.  The build pins the Debian amd64 base manifest and resolves
  build-time Debian dependencies from the dated snapshots recorded in
  `oci/base-images.lock.toml`.  The installed package inventory remains
  release-specific, and the finished machine intentionally restores the live
  `sid` repository so its owner can update the persistent guest normally.
- NVIDIA CUDA Runtime 13.1.80-1 and cuBLAS 13.2.2.2-1 use the NVIDIA CUDA EULA
  plus bundled third-party notices.  Their complete package copyright/EULA
  files must remain in the guest.  Inclusion of the libraries in NVIDIA's
  redistributable-file list does not replace a project-level review that Wild
  OS's method of redistribution satisfies all SDK distribution terms.

## Native portable host application

- **crane 0.21.8**, from go-containerregistry commit
  `2ea098f4b13456cd628460632760b0a74b7488e9`, is Apache-2.0 licensed.  Its Go
  build information identifies eleven dependency modules.  Their exact module
  versions, Go sums, canonical module-archive checksums, and preserved
  Apache/MIT/BSD notices are recorded in `LICENSES/crane-dependencies.toml`.
- The static Go helpers were compiled with **Go 1.26.5** (`crane`) and
  **Go 1.26.3** (`nvidia-ctk` and `nvidia-cdi-hook`). The byte-identical root
  Go BSD license and patent grant are preserved under
  `LICENSES/upstream/go-runtime/`. Every license, copying, notice, and patent
  file in the checksum-pinned official source archives is also bundled, as
  recorded by `LICENSES/go-runtime.toml` and
  `LICENSES/go-source-archives.tsv`.
- **slirp4netns 1.3.3-1** is the checksum-pinned Ubuntu dynamic package built
  from upstream commit `944fa94090e1fd1312232cbc0e6b43585553d824`. Its exact
  package copyright file, signed source descriptor, upstream source archive,
  and Debian packaging archive are bundled in every portable archive. Dynamic linking
  replaces the former static helper, so modified LGPL libraries can be used
  without relinking Buzzard OS; the exact shared-library closure remains
  covered by the host-application artifact gate.
- **NVIDIA Container Toolkit/libnvidia-container 1.19.1-1** is extracted from
  three checksum-pinned upstream Debian packages. Their Apache-2.0,
  BSD-3-Clause, MIT, and conditional LGPL notices are copied under
  `/usr/share/doc/` in the extracted host application. Go build information in `nvidia-ctk` and
  `nvidia-cdi-hook` identifies 23 dependency modules; their exact versions,
  source archives, license expressions, and notices are recorded and shipped
  as specified by `LICENSES/nvidia-go-dependencies.toml`.
- `bubblewrap`, `unshare`, GStreamer, PipeWire/PulseAudio clients and plugins,
  and linuxdeploy's transitive shared libraries come from the build host.
  Their exact binary package versions and Debian copyright files must be
  inventoried from the built AppDir.

## Current release gates

The machine-readable statuses in `LICENSES/release-components.toml` are
authoritative. Resolved historical findings are not repeated here as current
blockers. In particular, the project notice and asset authorship records, MPL
Source Code Form delivery, pinned Rust notice bundle, Go source-and-notice
delivery, dynamic slirp4netns source obligations, NVIDIA Go dependency
evidence, and the portable host dependency closure are recorded and
structurally audited.

The following gates remain and must not be inferred away:

1. Public redistribution of the NVIDIA CUDA Runtime and cuBLAS payloads still
   requires a project-level review against the NVIDIA CUDA EULA. Their pinned
   package checksums, installed EULA files, and appearance in NVIDIA's
   redistributable-file lists are necessary evidence, but do not by themselves
   establish that Buzzard OS's general-purpose persistent guest satisfies
   every SDK distribution condition.
2. Every candidate OCI archive must record and audit its exact manifest digest,
   compressed size, installed package/version inventory, and package copyright
   closure. Every candidate portable host app must likewise pass the final
   extracted-app audit, including the exact build-ID-to-package mapping and
   copyright file for its host-derived ELF closure. A successful earlier local
   build is not evidence for a later artifact.

The structural and artifact gates intentionally fail when any corresponding
machine-readable status remains unresolved or required evidence is absent.
`NOASSERTION` means "unknown", never "public domain" or "probably compatible".
