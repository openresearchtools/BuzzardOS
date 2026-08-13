# Hosted portable artifact assembly

`build-release-assets.yml` is a manually dispatched, read-only workflow on a
disposable GitHub-hosted x86-64 runner. It builds the OCI image only inside the
runner's local Docker daemon, converts it into the verified OCI install seed,
then deletes all image and builder state.

The workflow uploads exactly one seven-day Actions artifact containing:

`BuzzardOS-portable-linux-x86_64.tar.xz`

`BuzzardOS-portable-linux-x86_64.tar.xz.sha256`

Its archive root is exactly `BuzzardOS/`, containing the executable launcher,
`Install-Dependencies`, the dependency-complete `app/` directory, empty
`Machines/` and `shared/` directories, the compressed OCI seed, checksums,
notices, corresponding source, and provenance.

There is no automatic trigger, publisher job, release/prerelease input, write
permission, registry login, or OCI push. The workflow cannot create or modify
a GitHub Release, tag, environment, Package, GHCR image, or other registry
object. The uploaded Actions artifact is an engineering build for inspection,
not publication.
