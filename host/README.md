# Native host application

This directory is the native host deployment boundary:

- `crates/wildbuzzard` owns machine lifecycle and OCI import/export;
- `crates/wildbuzzard-broker` creates and supervises namespaces, devices,
  networking, ports, and media bridges;
- `crates/wildbuzzard-display` owns the native machine and manager windows;
- `crates/wb-core` contains shared portable configuration contracts;
- `packaging/` contains the extracted launchers, metadata, and Low Glide icon;
- `build-portable-app.sh` assembles the dependency-complete `app/` directory
  outside the checkout.

The host and guest are separate Cargo workspaces. The portable application
builder also builds the guest shell and CUA driver because those binaries are
managed migration assets for existing persistent rootfses.

```sh
./tools/test-local.sh
WILDBUZZARD_GUEST_RUNTIME_PAYLOAD=/path/to/sway-runtime-artifact \
WILDBUZZARD_BUILD_ROOT="${TMPDIR:-/tmp}/buzzardos-build-$(id -u)" \
  ./host/build-portable-app.sh
```

For host-workspace-only tests, keep the target outside the repository:

```sh
CARGO_TARGET_DIR="${TMPDIR:-/tmp}/buzzardos-host-target" \
  cargo test --manifest-path host/Cargo.toml --workspace --locked
```
