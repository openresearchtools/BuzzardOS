// SPDX-License-Identifier: AGPL-3.0-or-later

//! Descriptor-bound operations for the user's XDG Desktop directory.
//!
//! The shell owns confirmation and clipboard UI. This module owns the actual
//! filesystem contract so a mouse action, keyboard shortcut, Settings action,
//! and AT-SPI action cannot acquire subtly different path semantics.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

use crate::FileObservation;
use crate::persistence::{
    MAX_MANAGED_JSON_BYTES, PersistenceError, atomic_write_json, read_bounded,
};

pub const DESKTOP_LAYOUT_SCHEMA_VERSION: u32 = 1;
const MAX_NAME_BYTES: usize = 255;
const MAX_DIRECTORY_ENTRIES: usize = 65_536;
const MAX_RECURSION_DEPTH: usize = 128;
const MAX_TRAVERSAL_ENTRIES: u64 = 131_072;
const MAX_TRAVERSAL_BYTES: u64 = 1 << 50;
const MAX_LAYOUT_ENTRIES: usize = 65_536;
const MAX_LAYOUT_COORDINATE: u32 = 1_000_000;
const MAX_LAYOUT_PAGE: u32 = 4095;
const APPIMAGE_HEADER_LEN: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

impl FileIdentity {
    pub fn layout_key(self) -> String {
        format!("{}:{}", self.device, self.inode)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopItemKind {
    Launcher,
    AppImage,
    RegularFile,
    Directory,
    SymbolicLink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopItem {
    pub name: OsString,
    pub display_name: String,
    pub path: PathBuf,
    pub identity: FileIdentity,
    pub kind: DesktopItemKind,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopPosition {
    pub column: u32,
    pub row: u32,
    pub page: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopLayout {
    pub schema_version: u32,
    pub generation: u64,
    pub positions: BTreeMap<String, DesktopPosition>,
}

impl Default for DesktopLayout {
    fn default() -> Self {
        Self {
            schema_version: DESKTOP_LAYOUT_SCHEMA_VERSION,
            generation: 0,
            positions: BTreeMap::new(),
        }
    }
}

impl DesktopLayout {
    pub fn validate(&self) -> Result<(), DesktopFileError> {
        if self.schema_version != DESKTOP_LAYOUT_SCHEMA_VERSION {
            return Err(DesktopFileError::UnsupportedLayoutSchema {
                found: self.schema_version,
                current: DESKTOP_LAYOUT_SCHEMA_VERSION,
            });
        }
        if self.positions.len() > MAX_LAYOUT_ENTRIES {
            return Err(DesktopFileError::LayoutLimit);
        }
        for (key, position) in &self.positions {
            let Some((device, inode)) = key.split_once(':') else {
                return Err(DesktopFileError::InvalidLayoutKey(key.clone()));
            };
            if device.parse::<u64>().is_err() || inode.parse::<u64>().is_err() {
                return Err(DesktopFileError::InvalidLayoutKey(key.clone()));
            }
            if position.column > MAX_LAYOUT_COORDINATE
                || position.row > MAX_LAYOUT_COORDINATE
                || position.page > MAX_LAYOUT_PAGE
            {
                return Err(DesktopFileError::LayoutLimit);
            }
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<(), DesktopFileError> {
        self.validate()?;
        atomic_write_json(path, self)?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, DesktopFileError> {
        let bytes = read_bounded(path, MAX_MANAGED_JSON_BYTES)?;
        let layout: Self = serde_json::from_slice(&bytes).map_err(PersistenceError::from)?;
        layout.validate()?;
        Ok(layout)
    }

    pub fn retain_items(&mut self, items: &[DesktopItem]) {
        let live: HashSet<_> = items
            .iter()
            .map(|item| item.identity.layout_key())
            .collect();
        self.positions.retain(|identity, _| live.contains(identity));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollisionChoice {
    Replace,
    KeepBoth,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteConsequence {
    ShortcutOnly,
    LinkOnly,
    RegularFile,
    DirectoryTree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteResult {
    pub destination_name: OsString,
    pub source_removed: bool,
}

#[derive(Debug, Error)]
pub enum DesktopFileError {
    #[error("desktop path must be a real directory: {0}")]
    UnsafeDirectory(String),
    #[error("desktop item name must be one non-special path component")]
    InvalidName,
    #[error("desktop item name exceeds {MAX_NAME_BYTES} bytes")]
    NameTooLong,
    #[error("desktop item does not exist: {0}")]
    Missing(String),
    #[error("desktop item type is unsupported: {0}")]
    UnsupportedType(String),
    #[error("operation would cross a mounted filesystem at {0}")]
    MountBoundary(String),
    #[error("destination already exists: {0}")]
    Collision(String),
    #[error("desktop item changed identity while an operation was in progress: {0}")]
    ChangedIdentity(String),
    #[error("operation was cancelled")]
    Cancelled,
    #[error("directory tree exceeds the bounded traversal limit")]
    TraversalLimit,
    #[error("desktop layout exceeds its bounded entry or coordinate limits")]
    LayoutLimit,
    #[error("the kernel did not report a stable mount identity for {0}")]
    MissingMountIdentity(String),
    #[error("move copied the item but could not remove the source: {0}")]
    SourceRemovalAfterCopy(String),
    #[error("desktop layout schema {found} is unsupported (current {current})")]
    UnsupportedLayoutSchema { found: u32, current: u32 },
    #[error("invalid desktop layout identity key: {0}")]
    InvalidLayoutKey(String),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error("I/O error for {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

fn io_error(path: impl AsRef<Path>, source: std::io::Error) -> DesktopFileError {
    DesktopFileError::Io {
        path: path.as_ref().display().to_string(),
        source,
    }
}

#[derive(Debug)]
pub struct DesktopDirectory {
    path: PathBuf,
    file: File,
    device: u64,
    mount_id: u64,
}

impl DesktopDirectory {
    pub fn open(path: &Path) -> Result<Self, DesktopFileError> {
        let metadata = fs::symlink_metadata(path).map_err(|error| io_error(path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(DesktopFileError::UnsafeDirectory(
                path.display().to_string(),
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
            .map_err(|error| io_error(path, error))?;
        let stat = fstat(&file, path)?;
        let mount_id = mount_id_fd(&file, path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            device: stat.st_dev,
            mount_id,
        })
    }

    pub fn create_and_open(path: &Path) -> Result<Self, DesktopFileError> {
        fs::create_dir_all(path).map_err(|error| io_error(path, error))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| io_error(path, error))?;
        Self::open(path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> Result<Vec<DesktopItem>, DesktopFileError> {
        let mut items = Vec::new();
        for name in directory_names(&self.file, &self.path)? {
            validate_name(&name)?;
            let stat = stat_at(&self.file, &name, &self.path)?;
            let kind = item_kind(&self.file, &name, &stat, &self.path)?;
            if kind == DesktopItemKind::Launcher && stat.st_mode & libc::S_IXUSR == 0 {
                authorize_desktop_launcher(&self.file, &name, &stat, &self.path)?;
            }
            items.push(DesktopItem {
                display_name: name.to_string_lossy().into_owned(),
                path: self.path.join(&name),
                identity: FileIdentity {
                    device: stat.st_dev,
                    inode: stat.st_ino,
                },
                kind,
                size: u64::try_from(stat.st_size).unwrap_or_default(),
                name,
            });
        }
        items.sort_by(|left, right| {
            left.display_name
                .to_lowercase()
                .cmp(&right.display_name.to_lowercase())
                .then_with(|| left.name.as_bytes().cmp(right.name.as_bytes()))
        });
        Ok(items)
    }

    pub fn consequence(&self, name: &OsStr) -> Result<DeleteConsequence, DesktopFileError> {
        validate_name(name)?;
        let stat = stat_at(&self.file, name, &self.path)?;
        Ok(if mode_type(stat.st_mode) == libc::S_IFLNK {
            DeleteConsequence::LinkOnly
        } else if mode_type(stat.st_mode) == libc::S_IFDIR {
            DeleteConsequence::DirectoryTree
        } else if mode_type(stat.st_mode) == libc::S_IFREG
            && Path::new(name).extension() == Some(OsStr::new("desktop"))
        {
            DeleteConsequence::ShortcutOnly
        } else if mode_type(stat.st_mode) == libc::S_IFREG {
            DeleteConsequence::RegularFile
        } else {
            return Err(DesktopFileError::UnsupportedType(
                self.path.join(name).display().to_string(),
            ));
        })
    }

    /// Permanently remove an item after the caller has obtained explicit
    /// confirmation. Symbolic links are unlinked and never traversed.
    pub fn delete_confirmed(&self, name: &OsStr) -> Result<(), DesktopFileError> {
        validate_name(name)?;
        let mut budget = TraversalBudget::default();
        remove_entry(
            &self.file,
            name,
            self.device,
            self.mount_id,
            0,
            &self.path,
            &mut budget,
        )?;
        self.file
            .sync_all()
            .map_err(|error| io_error(&self.path, error))
    }

    pub fn create_folder(&self, preferred: &OsStr) -> Result<OsString, DesktopFileError> {
        validate_name(preferred)?;
        let name = keep_both_name(self, preferred)?;
        let name_c = c_string(&name)?;
        // SAFETY: the directory and NUL-terminated name remain valid.
        if unsafe { libc::mkdirat(self.file.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
            return Err(io_error(
                self.path.join(&name),
                std::io::Error::last_os_error(),
            ));
        }
        self.file
            .sync_all()
            .map_err(|error| io_error(&self.path, error))?;
        Ok(name)
    }

    pub fn rename(&self, old: &OsStr, new: &OsStr) -> Result<(), DesktopFileError> {
        validate_name(old)?;
        validate_name(new)?;
        if old == new {
            return Ok(());
        }
        stat_at(&self.file, old, &self.path)?;
        if exists_at(&self.file, new, &self.path)? {
            return Err(DesktopFileError::Collision(
                self.path.join(new).display().to_string(),
            ));
        }
        rename_at2(&self.file, old, &self.file, new, libc::RENAME_NOREPLACE).map_err(|error| {
            if matches!(error.raw_os_error(), Some(libc::EEXIST | libc::ENOTEMPTY)) {
                DesktopFileError::Collision(self.path.join(new).display().to_string())
            } else {
                io_error(self.path.join(old), error)
            }
        })?;
        self.file
            .sync_all()
            .map_err(|error| io_error(&self.path, error))
    }

    /// Observe one regular file through the already-open Desktop directory.
    ///
    /// This is intentionally descriptor-relative: callers may persist the
    /// returned identity in a transaction journal and later reject a replaced
    /// source without resolving a mutable absolute pathname.
    pub fn observe_regular_file(&self, name: &OsStr) -> Result<FileObservation, DesktopFileError> {
        validate_name(name)?;
        let stat = stat_at(&self.file, name, &self.path)?;
        if mode_type(stat.st_mode) != libc::S_IFREG {
            return Err(DesktopFileError::UnsupportedType(
                self.path.join(name).display().to_string(),
            ));
        }
        let mount_id = mount_id_at(&self.file, name, &self.path)?;
        if stat.st_dev != self.device || mount_id != self.mount_id {
            return Err(DesktopFileError::MountBoundary(
                self.path.join(name).display().to_string(),
            ));
        }
        Ok(FileObservation {
            device: stat.st_dev,
            inode: stat.st_ino,
            size: u64::try_from(stat.st_size).map_err(|_| {
                DesktopFileError::ChangedIdentity(self.path.join(name).display().to_string())
            })?,
        })
    }

    /// Confirm that one descriptor-relative basename is currently absent.
    /// Any existing object, including a symbolic link, is a collision.
    pub fn require_absent(&self, name: &OsStr) -> Result<(), DesktopFileError> {
        validate_name(name)?;
        if exists_at(&self.file, name, &self.path)? {
            return Err(DesktopFileError::Collision(
                self.path.join(name).display().to_string(),
            ));
        }
        Ok(())
    }

    /// Durably commit prior namespace changes in this verified Desktop.
    ///
    /// Recovery calls this even when it merely observes that an interrupted
    /// rename already reached its destination: the prior process may have
    /// died, or returned an error, between `renameat2` and directory fsync.
    pub fn sync(&self) -> Result<(), DesktopFileError> {
        self.file
            .sync_all()
            .map_err(|error| io_error(&self.path, error))
    }

    /// Rename a regular file only when it still has the journalled identity.
    /// The destination is never replaced, and the directory entry is synced
    /// before success is returned.
    pub fn rename_regular_file_verified(
        &self,
        old: &OsStr,
        new: &OsStr,
        expected: &FileObservation,
    ) -> Result<(), DesktopFileError> {
        validate_name(old)?;
        validate_name(new)?;
        if old == new {
            return if self.observe_regular_file(old)? == *expected {
                Ok(())
            } else {
                Err(DesktopFileError::ChangedIdentity(
                    self.path.join(old).display().to_string(),
                ))
            };
        }
        let before = self.observe_regular_file(old)?;
        if before != *expected {
            return Err(DesktopFileError::ChangedIdentity(
                self.path.join(old).display().to_string(),
            ));
        }
        if exists_at(&self.file, new, &self.path)? {
            return Err(DesktopFileError::Collision(
                self.path.join(new).display().to_string(),
            ));
        }
        rename_at2(&self.file, old, &self.file, new, libc::RENAME_NOREPLACE).map_err(|error| {
            if matches!(error.raw_os_error(), Some(libc::EEXIST | libc::ENOTEMPTY)) {
                DesktopFileError::Collision(self.path.join(new).display().to_string())
            } else {
                io_error(self.path.join(old), error)
            }
        })?;
        self.sync()?;
        if self.observe_regular_file(new)? != *expected {
            return Err(DesktopFileError::ChangedIdentity(
                self.path.join(new).display().to_string(),
            ));
        }
        Ok(())
    }

    pub fn copy_from(
        &self,
        source: &DesktopDirectory,
        source_name: &OsStr,
        preferred_name: &OsStr,
        collision: CollisionChoice,
    ) -> Result<PasteResult, DesktopFileError> {
        self.transfer_from(source, source_name, preferred_name, collision, false)
    }

    pub fn move_from(
        &self,
        source: &DesktopDirectory,
        source_name: &OsStr,
        preferred_name: &OsStr,
        collision: CollisionChoice,
    ) -> Result<PasteResult, DesktopFileError> {
        self.transfer_from(source, source_name, preferred_name, collision, true)
    }

    fn transfer_from(
        &self,
        source: &DesktopDirectory,
        source_name: &OsStr,
        preferred_name: &OsStr,
        collision: CollisionChoice,
        moving: bool,
    ) -> Result<PasteResult, DesktopFileError> {
        validate_name(source_name)?;
        validate_name(preferred_name)?;
        let source_stat = stat_at(&source.file, source_name, &source.path)?;
        ensure_supported(source_stat.st_mode, &source.path.join(source_name))?;
        let source_mount_id = mount_id_at(&source.file, source_name, &source.path)?;
        if source_stat.st_dev != source.device || source_mount_id != source.mount_id {
            return Err(DesktopFileError::MountBoundary(
                source.path.join(source_name).display().to_string(),
            ));
        }

        let destination_name = resolve_destination_name(self, preferred_name, collision)?;
        if moving && source.device == self.device && source.mount_id == self.mount_id {
            let destination_exists = exists_at(&self.file, &destination_name, &self.path)?;
            if destination_exists && collision == CollisionChoice::Replace {
                rename_at2(
                    &source.file,
                    source_name,
                    &self.file,
                    &destination_name,
                    libc::RENAME_EXCHANGE,
                )
                .map_err(|error| io_error(source.path.join(source_name), error))?;
                let mut budget = TraversalBudget::default();
                remove_entry(
                    &source.file,
                    source_name,
                    source.device,
                    source.mount_id,
                    0,
                    &source.path,
                    &mut budget,
                )?;
            } else {
                rename_at2(
                    &source.file,
                    source_name,
                    &self.file,
                    &destination_name,
                    libc::RENAME_NOREPLACE,
                )
                .map_err(|error| io_error(source.path.join(source_name), error))?;
            }
            source
                .file
                .sync_all()
                .map_err(|error| io_error(&source.path, error))?;
            if source.file.as_raw_fd() != self.file.as_raw_fd() {
                self.file
                    .sync_all()
                    .map_err(|error| io_error(&self.path, error))?;
            }
            return Ok(PasteResult {
                destination_name,
                source_removed: true,
            });
        }

        let staging = temporary_name("copy")?;
        let mut copy_budget = TraversalBudget::default();
        copy_entry(
            &source.file,
            source_name,
            source.device,
            source.mount_id,
            &self.file,
            &staging,
            0,
            &source.path.join(source_name),
            &self.path.join(&staging),
            &mut copy_budget,
        )?;
        if let Err(error) = commit_staging(self, &staging, &destination_name, collision) {
            let mut cleanup_budget = TraversalBudget::default();
            let _ = remove_entry(
                &self.file,
                &staging,
                self.device,
                self.mount_id,
                0,
                &self.path,
                &mut cleanup_budget,
            );
            return Err(error);
        }
        self.file
            .sync_all()
            .map_err(|error| io_error(&self.path, error))?;

        if moving {
            if let Err(error) = {
                let mut budget = TraversalBudget::default();
                remove_entry(
                    &source.file,
                    source_name,
                    source.device,
                    source.mount_id,
                    0,
                    &source.path,
                    &mut budget,
                )
            } {
                return Err(DesktopFileError::SourceRemovalAfterCopy(error.to_string()));
            }
            source
                .file
                .sync_all()
                .map_err(|error| io_error(&source.path, error))?;
        }
        Ok(PasteResult {
            destination_name,
            source_removed: moving,
        })
    }
}

fn authorize_desktop_launcher(
    directory: &File,
    name: &OsStr,
    observed: &libc::stat,
    display_directory: &Path,
) -> Result<(), DesktopFileError> {
    if mode_type(observed.st_mode) != libc::S_IFREG || observed.st_uid != unsafe { libc::geteuid() }
    {
        return Err(DesktopFileError::UnsupportedType(
            display_directory.join(name).display().to_string(),
        ));
    }
    let name_c = c_string(name)?;
    // SAFETY: the directory descriptor and validated NUL-terminated leaf are
    // live. O_NOFOLLOW prevents a launcher replacement symlink from being
    // authorized.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if descriptor < 0 {
        return Err(io_error(
            display_directory.join(name),
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: openat returned an owned descriptor.
    let file = unsafe { File::from_raw_fd(descriptor) };
    let current = file
        .metadata()
        .map_err(|error| io_error(display_directory.join(name), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if !current.is_file()
            || current.uid() != unsafe { libc::geteuid() }
            || current.dev() != observed.st_dev
            || current.ino() != observed.st_ino
        {
            return Err(DesktopFileError::ChangedIdentity(
                display_directory.join(name).display().to_string(),
            ));
        }
        let mode = current.permissions().mode() | libc::S_IXUSR;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| io_error(display_directory.join(name), error))?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct TraversalBudget {
    entries_remaining: u64,
    bytes_remaining: u64,
}

impl Default for TraversalBudget {
    fn default() -> Self {
        Self {
            entries_remaining: MAX_TRAVERSAL_ENTRIES,
            bytes_remaining: MAX_TRAVERSAL_BYTES,
        }
    }
}

impl TraversalBudget {
    fn charge(&mut self, stat: &libc::stat) -> Result<(), DesktopFileError> {
        self.entries_remaining = self
            .entries_remaining
            .checked_sub(1)
            .ok_or(DesktopFileError::TraversalLimit)?;
        let size = if matches!(mode_type(stat.st_mode), libc::S_IFREG | libc::S_IFLNK) {
            u64::try_from(stat.st_size).unwrap_or(u64::MAX)
        } else {
            0
        };
        self.bytes_remaining = self
            .bytes_remaining
            .checked_sub(size)
            .ok_or(DesktopFileError::TraversalLimit)?;
        Ok(())
    }
}

fn resolve_destination_name(
    destination: &DesktopDirectory,
    preferred: &OsStr,
    collision: CollisionChoice,
) -> Result<OsString, DesktopFileError> {
    if !exists_at(&destination.file, preferred, &destination.path)? {
        return Ok(preferred.to_os_string());
    }
    match collision {
        CollisionChoice::Cancel => Err(DesktopFileError::Cancelled),
        CollisionChoice::Replace => Ok(preferred.to_os_string()),
        CollisionChoice::KeepBoth => keep_both_name(destination, preferred),
    }
}

fn keep_both_name(
    destination: &DesktopDirectory,
    preferred: &OsStr,
) -> Result<OsString, DesktopFileError> {
    validate_name(preferred)?;
    if !exists_at(&destination.file, preferred, &destination.path)? {
        return Ok(preferred.to_os_string());
    }
    let path = Path::new(preferred);
    let stem = path.file_stem().unwrap_or(preferred).as_bytes();
    let extension = path.extension().map(OsStr::as_bytes);
    for index in 1..=9999u32 {
        let suffix = if index == 1 {
            b" (copy)".to_vec()
        } else {
            format!(" (copy {index})").into_bytes()
        };
        let extension_bytes = extension.map_or(0, |value| value.len() + 1);
        let maximum_stem = MAX_NAME_BYTES
            .saturating_sub(suffix.len())
            .saturating_sub(extension_bytes);
        let mut bytes = stem[..stem.len().min(maximum_stem)].to_vec();
        bytes.extend_from_slice(&suffix);
        if let Some(extension) = extension {
            bytes.push(b'.');
            bytes.extend_from_slice(extension);
        }
        let candidate = OsString::from_vec(bytes);
        if !exists_at(&destination.file, &candidate, &destination.path)? {
            return Ok(candidate);
        }
    }
    Err(DesktopFileError::Collision(
        destination.path.join(preferred).display().to_string(),
    ))
}

fn commit_staging(
    destination: &DesktopDirectory,
    staging: &OsStr,
    final_name: &OsStr,
    collision: CollisionChoice,
) -> Result<(), DesktopFileError> {
    let exists = exists_at(&destination.file, final_name, &destination.path)?;
    if exists {
        match collision {
            CollisionChoice::Cancel => return Err(DesktopFileError::Cancelled),
            CollisionChoice::KeepBoth => {
                return Err(DesktopFileError::Collision(
                    destination.path.join(final_name).display().to_string(),
                ));
            }
            CollisionChoice::Replace => {
                rename_at2(
                    &destination.file,
                    staging,
                    &destination.file,
                    final_name,
                    libc::RENAME_EXCHANGE,
                )
                .map_err(|error| io_error(destination.path.join(final_name), error))?;
                let mut budget = TraversalBudget::default();
                remove_entry(
                    &destination.file,
                    staging,
                    destination.device,
                    destination.mount_id,
                    0,
                    &destination.path,
                    &mut budget,
                )?;
                return Ok(());
            }
        }
    }
    rename_at2(
        &destination.file,
        staging,
        &destination.file,
        final_name,
        libc::RENAME_NOREPLACE,
    )
    .map_err(|error| io_error(destination.path.join(final_name), error))
}

#[allow(clippy::too_many_arguments)]
fn copy_entry(
    source_directory: &File,
    source_name: &OsStr,
    source_device: u64,
    source_mount_id: u64,
    destination_directory: &File,
    destination_name: &OsStr,
    depth: usize,
    source_display: &Path,
    destination_display: &Path,
    budget: &mut TraversalBudget,
) -> Result<(), DesktopFileError> {
    if depth > MAX_RECURSION_DEPTH {
        return Err(DesktopFileError::TraversalLimit);
    }
    let stat = stat_at(source_directory, source_name, source_display)?;
    let mount_id = mount_id_at(source_directory, source_name, source_display)?;
    if stat.st_dev != source_device || mount_id != source_mount_id {
        return Err(DesktopFileError::MountBoundary(
            source_display.display().to_string(),
        ));
    }
    budget.charge(&stat)?;
    match mode_type(stat.st_mode) {
        libc::S_IFREG => copy_regular(
            source_directory,
            source_name,
            &stat,
            mount_id,
            destination_directory,
            destination_name,
            source_display,
            destination_display,
        ),
        libc::S_IFLNK => copy_symlink(
            source_directory,
            source_name,
            &stat,
            mount_id,
            destination_directory,
            destination_name,
            source_display,
            destination_display,
        ),
        libc::S_IFDIR => copy_directory(
            source_directory,
            source_name,
            source_device,
            source_mount_id,
            &stat,
            mount_id,
            destination_directory,
            destination_name,
            depth,
            source_display,
            destination_display,
            budget,
        ),
        _ => Err(DesktopFileError::UnsupportedType(
            source_display.display().to_string(),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_regular(
    source_directory: &File,
    source_name: &OsStr,
    expected: &libc::stat,
    expected_mount_id: u64,
    destination_directory: &File,
    destination_name: &OsStr,
    source_display: &Path,
    destination_display: &Path,
) -> Result<(), DesktopFileError> {
    let source_c = c_string(source_name)?;
    // SAFETY: openat arguments remain valid; the returned fd is owned below.
    let source_fd = unsafe {
        libc::openat(
            source_directory.as_raw_fd(),
            source_c.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if source_fd < 0 {
        return Err(io_error(source_display, std::io::Error::last_os_error()));
    }
    // SAFETY: openat returned a fresh owned fd.
    let mut source = unsafe { File::from_raw_fd(source_fd) };
    let actual = fstat(&source, source_display)?;
    if mode_type(actual.st_mode) != libc::S_IFREG
        || actual.st_dev != expected.st_dev
        || actual.st_ino != expected.st_ino
        || mount_id_fd(&source, source_display)? != expected_mount_id
    {
        return Err(DesktopFileError::UnsupportedType(
            source_display.display().to_string(),
        ));
    }
    let destination_c = c_string(destination_name)?;
    let mode = (actual.st_mode as u32) & 0o777;
    // SAFETY: openat arguments remain valid; O_EXCL prevents replacement.
    let destination_fd = unsafe {
        libc::openat(
            destination_directory.as_raw_fd(),
            destination_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    if destination_fd < 0 {
        return Err(io_error(
            destination_display,
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: openat returned a fresh owned fd.
    let mut destination = unsafe { File::from_raw_fd(destination_fd) };
    let operation = (|| {
        let expected_length = u64::try_from(actual.st_size).unwrap_or(u64::MAX);
        let copied = std::io::copy(
            &mut std::io::Read::by_ref(&mut source).take(expected_length.saturating_add(1)),
            &mut destination,
        )
        .map_err(|error| io_error(destination_display, error))?;
        if copied > expected_length {
            return Err(DesktopFileError::TraversalLimit);
        }
        destination
            .flush()
            .map_err(|error| io_error(destination_display, error))?;
        destination
            .sync_all()
            .map_err(|error| io_error(destination_display, error))
    })();
    if operation.is_err() {
        let _ = unlink_at(destination_directory, destination_name, false);
    }
    operation
}

#[allow(clippy::too_many_arguments)]
fn copy_symlink(
    source_directory: &File,
    source_name: &OsStr,
    expected: &libc::stat,
    expected_mount_id: u64,
    destination_directory: &File,
    destination_name: &OsStr,
    source_display: &Path,
    destination_display: &Path,
) -> Result<(), DesktopFileError> {
    let source_c = c_string(source_name)?;
    let mut bytes = vec![0u8; 4096];
    // SAFETY: buffers and descriptor are valid for readlinkat.
    let length = unsafe {
        libc::readlinkat(
            source_directory.as_raw_fd(),
            source_c.as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if length < 0 {
        return Err(io_error(source_display, std::io::Error::last_os_error()));
    }
    let length = usize::try_from(length).unwrap_or(bytes.len());
    if length == bytes.len() {
        return Err(DesktopFileError::NameTooLong);
    }
    let actual = stat_at(source_directory, source_name, source_display)?;
    let actual_mount_id = mount_id_at(source_directory, source_name, source_display)?;
    if mode_type(actual.st_mode) != libc::S_IFLNK
        || actual.st_dev != expected.st_dev
        || actual.st_ino != expected.st_ino
        || actual_mount_id != expected_mount_id
    {
        return Err(DesktopFileError::UnsupportedType(
            source_display.display().to_string(),
        ));
    }
    bytes.truncate(length);
    let target = CString::new(bytes).map_err(|_| DesktopFileError::InvalidName)?;
    let destination_c = c_string(destination_name)?;
    // SAFETY: target, destination and directory descriptor are valid.
    if unsafe {
        libc::symlinkat(
            target.as_ptr(),
            destination_directory.as_raw_fd(),
            destination_c.as_ptr(),
        )
    } != 0
    {
        return Err(io_error(
            destination_display,
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn copy_directory(
    source_directory: &File,
    source_name: &OsStr,
    source_device: u64,
    source_mount_id: u64,
    expected: &libc::stat,
    expected_mount_id: u64,
    destination_directory: &File,
    destination_name: &OsStr,
    depth: usize,
    source_display: &Path,
    destination_display: &Path,
    budget: &mut TraversalBudget,
) -> Result<(), DesktopFileError> {
    let source = open_directory_at_verified(
        source_directory,
        source_name,
        expected,
        expected_mount_id,
        source_display,
    )?;
    let destination_c = c_string(destination_name)?;
    // SAFETY: descriptor and destination name are valid.
    if unsafe {
        libc::mkdirat(
            destination_directory.as_raw_fd(),
            destination_c.as_ptr(),
            0o700,
        )
    } != 0
    {
        return Err(io_error(
            destination_display,
            std::io::Error::last_os_error(),
        ));
    }
    let destination =
        match open_directory_at(destination_directory, destination_name, destination_display) {
            Ok(directory) => directory,
            Err(error) => {
                let _ = unlink_at(destination_directory, destination_name, true);
                return Err(error);
            }
        };
    let operation = (|| {
        for child in directory_names(&source, source_display)? {
            copy_entry(
                &source,
                &child,
                source_device,
                source_mount_id,
                &destination,
                &child,
                depth + 1,
                &source_display.join(&child),
                &destination_display.join(&child),
                budget,
            )?;
        }
        set_mode(&destination, expected.st_mode & 0o777, destination_display)?;
        destination
            .sync_all()
            .map_err(|error| io_error(destination_display, error))
    })();
    if operation.is_err() {
        let mut cleanup_budget = TraversalBudget::default();
        let destination_root_stat = fstat(destination_directory, destination_display)?;
        let destination_root_mount = mount_id_fd(destination_directory, destination_display)?;
        let _ = remove_entry(
            destination_directory,
            destination_name,
            destination_root_stat.st_dev,
            destination_root_mount,
            depth,
            destination_display.parent().unwrap_or(destination_display),
            &mut cleanup_budget,
        );
    }
    operation
}

fn remove_entry(
    directory: &File,
    name: &OsStr,
    root_device: u64,
    root_mount_id: u64,
    depth: usize,
    display_directory: &Path,
    budget: &mut TraversalBudget,
) -> Result<(), DesktopFileError> {
    if depth > MAX_RECURSION_DEPTH {
        return Err(DesktopFileError::TraversalLimit);
    }
    let display = display_directory.join(name);
    let stat = stat_at(directory, name, &display)?;
    let mount_id = mount_id_at(directory, name, &display)?;
    if stat.st_dev != root_device || mount_id != root_mount_id {
        return Err(DesktopFileError::MountBoundary(
            display.display().to_string(),
        ));
    }
    budget.charge(&stat)?;
    match mode_type(stat.st_mode) {
        libc::S_IFREG | libc::S_IFLNK => {
            unlink_at(directory, name, false).map_err(|error| io_error(&display, error))
        }
        libc::S_IFDIR => {
            let child = open_directory_at_verified(directory, name, &stat, mount_id, &display)?;
            for entry in directory_names(&child, &display)? {
                remove_entry(
                    &child,
                    &entry,
                    root_device,
                    root_mount_id,
                    depth + 1,
                    &display,
                    budget,
                )?;
            }
            child
                .sync_all()
                .map_err(|error| io_error(&display, error))?;
            let final_stat = stat_at(directory, name, &display)?;
            let final_mount_id = mount_id_at(directory, name, &display)?;
            if mode_type(final_stat.st_mode) != libc::S_IFDIR
                || final_stat.st_dev != stat.st_dev
                || final_stat.st_ino != stat.st_ino
                || final_mount_id != mount_id
            {
                return Err(DesktopFileError::UnsupportedType(
                    display.display().to_string(),
                ));
            }
            unlink_at(directory, name, true).map_err(|error| io_error(&display, error))
        }
        _ => Err(DesktopFileError::UnsupportedType(
            display.display().to_string(),
        )),
    }
}

fn item_kind(
    directory: &File,
    name: &OsStr,
    stat: &libc::stat,
    display_directory: &Path,
) -> Result<DesktopItemKind, DesktopFileError> {
    Ok(match mode_type(stat.st_mode) {
        libc::S_IFLNK => DesktopItemKind::SymbolicLink,
        libc::S_IFDIR => DesktopItemKind::Directory,
        libc::S_IFREG if Path::new(name).extension() == Some(OsStr::new("desktop")) => {
            DesktopItemKind::Launcher
        }
        libc::S_IFREG if has_appimage_marker(directory, name, display_directory)? => {
            DesktopItemKind::AppImage
        }
        libc::S_IFREG => DesktopItemKind::RegularFile,
        _ => {
            return Err(DesktopFileError::UnsupportedType(
                display_directory.join(name).display().to_string(),
            ));
        }
    })
}

fn has_appimage_marker(
    directory: &File,
    name: &OsStr,
    display_directory: &Path,
) -> Result<bool, DesktopFileError> {
    let name_c = c_string(name)?;
    // SAFETY: arguments remain valid and returned fd is owned below.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(io_error(
            display_directory.join(name),
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: openat returned a fresh fd.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut header = [0u8; APPIMAGE_HEADER_LEN];
    let read = file
        .read(&mut header)
        .map_err(|error| io_error(display_directory.join(name), error))?;
    Ok(read == APPIMAGE_HEADER_LEN && &header[..4] == b"\x7fELF" && &header[8..11] == b"AI\x02")
}

fn ensure_supported(mode: libc::mode_t, display: &Path) -> Result<(), DesktopFileError> {
    if matches!(
        mode_type(mode),
        libc::S_IFREG | libc::S_IFDIR | libc::S_IFLNK
    ) {
        Ok(())
    } else {
        Err(DesktopFileError::UnsupportedType(
            display.display().to_string(),
        ))
    }
}

fn directory_names(directory: &File, display: &Path) -> Result<Vec<OsString>, DesktopFileError> {
    // `dup(2)` would create another descriptor for the *same open file
    // description*, including its directory offset. After the first readdir,
    // every later model refresh would therefore start at end-of-directory and
    // incorrectly report an empty Desktop. Open `.` relative to the already
    // verified directory instead: this keeps path traversal out of the
    // operation while obtaining a fresh file description and offset for each
    // event-driven scan.
    let dot = c".";
    // SAFETY: directory is a live O_DIRECTORY descriptor, dot is terminated,
    // and a successful openat returns a fresh descriptor owned below.
    let listing = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if listing < 0 {
        return Err(io_error(display, std::io::Error::last_os_error()));
    }
    // SAFETY: listing is an owned directory descriptor.
    let stream = unsafe { libc::fdopendir(listing) };
    if stream.is_null() {
        // SAFETY: fdopendir failed and did not take ownership.
        unsafe { libc::close(listing) };
        return Err(io_error(display, std::io::Error::last_os_error()));
    }
    let mut names = Vec::new();
    loop {
        // POSIX distinguishes end-of-directory from failure by leaving errno
        // at zero. Clear it immediately before every readdir call.
        // SAFETY: Buzzard OS's guest target is Linux and errno is thread-local.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: stream is valid until closed below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                // SAFETY: closes stream and listing descriptor.
                unsafe { libc::closedir(stream) };
                return Err(io_error(display, error));
            }
            break;
        }
        // SAFETY: d_name is NUL-terminated by readdir.
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if names.len() >= MAX_DIRECTORY_ENTRIES {
            // SAFETY: closes stream and listing descriptor.
            unsafe { libc::closedir(stream) };
            return Err(DesktopFileError::TraversalLimit);
        }
        names.push(OsString::from_vec(bytes.to_vec()));
    }
    // SAFETY: closes stream and listing descriptor.
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(io_error(display, std::io::Error::last_os_error()));
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}

fn open_directory_at(
    parent: &File,
    name: &OsStr,
    display: &Path,
) -> Result<File, DesktopFileError> {
    let name = c_string(name)?;
    // SAFETY: arguments remain valid and returned fd is owned below.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(io_error(display, std::io::Error::last_os_error()));
    }
    // SAFETY: openat returned a fresh owned fd.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_directory_at_verified(
    parent: &File,
    name: &OsStr,
    expected: &libc::stat,
    expected_mount_id: u64,
    display: &Path,
) -> Result<File, DesktopFileError> {
    let directory = open_directory_at(parent, name, display)?;
    let actual = fstat(&directory, display)?;
    let actual_mount_id = mount_id_fd(&directory, display)?;
    if mode_type(actual.st_mode) != libc::S_IFDIR
        || actual.st_dev != expected.st_dev
        || actual.st_ino != expected.st_ino
        || actual_mount_id != expected_mount_id
    {
        return Err(DesktopFileError::UnsupportedType(
            display.display().to_string(),
        ));
    }
    Ok(directory)
}

fn set_mode(file: &File, mode: u32, display: &Path) -> Result<(), DesktopFileError> {
    // SAFETY: fd is valid and mode has already been stripped to permission bits.
    if unsafe { libc::fchmod(file.as_raw_fd(), mode) } != 0 {
        return Err(io_error(display, std::io::Error::last_os_error()));
    }
    Ok(())
}

fn fstat(file: &File, display: &Path) -> Result<libc::stat, DesktopFileError> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fd is valid and stat points to writable storage.
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io_error(display, std::io::Error::last_os_error()));
    }
    // SAFETY: successful fstat initialized stat.
    Ok(unsafe { stat.assume_init() })
}

fn stat_at(
    directory: &File,
    name: &OsStr,
    display_directory: &Path,
) -> Result<libc::stat, DesktopFileError> {
    let name_c = c_string(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: args are valid and stat points to writable storage.
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name_c.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Err(DesktopFileError::Missing(
                display_directory.join(name).display().to_string(),
            ))
        } else {
            Err(io_error(display_directory.join(name), error))
        };
    }
    // SAFETY: successful fstatat initialized stat.
    Ok(unsafe { stat.assume_init() })
}

fn mount_id_fd(file: &File, display: &Path) -> Result<u64, DesktopFileError> {
    let empty = c"";
    statx_mount_id(
        file.as_raw_fd(),
        empty,
        libc::AT_EMPTY_PATH | libc::AT_NO_AUTOMOUNT,
        display,
    )
}

fn mount_id_at(
    directory: &File,
    name: &OsStr,
    display_directory: &Path,
) -> Result<u64, DesktopFileError> {
    let name_c = c_string(name)?;
    statx_mount_id(
        directory.as_raw_fd(),
        &name_c,
        libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT,
        &display_directory.join(name),
    )
}

fn statx_mount_id(
    directory_fd: libc::c_int,
    name: &CStr,
    flags: libc::c_int,
    display: &Path,
) -> Result<u64, DesktopFileError> {
    let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: descriptor, name and output storage are valid for statx.
    if unsafe {
        libc::statx(
            directory_fd,
            name.as_ptr(),
            flags,
            libc::STATX_BASIC_STATS | libc::STATX_MNT_ID,
            statx.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io_error(display, std::io::Error::last_os_error()));
    }
    // SAFETY: successful statx initialized the structure.
    let statx = unsafe { statx.assume_init() };
    if statx.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(DesktopFileError::MissingMountIdentity(
            display.display().to_string(),
        ));
    }
    Ok(statx.stx_mnt_id)
}

fn exists_at(
    directory: &File,
    name: &OsStr,
    display_directory: &Path,
) -> Result<bool, DesktopFileError> {
    match stat_at(directory, name, display_directory) {
        Ok(_) => Ok(true),
        Err(DesktopFileError::Missing(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

fn rename_at2(
    source_directory: &File,
    source: &OsStr,
    destination_directory: &File,
    destination: &OsStr,
    flags: u32,
) -> std::io::Result<()> {
    let source = c_string(source).map_err(|error| std::io::Error::other(error.to_string()))?;
    let destination =
        c_string(destination).map_err(|error| std::io::Error::other(error.to_string()))?;
    // SAFETY: both descriptors and names are valid for the syscall.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_directory.as_raw_fd(),
            source.as_ptr(),
            destination_directory.as_raw_fd(),
            destination.as_ptr(),
            flags,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn unlink_at(directory: &File, name: &OsStr, directory_flag: bool) -> std::io::Result<()> {
    let name = c_string(name).map_err(|error| std::io::Error::other(error.to_string()))?;
    // SAFETY: descriptor and name are valid.
    let result = unsafe {
        libc::unlinkat(
            directory.as_raw_fd(),
            name.as_ptr(),
            if directory_flag {
                libc::AT_REMOVEDIR
            } else {
                0
            },
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn mode_type(mode: libc::mode_t) -> libc::mode_t {
    mode & libc::S_IFMT
}

fn validate_name(name: &OsStr) -> Result<(), DesktopFileError> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(DesktopFileError::InvalidName);
    }
    if bytes.len() > MAX_NAME_BYTES {
        return Err(DesktopFileError::NameTooLong);
    }
    CString::new(bytes).map_err(|_| DesktopFileError::InvalidName)?;
    Ok(())
}

fn c_string(name: &OsStr) -> Result<CString, DesktopFileError> {
    validate_name(name)?;
    CString::new(name.as_bytes()).map_err(|_| DesktopFileError::InvalidName)
}

fn temporary_name(purpose: &str) -> Result<OsString, DesktopFileError> {
    let name = OsString::from(format!(".buzzardos-{purpose}-{}", Uuid::new_v4()));
    validate_name(&name)?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn directories() -> (tempfile::TempDir, DesktopDirectory, DesktopDirectory) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        let source = DesktopDirectory::open(&source).unwrap();
        let destination = DesktopDirectory::open(&destination).unwrap();
        (temp, source, destination)
    }

    #[test]
    fn listing_classifies_files_without_following_links() {
        let (_temp, source, _destination) = directories();
        fs::write(source.path().join("note.txt"), b"hello").unwrap();
        fs::write(
            source.path().join("Tool.AppImage"),
            b"\x7fELF\x02\x01\x01\x00AI\x02payload",
        )
        .unwrap();
        fs::write(
            source.path().join("tool.desktop"),
            b"[Desktop Entry]\nType=Application\n",
        )
        .unwrap();
        fs::create_dir(source.path().join("folder")).unwrap();
        symlink("folder", source.path().join("folder-link")).unwrap();
        let items = source.list().unwrap();
        let by_name: BTreeMap<_, _> = items
            .into_iter()
            .map(|item| (item.name, item.kind))
            .collect();
        assert_eq!(
            by_name[OsStr::new("note.txt")],
            DesktopItemKind::RegularFile
        );
        assert_eq!(
            by_name[OsStr::new("Tool.AppImage")],
            DesktopItemKind::AppImage
        );
        assert_eq!(
            by_name[OsStr::new("tool.desktop")],
            DesktopItemKind::Launcher
        );
        assert_ne!(
            fs::metadata(source.path().join("tool.desktop"))
                .unwrap()
                .permissions()
                .mode()
                & libc::S_IXUSR,
            0,
            "validated owner desktop launchers are trusted on discovery"
        );
        assert_eq!(
            fs::metadata(source.path().join("note.txt"))
                .unwrap()
                .permissions()
                .mode()
                & libc::S_IXUSR,
            0,
            "ordinary files never gain execute permission"
        );
        assert_eq!(by_name[OsStr::new("folder")], DesktopItemKind::Directory);
        assert_eq!(
            by_name[OsStr::new("folder-link")],
            DesktopItemKind::SymbolicLink
        );
    }

    #[test]
    fn repeated_listing_starts_from_the_beginning_after_a_live_mutation() {
        let (_temp, source, _destination) = directories();
        fs::write(source.path().join("first.txt"), b"first").unwrap();

        assert_eq!(
            source
                .list()
                .unwrap()
                .into_iter()
                .map(|item| item.name)
                .collect::<Vec<_>>(),
            vec![OsString::from("first.txt")]
        );

        fs::write(source.path().join("second.txt"), b"second").unwrap();
        assert_eq!(
            source
                .list()
                .unwrap()
                .into_iter()
                .map(|item| item.name)
                .collect::<Vec<_>>(),
            vec![OsString::from("first.txt"), OsString::from("second.txt")]
        );
    }

    #[test]
    fn delete_link_and_shortcut_never_delete_targets() {
        let (_temp, source, _destination) = directories();
        fs::write(source.path().join("target"), b"keep").unwrap();
        symlink("target", source.path().join("link")).unwrap();
        fs::write(source.path().join("shortcut.desktop"), b"target-data").unwrap();
        assert_eq!(
            source.consequence(OsStr::new("link")).unwrap(),
            DeleteConsequence::LinkOnly
        );
        source.delete_confirmed(OsStr::new("link")).unwrap();
        assert_eq!(fs::read(source.path().join("target")).unwrap(), b"keep");
        assert_eq!(
            source.consequence(OsStr::new("shortcut.desktop")).unwrap(),
            DeleteConsequence::ShortcutOnly
        );
        source
            .delete_confirmed(OsStr::new("shortcut.desktop"))
            .unwrap();
        assert_eq!(fs::read(source.path().join("target")).unwrap(), b"keep");
    }

    #[test]
    fn recursive_delete_does_not_follow_nested_symlinks() {
        let (temp, source, _destination) = directories();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("evidence"), b"safe").unwrap();
        let tree = source.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("inside"), b"delete").unwrap();
        symlink(&outside, tree.join("escape")).unwrap();
        source.delete_confirmed(OsStr::new("tree")).unwrap();
        assert_eq!(fs::read(outside.join("evidence")).unwrap(), b"safe");
    }

    #[test]
    fn copy_and_move_preserve_links_and_support_collision_choices() {
        let (_temp, source, destination) = directories();
        fs::write(source.path().join("item.txt"), b"one").unwrap();
        symlink("item.txt", source.path().join("item-link")).unwrap();
        destination
            .copy_from(
                &source,
                OsStr::new("item-link"),
                OsStr::new("item-link"),
                CollisionChoice::Cancel,
            )
            .unwrap();
        assert_eq!(
            fs::read_link(destination.path().join("item-link")).unwrap(),
            PathBuf::from("item.txt")
        );
        fs::write(destination.path().join("item.txt"), b"old").unwrap();
        destination
            .copy_from(
                &source,
                OsStr::new("item.txt"),
                OsStr::new("item.txt"),
                CollisionChoice::Replace,
            )
            .unwrap();
        assert_eq!(
            fs::read(destination.path().join("item.txt")).unwrap(),
            b"one"
        );
        let kept = destination
            .copy_from(
                &source,
                OsStr::new("item.txt"),
                OsStr::new("item.txt"),
                CollisionChoice::KeepBoth,
            )
            .unwrap();
        assert_eq!(kept.destination_name, OsStr::new("item (copy).txt"));
        let moved = destination
            .move_from(
                &source,
                OsStr::new("item.txt"),
                OsStr::new("moved.txt"),
                CollisionChoice::Cancel,
            )
            .unwrap();
        assert!(moved.source_removed);
        assert!(!source.path().join("item.txt").exists());
        assert_eq!(
            fs::read(destination.path().join("moved.txt")).unwrap(),
            b"one"
        );
    }

    #[test]
    fn copy_directory_is_durable_shape_and_does_not_follow_links() {
        let (temp, source, destination) = directories();
        let outside = temp.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        let tree = source.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("inside"), b"inside").unwrap();
        symlink(&outside, tree.join("external-link")).unwrap();
        destination
            .copy_from(
                &source,
                OsStr::new("tree"),
                OsStr::new("tree"),
                CollisionChoice::Cancel,
            )
            .unwrap();
        assert_eq!(
            fs::read(destination.path().join("tree/inside")).unwrap(),
            b"inside"
        );
        assert!(
            fs::symlink_metadata(destination.path().join("tree/external-link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn directory_copy_preserves_safe_rwx_bits_without_special_bits() {
        let (_temp, source, destination) = directories();
        let tree = source.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::set_permissions(&tree, fs::Permissions::from_mode(0o2751)).unwrap();
        fs::write(tree.join("inside"), b"inside").unwrap();
        destination
            .copy_from(
                &source,
                OsStr::new("tree"),
                OsStr::new("tree"),
                CollisionChoice::Cancel,
            )
            .unwrap();
        assert_eq!(
            fs::metadata(destination.path().join("tree"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o751
        );
    }

    #[test]
    fn traversal_budget_is_global_across_nested_directories() {
        let (_temp, source, destination) = directories();
        let tree = source.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("one"), b"1").unwrap();
        fs::write(tree.join("two"), b"2").unwrap();
        let mut budget = TraversalBudget {
            entries_remaining: 2,
            bytes_remaining: 1024,
        };
        let error = copy_entry(
            &source.file,
            OsStr::new("tree"),
            source.device,
            source.mount_id,
            &destination.file,
            OsStr::new("staging"),
            0,
            &source.path.join("tree"),
            &destination.path.join("staging"),
            &mut budget,
        )
        .unwrap_err();
        assert!(matches!(error, DesktopFileError::TraversalLimit));
    }

    #[test]
    fn traversal_budget_bounds_total_regular_file_bytes() {
        let (_temp, source, destination) = directories();
        fs::write(source.path().join("large"), b"12345").unwrap();
        let mut budget = TraversalBudget {
            entries_remaining: 2,
            bytes_remaining: 4,
        };
        let error = copy_entry(
            &source.file,
            OsStr::new("large"),
            source.device,
            source.mount_id,
            &destination.file,
            OsStr::new("staging"),
            0,
            &source.path.join("large"),
            &destination.path.join("staging"),
            &mut budget,
        )
        .unwrap_err();
        assert!(matches!(error, DesktopFileError::TraversalLimit));
        assert!(!destination.path().join("staging").exists());
    }

    #[test]
    fn verified_directory_open_rejects_a_changed_identity_or_mount() {
        let (_temp, source, _destination) = directories();
        fs::create_dir(source.path().join("expected")).unwrap();
        fs::create_dir(source.path().join("replacement")).unwrap();
        let expected = stat_at(&source.file, OsStr::new("expected"), source.path()).unwrap();
        let expected_mount =
            mount_id_at(&source.file, OsStr::new("expected"), source.path()).unwrap();
        assert!(
            open_directory_at_verified(
                &source.file,
                OsStr::new("replacement"),
                &expected,
                expected_mount,
                &source.path.join("replacement"),
            )
            .is_err()
        );
    }

    #[test]
    fn rename_refuses_traversal_and_collisions() {
        let (_temp, source, _destination) = directories();
        fs::write(source.path().join("a"), b"a").unwrap();
        fs::write(source.path().join("b"), b"b").unwrap();
        assert!(matches!(
            source.rename(OsStr::new("a"), OsStr::new("../escape")),
            Err(DesktopFileError::InvalidName)
        ));
        assert!(matches!(
            source.rename(OsStr::new("a"), OsStr::new("b")),
            Err(DesktopFileError::Collision(_))
        ));
        source
            .rename(OsStr::new("a"), OsStr::new("renamed"))
            .unwrap();
        assert_eq!(fs::read(source.path().join("renamed")).unwrap(), b"a");
    }

    #[test]
    fn verified_regular_rename_rejects_a_replaced_source_and_never_touches_destination() {
        let (_temp, source, _destination) = directories();
        fs::write(source.path().join("old.AppImage"), b"original").unwrap();
        let expected = source
            .observe_regular_file(OsStr::new("old.AppImage"))
            .unwrap();
        fs::rename(
            source.path().join("old.AppImage"),
            source.path().join("retained.AppImage"),
        )
        .unwrap();
        fs::write(source.path().join("old.AppImage"), b"replacement").unwrap();
        assert!(matches!(
            source.rename_regular_file_verified(
                OsStr::new("old.AppImage"),
                OsStr::new("new.AppImage"),
                &expected,
            ),
            Err(DesktopFileError::ChangedIdentity(_))
        ));
        assert_eq!(
            fs::read(source.path().join("retained.AppImage")).unwrap(),
            b"original"
        );
        assert_eq!(
            fs::read(source.path().join("old.AppImage")).unwrap(),
            b"replacement"
        );
        assert!(!source.path().join("new.AppImage").exists());
    }

    #[test]
    fn layout_uses_stable_file_identity_and_prunes_stale_entries() {
        let (_temp, source, _destination) = directories();
        let path = source.path().join("item");
        fs::write(&path, b"x").unwrap();
        let item = source.list().unwrap().pop().unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(item.identity.inode, metadata.ino());
        let mut layout = DesktopLayout::default();
        layout.positions.insert(
            item.identity.layout_key(),
            DesktopPosition {
                column: 2,
                row: 3,
                page: 1,
            },
        );
        layout.positions.insert(
            "999:999".into(),
            DesktopPosition {
                column: 0,
                row: 0,
                page: 0,
            },
        );
        layout.retain_items(&[item]);
        assert_eq!(layout.positions.len(), 1);
    }

    #[test]
    fn layout_rejects_unbounded_coordinates_and_entry_counts() {
        let mut layout = DesktopLayout::default();
        layout.positions.insert(
            "1:1".into(),
            DesktopPosition {
                column: MAX_LAYOUT_COORDINATE + 1,
                row: 0,
                page: 0,
            },
        );
        assert!(matches!(
            layout.validate(),
            Err(DesktopFileError::LayoutLimit)
        ));

        let mut layout = DesktopLayout::default();
        for index in 0..=MAX_LAYOUT_ENTRIES {
            layout.positions.insert(
                format!("1:{index}"),
                DesktopPosition {
                    column: 0,
                    row: 0,
                    page: 0,
                },
            );
        }
        assert!(matches!(
            layout.validate(),
            Err(DesktopFileError::LayoutLimit)
        ));
    }
}
