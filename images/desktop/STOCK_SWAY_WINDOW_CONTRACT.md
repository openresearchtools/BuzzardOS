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
for_window [all] floating enable, border normal 3
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
