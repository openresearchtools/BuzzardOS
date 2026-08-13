# Guest rootfs notices and provenance

This directory is the guest/container distribution surface. It is deliberately
separate from the extracted host-application notice group.

- `usr-share-doc/` is copied from the exact audited rootfs and contains the
  Debian/NVIDIA package copyright and license files plus Buzzard OS, Sway,
  wlroots, Rust, TryCua, MPL, and other bundled notices/source evidence.
- `usr-share-common-licenses/` materializes Debian's common license texts
  referenced by package copyright records.
- `project-source/` contains a checksum-addressed archive of the exact clean
  Buzzard OS Git commit used for the build.
- `../../provenance/guest/` records base-image, Sway/wlroots, TryCua,
  package-inventory, source-OCI, and flattened-rootfs identities.

These records are evidence, not legal advice. Publication remains blocked until
every release blocker reported by `tools/check-licenses.sh` is resolved,
including the current CUDA/cuBLAS redistribution-rights review and the final
corresponding-source delivery policy for distribution package closures.
