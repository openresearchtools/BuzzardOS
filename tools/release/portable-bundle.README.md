# Buzzard OS portable folder

Run `./BuzzardOS`. The launcher and its complete dependency payload are under
this directory, in the same style as Blender's extracted Linux distribution.
Buzzard OS itself is not an AppImage and does not require host FUSE.

`app/runtime/default-rootfs.oci.tar.zst` is the digest-bound OCI install seed.
The first launch imports it once into `Machines/default/rootfs/`. Later package
installs and system changes modify that persistent flat rootfs directly.

`Machines/` contains independent persistent machines. `shared/` is ordinary
host-owned storage mounted read/write at `/shared` in every running machine.
Copying this entire `BuzzardOS/` directory moves the launcher, install seed,
machines, and shared data together. Use `./BuzzardOS export NAME --output FILE`
when moving a materialized machine between hosts with different subordinate-ID
mappings; import the resulting OCI archive on the destination.

If the host lacks `newuidmap`/`newgidmap` or subordinate ranges, run
`./Install-Dependencies` once. This installs only the normal Debian/Ubuntu
`uidmap` prerequisite; Buzzard OS remains rootless and installs no daemon or
setuid program of its own.

Licenses are separated under `app/licenses/host/` and
`app/licenses/guest/`. Exact source, package, OCI, and artwork records are under
`app/provenance/`. `SHA256SUMS` covers every regular file in this folder.
