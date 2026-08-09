# Wild Buzzard Linux CUA Driver agent skill

This skill teaches an in-guest agent to operate applications on Wild Buzzard's
private Linux Sway/Wayland/Xwayland desktop through `cua-driver` CLI or MCP.

It covers the snapshot-action-verify loop, exact Sway window addressing,
AT-SPI element actions, canonical guest-output coordinates, native input,
typed Chromium/Electron tools, and recording evidence. It never grants access
to the host compositor, host input, host screenshots, or host accessibility
services.

## Runtime prerequisites

The driver must run as the normal interactive guest user with access to:

- the private guest session D-Bus and AT-SPI registry;
- Sway's private `SWAYSOCK` and guest Wayland socket;
- Xwayland when automating legacy X11 clients;
- the guest-only full-output screencopy and input protocols.

Run `cua-driver doctor` to inspect those routes.

## Reading order

- `SKILL.md`: required snapshot/action/evidence contract and tool selection;
- `LINUX.md`: Linux launch, Sway, AT-SPI, Wayland/Xwayland, and input details;
- `BROWSER.md`: exact browser-window binding and trusted page actions;
- `RECORDING.md`: trajectory evidence and replay.

## Install the Linux skill

The Wild Buzzard guest image carries the version-matched files. A source-built
driver can install them into a detected agent directory with:

```bash
cua-driver skills install
```

The Wild Buzzard fork intentionally rejects `--all-platforms`; macOS and
Windows guides do not belong to this auditable Linux source subset.

## License

These vendored upstream sources retain the MIT license recorded in
`../../../../LICENSE.md`. Wild Buzzard modifications and origin metadata are
recorded beside that license.
