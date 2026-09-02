// SPDX-License-Identifier: AGPL-3.0-or-later

//! Bounded inotify watches for shell-owned XDG models.

use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::ffi::{CString, OsString};
use std::fs::{self, File};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const MAX_WATCH_DIRECTORIES: usize = 4096;
const MAX_WATCH_DEPTH: usize = 16;
const EVENT_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct DirectoryWatcher {
    backend: WatchBackend,
}

/// An inotify source scoped to one exact file name. Watching its parent keeps
/// the source live when an atomic writer replaces the file with `rename(2)`.
#[derive(Debug)]
pub struct FileWatcher {
    backend: WatchBackend,
    file_name: OsString,
}

#[derive(Debug)]
enum WatchBackend {
    Inotify(File),
    /// Explicit shell-control notifications remain available when the host's
    /// inotify quota is exhausted. Never replace an event source with a
    /// recurring filesystem scan: that would add idle work and visible
    /// refreshes to every running desktop.
    Unavailable,
}

impl DirectoryWatcher {
    pub fn new(roots: &[PathBuf]) -> Result<Self> {
        // SAFETY: successful inotify_init1 returns a new owned descriptor.
        let descriptor = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOSPC) {
                return Ok(Self {
                    backend: WatchBackend::Unavailable,
                });
            }
            return Err(error).context("creating inotify watcher");
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
                if error.raw_os_error() == Some(libc::ENOSPC) {
                    return Ok(Self {
                        backend: WatchBackend::Unavailable,
                    });
                }
                if error.kind() != std::io::ErrorKind::NotFound {
                    return Err(error).with_context(|| format!("watching {}", path.display()));
                }
            }
        }
        Ok(Self {
            backend: WatchBackend::Inotify(descriptor),
        })
    }

    pub fn changed(&self) -> Result<bool> {
        match &self.backend {
            WatchBackend::Inotify(descriptor) => Self::inotify_changed(descriptor),
            WatchBackend::Unavailable => Ok(false),
        }
    }

    /// Return the guest-local event descriptor when kernel notifications are
    /// available. Callers use this only as an independent readiness source;
    /// no filesystem state enters the Wayland protocol or host gateway.
    pub fn raw_fd(&self) -> Option<RawFd> {
        match &self.backend {
            WatchBackend::Inotify(descriptor) => Some(descriptor.as_raw_fd()),
            WatchBackend::Unavailable => None,
        }
    }

    fn inotify_changed(descriptor: &File) -> Result<bool> {
        let mut buffer = [0u8; EVENT_BUFFER_BYTES];
        let mut changed = false;
        loop {
            // SAFETY: buffer and descriptor are valid. Parsing event records is
            // unnecessary because every watched mutation invalidates the model.
            let length = unsafe {
                libc::read(
                    descriptor.as_raw_fd(),
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

impl FileWatcher {
    pub fn new(path: &Path) -> Result<Self> {
        let file_name = path
            .file_name()
            .context("watched file has no final component")?
            .to_os_string();
        let parent = path
            .parent()
            .context("watched file has no parent directory")?;
        let metadata = fs::symlink_metadata(parent)
            .with_context(|| format!("inspecting watched directory {}", parent.display()))?;
        anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "watched file parent is not a real directory: {}",
            parent.display()
        );

        // SAFETY: successful inotify_init1 returns a new owned descriptor.
        let descriptor = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOSPC) {
                return Ok(Self {
                    backend: WatchBackend::Unavailable,
                    file_name,
                });
            }
            return Err(error).context("creating exact-file inotify watcher");
        }
        // SAFETY: descriptor is freshly owned.
        let descriptor = unsafe { File::from_raw_fd(descriptor) };
        let parent_c = CString::new(parent.as_os_str().as_bytes())
            .with_context(|| format!("watch path contains NUL: {}", parent.display()))?;
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_CLOSE_WRITE
            | libc::IN_ATTRIB;
        // SAFETY: descriptor and NUL-terminated path are valid.
        if unsafe { libc::inotify_add_watch(descriptor.as_raw_fd(), parent_c.as_ptr(), mask) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOSPC) {
                return Ok(Self {
                    backend: WatchBackend::Unavailable,
                    file_name,
                });
            }
            return Err(error).with_context(|| format!("watching {}", parent.display()));
        }
        Ok(Self {
            backend: WatchBackend::Inotify(descriptor),
            file_name,
        })
    }

    pub fn changed(&self) -> Result<bool> {
        match &self.backend {
            WatchBackend::Inotify(descriptor) => Self::inotify_changed(descriptor, &self.file_name),
            WatchBackend::Unavailable => Ok(false),
        }
    }

    pub fn raw_fd(&self) -> Option<RawFd> {
        match &self.backend {
            WatchBackend::Inotify(descriptor) => Some(descriptor.as_raw_fd()),
            WatchBackend::Unavailable => None,
        }
    }

    fn inotify_changed(descriptor: &File, file_name: &OsString) -> Result<bool> {
        let mut buffer = [0u8; EVENT_BUFFER_BYTES];
        let expected = file_name.as_os_str().as_bytes();
        let header_size = std::mem::size_of::<libc::inotify_event>();
        let mut changed = false;
        loop {
            // SAFETY: buffer and descriptor are valid.
            let length = unsafe {
                libc::read(
                    descriptor.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if length > 0 {
                let length = usize::try_from(length).unwrap_or(buffer.len());
                let mut offset = 0usize;
                while offset.saturating_add(header_size) <= length {
                    // SAFETY: the bounds check covers the fixed header;
                    // inotify records themselves may be unaligned.
                    let event = unsafe {
                        buffer
                            .as_ptr()
                            .add(offset)
                            .cast::<libc::inotify_event>()
                            .read_unaligned()
                    };
                    let name_length = usize::try_from(event.len).unwrap_or(0);
                    let record_length = header_size.saturating_add(name_length);
                    if offset.saturating_add(record_length) > length {
                        break;
                    }
                    if event.mask & libc::IN_Q_OVERFLOW != 0 {
                        changed = true;
                    } else if name_length > 0 {
                        let name = &buffer[offset + header_size..offset + record_length];
                        let end = name
                            .iter()
                            .position(|byte| *byte == 0)
                            .unwrap_or(name.len());
                        if &name[..end] == expected {
                            changed = true;
                        }
                    }
                    offset = offset.saturating_add(record_length);
                }
                continue;
            }
            if length == 0 {
                return Ok(changed);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(changed);
            }
            return Err(error).context("reading exact-file inotify events");
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

    #[test]
    fn unavailable_backend_never_scans_or_reports_periodic_changes() {
        let temp = tempfile::tempdir().unwrap();
        let watcher = DirectoryWatcher {
            backend: WatchBackend::Unavailable,
        };
        fs::write(temp.path().join("entry.desktop"), b"fixture").unwrap();
        assert!(!watcher.changed().unwrap());
    }

    #[test]
    fn exact_file_watcher_ignores_siblings_and_observes_atomic_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let watched = temp.path().join("settings.json");
        let watcher = FileWatcher::new(&watched).unwrap();
        fs::write(temp.path().join("unrelated"), b"noise").unwrap();
        assert!(!watcher.changed().unwrap());

        let staged = temp.path().join("settings.json.new");
        fs::write(&staged, b"new settings").unwrap();
        fs::rename(staged, &watched).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if watcher.changed().unwrap() {
                return;
            }
            std::thread::yield_now();
        }
        panic!("exact-file inotify did not report atomic replacement");
    }
}
