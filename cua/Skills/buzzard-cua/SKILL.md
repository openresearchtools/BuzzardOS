---
name: buzzard-cua
description: Use the daemonless Buzzard CUA CLI to inspect and control one numbered stock-Sway guest output through screenshots, Sway window metadata, AT-SPI, and synthetic input.
version: 0.1.0
metadata:
  requires:
    bins:
      - cua
---

# Buzzard CUA

Use `cua` for the first agent output, `cua2` for the second, and so on.
`cua` and `cua1` are identical. Each number has its own Sway workspace,
off-screen output, synthetic seat, coordinate space, and lock. If no numbered
link exists, use `cua --index N ...`.

There is no server to start and no CUA session to create. Every command runs,
prints JSON, and exits. Do not look for MCP, CDP, recording, update, or remote
skill commands.

## Discover the exact contract

```bash
cua list-tools
cua list-tools --json
cua describe get_window_state
cua describe click
```

Tool names and JSON schemas reported by the installed binary are authoritative.
`focus` aliases `bring_to_front`; `screenshot` aliases
`get_desktop_state`. `cua browser ARGS...` directly executes the separately
installed WildBuzzard CLI and does not implement a browser API here.

## Normal loop

1. Run `list_apps` or `list_windows`. Window rows include `window_id`, PID,
   workspace, output, state, and geometry.
2. Launch an app or call `focus` with its exact `window_id`. Focus moves a
   foreign-workspace window into this caller's numbered workspace before it
   focuses it.
3. Observe with `get_window_state` for one window or `screenshot` for the exact
   numbered output.
4. Prefer a fresh `element_token` for AT-SPI actions. Otherwise use pixels from
   that same screenshot. Never reuse coordinates after output resize.
5. Perform one action, then observe or verify the result. A helper exit status
   alone is not evidence that the application changed.

Example:

```bash
cua launch_app '{"name":"thunar"}'
cua list_windows
cua get_window_state '{"pid":1234,"window_id":5678}'
cua click '{"pid":1234,"element_token":"s12ab34cd:7"}'
cua verify_state '{"pid":1234,"window_id":5678,"expect":[]}'
cua screenshot --screenshot-out-file /tmp/cua.png
```

Raw pointer, keyboard, and screenshot operations always target this invocation's
`seatN` and output. They never use human `seat0` and never capture host chrome
or another CUA output. Screenshots include native cursors on that output and
may show both the human and agent pointer while the human views it. Pointer
appearance is not input routing; the caller still injects only through seatN.
Accessibility discovery may list all guest apps, but a
visual action on a selected window first routes that window to the caller's
workspace.

## Concurrent agents

Give each independent agent a different command identity (`cua`, `cua2`,
`cua3`, ...). Do not share one number between concurrent mutating calls. Calls
on one number serialize; calls on different numbers can proceed independently.

For a normal drag, use `drag`. The low-level held-button triplet must remain in
one process:

```bash
cua batch '[
  {"tool":"mouse_button_down","scope":"desktop","x":100,"y":100},
  {"tool":"mouse_drag","scope":"desktop","x":500,"y":300},
  {"tool":"mouse_button_up","scope":"desktop"}
]'
```

The batch stops on the first failed step and still exits; it does not create a
daemon or durable session.

## Failure rules

- Stale element token or geometry: observe again; never guess a replacement.
- Busy numbered workspace: let the current bounded call finish, then retry.
- Target on another CUA output: `focus` may move it only after obtaining that
  source seat's lock; a busy source fails instead of disrupting its agent.
- Missing Sway protocol, named seat, or named output: treat as a guest runtime
  error. Never fall back to host input or the human seat.
- Always release pressed input. Buzzard CUA cleans up keys/buttons on errors and
  refuses cross-process held-button state.
