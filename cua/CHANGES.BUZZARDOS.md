# Buzzard OS changes to Cua Driver

Upstream: `trycua/cua` tag `cua-driver-rs-v0.17.0`, commit
`10279552e2bbe479e367a082f78b1b98ee85a697`.

Buzzard CUA is a Linux-only, stock-Sway/wlroots-derived fork. The original MIT
license and notices are preserved in this directory. Buzzard-authored changes
are AGPL-3.0-or-later.

The fork is intentionally one Rust crate and one daemonless executable:

- `/usr/bin/cua`, `cua1`, `cua2`, ... are multicall entry points; `cua` and
  `cua1` mean the same first CUA workspace/output/seat;
- every invocation prepares and locks only its numbered Sway workspace,
  performs one tool operation or one bounded batch, writes JSON, and exits;
- there is no CUA daemon, server, MCP endpoint, network listener, remote skill
  downloader, self-updater, browser/CDP engine, recording subsystem, or
  session-start/session-end API;
- WildBuzzard browser automation remains an explicit `cua browser ...`
  compatibility exec route. Buzzard CUA does not duplicate that browser API;
- cross-invocation element, cursor, zoom, and resize state is bounded, private
  mode-0600 state below `$XDG_RUNTIME_DIR`, normally tmpfs, and disappears at
  logout or boot. It is internal coordination, not telemetry or history;
- upstream telemetry code, endpoints, installation identity, preferences,
  hooks, and tests are deleted rather than disabled;
- macOS, Windows, Chromium-specific, binding-generator, remote service,
  theme-authoring runtime, and upstream platform-fleet code are absent.

Buzzard-specific correctness changes include:

- each numbered command owns a distinct Sway output, workspace, virtual seat,
  coordinate space, and non-blocking lock; raw screenshots and synthetic input
  never use the human Desktop seat;
- compact global window/application metadata reports the owning output and
  workspace; focusing or targeting a window moves it atomically to the
  caller's workspace before acting;
- seat-specific focus exits an obstructing fullscreen container only on the
  caller's workspace, with confirmed Sway state and exact final seat focus;
  other workspaces and the host window/rendering path are not changed;
- canonical coordinates are physical guest-output pixels and fractional-scale
  geometry is transformed exactly once with generation checks;
- full-output screenshot, Sway window control, AT-SPI inspection/actions,
  application launch/focus, pointer input, keyboard input, clipboard, and
  window operations return structured evidence instead of treating helper exit
  status as success;
- application launches close inherited standard streams before spawning, so a
  long-running GUI cannot keep a one-shot agent or guest-control pipe open;
- element tokens are opaque, bounded, and validated against live AT-SPI
  identity across one-shot invocations;
- held pointer button primitives are allowed only inside one bounded `batch`
  process, ensuring cleanup on success or failure. The ordinary `drag` tool is
  safe as a standalone command;
- virtual keyboard and pointer objects are compositor-native, bound to the
  requested numbered seat/output, and release every held key/button before the
  process exits;
- screenshots remain guest-only and exclude host chrome. No host Wayland,
  clipboard, accessibility, input, or automation socket is exposed.

The inherited animated agent-cursor overlay, cursor themes, cursor registry,
and cursor-configuration tools are removed. Pointer tools use only Sway's
native cursor for the invoking numbered virtual seat/output. The Wayland
virtual-keyboard protocol remains with its original license evidence.

This fork is not endorsed by Cua AI, Inc.
