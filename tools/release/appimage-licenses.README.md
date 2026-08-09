# Native AppImage notices and provenance

This directory is the native host/AppImage distribution surface. It is kept
separate from the guest root-filesystem notice group.

- `usr-share-doc/` is materialized from the exact audited AppImage and includes
  Wild Buzzard notices, the locked host package closure, dependency evidence,
  bundled upstream sources, and relink material where required.
- `usr-share-doc/wildbuzzard/sources/project/` contains a checksum-addressed
  archive of the exact clean Wild Buzzard Git commit used for the build.
- `../../provenance/appimage/` binds the AppImage hash and size to that source
  commit and corresponding-source archive.

Notice symlinks found inside the AppImage are resolved while creating this
portable evidence tree. The files remain byte-for-byte notice/source content;
the portable outer archive itself contains no symlinks or special files.

These records are compliance evidence, not legal advice. A publication job
must pass the repository's strict licensing gate; artifacts-only CI may use
the structural gate solely to report the explicitly recorded policy blockers.
