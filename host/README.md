# Native host application

This directory is the native host package boundary:

- `crates/buzzardos` implements the `buzzardos` CLI, registry, lifecycle,
  and OCI import/export;
- `crates/buzzardos-broker` constructs and supervises namespaces, devices,
  networking, shares, ports, and media bridges;
- `crates/buzzardos-display` owns the native machine and manager windows; and
- `crates/wb-core` contains shared machine, registry, and protocol contracts.

The historical crate directory names are source-internal. Installed binaries,
desktop metadata, windows, diagnostics, and other human-facing identifiers use
Buzzard OS naming.

The host and guest are separate Cargo workspaces. The host package never
builds, embeds, migrates, or overwrites the guest package payload. A created or
imported OCI rootfs must already satisfy the Buzzard OS guest contract.

Build all four Debian packages with:

```sh
BUZZARDOS_DEB_OUTPUT_DIR=/path/on/data-disk/debs \
  ./packaging/build-debs.sh all
```

For host-workspace-only tests, keep the target outside the repository:

```sh
CARGO_TARGET_DIR="${TMPDIR:-/tmp}/buzzardos-host-target" \
  cargo test --manifest-path host/Cargo.toml --workspace --locked
```
