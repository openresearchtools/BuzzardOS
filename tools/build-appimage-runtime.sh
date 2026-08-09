#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

runtime_commit=75849dce7cc37e4319b633df1f116ca895c71a12
runtime_epoch=1782256868
zig_version=0.14.1
zig_archive_sha256=24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c

runtime_archive=type2-runtime-75849dce.tar.gz
runtime_archive_url="https://github.com/AppImage/type2-runtime/archive/$runtime_commit.tar.gz"
runtime_archive_sha256=b7af4960da4b90364e935a3281d04fad6560da4813c012414fa2f738291ad443
fuse_archive=fuse-3.15.0.tar.xz
fuse_archive_url=https://github.com/libfuse/libfuse/releases/download/fuse-3.15.0/fuse-3.15.0.tar.xz
fuse_archive_sha256=70589cfd5e1cff7ccd6ac91c86c01be340b227285c5e200baa284e401eea2ca0
squashfuse_archive=squashfuse-0.5.2.tar.gz
squashfuse_archive_url=https://github.com/vasi/squashfuse/archive/0.5.2.tar.gz
squashfuse_archive_sha256=db0238c5981dabbd80ee09ae15387f390091668ca060a7bc38047912491443d3
zstd_archive=zstd-1.5.6.tar.gz
zstd_archive_url=https://github.com/facebook/zstd/releases/download/v1.5.6/zstd-1.5.6.tar.gz
zstd_download_sha256=8c29e06cf42aacc1eafc4077ae2ec6c6fcb96a626157e0593d5e82a34fd403c1
zstd_archive_sha256=30f35f71c1203369dc979ecde0400ffea93c27391bfd2ac5a9715d2173d92ff7
zlib_archive=zlib-1.3.2.tar.gz
zlib_archive_url=https://github.com/madler/zlib/archive/refs/tags/v1.3.2.tar.gz
zlib_archive_sha256=b99a0b86c0ba9360ec7e78c4f1e43b1cbdf1e6936c8fa0f6835c0cd694a495a1
mimalloc_archive=mimalloc-2.1.7.tar.gz
mimalloc_archive_url=https://github.com/microsoft/mimalloc/archive/refs/tags/v2.1.7.tar.gz
mimalloc_archive_sha256=0eed39319f139afde8515010ff59baf24de9e47ea316a315398e8027d198202d
meson_wheel=meson-1.7.2-py3-none-any.whl
meson_wheel_url=https://files.pythonhosted.org/packages/e5/2b/46bda4ef5a7ae4135dbfe27fc0368c44e5a349a897a54fdf2cedb8dcb66e/meson-1.7.2-py3-none-any.whl
meson_wheel_sha256=82c6818dc81743c96de3a458f06175776ebfde4081195ea31ea6971838f25e38

usage() {
    cat <<'EOF'
Usage: tools/build-appimage-runtime.sh \
  --zig PATH --zig-archive PATH --source-cache DIR \
  --build-dir DIR --output-dir DIR [--offline] [--self-test]

Build the pinned x86_64-linux-musl AppImage Type-2 runtime and its LGPL
relink kit. All generated files are written below --build-dir/--output-dir.
EOF
}

zig=
zig_archive_path=
source_cache=
build_dir=
output_dir=
offline=false
self_test=false
while (($#)); do
    case "$1" in
        --zig)
            zig=${2:?missing value for --zig}
            shift 2
            ;;
        --zig-archive)
            zig_archive_path=${2:?missing value for --zig-archive}
            shift 2
            ;;
        --source-cache)
            source_cache=${2:?missing value for --source-cache}
            shift 2
            ;;
        --build-dir)
            build_dir=${2:?missing value for --build-dir}
            shift 2
            ;;
        --output-dir)
            output_dir=${2:?missing value for --output-dir}
            shift 2
            ;;
        --offline)
            offline=true
            shift
            ;;
        --self-test)
            self_test=true
            shift
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

for value_name in zig zig_archive_path source_cache build_dir output_dir; do
    if [[ -z "${!value_name}" ]]; then
        echo "missing required option: ${value_name//_/-}" >&2
        usage >&2
        exit 2
    fi
done

for command_name in curl cut dd file find gzip install make ninja od patch python3 readelf realpath sha256sum sort stat tar touch tr xargs; do
    if ! command -v "$command_name" >/dev/null 2>&1; then
        echo "runtime build dependency missing: $command_name" >&2
        exit 1
    fi
done
if [[ "$self_test" == true ]]; then
    for command_name in mksquashfs setpriv; do
        if ! command -v "$command_name" >/dev/null 2>&1; then
            echo "runtime self-test dependency missing: $command_name" >&2
            exit 1
        fi
    done
fi

zig=$(realpath -- "$zig")
zig_archive_path=$(realpath -- "$zig_archive_path")
source_cache=$(realpath -m -- "$source_cache")
build_dir=$(realpath -m -- "$build_dir")
output_dir=$(realpath -m -- "$output_dir")
script_path=$(realpath -- "${BASH_SOURCE[0]}")
project_dir=$(realpath -- "$(dirname -- "$script_path")/..")
[[ -x "$zig" ]] || {
    echo "Zig compiler is not executable: $zig" >&2
    exit 1
}
[[ -f "$zig_archive_path" ]] || {
    echo "Zig archive does not exist: $zig_archive_path" >&2
    exit 1
}
if [[ "$($zig version)" != "$zig_version" ]]; then
    echo "Zig version mismatch: expected $zig_version, found $($zig version)" >&2
    exit 1
fi
printf '%s  %s\n' "$zig_archive_sha256" "$zig_archive_path" | sha256sum --check -

case "$build_dir" in
    /|"$HOME"|"$source_cache")
        echo "refusing unsafe runtime build directory: $build_dir" >&2
        exit 1
        ;;
esac
case "$output_dir" in
    /|"$HOME"|"$source_cache")
        echo "refusing unsafe runtime output directory: $output_dir" >&2
        exit 1
        ;;
esac
for generated_path in "$build_dir" "$output_dir"; do
    case "$generated_path/" in
        "$project_dir/"*)
            echo "refusing to place runtime build output inside the source repository: $generated_path" >&2
            exit 1
            ;;
    esac
done
for protected_path in "$project_dir" "$source_cache" "$output_dir"; do
    case "$protected_path/" in
        "$build_dir/"*)
            echo "refusing runtime build directory that contains protected data: $build_dir" >&2
            exit 1
            ;;
    esac
done
build_marker="$build_dir/.wildbuzzard-appimage-runtime-build"
if [[ -d "$build_dir" && ! -f "$build_marker" ]] &&
    find "$build_dir" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo "refusing to replace an unmarked runtime build directory: $build_dir" >&2
    exit 1
fi

export LC_ALL=C
export LANG=C
export TZ=UTC
export SOURCE_DATE_EPOCH=$runtime_epoch
export ZERO_AR_DATE=1
umask 022

mkdir -p "$source_cache" "$output_dir"
rm -rf -- "$build_dir"
mkdir -p "$build_dir" "$build_dir/src" "$build_dir/obj" "$build_dir/sysroot/usr/include" \
    "$build_dir/sysroot/usr/lib" "$build_dir/zig-cache/global" "$build_dir/zig-cache/local"
touch "$build_marker"
export ZIG_GLOBAL_CACHE_DIR="$build_dir/zig-cache/global"
export ZIG_LOCAL_CACHE_DIR="$build_dir/zig-cache/local"

fetch() {
    local filename=$1
    local url=$2
    local expected=$3
    local destination="$source_cache/$filename"
    if [[ -f "$destination" ]] &&
        [[ "$(sha256sum "$destination" | cut -d' ' -f1)" == "$expected" ]]; then
        return
    fi
    if [[ "$offline" == true ]]; then
        echo "offline source cache is missing or corrupt: $destination" >&2
        exit 1
    fi
    curl --fail --location --retry 3 --output "$destination.tmp" "$url"
    printf '%s  %s\n' "$expected" "$destination.tmp" | sha256sum --check -
    mv -f -- "$destination.tmp" "$destination"
}

# GitHub's zstd release asset carries a timestamped gzip wrapper. Verify those
# exact upstream bytes first, then deterministically recompress the unchanged
# tar stream with gzip's timestamp/name fields disabled. The normalized archive
# is the reproducible build and relink-kit input recorded by the license audit.
fetch_normalized_gzip() {
    local filename=$1
    local url=$2
    local download_expected=$3
    local normalized_expected=$4
    local destination="$source_cache/$filename"
    local downloaded="$destination.download.tmp"
    local normalized="$destination.normalized.tmp"
    if [[ -f "$destination" ]] &&
        [[ "$(sha256sum "$destination" | cut -d' ' -f1)" == "$normalized_expected" ]]; then
        return
    fi
    if [[ "$offline" == true ]]; then
        echo "offline source cache is missing or corrupt: $destination" >&2
        exit 1
    fi
    rm -f -- "$downloaded" "$normalized"
    curl --fail --location --retry 3 --output "$downloaded" "$url"
    printf '%s  %s\n' "$download_expected" "$downloaded" | sha256sum --check -
    gzip -dc -- "$downloaded" | gzip -n > "$normalized"
    printf '%s  %s\n' "$normalized_expected" "$normalized" | sha256sum --check -
    mv -f -- "$normalized" "$destination"
    rm -f -- "$downloaded" "$destination.tmp"
}

fetch "$runtime_archive" "$runtime_archive_url" "$runtime_archive_sha256"
fetch "$fuse_archive" "$fuse_archive_url" "$fuse_archive_sha256"
fetch "$squashfuse_archive" "$squashfuse_archive_url" "$squashfuse_archive_sha256"
fetch_normalized_gzip \
    "$zstd_archive" "$zstd_archive_url" \
    "$zstd_download_sha256" "$zstd_archive_sha256"
fetch "$zlib_archive" "$zlib_archive_url" "$zlib_archive_sha256"
fetch "$mimalloc_archive" "$mimalloc_archive_url" "$mimalloc_archive_sha256"
fetch "$meson_wheel" "$meson_wheel_url" "$meson_wheel_sha256"

tar -xzf "$source_cache/$runtime_archive" -C "$build_dir/src" --no-same-owner
tar -xJf "$source_cache/$fuse_archive" -C "$build_dir/src" --no-same-owner
tar -xzf "$source_cache/$squashfuse_archive" -C "$build_dir/src" --no-same-owner
tar -xzf "$source_cache/$zstd_archive" -C "$build_dir/src" --no-same-owner
tar -xzf "$source_cache/$zlib_archive" -C "$build_dir/src" --no-same-owner
tar -xzf "$source_cache/$mimalloc_archive" -C "$build_dir/src" --no-same-owner
python3 -m zipfile -e "$source_cache/$meson_wheel" "$build_dir/meson"

runtime_source="$build_dir/src/type2-runtime-$runtime_commit"
fuse_source="$build_dir/src/fuse-3.15.0"
squashfuse_source="$build_dir/src/squashfuse-0.5.2"
zstd_source="$build_dir/src/zstd-1.5.6"
zlib_source="$build_dir/src/zlib-1.3.2"
mimalloc_source="$build_dir/src/mimalloc-2.1.7"
sysroot="$build_dir/sysroot"
obj="$build_dir/obj"
cc="$zig cc -target x86_64-linux-musl"
ar="$zig ar"
ranlib="$zig ranlib"
canonical_source=/usr/src/wildbuzzard-appimage-runtime
common_cflags="-Os -g0 -fPIC -ffunction-sections -fdata-sections -fno-ident -fno-record-gcc-switches -ffile-prefix-map=$build_dir=$canonical_source -fdebug-prefix-map=$build_dir=$canonical_source -fmacro-prefix-map=$build_dir=$canonical_source"

# In addition to this build script's AGPL-3.0-or-later license, Open Research
# Tools licenses every C fragment emitted by the transformation below under
# the MIT license recorded at
# LICENSES/wildbuzzard-appimage-runtime-patch-MIT. This keeps the resulting
# patched Type-2 runtime under the same permissive terms as its upstream C.
#
# A confined launcher can successfully open /dev/fuse but have its inherited
# libfuse communication socket closed during the setuid fusermount3 exec. The
# upstream runtime currently reports a mount failure and exits in that case.
# Keep native FUSE as the first choice, but make ordinary application launches
# transparently re-enter the runtime's existing extract-and-run path if the
# mount never materializes. An explicit --appimage-mount remains a strict FUSE
# operation and continues to report the real error.
python3 - "$runtime_source/src/runtime/runtime.c" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
source = path.read_text(encoding="utf-8")

old_child = '''        if (0 != fusefs_main(5, child_argv, fuse_mounted)) {
            char* title;
            char* body;
            title = "Cannot mount AppImage, please check your FUSE setup.";
            body = "You might still be able to extract the contents of this AppImage \\n"
                   "if you run it with the --appimage-extract option. \\n"
                   "See https://github.com/AppImage/AppImageKit/wiki/FUSE \\n"
                   "for more information";
            printf("\\n%s\\n", title);
            printf("%s\\n", body);
        };
'''
new_child = '''        if (0 != fusefs_main(5, child_argv, fuse_mounted)) {
            if (arg && strcmp(arg, "appimage-mount") == 0) {
                fprintf(stderr, "Cannot mount AppImage; the host FUSE setup rejected the mount.\\n");
            } else if (verbose) {
                fprintf(stderr, "Native FUSE mount unavailable; selecting temporary extract-and-run.\\n");
            }
        };
'''

old_parent = '''        dir_fd = open(mount_dir, O_RDONLY);
        if (dir_fd == -1) {
            perror("open dir error");
            exit(EXIT_EXECERROR);
        }
'''

old_native_keepalive_pipe = '''    if (pipe(keepalive_pipe) == -1) {
        perror("pipe error");
        exit(EXIT_EXECERROR);
    }
'''
new_native_keepalive_pipe = '''    keepalive_pipe[0] = -1;
    keepalive_pipe[1] = -1;
    if (pipe(keepalive_pipe) == -1 ||
            !normalize_appimage_lease_pipe(keepalive_pipe)) {
        perror("pipe error");
        if (keepalive_pipe[0] != -1)
            close(keepalive_pipe[0]);
        if (keepalive_pipe[1] != -1)
            close(keepalive_pipe[1]);
        exit(EXIT_EXECERROR);
    }
'''

old_temp_directory = '''    {
        const char* const TMPDIR = getenv("TMPDIR");
        if (TMPDIR != NULL)
            strcpy(temp_base, getenv("TMPDIR"));
    }
'''
new_temp_directory = '''    {
        const char* const TMPDIR = getenv("TMPDIR");
        if (TMPDIR != NULL) {
            size_t temp_length = strlen(TMPDIR);
            if (temp_length == 0 || temp_length >= sizeof(temp_base)) {
                fprintf(stderr, "TMPDIR is empty or exceeds the AppImage runtime path limit\\n");
                exit(EXIT_EXECERROR);
            }
            memcpy(temp_base, TMPDIR, temp_length + 1);
        }
    }
'''

old_cleanup = '''    int rv = nftw(path, &rm_recursive_callback, 0, FTW_DEPTH | FTW_MOUNT | FTW_PHYS);
'''
new_cleanup = '''    int rv = nftw(path, &rm_recursive_callback, 64, FTW_DEPTH | FTW_MOUNT | FTW_PHYS);
'''

old_lease_helpers = '''    return rv == 0;
}

void build_mount_point'''
new_lease_helpers = '''    return rv == 0;
}

static int appimage_lease_has_writer(const int descriptor) {
    int flags = fcntl(descriptor, F_GETFL);
    if (flags == -1 || fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) == -1)
        return -1;

    for (;;) {
        char byte;
        ssize_t bytes = read(descriptor, &byte, sizeof(byte));
        if (bytes == 0)
            return 0;
        if (bytes > 0)
            continue;
        if (errno == EINTR)
            continue;
        if (errno == EAGAIN || errno == EWOULDBLOCK)
            return 1;
        return -1;
    }
}

static bool normalize_appimage_lease_pipe(int descriptors[2]) {
    for (int index = 0; index < 2; ++index) {
        if (descriptors[index] > STDERR_FILENO && descriptors[index] != 1023)
            continue;
        int descriptor_flags = fcntl(descriptors[index], F_GETFD);
        if (descriptor_flags == -1)
            return false;
        int duplicate_command = (descriptor_flags & FD_CLOEXEC) != 0
            ? F_DUPFD_CLOEXEC : F_DUPFD;
        int minimum = descriptors[index] == 1023 ? 1024 : STDERR_FILENO + 1;
        int replacement = fcntl(descriptors[index], duplicate_command, minimum);
        if (replacement == -1)
            return false;
        close(descriptors[index]);
        descriptors[index] = replacement;
    }
    return true;
}

static bool wait_for_appimage_lease_release(const int descriptor) {
    int flags = fcntl(descriptor, F_GETFL);
    if (flags == -1 || fcntl(descriptor, F_SETFL, flags & ~O_NONBLOCK) == -1)
        return false;

    for (;;) {
        char buffer[32];
        ssize_t bytes = read(descriptor, buffer, sizeof(buffer));
        if (bytes == 0)
            return true;
        if (bytes > 0)
            continue;
        if (errno == EINTR)
            continue;
        return false;
    }
}

static bool remove_extracted_appdir(const char* const path) {
    for (int attempt = 0; attempt < 20; ++attempt) {
        if (access(path, F_OK) == -1 && errno == ENOENT)
            return true;
        if (rm_recursive(path))
            return true;
        usleep(50000);
    }
    return false;
}

static void close_unrelated_descriptors(const int lease_descriptor) {
    DIR* descriptors = opendir("/proc/self/fd");
    if (descriptors != NULL) {
        int scan_descriptor = dirfd(descriptors);
        struct dirent* entry;
        while ((entry = readdir(descriptors)) != NULL) {
            char* end = NULL;
            long descriptor = strtol(entry->d_name, &end, 10);
            if (end == entry->d_name || *end != '\\0')
                continue;
            if (descriptor > STDERR_FILENO &&
                    descriptor != lease_descriptor && descriptor != scan_descriptor)
                close((int) descriptor);
        }
        closedir(descriptors);
        return;
    }

    long maximum = sysconf(_SC_OPEN_MAX);
    if (maximum < 0)
        maximum = 65536;
    for (int descriptor = STDERR_FILENO + 1; descriptor < maximum; ++descriptor) {
        if (descriptor != lease_descriptor)
            close(descriptor);
    }
}

/*
 * AppRun can intentionally leave Wild Buzzard's detached broker alive. The
 * broker inherits the write end of this pipe and owns it until shutdown. Let
 * the foreground AppImage process return immediately, while a session-detached
 * reaper retains only the read end and private extraction path. Normal commands
 * have no surviving writer and are cleaned synchronously.
 */
static bool release_or_defer_extracted_appdir(
        const char* const path, const int lease_descriptor) {
    if (getenv("NO_CLEANUP") != NULL) {
        close(lease_descriptor);
        return true;
    }

    int has_writer = appimage_lease_has_writer(lease_descriptor);
    if (has_writer == 0) {
        close(lease_descriptor);
        return remove_extracted_appdir(path);
    }
    if (has_writer == -1) {
        fprintf(stderr, "Failed to inspect AppImage runtime lease: %s\\n", strerror(errno));
        close(lease_descriptor);
        return false;
    }

    pid_t reaper = fork();
    if (reaper == -1) {
        fprintf(stderr, "Failed to start AppImage cleanup reaper: %s\\n", strerror(errno));
        close(lease_descriptor);
        /* Preserve the live AppDir rather than block or delete under a broker. */
        return false;
    }
    if (reaper == 0) {
        (void) setsid();
        signal(SIGHUP, SIG_IGN);
        int null_descriptor = open("/dev/null", O_RDWR | O_CLOEXEC);
        if (null_descriptor != -1) {
            (void) dup2(null_descriptor, STDIN_FILENO);
            (void) dup2(null_descriptor, STDOUT_FILENO);
            (void) dup2(null_descriptor, STDERR_FILENO);
            if (null_descriptor > STDERR_FILENO)
                close(null_descriptor);
        } else {
            close(STDIN_FILENO);
            close(STDOUT_FILENO);
            close(STDERR_FILENO);
        }
        close_unrelated_descriptors(lease_descriptor);
        bool released = wait_for_appimage_lease_release(lease_descriptor);
        close(lease_descriptor);
        _exit(released && remove_extracted_appdir(path) ? 0 : EXIT_EXECERROR);
    }

    close(lease_descriptor);
    return true;
}

void build_mount_point'''

old_extract_directory = '''        char* hexlified_digest = NULL;

        // calculate MD5 hash of file, and use it to make extracted directory name "content-aware"
        // see https://github.com/AppImage/AppImageKit/issues/841 for more information
        {
            FILE* f = fopen(appimage_path, "rb");
            if (f == NULL) {
                perror("Failed to open AppImage file");
                exit(EXIT_EXECERROR);
            }

            Md5Context ctx;
            Md5Initialise(&ctx);

            char buf[4096];
            size_t bytes_read;
            while ((bytes_read = fread(buf, sizeof(char), sizeof(buf), f)) > 0) {
                Md5Update(&ctx, buf, (uint32_t) bytes_read);
            }

            MD5_HASH digest;
            Md5Finalise(&ctx, &digest);

            hexlified_digest = appimage_hexlify((const char*)digest.bytes, sizeof(digest.bytes));
        }

        char* prefix = malloc(strlen(temp_base) + 20 + strlen(hexlified_digest) + 2);
        strcpy(prefix, temp_base);
        strcat(prefix, "/appimage_extracted_");
        strcat(prefix, hexlified_digest);
        free(hexlified_digest);
'''
new_extract_directory = '''        char* prefix = malloc(strlen(temp_base) + sizeof("/appimage_extracted_XXXXXX"));
        strcpy(prefix, temp_base);
        strcat(prefix, "/appimage_extracted_XXXXXX");
        if (mkdtemp(prefix) == NULL) {
            perror("create private extract-and-run directory");
            free(prefix);
            exit(EXIT_EXECERROR);
        }
'''

old_extract_failure = '''        if (!extract_appimage(appimage_path, prefix, NULL, false, verbose)) {
            fprintf(stderr, "Failed to extract AppImage\\n");
            exit(EXIT_EXECERROR);
        }
'''
new_extract_failure = '''        if (!extract_appimage(appimage_path, prefix, NULL, false, verbose)) {
            fprintf(stderr, "Failed to extract AppImage\\n");
            remove_extracted_appdir(prefix);
            free(prefix);
            exit(EXIT_EXECERROR);
        }
'''

old_extract_fork = '''        int pid;
        if ((pid = fork()) == -1) {
            int error = errno;
            fprintf(stderr, "fork() failed: %s\\n", strerror(error));
            exit(EXIT_EXECERROR);
        } else if (pid == 0) {
'''
new_extract_fork = '''        int extract_lease_pipe[2] = {-1, -1};
        if (pipe2(extract_lease_pipe, O_CLOEXEC) == -1 ||
                !normalize_appimage_lease_pipe(extract_lease_pipe)) {
            fprintf(stderr, "AppImage lease pipe failed: %s\\n", strerror(errno));
            if (extract_lease_pipe[0] != -1)
                close(extract_lease_pipe[0]);
            if (extract_lease_pipe[1] != -1)
                close(extract_lease_pipe[1]);
            remove_extracted_appdir(prefix);
            exit(EXIT_EXECERROR);
        }

        int pid;
        if ((pid = fork()) == -1) {
            int error = errno;
            close(extract_lease_pipe[0]);
            close(extract_lease_pipe[1]);
            remove_extracted_appdir(prefix);
            fprintf(stderr, "fork() failed: %s\\n", strerror(error));
            exit(EXIT_EXECERROR);
        } else if (pid == 0) {
            close(extract_lease_pipe[0]);
            int lease_flags = fcntl(extract_lease_pipe[1], F_GETFD);
            if (lease_flags == -1 ||
                    fcntl(extract_lease_pipe[1], F_SETFD, lease_flags & ~FD_CLOEXEC) == -1) {
                fprintf(stderr, "AppImage lease handoff failed: %s\\n", strerror(errno));
                exit(EXIT_EXECERROR);
            }
'''

old_extract_environment = '''            setenv("APPIMAGE", fullpath, 1);
            setenv("ARGV0", argv0_path, 1);
            setenv("APPDIR", prefix, 1);

            set_portable_home_and_config(fullpath);

            execv(apprun_path, new_argv);
'''
new_extract_environment = '''            setenv("APPIMAGE", fullpath, 1);
            setenv("ARGV0", argv0_path, 1);
            setenv("APPDIR", prefix, 1);
            char lease_descriptor[32];
            snprintf(lease_descriptor, sizeof(lease_descriptor), "%d", extract_lease_pipe[1]);
            setenv("WILDBUZZARD_APPIMAGE_LEASE_FD", lease_descriptor, 1);

            set_portable_home_and_config(fullpath);

            execv(apprun_path, new_argv);
'''

old_extract_wait = '''            free(apprun_path);
            exit(EXIT_EXECERROR);
        }

        int status = 0;
        int rv = waitpid(pid, &status, 0);
        status = rv > 0 && WIFEXITED (status) ? WEXITSTATUS (status) : EXIT_EXECERROR;

        if (getenv("NO_CLEANUP") == NULL) {
            if (!rm_recursive(prefix)) {
                fprintf(stderr, "Failed to clean up cache directory\\n");
                if (status == 0)        /* avoid messing existing failure exit status */
                    status = EXIT_EXECERROR;
            }
        }
'''
new_extract_wait = '''            free(apprun_path);
            exit(EXIT_EXECERROR);
        }

        close(extract_lease_pipe[1]);
        int status = 0;
        int rv = waitpid(pid, &status, 0);
        status = rv > 0 && WIFEXITED (status) ? WEXITSTATUS (status) : EXIT_EXECERROR;

        if (!release_or_defer_extracted_appdir(prefix, extract_lease_pipe[0])) {
            fprintf(stderr, "Failed to clean up cache directory\\n");
            if (status == 0)        /* avoid messing existing failure exit status */
                status = EXIT_EXECERROR;
        }
'''

old_fuse_environment = '''        setenv("APPIMAGE", fullpath, 1);
        setenv("ARGV0", argv0_path, 1);
        setenv("APPDIR", mount_dir, 1);

        set_portable_home_and_config(fullpath);
'''

old_mount_directory_descriptor = '''        res = dup2(dir_fd, 1023);
        if (res == -1) {
            perror("dup2 error");
            exit(EXIT_EXECERROR);
        }
        close(dir_fd);
'''
new_mount_directory_descriptor = '''        res = dup2(dir_fd, 1023);
        if (res == -1) {
            perror("dup2 error");
            exit(EXIT_EXECERROR);
        }
        if (dir_fd != 1023)
            close(dir_fd);
'''
new_fuse_environment = '''        setenv("APPIMAGE", fullpath, 1);
        setenv("ARGV0", argv0_path, 1);
        setenv("APPDIR", mount_dir, 1);
        char lease_descriptor[32];
        snprintf(lease_descriptor, sizeof(lease_descriptor), "%d", keepalive_pipe[0]);
        setenv("WILDBUZZARD_APPIMAGE_LEASE_FD", lease_descriptor, 1);

        set_portable_home_and_config(fullpath);
'''
new_parent = '''        dir_fd = open(mount_dir, O_RDONLY);
        if (dir_fd == -1) {
            int mount_error = errno;
            if (!(arg && strcmp(arg, "appimage-mount") == 0)) {
                if (rmdir(mount_dir) == -1 && errno != ENOENT) {
                    perror("remove failed FUSE mount directory");
                    exit(EXIT_EXECERROR);
                }
                if (verbose)
                    fprintf(stderr, "Re-executing AppImage with automatic extract-and-run fallback.\\n");
                close(keepalive_pipe[0]);
                if (setenv("APPIMAGE_EXTRACT_AND_RUN", "1", 1) == -1) {
                    perror("set extract-and-run fallback");
                    exit(EXIT_EXECERROR);
                }
                execv(appimage_path, argv);
                perror("exec extract-and-run fallback");
                exit(EXIT_EXECERROR);
            }
            errno = mount_error;
            perror("open FUSE mount directory");
            exit(EXIT_EXECERROR);
        }
'''

for description, old, new in (
    ("mount failure reporting", old_child, new_child),
    ("automatic extract-and-run fallback", old_parent, new_parent),
    ("native FUSE keepalive descriptor normalization", old_native_keepalive_pipe, new_native_keepalive_pipe),
    ("bounded temporary directory", old_temp_directory, new_temp_directory),
    ("temporary extraction cleanup descriptor budget", old_cleanup, new_cleanup),
    ("AppDir lease cleanup helpers", old_lease_helpers, new_lease_helpers),
    ("private randomized extract-and-run directory", old_extract_directory, new_extract_directory),
    ("failed extraction cleanup", old_extract_failure, new_extract_failure),
    ("extract-and-run lease creation", old_extract_fork, new_extract_fork),
    ("extract-and-run lease environment", old_extract_environment, new_extract_environment),
    ("extract-and-run deferred cleanup", old_extract_wait, new_extract_wait),
    ("native FUSE lease environment", old_fuse_environment, new_fuse_environment),
    ("native FUSE reserved directory descriptor", old_mount_directory_descriptor, new_mount_directory_descriptor),
):
    if source.count(old) != 1:
        raise SystemExit(f"upstream runtime {description} context changed")
    source = source.replace(old, new)

path.write_text(source, encoding="utf-8")
PY

# libfuse is the one LGPL component in the runtime. Apply the exact upstream
# Type-2 runtime patch before compiling the static PIC archive.
patch --directory="$fuse_source" --strip=1 --forward \
    < "$runtime_source/patches/libfuse/mount.c.diff"
CC="$cc" AR="$ar" CFLAGS="$common_cflags" \
PYTHONPATH="$build_dir/meson" \
    python3 -m mesonbuild.mesonmain setup \
        "$build_dir/fuse-build" "$fuse_source" \
        --buildtype=minsize \
        --default-library=static \
        --prefix=/usr \
        -Db_ndebug=true \
        -Dexamples=false \
        -Dtests=false \
        -Dutils=false
PYTHONPATH="$build_dir/meson" \
    python3 -m mesonbuild.mesonmain compile -C "$build_dir/fuse-build"
DESTDIR="$sysroot" PYTHONPATH="$build_dir/meson" \
    python3 -m mesonbuild.mesonmain install -C "$build_dir/fuse-build" --no-rebuild
mapfile -t fuse_archives < <(find "$sysroot/usr/lib" -type f -name libfuse3.a -print)
if ((${#fuse_archives[@]} != 1)); then
    echo "expected one installed libfuse3.a, found ${#fuse_archives[@]}" >&2
    exit 1
fi
install -m644 "${fuse_archives[0]}" "$sysroot/usr/lib/libfuse3.a"

# Build zlib's static PIC archive with the pinned Zig/musl compiler.
(
    cd "$zlib_source"
    CC="$cc" AR="$ar" RANLIB="$ranlib" CFLAGS="$common_cflags" \
        ./configure --static --prefix=/usr
    make -j"$(nproc)" libz.a
    make DESTDIR="$sysroot" install-libs
    install -m644 zlib.h zconf.h "$sysroot/usr/include/"
)

# zstd's release archive contains a complete deterministic static-library
# Makefile; no host package metadata enters the result.
make -C "$zstd_source/lib" -j"$(nproc)" libzstd.a \
    CC="$cc" AR="$ar" CFLAGS="$common_cflags"
install -m644 "$zstd_source/lib/libzstd.a" "$sysroot/usr/lib/libzstd.a"
install -m644 "$zstd_source/lib/zstd.h" "$zstd_source/lib/zdict.h" \
    "$zstd_source/lib/zstd_errors.h" "$sysroot/usr/include/"

# The static mimalloc target is one translation unit. These definitions match
# mimalloc's upstream static musl configuration without involving host CMake.
mkdir -p "$obj/mimalloc"
$cc $common_cflags -DMI_STATIC_LIB -DMI_LIBC_MUSL=1 \
    -ftls-model=local-dynamic -fno-builtin-malloc \
    -I"$mimalloc_source/include" \
    -c "$mimalloc_source/src/static.c" -o "$obj/mimalloc/static.o"
$ar rcsD "$sysroot/usr/lib/libmimalloc.a" "$obj/mimalloc/static.o"
install -m644 "$mimalloc_source/include/mimalloc.h" "$sysroot/usr/include/mimalloc.h"

# squashfuse 0.5.2 does not ship generated Autotools outputs. Build its two
# archives directly from the authoritative Makefile.am source lists so the
# result does not depend on an unpinned host Autoconf/Automake/libtool stack.
cat > "$squashfuse_source/config.h" <<'EOF'
#ifndef SQFS_CONFIG_H
#define SQFS_CONFIG_H
#define FUSE_USE_VERSION 32
#define HAVE_ASM_BYTEORDER_H 1
#define HAVE_DECL_FUSE_ADD_DIRENT 0
#define HAVE_DECL_FUSE_ADD_DIRENTRY 1
#define HAVE_DECL_FUSE_CMDLINE_HELP 1
#define HAVE_DECL_FUSE_DAEMONIZE 1
#define HAVE_DECL_FUSE_SESSION_REMOVE_CHAN 0
#define HAVE_DLFCN_H 1
#define HAVE_ENDIAN_H 1
#define HAVE_FUSE_LL_FORGET_OP_64T 1
#define HAVE_INTTYPES_H 1
#define HAVE_LIBPTHREAD 1
#define HAVE_LINUX_TYPES_LE16 1
#define HAVE_STDINT_H 1
#define HAVE_STDIO_H 1
#define HAVE_STDLIB_H 1
#define HAVE_STRINGS_H 1
#define HAVE_STRING_H 1
#define HAVE_SYS_STAT_H 1
#define HAVE_SYS_SYSMACROS_H 1
#define HAVE_SYS_TYPES_H 1
#define HAVE_UNISTD_H 1
#define HAVE_WCHAR_H 1
#define HAVE_ZLIB_H 1
#define HAVE_ZSTD_H 1
#define PACKAGE "squashfuse"
#define PACKAGE_NAME "squashfuse"
#define PACKAGE_STRING "squashfuse 0.5.2"
#define PACKAGE_TARNAME "squashfuse"
#define PACKAGE_VERSION "0.5.2"
#define SQFS_MULTITHREADED 1
#define STDC_HEADERS 1
#define VERSION "0.5.2"
#ifndef _ALL_SOURCE
#define _ALL_SOURCE 1
#endif
#ifndef _GNU_SOURCE
#define _GNU_SOURCE 1
#endif
#ifndef _NETBSD_SOURCE
#define _NETBSD_SOURCE 1
#endif
#ifndef _OPENBSD_SOURCE
#define _OPENBSD_SOURCE 1
#endif
#ifndef _POSIX_PTHREAD_SEMANTICS
#define _POSIX_PTHREAD_SEMANTICS 1
#endif
#endif
EOF
(
    cd "$squashfuse_source"
    SED=sed sh ./gen_swap.sh ./squashfs_fs.h
)
squash_flags="$common_cflags -DHAVE_CONFIG_H -pthread -I$squashfuse_source -I$sysroot/usr/include -I$sysroot/usr/include/fuse3"
common_sources=(
    swap.c cache.c table.c dir.c file.c fs.c decompress.c xattr.c hash.c
    stack.c traverse.c util.c nonstd-pread.c nonstd-stat.c cache_mt.c
)
private_sources=(fuseprivate.c nonstd-makedev.c nonstd-enoattr.c stat.c)
ll_sources=(ll.c ll_inode.c nonstd-daemon.c)
mkdir -p "$obj/squashfuse/common" "$obj/squashfuse/private" "$obj/squashfuse/ll"
common_objects=()
for source in "${common_sources[@]}"; do
    object="$obj/squashfuse/common/${source%.c}.o"
    $cc $squash_flags -c "$squashfuse_source/$source" -o "$object"
    common_objects+=("$object")
done
private_objects=()
for source in "${private_sources[@]}"; do
    object="$obj/squashfuse/private/${source%.c}.o"
    $cc $squash_flags -c "$squashfuse_source/$source" -o "$object"
    private_objects+=("$object")
done
ll_objects=()
for source in "${ll_sources[@]}"; do
    object="$obj/squashfuse/ll/${source%.c}.o"
    $cc $squash_flags -c "$squashfuse_source/$source" -o "$object"
    ll_objects+=("$object")
done
$ar rcsD "$sysroot/usr/lib/libsquashfuse.a" "${common_objects[@]}"
$ar rcsD "$sysroot/usr/lib/libsquashfuse_ll.a" \
    "${private_objects[@]}" "${common_objects[@]}" "${ll_objects[@]}"
mkdir -p "$sysroot/usr/include/squashfuse"
install -m644 \
    "$squashfuse_source"/{cache.h,common.h,config.h,decompress.h,dir.h,file.h,fs.h,fuseprivate.h,hash.h,ll.h,nonstd-internal.h,nonstd.h,squashfs_fs.h,squashfuse.h,stack.h,stat.h,swap.h,table.h,traverse.h,util.h,xattr.h} \
    "$sysroot/usr/include/squashfuse/"

# Preserve the upstream marker and metadata sections. LLD requires a
# power-of-two ALIGN argument and a section that exists in its static-PIE
# default layout, so 0x404 becomes the equivalent next 0x400 boundary and the
# marker block is inserted before .gnu.hash instead of after absent .interp.
# The resulting file offsets remain the canonical 0x400 and 0x800.
sed \
    -e 's/ALIGN(0x404)/ALIGN(0x400)/' \
    -e 's/INSERT AFTER \.interp;/INSERT BEFORE .gnu.hash;/' \
    "$runtime_source/src/runtime/data_sections.ld" > "$obj/data-sections-zig.ld"

runtime_c="$runtime_source/src/runtime/runtime.c"
$cc $common_cflags -std=gnu99 -D_FILE_OFFSET_BITS=64 \
    "-DGIT_COMMIT=\"$runtime_commit\"" \
    -I"$sysroot/usr/include" -I"$sysroot/usr/include/fuse3" \
    -c "$runtime_c" -o "$obj/runtime.o"

link_runtime() {
    local fuse_library=$1
    local destination=$2
    $cc -pie -T "$obj/data-sections-zig.ld" \
        -Wl,--gc-sections -Wl,--build-id=none -Wl,--strip-debug \
        "$obj/runtime.o" \
        -L"$sysroot/usr/lib" \
        -Wl,--start-group \
        -lsquashfuse -lsquashfuse_ll -lzstd -lz \
        "$fuse_library" -lmimalloc \
        -Wl,--end-group -lpthread -ldl -lrt \
        -o "$destination"
}

link_runtime "$sysroot/usr/lib/libfuse3.a" "$obj/runtime.unstripped"
$zig objcopy --only-keep-debug "$obj/runtime.unstripped" "$obj/runtime-x86_64.debug"
$zig objcopy --strip-all "$obj/runtime.unstripped" "$obj/runtime.stripped"
(
    cd "$obj"
    "$zig" objcopy --add-gnu-debuglink=runtime-x86_64.debug \
        runtime.stripped runtime-x86_64
)
printf 'AI\002' | dd of="$obj/runtime-x86_64" bs=1 seek=8 conv=notrunc status=none

runtime_type=$(readelf -h "$obj/runtime-x86_64" | sed -n 's/^  Type:[[:space:]]*\([^[:space:]]*\).*/\1/p')
if [[ "$runtime_type" != DYN ]]; then
    echo "runtime is not a static PIE (ELF type is $runtime_type)" >&2
    exit 1
fi
if readelf -l "$obj/runtime-x86_64" | grep -q INTERP; then
    echo "runtime unexpectedly contains a dynamic interpreter" >&2
    exit 1
fi
if readelf -d "$obj/runtime-x86_64" 2>/dev/null | grep -q NEEDED; then
    echo "runtime unexpectedly contains a dynamic dependency" >&2
    exit 1
fi
if [[ "$(dd if="$obj/runtime-x86_64" bs=1 skip=8 count=3 status=none | od -An -tx1 | tr -d ' \n')" != 414902 ]]; then
    echo "runtime has no Type-2 magic" >&2
    exit 1
fi
section_table=$(readelf -S -W "$obj/runtime-x86_64")
appimage_offset=$(awk '/] \.appimage[[:space:]]/ { print $(NF-6) }' <<<"$section_table")
static_offset=$(awk '/] \.static[[:space:]]/ { print $(NF-6) }' <<<"$section_table")
digest_offset_hex=$(awk '/] \.digest_md5[[:space:]]/ { print $(NF-6) }' <<<"$section_table")
digest_size_hex=$(awk '/] \.digest_md5[[:space:]]/ { print $(NF-5) }' <<<"$section_table")
if [[ "$appimage_offset" != 000400 || "$static_offset" != 000800 || \
    "$digest_size_hex" != 000010 ]]; then
    echo "runtime has an unexpected AppImage section layout" >&2
    exit 1
fi
digest_offset=$((16#$digest_offset_hex))
digest_size=$((16#$digest_size_hex))
if [[ "$(dd if="$obj/runtime-x86_64" bs=1 skip="$digest_offset" count="$digest_size" status=none | od -An -tx1 | tr -d ' 0\n')" != "" ]]; then
    echo "runtime .digest_md5 section is not zero initialized" >&2
    exit 1
fi

rm -rf -- "$output_dir/relink-kit"
mkdir -p "$output_dir/relink-kit/inputs" "$output_dir/relink-kit/objects"
install -m755 "$obj/runtime-x86_64" "$output_dir/runtime-x86_64"
install -m644 "$obj/runtime-x86_64.debug" "$output_dir/runtime-x86_64.debug"
for archive in \
    "$runtime_archive" "$fuse_archive" "$squashfuse_archive" \
    "$zstd_archive" "$zlib_archive" "$mimalloc_archive" "$meson_wheel"; do
    install -m644 "$source_cache/$archive" "$output_dir/relink-kit/inputs/$archive"
done
install -m644 "$obj/runtime.o" "$obj/data-sections-zig.ld" \
    "$output_dir/relink-kit/objects/"
install -m755 "$script_path" \
    "$output_dir/relink-kit/build-appimage-runtime.sh"
install -m644 \
    "$sysroot/usr/lib/libsquashfuse.a" \
    "$sysroot/usr/lib/libsquashfuse_ll.a" \
    "$sysroot/usr/lib/libzstd.a" \
    "$sysroot/usr/lib/libz.a" \
    "$sysroot/usr/lib/libmimalloc.a" \
    "$output_dir/relink-kit/objects/"

cat > "$output_dir/relink-kit/relink.sh" <<'EOF'
#!/usr/bin/env bash
# Relink a Wild Buzzard Type-2 runtime against a recipient-modified libfuse3.
set -euo pipefail
kit=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
modified_fuse=${1:?usage: relink.sh MODIFIED_LIBFUSE3_A ZIG}
zig=${2:?usage: relink.sh MODIFIED_LIBFUSE3_A ZIG}
[[ -f "$modified_fuse" && -x "$zig" ]] || {
    echo "modified libfuse archive or Zig compiler is unavailable" >&2
    exit 1
}
[[ "$($zig version)" == 0.14.1 ]] || {
    echo "relinking requires Zig 0.14.1" >&2
    exit 1
}
out=${WILDBUZZARD_RELINK_OUTPUT:-"$PWD/runtime-x86_64.relinked"}
work=$(mktemp -d "${TMPDIR:-/tmp}/wildbuzzard-runtime-relink.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
"$zig" cc -target x86_64-linux-musl -pie \
    -T "$kit/objects/data-sections-zig.ld" \
    -Wl,--gc-sections -Wl,--build-id=none -Wl,--strip-debug \
    "$kit/objects/runtime.o" \
    -L"$kit/objects" -Wl,--start-group \
    -lsquashfuse -lsquashfuse_ll -lzstd -lz \
    "$modified_fuse" -lmimalloc \
    -Wl,--end-group -lpthread -ldl -lrt -o "$work/runtime.unstripped"
"$zig" objcopy --only-keep-debug \
    "$work/runtime.unstripped" "$work/runtime-x86_64.debug"
"$zig" objcopy --strip-all "$work/runtime.unstripped" "$work/runtime.stripped"
(
    cd "$work"
    "$zig" objcopy --add-gnu-debuglink=runtime-x86_64.debug \
        runtime.stripped runtime-x86_64
)
printf 'AI\002' | dd of="$work/runtime-x86_64" bs=1 seek=8 conv=notrunc status=none
install -m755 "$work/runtime-x86_64" "$out"
install -m644 "$work/runtime-x86_64.debug" "$out.debug"
printf '%s  %s\n' "$(sha256sum "$out" | cut -d' ' -f1)" "$out"
EOF
chmod 755 "$output_dir/relink-kit/relink.sh"

cat > "$output_dir/relink-kit/README.md" <<EOF
# Wild Buzzard AppImage runtime relink kit

This kit accompanies Wild Buzzard's statically linked Type-2 runtime so a
recipient can rebuild libfuse 3.15.0 with modifications and relink the runtime,
as required for the LGPL-2.1-only component.

The original sources are in \`inputs/\`. The exact Type-2 runtime source is
commit \`$runtime_commit\`; its \`patches/libfuse/mount.c.diff\` is applied to
libfuse before the release archive is built. \`build-appimage-runtime.sh\` is
the exact compilation script used for every source archive. \`objects/\`
contains the MIT/BSD/zlib non-LGPL objects and archives needed for relinking
without recompiling them. Build the modified libfuse as a static PIC
\`libfuse3.a\` for \`x86_64-linux-musl\` with the pinned script/toolchain, then
run:

\`./relink.sh /path/to/modified/libfuse3.a /path/to/zig\`

The compiler is Zig $zig_version from
\`zig-x86_64-linux-$zig_version.tar.xz\`, SHA-256
\`$zig_archive_sha256\`. Zig supplies the pinned musl and compiler-rt source
and link objects; its checksum-pinned archive is the complete toolchain input.
\`BUILD-INPUTS.sha256\` authenticates every source and relink object in this
kit. The generated runtime remains an x86-64 static PIE and has no host shared
library dependency.
EOF

(
    cd "$output_dir/relink-kit"
    find . -type f ! -name BUILD-INPUTS.sha256 -print0 |
        sort -z |
        xargs -0 sha256sum > BUILD-INPUTS.sha256
)
find "$output_dir/relink-kit" -exec touch -h -d "@$runtime_epoch" {} +

runtime_sha256=$(sha256sum "$output_dir/runtime-x86_64" | cut -d' ' -f1)
runtime_size=$(stat -c %s "$output_dir/runtime-x86_64")
relink_manifest_sha256=$(sha256sum "$output_dir/relink-kit/BUILD-INPUTS.sha256" | cut -d' ' -f1)
cat > "$output_dir/runtime-metadata.toml" <<EOF
schema = 1
target = "x86_64-linux-musl"
elf_type = "DYN"
static_pie = true
runtime_source_commit = "$runtime_commit"
source_date_epoch = $runtime_epoch
zig_version = "$zig_version"
zig_archive_sha256 = "$zig_archive_sha256"
runtime_sha256 = "$runtime_sha256"
runtime_size = $runtime_size
appimage_marker_offset = 1024
static_marker_offset = 2048
digest_patch_offset = $digest_offset
digest_patch_size = $digest_size
relink_manifest_sha256 = "$relink_manifest_sha256"
EOF
touch -d "@$runtime_epoch" \
    "$output_dir/runtime-x86_64" "$output_dir/runtime-x86_64.debug" \
    "$output_dir/runtime-metadata.toml"

"$output_dir/runtime-x86_64" --appimage-version 2>&1 |
    grep -F "$runtime_commit" >/dev/null

if [[ "$self_test" == true ]]; then
    test_root="$build_dir/self-test"
    mkdir -p "$test_root/AppDir/usr/bin" "$test_root/extract" "$test_root/tmp"
    cat > "$test_root/AppDir/AppRun" <<'EOF'
#!/bin/sh
set -eu
if [ "${1:-}" = detached-lease ]; then
    exec "$APPDIR/usr/bin/lease-supervisor" \
        "${2:?missing first release gate}" \
        "${3:?missing first lease marker}" \
        "${4:?missing second release gate}" \
        "${5:?missing second lease marker}" \
        "${6:?missing AppDir record}"
fi
if [ "${1:-}" = record-appdir ]; then
    printf '%s\n' "$APPDIR" > "${2:?missing AppDir record}"
    exit 0
fi
if [ "${1:-}" = nonzero ]; then
    exit 37
fi
printf 'wildbuzzard-runtime-self-test:%s\n' "${1:-none}"
EOF
    chmod 755 "$test_root/AppDir/AppRun"
    cat > "$test_root/AppDir/usr/bin/lease-helper" <<'EOF'
#!/bin/sh
printf 'wildbuzzard-runtime-lease:%s\n' "${1:-none}"
EOF
    chmod 755 "$test_root/AppDir/usr/bin/lease-helper"
    cat > "$test_root/AppDir/usr/bin/lease-supervisor" <<'PY'
#!/usr/bin/python3
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

lease_fd = int(os.environ["WILDBUZZARD_APPIMAGE_LEASE_FD"])
os.fstat(lease_fd)
appdir = Path(os.environ["APPDIR"])
first_gate, first_marker, second_gate, second_marker, appdir_record = map(Path, sys.argv[1:])
appdir_record.write_text(f"{appdir}\n", encoding="utf-8")

def spawn(gate: Path, marker: Path, label: str) -> None:
    if os.fork() != 0:
        return
    try:
        os.setsid()
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
        null_fd = os.open("/dev/null", os.O_RDWR)
        for descriptor in (0, 1, 2):
            os.dup2(null_fd, descriptor)
        if null_fd > 2:
            os.close(null_fd)
        deadline = time.monotonic() + 20
        while not gate.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        if not gate.exists():
            os._exit(70)
        output = subprocess.check_output(
            [appdir / "usr/bin/lease-helper", label],
            text=True,
        )
        marker.write_text(output, encoding="utf-8")
        os.close(lease_fd)
        os._exit(0)
    except BaseException:
        os._exit(71)

spawn(first_gate, first_marker, "first")
spawn(second_gate, second_marker, "second")
print("wildbuzzard-runtime-lease-launched", flush=True)
os.close(lease_fd)
PY
    chmod 755 "$test_root/AppDir/usr/bin/lease-supervisor"
    deep_test_path="$test_root/AppDir"
    for depth in $(seq 1 24); do
        deep_test_path="$deep_test_path/depth-$depth"
        mkdir "$deep_test_path"
    done
    touch "$deep_test_path/cleanup-depth-sentinel"
    env -u SOURCE_DATE_EPOCH mksquashfs \
        "$test_root/AppDir" "$test_root/payload.squashfs" \
        -noappend -all-root -no-xattrs -no-progress \
        -mkfs-time "$runtime_epoch" -all-time "$runtime_epoch" >/dev/null
    install -m755 "$output_dir/runtime-x86_64" "$test_root/test.AppImage"
    dd if="$test_root/payload.squashfs" of="$test_root/test.AppImage" \
        oflag=append conv=notrunc status=none
    (
        cd "$test_root/extract"
        "$test_root/test.AppImage" --appimage-extract >/dev/null
        [[ -x squashfs-root/AppRun ]]
    )
    extract_output=$(
        TMPDIR="$test_root/tmp" APPIMAGE_EXTRACT_AND_RUN=1 \
            "$test_root/test.AppImage" extract
    )
    [[ "$extract_output" == wildbuzzard-runtime-self-test:extract ]]
    if find "$test_root/tmp" -mindepth 1 -print -quit | grep -q .; then
        echo "ordinary extract-and-run command did not clean synchronously" >&2
        exit 1
    fi

    wait_for_file() {
        local path=$1
        for _ in $(seq 1 200); do
            [[ -f "$path" ]] && return 0
            sleep 0.05
        done
        echo "timed out waiting for file: $path" >&2
        return 1
    }
    wait_for_absent() {
        local path=$1
        for _ in $(seq 1 200); do
            [[ ! -e "$path" ]] && return 0
            sleep 0.05
        done
        echo "timed out waiting for cleanup: $path" >&2
        return 1
    }

    # A command-substitution caller must return with the direct AppRun process,
    # not when detached descendants finally release the AppDir. Both descendants
    # then read a bundled helper, and cleanup waits for the second writer.
    lease_first_gate="$test_root/extract-lease-first-gate"
    lease_first="$test_root/extract-lease-first"
    lease_second_gate="$test_root/extract-lease-second-gate"
    lease_second="$test_root/extract-lease-second"
    lease_appdir_record="$test_root/extract-lease-appdir"
    lease_output_file="$test_root/extract-lease-output"
    lease_returned="$test_root/extract-lease-returned"
    (
        lease_output=$(
            TMPDIR="$test_root/tmp" APPIMAGE_EXTRACT_AND_RUN=1 \
                "$test_root/test.AppImage" detached-lease \
                    "$lease_first_gate" "$lease_first" \
                    "$lease_second_gate" "$lease_second" "$lease_appdir_record"
        )
        printf '%s\n' "$lease_output" > "$lease_output_file"
        touch "$lease_returned"
    ) &
    lease_caller=$!
    if ! wait_for_file "$lease_returned"; then
        touch "$lease_first_gate" "$lease_second_gate"
        wait "$lease_caller" || true
        echo "extract-and-run caller waited for detached lease release" >&2
        exit 1
    fi
    wait "$lease_caller"
    lease_output=$(<"$lease_output_file")
    [[ "$lease_output" == wildbuzzard-runtime-lease-launched ]]
    [[ ! -e "$lease_first" && ! -e "$lease_second" ]]
    extract_lease_appdir=$(<"$lease_appdir_record")
    [[ "$extract_lease_appdir" == "$test_root/tmp/appimage_extracted_"* ]]
    [[ "$(stat -c %a "$extract_lease_appdir")" == 700 ]]
    touch "$lease_first_gate"
    wait_for_file "$lease_first"
    [[ "$(<"$lease_first")" == wildbuzzard-runtime-lease:first ]]
    [[ -d "$extract_lease_appdir" ]]
    [[ ! -e "$lease_second" ]]
    touch "$lease_second_gate"
    wait_for_file "$lease_second"
    [[ "$(<"$lease_second")" == wildbuzzard-runtime-lease:second ]]
    wait_for_absent "$extract_lease_appdir"

    # Two invocations must get independent random mode-0700 roots and cleanup
    # independently after their own final writer closes.
    concurrency_tmp="$test_root/concurrency-tmp"
    mkdir -p "$concurrency_tmp"
    for invocation in a b; do
        TMPDIR="$concurrency_tmp" APPIMAGE_EXTRACT_AND_RUN=1 \
            "$test_root/test.AppImage" detached-lease \
                "$test_root/concurrent-$invocation-first-gate" \
                "$test_root/concurrent-$invocation-first" \
                "$test_root/concurrent-$invocation-second-gate" \
                "$test_root/concurrent-$invocation-second" \
                "$test_root/concurrent-$invocation-appdir" \
                > "$test_root/concurrent-$invocation-output" &
        eval "concurrent_${invocation}_pid=$!"
    done
    wait "$concurrent_a_pid"
    wait "$concurrent_b_pid"
    concurrent_a_appdir=$(<"$test_root/concurrent-a-appdir")
    concurrent_b_appdir=$(<"$test_root/concurrent-b-appdir")
    [[ "$concurrent_a_appdir" != "$concurrent_b_appdir" ]]
    [[ "$(stat -c %a "$concurrent_a_appdir")" == 700 ]]
    [[ "$(stat -c %a "$concurrent_b_appdir")" == 700 ]]
    touch \
        "$test_root/concurrent-a-first-gate" \
        "$test_root/concurrent-a-second-gate" \
        "$test_root/concurrent-b-first-gate" \
        "$test_root/concurrent-b-second-gate"
    wait_for_file "$test_root/concurrent-a-first"
    wait_for_file "$test_root/concurrent-b-first"
    wait_for_file "$test_root/concurrent-a-second"
    wait_for_file "$test_root/concurrent-b-second"
    wait_for_absent "$concurrent_a_appdir"
    wait_for_absent "$concurrent_b_appdir"
    if find "$concurrency_tmp" -mindepth 1 -print -quit | grep -q .; then
        echo "concurrent runtime leases left temporary files" >&2
        exit 1
    fi

    # Exit status still propagates, and failed foreground commands have no
    # long-lived writer so they clean immediately.
    nonzero_tmp="$test_root/nonzero-tmp"
    mkdir -p "$nonzero_tmp"
    set +e
    TMPDIR="$nonzero_tmp" APPIMAGE_EXTRACT_AND_RUN=1 \
        "$test_root/test.AppImage" nonzero
    nonzero_status=$?
    set -e
    [[ "$nonzero_status" == 37 ]]
    if find "$nonzero_tmp" -mindepth 1 -print -quit | grep -q .; then
        echo "nonzero extract-and-run command left temporary files" >&2
        exit 1
    fi

    # Preserve the upstream NO_CLEANUP debugging contract, then remove only the
    # test-owned randomized directory after proving it was retained.
    no_cleanup_tmp="$test_root/no-cleanup-tmp"
    no_cleanup_record="$test_root/no-cleanup-appdir"
    mkdir -p "$no_cleanup_tmp"
    TMPDIR="$no_cleanup_tmp" APPIMAGE_EXTRACT_AND_RUN=1 NO_CLEANUP=1 \
        "$test_root/test.AppImage" record-appdir "$no_cleanup_record"
    no_cleanup_appdir=$(<"$no_cleanup_record")
    [[ "$no_cleanup_appdir" == "$no_cleanup_tmp/"appimage_extracted_* ]]
    [[ -x "$no_cleanup_appdir/AppRun" ]]
    rm -rf -- "$no_cleanup_appdir"

    # Deterministically exercise the automatic fallback without depending on
    # the build host's confinement policy. A privileged CI container can mount
    # FUSE directly and bypass FUSERMOUNT_PROG, so run only these fallback
    # cases as an unprivileged user. The helper passes discovery but rejects
    # the actual mount, exactly like a host that closes or denies the libfuse
    # communication descriptor during the privileged transition.
    fallback_state="$test_root/fallback-state"
    fallback_tmp="$fallback_state/tmp"
    mkdir -p "$fallback_tmp"
    fallback_runner=()
    if [[ "$(id -u)" == 0 ]]; then
        chown -R 65534:65534 "$fallback_state"
        fallback_runner=(
            setpriv --reuid=65534 --regid=65534 --clear-groups --no-new-privs --
        )
    fi
    cat > "$test_root/reject-fusermount3" <<'EOF'
#!/bin/sh
if [ "${1:-}" = --version ]; then
    printf '%s\n' 'fusermount3 fallback-test helper'
    exit 0
fi
exit 1
EOF
    chmod 755 "$test_root/reject-fusermount3"
    fallback_appdir_record="$fallback_state/appdir"
    env \
        TMPDIR="$fallback_tmp" \
        FUSERMOUNT_PROG="$test_root/reject-fusermount3" \
        "${fallback_runner[@]}" \
        "$test_root/test.AppImage" record-appdir "$fallback_appdir_record"
    fallback_appdir=$(<"$fallback_appdir_record")
    [[ "$fallback_appdir" == "$fallback_tmp/appimage_extracted_"* ]]
    [[ ! -e "$fallback_appdir" ]]
    if find "$fallback_tmp" -mindepth 1 -print -quit | grep -q .; then
        echo "runtime automatic extract-and-run fallback left temporary files" >&2
        exit 1
    fi

    fallback_first_gate="$fallback_state/lease-first-gate"
    fallback_first="$fallback_state/lease-first"
    fallback_second_gate="$fallback_state/lease-second-gate"
    fallback_second="$fallback_state/lease-second"
    fallback_appdir_record="$fallback_state/lease-appdir"
    fallback_output_file="$fallback_state/lease-output"
    fallback_returned="$fallback_state/lease-returned"
    (
        fallback_lease_output=$(
            env \
                TMPDIR="$fallback_tmp" \
                FUSERMOUNT_PROG="$test_root/reject-fusermount3" \
                "${fallback_runner[@]}" \
                "$test_root/test.AppImage" detached-lease \
                    "$fallback_first_gate" "$fallback_first" \
                    "$fallback_second_gate" "$fallback_second" \
                    "$fallback_appdir_record"
        )
        printf '%s\n' "$fallback_lease_output" > "$fallback_output_file"
        touch "$fallback_returned"
    ) &
    fallback_caller=$!
    if ! wait_for_file "$fallback_returned"; then
        touch "$fallback_first_gate" "$fallback_second_gate"
        wait "$fallback_caller" || true
        echo "automatic fallback caller waited for detached lease release" >&2
        exit 1
    fi
    wait "$fallback_caller"
    [[ "$(<"$fallback_output_file")" == wildbuzzard-runtime-lease-launched ]]
    fallback_lease_appdir=$(<"$fallback_appdir_record")
    [[ "$fallback_lease_appdir" == "$fallback_tmp/appimage_extracted_"* ]]
    if find "$fallback_tmp" -maxdepth 1 -type d -name '.mount_*' \
        -print -quit | grep -q .; then
        echo "failed native FUSE attempt left its mount directory" >&2
        exit 1
    fi
    touch "$fallback_first_gate"
    wait_for_file "$fallback_first"
    [[ "$(<"$fallback_first")" == wildbuzzard-runtime-lease:first ]]
    [[ -d "$fallback_lease_appdir" && ! -e "$fallback_second" ]]
    touch "$fallback_second_gate"
    wait_for_file "$fallback_second"
    [[ "$(<"$fallback_second")" == wildbuzzard-runtime-lease:second ]]
    wait_for_absent "$fallback_lease_appdir"
    if find "$fallback_tmp" -mindepth 1 -print -quit | grep -q .; then
        echo "detached automatic fallback left temporary files" >&2
        exit 1
    fi

    if [[ -e /dev/fuse ]] && command -v fusermount3 >/dev/null 2>&1; then
        # A setuid fusermount3 intentionally loses its communication fd when
        # this build is launched from a confined Snap profile. Exercise the
        # real host helper outside that build-tool profile when AppArmor
        # permits the unprivileged transition; ordinary unconfined builds use
        # the direct path.
        fuse_runner=()
        if [[ "$(cat /proc/self/attr/current 2>/dev/null || true)" != unconfined ]] && \
            command -v aa-exec >/dev/null 2>&1 && \
            aa-exec -p unconfined -- true 2>/dev/null; then
            fuse_runner=(aa-exec -p unconfined --)
        fi
        fuse_output=$(env \
            TMPDIR="$test_root/tmp" FUSERMOUNT_PROG="$(command -v fusermount3)" \
            "${fuse_runner[@]}" "$test_root/test.AppImage" fuse)
        [[ "$fuse_output" == wildbuzzard-runtime-self-test:fuse ]]

        fuse_first_gate="$test_root/fuse-lease-first-gate"
        fuse_first="$test_root/fuse-lease-first"
        fuse_second_gate="$test_root/fuse-lease-second-gate"
        fuse_second="$test_root/fuse-lease-second"
        fuse_appdir_record="$test_root/fuse-lease-appdir"
        fuse_output_file="$test_root/fuse-lease-output"
        fuse_returned="$test_root/fuse-lease-returned"
        (
            fuse_lease_output=$(env \
                TMPDIR="$test_root/tmp" FUSERMOUNT_PROG="$(command -v fusermount3)" \
                "${fuse_runner[@]}" "$test_root/test.AppImage" detached-lease \
                    "$fuse_first_gate" "$fuse_first" \
                    "$fuse_second_gate" "$fuse_second" "$fuse_appdir_record")
            printf '%s\n' "$fuse_lease_output" > "$fuse_output_file"
            touch "$fuse_returned"
        ) &
        fuse_caller=$!
        if ! wait_for_file "$fuse_returned"; then
            touch "$fuse_first_gate" "$fuse_second_gate"
            wait "$fuse_caller" || true
            echo "native FUSE caller waited for detached lease release" >&2
            exit 1
        fi
        wait "$fuse_caller"
        fuse_lease_output=$(<"$fuse_output_file")
        [[ "$fuse_lease_output" == wildbuzzard-runtime-lease-launched ]]
        [[ ! -e "$fuse_first" && ! -e "$fuse_second" ]]
        fuse_lease_appdir=$(<"$fuse_appdir_record")
        [[ "$fuse_lease_appdir" == "$test_root/tmp/.mount_"* ]]
        touch "$fuse_first_gate"
        wait_for_file "$fuse_first"
        [[ "$(<"$fuse_first")" == wildbuzzard-runtime-lease:first ]]
        [[ -d "$fuse_lease_appdir" ]]
        [[ ! -e "$fuse_second" ]]
        touch "$fuse_second_gate"
        wait_for_file "$fuse_second"
        [[ "$(<"$fuse_second")" == wildbuzzard-runtime-lease:second ]]
        wait_for_absent "$fuse_lease_appdir"
    else
        echo "runtime FUSE self-test unavailable: /dev/fuse or fusermount3 missing" >&2
        exit 1
    fi
fi

printf 'runtime_sha256=%s\n' "$runtime_sha256"
printf 'runtime_size=%s\n' "$runtime_size"
printf 'digest_patch_offset=%s\n' "$digest_offset"
printf 'relink_manifest_sha256=%s\n' "$relink_manifest_sha256"
