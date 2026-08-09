#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
task_uid=$(id -u)
build_root=${WILDBUZZARD_BUILD_ROOT:-"${TMPDIR:-/tmp}/wildbuzzard-build-$task_uid"}
test_root=${WILDBUZZARD_TEST_DIR:-"$build_root/tests"}
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
    "$project_dir/guest/third_party/trycua-cua/cua-driver/rust/Cargo.toml" \
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
        --manifest-path \
        "$project_dir/guest/third_party/trycua-cua/cua-driver/rust/Cargo.toml" \
        --package platform-linux --locked

python3 -m unittest discover -s "$project_dir/guest/tests" -v
python3 -m unittest discover -s "$project_dir/oci/tests" -v
python3 -m unittest discover -s "$project_dir/tools/tests" -v
for script in \
    "$project_dir/guest/install-rootfs-assets.sh" \
    "$project_dir/guest/assets/wildbuzzard-init" \
    "$project_dir/guest/assets/wildbuzzard-fusermount"; do
    sh -n "$script"
done
for script in \
    "$project_dir/host/build-appimage.sh" \
    "$project_dir/oci/build-local.sh" \
    "$project_dir/oci/verify-image.sh"; do
    bash -n "$script"
done

asset_root=$(mktemp -d "$test_root/guest-assets.XXXXXX")
cleanup() {
    rm -r -- "$asset_root"
}
trap cleanup EXIT
mkdir "$asset_root/rootfs"
"$project_dir/guest/install-rootfs-assets.sh" \
    "$asset_root/rootfs" \
    /bin/true \
    /bin/true
python3 - "$asset_root/rootfs" <<'PY'
import json
from pathlib import Path
import sys

root = Path(sys.argv[1])
manifest = json.loads(
    (root / "usr/lib/wildbuzzard/guest-assets.manifest.json").read_text()
)
assert manifest["schema"] == 1
for relative, record in manifest["assets"].items():
    path = root / relative
    assert path.is_file(), relative
    assert path.stat().st_mode & 0o7777 == record["mode"], relative
PY
trap - EXIT
cleanup

# Local source validation must remain usable while an explicitly recorded
# distribution-policy blocker (currently the proprietary CUDA payload) keeps
# public binary releases fail-closed. Artifact builds are audited separately,
# and the release gate intentionally runs without --structural.
"$project_dir/tools/check-licenses.sh" --structural
if command -v docker >/dev/null 2>&1; then
    docker compose --project-directory "$project_dir" \
        -f "$project_dir/oci/compose.yaml" \
        config --quiet
fi

printf 'All local source tests passed; outputs are under %s\n' "$test_root"
