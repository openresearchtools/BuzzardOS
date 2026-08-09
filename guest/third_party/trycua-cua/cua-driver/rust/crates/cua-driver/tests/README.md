# Linux CUA Driver integration tests

Tests in this directory exercise the source-built Linux driver's public CLI,
MCP, SDK, policy, capture, Sway/Wayland/Xwayland, AT-SPI, and browser behavior.

Headless tests run normally. Real-desktop tests are marked `#[ignore]` and need
an interactive Linux session plus their named fixture. Missing required
fixtures must become failures when `CUA_TEST_REQUIRE_FIXTURES=1` is set.

## Headless suite

Run the complete workspace with generated output outside the repository:

```bash
export CARGO_TARGET_DIR="$(mktemp -d)/cua-target"
cargo test --locked --workspace --all-targets
```

Important always-on coverage includes:

- CLI/MCP compatibility and schema consistency;
- daemon startup authorization and missing-daemon failure behavior;
- per-session capture-scope isolation;
- SDK/MCP shared-daemon behavior;
- Linux vendored-scope enforcement.

## Interactive Linux suite

Representative ignored targets include:

```bash
cargo test --locked -p cua-driver --test desktop_scope_linux_test \
  -- --ignored --nocapture --test-threads=1
cargo test --locked -p cua-driver --test harness_gtk3_test \
  -- --ignored --nocapture --test-threads=1
cargo test --locked -p cua-driver --test standalone_browser_behavior_test \
  -- --ignored --nocapture --test-threads=1
```

The runner may override relocated artifacts with `CUA_TEST_DRIVER_BIN`,
`CUA_TEST_APPS_ROOT`, and `CUA_TEST_WORKSPACE_ROOT`. `CUA_E2E_RECORDINGS_ROOT`
collects trajectory JSON, screenshots, and recordings for evidence review.

Each interactive action must verify external state or a fresh guest-only
capture. A successful process exit or synthetic event acknowledgement is not
enough.
