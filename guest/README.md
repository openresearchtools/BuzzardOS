# Guest userspace

This directory is everything Buzzard OS installs into the persistent guest:

- `shell/` is the Rust classic desktop shell;
- `desktop-core/` owns the versioned Settings/AppImage schemas, XDG
  discovery, desktop files, typed themes, and atomic persistence shared by
  guest applications;
- `settings/` is the unprivileged native GTK4 Settings application;
- `shortcut-helper/` validates and links AppImages in place and provides the
  descriptor-bound desktop-operation backend;
- `clipboard-agent/` owns the private guest side of explicit one-shot
  clipboard snapshots;
- `updater/` is the fixed-operation package-update service;
- `assets/` contains systemd, Sway, D-Bus, theme, integration, and native
  AppImage support files;
- `third_party/trycua-cua/` is the attributed, pinned CUA fork;
- `asset-manifest.tsv` is the authoritative source-to-rootfs mapping; and
- `install-rootfs-assets.sh` assembles exactly that payload for the OCI image.

The host launcher's managed-asset table is unit-tested against the same TSV
manifest, preventing new images and persistent-machine migrations from
silently diverging.

Run the complete local gate from the repository root. It places Cargo targets
and other generated test artifacts outside the checkout:

```sh
./tools/test-local.sh
```

For a guest-only pass, keep both Cargo and Python-generated files external:

```sh
CARGO_TARGET_DIR="${TMPDIR:-/tmp}/wildbuzzard-build-$(id -u)/tests/guest-target" \
  cargo test --manifest-path guest/Cargo.toml --workspace --locked
PYTHONDONTWRITEBYTECODE=1 \
  python3 -m unittest discover -s guest/tests -v
```
