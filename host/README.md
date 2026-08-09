# Host application

This directory is the native host deployment boundary:

- `crates/wildbuzzard` owns portable machine creation and lifecycle;
- `crates/wildbuzzard-broker` creates and supervises namespaces, devices,
  networking, ports, and media bridges;
- `crates/wildbuzzard-display` owns the one native host Wayland window;
- `crates/wb-core` contains their shared portable configuration contracts;
- `packaging/` is the AppDir metadata and entry point; and
- `build-appimage.sh` assembles the self-contained x86-64 AppImage outside the
  checkout.

The host workspace intentionally does not include the guest desktop shell.
The packaging step separately builds the guest shell and CUA driver because
those executables are bundled as persistent-rootfs migration assets.

Run these commands from the repository root. The preferred test entry point
keeps every Cargo target and test artifact outside the checkout:

```sh
./tools/test-local.sh
WILDBUZZARD_BUILD_ROOT="${TMPDIR:-/tmp}/wildbuzzard-build-$(id -u)" \
  ./host/build-appimage.sh
```

For a host-workspace-only test, keep its target external explicitly:

```sh
CARGO_TARGET_DIR="${TMPDIR:-/tmp}/wildbuzzard-build-$(id -u)/tests/host-target" \
  cargo test --manifest-path host/Cargo.toml --workspace --locked
```
