# buzzardos-desktop changelog

## 0.1.3

- Add a Security page for changing the `user` password and explicitly toggling
  the exact guest-local passwordless-sudo policy, which remains off by default.
- Ask for the machine password when Settings changes the guest time zone and
  apply it through the real distro sudo path without a privileged Settings
  service or broad Polkit rule.

## 0.1.2

- Replaced the remaining per-frame repaint-marker read and 250 ms Settings
  read with exact-file inotify sources multiplexed into the shell's existing
  event wait. Idle desktops perform no recurring filesystem reads or scans.

## 0.1.1

- Fixed event-driven Desktop refreshes by opening a fresh secured directory
  description for each inotify-triggered scan. Repeated scans no longer share
  an end-of-directory offset, and no timer or idle directory scan is added.

## 0.1.0

- Initial independently versioned Buzzard OS classic desktop package.
- Added the human Desktop/CUA/CUA2 selector, lazy manual workspaces, and a
  workspace-scoped taskbar/background on every guest output.
- Kept the human selector bar visible while any CUA workspace is presented,
  reserved its full height from application frames, and made unused top/bottom
  bar space use the same surface palette instead of black gaps.
- Re-clamped every affected window after Desktop/CUA output swaps so titlebars
  remain fully below the selector and above the taskbar.
- Made workspace creation, selection, and closing follow verified Sway state;
  closing a numbered workspace moves its windows to Desktop before removal.
- Routed Applications launches to the workspace visible when the user clicked,
  and reveal an existing window there when an application is single-instance.
- Made Applications search a focused, clickable editing control with working
  Backspace/Delete/Enter behavior.
- Made the whole area of workspace, CUA, taskbar, and menu controls clickable,
  and removed the unwanted light separator above the taskbar.
- Re-clamped existing floating windows into their own workspace after output
  resizes, preventing another workspace's windows from crossing the new output
  boundary.
- Made the GTK4 Settings window root explicitly opaque so other guest windows
  cannot show through its page stack during focus changes; the stack also
  has a palette-derived solid drawing layer beneath it in both focused and
  backdrop states.
- Reworked Light mode as a complete neutral GTK palette with distinct desktop,
  panel, navigation, application, field, control, hover, border, and backdrop
  surfaces instead of near-white layers collapsing into one another.
- Gave Settings a native system-panel hierarchy with a tinted navigation rail,
  rounded selected rows, a quiet page canvas, and bordered raised setting cards;
  the shell workspace bar and taskbar now use the dedicated panel colour.
- Matched the requested Light palette samples exactly: workspace/task bars and
  the Settings sidebar use `#ebebeb`, while the desktop and Settings window
  canvas use the lighter `#fafafa`. Active shell segments remain continuous
  with their bar and Light hover states use a darker tone of the same neutral
  surface; orange remains reserved for selection and focus.
- Made focused and unfocused Thunar use one deliberate Light hierarchy instead
  of the old blue-grey fallback: panel-tone title/sidebar, window-tone
  menu/toolbar/status area, white file view, and neutral borders.
- Added Buzzard-orange icons for Home, Desktop, and every conventional XDG
  folder; one-time reference-image provisioning initializes the standard user
  directories and seeds Thunar Places with Documents and Downloads. Machine
  start performs no folder/bookmark setup and never restores removed entries.
- Update the real XDG Desktop from guest-local inotify or a one-shot helper
  notification, with no periodic directory scan, idle refresh, or restart.
- Wrap desktop labels to at most two bounded lines and ellipsize overflow so
  names never escape their icon cell.
- Render Files, Shared, and ordinary desktop folders with the installed
  Buzzard icon theme instead of placeholder rectangles, and use a launcher's
  localized `Name=` without appending its generic category.
