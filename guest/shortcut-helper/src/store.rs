// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::HELPER_EXECUTABLE;
use crate::inspector::{InspectedAppImage, InspectionError, validate_appimage};
use buzzardos_desktop_core::persistence::PersistenceError;
use buzzardos_desktop_core::{
    APPIMAGE_REGISTRATION_SCHEMA_VERSION, AppImageIcon, AppImageRegistration, DesktopDirectory,
    DesktopFileError, FileObservation, GeneratedAppImageDesktopEntry, RegistrationId, XdgPaths,
    atomic_write, atomic_write_json, read_bounded,
};
use gio::prelude::*;
use serde::{Deserialize, Serialize};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const MAX_REGISTRATIONS: usize = 4096;
const MAX_PROJECTION_BACKUP_BYTES: usize = 8 * 1024 * 1024;
const DESKTOP_RENAME_JOURNAL_SCHEMA_VERSION: u32 = 1;
const DESKTOP_RENAME_JOURNAL: &str = "appimage-desktop-rename.json";
const DESKTOP_RENAME_LOCK: &str = "appimage-desktop-rename.lock";
const FALLBACK_ICON: &[u8] = include_bytes!("../assets/appimage-fallback.svg");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DesktopRenamePhase {
    Prepared,
    FileRenamed,
    RegistrationUpdated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DesktopRenameJournal {
    schema_version: u32,
    registration_id: RegistrationId,
    desktop_path: Vec<u8>,
    old_name: Vec<u8>,
    new_name: Vec<u8>,
    expected_file: FileObservation,
    phase: DesktopRenamePhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopRenameFault {
    PreparedJournal,
    FileRename,
    FilePhase,
    RegistrationUpdate,
    RegistrationPhase,
    JournalClear,
    #[cfg(test)]
    DestinationCollision,
    RecoveryDirectorySync,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistrationFlags {
    pub applications_launcher: bool,
    pub desktop_shortcut: bool,
}

impl RegistrationFlags {
    pub const APPLICATIONS: Self = Self {
        applications_launcher: true,
        desktop_shortcut: false,
    };
    pub const DESKTOP: Self = Self {
        applications_launcher: false,
        desktop_shortcut: true,
    };
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchStatus {
    Started,
    TargetMissing,
    TargetInvalid,
}

#[derive(Debug)]
pub struct LaunchResult {
    pub status: LaunchStatus,
    pub registration: AppImageRegistration,
    pub child: Option<Child>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelinkPreview {
    pub current: AppImageRegistration,
    pub candidate: InspectedAppImage,
    pub candidate_path: PathBuf,
    pub identity_differs: bool,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Inspection(#[from] InspectionError),
    #[error(transparent)]
    Registration(#[from] buzzardos_desktop_core::appimage::RegistrationError),
    #[error(transparent)]
    DesktopEntry(#[from] buzzardos_desktop_core::desktop_entry::DesktopEntryError),
    #[error(transparent)]
    Xdg(#[from] buzzardos_desktop_core::xdg::XdgPathError),
    #[error("AppImage registration does not exist: {0}")]
    MissingRegistration(RegistrationId),
    #[error("registration directory contains too many records")]
    RegistrationLimit,
    #[error("managed path is unsafe: {0}")]
    UnsafeManagedPath(String),
    #[error("desktop rename transaction is ambiguous and was left intact: {0}")]
    AmbiguousDesktopRename(String),
    #[error("replacement identity differs: expected {expected}, found {found}")]
    IdentityMismatch { expected: String, found: String },
    #[error("I/O error for {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "projection failed ({operation}) and restoring the previous projection also failed ({rollback})"
    )]
    ProjectionRollback { operation: String, rollback: String },
    #[error(transparent)]
    DesktopFile(#[from] DesktopFileError),
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error("injected desktop rename interruption after {0}")]
    InjectedDesktopRenameFault(&'static str),
}

fn io_error(path: impl AsRef<Path>, source: std::io::Error) -> StoreError {
    StoreError::Io {
        path: path.as_ref().display().to_string(),
        source,
    }
}

#[derive(Debug, Clone)]
pub struct RegistrationStore {
    paths: XdgPaths,
    helper: PathBuf,
}

impl RegistrationStore {
    pub fn discover() -> Result<Self, StoreError> {
        Self::new(XdgPaths::discover()?, PathBuf::from(HELPER_EXECUTABLE))
    }

    pub fn new(paths: XdgPaths, helper: PathBuf) -> Result<Self, StoreError> {
        if !helper.is_absolute()
            || helper.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
        {
            return Err(StoreError::UnsafeManagedPath(helper.display().to_string()));
        }
        paths.ensure_private_directories()?;
        ensure_real_directory(&paths.user_applications_dir(), 0o700)?;
        ensure_real_directory(&paths.desktop_dir, 0o700)?;
        let store = Self { paths, helper };
        store.recover_pending_desktop_rename()?;
        Ok(store)
    }

    pub fn paths(&self) -> &XdgPaths {
        &self.paths
    }

    pub fn list(&self) -> Result<Vec<AppImageRegistration>, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        self.list_locked()
    }

    fn list_locked(&self) -> Result<Vec<AppImageRegistration>, StoreError> {
        let directory = &self.paths.appimage_registration_dir();
        let mut paths = Vec::new();
        for entry in fs::read_dir(directory).map_err(|source| io_error(directory, source))? {
            let entry = entry.map_err(|source| io_error(directory, source))?;
            let path = entry.path();
            if path.extension() == Some(OsStr::new("json")) {
                if paths.len() >= MAX_REGISTRATIONS {
                    return Err(StoreError::RegistrationLimit);
                }
                paths.push(path);
            }
        }
        paths.sort();
        paths
            .iter()
            .map(|path| {
                let metadata =
                    fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(StoreError::UnsafeManagedPath(path.display().to_string()));
                }
                Ok(AppImageRegistration::load_and_migrate(path)?.value)
            })
            .collect()
    }

    pub fn load(&self, id: RegistrationId) -> Result<AppImageRegistration, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        self.load_locked(id)
    }

    fn load_locked(&self, id: RegistrationId) -> Result<AppImageRegistration, StoreError> {
        let path = self.paths.appimage_registration_path(id);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                Ok(AppImageRegistration::load_and_migrate(&path)?.value)
            }
            Ok(_) => Err(StoreError::UnsafeManagedPath(path.display().to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(StoreError::MissingRegistration(id))
            }
            Err(source) => Err(io_error(path, source)),
        }
    }

    pub fn find_by_target(
        &self,
        target: &Path,
    ) -> Result<Option<AppImageRegistration>, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        self.find_by_target_locked(target)
    }

    fn find_by_target_locked(
        &self,
        target: &Path,
    ) -> Result<Option<AppImageRegistration>, StoreError> {
        Ok(self
            .list_locked()?
            .into_iter()
            .find(|registration| registration.target_path == target))
    }

    /// Rename one item in the verified XDG Desktop directory.
    ///
    /// Unregistered items use the ordinary descriptor-relative Desktop
    /// rename. A registered AppImage is renamed together with the one mutable
    /// field in its stable-ID registration projection. That combined case is
    /// serialized and write-ahead journalled so startup can deterministically
    /// finish it after process termination or power loss.
    pub fn rename_desktop_item(
        &self,
        old_name: &OsStr,
        new_name: &OsStr,
    ) -> Result<PathBuf, StoreError> {
        self.rename_desktop_item_impl(old_name, new_name, None)
    }

    fn rename_desktop_item_impl(
        &self,
        old_name: &OsStr,
        new_name: &OsStr,
        fault: Option<DesktopRenameFault>,
    ) -> Result<PathBuf, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        let desktop = DesktopDirectory::open(&self.paths.desktop_dir)?;
        let old_path = desktop.path().join(old_name);
        let new_path = desktop.path().join(new_name);
        if old_name == new_name {
            desktop.consequence(old_name)?;
            return Ok(new_path);
        }

        let registrations = self.list_locked()?;
        let registered = registrations
            .iter()
            .filter(|registration| registration.target_path == old_path)
            .collect::<Vec<_>>();
        if registered.is_empty() {
            desktop.rename(old_name, new_name)?;
            return Ok(new_path);
        }
        if registered.len() != 1 {
            return Err(StoreError::AmbiguousDesktopRename(format!(
                "{} registrations name the source {}",
                registered.len(),
                old_path.display()
            )));
        }
        if registrations
            .iter()
            .any(|registration| registration.target_path == new_path)
        {
            return Err(StoreError::AmbiguousDesktopRename(format!(
                "another registration already names {}",
                new_path.display()
            )));
        }
        let registration = (*registered[0]).clone();
        // Recovery must be able to represent and validate the destination
        // registration before the filesystem is touched. In particular,
        // arbitrary Unix basenames may be non-UTF-8 and a valid 255-byte
        // basename can still make the absolute guest path exceed its schema
        // limit.
        let mut desired_registration = registration.clone();
        desired_registration.target_path = new_path.clone();
        desired_registration.validate()?;
        let expected_file = desktop.observe_regular_file(old_name)?;
        // A normal pre-existing collision is a user-facing validation error,
        // not an interrupted transaction. Recheck under renameat2 after the
        // journal is durable to reject a destination created in the race
        // window without replacing it.
        desktop.require_absent(new_name)?;
        let mut journal = DesktopRenameJournal {
            schema_version: DESKTOP_RENAME_JOURNAL_SCHEMA_VERSION,
            registration_id: registration.id,
            desktop_path: self.paths.desktop_dir.as_os_str().as_bytes().to_vec(),
            old_name: old_name.as_bytes().to_vec(),
            new_name: new_name.as_bytes().to_vec(),
            expected_file,
            phase: DesktopRenamePhase::Prepared,
        };
        journal.validate(&self.paths)?;
        self.write_desktop_rename_journal(&journal)?;
        self.inject_desktop_rename_fault(fault, DesktopRenameFault::PreparedJournal)?;
        #[cfg(test)]
        if fault == Some(DesktopRenameFault::DestinationCollision) {
            fs::write(&new_path, b"raced destination")
                .map_err(|source| io_error(&new_path, source))?;
        }

        if let Err(error) =
            desktop.rename_regular_file_verified(old_name, new_name, &journal.expected_file)
        {
            // If the syscall made no durable change, abort this just-prepared
            // transaction so a normal validation/I/O failure cannot become a
            // surprising rename on the next login. If location is uncertain
            // (including rename success followed by fsync failure), preserve
            // the journal and let recovery decide from the observed inode.
            let source_unchanged =
                observe_optional(&desktop, old_name)? == Some(journal.expected_file.clone());
            let conclusively_not_renamed = matches!(&error, DesktopFileError::Collision(_));
            if source_unchanged
                && (conclusively_not_renamed || observe_optional(&desktop, new_name)?.is_none())
            {
                self.clear_desktop_rename_journal()?;
            }
            return Err(error.into());
        }
        self.inject_desktop_rename_fault(fault, DesktopRenameFault::FileRename)?;
        journal.phase = DesktopRenamePhase::FileRenamed;
        self.write_desktop_rename_journal(&journal)?;
        self.inject_desktop_rename_fault(fault, DesktopRenameFault::FilePhase)?;

        self.update_renamed_registration(&journal)?;
        self.inject_desktop_rename_fault(fault, DesktopRenameFault::RegistrationUpdate)?;
        journal.phase = DesktopRenamePhase::RegistrationUpdated;
        self.write_desktop_rename_journal(&journal)?;
        self.inject_desktop_rename_fault(fault, DesktopRenameFault::RegistrationPhase)?;
        self.clear_desktop_rename_journal()?;
        self.inject_desktop_rename_fault(fault, DesktopRenameFault::JournalClear)?;
        Ok(new_path)
    }

    fn recover_pending_desktop_rename(&self) -> Result<(), StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()
    }

    fn recover_pending_desktop_rename_locked(&self) -> Result<(), StoreError> {
        self.recover_pending_desktop_rename_locked_impl(None)
    }

    fn recover_pending_desktop_rename_locked_impl(
        &self,
        fault: Option<DesktopRenameFault>,
    ) -> Result<(), StoreError> {
        let Some(mut journal) = self.read_desktop_rename_journal()? else {
            return Ok(());
        };
        journal.validate(&self.paths)?;
        self.preflight_pending_desktop_rename(&journal)?;
        let desktop = DesktopDirectory::open(&self.paths.desktop_dir)?;
        let old_name = OsString::from_vec(journal.old_name.clone());
        let new_name = OsString::from_vec(journal.new_name.clone());
        let old = observe_optional(&desktop, &old_name)?;
        let new = observe_optional(&desktop, &new_name)?;
        match (old, new) {
            (Some(old), None) if old == journal.expected_file => {
                desktop.rename_regular_file_verified(
                    &old_name,
                    &new_name,
                    &journal.expected_file,
                )?;
            }
            (None, Some(new)) if new == journal.expected_file => {}
            (old, new) => {
                return Err(StoreError::AmbiguousDesktopRename(format!(
                    "expected device {} inode {} size {} at exactly one of {} or {}; observed old={old:?}, new={new:?}",
                    journal.expected_file.device,
                    journal.expected_file.inode,
                    journal.expected_file.size,
                    self.paths.desktop_dir.join(&old_name).display(),
                    self.paths.desktop_dir.join(&new_name).display(),
                )));
            }
        }
        // Observing the inode at `new` does not prove that the directory entry
        // survived a prior process's failed/missing fsync. Establish that
        // durability before allowing the record or journal to advance.
        self.inject_desktop_rename_fault(fault, DesktopRenameFault::RecoveryDirectorySync)?;
        desktop.sync()?;
        journal.phase = DesktopRenamePhase::FileRenamed;
        self.write_desktop_rename_journal(&journal)?;
        self.update_renamed_registration(&journal)?;
        journal.phase = DesktopRenamePhase::RegistrationUpdated;
        self.write_desktop_rename_journal(&journal)?;
        self.clear_desktop_rename_journal()
    }

    fn preflight_pending_desktop_rename(
        &self,
        journal: &DesktopRenameJournal,
    ) -> Result<(), StoreError> {
        let old = self
            .paths
            .desktop_dir
            .join(OsString::from_vec(journal.old_name.clone()));
        let new = self
            .paths
            .desktop_dir
            .join(OsString::from_vec(journal.new_name.clone()));
        let registrations = self.list_locked().map_err(|error| {
            StoreError::AmbiguousDesktopRename(format!(
                "registrations cannot be enumerated during recovery: {error}"
            ))
        })?;
        let latest = registrations
            .iter()
            .find(|registration| registration.id == journal.registration_id)
            .ok_or_else(|| {
                StoreError::AmbiguousDesktopRename(format!(
                    "registration {} is missing during recovery",
                    journal.registration_id
                ))
            })?;
        if latest.target_path != old && latest.target_path != new {
            return Err(StoreError::AmbiguousDesktopRename(format!(
                "registration {} names {}, expected {} or {}",
                latest.id,
                latest.target_path.display(),
                old.display(),
                new.display()
            )));
        }
        if registrations.iter().any(|registration| {
            registration.id != journal.registration_id
                && (registration.target_path == old || registration.target_path == new)
        }) {
            return Err(StoreError::AmbiguousDesktopRename(format!(
                "another registration names the source or destination of transaction {}",
                journal.registration_id
            )));
        }
        let mut desired = latest.clone();
        desired.target_path = new;
        desired.validate().map_err(|error| {
            StoreError::AmbiguousDesktopRename(format!(
                "rename destination cannot be represented by registration {}: {error}",
                journal.registration_id
            ))
        })
    }

    fn update_renamed_registration(
        &self,
        journal: &DesktopRenameJournal,
    ) -> Result<(), StoreError> {
        let old = self
            .paths
            .desktop_dir
            .join(OsString::from_vec(journal.old_name.clone()));
        let new = self
            .paths
            .desktop_dir
            .join(OsString::from_vec(journal.new_name.clone()));
        if self.list_locked()?.into_iter().any(|registration| {
            registration.id != journal.registration_id && registration.target_path == new
        }) {
            return Err(StoreError::AmbiguousDesktopRename(format!(
                "another registration began naming {} during the rename",
                new.display()
            )));
        }
        let mut latest = self.load_locked(journal.registration_id).map_err(|error| {
            StoreError::AmbiguousDesktopRename(format!(
                "registration {} cannot be loaded during recovery: {error}",
                journal.registration_id
            ))
        })?;
        if latest.target_path == new {
            // A previous atomic replacement may have become visible and then
            // reported a parent-directory fsync failure. Re-saving the exact
            // validated record re-establishes record-directory durability
            // before the recovery journal can be deleted.
            latest.save(&self.paths.appimage_registration_path(latest.id))?;
            return Ok(());
        }
        if latest.target_path != old {
            return Err(StoreError::AmbiguousDesktopRename(format!(
                "registration {} names {}, expected {} or {}",
                latest.id,
                latest.target_path.display(),
                old.display(),
                new.display()
            )));
        }
        latest.target_path = new;
        latest.save(&self.paths.appimage_registration_path(latest.id))?;
        Ok(())
    }

    fn desktop_rename_journal_path(&self) -> PathBuf {
        self.paths.managed_state_dir().join(DESKTOP_RENAME_JOURNAL)
    }

    fn write_desktop_rename_journal(
        &self,
        journal: &DesktopRenameJournal,
    ) -> Result<(), StoreError> {
        journal.validate(&self.paths)?;
        atomic_write_json(&self.desktop_rename_journal_path(), journal)?;
        Ok(())
    }

    fn read_desktop_rename_journal(&self) -> Result<Option<DesktopRenameJournal>, StoreError> {
        let path = self.desktop_rename_journal_path();
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(StoreError::UnsafeManagedPath(path.display().to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error(path, source)),
        }
        let bytes = read_bounded(&path, 64 * 1024)?;
        let journal = serde_json::from_slice(&bytes).map_err(|error| {
            StoreError::AmbiguousDesktopRename(format!(
                "rename journal is malformed and was preserved: {error}"
            ))
        })?;
        Ok(Some(journal))
    }

    fn clear_desktop_rename_journal(&self) -> Result<(), StoreError> {
        remove_managed_file_if_exists(&self.desktop_rename_journal_path())
    }

    fn acquire_desktop_rename_lock(&self) -> Result<DesktopRenameLock, StoreError> {
        DesktopRenameLock::acquire(&self.paths.managed_state_dir())
    }

    fn inject_desktop_rename_fault(
        &self,
        selected: Option<DesktopRenameFault>,
        stage: DesktopRenameFault,
    ) -> Result<(), StoreError> {
        if selected == Some(stage) {
            return Err(StoreError::InjectedDesktopRenameFault(match stage {
                DesktopRenameFault::PreparedJournal => "prepared journal",
                DesktopRenameFault::FileRename => "file rename",
                DesktopRenameFault::FilePhase => "file phase journal",
                DesktopRenameFault::RegistrationUpdate => "registration update",
                DesktopRenameFault::RegistrationPhase => "registration phase journal",
                DesktopRenameFault::JournalClear => "journal deletion",
                #[cfg(test)]
                DesktopRenameFault::DestinationCollision => "destination collision",
                DesktopRenameFault::RecoveryDirectorySync => "recovery Desktop directory sync",
            }));
        }
        Ok(())
    }

    pub fn register(
        &self,
        target: &Path,
        flags: RegistrationFlags,
    ) -> Result<AppImageRegistration, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        self.register_locked(target, flags)
    }

    fn register_locked(
        &self,
        target: &Path,
        flags: RegistrationFlags,
    ) -> Result<AppImageRegistration, StoreError> {
        let validated = validate_appimage(target)?;
        let inspected = validated.inspect_metadata()?;
        if let Some(mut existing) = self
            .list_locked()?
            .into_iter()
            .find(|registration| registration.target_path == target)
        {
            existing.applications_launcher |= flags.applications_launcher;
            existing.desktop_shortcut |= flags.desktop_shortcut;
            apply_inspection(&mut existing, &inspected, target);
            self.save_projected(&existing)?;
            return Ok(existing);
        }
        let registration = AppImageRegistration {
            schema_version: APPIMAGE_REGISTRATION_SCHEMA_VERSION,
            id: RegistrationId::generate(),
            target_path: target.to_path_buf(),
            display_name: inspected.display_name.clone(),
            icon: icon_metadata(&inspected),
            last_observed: Some(inspected.observation),
            applications_launcher: flags.applications_launcher,
            desktop_shortcut: flags.desktop_shortcut,
            created_at_unix_seconds: unix_time(),
            last_successful_launch_unix_seconds: None,
        };
        self.save_projected_with_icon(&registration, inspected.icon.as_ref())?;
        Ok(registration)
    }

    pub fn set_flags(
        &self,
        id: RegistrationId,
        flags: RegistrationFlags,
    ) -> Result<Option<AppImageRegistration>, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        self.set_flags_locked(id, flags)
    }

    fn set_flags_locked(
        &self,
        id: RegistrationId,
        flags: RegistrationFlags,
    ) -> Result<Option<AppImageRegistration>, StoreError> {
        let mut registration = self.load_locked(id)?;
        registration.applications_launcher = flags.applications_launcher;
        registration.desktop_shortcut = flags.desktop_shortcut;
        if !flags.applications_launcher && !flags.desktop_shortcut {
            self.remove_registration_files(&registration)?;
            return Ok(None);
        }
        self.save_projected(&registration)?;
        Ok(Some(registration))
    }

    pub fn add_applications(&self, id: RegistrationId) -> Result<AppImageRegistration, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        let registration = self.load_locked(id)?;
        self.set_flags_locked(
            id,
            RegistrationFlags {
                applications_launcher: true,
                desktop_shortcut: registration.desktop_shortcut,
            },
        )?
        .ok_or(StoreError::MissingRegistration(id))
    }

    pub fn remove_applications(
        &self,
        id: RegistrationId,
    ) -> Result<Option<AppImageRegistration>, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        let registration = self.load_locked(id)?;
        self.set_flags_locked(
            id,
            RegistrationFlags {
                applications_launcher: false,
                desktop_shortcut: registration.desktop_shortcut,
            },
        )
    }

    pub fn add_desktop(&self, id: RegistrationId) -> Result<AppImageRegistration, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        let registration = self.load_locked(id)?;
        self.set_flags_locked(
            id,
            RegistrationFlags {
                applications_launcher: registration.applications_launcher,
                desktop_shortcut: true,
            },
        )?
        .ok_or(StoreError::MissingRegistration(id))
    }

    pub fn remove_desktop(
        &self,
        id: RegistrationId,
    ) -> Result<Option<AppImageRegistration>, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        let registration = self.load_locked(id)?;
        self.set_flags_locked(
            id,
            RegistrationFlags {
                applications_launcher: registration.applications_launcher,
                desktop_shortcut: false,
            },
        )
    }

    pub fn launch(&self, id: RegistrationId) -> Result<LaunchResult, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        let mut registration = self.load_locked(id)?;
        let validated = match validate_appimage(&registration.target_path) {
            Ok(validated) => validated,
            Err(InspectionError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(LaunchResult {
                    status: LaunchStatus::TargetMissing,
                    registration,
                    child: None,
                    diagnostic: Some(source.to_string()),
                });
            }
            Err(error) => {
                return Ok(LaunchResult {
                    status: LaunchStatus::TargetInvalid,
                    registration,
                    child: None,
                    diagnostic: Some(error.to_string()),
                });
            }
        };
        let inspected = validated.inspect_metadata()?;
        apply_inspection(&mut registration, &inspected, validated.path());
        self.save_projected_with_icon(&registration, inspected.icon.as_ref())?;
        let child = crate::launch_validated(&validated).map_err(|error| {
            StoreError::UnsafeManagedPath(format!("launching validated AppImage failed: {error:#}"))
        })?;
        registration.last_successful_launch_unix_seconds = Some(unix_time());
        registration.save(&self.paths.appimage_registration_path(id))?;
        Ok(LaunchResult {
            status: LaunchStatus::Started,
            registration,
            child: Some(child),
            diagnostic: None,
        })
    }

    pub fn preview_relink(
        &self,
        id: RegistrationId,
        candidate_path: &Path,
    ) -> Result<RelinkPreview, StoreError> {
        let current = {
            let _lock = self.acquire_desktop_rename_lock()?;
            self.recover_pending_desktop_rename_locked()?;
            self.load_locked(id)?
        };
        let candidate = validate_appimage(candidate_path)?.inspect_metadata()?;
        let expected = identity_key(&current.display_name);
        let identity_differs = expected != candidate.identity_key;
        Ok(RelinkPreview {
            current,
            candidate,
            candidate_path: candidate_path.to_path_buf(),
            identity_differs,
        })
    }

    pub fn commit_relink(
        &self,
        preview: RelinkPreview,
        accept_different_identity: bool,
    ) -> Result<AppImageRegistration, StoreError> {
        let _lock = self.acquire_desktop_rename_lock()?;
        self.recover_pending_desktop_rename_locked()?;
        if preview.identity_differs && !accept_different_identity {
            return Err(StoreError::IdentityMismatch {
                expected: preview.current.display_name,
                found: preview.candidate.display_name,
            });
        }
        // Reload immediately before committing so a concurrent Settings or
        // shell action cannot be overwritten by a stale chooser result.
        let latest = self.load_locked(preview.current.id)?;
        if latest != preview.current {
            return Err(StoreError::UnsafeManagedPath(
                "registration changed while the relink chooser was open".into(),
            ));
        }
        let mut registration = latest;
        apply_inspection(
            &mut registration,
            &preview.candidate,
            &preview.candidate_path,
        );
        self.save_projected_with_icon(&registration, preview.candidate.icon.as_ref())?;
        Ok(registration)
    }

    pub fn reveal_target(&self, id: RegistrationId) -> Result<(), StoreError> {
        let registration = {
            let _lock = self.acquire_desktop_rename_lock()?;
            self.recover_pending_desktop_rename_locked()?;
            self.load_locked(id)?
        };
        let file = gio::File::for_path(&registration.target_path);
        let uri = file.uri();
        let proxy = gio::DBusProxy::for_bus_sync(
            gio::BusType::Session,
            gio::DBusProxyFlags::DO_NOT_AUTO_START_AT_CONSTRUCTION,
            None,
            "org.freedesktop.FileManager1",
            "/org/freedesktop/FileManager1",
            "org.freedesktop.FileManager1",
            gio::Cancellable::NONE,
        )
        .map_err(|error| StoreError::UnsafeManagedPath(error.to_string()))?;
        let parameters = glib::Variant::from((vec![uri.as_str()], ""));
        proxy
            .call_sync(
                "ShowItems",
                Some(&parameters),
                gio::DBusCallFlags::NONE,
                5_000,
                gio::Cancellable::NONE,
            )
            .map(|_| ())
            .map_err(|error| StoreError::UnsafeManagedPath(error.to_string()))
    }

    fn save_projected(&self, registration: &AppImageRegistration) -> Result<(), StoreError> {
        self.save_projected_with_icon(registration, None)
    }

    fn save_projected_with_icon(
        &self,
        registration: &AppImageRegistration,
        inspected_icon: Option<&crate::inspector::InspectedIcon>,
    ) -> Result<(), StoreError> {
        registration.validate()?;
        let paths = self.projection_paths(registration.id);
        let snapshots = paths
            .iter()
            .map(|path| ManagedFileSnapshot::capture(path))
            .collect::<Result<Vec<_>, _>>()?;
        let operation: Result<(), StoreError> = (|| {
            self.write_icon(registration, inspected_icon)?;
            let entry = GeneratedAppImageDesktopEntry {
                id: registration.id,
                display_name: registration.display_name.clone(),
                helper: self.helper.clone(),
            };
            project_entry(
                &entry,
                &self.paths.managed_appimage_desktop_path(registration.id),
                registration.applications_launcher,
            )?;
            project_entry(
                &entry,
                &self
                    .paths
                    .desktop_dir
                    .join(registration.id.desktop_file_id()),
                registration.desktop_shortcut,
            )?;
            if registration.desktop_shortcut {
                let desktop_entry = self
                    .paths
                    .desktop_dir
                    .join(registration.id.desktop_file_id());
                let metadata = fs::symlink_metadata(&desktop_entry)
                    .map_err(|source| io_error(&desktop_entry, source))?;
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(StoreError::UnsafeManagedPath(
                        desktop_entry.display().to_string(),
                    ));
                }
                fs::set_permissions(&desktop_entry, fs::Permissions::from_mode(0o755))
                    .map_err(|source| io_error(&desktop_entry, source))?;
            }
            registration.save(&self.paths.appimage_registration_path(registration.id))?;
            Ok(())
        })();
        if let Err(operation_error) = operation {
            for (path, snapshot) in paths.iter().zip(snapshots.iter()).rev() {
                if let Err(rollback_error) = snapshot.restore(path) {
                    return Err(StoreError::ProjectionRollback {
                        operation: operation_error.to_string(),
                        rollback: rollback_error.to_string(),
                    });
                }
            }
            return Err(operation_error);
        }
        Ok(())
    }

    fn projection_paths(&self, id: RegistrationId) -> [PathBuf; 5] {
        [
            self.paths.managed_appimage_desktop_path(id),
            self.paths.desktop_dir.join(id.desktop_file_id()),
            self.paths
                .managed_appimage_icon_dir(256)
                .join(format!("{}.png", id.icon_name())),
            self.paths
                .data_home
                .join("icons/hicolor/scalable/apps")
                .join(format!("{}.svg", id.icon_name())),
            self.paths.appimage_registration_path(id),
        ]
    }

    fn write_icon(
        &self,
        registration: &AppImageRegistration,
        inspected: Option<&crate::inspector::InspectedIcon>,
    ) -> Result<(), StoreError> {
        let scalable = self.paths.data_home.join("icons/hicolor/scalable/apps");
        let png = self.paths.managed_appimage_icon_dir(256);
        ensure_real_directory(&scalable, 0o700)?;
        ensure_real_directory(&png, 0o700)?;
        let svg_path = scalable.join(format!("{}.svg", registration.id.icon_name()));
        let png_path = png.join(format!("{}.png", registration.id.icon_name()));
        if let Some(icon) = inspected {
            atomic_write(&png_path, &icon.png_256, 0o600)
                .map_err(buzzardos_desktop_core::desktop_entry::DesktopEntryError::from)?;
            remove_managed_file_if_exists(&svg_path)?;
        } else if matches!(registration.icon, AppImageIcon::BuiltIn) {
            atomic_write(&svg_path, FALLBACK_ICON, 0o600)
                .map_err(buzzardos_desktop_core::desktop_entry::DesktopEntryError::from)?;
            remove_managed_file_if_exists(&png_path)?;
        }
        Ok(())
    }

    fn remove_registration_files(
        &self,
        registration: &AppImageRegistration,
    ) -> Result<(), StoreError> {
        for path in [
            self.paths.managed_appimage_desktop_path(registration.id),
            self.paths
                .desktop_dir
                .join(registration.id.desktop_file_id()),
            self.paths
                .managed_appimage_icon_dir(256)
                .join(format!("{}.png", registration.id.icon_name())),
            self.paths
                .data_home
                .join("icons/hicolor/scalable/apps")
                .join(format!("{}.svg", registration.id.icon_name())),
            self.paths.appimage_registration_path(registration.id),
        ] {
            remove_managed_file_if_exists(&path)?;
        }
        Ok(())
    }
}

impl DesktopRenameJournal {
    fn validate(&self, paths: &XdgPaths) -> Result<(), StoreError> {
        if self.schema_version != DESKTOP_RENAME_JOURNAL_SCHEMA_VERSION {
            return Err(StoreError::AmbiguousDesktopRename(format!(
                "rename journal schema {} is unsupported (current {})",
                self.schema_version, DESKTOP_RENAME_JOURNAL_SCHEMA_VERSION
            )));
        }
        validate_journal_name(&self.old_name)?;
        validate_journal_name(&self.new_name)?;
        if self.old_name == self.new_name {
            return Err(StoreError::AmbiguousDesktopRename(
                "rename journal source and destination are identical".into(),
            ));
        }
        if self.desktop_path != paths.desktop_dir.as_os_str().as_bytes() {
            return Err(StoreError::AmbiguousDesktopRename(format!(
                "rename journal is bound to a different XDG Desktop: {}",
                Path::new(&OsString::from_vec(self.desktop_path.clone())).display()
            )));
        }
        if self.expected_file.device == 0
            || self.expected_file.inode == 0
            || self.expected_file.size == 0
        {
            return Err(StoreError::AmbiguousDesktopRename(
                "rename journal contains an invalid file identity".into(),
            ));
        }
        if !paths.desktop_dir.is_absolute() {
            return Err(StoreError::UnsafeManagedPath(
                paths.desktop_dir.display().to_string(),
            ));
        }
        Ok(())
    }
}

fn validate_journal_name(name: &[u8]) -> Result<(), StoreError> {
    if name.is_empty()
        || name.len() > 255
        || name == b"."
        || name == b".."
        || name.contains(&b'/')
        || name.contains(&0)
    {
        return Err(StoreError::AmbiguousDesktopRename(
            "rename journal contains an invalid Desktop basename".into(),
        ));
    }
    if std::str::from_utf8(name).is_err() {
        return Err(StoreError::AmbiguousDesktopRename(
            "rename journal contains a non-UTF-8 Desktop basename".into(),
        ));
    }
    Ok(())
}

fn observe_optional(
    desktop: &DesktopDirectory,
    name: &OsStr,
) -> Result<Option<FileObservation>, StoreError> {
    match desktop.observe_regular_file(name) {
        Ok(observation) => Ok(Some(observation)),
        Err(DesktopFileError::Missing(_)) => Ok(None),
        Err(error) => Err(StoreError::AmbiguousDesktopRename(format!(
            "cannot safely inspect {} during recovery: {error}",
            desktop.path().join(name).display()
        ))),
    }
}

#[derive(Debug)]
struct DesktopRenameLock {
    file: File,
}

impl DesktopRenameLock {
    fn acquire(state_directory: &Path) -> Result<Self, StoreError> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(state_directory)
            .map_err(|source| io_error(state_directory, source))?;
        let leaf = CString::new(DESKTOP_RENAME_LOCK).expect("fixed lock name has no NUL");
        // SAFETY: the directory descriptor and fixed NUL-terminated basename
        // remain valid. O_NOFOLLOW prevents a mutable lock-file symlink from
        // redirecting this synchronization primitive.
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDWR
                    | libc::O_CREAT
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(io_error(
                state_directory.join(DESKTOP_RENAME_LOCK),
                std::io::Error::last_os_error(),
            ));
        }
        // SAFETY: openat returned a uniquely owned descriptor.
        let file = unsafe { File::from_raw_fd(descriptor) };
        let metadata = file
            .metadata()
            .map_err(|source| io_error(state_directory.join(DESKTOP_RENAME_LOCK), source))?;
        if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(StoreError::UnsafeManagedPath(
                state_directory
                    .join(DESKTOP_RENAME_LOCK)
                    .display()
                    .to_string(),
            ));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| io_error(state_directory.join(DESKTOP_RENAME_LOCK), source))?;
        loop {
            // SAFETY: file owns a live descriptor. LOCK_EX blocks only other
            // Buzzard OS helper/store transactions on this private file.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(io_error(state_directory.join(DESKTOP_RENAME_LOCK), error));
            }
        }
        Ok(Self { file })
    }
}

impl Drop for DesktopRenameLock {
    fn drop(&mut self) {
        // SAFETY: best-effort release of the live descriptor. Closing the file
        // immediately afterwards would release it too; explicit unlock makes
        // the synchronization lifetime obvious.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Debug)]
enum ManagedFileSnapshot {
    Missing,
    Regular { bytes: Vec<u8>, mode: u32 },
}

impl ManagedFileSnapshot {
    fn capture(path: &Path) -> Result<Self, StoreError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                let bytes = read_bounded(path, MAX_PROJECTION_BACKUP_BYTES)
                    .map_err(buzzardos_desktop_core::desktop_entry::DesktopEntryError::from)?;
                Ok(Self::Regular {
                    bytes,
                    mode: metadata.permissions().mode() & 0o777,
                })
            }
            Ok(_) => Err(StoreError::UnsafeManagedPath(path.display().to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::Missing),
            Err(source) => Err(io_error(path, source)),
        }
    }

    fn restore(&self, path: &Path) -> Result<(), StoreError> {
        match self {
            Self::Missing => remove_managed_file_if_exists(path),
            Self::Regular { bytes, mode } => atomic_write(path, bytes, *mode)
                .map_err(buzzardos_desktop_core::desktop_entry::DesktopEntryError::from)
                .map_err(StoreError::from),
        }
    }
}

fn apply_inspection(
    registration: &mut AppImageRegistration,
    inspected: &InspectedAppImage,
    target: &Path,
) {
    registration.target_path = target.to_path_buf();
    registration
        .display_name
        .clone_from(&inspected.display_name);
    registration.icon = icon_metadata(inspected);
    registration.last_observed = Some(inspected.observation.clone());
}

fn icon_metadata(inspected: &InspectedAppImage) -> AppImageIcon {
    inspected
        .icon
        .as_ref()
        .map_or(AppImageIcon::BuiltIn, |icon| AppImageIcon::Extracted {
            source_name: icon.source_name.clone(),
            content_sha256: icon.content_sha256.clone(),
        })
}

fn identity_key(name: &str) -> String {
    name.chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
        .collect()
}

fn project_entry(
    entry: &GeneratedAppImageDesktopEntry,
    path: &Path,
    enabled: bool,
) -> Result<(), StoreError> {
    if enabled {
        entry.write(path)?;
    } else {
        remove_managed_file_if_exists(path)?;
    }
    Ok(())
}

fn remove_managed_file_if_exists(path: &Path) -> Result<(), StoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| StoreError::UnsafeManagedPath(path.display().to_string()))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| StoreError::UnsafeManagedPath(path.display().to_string()))?;
    let leaf = CString::new(leaf.as_bytes())
        .map_err(|_| StoreError::UnsafeManagedPath(path.display().to_string()))?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(parent)
        .map_err(|source| io_error(parent, source))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: descriptor, leaf and output storage are valid.
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            leaf.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(io_error(path, error))
        };
    }
    // SAFETY: successful fstatat initialized stat.
    if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(StoreError::UnsafeManagedPath(path.display().to_string()));
    }
    // SAFETY: descriptor and leaf remain valid; unlinkat never follows leaf.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), leaf.as_ptr(), 0) } != 0 {
        return Err(io_error(path, std::io::Error::last_os_error()));
    }
    directory
        .sync_all()
        .map_err(|source| io_error(parent, source))
}

fn ensure_real_directory(path: &Path, mode: u32) -> Result<(), StoreError> {
    fs::create_dir_all(path).map_err(|source| io_error(path, source))?;
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StoreError::UnsafeManagedPath(path.display().to_string()));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|source| io_error(path, source))
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspector::InspectedIcon;
    use std::collections::BTreeMap;
    use std::os::unix::ffi::OsStrExt;
    use std::sync::{Arc, Barrier};

    fn paths(root: &Path) -> XdgPaths {
        XdgPaths::from_bases(
            root.join("home"),
            root.join("config"),
            root.join("data"),
            root.join("state"),
            vec![root.join("system-data")],
            root.join("home/Desktop"),
        )
        .unwrap()
    }

    fn store(root: &Path) -> RegistrationStore {
        for path in [
            root.join("home"),
            root.join("config"),
            root.join("data"),
            root.join("state"),
            root.join("system-data"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        RegistrationStore::new(paths(root), PathBuf::from(HELPER_EXECUTABLE)).unwrap()
    }

    fn registration(id: RegistrationId, target: PathBuf) -> AppImageRegistration {
        AppImageRegistration {
            schema_version: APPIMAGE_REGISTRATION_SCHEMA_VERSION,
            id,
            target_path: target,
            display_name: "Fixture".into(),
            icon: AppImageIcon::BuiltIn,
            last_observed: None,
            applications_launcher: true,
            desktop_shortcut: true,
            created_at_unix_seconds: 1,
            last_successful_launch_unix_seconds: None,
        }
    }

    #[test]
    fn projection_uses_only_opaque_id_and_never_embeds_target() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let id = RegistrationId::generate();
        let target = PathBuf::from("/shared/odd name ' 100%\n日本語.AppImage");
        let record = registration(id, target.clone());
        store.save_projected(&record).unwrap();
        let application =
            fs::read_to_string(store.paths.managed_appimage_desktop_path(id)).unwrap();
        let desktop =
            fs::read_to_string(store.paths.desktop_dir.join(id.desktop_file_id())).unwrap();
        for contents in [&application, &desktop] {
            assert!(contents.contains(&format!("launch {id}")));
            assert!(!contents.contains(target.to_str().unwrap()));
            assert!(!contents.contains("sh -c"));
        }
        assert_ne!(
            fs::metadata(store.paths.desktop_dir.join(id.desktop_file_id()))
                .unwrap()
                .permissions()
                .mode()
                & 0o100,
            0,
            "a generated desktop shortcut is immediately trusted"
        );
        assert_eq!(store.load(id).unwrap(), record);
    }

    #[test]
    fn repeated_projection_is_idempotent_and_flag_removal_keeps_target() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let id = RegistrationId::generate();
        let target = temp.path().join("user-owned.AppImage");
        fs::write(&target, b"user-owned").unwrap();
        let record = registration(id, target.clone());
        store.save_projected(&record).unwrap();
        store.save_projected(&record).unwrap();
        let remaining = store.remove_applications(id).unwrap().unwrap();
        assert!(!remaining.applications_launcher);
        assert!(remaining.desktop_shortcut);
        assert!(target.exists());
        assert!(!store.paths.managed_appimage_desktop_path(id).exists());
        assert!(store.paths.desktop_dir.join(id.desktop_file_id()).exists());
        assert!(store.remove_desktop(id).unwrap().is_none());
        assert!(target.exists());
        assert!(matches!(
            store.load(id),
            Err(StoreError::MissingRegistration(_))
        ));
    }

    #[test]
    fn failed_projection_restores_every_preexisting_managed_artifact() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let id = RegistrationId::generate();
        let record = registration(id, PathBuf::from("/shared/fixture.AppImage"));
        fs::set_permissions(&store.paths.desktop_dir, fs::Permissions::from_mode(0o500)).unwrap();
        let result = store.save_projected(&record);
        fs::set_permissions(&store.paths.desktop_dir, fs::Permissions::from_mode(0o700)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert!(result.is_err());
            assert!(!store.paths.managed_appimage_desktop_path(id).exists());
            assert!(!store.paths.appimage_registration_path(id).exists());
            assert!(
                !store
                    .paths
                    .data_home
                    .join("icons/hicolor/scalable/apps")
                    .join(format!("{}.svg", id.icon_name()))
                    .exists()
            );
        }
    }

    #[test]
    fn relink_preserves_id_and_launcher_names_and_detects_identity_change() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let id = RegistrationId::generate();
        let current = registration(id, PathBuf::from("/shared/old.AppImage"));
        store.save_projected(&current).unwrap();
        let preview = RelinkPreview {
            current: current.clone(),
            candidate: InspectedAppImage {
                display_name: "Different".into(),
                identity_key: "different".into(),
                observation: buzzardos_desktop_core::FileObservation {
                    device: 1,
                    inode: 2,
                    size: 3,
                },
                squashfs_offset: 4096,
                icon: Some(InspectedIcon {
                    source_name: "icon.png".into(),
                    content_sha256: "a".repeat(64),
                    png_256: vec![1, 2, 3],
                }),
            },
            candidate_path: PathBuf::from("/shared/new.AppImage"),
            identity_differs: true,
        };
        assert!(matches!(
            store.commit_relink(preview.clone(), false),
            Err(StoreError::IdentityMismatch { .. })
        ));
        assert_eq!(store.load(id).unwrap(), current);
        let changed = store.commit_relink(preview, true).unwrap();
        assert_eq!(changed.id, id);
        assert_eq!(changed.target_path, PathBuf::from("/shared/new.AppImage"));
        assert!(store.paths.managed_appimage_desktop_path(id).exists());
        assert!(store.paths.desktop_dir.join(id.desktop_file_id()).exists());
    }

    #[test]
    fn record_listing_is_bounded_and_sorted() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let mut expected = BTreeMap::new();
        for name in ["Zulu", "Alpha"] {
            let id = RegistrationId::generate();
            let mut record = registration(id, PathBuf::from(format!("/shared/{name}.AppImage")));
            record.display_name = name.into();
            store.save_projected(&record).unwrap();
            expected.insert(id.to_string(), name.to_owned());
        }
        let listed = store.list().unwrap();
        let actual: BTreeMap<_, _> = listed
            .into_iter()
            .map(|record| (record.id.to_string(), record.display_name))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn registered_desktop_rename_recovers_after_every_durable_phase() {
        for fault in [
            DesktopRenameFault::PreparedJournal,
            DesktopRenameFault::FileRename,
            DesktopRenameFault::FilePhase,
            DesktopRenameFault::RegistrationUpdate,
            DesktopRenameFault::RegistrationPhase,
            DesktopRenameFault::JournalClear,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store = store(temp.path());
            let old = store.paths.desktop_dir.join("Old.AppImage");
            let new = store.paths.desktop_dir.join("New.AppImage");
            fs::write(&old, format!("registered fixture for {fault:?}")).unwrap();
            fs::set_permissions(&old, fs::Permissions::from_mode(0o751)).unwrap();
            let original_bytes = fs::read(&old).unwrap();
            let original_metadata = fs::metadata(&old).unwrap();
            let id = RegistrationId::generate();
            let record = registration(id, old.clone());
            store.save_projected(&record).unwrap();
            let stable_application =
                fs::read(store.paths.managed_appimage_desktop_path(id)).unwrap();
            let stable_shortcut =
                fs::read(store.paths.desktop_dir.join(id.desktop_file_id())).unwrap();
            let stable_icon = fs::read(
                store
                    .paths
                    .data_home
                    .join("icons/hicolor/scalable/apps")
                    .join(format!("{}.svg", id.icon_name())),
            )
            .unwrap();

            assert!(matches!(
                store.rename_desktop_item_impl(
                    OsStr::new("Old.AppImage"),
                    OsStr::new("New.AppImage"),
                    Some(fault),
                ),
                Err(StoreError::InjectedDesktopRenameFault(_))
            ));

            // Constructing any later store instance is the recovery boundary.
            let recovered = self::store(temp.path());
            assert!(!old.exists());
            assert!(new.exists());
            assert_eq!(fs::read(&new).unwrap(), original_bytes);
            let recovered_metadata = fs::metadata(&new).unwrap();
            assert_eq!(recovered_metadata.dev(), original_metadata.dev());
            assert_eq!(recovered_metadata.ino(), original_metadata.ino());
            assert_eq!(recovered_metadata.uid(), original_metadata.uid());
            assert_eq!(recovered_metadata.gid(), original_metadata.gid());
            assert_eq!(
                recovered_metadata.permissions().mode() & 0o777,
                original_metadata.permissions().mode() & 0o777
            );
            let changed = recovered.load(id).unwrap();
            let mut expected = record.clone();
            expected.target_path = new.clone();
            assert_eq!(changed, expected);
            assert_eq!(
                fs::read(recovered.paths.managed_appimage_desktop_path(id)).unwrap(),
                stable_application
            );
            assert_eq!(
                fs::read(recovered.paths.desktop_dir.join(id.desktop_file_id())).unwrap(),
                stable_shortcut
            );
            assert_eq!(
                fs::read(
                    recovered
                        .paths
                        .data_home
                        .join("icons/hicolor/scalable/apps")
                        .join(format!("{}.svg", id.icon_name())),
                )
                .unwrap(),
                stable_icon
            );
            assert!(!recovered.desktop_rename_journal_path().exists());
        }
    }

    #[test]
    fn recovery_syncs_an_already_renamed_desktop_before_advancing_record() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let old = store.paths.desktop_dir.join("Old.AppImage");
        let new = store.paths.desktop_dir.join("New.AppImage");
        fs::write(&old, b"registered target").unwrap();
        let id = RegistrationId::generate();
        let original = registration(id, old.clone());
        store.save_projected(&original).unwrap();
        assert!(matches!(
            store.rename_desktop_item_impl(
                OsStr::new("Old.AppImage"),
                OsStr::new("New.AppImage"),
                Some(DesktopRenameFault::PreparedJournal),
            ),
            Err(StoreError::InjectedDesktopRenameFault(_))
        ));
        // State-equivalent to termination after renameat2 and before the
        // Desktop directory fsync.
        fs::rename(&old, &new).unwrap();
        {
            let _lock = store.acquire_desktop_rename_lock().unwrap();
            assert!(matches!(
                store.recover_pending_desktop_rename_locked_impl(Some(
                    DesktopRenameFault::RecoveryDirectorySync
                )),
                Err(StoreError::InjectedDesktopRenameFault(_))
            ));
        }
        assert!(store.desktop_rename_journal_path().exists());
        assert_eq!(
            AppImageRegistration::load(&store.paths.appimage_registration_path(id))
                .unwrap()
                .value,
            original
        );
        assert_eq!(fs::read(&new).unwrap(), b"registered target");

        let recovered = self::store(temp.path());
        assert_eq!(recovered.load(id).unwrap().target_path, new);
        assert!(!recovered.desktop_rename_journal_path().exists());
    }

    #[test]
    fn recovery_redurably_saves_an_already_updated_registration() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let old = store.paths.desktop_dir.join("Old.AppImage");
        let new = store.paths.desktop_dir.join("New.AppImage");
        fs::write(&old, b"registered target").unwrap();
        let id = RegistrationId::generate();
        store
            .save_projected(&registration(id, old.clone()))
            .unwrap();
        assert!(matches!(
            store.rename_desktop_item_impl(
                OsStr::new("Old.AppImage"),
                OsStr::new("New.AppImage"),
                Some(DesktopRenameFault::RegistrationUpdate),
            ),
            Err(StoreError::InjectedDesktopRenameFault(_))
        ));
        let record_path = store.paths.appimage_registration_path(id);
        assert_eq!(
            AppImageRegistration::load(&record_path)
                .unwrap()
                .value
                .target_path,
            new
        );
        let visible_inode = fs::metadata(&record_path).unwrap().ino();

        let recovered = self::store(temp.path());
        assert_eq!(recovered.load(id).unwrap().target_path, new);
        assert_ne!(fs::metadata(&record_path).unwrap().ino(), visible_inode);
        assert!(!recovered.desktop_rename_journal_path().exists());
    }

    #[test]
    fn concurrent_registered_renames_serialize_without_losing_the_target() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let old = store.paths.desktop_dir.join("Old.AppImage");
        fs::write(&old, b"registered fixture").unwrap();
        let id = RegistrationId::generate();
        store
            .save_projected(&registration(id, old.clone()))
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let left_store = store.clone();
        let left_barrier = barrier.clone();
        let left = std::thread::spawn(move || {
            left_barrier.wait();
            left_store.rename_desktop_item(OsStr::new("Old.AppImage"), OsStr::new("Left.AppImage"))
        });
        let right_store = store.clone();
        let right_barrier = barrier.clone();
        let right = std::thread::spawn(move || {
            right_barrier.wait();
            right_store
                .rename_desktop_item(OsStr::new("Old.AppImage"), OsStr::new("Right.AppImage"))
        });
        barrier.wait();
        let outcomes = [left.join().unwrap(), right.join().unwrap()];
        assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(outcomes.iter().filter(|result| result.is_err()).count(), 1);
        let reopened = self::store(temp.path());
        let record = reopened.load(id).unwrap();
        assert!(record.target_path.exists());
        assert!(
            record.target_path.ends_with("Left.AppImage")
                || record.target_path.ends_with("Right.AppImage")
        );
        assert!(!old.exists());
        assert!(!reopened.desktop_rename_journal_path().exists());
    }

    #[test]
    fn concurrent_projection_change_and_rename_preserve_both_updates() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let old = store.paths.desktop_dir.join("Old.AppImage");
        fs::write(&old, b"registered fixture").unwrap();
        let id = RegistrationId::generate();
        store
            .save_projected(&registration(id, old.clone()))
            .unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let rename_store = store.clone();
        let rename_barrier = barrier.clone();
        let rename = std::thread::spawn(move || {
            rename_barrier.wait();
            rename_store.rename_desktop_item(OsStr::new("Old.AppImage"), OsStr::new("New.AppImage"))
        });
        let flags_store = store.clone();
        let flags_barrier = barrier.clone();
        let flags = std::thread::spawn(move || {
            flags_barrier.wait();
            flags_store.remove_applications(id)
        });
        barrier.wait();
        rename.join().unwrap().unwrap();
        flags.join().unwrap().unwrap();

        let reopened = self::store(temp.path());
        let record = reopened.load(id).unwrap();
        assert_eq!(
            record.target_path,
            reopened.paths.desktop_dir.join("New.AppImage")
        );
        assert!(!record.applications_launcher);
        assert!(record.desktop_shortcut);
        assert!(record.target_path.exists());
        assert!(!reopened.paths.managed_appimage_desktop_path(id).exists());
        assert!(
            reopened
                .paths
                .desktop_dir
                .join(id.desktop_file_id())
                .exists()
        );
        assert!(!reopened.desktop_rename_journal_path().exists());
    }

    #[test]
    fn ordinary_destination_collision_never_starts_a_journal_or_touches_files() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let old = store.paths.desktop_dir.join("Old.AppImage");
        let new = store.paths.desktop_dir.join("New.AppImage");
        fs::write(&old, b"registered target").unwrap();
        fs::write(&new, b"existing destination").unwrap();
        let id = RegistrationId::generate();
        let original = registration(id, old.clone());
        store.save_projected(&original).unwrap();

        assert!(matches!(
            store.rename_desktop_item(OsStr::new("Old.AppImage"), OsStr::new("New.AppImage")),
            Err(StoreError::DesktopFile(DesktopFileError::Collision(_)))
        ));
        assert_eq!(fs::read(&old).unwrap(), b"registered target");
        assert_eq!(fs::read(&new).unwrap(), b"existing destination");
        assert_eq!(store.load(id).unwrap(), original);
        assert!(!store.desktop_rename_journal_path().exists());
    }

    #[test]
    fn destination_created_after_prepared_journal_aborts_without_wedging_startup() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let old = store.paths.desktop_dir.join("Old.AppImage");
        let new = store.paths.desktop_dir.join("New.AppImage");
        fs::write(&old, b"registered target").unwrap();
        let id = RegistrationId::generate();
        let original = registration(id, old.clone());
        store.save_projected(&original).unwrap();

        assert!(matches!(
            store.rename_desktop_item_impl(
                OsStr::new("Old.AppImage"),
                OsStr::new("New.AppImage"),
                Some(DesktopRenameFault::DestinationCollision),
            ),
            Err(StoreError::DesktopFile(DesktopFileError::Collision(_)))
        ));
        assert_eq!(fs::read(&old).unwrap(), b"registered target");
        assert_eq!(fs::read(&new).unwrap(), b"raced destination");
        assert_eq!(store.load(id).unwrap(), original);
        assert!(!store.desktop_rename_journal_path().exists());
        assert!(
            RegistrationStore::new(paths(temp.path()), PathBuf::from(HELPER_EXECUTABLE)).is_ok()
        );
    }

    #[test]
    fn registered_rename_rejects_non_utf8_destination_before_any_durable_change() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let old = store.paths.desktop_dir.join("Old.AppImage");
        fs::write(&old, b"registered target").unwrap();
        let id = RegistrationId::generate();
        let original = registration(id, old.clone());
        store.save_projected(&original).unwrap();
        let invalid = OsStr::from_bytes(b"New-\xff.AppImage");

        assert!(matches!(
            store.rename_desktop_item(OsStr::new("Old.AppImage"), invalid),
            Err(StoreError::Registration(
                buzzardos_desktop_core::appimage::RegistrationError::InvalidTargetPath(_)
            ))
        ));
        assert_eq!(fs::read(&old).unwrap(), b"registered target");
        assert!(!store.paths.desktop_dir.join(invalid).exists());
        assert_eq!(store.load(id).unwrap(), original);
        assert!(!store.desktop_rename_journal_path().exists());
    }

    #[test]
    fn registered_rename_rejects_overlong_absolute_target_before_filesystem_change() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for path in [
            root.join("home"),
            root.join("config"),
            root.join("data"),
            root.join("state"),
            root.join("system-data"),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let mut desktop = root.join("deep-desktop");
        while desktop.as_os_str().as_bytes().len() + 182 < 3_850 {
            desktop.push("d".repeat(180));
        }
        let remaining = 3_850usize
            .checked_sub(desktop.as_os_str().as_bytes().len() + 1)
            .unwrap();
        assert!((1..=255).contains(&remaining));
        desktop.push("e".repeat(remaining));
        assert_eq!(desktop.as_os_str().as_bytes().len(), 3_850);
        fs::create_dir_all(&desktop).unwrap();
        let custom_paths = XdgPaths::from_bases(
            root.join("home"),
            root.join("config"),
            root.join("data"),
            root.join("state"),
            vec![root.join("system-data")],
            desktop.clone(),
        )
        .unwrap();
        let store = RegistrationStore::new(custom_paths, PathBuf::from(HELPER_EXECUTABLE)).unwrap();
        let old = desktop.join("Old.AppImage");
        fs::write(&old, b"registered target").unwrap();
        let id = RegistrationId::generate();
        let original = registration(id, old.clone());
        store.save_projected(&original).unwrap();
        let long_name = OsString::from(format!("{}.AppImage", "n".repeat(240)));
        assert!(desktop.join(&long_name).as_os_str().as_bytes().len() > 4_096);

        assert!(matches!(
            store.rename_desktop_item(OsStr::new("Old.AppImage"), &long_name),
            Err(StoreError::Registration(
                buzzardos_desktop_core::appimage::RegistrationError::InvalidTargetPath(_)
            ))
        ));
        assert_eq!(fs::read(&old).unwrap(), b"registered target");
        assert_eq!(store.load(id).unwrap(), original);
        assert!(!store.desktop_rename_journal_path().exists());
    }

    #[test]
    fn duplicate_source_registrations_fail_closed_before_rename() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let old = store.paths.desktop_dir.join("Old.AppImage");
        let new = store.paths.desktop_dir.join("New.AppImage");
        fs::write(&old, b"registered target").unwrap();
        let first = registration(RegistrationId::generate(), old.clone());
        store.save_projected(&first).unwrap();
        let second = registration(RegistrationId::generate(), old.clone());
        second
            .save(&store.paths.appimage_registration_path(second.id))
            .unwrap();

        assert!(matches!(
            store.rename_desktop_item(OsStr::new("Old.AppImage"), OsStr::new("New.AppImage")),
            Err(StoreError::AmbiguousDesktopRename(_))
        ));
        assert_eq!(fs::read(&old).unwrap(), b"registered target");
        assert!(!new.exists());
        assert_eq!(store.load(first.id).unwrap(), first);
        assert_eq!(store.load(second.id).unwrap(), second);
        assert!(!store.desktop_rename_journal_path().exists());
    }

    #[test]
    fn recovery_rejects_a_replaced_source_without_deleting_either_file() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let old = store.paths.desktop_dir.join("Old.AppImage");
        let retained = store.paths.desktop_dir.join("Retained.AppImage");
        fs::write(&old, b"original registered target").unwrap();
        let id = RegistrationId::generate();
        let original = registration(id, old.clone());
        store.save_projected(&original).unwrap();
        assert!(matches!(
            store.rename_desktop_item_impl(
                OsStr::new("Old.AppImage"),
                OsStr::new("New.AppImage"),
                Some(DesktopRenameFault::PreparedJournal),
            ),
            Err(StoreError::InjectedDesktopRenameFault(_))
        ));
        fs::rename(&old, &retained).unwrap();
        fs::write(&old, b"hostile replacement").unwrap();

        assert!(matches!(
            RegistrationStore::new(paths(temp.path()), PathBuf::from(HELPER_EXECUTABLE)),
            Err(StoreError::AmbiguousDesktopRename(_))
        ));
        assert_eq!(fs::read(&retained).unwrap(), b"original registered target");
        assert_eq!(fs::read(&old).unwrap(), b"hostile replacement");
        assert!(!store.paths.desktop_dir.join("New.AppImage").exists());
        assert_eq!(
            AppImageRegistration::load(&store.paths.appimage_registration_path(id))
                .unwrap()
                .value,
            original
        );
        assert!(store.desktop_rename_journal_path().exists());
    }

    #[test]
    fn malformed_or_symlinked_journal_is_preserved_and_blocks_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let store = store(temp.path());
        let journal = store.desktop_rename_journal_path();
        fs::write(&journal, b"{\"unknown\":true}\n").unwrap();
        assert!(matches!(
            RegistrationStore::new(paths(temp.path()), PathBuf::from(HELPER_EXECUTABLE)),
            Err(StoreError::AmbiguousDesktopRename(_))
        ));
        assert_eq!(fs::read(&journal).unwrap(), b"{\"unknown\":true}\n");

        fs::remove_file(&journal).unwrap();
        let victim = temp.path().join("victim");
        fs::write(&victim, b"do not touch").unwrap();
        std::os::unix::fs::symlink(&victim, &journal).unwrap();
        assert!(matches!(
            RegistrationStore::new(paths(temp.path()), PathBuf::from(HELPER_EXECUTABLE)),
            Err(StoreError::UnsafeManagedPath(_))
        ));
        assert_eq!(fs::read(&victim).unwrap(), b"do not touch");
    }
}
