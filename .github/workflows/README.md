# Hosted Debian package assembly

`build-release-assets.yml` runs manually or for a pushed `v*` tag. On a
disposable Ubuntu 24.04 x86-64 runner it:

1. validates source, packaging, OCI, and licensing contracts;
2. builds `buzzardos`, `buzzardos-guest`, `buzzardos-desktop`, and `buzzardoscua` `.deb` files;
3. install-smokes the host package;
4. on manual runs, builds and verifies the published-APT reference OCI with
   distro Sway/wlroots;
5. uploads the four packages and their SHA-256 files as one seven-day Actions
   artifact named `BuzzardOS-debian-packages-amd64`.
6. on version tags, publishes each newly versioned package and checksum to the
   matching stable GitHub Release. All four are rebuilt and audited, but an
   unchanged component already present on an earlier stable release is not
   duplicated, preserving independent package versions for the APT indexer.

The OCI image exists only in the runner's local Buildah store and is removed
after verification. The workflow never publishes an OCI image or writes APT
metadata. The separate `openresearchtools/apt` workflow indexes stable package
release assets and signs the central catalogue in its protected environment.
