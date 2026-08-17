# Buzzard CUA Rust workspace

This locked Cargo workspace builds the Linux CUA Driver embedded in Wild
Buzzard's guest image.

## Workspace crates

| Crate | Linux scope |
| --- | --- |
| `cua-driver` | CLI/MCP daemon and Linux integration tests |
| `cua-driver-core` | Shared protocol, policy, capture, recording, and browser logic |
| `cua-driver-contract` | Typed public tool and result contracts |
| `cua-driver-sdk` | Typed Rust/UniFFI boundary used by the daemon |
| `cua-driver-testkit` | Linux-only test process, transport, and evidence helpers |
| `platform-linux` | Sway, Wayland, Xwayland, AT-SPI, capture, and input backend |
| `cursor-overlay` | Semantic cursor renderer and compiled theme loader |
| `pip-preview` | Recording preview support |

No macOS or Windows platform crate belongs to this workspace.

## Build and verify

Use the checked-in lockfile and an external target directory:

```bash
export CARGO_TARGET_DIR="$(mktemp -d)/cua-target"
cargo build --locked --release -p cua-driver
cargo fmt --all -- --check
cargo test --locked --workspace --all-targets
```

Default tests are headless. Tests marked `#[ignore]` require a real Linux
desktop session and the Sway/AT-SPI fixtures described in
`crates/cua-driver/tests/README.md`.

`target/`, staged applications, recordings, screenshots, and other generated
outputs are never source and must remain outside the repository.

## Policy files

`CUA_DRIVER_POLICY_FILE` accepts a YAML/Rego file or a directory of Rego files.
The daemon evaluates the configured deny-by-default policy locally; no OPA
service is required. Buzzard OS normally starts the driver in its managed
guest session with the product's explicit authorization policy.
