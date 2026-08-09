# Wild Buzzard portable bundle

This directory is self-contained and movable. Keep its files together:

```text
WildBuzzard/
├── WildBuzzard-x86_64.AppImage
├── runtime/
│   ├── WildBuzzard-rootfs-linux-x86_64.tar.zst
│   └── WildBuzzard-rootfs-linux-x86_64.json
├── licenses/
│   ├── appimage/
│   └── guest-rootfs/
├── provenance/
├── vm/
├── shared/
├── cache/
└── SHA256SUMS
```

The runtime payload is the verified, flattened reference root filesystem with
canonical guest numeric IDs. On first machine creation, Wild Buzzard verifies
its manifest and expands it through the launcher's subordinate-ID user
namespace. Do not manually unpack it into `vm/` as the host user. Later starts
reuse the same mutable rootfs; they do not pull or unpack it again.

`vm/` holds persistent machines. `shared/` is mounted read/write at `/shared`
in each guest. `cache/` is disposable. All three are intentionally empty in a
new bundle. No Docker/Podman store, GHCR package, or hidden home-directory
state is part of this bundle.

The plain AppImage is also published separately so an existing portable folder
can update only the launcher. The AppImage and rootfs each carry a separate
notice/provenance group. `SHA256SUMS` authenticates every regular file in this
directory except itself, including both exact project-source archives.
