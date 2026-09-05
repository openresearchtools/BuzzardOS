# Private stock crun

Buzzard builds the unmodified crun **1.29.1** sources at
`f0d911de5587342cfeb16473bf32ecdfeaf25957`. `UPSTREAM.toml` records the
annotated release tag, every recursive submodule commit and a checksum covering
all vendored paths, file contents and executable/symlink modes. `source/` is the
complete upstream Git archive with each pinned submodule expanded. No local
runtime patch, binary download, submodule fetch or Git configuration change is
part of the package build.

Run `python3 tools/crun_source.py` to verify the source. After an intentional
upstream update, archive the exact upstream and recursive submodule commits,
update their records, and compute `python3 tools/crun_source.py --print-digest`.
Review upstream changes and repeat the package/runtime acceptance tests before
updating the checksum. Do not change it to bless an unexplained source edit.

`packaging/build-crun.sh /absolute/build/directory` builds a private copy outside
the source checkout. It generates upstream's tarball-version inputs there,
retains standard capabilities, seccomp, systemd and eBPF support, and uses
the upstream portable BLAKE3 implementation. It dynamically links the distro's
ordinary C libraries; those dependencies are derived with `dpkg-shlibdeps`.
Build on Ubuntu 24.04 for the supported host ABI floor. The build rejects ELF
objects requiring glibc newer than 2.39.

Checkpoint/restore follows upstream's optional CRIU detection: builds with
the distro CRIU headers enable it (the Debian build container installs them).
Ubuntu 24.04 does not provide CRIU, so builds there omit that optional feature.
The package recommends the distro `criu` package, not a mandatory `libcriu2`
dependency unavailable on Ubuntu 24.04. The built runtime's `--version` and
`features` report its compiled capabilities; checkpoint/restore also requires
the host CRIU tools/library. Normal create/start/stop does not require CRIU.

The host `.deb` installs `/usr/libexec/buzzardos/crun`; the side-by-side package
installs `/usr/libexec/buzzardos-pod/crun`. Neither installs `/usr/bin/crun`,
sets file capabilities or setuid bits, modifies `containers.conf`, nor migrates
other containers. Buzzard passes the private absolute path through Podman's
native `--runtime` option. Native custom arguments remain unrestricted. The
selected runtime path participates in the machine definition digest; a changed
path is reconciled only for that stopped machine. Normal lifecycle calls keep
the existing container. Updating the binary at the stable installed path does
not recreate a container.

This is a separately executed upstream component, not Rust code relicensed
as Buzzard's own. Preserve:

- crun executable: GPL-2.0-or-later, `source/COPYING`;
- libcrun: LGPL-2.1-or-later, `source/COPYING.libcrun`;
- libocispec generator: GPL-3.0-or-later with the upstream parser-skeleton
  exception in `source/libocispec/COPYING`;
- OCI image/runtime schemas: Apache-2.0, their respective `LICENSE` files;
- portable BLAKE3: CC0-1.0 OR Apache-2.0, `source/src/libcrun/blake3/LICENSE`.

All original source-file notices remain intact. The installed host package
carries these notices, the source pin, build feature record, and the complete
corresponding source plus build/verification scripts under
`/usr/share/doc/<host-package>/sources/crun-source.tar.gz`. Thus a recipient has
the source even without access to a release server. Buzzard package updates own
updates to this private crun; distro crun updates do not update this copy.
