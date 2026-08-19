# Buzzard OS changes to Cua Driver

Upstream: `trycua/cua` tag `cua-driver-rs-v0.17.0`, commit
`10279552e2bbe479e367a082f78b1b98ee85a697`.

Buzzard OS carries the Linux Cua Driver, the shared packages/assets needed
to build it, and the CLI/MCP contract testkit. macOS, Windows,
binding-generator, theme-authoring, and platform test-fleet packages are
intentionally not vendored.

Local changes are kept as ordinary reviewable source changes in this tree:

- partially off-output Sway windows retain transparent padding in their
  per-window screenshots, so screenshot pixels and window-local pointer input
  keep the same origin instead of shifting by the clipped edge;
- product telemetry is removed rather than disabled: the PostHog endpoint and
  key, payload/sender workers, installation identity and preference state,
  telemetry CLI, lifecycle/tool hooks, and telemetry-only tests are absent;
  automatic version checks, the CUA self-updater/check tool, and remote CUA
  skill-pack download commands are removed so the pinned in-guest driver has
  no vendor phone-home path;
- requested keyboard modifiers remain depressed for the complete native
  Wayland pointer-drag transaction, so gestures such as Blender
  Shift+middle-drag pan instead of silently degrading to an unmodified orbit;
- single-key input uses the same daemon-owned compositor-native virtual
  keyboard as working hotkeys and fails closed when the required stock-Sway
  protocol is absent; there is no uncancellable keyboard libei fallback, so
  Enter, Tab, Escape, arrows, and other keys reliably reach the focused
  surface in nested Sway without a one-shot `wtype` device;
- compositor-native keyboard input is owned by one persistent worker instead
  of repeatedly creating, activating, and destroying a Sway input device for
  every shortcut and named key. Every success, error, unwind, modifier-drag
  completion, and CUA session end releases all tracked keys in reverse order
  and publishes an all-zero modifier state; session end waits for that reset's
  bounded worker acknowledgement. A failed roundtrip first retries cleanup on
  that same device. When the connection is dead, pinned wlroots releases its
  compositor-side pressed-key set on that same virtual keyboard during device
  destruction before CUA reconnects; recovery never replays presses through a
  replacement device. Cancellation restores the fixed keymap, releases every
  key, zeros modifiers, and completes a bounded sync on that same Wayland
  client; an unprovable barrier fail-stops session teardown. SDK shutdown
  resets the process-global owner but keeps it reusable by later SDK instances;
  process exit performs final device destruction. A Ctrl+L/text/Enter sequence therefore
  cannot leave Ctrl held or leave parent input behind a torn-down active
  virtual device, and duplicate modifiers are collapsed before injection;
- Unicode text uses a dynamically generated wtype-compatible one-keysym-per-key
  map on that persistent owner, with a bounded per-character delay. Chromium
  address fields and other busy renderers receive every character in order
  without creating and destroying a Sway input device for each call;
- AT-SPI `EditableText.InsertText` uses the ATK ABI's UTF-8 byte length while
  retaining Unicode-character offsets for the Text interface. Native
  insertions and whole-value replacements are read back exactly before success
  is returned, so multibyte text cannot be truncated or retried into a
  duplicate mutation;
- AT-SPI clicks require the boolean acknowledgement returned by
  `Action.DoAction`; rejected toolkit actions are fallback/error conditions,
  not successes merely because the D-Bus method call completed;
- Linux AT-SPI element tokens bind to the exporting D-Bus object identity,
  rather than reusing a bounded snapshot's dense ordinal in a later unbounded
  tree walk. Chromium/Electron actions therefore stay on the element the
  agent observed when `max_depth` or `max_elements` is used;
- post-action observation polls a bounded sequence of real guest frames, so
  application launches and browser navigations are not mislabeled as no-ops
  merely because their first changed frame follows the initial AT-SPI
  acknowledgement;
- foreground Wayland text entry captures a stable guest-only baseline after
  focus and returns confirmed only when bounded post-action screencopy evidence
  shows that the field visibly changed;
- Wayland single-key and chord routes use the same bounded guest-output
  observation instead of returning an unverified synthetic-event
  acknowledgement;
- the Rust workspace member list is reduced to the Linux runtime packages;
- target-only path dependencies on non-vendored macOS/Windows packages are
  removed from the Linux-only fork manifests;
- macOS/Windows-only build scripts, executable resources, skill guides,
  harnesses, desktop-scope tests, installed-application tests, and protocol
  tests are removed from the vendored Linux subset. `LINUX_SCOPE.toml` records
  the exact eight-package/five-skill inventory, and a Linux regression test
  prevents those reviewed platform-only paths or direct dependencies from
  returning;
- every selected local Cargo package inherits the preserved upstream MIT
  license metadata. Platform-named lockfile packages that remain are
  target-specific transitive metadata of the Linux clipboard dependency, not
  selected non-Linux backends;
- the reference-image build compiles this pinned source instead of downloading
  a release binary;
- guest-output physical dmabuf pixels are the canonical screenshot/input
  coordinate space. Full-output screencopy preserves the native buffer byte
  dimensions without filtering or resampling; compositor geometry, AT-SPI
  bounds, and input cross the logical/physical boundary exactly once;
- Linux `get_accessibility_tree` enumerates compositor/AT-SPI windows and
  returns every registered application's actionable AT-SPI tree, including
  the layer-shell desktop, instead of the upstream X11-only process snapshot;
  every application snapshot and actionable element includes the opaque token
  needed for safe direct invocation from that one aggregate response;
- native Wayland enumeration retains AT-SPI-only layer-shell applications
  beside ordinary foreign-toplevel windows, so opening an application cannot
  make the Buzzard OS desktop controls disappear from CUA. Internal AT-SPI
  roots belonging to a process already represented by compositor-owned
  toplevels are not misreported as duplicate public windows;
- Linux `list_apps` returns visible XDG desktop applications exactly once,
  omits unmatched session/service processes, and honors higher-priority
  Hidden/NoDisplay desktop-file tombstones so helpers suppressed by the
  reference image cannot reappear from `/usr/share`;
- AT-SPI role discovery falls back to the standard numeric role when an
  application such as AccessKit omits the optional localized role-name method;
- post-action results are accepted only when the target state can be observed;
  guest-desktop click, double-click, right-click, drag, scroll, typing, key,
  and chord routes compare native guest-only output captures before and after
  injection and publish explicit screenshot-change evidence rather than
  treating a zero exit status as success; pointer-targeted actions position
  the real guest cursor and require a stable baseline before injection;
  comparison decodes RGBA pixels and masks the measured animated guest cursor,
  its previous location, and its motion corridor so cursor-only changes cannot
  falsely confirm a no-op;
- double-click and right-click reuse click's canonical desktop, window, and
  AT-SPI target routes and its post-action evidence instead of maintaining
  separate unverified delivery implementations;
- `mouse_button_down`, `mouse_drag`, and `mouse_button_up` retain the held
  button in the guest compositor across separate CLI/MCP calls, support
  canonical desktop coordinates, translate window-local Wayland coordinates
  exactly once, and return observable held-state readback evidence;
- Linux `close_window`, `minimize_window`, `maximize_window`, and
  `restore_window` are typed, risk-classified exact-window tools. They use
  wlroots foreign-toplevel management for native Wayland/Xwayland windows and
  EWMH/ICCCM on X11, then publish success only after independent compositor or
  window-manager state readback. One daemon-owned foreign-toplevel connection
  assigns monotonic window IDs and also performs activation and control, so
  protocol object IDs cannot alias across one-shot client connections,
  duplicate titles remain distinct, and a closed ID cannot fall back to title
  guessing. Compositor-minimized state remains authoritative over stale
  geometry/AT-SPI visibility. A unique live same-user executable/app-id match
  supplies the process guard for non-AT-SPI Wayland clients such as terminals,
  while the persistent toplevel ID remains the exact window identity;
- the shared Rust SDK exports those four published Linux window-control
  contracts for both direct drivers and bound sessions; its contract parity
  test now covers the complete published Linux manifest;
- Buzzard OS's stock Sway/wlroots session is the supported Wayland target.
  Its private `SWAYSOCK` exposes Sway's authoritative IPC tree for global
  toplevel geometry and exact-container configure/focus/close operations
  wholly inside the guest. CUA converts those logical compositor rectangles
  once into the canonical guest-output physical coordinate space and confirms
  `set_window_frame` from a fresh compositor readback; it does not infer global
  placement from foreign-toplevel metadata or touch the host compositor;
- full-output capture uses a private in-guest repaint handshake to wake an
  otherwise idle nested Sway output before native screencopy (and its grim
  fallback). The handshake only damages the guest shell; it neither captures
  nor exposes the surrounding host desktop. The grim fallback has its own
  bounded timeout and reaps its process and output readers, so a timed-out API
  caller cannot leave screenshot helpers running;
- fractional-scale screencopy returns the unchanged physical guest dmabuf
  dimensions. Virtual-pointer extents use that physical canonical space while
  compositor/Xwayland geometry and logical AT-SPI rectangles are transformed
  once. A capture that races an output-mode change fails with
  `stale_output_geometry` instead of resampling or returning coordinates for
  the previous mode. Chromium/Electron renderer subtrees that already expose
  physical AT-SPI extents are detected from the document viewport and kept
  unchanged while logical browser chrome is transformed once; semantic
  rectangles, hit-testing, screenshots, and element-coordinate input therefore
  remain aligned at fractional scale.
- Buzzard OS output metadata now consumes the exact split host-surface and
  guest-UI scale schema and carries `geometry_generation` through screen-size,
  desktop-screenshot, window-list, and pointer-click evidence. Full-output
  capture and window enumeration compare the complete state before and after
  each operation, rejecting even a same-sized scale-generation race. Absolute
  pointer actions use the same guard; persistent held-button input records its
  press generation and releases every compositor button before rejecting a
  stale cross-call drag. The host-published state is read only as a bounded,
  no-follow, session-owned regular file with an exact field set. Inside a
  Buzzard OS machine, `get_screen_size` and generation-sensitive operations
  fail closed if that canonical state is missing or invalid instead of
  fabricating generation-zero fallback geometry; standalone upstream use
  retains its ordinary compositor/X11 fallback.
- CUA no longer writes a shared last-click coordinate file. Agents retain the
  coordinates of actions they issue and verify them through the existing
  screenshot, accessibility, and window-state APIs, but receive no passive
  side channel for human clicks in other guest applications.
- wlroots virtual pointers are bound to Buzzard OS's concrete guest output
  with protocol version 2. This prevents absolute input from being accepted
  against an ambiguous nested output layout while leaving the real Sway seat
  at its previous position.
- Linux `launch_app` retains and reaps every spawned child. A direct launcher
  that exits unsuccessfully before publishing a window now returns a
  structured `exited_before_window` failure with `running: false`, instead of
  leaving a zombie and claiming success. A zero-exit launcher that publishes
  no observable window is reported as an unconfirmed failure rather than
  treating a possible daemon/existing-instance handoff as proven; live GUI
  children are reaped asynchronously when they later exit, and `xdg-open`
  helper children receive the same cleanup.
- the release build cross-compiles this pinned driver to the guest glibc
  baseline and carries it in the AppImage as a versioned managed guest asset,
  so existing persistent machines receive the audited fork without an
  unpinned runtime download.

This fork is not endorsed by Cua AI, Inc. The upstream MIT license and
copyright notice are preserved in `LICENSE.md`.

Buzzard OS privacy default:

- upstream CUA product telemetry is disabled by default in the fork;
- the managed guest service also pins `CUA_DRIVER_RS_TELEMETRY_ENABLED=false`,
  so starting the reference desktop never contacts CUA's telemetry endpoint.
