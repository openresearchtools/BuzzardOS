#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

project_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
build_root=$(realpath -m -- "${1:?usage: build-crun.sh BUILD_DIRECTORY}")
case "$build_root/" in
    "$project_dir/"*|/) echo 'crun build output must be outside source' >&2; exit 1 ;;
esac
for tool in autoreconf make cc pkg-config python3 dpkg-buildflags dpkg-shlibdeps tar gzip; do
    command -v "$tool" >/dev/null || { echo "missing crun build dependency: $tool" >&2; exit 1; }
done
pkg-config --exists json-c
python3 "$project_dir/tools/crun_source.py"
vendor="$project_dir/third-party/crun"
crun_version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$vendor/UPSTREAM.toml")
crun_commit=$(sed -n 's/^commit = "\([^"]*\)"/\1/p' "$vendor/UPSTREAM.toml" | head -n1)
mkdir -p "$build_root/bin"
work=$(mktemp -d "$build_root/work.XXXXXX")
cp -a "$vendor/source/." "$work/"
# These are upstream's release-tarball inputs, generated only in the build copy.
# Neither upstream source nor any Git configuration is changed during build.
printf '%s\n' "$crun_version" >"$work/.tarball-version"
printf '#define GIT_VERSION "%s"\n' "$crun_commit" >"$work/.tarball-git-version.h"
(
    cd "$work"
    autoreconf -fi
    CFLAGS="$(dpkg-buildflags --get CFLAGS) -ffile-prefix-map=$work=." \
    CPPFLAGS="$(dpkg-buildflags --get CPPFLAGS)" \
    LDFLAGS="$(dpkg-buildflags --get LDFLAGS)" \
        ./configure --prefix=/usr --disable-shared --disable-libcrun \
        --enable-embedded-blake3 --enable-caps --enable-seccomp \
        --enable-systemd --enable-bpf
    make --jobs="${BUZZARDOS_BUILD_JOBS:-$(nproc)}"
    ./crun --version
    ./crun features >"$build_root/features.json"
    install -m 0755 crun "$build_root/bin/crun"
    mkdir -p debian
    printf 'Source: buzzardos\nSection: utils\nPriority: optional\nMaintainer: Open Research Tools <maintainers@openresearchtools.org>\n\nPackage: buzzardos\nArchitecture: any\nDescription: private crun runtime\n' >debian/control
    dpkg-shlibdeps -O -e "$build_root/bin/crun" | sed -n 's/^shlibs:Depends=//p' >"$build_root/depends"
    test -s "$build_root/depends"
)
python3 "$project_dir/tools/verify-elf-glibc-floor.py" --maximum 2.39 --root "$build_root/bin"
# Ship corresponding source, recursive dependencies and the exact build recipe
# alongside the executable, independently of GitHub/release availability.
source_bundle=$(mktemp -d "$build_root/corresponding-source.XXXXXX")
mkdir -p "$source_bundle/third-party" "$source_bundle/packaging" "$source_bundle/tools"
cp -a "$vendor" "$source_bundle/third-party/crun"
cp "$project_dir/packaging/build-crun.sh" "$source_bundle/packaging/"
cp "$project_dir/tools/crun_source.py" "$project_dir/tools/verify-elf-glibc-floor.py" "$source_bundle/tools/"
tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner \
    -C "$source_bundle" -cf - third-party packaging tools \
    | gzip -n -9 >"$build_root/crun-source.tar.gz"
