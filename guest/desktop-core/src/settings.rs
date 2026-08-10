// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::persistence::{
    LoadOutcome, MAX_MANAGED_JSON_BYTES, PersistenceError, atomic_write_json, read_bounded,
};
use crate::state::{BackgroundChoice, GuestScalePreset, ThemeMode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::Path;
use thiserror::Error;

pub const SETTINGS_SCHEMA_VERSION: u32 = 1;
const MAX_PLAN_GENERATION_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppearanceSettings {
    pub theme: ThemeMode,
    pub background: BackgroundChoice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplaySettings {
    pub guest_ui_scale: GuestScalePreset,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePreferences {
    pub last_notified_plan_generation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub schema_version: u32,
    pub generation: u64,
    pub appearance: AppearanceSettings,
    pub display: DisplaySettings,
    pub updates: UpdatePreferences,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            generation: 0,
            appearance: AppearanceSettings {
                theme: ThemeMode::Dark,
                background: BackgroundChoice::DarkPlain,
            },
            display: DisplaySettings {
                guest_ui_scale: GuestScalePreset::Automatic,
            },
            updates: UpdatePreferences {
                last_notified_plan_generation: None,
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsV0 {
    schema_version: u32,
    dark_mode: bool,
    scale_percent: Option<u16>,
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error("settings document must be a JSON object")]
    NotObject,
    #[error("settings document is missing integer schema_version")]
    MissingSchemaVersion,
    #[error(
        "settings schema {found} is newer than supported schema {current}; the file was preserved"
    )]
    NewerSchema { found: u32, current: u32 },
    #[error("settings schema {found} cannot be migrated to schema {current}")]
    UnsupportedOlderSchema { found: u32, current: u32 },
    #[error("settings schema has unexpected fields; required fields are: {expected}")]
    UnexpectedFields { expected: String },
    #[error("settings validation failed: {0}")]
    Validation(String),
    #[error("settings JSON does not match schema: {0}")]
    Schema(#[from] serde_json::Error),
}

impl Settings {
    pub fn validate(&self) -> Result<(), SettingsError> {
        if self.schema_version != SETTINGS_SCHEMA_VERSION {
            return Err(if self.schema_version > SETTINGS_SCHEMA_VERSION {
                SettingsError::NewerSchema {
                    found: self.schema_version,
                    current: SETTINGS_SCHEMA_VERSION,
                }
            } else {
                SettingsError::UnsupportedOlderSchema {
                    found: self.schema_version,
                    current: SETTINGS_SCHEMA_VERSION,
                }
            });
        }
        if let Some(generation) = &self.updates.last_notified_plan_generation {
            if generation.is_empty() || generation.len() > MAX_PLAN_GENERATION_BYTES {
                return Err(SettingsError::Validation(format!(
                    "last_notified_plan_generation must contain 1 to {MAX_PLAN_GENERATION_BYTES} bytes"
                )));
            }
            if generation.chars().any(char::is_control) {
                return Err(SettingsError::Validation(
                    "last_notified_plan_generation contains control characters".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<LoadOutcome<Self>, SettingsError> {
        let bytes = read_bounded(path, MAX_MANAGED_JSON_BYTES)?;
        let value: Value = serde_json::from_slice(&bytes)?;
        let object = value.as_object().ok_or(SettingsError::NotObject)?;
        let version = schema_version(object)?;
        let outcome = match version {
            SETTINGS_SCHEMA_VERSION => {
                exact_keys(
                    object,
                    &[
                        "schema_version",
                        "generation",
                        "appearance",
                        "display",
                        "updates",
                    ],
                )?;
                exact_nested_keys(&value, "appearance", &["theme", "background"])?;
                exact_background_keys(&value)?;
                exact_nested_keys(&value, "display", &["guest_ui_scale"])?;
                exact_nested_keys(&value, "updates", &["last_notified_plan_generation"])?;
                LoadOutcome {
                    value: serde_json::from_value(value)?,
                    migrated_from: None,
                }
            }
            0 => {
                exact_keys(object, &["schema_version", "dark_mode", "scale_percent"])?;
                let old: SettingsV0 = serde_json::from_value(value)?;
                if old.schema_version != 0 {
                    return Err(SettingsError::UnsupportedOlderSchema {
                        found: old.schema_version,
                        current: SETTINGS_SCHEMA_VERSION,
                    });
                }
                let guest_ui_scale = match old.scale_percent {
                    None => GuestScalePreset::Automatic,
                    Some(percent) => GuestScalePreset::from_percent(percent).ok_or_else(|| {
                        SettingsError::Validation(format!(
                            "legacy scale_percent {percent} is not a supported preset"
                        ))
                    })?,
                };
                LoadOutcome {
                    value: Self {
                        appearance: AppearanceSettings {
                            theme: if old.dark_mode {
                                ThemeMode::Dark
                            } else {
                                ThemeMode::Light
                            },
                            background: if old.dark_mode {
                                BackgroundChoice::DarkPlain
                            } else {
                                BackgroundChoice::LightPlain
                            },
                        },
                        display: DisplaySettings { guest_ui_scale },
                        ..Self::default()
                    },
                    migrated_from: Some(0),
                }
            }
            found if found > SETTINGS_SCHEMA_VERSION => {
                return Err(SettingsError::NewerSchema {
                    found,
                    current: SETTINGS_SCHEMA_VERSION,
                });
            }
            found => {
                return Err(SettingsError::UnsupportedOlderSchema {
                    found,
                    current: SETTINGS_SCHEMA_VERSION,
                });
            }
        };
        outcome.value.validate()?;
        Ok(outcome)
    }

    pub fn load_and_migrate(path: &Path) -> Result<LoadOutcome<Self>, SettingsError> {
        let outcome = Self::load(path)?;
        if outcome.migrated_from.is_some() {
            outcome.value.save(path)?;
        }
        Ok(outcome)
    }

    pub fn save(&self, path: &Path) -> Result<(), SettingsError> {
        self.validate()?;
        atomic_write_json(path, self)?;
        Ok(())
    }
}

fn schema_version(object: &Map<String, Value>) -> Result<u32, SettingsError> {
    let raw = object
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or(SettingsError::MissingSchemaVersion)?;
    u32::try_from(raw).map_err(|_| SettingsError::MissingSchemaVersion)
}

fn exact_nested_keys(value: &Value, field: &str, expected: &[&str]) -> Result<(), SettingsError> {
    let object = value
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| SettingsError::Validation(format!("{field} must be an object")))?;
    exact_keys(object, expected)
}

fn exact_background_keys(value: &Value) -> Result<(), SettingsError> {
    let object = value
        .get("appearance")
        .and_then(|appearance| appearance.get("background"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            SettingsError::Validation("appearance.background must be an object".into())
        })?;
    let kind = object.get("kind").and_then(Value::as_str).ok_or_else(|| {
        SettingsError::Validation("appearance.background.kind must be a string".into())
    })?;
    if kind == "custom_solid" {
        exact_keys(object, &["kind", "color"])
    } else {
        exact_keys(object, &["kind"])
    }
}

fn exact_keys(object: &Map<String, Value>, expected: &[&str]) -> Result<(), SettingsError> {
    let actual: BTreeSet<_> = object.keys().map(String::as_str).collect();
    let expected_set: BTreeSet<_> = expected.iter().copied().collect();
    if actual == expected_set {
        Ok(())
    } else {
        Err(SettingsError::UnexpectedFields {
            expected: expected.join(", "),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn current_settings_round_trip_strictly() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let settings = Settings::default();
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap().value, settings);
    }

    #[test]
    fn legacy_settings_migrate_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        fs::write(
            &path,
            br#"{"schema_version":0,"dark_mode":false,"scale_percent":175}"#,
        )
        .unwrap();
        let outcome = Settings::load_and_migrate(&path).unwrap();
        assert_eq!(outcome.migrated_from, Some(0));
        assert_eq!(outcome.value.appearance.theme, ThemeMode::Light);
        assert_eq!(
            outcome.value.appearance.background,
            BackgroundChoice::LightPlain
        );
        assert_eq!(
            outcome.value.display.guest_ui_scale,
            GuestScalePreset::Percent175
        );
        let persisted: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["schema_version"], SETTINGS_SCHEMA_VERSION);
    }

    #[test]
    fn newer_schema_is_preserved_byte_for_byte() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let original = b"{\"schema_version\":999,\"future\":true}\n";
        fs::write(&path, original).unwrap();
        assert!(matches!(
            Settings::load_and_migrate(&path),
            Err(SettingsError::NewerSchema { found: 999, .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn missing_optional_key_and_unknown_key_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        fs::write(
            &path,
            br#"{"schema_version":1,"generation":0,"appearance":{"theme":"dark","background":{"kind":"dark_logo"}},"display":{"guest_ui_scale":"automatic"},"updates":{}}"#,
        )
        .unwrap();
        assert!(matches!(
            Settings::load(&path),
            Err(SettingsError::UnexpectedFields { .. })
        ));
        fs::write(
            &path,
            br#"{"schema_version":1,"generation":0,"appearance":{"theme":"dark","background":{"kind":"dark_logo"},"surprise":1},"display":{"guest_ui_scale":"automatic"},"updates":{"last_notified_plan_generation":null}}"#,
        )
        .unwrap();
        assert!(matches!(
            Settings::load(&path),
            Err(SettingsError::UnexpectedFields { .. })
        ));
    }

    #[test]
    fn invalid_legacy_scale_does_not_rewrite_source() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let original = br#"{"schema_version":0,"dark_mode":true,"scale_percent":133}"#;
        fs::write(&path, original).unwrap();
        assert!(matches!(
            Settings::load_and_migrate(&path),
            Err(SettingsError::Validation(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn theme_and_background_are_independent_and_remote_values_are_impossible() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let mut settings = Settings::default();
        settings.appearance.theme = ThemeMode::Light;
        settings.appearance.background = BackgroundChoice::CustomSolid {
            color: "#123456".parse().unwrap(),
        };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap().value, settings);

        let hostile = fs::read_to_string(&path)
            .unwrap()
            .replace("#123456", "url(https://example.test/wallpaper)");
        fs::write(&path, hostile).unwrap();
        assert!(matches!(
            Settings::load(&path),
            Err(SettingsError::Schema(_))
        ));
    }
}
