# Local reference-image assembly

`compose.yaml` builds the Debian reference image from `desktop/Containerfile`.
The build compiles the pinned stock Sway/wlroots stack, the guest shell, and the
vendored CUA driver, then installs the complete `guest/asset-manifest.tsv`
payload. The final image includes systemd, Sway, Xwayland, Foot, Firefox ESR,
Chromium, AT-SPI, PipeWire/WirePlumber, GPU userspace, and generic native
Electron AppImage dependencies. It contains no host launcher code, compiler,
compositor source, bundled Electron SDK/demo, LM Studio binary, or Blender.

```sh
./oci/build-local.sh
WILDBUZZARD_EXPORT_ARCHIVE=1 ./oci/build-local.sh
```

Every successful build records the content-addressed local image ID, unpacked
size, and exact installed `dpkg` package/version inventory under
`${WILDBUZZARD_OCI_OUTPUT_DIR:-/tmp/wildbuzzard-build-UID/oci}`. The optional
compressed archive and checksum are written there as well. These files are
build evidence; they are not committed to the source tree.

The build is local-only. It does not log into a registry, push an image, create
a GitHub package, or publish a release.
