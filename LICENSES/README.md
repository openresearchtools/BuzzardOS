# Release licensing records

This directory is the release-compliance source of truth for Buzzard OS. It
complements the project-level `LICENSE` and the copyright records shipped by
Debian packages.

The maintained binary surfaces are the four Buzzard OS `.deb` packages. The
distributed guest Containerfiles are build recipes, not prebuilt machine
images. A person running a recipe obtains Debian, Sway/wlroots, CUDA when
selected, and all other non-Buzzard packages from their respective package
repositories; those packages are not bundled into a Buzzard `.deb`.

Each Buzzard package has an independent copyright record, third-party notice,
locked Cargo inventory, and retained Rust notice bundle. Host, guest mechanics,
desktop, and CUA notices must not be combined. The host About window exposes
only the host package's embedded material and clearly excludes machine and
guest-package licensing.

`release-components.toml` records direct non-Cargo inputs.
`package-inputs.toml` mirrors the ordered APT and direct-download blocks used
when locally verifying the Containerfiles. The Standard and CUDA recipes have
separate exact package inventories because the CUDA choice intentionally adds
NVIDIA's independently installed package closure.
`rust-runtime.toml` records compiler-runtime licensing that does not appear in
Cargo metadata.

The generated Cargo inventories and consolidated notice text come from the
locked Linux release graphs. Registry checksums come from each `Cargo.lock`;
license expressions and repository locations come from `cargo metadata`; and
shipped notice files are hashed from checksum-verified crate sources.

For Debian-format Buzzard packages, the authoritative per-package notice is
`/usr/share/doc/<package>/copyright`. A candidate release audit must inspect the
exact four `.deb` files. An optional local machine-build audit may inspect the
resulting package/version inventory, but that local rootfs is not a Buzzard
release artifact.

Regenerate deterministic Cargo evidence after an intentional lockfile change:

```sh
tools/check-licenses.sh --generate --structural
```

The normal gate fails on every unresolved policy or provenance blocker:

```sh
tools/check-licenses.sh
```

`--structural` suppresses only explicitly recorded policy blockers. It never
suppresses stale generated evidence, checksum mismatches, unclassified assets,
or missing notices. CUDA packages named by the optional recipe are downloaded
from NVIDIA by the person building the machine; Buzzard does not redistribute
their payload. The recipe retains NVIDIA's checksum-pinned MIT notice for the
repository keyring and the installed CUDA EULA for the CUDA libraries
metapackage because those two upstream packages omit Debian `copyright` files.

The manually dispatched workflow builds and checks four `.deb` files and may
build a disposable local machine for acceptance testing. It has no OCI
publishing authority and uploads only the `.deb` artifacts for inspection. A
later APT publication workflow requires its own reviewed signing, approval, and
strict artifact gates.

The audit is evidence collection, not legal advice.
