# Buzzard OS host changelog

## Unreleased

- Default desktop-machine task capacity to native Podman `--pids-limit=-1`,
  preserving explicit per-machine limits supplied through custom arguments.

- Build an unmodified private crun 1.29.1 from its exact upstream and recursive
  submodule commits. Ship corresponding source, license notices and features
  with the host package; retain the host's system crun and Podman defaults.
- Select the private runtime through native Podman arguments for all Buzzard
  commands. Include its path in persistent definition reconciliation without
  migrating other containers or recreating machines on ordinary starts.

- Restore the hardware-only primary display path: configure the guest session
  for GLES2, accept DMA-BUF frames, and remove the primary-output shared-memory
  CPU-copy path. Shared-memory cursor support is unchanged.
- Keep native Podman namespace options unchanged. This source correction does
  not resolve render-device permissions for the current subordinate-mapped
  desktop user; hardware startup and installed-package acceptance remain open.

## 0.1.5

- Keep the host completely out of guest credential management: create, pull,
  import, clone, export, start, and stop never read or rewrite guest passwords.
- Report the official built-in image's documented `user` / `buzzard`
  credential only after a successful build; custom images and imports retain
  their own credentials without a host-side claim.

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

- Initial Debian-package architecture for the manager, native display, and
  persistent rootless Podman machines.
- Added per-machine external rootfs directories, native Podman pull/lifecycle
  and OCI import/export, and Podman/Buildah Containerfile builds.
- Declared the embedded guest monitor rectangle opaque on the native Wayland
  surface so an unfocused or partially damaged frame cannot expose host
  windows underneath it.
- Changed built-in Standard/CUDA creation to bootstrap the checksum-pinned Open
  Research Tools keyring and install the independently released guest packages
  from signed APT, with no developer package-folder input.
