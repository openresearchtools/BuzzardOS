# Debian packages

`build-debs.sh` is the single local entry point for the three Buzzard OS
binary packages. Run it in the Ubuntu 24.04 build environment:

```sh
BUZZARDOS_DEB_OUTPUT_DIR=/path/on/data-disk/debs packaging/build-debs.sh all
```

Selections `host`, `guest`, and `cua` build one package. Generated targets,
staging roots, `.deb` files, and checksums remain outside the source tree.
The canonical filenames are:

```text
buzzardos_<VERSION>_amd64.deb
buzzardos-guest-desktop_<VERSION>_amd64.deb
buzzardcua_<BUZZARDCUA_VERSION>_amd64.deb
```

These are binary development artifacts. Publishing them from a signed APT
repository requires a separate reviewed release design.
