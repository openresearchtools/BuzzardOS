# Reference OCI assembly

`compose.yaml` and `build-local.sh` build the Debian reference guest from
`desktop/Containerfile`. A build stage produces the local
`buzzardos-guest-desktop` and `buzzardcua` Debian packages; the final stage
installs those packages plus the distribution's stock Sway/wlroots packages
with APT.

The final image includes systemd, Sway, Xwayland, Foot, Firefox ESR, Mousepad,
Thunar, AT-SPI, PipeWire/WirePlumber, graphics runtimes, guest AppImage support,
Buzzard OS Guest Desktop, and Buzzard CUA. It contains no host-manager code,
private compositor fork, compositor source, compiler toolchain, Electron demo,
LM Studio, or Blender.

```sh
./oci/build-local.sh
BUZZARDOS_EXPORT_ARCHIVE=1 ./oci/build-local.sh
```

Every successful build records the content-addressed local image ID, unpacked
size, and exact installed dpkg inventory below
`${BUZZARDOS_OCI_OUTPUT_DIR:-/tmp/buzzardos-build-UID/oci}`. Optional OCI
archive export writes its archive and checksum there as build evidence.

The developer entry point is local-only. It never logs into or pushes a
registry. The manually dispatched Actions workflow uses the same definition
only to verify the image alongside the three `.deb` artifacts; the image is
discarded with the runner and is never uploaded.
