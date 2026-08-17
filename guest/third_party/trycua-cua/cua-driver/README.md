# Buzzard CUA source subset

This directory contains the source required to build and validate Wild
Buzzard's Linux CUA Driver for the private Sway/Wayland/Xwayland guest session.
It is derived from Cua Driver 0.17.0 in [`trycua/cua`](https://github.com/trycua/cua).

The authoritative origin, license, and downstream-change records are one level
above this directory:

- `../UPSTREAM.toml` pins the exact upstream repository, tag, and commit;
- `../LICENSE.md` preserves the upstream MIT license and copyright notice;
- `../CITATION.cff` preserves upstream citation metadata;
- `../CHANGES.BUZZARDOS.md` identifies Buzzard OS changes;
- `../LINUX_SCOPE.toml` records the reviewed source boundary.

This is deliberately not a cross-platform Cua distribution. macOS and Windows
platform crates, build resources, platform-only tests, and platform-only skill
guides are absent. Shared contract code can retain platform-labelled serialized
enum variants where compatibility requires them; those names do not add a
non-Linux backend to the build.

## Included interfaces

- `cua-driver mcp` and `cua-driver call` for agent integrations;
- the daemon and typed Rust SDK used by those interfaces;
- Linux AT-SPI accessibility, Sway IPC, native Wayland and Xwayland routes;
- full-output capture and canonical guest-output coordinates;
- browser/CDP helpers used by Linux Chromium and Electron;
- the Linux agent skill, recording guide, browser guide, and contract fixtures;
- Linux unit, protocol, integration, and ignored real-desktop tests.

## Build and test

From `rust/`:

```bash
cargo build --locked --release -p cua-driver
cargo fmt --all -- --check
cargo test --locked --workspace --all-targets
```

Set `CARGO_TARGET_DIR` to a path outside the repository. Interactive tests are
marked `#[ignore]` and require the Buzzard OS guest's Sway, AT-SPI, and
application fixtures. See `rust/crates/cua-driver/tests/README.md`.

The source fork is not endorsed by Cua AI, Inc. Its MIT terms remain separate
from Buzzard OS's own project license.
