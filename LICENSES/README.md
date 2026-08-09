# Release licensing records

This directory is the release-compliance source of truth for Wild Buzzard.
It complements, and does not replace, the project-level `LICENSE` file or any
license/copyright notice embedded in a third-party source or binary package.

The records distinguish three different distribution surfaces:

1. the source repository, including the audited TryCua fork and guest assets;
2. the native AppImage, including Rust dependencies, downloaded helpers, and
   the shared-library closure copied from the build host; and
3. the reference OCI image, including Debian/NVIDIA packages and the
   source-built Sway/wlroots and Cua Driver binaries.

`release-components.toml` records direct, non-Cargo inputs.  Checksums are for
the exact downloaded artifact where the build has one.  A source commit is
recorded separately because a release-asset checksum alone is not a
corresponding-source record.

`package-inputs.toml` mirrors every ordered apt-install block in the OCI
Containerfile and records the AppImage's build-host payload owners.
`crane-dependencies.toml`, `nvidia-go-dependencies.toml`,
`go-runtime.toml`, and `appimage-runtime-dependencies.toml` expose dependency
closures that would
otherwise be hidden inside downloaded ELF binaries.  In particular, a
top-level helper license is not evidence for statically linked or Go-module
dependencies.  The Go record preserves the exact root license and patent grant
for the compiler releases reported by the helper binaries and ships their
checksum-pinned source archives together with every discovered license, notice,
and patent file.  The crane record separately inventories and ships the exact
license/notice closure for every module present in its build information.
`rust-runtime.toml` records the standard library and compiler runtime linked
into Rust binaries; those components do not appear in Cargo metadata.

`guest-assets.toml` records the provenance decision for repository-owned visual
and configuration assets.  `NOASSERTION` is intentional: it is a release
blocker until the author supplies provenance and a license; it must not be
silently converted into a guessed license.

The generated Cargo inventories and consolidated license text are produced by
the licensing checker from the locked Linux release graphs.  Registry package
checksums come from each `Cargo.lock`; license expressions and repositories
come from `cargo metadata`; shipped license/notice files are hashed from the
checksum-verified crate source.  A crate that omits its license text requires a
commit-pinned fallback record.

For Debian-family payloads, the authoritative per-binary-package notice is the
package's `/usr/share/doc/<package>/copyright` file (including a valid Debian
doc-directory symlink).  A release audit must enumerate the *built* AppDir and
OCI rootfs; the Containerfile and AppImage script alone cannot describe the
transitive package closure.

`generated/oci-packages.tsv` is the exact, sorted binary-package/version
inventory from the locally built reference image named in
`release-components.toml`.  The structural gate validates its count and hash;
the OCI artifact gate independently reconstructs the list from dpkg status and
requires an exact match.  A later reference-image build must deliberately
replace this record and its content-addressed image evidence.

The audit is evidence collection, not legal advice.  In particular, the
project still needs a documented corresponding-source delivery policy for
copyleft binaries and a distribution-rights review for proprietary CUDA
runtime packages.

## Release gate

Regenerate deterministic Cargo evidence after an intentional lockfile change:

```sh
tools/check-licenses.sh --generate --structural
```

The normal structural gate fails on every recorded unresolved release blocker:

```sh
tools/check-licenses.sh
```

Artifact checks are additional and always release-failing.  Run them on the
exact outputs that will be distributed, on the build host while its dpkg
ownership database still matches the copied ELF closure:

```sh
tools/check-licenses.sh --appdir /path/to/WildBuzzard.AppDir
tools/check-licenses.sh --appimage /path/to/WildBuzzard-x86_64.AppImage
tools/check-licenses.sh --guest-rootfs /path/to/extracted/rootfs
```

`--structural` suppresses only the explicitly recorded policy/provenance
blockers; it never suppresses stale generated evidence, a checksum mismatch,
an unclassified asset, or an artifact missing a required notice.
