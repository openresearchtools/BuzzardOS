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

pub const SETTINGS_SCHEMA_VERSION: u32 = 3;
const MAX_PLAN_GENERATION_BYTES: usize = 512;
const MAX_XKB_MODEL_BYTES: usize = 64;
const MAX_XKB_LAYOUT_BYTES: usize = 256;
const MAX_XKB_VARIANT_BYTES: usize = 256;
const MAX_XKB_OPTIONS_BYTES: usize = 512;
const MAX_XKB_LAYOUT_GROUPS: usize = 4;
const MAX_PINNED_APPLICATIONS: usize = 256;
const MAX_APPLICATION_ID_BYTES: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppearanceSettings {
    pub theme: ThemeMode,
    pub background: BackgroundChoice,
    pub capped_task_buttons: bool,
    pub pinned_applications: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplaySettings {
    pub guest_ui_scale: GuestScalePreset,
}

/// The keymap compiled by Sway for every keyboard on the private guest seat.
///
/// Values are XKB component names, never commands or paths.  Keeping this
/// contract in the strict Settings schema lets the desktop apply the same
/// layout at session startup and at runtime without granting the Settings UI
/// an arbitrary compositor-command surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyboardSettings {
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: String,
}

impl Default for KeyboardSettings {
    fn default() -> Self {
        Self {
            model: "pc105".into(),
            layout: "us".into(),
            variant: String::new(),
            options: String::new(),
        }
    }
}

impl KeyboardSettings {
    pub fn validate(&self) -> Result<(), SettingsError> {
        validate_xkb_component("keyboard.model", &self.model, 1, MAX_XKB_MODEL_BYTES)?;
        validate_xkb_component("keyboard.layout", &self.layout, 1, MAX_XKB_LAYOUT_BYTES)?;
        validate_xkb_component("keyboard.variant", &self.variant, 0, MAX_XKB_VARIANT_BYTES)?;
        validate_xkb_component("keyboard.options", &self.options, 0, MAX_XKB_OPTIONS_BYTES)?;

        let layouts = validate_xkb_groups("keyboard.layout", &self.layout, MAX_XKB_LAYOUT_GROUPS)?;
        if !self.variant.is_empty() {
            let variants = self.variant.split(',').count();
            if variants > MAX_XKB_LAYOUT_GROUPS {
                return Err(SettingsError::Validation(format!(
                    "keyboard.variant must contain at most {MAX_XKB_LAYOUT_GROUPS} comma-aligned groups"
                )));
            }
            if variants > layouts {
                return Err(SettingsError::Validation(
                    "keyboard.variant defines more groups than keyboard.layout".into(),
                ));
            }
        }
        if !self.options.is_empty() && self.options.split(',').any(str::is_empty) {
            return Err(SettingsError::Validation(
                "keyboard.options contains an empty option segment".into(),
            ));
        }
        Ok(())
    }
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
    pub keyboard: KeyboardSettings,
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
                capped_task_buttons: true,
                pinned_applications: Vec::new(),
            },
            display: DisplaySettings {
                guest_ui_scale: GuestScalePreset::Automatic,
            },
            keyboard: KeyboardSettings::default(),
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsV1 {
    schema_version: u32,
    generation: u64,
    appearance: AppearanceSettingsV2,
    display: DisplaySettings,
    updates: UpdatePreferences,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsV2 {
    schema_version: u32,
    generation: u64,
    appearance: AppearanceSettingsV2,
    display: DisplaySettings,
    keyboard: KeyboardSettings,
    updates: UpdatePreferences,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppearanceSettingsV2 {
    theme: ThemeMode,
    background: BackgroundChoice,
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
        self.keyboard.validate()?;
        if self.appearance.pinned_applications.len() > MAX_PINNED_APPLICATIONS {
            return Err(SettingsError::Validation(format!(
                "appearance.pinned_applications may contain at most {MAX_PINNED_APPLICATIONS} entries"
            )));
        }
        let mut pinned = BTreeSet::new();
        for id in &self.appearance.pinned_applications {
            if id.is_empty()
                || id.len() > MAX_APPLICATION_ID_BYTES
                || id.chars().any(char::is_control)
                || !pinned.insert(id)
            {
                return Err(SettingsError::Validation(
                    "appearance.pinned_applications contains an invalid or duplicate desktop ID"
                        .into(),
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
                        "keyboard",
                        "updates",
                    ],
                )?;
                exact_nested_keys(
                    &value,
                    "appearance",
                    &[
                        "theme",
                        "background",
                        "capped_task_buttons",
                        "pinned_applications",
                    ],
                )?;
                exact_background_keys(&value)?;
                exact_nested_keys(&value, "display", &["guest_ui_scale"])?;
                exact_nested_keys(
                    &value,
                    "keyboard",
                    &["model", "layout", "variant", "options"],
                )?;
                exact_nested_keys(&value, "updates", &["last_notified_plan_generation"])?;
                LoadOutcome {
                    value: serde_json::from_value(value)?,
                    migrated_from: None,
                }
            }
            2 => {
                exact_keys(
                    object,
                    &[
                        "schema_version",
                        "generation",
                        "appearance",
                        "display",
                        "keyboard",
                        "updates",
                    ],
                )?;
                exact_nested_keys(&value, "appearance", &["theme", "background"])?;
                exact_background_keys(&value)?;
                exact_nested_keys(&value, "display", &["guest_ui_scale"])?;
                exact_nested_keys(
                    &value,
                    "keyboard",
                    &["model", "layout", "variant", "options"],
                )?;
                exact_nested_keys(&value, "updates", &["last_notified_plan_generation"])?;
                let old: SettingsV2 = serde_json::from_value(value)?;
                if old.schema_version != 2 {
                    return Err(SettingsError::UnsupportedOlderSchema {
                        found: old.schema_version,
                        current: SETTINGS_SCHEMA_VERSION,
                    });
                }
                LoadOutcome {
                    value: Self {
                        schema_version: SETTINGS_SCHEMA_VERSION,
                        generation: old.generation,
                        appearance: AppearanceSettings {
                            theme: old.appearance.theme,
                            background: old.appearance.background,
                            capped_task_buttons: true,
                            pinned_applications: Vec::new(),
                        },
                        display: old.display,
                        keyboard: old.keyboard,
                        updates: old.updates,
                    },
                    migrated_from: Some(2),
                }
            }
            1 => {
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
                let old: SettingsV1 = serde_json::from_value(value)?;
                if old.schema_version != 1 {
                    return Err(SettingsError::UnsupportedOlderSchema {
                        found: old.schema_version,
                        current: SETTINGS_SCHEMA_VERSION,
                    });
                }
                LoadOutcome {
                    value: Self {
                        schema_version: SETTINGS_SCHEMA_VERSION,
                        generation: old.generation,
                        appearance: AppearanceSettings {
                            theme: old.appearance.theme,
                            background: old.appearance.background,
                            capped_task_buttons: true,
                            pinned_applications: Vec::new(),
                        },
                        display: old.display,
                        keyboard: KeyboardSettings::default(),
                        updates: old.updates,
                    },
                    migrated_from: Some(1),
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
                            capped_task_buttons: true,
                            pinned_applications: Vec::new(),
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

fn validate_xkb_component(
    field: &str,
    value: &str,
    minimum_bytes: usize,
    maximum_bytes: usize,
) -> Result<(), SettingsError> {
    if !(minimum_bytes..=maximum_bytes).contains(&value.len()) {
        return Err(SettingsError::Validation(format!(
            "{field} must contain {minimum_bytes} to {maximum_bytes} bytes"
        )));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_-+,:.".contains(&byte))
    {
        return Err(SettingsError::Validation(format!(
            "{field} contains characters which are not valid in a bounded XKB component name"
        )));
    }
    Ok(())
}

fn validate_xkb_groups(
    field: &str,
    value: &str,
    maximum_groups: usize,
) -> Result<usize, SettingsError> {
    let groups = value.split(',').collect::<Vec<_>>();
    if groups.is_empty()
        || groups.len() > maximum_groups
        || groups.iter().any(|part| part.is_empty())
    {
        return Err(SettingsError::Validation(format!(
            "{field} must contain 1 to {maximum_groups} non-empty comma-separated groups"
        )));
    }
    Ok(groups.len())
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
    fn schema_one_migrates_with_a_safe_us_keyboard_default() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        fs::write(
            &path,
            br##"{"schema_version":1,"generation":7,"appearance":{"theme":"dark","background":{"kind":"custom_solid","color":"#202225"}},"display":{"guest_ui_scale":"125"},"updates":{"last_notified_plan_generation":null}}"##,
        )
        .unwrap();
        let outcome = Settings::load_and_migrate(&path).unwrap();
        assert_eq!(outcome.migrated_from, Some(1));
        assert_eq!(outcome.value.generation, 7);
        assert_eq!(outcome.value.keyboard, KeyboardSettings::default());
        let persisted: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["schema_version"], SETTINGS_SCHEMA_VERSION);
        assert_eq!(persisted["keyboard"]["layout"], "us");
    }

    #[test]
    fn keyboard_schema_accepts_real_xkb_names_and_rejects_commands() {
        let mut settings = Settings {
            keyboard: KeyboardSettings {
                model: "pc105".into(),
                layout: "us,gb".into(),
                variant: "intl,".into(),
                options: "compose:ralt,grp:alt_shift_toggle".into(),
            },
            ..Settings::default()
        };
        settings.validate().unwrap();

        for hostile in ["us;exec_foot", "us $(id)", "../../symbols/us", "us\n"] {
            settings.keyboard.layout = hostile.into();
            assert!(matches!(
                settings.validate(),
                Err(SettingsError::Validation(_))
            ));
        }

        for malformed in [
            KeyboardSettings {
                layout: "us,,gb".into(),
                ..KeyboardSettings::default()
            },
            KeyboardSettings {
                layout: "us,gb,de,fr,es".into(),
                ..KeyboardSettings::default()
            },
            KeyboardSettings {
                layout: "us".into(),
                variant: "intl,extd".into(),
                ..KeyboardSettings::default()
            },
            KeyboardSettings {
                options: "compose:ralt,,grp:alt_shift_toggle".into(),
                ..KeyboardSettings::default()
            },
        ] {
            assert!(matches!(
                malformed.validate(),
                Err(SettingsError::Validation(_))
            ));
        }
    }

    #[test]
    fn shared_manually_authored_keyboard_contract_matches_settings_loader() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/xkb-settings-contract.json"
        ))
        .unwrap();
        assert_eq!(fixture["schema"], 1);
        for case in fixture["cases"].as_array().unwrap() {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("settings.json");
            let document = serde_json::json!({
                "schema_version": 2,
                "generation": 0,
                "appearance": {"theme": "dark", "background": {"kind": "dark_plain"}},
                "display": {"guest_ui_scale": "automatic"},
                "keyboard": case["keyboard"].clone(),
                "updates": {"last_notified_plan_generation": null}
            });
            fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
            assert_eq!(
                Settings::load(&path).is_ok(),
                case["valid"].as_bool().unwrap(),
                "shared XKB contract case {}",
                case["name"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn keyboard_component_byte_bounds_are_exact() {
        let mut keyboard = KeyboardSettings::default();
        for (field, maximum) in [
            ("model", MAX_XKB_MODEL_BYTES),
            ("layout", MAX_XKB_LAYOUT_BYTES),
            ("variant", MAX_XKB_VARIANT_BYTES),
            ("options", MAX_XKB_OPTIONS_BYTES),
        ] {
            let valid = "a".repeat(maximum);
            let invalid = "a".repeat(maximum + 1);
            match field {
                "model" => keyboard.model = valid,
                "layout" => keyboard.layout = valid,
                "variant" => keyboard.variant = valid,
                "options" => keyboard.options = valid,
                _ => unreachable!(),
            }
            keyboard.validate().unwrap();
            match field {
                "model" => keyboard.model = invalid,
                "layout" => keyboard.layout = invalid,
                "variant" => keyboard.variant = invalid,
                "options" => keyboard.options = invalid,
                _ => unreachable!(),
            }
            assert!(matches!(
                keyboard.validate(),
                Err(SettingsError::Validation(_))
            ));
            keyboard = KeyboardSettings::default();
        }
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

    #[test]
    fn pinned_application_ids_round_trip_and_reject_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.json");
        let mut settings = Settings::default();
        settings.appearance.pinned_applications = vec![
            "firefox-esr.desktop".into(),
            "org.xfce.Thunar.desktop".into(),
        ];
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap().value, settings);

        settings
            .appearance
            .pinned_applications
            .push("firefox-esr.desktop".into());
        assert!(matches!(
            settings.validate(),
            Err(SettingsError::Validation(_))
        ));
    }
}
