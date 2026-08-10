// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::persistence::{
    LoadOutcome, MAX_MANAGED_JSON_BYTES, PersistenceError, atomic_write_json, read_bounded,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use thiserror::Error;
use uuid::{Uuid, Version};

pub const APPIMAGE_REGISTRATION_SCHEMA_VERSION: u32 = 1;
const MAX_GUEST_PATH_BYTES: usize = 4096;
const MAX_DISPLAY_NAME_BYTES: usize = 512;
const MAX_ICON_NAME_BYTES: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RegistrationId(Uuid);

impl RegistrationId {
    pub fn generate() -> Self {
        Self(Uuid::new_v4())
    }

    pub fn desktop_file_id(self) -> String {
        format!("wildbuzzard-appimage-{}.desktop", self.0)
    }

    pub fn icon_name(self) -> String {
        format!("wildbuzzard-appimage-{}", self.0)
    }

    pub fn registration_filename(self) -> String {
        format!("{}.json", self.0)
    }
}

impl fmt::Display for RegistrationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for RegistrationId {
    type Err = RegistrationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(value)
            .map_err(|_| RegistrationError::InvalidId("ID is not a UUID".into()))?;
        if uuid.get_version() != Some(Version::Random) || uuid.to_string() != value {
            return Err(RegistrationError::InvalidId(
                "ID must be a canonical lowercase random UUID".into(),
            ));
        }
        Ok(Self(uuid))
    }
}

impl Serialize for RegistrationId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for RegistrationId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppImageIcon {
    BuiltIn,
    Extracted {
        source_name: String,
        content_sha256: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileObservation {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppImageRegistration {
    pub schema_version: u32,
    pub id: RegistrationId,
    pub target_path: PathBuf,
    pub display_name: String,
    pub icon: AppImageIcon,
    pub last_observed: Option<FileObservation>,
    pub applications_launcher: bool,
    pub desktop_shortcut: bool,
    pub created_at_unix_seconds: u64,
    pub last_successful_launch_unix_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistrationV0 {
    schema_version: u32,
    id: RegistrationId,
    target_path: PathBuf,
    display_name: String,
    applications_launcher: bool,
    desktop_shortcut: bool,
    created_at_unix_seconds: u64,
}

#[derive(Debug, Error)]
pub enum RegistrationError {
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error("AppImage registration document must be a JSON object")]
    NotObject,
    #[error("AppImage registration is missing integer schema_version")]
    MissingSchemaVersion,
    #[error(
        "AppImage registration schema {found} is newer than supported schema {current}; the file was preserved"
    )]
    NewerSchema { found: u32, current: u32 },
    #[error("AppImage registration schema {found} cannot be migrated to schema {current}")]
    UnsupportedOlderSchema { found: u32, current: u32 },
    #[error("AppImage registration schema has unexpected fields; required fields are: {expected}")]
    UnexpectedFields { expected: String },
    #[error("invalid AppImage registration ID: {0}")]
    InvalidId(String),
    #[error("invalid AppImage target path: {0}")]
    InvalidTargetPath(String),
    #[error("invalid AppImage display name: {0}")]
    InvalidDisplayName(String),
    #[error("invalid AppImage icon metadata: {0}")]
    InvalidIcon(String),
    #[error("invalid AppImage registration timestamp: {0}")]
    InvalidTimestamp(String),
    #[error("AppImage registration JSON does not match schema: {0}")]
    Schema(#[from] serde_json::Error),
}

impl AppImageRegistration {
    pub fn validate(&self) -> Result<(), RegistrationError> {
        if self.schema_version != APPIMAGE_REGISTRATION_SCHEMA_VERSION {
            return Err(
                if self.schema_version > APPIMAGE_REGISTRATION_SCHEMA_VERSION {
                    RegistrationError::NewerSchema {
                        found: self.schema_version,
                        current: APPIMAGE_REGISTRATION_SCHEMA_VERSION,
                    }
                } else {
                    RegistrationError::UnsupportedOlderSchema {
                        found: self.schema_version,
                        current: APPIMAGE_REGISTRATION_SCHEMA_VERSION,
                    }
                },
            );
        }
        validate_target_path(&self.target_path)?;
        validate_display_name(&self.display_name)?;
        validate_icon(&self.icon)?;
        if self.created_at_unix_seconds == 0 {
            return Err(RegistrationError::InvalidTimestamp(
                "creation time must be a positive Unix timestamp".into(),
            ));
        }
        if self
            .last_successful_launch_unix_seconds
            .is_some_and(|time| time == 0)
        {
            return Err(RegistrationError::InvalidTimestamp(
                "last successful launch time must be positive when present".into(),
            ));
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<LoadOutcome<Self>, RegistrationError> {
        let bytes = read_bounded(path, MAX_MANAGED_JSON_BYTES)?;
        let value: Value = serde_json::from_slice(&bytes)?;
        let object = value.as_object().ok_or(RegistrationError::NotObject)?;
        let version = schema_version(object)?;
        let outcome = match version {
            APPIMAGE_REGISTRATION_SCHEMA_VERSION => {
                exact_keys(
                    object,
                    &[
                        "schema_version",
                        "id",
                        "target_path",
                        "display_name",
                        "icon",
                        "last_observed",
                        "applications_launcher",
                        "desktop_shortcut",
                        "created_at_unix_seconds",
                        "last_successful_launch_unix_seconds",
                    ],
                )?;
                exact_icon_keys(&value)?;
                if let Some(observation) =
                    value.get("last_observed").filter(|value| !value.is_null())
                {
                    let observation = observation.as_object().ok_or_else(|| {
                        RegistrationError::InvalidTargetPath(
                            "last_observed must be null or an object".into(),
                        )
                    })?;
                    exact_keys(observation, &["device", "inode", "size"])?;
                }
                LoadOutcome {
                    value: serde_json::from_value(value)?,
                    migrated_from: None,
                }
            }
            0 => {
                exact_keys(
                    object,
                    &[
                        "schema_version",
                        "id",
                        "target_path",
                        "display_name",
                        "applications_launcher",
                        "desktop_shortcut",
                        "created_at_unix_seconds",
                    ],
                )?;
                let old: RegistrationV0 = serde_json::from_value(value)?;
                if old.schema_version != 0 {
                    return Err(RegistrationError::UnsupportedOlderSchema {
                        found: old.schema_version,
                        current: APPIMAGE_REGISTRATION_SCHEMA_VERSION,
                    });
                }
                LoadOutcome {
                    value: Self {
                        schema_version: APPIMAGE_REGISTRATION_SCHEMA_VERSION,
                        id: old.id,
                        target_path: old.target_path,
                        display_name: old.display_name,
                        icon: AppImageIcon::BuiltIn,
                        last_observed: None,
                        applications_launcher: old.applications_launcher,
                        desktop_shortcut: old.desktop_shortcut,
                        created_at_unix_seconds: old.created_at_unix_seconds,
                        last_successful_launch_unix_seconds: None,
                    },
                    migrated_from: Some(0),
                }
            }
            found if found > APPIMAGE_REGISTRATION_SCHEMA_VERSION => {
                return Err(RegistrationError::NewerSchema {
                    found,
                    current: APPIMAGE_REGISTRATION_SCHEMA_VERSION,
                });
            }
            found => {
                return Err(RegistrationError::UnsupportedOlderSchema {
                    found,
                    current: APPIMAGE_REGISTRATION_SCHEMA_VERSION,
                });
            }
        };
        outcome.value.validate()?;
        ensure_record_filename(path, outcome.value.id)?;
        Ok(outcome)
    }

    pub fn load_and_migrate(path: &Path) -> Result<LoadOutcome<Self>, RegistrationError> {
        let outcome = Self::load(path)?;
        if outcome.migrated_from.is_some() {
            outcome.value.save(path)?;
        }
        Ok(outcome)
    }

    pub fn save(&self, path: &Path) -> Result<(), RegistrationError> {
        self.validate()?;
        let expected = self.id.registration_filename();
        if path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str()) {
            return Err(RegistrationError::InvalidId(format!(
                "record for {} must be stored as {expected}",
                self.id
            )));
        }
        atomic_write_json(path, self)?;
        Ok(())
    }
}

fn validate_target_path(path: &Path) -> Result<(), RegistrationError> {
    if !path.is_absolute() || path == Path::new("/") {
        return Err(RegistrationError::InvalidTargetPath(
            "path must be an absolute guest file path".into(),
        ));
    }
    if path.as_os_str().as_bytes().len() > MAX_GUEST_PATH_BYTES {
        return Err(RegistrationError::InvalidTargetPath(format!(
            "path exceeds {MAX_GUEST_PATH_BYTES} bytes"
        )));
    }
    if path.as_os_str().as_bytes()[1..]
        .split(|byte| *byte == b'/')
        .any(|component| component.is_empty() || component == b"." || component == b"..")
    {
        return Err(RegistrationError::InvalidTargetPath(
            "path must be lexically normalized and contain no empty, dot, or dot-dot components"
                .into(),
        ));
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::CurDir | Component::Prefix(_)
        )
    }) {
        return Err(RegistrationError::InvalidTargetPath(
            "path must be lexically normalized and contain no traversal".into(),
        ));
    }
    path.to_str().ok_or_else(|| {
        RegistrationError::InvalidTargetPath(
            "path is not valid UTF-8 and cannot be represented in JSON".into(),
        )
    })?;
    Ok(())
}

fn ensure_record_filename(path: &Path, id: RegistrationId) -> Result<(), RegistrationError> {
    let expected = id.registration_filename();
    if path.file_name().and_then(|name| name.to_str()) == Some(expected.as_str()) {
        Ok(())
    } else {
        Err(RegistrationError::InvalidId(format!(
            "record for {id} must be loaded from {expected}"
        )))
    }
}

fn validate_display_name(name: &str) -> Result<(), RegistrationError> {
    if name.is_empty() || name.trim() != name {
        return Err(RegistrationError::InvalidDisplayName(
            "name must be nonempty and have no surrounding whitespace".into(),
        ));
    }
    if name.len() > MAX_DISPLAY_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(RegistrationError::InvalidDisplayName(format!(
            "name must contain at most {MAX_DISPLAY_NAME_BYTES} bytes and no control characters"
        )));
    }
    Ok(())
}

fn validate_icon(icon: &AppImageIcon) -> Result<(), RegistrationError> {
    let AppImageIcon::Extracted {
        source_name,
        content_sha256,
    } = icon
    else {
        return Ok(());
    };
    if source_name.is_empty()
        || source_name.len() > MAX_ICON_NAME_BYTES
        || !source_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        || source_name == "."
        || source_name == ".."
    {
        return Err(RegistrationError::InvalidIcon(
            "source_name must be a bounded FreeDesktop icon basename".into(),
        ));
    }
    if content_sha256.len() != 64
        || !content_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RegistrationError::InvalidIcon(
            "content_sha256 must be 64 lowercase hexadecimal characters".into(),
        ));
    }
    Ok(())
}

fn schema_version(object: &Map<String, Value>) -> Result<u32, RegistrationError> {
    object
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(RegistrationError::MissingSchemaVersion)
}

fn exact_icon_keys(value: &Value) -> Result<(), RegistrationError> {
    let object = value
        .get("icon")
        .and_then(Value::as_object)
        .ok_or_else(|| RegistrationError::InvalidIcon("icon must be an object".into()))?;
    match object.get("kind").and_then(Value::as_str) {
        Some("built_in") => exact_keys(object, &["kind"]),
        Some("extracted") => exact_keys(object, &["kind", "source_name", "content_sha256"]),
        _ => Err(RegistrationError::InvalidIcon(
            "icon kind must be built_in or extracted".into(),
        )),
    }
}

fn exact_keys(object: &Map<String, Value>, expected: &[&str]) -> Result<(), RegistrationError> {
    let actual: BTreeSet<_> = object.keys().map(String::as_str).collect();
    let expected_set: BTreeSet<_> = expected.iter().copied().collect();
    if actual == expected_set {
        Ok(())
    } else {
        Err(RegistrationError::UnexpectedFields {
            expected: expected.join(", "),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn registration() -> AppImageRegistration {
        AppImageRegistration {
            schema_version: APPIMAGE_REGISTRATION_SCHEMA_VERSION,
            id: RegistrationId::generate(),
            target_path: PathBuf::from("/shared/odd name ' 100%\n日本語.AppImage"),
            display_name: "Example 日本語".into(),
            icon: AppImageIcon::BuiltIn,
            last_observed: Some(FileObservation {
                device: 1,
                inode: 2,
                size: 3,
            }),
            applications_launcher: true,
            desktop_shortcut: false,
            created_at_unix_seconds: 1,
            last_successful_launch_unix_seconds: None,
        }
    }

    #[test]
    fn random_ids_are_canonical_and_generate_safe_stable_names() {
        let id = RegistrationId::generate();
        assert_eq!(id.to_string().parse::<RegistrationId>().unwrap(), id);
        assert!(id.desktop_file_id().starts_with("wildbuzzard-appimage-"));
        assert!(id.desktop_file_id().ends_with(".desktop"));
        assert!(!id.desktop_file_id().contains('/'));
        assert!(
            "550E8400-E29B-41D4-A716-446655440000"
                .parse::<RegistrationId>()
                .is_err()
        );
    }

    #[test]
    fn registration_round_trips_paths_without_shell_interpretation() {
        let temp = tempfile::tempdir().unwrap();
        let registration = registration();
        let path = temp.path().join(registration.id.registration_filename());
        registration.save(&path).unwrap();
        assert_eq!(
            AppImageRegistration::load(&path).unwrap().value,
            registration
        );
    }

    #[test]
    fn traversal_relative_root_and_oversized_paths_are_rejected() {
        let mut value = registration();
        for path in [
            "relative.AppImage",
            "/",
            "/shared/../escape.AppImage",
            "/shared/./alias.AppImage",
            "/shared//alias.AppImage",
        ] {
            value.target_path = PathBuf::from(path);
            assert!(matches!(
                value.validate(),
                Err(RegistrationError::InvalidTargetPath(_))
            ));
        }
        value.target_path = PathBuf::from(format!("/shared/{}", "a".repeat(4097)));
        assert!(matches!(
            value.validate(),
            Err(RegistrationError::InvalidTargetPath(_))
        ));
    }

    #[test]
    fn icon_metadata_rejects_paths_and_noncanonical_digests() {
        let mut value = registration();
        value.icon = AppImageIcon::Extracted {
            source_name: "../../icon.png".into(),
            content_sha256: "a".repeat(64),
        };
        assert!(matches!(
            value.validate(),
            Err(RegistrationError::InvalidIcon(_))
        ));
        value.icon = AppImageIcon::Extracted {
            source_name: "icon.png".into(),
            content_sha256: "A".repeat(64),
        };
        assert!(matches!(
            value.validate(),
            Err(RegistrationError::InvalidIcon(_))
        ));
    }

    #[test]
    fn version_zero_migrates_and_newer_versions_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let id = RegistrationId::generate();
        let path = temp.path().join(id.registration_filename());
        let legacy = format!(
            r#"{{"schema_version":0,"id":"{id}","target_path":"/shared/a.AppImage","display_name":"A","applications_launcher":true,"desktop_shortcut":false,"created_at_unix_seconds":1}}"#
        );
        fs::write(&path, &legacy).unwrap();
        let outcome = AppImageRegistration::load_and_migrate(&path).unwrap();
        assert_eq!(outcome.migrated_from, Some(0));
        assert_eq!(outcome.value.icon, AppImageIcon::BuiltIn);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap()["schema_version"],
            1
        );

        let future = b"{\"schema_version\":42,\"opaque\":\"keep me\"}\n";
        fs::write(&path, future).unwrap();
        assert!(matches!(
            AppImageRegistration::load_and_migrate(&path),
            Err(RegistrationError::NewerSchema { found: 42, .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), future);
    }

    #[test]
    fn missing_nullable_fields_and_unknown_fields_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let registration = registration();
        let path = temp.path().join(registration.id.registration_filename());
        let mut value = serde_json::to_value(&registration).unwrap();
        value.as_object_mut().unwrap().remove("last_observed");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            AppImageRegistration::load(&path),
            Err(RegistrationError::UnexpectedFields { .. })
        ));
    }

    #[test]
    fn current_and_legacy_records_must_match_their_filename() {
        let temp = tempfile::tempdir().unwrap();
        let registration = registration();
        let wrong_id = RegistrationId::generate();
        let wrong_path = temp.path().join(wrong_id.registration_filename());
        fs::write(&wrong_path, serde_json::to_vec(&registration).unwrap()).unwrap();
        assert!(matches!(
            AppImageRegistration::load(&wrong_path),
            Err(RegistrationError::InvalidId(_))
        ));

        let legacy = format!(
            r#"{{"schema_version":0,"id":"{}","target_path":"/shared/a.AppImage","display_name":"A","applications_launcher":true,"desktop_shortcut":false,"created_at_unix_seconds":1}}"#,
            registration.id
        );
        fs::write(&wrong_path, &legacy).unwrap();
        let original = fs::read(&wrong_path).unwrap();
        assert!(matches!(
            AppImageRegistration::load_and_migrate(&wrong_path),
            Err(RegistrationError::InvalidId(_))
        ));
        assert_eq!(fs::read(wrong_path).unwrap(), original);
    }
}
