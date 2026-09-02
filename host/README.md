# Native host application

This directory is the native host package boundary:

- `crates/buzzardos` implements the `buzzardos` CLI and orchestrates native
  Podman creation, lifecycle, OCI exchange, networking, shares, and devices;
- `crates/buzzardos-display` owns the native machine and manager windows; and
- `crates/wb-core` contains shared machine, registry, and protocol contracts.

The historical crate directory names are source-internal. Installed binaries,
desktop metadata, windows, diagnostics, and other human-facing identifiers use
Buzzard OS naming.

The host and guest are separate Cargo workspaces. The host package never
builds, embeds, migrates, or overwrites the guest package payload. A created or
imported OCI rootfs must already satisfy the Buzzard OS guest contract.

## Native Podman boundary

Buzzard does not select or emulate a user-namespace mapping. A blank custom
arguments field leaves Podman's configured rootless default untouched.
`--userns=keep-id`, `--userns=auto`, `--userns=nomap`, `--userns=host`, and
explicit `--uidmap`/`--gidmap` values are parsed into argv and forwarded to
`podman create` unchanged. The same unrestricted field also accepts every
other create argument supported by the installed Podman version.

The native display, input, PipeWire media, and explicit one-shot clipboard
bridges are Buzzard-specific. Container lifecycle, namespaces, cgroups,
seccomp, capabilities, networking, port publishing, bind mounts, devices, CDI,
and OCI/Containerfile operations remain stock Podman or Buildah behavior.
Buzzard adds no parallel runtime or security-policy layer.

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
