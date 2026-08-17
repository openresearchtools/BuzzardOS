# Buzzard OS CUA Driver fork

This directory is an auditable, source-built fork of the Linux portion of
[`trycua/cua`](https://github.com/trycua/cua). `UPSTREAM.toml` is the
machine-readable origin record, `LICENSE.md` is the preserved upstream MIT
license, `LINUX_SCOPE.toml` is the reviewed machine-readable source inventory,
and `CHANGES.BUZZARDOS.md` records downstream changes.

Build the pinned Linux driver with:

```sh
CARGO_TARGET_DIR="${TMPDIR:-/tmp}/buzzardos-build-$(id -u)/cua-target" \
  cargo build --locked --release --manifest-path cua-driver/rust/Cargo.toml \
    -p cua-driver
```

Run that command from this vendored-fork directory. The external target keeps
generated build output out of both the vendored source and repository root.

Release image builds use this source tree and do not fetch an unpinned Cua
Driver binary. A regression test enforces the eight selected Cargo packages,
their MIT package metadata, the five Linux skill files, and the absence of the
reviewed platform-only files.
