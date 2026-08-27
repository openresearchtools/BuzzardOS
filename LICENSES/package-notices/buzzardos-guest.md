# Buzzard OS guest mechanics package notices

This notice covers only the payload of the `buzzardos-guest` package.

The guest integration scripts, systemd/session assets, clipboard agent, and
other Buzzard-authored mechanics are Copyright (C) 2026 Open Research Tools
contributors and licensed under AGPL-3.0-or-later. Locked Rust crates and the
Rust standard library linked into `buzzardos-clipboard-agent` retain their own
terms; their exact inventory and notice texts are shipped beside this file.

Sway, wlroots, PipeWire, WirePlumber, portals, Python, FUSE, GTK, GStreamer,
Xwayland, and other APT dependencies are separate packages. The
`buzzardos-guest` package does not contain their files or license texts.

This notice does not cover `buzzardos-desktop`, `buzzardoscua`, the base
distribution, or any other software selected by the person building or using
a machine.
