# Buzzard OS host changelog

## 0.1.1

- Updated the built-in Standard and CUDA image recipes to install
  `buzzardos-desktop` 0.1.1 from the signed Open Research Tools APT archive.

## 0.1.0

- Initial Debian-package architecture for the manager, broker, and display.
- Added per-machine directories, rootless Buildah pull/build flows, and OCI
  import/export without a portable application bundle.
- Declared the embedded guest monitor rectangle opaque on the native Wayland
  surface so an unfocused or partially damaged frame cannot expose host
  windows underneath it.
- Changed built-in Standard/CUDA creation to bootstrap the checksum-pinned Open
  Research Tools keyring and install the independently released guest packages
  from signed APT, with no developer package-folder input.
