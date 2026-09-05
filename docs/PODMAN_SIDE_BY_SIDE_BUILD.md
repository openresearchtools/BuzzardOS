# Local side-by-side Podman host build

Set `BUZZARDOS_HOST_IDENTITY=buzzardos-pod` when running
`packaging/build-debs.sh host` in the normal build container. This compiles and
packages the same host code as `buzzardos-pod_<version>_amd64.deb`.

This temporary installation is **Buzzard OS (Podman)** in the application menu.
Its launcher is `/usr/bin/buzzardos-pod`; its helper, assets, icon, desktop ID,
AppStream metadata, and license documentation have separate installation paths.
It uses `$XDG_CONFIG_HOME/buzzardos-pod/machines.json` (or
`~/.config/buzzardos-pod/machines.json`), `$XDG_RUNTIME_DIR/buzzardos-pod`, and
`buzzardos-pod-<uuid>` Podman container names. It does not adopt the main app's
registry or machines. Do not register the same machine directory in both apps.

Guest packages, guest interfaces, signed APT Containerfiles, and the rootfs
format are unchanged. Host license evidence still identifies the canonical
Buzzard host component and is installed under `/usr/share/doc/buzzardos-pod`.
This local build does not publish a release or alter the APT repository.

The identity is compiled in, not selected at runtime. To build the normal
identity again, omit the setting or set it to `buzzardos`. Cargo tracks this
compile-time setting, including when reusing a build cache. To uninstall only
the temporary application, use `sudo apt remove buzzardos-pod`; machine
directories and user configuration are not removed or migrated. The original
`buzzardos` package remains independent.
