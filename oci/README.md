# Local guest assembly from the distributed Containerfile

`build-local.sh` locally assembles a Debian guest from
`desktop/Containerfile` with Buildah. The Containerfile is the distributed
recipe; Buzzard does not publish the resulting rootfs or OCI payload. Package
compilation happens first and outside the local OCI build. The current
developer workflow supplies only the three prebuilt Buzzard guest packages:
`buzzardos-guest`, `buzzardos-desktop`, and `buzzardoscua`; the release recipe
will obtain those same packages from the Buzzard APT repository. APT resolves
all non-Buzzard dependencies, including distribution Sway and wlroots, from
their own repositories. No compiler or source tree enters the image build
context.

The final image includes systemd, Sway, Xwayland, Foot, Firefox ESR, Mousepad,
Thunar, AT-SPI, PipeWire/WirePlumber, graphics runtimes, guest AppImage support,
Buzzard OS Guest Desktop, and Buzzard CUA. It contains no host-manager code,
private compositor fork, compositor source, compiler toolchain, Electron demo,
LM Studio, or Blender.

```sh
./oci/build-local.sh
BUZZARDOS_EXPORT_ARCHIVE=1 ./oci/build-local.sh
```

Every successful build records the content-addressed image ID, uncached build
duration, and exact installed dpkg inventory below
`${BUZZARDOS_OCI_OUTPUT_DIR:-/tmp/buzzardos-oci-build-UID/output}`. Optional OCI
archive export writes its archive and checksum there as build evidence.

Buildah uses a private temporary `vfs` store with `--no-cache` and
`--pull=always`. The image, working containers, layers, and build context are
deleted after verification; only requested output evidence remains.

The developer entry point is local-only. It never logs into or pushes a
registry. The manually dispatched Actions workflow uses the same definition
only to verify the image alongside the four `.deb` artifacts; the image is
discarded with the runner and is never uploaded.
