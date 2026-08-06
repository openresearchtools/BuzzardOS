# Wild Buzzard CUA Driver fork

This directory is an auditable, source-built fork of the Linux portion of
[`trycua/cua`](https://github.com/trycua/cua). `UPSTREAM.toml` is the
machine-readable origin record, `LICENSE.md` is the preserved upstream MIT
license, and `CHANGES.WILDBUZZARD.md` records downstream changes.

Build the pinned Linux driver with:

```sh
cargo build --locked --release --manifest-path cua-driver/rust/Cargo.toml \
  -p cua-driver
```

Release image builds use this source tree and do not fetch an unpinned Cua
Driver binary.
