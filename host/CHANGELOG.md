# Buzzard OS host changelog

## 0.1.4

- Split reproducible Standard and CUDA machine-package inventories so each
  reference build is audited against its own exact APT closure.
- Retained NVIDIA's pinned MIT notice for `cuda-keyring` and the installed CUDA
  EULA for NVIDIA's CUDA libraries metapackage inside CUDA machine root filesystems.
- Updated CUDA notice validation from the obsolete CUDA 13.1 paths to the
  installed CUDA 13.3 package paths.

## 0.1.3

- Updated the built-in Standard and CUDA image recipes to install the first
  independently versioned Buzzard CUA release, `buzzardoscua` 0.1.0.
- Removed the withdrawn TryCua-derived version string from every product and
  package-selection surface; the source baseline remains attribution only.

## 0.1.2

- Updated the built-in Standard and CUDA image recipes to install
  `buzzardos-desktop` 0.1.2 from the signed Open Research Tools APT archive.

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
