# Buzzard OS portability handoff

This is the short restart handoff for the portable-folder and OCI work. The
authoritative product contract remains [`../AGENTS.md`](../AGENTS.md); this
file records the current implementation state and the next acceptance steps.

## Repository state

- Repository: `openresearchtools/BuzzardOS`
- Working branch: `main`
- The portability sequence immediately before this handoff is:
  - `5dfda6f` — portable OCI machine exchange
  - `fbf0efc` — complete portable OCI workflow
  - `1555602` — Ubuntu 24 SPA runtime packaging fix
  - `0f40692` — complete host-library closure
  - `c239596` — static-helper closure audit
- The commit containing this document removes the accidentally introduced LXC
  dependency and restores the bundled `unshare` namespace path.
- Generated app folders, OCI archives, screenshots, Cargo targets and release
  archives are intentionally outside the repository and are not committed.

## Product and source layout

The delivered archive expands to one relocatable Blender-style folder:

```text
BuzzardOS/
├── BuzzardOS
├── Install-Dependencies
├── app/                         bundled host application and helpers
├── Machines/                    persistent flat mutable machine rootfses
└── shared/                      host/guest files shared by every machine
```

The source repository is split as follows:

- `host/`: launcher, broker, display, portable-folder builder and entry points.
- `guest/`: Sway desktop, Settings, shell, CUA fork and managed rootfs assets.
- `oci/`: disposable reference-image construction; never an end-user runtime.
- `tools/`: rootfs seed build, release assembly, licensing and contract gates.
- `tests/acceptance/`: live desktop, hardware, CUA and OCI journeys.
- `LICENSES/`: machine-readable source, package and distribution evidence.

Normal machine operation is rootless and uses the already-expanded flat
`Machines/<name>/rootfs/`; it does not use Docker, Podman, LXC, FUSE, an
overlay filesystem or a system Buzzard OS service.

## Only installed host dependency

`Install-Dependencies` installs exactly one Debian/Ubuntu package:

```text
uidmap
```

That package supplies the distro-owned `newuidmap` and `newgidmap`
authorization gates. The portable folder supplies its own `unshare` and all
other Buzzard OS helpers. LXC is neither installed nor used.

On Ubuntu hosts that enforce AppArmor's unprivileged-user-namespace gate, the
installer also writes and loads one exact-path AppArmor profile for the
bundled `app/usr/libexec/wildbuzzard/unshare`. This is policy configuration,
not another package or a privileged Buzzard OS service. It never disables the
global AppArmor sysctl, never grants a wildcard executable path, and must be
rerun after relocating the portable folder.

The account must have non-overlapping subordinate UID and GID ranges of at
least 65,536 IDs. The bundled util-linux 2.39-compatible invocation is:

```text
--user
--map-users 0:<subuid-start>:65536
--map-user 1000
--map-groups 0:<subgid-start>:65536
--map-group 1000
--setuid 0 --setgid 0 --
```

It maps guest UID/GID 1000 to the logged-in host user and every other guest
identity into the subordinate range. The installer executes this exact probe
and fails with a specific diagnostic if the kernel, policy, helpers or ranges
are unsuitable. Host Wayland and GPU drivers are environmental prerequisites,
not packages installed by Buzzard OS. Host PipeWire is needed only for enabled
audio, microphone or camera integration.

## Completed portability evidence

- Ubuntu export to OCI, Debian restore and Debian re-export passed.
- Restore and clone identity behavior passed; clones received distinct machine
  identity.
- Numeric ownership, hardlinks, symlinks, xattrs, ACLs, capabilities, package
  state and desktop settings survived the cross-host round trip.
- Two machines ran simultaneously with separate systemd, Sway, PipeWire, CUA,
  networking and display state.
- The host application builds against the Ubuntu 24.04 / GLIBC 2.39 floor and
  its recursive portable ELF closure passed a Debian trixie relocation smoke.
- The bundled `unshare` plus system `newuidmap`/`newgidmap` path was exercised
  successfully with the exact map above, including mount, write and unmount.

## Resume after installing Debian

1. Clone the repository, check out `main`, and verify this handoff commit is
   present. Do not recover build output from the old workstation.
2. Run the source gates:

   ```sh
   cargo test --offline --locked --manifest-path host/Cargo.toml -p wb-core
   python3 -m unittest tools.tests.test_actions_artifact_workflow -v
   python3 -m unittest discover -s tools/tests -v
   ./tools/check-licenses.sh --structural
   ```

3. Build the portable `app/` outside the repository with
   `host/build-portable-app.sh`, using the pinned Ubuntu 24.04 builder defined
   by `.github/workflows/build-release-assets.yml`. Run the recursive GLIBC
   ceiling and Debian relocation checks from that workflow.
4. Install only `uidmap` through the generated
   `BuzzardOS/Install-Dependencies`, then rerun the real namespace/mount probe
   on Debian.
5. Build the flattened identity-free OCI seed:

   ```sh
   tools/build-release-rootfs.sh PORTABLE_ROOT ROOTFS_STAGE
   ```

6. Assemble the one final artifact:

   ```sh
   tools/assemble-release-assets.sh PORTABLE_ROOT ROOTFS_STAGE OUTPUT_DIR
   ```

7. Extract the resulting `BuzzardOS-portable-linux-x86_64.tar.xz` into a new
   directory and perform the complete live acceptance: first creation, start,
   stop, persistence, multiple machines, import, restore, clone, export,
   desktop controls, CUA, GPU and enabled media/clipboard paths.
8. Only after local acceptance, manually dispatch the existing GitHub Actions
   workflow. It is artifact-only: no release, tag, GHCR image or package.

Public distribution remains blocked until the recorded CUDA cuBLAS/runtime
redistribution-rights review is resolved. This licensing gate does not change
the rootless runtime architecture above.
