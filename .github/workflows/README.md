# Hosted release-asset assembly

`build-release-assets.yml` is manually dispatched and does all expensive
distribution assembly on disposable GitHub-hosted x86-64 runners. It never
logs in to a container registry and never publishes GHCR or another GitHub
Package. The OCI image exists only inside that runner as a verified assembly
intermediate; the workflow flattens it into the persistent-rootfs seed.

The workflow uploads exactly two seven-day Actions artifacts:

- `WildBuzzard-x86_64.AppImage`
- `WildBuzzard-portable-x86_64.tar.zst`

The portable archive contains the same AppImage, the compressed flat-rootfs
seed, initial `vm/`, `shared/`, and `cache/` directories, and separate AppImage
and guest-rootfs license/provenance groups.

Before upload, the portable runner uses the built AppImage to create a
temporary machine from that exact seed through the normal subordinate-ID
namespace path. It compares the resulting content, metadata, and translated
ownership against the canonical flattened rootfs, then deletes the temporary
machine. This is an offline first-creation acceptance check, not a second
packaging implementation.

Ubuntu 24.04 runners keep their global unprivileged-user-namespace AppArmor
restriction enabled. For this recipient-path check only, the workflow copies
the checksum-verified AppImage to a root-owned, non-writable, run-specific
path and temporarily loads Canonical's exact-path `flags=(unconfined)` profile
with `userns` permission. An early `doctor` probe must prove the complete
keep-ID subordinate UID/GID map before the OCI build begins. The profile is
removed in an unconditional cleanup step; it does not change the AppImage or
the runtime policy on an end user's host.

This is the workflow's only mode. It has no automatic trigger, publisher job,
release/prerelease input, or write permission. It cannot create or modify a
GitHub Release, tag, environment, package, GHCR image, or other registry
object. The two Actions artifacts are engineering outputs for inspection, not
a publication action.

Release or prerelease publishing may be added only by a later, separately
reviewed explicit change. Such a change must design its own strict licensing
gate, tag/commit validation, approvals, and least-privilege write boundary;
none of that authority exists in the checked-in artifact workflow.
