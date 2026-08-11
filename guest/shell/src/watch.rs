// SPDX-License-Identifier: AGPL-3.0-or-later

//! Bounded inotify watches for shell-owned XDG models.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{self, File};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const MAX_WATCH_DIRECTORIES: usize = 4096;
const MAX_WATCH_DEPTH: usize = 16;
const EVENT_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct DirectoryWatcher {
    descriptor: File,
}

impl DirectoryWatcher {
    pub fn new(roots: &[PathBuf]) -> Result<Self> {
        // SAFETY: successful inotify_init1 returns a new owned descriptor.
        let descriptor = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error()).context("creating inotify watcher");
        }
        // SAFETY: descriptor is freshly owned.
        let descriptor = unsafe { File::from_raw_fd(descriptor) };
        let mut directories = BTreeSet::new();
        for root in roots {
            // A new machine commonly has no user applications directory yet.
            // Watching the nearest real ancestor lets creation of the first
            // launcher invalidate the model; the caller then rearms over the
            // newly created directory tree.
            if let Some((existing, root_exists)) = nearest_real_directory(root)? {
                if root_exists {
                    collect_real_directories(&existing, 0, &mut directories)?;
                } else if directories.len() < MAX_WATCH_DIRECTORIES {
                    // Do not recursively watch an entire home/data directory
                    // just because one bounded application root is absent.
                    directories.insert(existing);
                }
            }
        }
        for path in directories {
            let path_c = CString::new(path.as_os_str().as_bytes())
                .with_context(|| format!("watch path contains NUL: {}", path.display()))?;
            let mask = libc::IN_CREATE
                | libc::IN_DELETE
                | libc::IN_MOVED_FROM
                | libc::IN_MOVED_TO
                | libc::IN_CLOSE_WRITE
                | libc::IN_ATTRIB
                | libc::IN_DELETE_SELF
                | libc::IN_MOVE_SELF;
            // SAFETY: descriptor and NUL-terminated path are valid.
            if unsafe { libc::inotify_add_watch(descriptor.as_raw_fd(), path_c.as_ptr(), mask) } < 0
            {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error).with_context(|| format!("watching {}", path.display()));
                }
            }
        }
        Ok(Self { descriptor })
    }

    pub fn changed(&self) -> Result<bool> {
        let mut buffer = [0u8; EVENT_BUFFER_BYTES];
        let mut changed = false;
        loop {
            // SAFETY: buffer and descriptor are valid. Parsing event records is
            // unnecessary because every watched mutation invalidates the model.
            let length = unsafe {
                libc::read(
                    self.descriptor.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if length > 0 {
                changed = true;
                continue;
            }
            if length == 0 {
                return Ok(changed);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(changed);
            }
            return Err(error).context("reading inotify events");
        }
    }
}

fn nearest_real_directory(path: &Path) -> Result<Option<(PathBuf, bool)>> {
    let mut candidate = Some(path);
    while let Some(current) = candidate {
        match fs::symlink_metadata(current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(None),
            Ok(metadata) if metadata.is_dir() => {
                return Ok(Some((current.to_path_buf(), current == path)));
            }
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                candidate = current.parent();
            }
            Err(error) => {
                return Err(error).with_context(|| format!("inspecting {}", current.display()));
            }
        }
    }
    Ok(None)
}

fn collect_real_directories(
    path: &Path,
    depth: usize,
    directories: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if depth > MAX_WATCH_DEPTH || directories.len() >= MAX_WATCH_DIRECTORIES {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(());
    }
    if !directories.insert(path.to_path_buf()) {
        return Ok(());
    }
    let mut children = fs::read_dir(path)
        .with_context(|| format!("listing {}", path.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    children.sort();
    for child in children {
        if directories.len() >= MAX_WATCH_DIRECTORIES {
            break;
        }
        collect_real_directories(&child, depth + 1, directories)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn real_directory_mutation_is_observed_without_polling_contents() {
        let temp = tempfile::tempdir().unwrap();
        let watched = temp.path().join("watched");
        fs::create_dir(&watched).unwrap();
        let watcher = DirectoryWatcher::new(std::slice::from_ref(&watched)).unwrap();
        fs::write(watched.join("new.desktop"), b"fixture").unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if watcher.changed().unwrap() {
                return;
            }
            std::thread::yield_now();
        }
        panic!("inotify did not report the write");
    }

    #[test]
    fn symbolic_link_roots_are_not_watched() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();
        let watcher = DirectoryWatcher::new(&[link]).unwrap();
        fs::write(real.join("hidden.desktop"), b"fixture").unwrap();
        assert!(!watcher.changed().unwrap());
    }

    #[test]
    fn missing_root_creation_is_observed_via_its_existing_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join(".local/share/applications");
        let watcher = DirectoryWatcher::new(std::slice::from_ref(&missing)).unwrap();
        fs::create_dir_all(&missing).unwrap();
        fs::write(missing.join("first.desktop"), b"fixture").unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if watcher.changed().unwrap() {
                return;
            }
            std::thread::yield_now();
        }
        panic!("creation of a previously absent watched root was not reported");
    }
}
