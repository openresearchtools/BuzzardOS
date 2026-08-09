# Stock Sway window-decoration contract

The reference image builds unmodified Sway 1.12 at
`88869399f421d9180dd8b6ed0b5a1f4a3585d252`. Wild Buzzard does not carry a
compositor patch or private fork.

At that commit, Sway's normal border is a compositor-owned scene containing
the title background, border, title text, and marks text. Its default input
seat:

- starts a floating move when button 1 is pressed on the titlebar;
- independently detects left, top, right, and bottom border bits;
- combines those bits for the four corner resize masks; and
- changes the container frame and client content through the same floating
  move/resize transaction path.

The reference configuration uses:

```text
for_window [all] floating enable, border normal 8
titlebar_padding 6 7
```

`[all]` is Sway's managed-view catch-all, so it covers normal xdg-shell and
Xwayland toplevels. Layer-shell surfaces used by the desktop and panel are not
managed views and do not match this rule.

## Stock limitation

Sway 1.12 does **not** draw or hit-test minimize, maximize/restore, or close
buttons in its server-side titlebar. The titlebar scene at the pinned commit
has no control nodes, and the default titlebar input path only focuses and
moves the container.

Sway still owns the authoritative operations:

- `kill` closes the selected view;
- `move scratchpad` and `scratchpad show` provide hide/restore semantics;
- `resize` and `move` set an exact floating frame; and
- `fullscreen` is available, but is not a classic maximize operation because
  it intentionally occupies the output outside the normal decorated workspace.

Wild Buzzard integrations must use those private in-guest IPC/input routes and
confirm the resulting Sway tree state. They must not claim that stock Sway
provides titlebar buttons, draw detached layer-shell decorations, or patch the
reference compositor.

Stock Sway scopes an unqualified mouse binding to the titlebar. The reference
session binds titlebar button 3 to focus the exact container under the pointer
and ask the native Wild Buzzard shell to open the same accessible window menu
used by a taskbar secondary click. Application content retains its normal
button-3 behavior. The menu exposes Focus, Bring Into View, Minimize,
Maximize/Restore, and Close. Bring Into View focuses the exact Sway identifier
and clamps its complete compositor-reported frame into the current usable
workspace, which recovers a window moved beyond a resized output.

Wild Buzzard's classic maximize is deliberately not Sway fullscreen. It sizes
the complete floating container to the workspace rectangle reported by Sway,
which is the virtual output's usable area after the bottom taskbar's exclusive
zone. The normal restore rectangle is stored as a container-scoped
`__wildbuzzard_restore_v1_*` Sway mark so the shell and in-guest CUA driver
share the same mapped-lifetime state. Restore removes that mark and clamps the
saved frame into the current workspace after an output resize. Minimize uses
the exact container's scratchpad state and retains the mark only when the
window was maximized. The reference config sets `show_marks no`, keeping this
internal state out of the compositor-rendered title.

State mutations subscribe to Sway events before issuing commands, then confirm
the exact container through `get_tree`; they do not periodically scan every
window. Stock Sway does not emit a window event for every pointer-driven
floating-resize motion. The shell therefore refreshes the exact Sway tree
synchronously before opening a task context menu, and the maximize/restore
action independently re-reads the live tree before choosing its operation.
Pointer-resizing a maximized window consequently changes the next menu label
and action to `Maximize` without periodic geometry polling.
