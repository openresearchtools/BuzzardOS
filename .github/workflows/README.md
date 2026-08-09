# Hosted release-asset assembly

`build-release-assets.yml` is manually dispatched and does all expensive
distribution assembly on disposable GitHub-hosted x86-64 runners. It never
logs in to a container registry and never publishes GHCR or another GitHub
Package. The OCI image exists only inside that runner as a verified assembly
intermediate; the workflow flattens it into the persistent-rootfs seed.

The default `artifacts` mode uploads two seven-day Actions artifacts and cannot
create a GitHub Release:

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

`prerelease` and `release` modes additionally require an explicit confirmation
and an existing SemVer tag that resolves to the selected workflow commit. They
run the strict licensing gate and grant `contents: write` only to the final
publisher job. Prereleases and production releases use the literal
`prerelease` and `production` GitHub environments respectively, so repository
owners can attach independent reviewers and protection rules. The selected
publisher creates a new Release with exactly the two files above and refuses
to replace an existing Release. Known unresolved licensing blockers therefore
prevent publication while still allowing artifact-only assembly for
engineering verification.
