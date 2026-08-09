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
  draw titlebar window buttons, so titlebar or taskbar secondary click opens
  the shell's accessible Focus, Bring Into View, Minimize, Maximize/Restore,
  and Close menu backed by private in-guest Sway IPC;
- Xwayland;
- Firefox ESR, Chromium, Foot, and the shared libraries/FUSE integration used
  by typical native Electron AppImages;
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

No Electron SDK/demo, LM Studio binary, or other vendor application is bundled.
Native Electron acceptance supplies the official LM Studio AppImage from an
outside-repository temporary path and launches it directly through the generic
guest AppImage path.

The exact stock decoration/input boundary, including the absence of
minimize/maximize/close titlebar buttons, is recorded in
`STOCK_SWAY_WINDOW_CONTRACT.md` and checked against the pinned source during
the image build.

Build and verify the image locally through Compose:

```sh
./oci/build-local.sh
WILDBUZZARD_EXPORT_ARCHIVE=1 ./oci/build-local.sh
```

The build consumes `guest/asset-manifest.tsv` and verifies the installed image
with `oci/verify-image.sh`. It does not log into a registry or publish anything.
The repository intentionally has no active GitHub image, AppImage, package, or
release workflow at this stage.
