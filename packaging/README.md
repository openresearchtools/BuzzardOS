# Debian packages

`build-debs.sh` is the single local entry point for the four Buzzard OS
binary packages. Run it in the Ubuntu 24.04 build environment:

```sh
BUZZARDOS_DEB_OUTPUT_DIR=/path/on/data-disk/debs packaging/build-debs.sh all
```

Selections `host`, `guest`, `desktop`, and `cua` build one package. Generated targets,
staging roots, `.deb` files, and checksums remain outside the source tree.
The canonical filenames are:

```text
buzzardos_<VERSION>_amd64.deb
buzzardos-guest_<GUEST_VERSION>_amd64.deb
buzzardos-desktop_<DESKTOP_VERSION>_amd64.deb
buzzardoscua_<cua/VERSION>_amd64.deb
```

All four filenames use their component's own product version. In particular,
the Buzzard CUA package version is not derived from its historical TryCua
source baseline; that baseline is recorded only under `cua/` and in the
package's attribution files.

On a pushed `v*` tag the release workflow publishes all four artifacts and
their checksums to the matching Buzzard OS GitHub release. The separately
signed Open Research Tools APT repository indexes those release assets.

## License boundaries

Each package carries an independent Debian-format copyright record, a
package-specific third-party notice, the exact locked Cargo inventory for its
own executables, retained Rust dependency notices, and the Rust standard
library notice. The host, guest mechanics, desktop, and CUA inventories are not
interchangeable.

Packages named in `Depends` are installed separately by APT. They are not
copied into a Buzzard package, and their licenses remain with their own package
metadata. Likewise, the distributed guest Containerfiles are recipes rather
than prebuilt Debian or OCI payloads; licenses for a machine assembled from a
recipe are not presented as licenses of the host package.

The host package installs the Standard and CUDA recipes under
`/usr/share/buzzardos/containerfiles/desktop`. It does not install guest `.deb`
files there. The manager creates a temporary recipe-only Buildah context. The
recipe checksum-verifies and installs the Open Research Tools archive-keyring
package, then installs exact guest/Desktop/CUA versions from signed APT.

Audit the exact package files intended for handoff by repeating `--deb` in one
licensing-gate invocation:

```sh
tools/check-licenses.sh --structural \
  --deb /path/to/buzzardos_VERSION_amd64.deb \
  --deb /path/to/buzzardos-guest_VERSION_amd64.deb \
  --deb /path/to/buzzardos-desktop_VERSION_amd64.deb \
  --deb /path/to/buzzardoscua_VERSION_amd64.deb
```

The audit extracts each archive privately, rejects documentation from another
Buzzard package, checks every expected file byte-for-byte, verifies the Rust
standard-library notice, and checks any MPL source-code-form obligation against
that package's own Cargo closure.
