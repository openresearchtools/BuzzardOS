# Release licensing records

This directory is the release-compliance source of truth for Buzzard OS. It
complements the project-level `LICENSE` and the copyright records shipped by
Debian packages.

The maintained binary surfaces are the four Buzzard OS `.deb` packages and the
reference OCI image assembled from the three guest packages. The host package
declares Buildah and its other host tools as distro dependencies; it does not
copy Crane, Skopeo, or another downloaded OCI client into its payload. The OCI
image installs stock Sway/wlroots and all other normal Debian dependencies from
the pinned distro snapshot.

`release-components.toml` records direct non-Cargo inputs.
`package-inputs.toml` mirrors the ordered APT and direct-download blocks in the
reference Containerfile. `nvidia-go-dependencies.toml` and `go-runtime.toml`
record the dependency closure of separately reviewed NVIDIA helper artifacts.
`rust-runtime.toml` records compiler-runtime licensing that does not appear in
Cargo metadata.

The generated Cargo inventories and consolidated notice text come from the
locked Linux release graphs. Registry checksums come from each `Cargo.lock`;
license expressions and repository locations come from `cargo metadata`; and
shipped notice files are hashed from checksum-verified crate sources.

For Debian-family payloads, the authoritative per-package notice is
`/usr/share/doc/<package>/copyright`. A candidate release audit must inspect the
exact four `.deb` files and the exact OCI package/version inventory rather than
inferring the transitive closure from a Containerfile.

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
or missing notices. The current known publication blocker is the required
distribution-rights review for the proprietary CUDA runtime payload.

The manually dispatched workflow builds and checks four `.deb` files and a
disposable reference OCI image. It has no publishing authority and uploads only
the `.deb` artifacts for inspection. A later publication workflow requires its
own reviewed signing, approval, and strict artifact gates.

The audit is evidence collection, not legal advice.
