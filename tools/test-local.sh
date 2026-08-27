#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
task_uid=$(id -u)
build_root=${BUZZARDOS_BUILD_ROOT:-"${TMPDIR:-/tmp}/buzzardos-build-$task_uid"}
test_root=${BUZZARDOS_TEST_DIR:-"$build_root/tests"}
test_root=$(realpath -m -- "$test_root")
case "$test_root/" in
    "$project_dir/"*)
        echo "refusing to place test output inside the source repository: $test_root" >&2
        exit 1
        ;;
esac
mkdir -p "$test_root"

for command_name in cargo id python3 realpath; do
    command -v "$command_name" >/dev/null 2>&1 || {
        echo "local test dependency missing: $command_name" >&2
        exit 1
    }
done

cargo fmt --manifest-path "$project_dir/host/Cargo.toml" --all -- --check
cargo fmt --manifest-path "$project_dir/guest/Cargo.toml" --all -- --check
cargo fmt \
    --manifest-path \
    "$project_dir/cua/Cargo.toml" \
    --all -- --check

CARGO_TARGET_DIR="$test_root/host-target" \
    cargo clippy \
        --manifest-path "$project_dir/host/Cargo.toml" \
        --workspace --all-targets --locked -- -D warnings
CARGO_TARGET_DIR="$test_root/host-target" \
    cargo test \
        --manifest-path "$project_dir/host/Cargo.toml" \
        --workspace --locked

CARGO_TARGET_DIR="$test_root/guest-target" \
    cargo clippy \
        --manifest-path "$project_dir/guest/Cargo.toml" \
        --workspace --all-targets --locked -- -D warnings
CARGO_TARGET_DIR="$test_root/guest-target" \
    cargo test \
        --manifest-path "$project_dir/guest/Cargo.toml" \
        --workspace --locked

CARGO_TARGET_DIR="$test_root/cua-target" \
    cargo test \
        --manifest-path "$project_dir/cua/Cargo.toml" \
        --all-targets --locked

python3 -m unittest discover -s "$project_dir/guest/tests" -v
python3 -m unittest discover -s "$project_dir/oci/tests" -v
python3 -m unittest discover -s "$project_dir/tools/tests" -v
for script in \
    "$project_dir/guest/install-rootfs-assets.sh" \
    "$project_dir/guest/assets/buzzardos-init" \
    "$project_dir/guest/assets/buzzardos-fusermount"; do
    sh -n "$script"
done
for script in \
    "$project_dir/host/packaging/generate-icons.sh" \
    "$project_dir/packaging/build-debs.sh" \
    "$project_dir/tools/test-host-package-matrix.sh" \
    "$project_dir/oci/build-local.sh" \
    "$project_dir/oci/verify-image.sh" \
    "$project_dir/tests/acceptance/hardware-acceptance.sh"; do
    bash -n "$script"
done

asset_root=$(mktemp -d "$test_root/guest-assets.XXXXXX")
cleanup() {
    rm -rf -- "$asset_root"
}
trap cleanup EXIT
mkdir "$asset_root/rootfs"
"$project_dir/guest/install-rootfs-assets.sh" \
    "$asset_root/rootfs" \
    /bin/true
"$project_dir/guest/install-desktop-assets.sh" \
    "$asset_root/rootfs" \
    /bin/true \
    /bin/true \
    /bin/true
python3 - "$asset_root/rootfs" <<'PY'
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])
manifest = json.loads(
    (root / "usr/lib/buzzardos/guest-assets.manifest.json").read_text()
)
assert manifest["schema"] == 1
for relative, record in manifest["assets"].items():
    path = root / relative
    assert path.is_file(), relative
    assert path.stat().st_mode & 0o7777 == record["mode"], relative
revision = (root / "opt/buzzardos/runtime/current").readlink()
runtime = root / "opt/buzzardos/runtime" / revision
runtime_manifest = json.loads((runtime / "runtime.manifest.json").read_text())
assert runtime_manifest["revision"] == str(revision)
for required in (
    "libexec/buzzardos-clipboard-agent",
):
    assert (runtime / required).is_file(), required
for required in (
    "usr/bin/buzzardos-desktop",
    "usr/bin/buzzardos-settings",
    "usr/libexec/buzzardos-desktop/buzzardos-shortcut-helper",
):
    assert (root / required).is_file(), required
PY
trap - EXIT
cleanup

# Local source validation and exact binary-package audits are separate. A
# locally built machine may also be inspected, but no resulting OCI/rootfs is
# a Buzzard release artifact.
"$project_dir/tools/check-licenses.sh" --structural

printf 'All local source tests passed; outputs are under %s\n' "$test_root"
