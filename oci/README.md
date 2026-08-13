# Reference-image assembly

`compose.yaml` builds the Debian reference image from `desktop/Containerfile`.
The build compiles the pinned stock Sway/wlroots stack, the guest shell, and the
vendored CUA driver, then installs the complete `guest/asset-manifest.tsv`
payload. The final image includes systemd, Sway, Xwayland, Foot, Firefox ESR,
Mousepad, Thunar, AT-SPI, PipeWire/WirePlumber, GPU userspace, and generic
native Electron AppImage dependencies. It contains no host launcher code,
compiler, compositor source, bundled Electron SDK/demo, LM Studio binary, or
Blender. Chromium, Dolphin, Pavucontrol, `x11-apps`, XTerm/UXTerm, and
Mesa/Vulkan diagnostic applications are also absent; Xwayland and the actual
Mesa/Vulkan runtimes remain present.

```sh
./oci/build-local.sh
WILDBUZZARD_EXPORT_ARCHIVE=1 ./oci/build-local.sh
```

Every successful build records the content-addressed local image ID, unpacked
size, and exact installed `dpkg` package/version inventory under
`${WILDBUZZARD_OCI_OUTPUT_DIR:-/tmp/wildbuzzard-build-UID/oci}`. The optional
compressed archive and checksum are written there as well. These files are
build evidence; they are not committed to the source tree.

The developer entry point is local-only. It does not log into a registry, push
an image, create a GitHub package, or publish a release.

The manually dispatched release-assets workflow uses the same Containerfile on
a disposable GitHub-hosted x86-64 runner. Its image remains in that runner's
Docker daemon long enough to pass `verify-image.sh`, export a digest-verified
OCI layout, and flatten the filesystem into the compressed rootfs seed carried
inside `BuzzardOS-portable-linux-x86_64.tar.xz`. The OCI layout and local image
are then discarded. The checked-in workflow is artifact-only, has no write
permission, and never pushes GHCR or another container registry/package.
