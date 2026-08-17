# Hosted Debian package assembly

`build-release-assets.yml` is manually dispatched and read-only. On a
disposable Ubuntu 24.04 x86-64 runner it:

1. validates source, packaging, OCI, and licensing contracts;
2. builds `buzzardos`, `buzzardos-guest-desktop`, and `buzzardcua` `.deb` files;
3. install-smokes the host package;
4. builds and verifies the reference OCI with distro Sway/wlroots; and
5. uploads the three packages and their SHA-256 files as one seven-day Actions
   artifact named `BuzzardOS-debian-packages-amd64`.

The OCI image exists only in the runner's local Docker daemon and is removed
after verification. The workflow has no automatic trigger, write permission,
publisher job, registry login, OCI push, APT upload, or GitHub Release action.
It cannot create or modify a tag, environment, Package, GHCR image, or APT
repository.
