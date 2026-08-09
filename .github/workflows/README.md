# No hosted package builds yet

Wild Buzzard intentionally has no active GitHub Actions workflow at this
stage. AppImage creation, OCI assembly, licensing gates, and hardware
acceptance run locally through the checked-in entry points. Nothing in this
repository publishes a GitHub Release, Actions artifact, container package, or
GHCR image.

The local scripts are structured so a later reviewed workflow can call the
same commands without changing their build contracts.
