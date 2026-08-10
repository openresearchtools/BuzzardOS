// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::Serialize;
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};
use thiserror::Error;
use uuid::Uuid;

pub const MAX_MANAGED_JSON_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadOutcome<T> {
    pub value: T,
    pub migrated_from: Option<u32>,
}

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("managed path has no parent: {0}")]
    MissingParent(String),
    #[error("managed filename must be one normal path component: {0}")]
    InvalidFilename(String),
    #[error("managed directory must be a real directory, not a symlink: {0}")]
    UnsafeDirectory(String),
    #[error("refusing to replace a symbolic link or non-regular file: {0}")]
    UnsafeTarget(String),
    #[error("managed file exceeds the {limit}-byte limit: {path}")]
    TooLarge { path: String, limit: usize },
    #[error("managed JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("managed path contains a NUL byte: {0}")]
    Nul(String),
    #[error("I/O error for {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

fn io_error(path: &Path, source: std::io::Error) -> PersistenceError {
    PersistenceError::Io {
        path: path.display().to_string(),
        source,
    }
}

fn filename(path: &Path) -> Result<(&Path, &OsStr), PersistenceError> {
    let parent = path
        .parent()
        .ok_or_else(|| PersistenceError::MissingParent(path.display().to_string()))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| PersistenceError::InvalidFilename(path.display().to_string()))?;
    if leaf.is_empty()
        || !matches!(
            Path::new(leaf).components().next(),
            Some(Component::Normal(_))
        )
        || Path::new(leaf).components().count() != 1
    {
        return Err(PersistenceError::InvalidFilename(
            path.display().to_string(),
        ));
    }
    Ok((parent, leaf))
}

fn c_string(value: &OsStr, display: &Path) -> Result<CString, PersistenceError> {
    CString::new(value.as_bytes()).map_err(|_| PersistenceError::Nul(display.display().to_string()))
}

fn open_directory(path: &Path) -> Result<File, PersistenceError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PersistenceError::UnsafeDirectory(
            path.display().to_string(),
        ));
    }
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| io_error(path, error))
}

fn reject_unsafe_existing_target(
    directory: &File,
    leaf: &OsStr,
    path: &Path,
) -> Result<(), PersistenceError> {
    let leaf = c_string(leaf, path)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `directory` and `leaf` remain live for the call and `stat`
    // points to writable, correctly sized storage.
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            leaf.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        // SAFETY: a successful fstatat initialized `stat`.
        let mode = unsafe { stat.assume_init() }.st_mode;
        if mode & libc::S_IFMT != libc::S_IFREG {
            return Err(PersistenceError::UnsafeTarget(path.display().to_string()));
        }
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(())
    } else {
        Err(io_error(path, error))
    }
}

/// Read a regular managed file without following a final symbolic link.
pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, PersistenceError> {
    let (parent, leaf) = filename(path)?;
    let directory = open_directory(parent)?;
    let leaf = c_string(leaf, path)?;
    read_bounded_at(&directory, &leaf, path, limit)
}

fn read_bounded_at(
    directory: &File,
    leaf: &CString,
    display_path: &Path,
    limit: usize,
) -> Result<Vec<u8>, PersistenceError> {
    // O_NONBLOCK prevents a hostile FIFO from hanging this process before we
    // can inspect the opened object. O_NOCTTY avoids acquiring a terminal if
    // a character device is substituted. The descriptor is opened relative
    // to the already-bound parent and is validated with fstat before reading.
    // SAFETY: the directory descriptor and NUL-terminated leaf remain valid
    // for the call. A successful descriptor is immediately wrapped by File.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        return if matches!(error.raw_os_error(), Some(code) if code == libc::ELOOP || code == libc::ENXIO)
        {
            Err(PersistenceError::UnsafeTarget(
                display_path.display().to_string(),
            ))
        } else {
            Err(io_error(display_path, error))
        };
    }
    // SAFETY: openat returned a new owned descriptor.
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: file owns a valid descriptor and stat is writable storage.
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io_error(display_path, std::io::Error::last_os_error()));
    }
    // SAFETY: successful fstat initialized stat.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(PersistenceError::UnsafeTarget(
            display_path.display().to_string(),
        ));
    }
    let size = u64::try_from(stat.st_size).map_err(|_| {
        PersistenceError::UnsafeTarget(format!("{} has a negative size", display_path.display()))
    })?;
    if size > limit as u64 {
        return Err(PersistenceError::TooLarge {
            path: display_path.display().to_string(),
            limit,
        });
    }
    let mut bytes = Vec::with_capacity(size as usize);
    Read::by_ref(&mut file)
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(display_path, error))?;
    if bytes.len() > limit {
        return Err(PersistenceError::TooLarge {
            path: display_path.display().to_string(),
            limit,
        });
    }
    Ok(bytes)
}

/// Atomically replace one regular file using a same-directory temporary file.
///
/// The temporary file is synced before rename and the directory is synced
/// afterwards. Final symlinks and non-regular targets are rejected. The open
/// directory descriptor binds the write and rename to one directory even if a
/// path component is concurrently changed.
pub fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> Result<(), PersistenceError> {
    let (parent, leaf) = filename(path)?;
    let directory = open_directory(parent)?;
    reject_unsafe_existing_target(&directory, leaf, path)?;
    let leaf_c = c_string(leaf, path)?;

    let mut last_collision = None;
    for _ in 0..16 {
        let temporary_name = format!(".wildbuzzard-{}.tmp", Uuid::new_v4());
        let temporary_c = CString::new(temporary_name.as_bytes()).expect("UUID has no NUL");
        // SAFETY: all pointers and the directory descriptor are valid. The
        // returned descriptor is uniquely owned and immediately wrapped.
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                temporary_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode & 0o777,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EEXIST) {
                last_collision = Some(error);
                continue;
            }
            return Err(io_error(path, error));
        }
        // SAFETY: `openat` returned a new owned descriptor.
        let mut temporary = unsafe { File::from_raw_fd(descriptor) };
        let operation = (|| {
            temporary
                .set_permissions(fs::Permissions::from_mode(mode & 0o777))
                .map_err(|error| io_error(path, error))?;
            temporary
                .write_all(bytes)
                .map_err(|error| io_error(path, error))?;
            temporary
                .sync_all()
                .map_err(|error| io_error(path, error))?;
            drop(temporary);
            // SAFETY: both names are NUL-terminated and both descriptors
            // identify the same still-open directory.
            let renamed = unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    temporary_c.as_ptr(),
                    directory.as_raw_fd(),
                    leaf_c.as_ptr(),
                )
            };
            if renamed != 0 {
                return Err(io_error(path, std::io::Error::last_os_error()));
            }
            directory
                .sync_all()
                .map_err(|error| io_error(parent, error))
        })();
        if operation.is_err() {
            // SAFETY: best-effort removal of the known temporary leaf from
            // the already opened directory; no path is followed.
            unsafe {
                libc::unlinkat(directory.as_raw_fd(), temporary_c.as_ptr(), 0);
            }
        }
        return operation;
    }
    Err(io_error(
        path,
        last_collision.unwrap_or_else(|| std::io::Error::from_raw_os_error(libc::EEXIST)),
    ))
}

pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), PersistenceError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_MANAGED_JSON_BYTES {
        return Err(PersistenceError::TooLarge {
            path: path.display().to_string(),
            limit: MAX_MANAGED_JSON_BYTES,
        });
    }
    atomic_write(path, &bytes, 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::ser::{Error as _, Serializer};
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};

    #[test]
    fn atomic_write_is_durable_shape_and_private() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("settings.json");
        atomic_write(&target, b"old", 0o600).unwrap();
        atomic_write(&target, b"new", 0o600).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn final_symlink_is_rejected_without_touching_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("victim");
        let target = temp.path().join("settings.json");
        fs::write(&victim, b"untouched").unwrap();
        symlink(&victim, &target).unwrap();
        assert!(matches!(
            atomic_write(&target, b"hostile", 0o600),
            Err(PersistenceError::UnsafeTarget(_))
        ));
        assert_eq!(fs::read(&victim).unwrap(), b"untouched");
        assert!(
            fs::symlink_metadata(&target)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn symlink_directory_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();
        assert!(matches!(
            atomic_write(&link.join("state.json"), b"no", 0o600),
            Err(PersistenceError::UnsafeDirectory(_))
        ));
        assert!(!real.join("state.json").exists());
    }

    #[test]
    fn bounded_reads_reject_non_regular_and_oversized_files() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("directory");
        fs::create_dir(&directory).unwrap();
        assert!(matches!(
            read_bounded(&directory, 4),
            Err(PersistenceError::UnsafeTarget(_))
        ));
        let file = temp.path().join("large");
        fs::write(&file, b"12345").unwrap();
        assert!(matches!(
            read_bounded(&file, 4),
            Err(PersistenceError::TooLarge { .. })
        ));
    }

    #[test]
    fn bounded_read_rejects_fifo_without_waiting_for_a_writer() {
        let temp = tempfile::tempdir().unwrap();
        let fifo = temp.path().join("hostile.json");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_c is a valid NUL-terminated path in the temporary dir.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert!(matches!(
            read_bounded(&fifo, 1024),
            Err(PersistenceError::UnsafeTarget(_))
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bounded_read_rejects_character_device_after_opened_fd_validation() {
        assert!(matches!(
            read_bounded(Path::new("/dev/null"), 1024),
            Err(PersistenceError::UnsafeTarget(_))
        ));
    }

    #[test]
    fn descriptor_relative_read_stays_bound_when_directory_path_is_swapped() {
        let temp = tempfile::tempdir().unwrap();
        let visible = temp.path().join("state");
        let moved = temp.path().join("bound-state");
        fs::create_dir(&visible).unwrap();
        fs::write(visible.join("record.json"), b"bound-record").unwrap();
        let directory = open_directory(&visible).unwrap();
        fs::rename(&visible, &moved).unwrap();
        fs::create_dir(&visible).unwrap();
        fs::write(visible.join("record.json"), b"replacement-record").unwrap();

        let leaf = CString::new("record.json").unwrap();
        assert_eq!(
            read_bounded_at(&directory, &leaf, &visible.join("record.json"), 1024).unwrap(),
            b"bound-record"
        );
    }

    #[test]
    fn serialization_failure_preserves_the_previous_file() {
        struct Fails;
        impl Serialize for Fails {
            fn serialize<S: Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(S::Error::custom("intentional test failure"))
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        fs::write(&path, b"previous").unwrap();
        assert!(matches!(
            atomic_write_json(&path, &Fails),
            Err(PersistenceError::Json(_))
        ));
        assert_eq!(fs::read(path).unwrap(), b"previous");
    }
}
