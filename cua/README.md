# Buzzard CUA

Buzzard CUA is one Rust crate and one daemonless CLI for the stock-Sway guest
inside Buzzard OS. It contains only the reviewed Linux/Wayland, Xwayland,
AT-SPI, screenshot, input, window, application, clipboard, health, zoom, and
agent-cursor code used by the product.

`cua` is `cua1`. `cuaN` binds to `seatN`, workspace `CUA`/`CUAN`, and that
workspace's exact Sway output. `cua --index N` supports an arbitrary positive
number without requiring another installed link. Different numbers have
independent output coordinates and per-seat mutation locks.

There is no daemon, MCP server, browser/CDP implementation, recording,
telemetry, self-update, remote skill download, macOS code, or Windows code.
`cua browser ...` is only an `exec(2)` compatibility route to the separately
installed `/usr/bin/wildbuzzard`; Buzzard CUA does not interpret browser tools.

## CLI

```bash
cua list-tools
cua describe TOOL
cua TOOL '{"field":"value"}'
cua screenshot --screenshot-out-file /tmp/cua.png
cua2 list_windows
cua --index 19 launch_app '{"name":"firefox-esr"}'
```

Each tool invocation prepares its numbered output, holds that seat's private
lock, performs one bounded operation, prints structured JSON, and exits.
Cross-invocation element-token, zoom, resize, and cursor settings are capped,
mode-0600 files in `$XDG_RUNTIME_DIR/buzzardoscua`; this is normally tmpfs and
is removed at logout or reboot. It is neither a session nor telemetry.

Wayland requires a button-down, motion, and button-up to share one live client
connection. Use the ordinary `drag` tool, or place the three low-level held
pointer tools in one bounded `cua batch` call. Standalone held-button calls
refuse instead of falsely claiming that a button remains pressed after exit.

## Build

```bash
export CARGO_TARGET_DIR="$(mktemp -d)/buzzardoscua-target"
cargo fmt --manifest-path cua/Cargo.toml --all -- --check
cargo test --manifest-path cua/Cargo.toml --locked --all-targets
cargo build --manifest-path cua/Cargo.toml --locked --release
```

The package build installs the release binary as `/usr/bin/cua`, numbered
convenience links, `/usr/bin/buzzardoscua`, this crate's AGPL license, and the
pinned upstream MIT notice. Generated targets, screenshots, and state never
belong in source.
