# Desktop image

`Containerfile` defines the persistent Debian-family guest image for Wild
Buzzard.

The target image contains:

- systemd and a normal interactive user with passwordless guest sudo;
- unmodified upstream Sway 1.12 and wlroots 0.20.2 pinned to exact source
  commits;
- Wild Buzzard's native Rust desktop shell, with an always-visible bottom
  classic taskbar, compact vertical Applications menu with real theme icons,
  desktop shortcuts, running-application buttons, and guest session controls;
- stock compositor-level titlebars, draggable windows, and four-edge/four-corner
  resizable borders for Wayland and Xwayland applications; stock Sway does not
  draw titlebar window buttons, so close and state/geometry actions use its
  private in-guest IPC/input routes;
- Xwayland;
- Firefox, Chromium, and a pinned representative accessible Electron app;
- private system and session D-Bus services;
- a private AT-SPI registry for GTK, Qt/KDE, Electron/Chromium, and compatible
  Xwayland applications;
- TryCua Cua Driver with its MCP/CLI integration;
- private guest audio services; and
- Vulkan/OpenGL desktop support.

It contains no KWin, KDE Plasma shell, XFCE shell, labwc, Waybar, Fuzzel,
patched wlroots, private compositor fork, compiler toolchain, or Blender. KDE,
GTK, Electron, and other applications remain supported and may be installed
like any other guest software. Newly installed desktop entries are discovered
by the Applications menu without rebuilding the image.
The visual menu may scroll, but AT-SPI exposes the complete installed-app list
and every running window for direct agent invocation. KDE Wallet
auto-activation is disabled; Chromium uses its guest-local basic password
store and does not display a wallet prompt.

The exact stock decoration/input boundary, including the absence of
minimize/maximize/close titlebar buttons, is recorded in
`STOCK_SWAY_WINDOW_CONTRACT.md` and checked against the pinned source during
the image build.

Build and publish the image with an OCI-compatible build service:

```sh
docker build -f images/desktop/Containerfile \
  -t ghcr.io/openresearchtools/buzzardos-desktop:sid .
docker push ghcr.io/openresearchtools/buzzardos-desktop:sid
```

The checked-in Containerfile is the Sway/wlroots image definition. Do not publish
it as a release image until the session, D-Bus/AT-SPI, minimal shell, TryCua,
persistence, and hardware integration tests pass.

The repository's `Desktop image` workflow publishes `latest`, commit-SHA, and
release-tag build candidates. The separate manually dispatched
`Hardware acceptance` workflow is the release gate on a self-hosted Wayland
and NVIDIA runner; publishing an image does not by itself certify it as a
release.
