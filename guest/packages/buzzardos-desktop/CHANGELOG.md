# buzzardos-desktop changelog

## 0.1.0

- Initial independently versioned Buzzard OS classic desktop package.
- Added the human Desktop/CUA/CUA2 selector, lazy manual workspaces, and a
  workspace-scoped taskbar/background on every guest output.
- Kept the human selector bar visible while any CUA workspace is presented,
  reserved its full height from application frames, and made unused top/bottom
  bar space use the same surface palette instead of black gaps.
- Re-clamped every affected window after Desktop/CUA output swaps so titlebars
  remain fully below the selector and above the taskbar.
