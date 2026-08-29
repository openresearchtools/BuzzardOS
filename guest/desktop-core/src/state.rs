// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::persistence::{
    MAX_MANAGED_JSON_BYTES, PersistenceError, atomic_write_json, read_bounded,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use thiserror::Error;

pub const UPDATE_STATE_SCHEMA_VERSION: u32 = 2;
const MAX_DISPLAY_EXTENT: u32 = 65_535;
const MIN_SCALE_120: u32 = 30;
const MAX_SCALE_120: u32 = 960;
const MAX_UPDATE_PACKAGES: usize = 16_384;
const MAX_REPOSITORY_ERRORS: usize = 1_024;
const MAX_RESTART_REASONS: usize = 256;
const MAX_TEXT_BYTES: usize = 16_384;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
}

pub const DARK_WALLPAPER: SolidColor = SolidColor::new(0x20, 0x22, 0x25);
pub const LIGHT_WALLPAPER: SolidColor = SolidColor::new(0xfa, 0xfa, 0xfa);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SolidColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl SolidColor {
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    pub const fn rgba(self) -> [u8; 4] {
        [self.red, self.green, self.blue, 0xff]
    }
}

impl fmt::Display for SolidColor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "#{:02X}{:02X}{:02X}",
            self.red, self.green, self.blue
        )
    }
}

#[derive(Debug, Error, Clone, PartialEq)]
#[error("solid colour must be exactly #RRGGBB")]
pub struct SolidColorParseError;

impl FromStr for SolidColor {
    type Err = SolidColorParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let bytes = value.as_bytes();
        if bytes.len() != 7 || bytes[0] != b'#' {
            return Err(SolidColorParseError);
        }
        fn component(value: &[u8]) -> Result<u8, SolidColorParseError> {
            let text = std::str::from_utf8(value).map_err(|_| SolidColorParseError)?;
            u8::from_str_radix(text, 16).map_err(|_| SolidColorParseError)
        }
        Ok(Self::new(
            component(&bytes[1..3])?,
            component(&bytes[3..5])?,
            component(&bytes[5..7])?,
        ))
    }
}

impl Serialize for SolidColor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for SolidColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

/// Desktop background selection. Deprecated logo preset names deserialize as
/// their corresponding solid colour so existing persistent machines migrate
/// without becoming read-only; newly saved settings are always plain/solid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BackgroundChoice {
    #[default]
    #[serde(alias = "dark_logo")]
    DarkPlain,
    #[serde(alias = "light_logo")]
    LightPlain,
    CustomSolid {
        color: SolidColor,
    },
}

impl BackgroundChoice {
    pub const fn solid_color(self) -> SolidColor {
        match self {
            Self::DarkPlain => DARK_WALLPAPER,
            Self::LightPlain => LIGHT_WALLPAPER,
            Self::CustomSolid { color } => color,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GuestScalePreset {
    #[default]
    #[serde(rename = "automatic")]
    Automatic,
    #[serde(rename = "100")]
    Percent100,
    #[serde(rename = "125")]
    Percent125,
    #[serde(rename = "150")]
    Percent150,
    #[serde(rename = "175")]
    Percent175,
    #[serde(rename = "200")]
    Percent200,
}

impl GuestScalePreset {
    pub const ALL: [Self; 6] = [
        Self::Automatic,
        Self::Percent100,
        Self::Percent125,
        Self::Percent150,
        Self::Percent175,
        Self::Percent200,
    ];

    pub const fn scale_120(self) -> Option<u32> {
        match self {
            Self::Automatic => None,
            Self::Percent100 => Some(120),
            Self::Percent125 => Some(150),
            Self::Percent150 => Some(180),
            Self::Percent175 => Some(210),
            Self::Percent200 => Some(240),
        }
    }

    pub const fn from_percent(percent: u16) -> Option<Self> {
        match percent {
            100 => Some(Self::Percent100),
            125 => Some(Self::Percent125),
            150 => Some(Self::Percent150),
            175 => Some(Self::Percent175),
            200 => Some(Self::Percent200),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DisplayGeometry {
    pub physical_width: u32,
    pub physical_height: u32,
    pub host_surface_scale_120: u32,
    pub guest_ui_scale_120: u32,
    pub logical_width: u32,
    pub logical_height: u32,
    pub geometry_generation: u64,
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum StateValidationError {
    #[error("{field} must be between {minimum} and {maximum}, got {actual}")]
    OutOfRange {
        field: &'static str,
        minimum: u64,
        maximum: u64,
        actual: u64,
    },
    #[error("geometry generation {actual} is stale; current generation is {current}")]
    StaleGeometry { actual: u64, current: u64 },
    #[error(
        "{axis} logical extent is incoherent with physical extent and guest UI scale: expected {expected}, got {actual}"
    )]
    IncoherentGeometry {
        axis: &'static str,
        expected: u64,
        actual: u64,
    },
    #[error("{space} coordinate must be finite")]
    NonFiniteCoordinate { space: &'static str },
    #[error("{space} coordinate ({x}, {y}) is outside extent {width}x{height}")]
    CoordinateOutOfBounds {
        space: &'static str,
        x: f64,
        y: f64,
        width: u32,
        height: u32,
    },
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} exceeds the {maximum}-byte limit")]
    TextTooLong { field: &'static str, maximum: usize },
    #[error("{field} contains a control character")]
    ControlCharacter { field: &'static str },
    #[error("update state contains too many packages")]
    TooManyPackages,
    #[error("update state contains too many repository errors")]
    TooManyRepositoryErrors,
    #[error("update state contains duplicate package {0}")]
    DuplicatePackage(String),
    #[error("update package download sizes overflow u64")]
    DownloadSizeOverflow,
    #[error("update state invariant failed: {0}")]
    UpdateInvariant(&'static str),
    #[error("unsupported update-state schema {found}; current schema is {current}")]
    UnsupportedUpdateSchema { found: u32, current: u32 },
}

#[derive(Debug, Error)]
pub enum UpdateStateError {
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
    #[error("update-state document must be a JSON object")]
    NotObject,
    #[error("update-state document is missing integer schema_version")]
    MissingSchemaVersion,
    #[error(
        "update-state schema {found} is newer than supported schema {current}; the file was preserved"
    )]
    NewerSchema { found: u32, current: u32 },
    #[error("update-state schema {found} is older than supported schema {current}")]
    OlderSchema { found: u32, current: u32 },
    #[error("update-state schema has unexpected fields; required fields are: {expected}")]
    UnexpectedFields { expected: String },
    #[error("update-state JSON does not match schema: {0}")]
    Schema(#[from] serde_json::Error),
    #[error("update-state document exceeds the {maximum}-byte limit")]
    TooLarge { maximum: usize },
    #[error(transparent)]
    Validation(#[from] StateValidationError),
}

impl DisplayGeometry {
    pub fn validate(&self) -> Result<(), StateValidationError> {
        validate_range("physical_width", self.physical_width, 1, MAX_DISPLAY_EXTENT)?;
        validate_range(
            "physical_height",
            self.physical_height,
            1,
            MAX_DISPLAY_EXTENT,
        )?;
        validate_range("logical_width", self.logical_width, 1, MAX_DISPLAY_EXTENT)?;
        validate_range("logical_height", self.logical_height, 1, MAX_DISPLAY_EXTENT)?;
        validate_range(
            "host_surface_scale_120",
            self.host_surface_scale_120,
            MIN_SCALE_120,
            MAX_SCALE_120,
        )?;
        validate_range(
            "guest_ui_scale_120",
            self.guest_ui_scale_120,
            MIN_SCALE_120,
            MAX_SCALE_120,
        )?;
        self.validate_axis("horizontal", self.physical_width, self.logical_width)?;
        self.validate_axis("vertical", self.physical_height, self.logical_height)
    }

    fn validate_axis(
        &self,
        axis: &'static str,
        physical: u32,
        logical: u32,
    ) -> Result<(), StateValidationError> {
        let expected =
            guest_logical_extent(u64::from(physical), u64::from(self.guest_ui_scale_120));
        if u64::from(logical) == expected {
            Ok(())
        } else {
            Err(StateValidationError::IncoherentGeometry {
                axis,
                expected,
                actual: u64::from(logical),
            })
        }
    }

    pub fn require_generation(&self, generation: u64) -> Result<(), StateValidationError> {
        if generation == self.geometry_generation {
            Ok(())
        } else {
            Err(StateValidationError::StaleGeometry {
                actual: generation,
                current: self.geometry_generation,
            })
        }
    }

    /// Transform a point exactly once using the committed geometry
    /// generation. Extent edges are accepted so rectangle boundaries map
    /// exactly; callers validating pixel indices must still use half-open
    /// bounds. Host surface scale is intentionally not part of this mapping.
    pub fn physical_to_logical(
        &self,
        generation: u64,
        x: f64,
        y: f64,
    ) -> Result<(f64, f64), StateValidationError> {
        self.validate()?;
        self.require_generation(generation)?;
        validate_coordinate("physical", x, y, self.physical_width, self.physical_height)?;
        Ok((
            x * f64::from(self.logical_width) / f64::from(self.physical_width),
            y * f64::from(self.logical_height) / f64::from(self.physical_height),
        ))
    }

    pub fn logical_to_physical(
        &self,
        generation: u64,
        x: f64,
        y: f64,
    ) -> Result<(f64, f64), StateValidationError> {
        self.validate()?;
        self.require_generation(generation)?;
        validate_coordinate("logical", x, y, self.logical_width, self.logical_height)?;
        Ok((
            x * f64::from(self.physical_width) / f64::from(self.logical_width),
            y * f64::from(self.physical_height) / f64::from(self.logical_height),
        ))
    }
}

fn guest_logical_extent(physical: u64, guest_scale_120: u64) -> u64 {
    (physical.saturating_mul(120) / guest_scale_120.max(1)).max(1)
}

fn validate_coordinate(
    space: &'static str,
    x: f64,
    y: f64,
    width: u32,
    height: u32,
) -> Result<(), StateValidationError> {
    if !x.is_finite() || !y.is_finite() {
        return Err(StateValidationError::NonFiniteCoordinate { space });
    }
    if x < 0.0 || y < 0.0 || x > f64::from(width) || y > f64::from(height) {
        return Err(StateValidationError::CoordinateOutOfBounds {
            space,
            x,
            y,
            width,
            height,
        });
    }
    Ok(())
}

fn validate_range(
    field: &'static str,
    actual: u32,
    minimum: u32,
    maximum: u32,
) -> Result<(), StateValidationError> {
    if (minimum..=maximum).contains(&actual) {
        Ok(())
    } else {
        Err(StateValidationError::OutOfRange {
            field,
            minimum: u64::from(minimum),
            maximum: u64::from(maximum),
            actual: u64::from(actual),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStatus {
    NeverChecked,
    Checking,
    UpToDate,
    Available,
    Installing,
    Failed,
    RestartRecommended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateAction {
    Upgrade,
    Install,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateProgressPhase {
    Refreshing,
    Resolving,
    Downloading,
    Installing,
    Repairing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateProgressUnit {
    Bytes,
    Packages,
    Steps,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateProgress {
    pub phase: UpdateProgressPhase,
    pub completed: u64,
    pub total: u64,
    pub unit: UpdateProgressUnit,
    pub detail: Option<String>,
    pub cancellable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePackage {
    pub name: String,
    pub installed_version: String,
    pub candidate_version: String,
    pub download_size: u64,
    pub security_origin: Option<String>,
    pub action: UpdateAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateState {
    pub schema_version: u32,
    pub state_generation: u64,
    pub status: UpdateStatus,
    pub checked_at_unix_seconds: Option<u64>,
    pub repository_errors: Vec<String>,
    pub packages: Vec<UpdatePackage>,
    pub download_size: u64,
    pub plan_generation: Option<String>,
    pub progress: Option<UpdateProgress>,
    pub failure: Option<String>,
    pub repair_available: bool,
    pub restart_reasons: Vec<String>,
    pub last_log_id: Option<String>,
    pub runtime_revision: Option<String>,
    pub runtime_ready: bool,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            schema_version: UPDATE_STATE_SCHEMA_VERSION,
            state_generation: 1,
            status: UpdateStatus::NeverChecked,
            checked_at_unix_seconds: None,
            repository_errors: Vec::new(),
            packages: Vec::new(),
            download_size: 0,
            plan_generation: None,
            progress: None,
            failure: None,
            repair_available: false,
            restart_reasons: Vec::new(),
            last_log_id: None,
            runtime_revision: None,
            runtime_ready: false,
        }
    }
}

impl UpdateState {
    pub fn validate(&self) -> Result<(), StateValidationError> {
        if self.schema_version != UPDATE_STATE_SCHEMA_VERSION {
            return Err(StateValidationError::UnsupportedUpdateSchema {
                found: self.schema_version,
                current: UPDATE_STATE_SCHEMA_VERSION,
            });
        }
        if self.state_generation == 0 {
            return Err(StateValidationError::UpdateInvariant(
                "state_generation must be positive",
            ));
        }
        if self.packages.len() > MAX_UPDATE_PACKAGES {
            return Err(StateValidationError::TooManyPackages);
        }
        if self.repository_errors.len() > MAX_REPOSITORY_ERRORS {
            return Err(StateValidationError::TooManyRepositoryErrors);
        }
        if self.restart_reasons.len() > MAX_RESTART_REASONS {
            return Err(StateValidationError::UpdateInvariant(
                "restart_reasons exceeds the bounded entry limit",
            ));
        }
        for error in &self.repository_errors {
            validate_text("repository_errors", error, true)?;
        }
        for reason in &self.restart_reasons {
            validate_text("restart_reasons", reason, true)?;
        }
        let mut package_names = HashSet::new();
        let mut calculated_download_size = 0u64;
        let mut previous_name: Option<&str> = None;
        for package in &self.packages {
            validate_text("package.name", &package.name, true)?;
            if !is_debian_package_name(&package.name) {
                return Err(StateValidationError::UpdateInvariant(
                    "package.name is not a canonical Debian package identifier",
                ));
            }
            if !package_names.insert(package.name.as_str()) {
                return Err(StateValidationError::DuplicatePackage(package.name.clone()));
            }
            if previous_name.is_some_and(|previous| previous > package.name.as_str()) {
                return Err(StateValidationError::UpdateInvariant(
                    "packages must be sorted by package name",
                ));
            }
            previous_name = Some(&package.name);
            validate_text(
                "package.installed_version",
                &package.installed_version,
                true,
            )?;
            validate_text(
                "package.candidate_version",
                &package.candidate_version,
                true,
            )?;
            if let Some(origin) = &package.security_origin {
                validate_text("package.security_origin", origin, true)?;
            }
            calculated_download_size = calculated_download_size
                .checked_add(package.download_size)
                .ok_or(StateValidationError::DownloadSizeOverflow)?;
        }
        if calculated_download_size != self.download_size {
            return Err(StateValidationError::UpdateInvariant(
                "download_size must equal the checked sum of package download sizes",
            ));
        }
        if self.checked_at_unix_seconds == Some(0) {
            return Err(StateValidationError::UpdateInvariant(
                "checked_at_unix_seconds must be positive when present",
            ));
        }
        if let Some(generation) = &self.plan_generation {
            validate_text("plan_generation", generation, true)?;
            if !is_lower_hex(generation, 64) {
                return Err(StateValidationError::UpdateInvariant(
                    "plan_generation must be 64 lowercase hexadecimal characters",
                ));
            }
        }
        if let Some(progress) = &self.progress {
            if progress.completed > progress.total {
                return Err(StateValidationError::UpdateInvariant(
                    "progress completed exceeds total",
                ));
            }
            if let Some(detail) = &progress.detail {
                validate_text("progress.detail", detail, true)?;
            }
            if progress.cancellable && progress.phase != UpdateProgressPhase::Downloading {
                return Err(StateValidationError::UpdateInvariant(
                    "only download progress may be cancellable",
                ));
            }
            let expected_unit = match progress.phase {
                UpdateProgressPhase::Refreshing | UpdateProgressPhase::Downloading => {
                    UpdateProgressUnit::Bytes
                }
                UpdateProgressPhase::Resolving => UpdateProgressUnit::Steps,
                UpdateProgressPhase::Installing | UpdateProgressPhase::Repairing => {
                    UpdateProgressUnit::Packages
                }
            };
            if progress.unit != expected_unit {
                return Err(StateValidationError::UpdateInvariant(
                    "update progress unit does not match its phase",
                ));
            }
        }
        if let Some(failure) = &self.failure {
            validate_text("failure", failure, true)?;
        }
        if let Some(log) = &self.last_log_id {
            validate_text("last_log_id", log, true)?;
            if !is_log_id(log) {
                return Err(StateValidationError::UpdateInvariant(
                    "last_log_id is not a safe updater log identifier",
                ));
            }
        }
        if let Some(revision) = &self.runtime_revision {
            validate_text("runtime_revision", revision, true)?;
            if !is_runtime_revision(revision) {
                return Err(StateValidationError::UpdateInvariant(
                    "runtime_revision is not a safe single path component",
                ));
            }
        }
        if self.runtime_ready != self.runtime_revision.is_some() {
            return Err(StateValidationError::UpdateInvariant(
                "runtime_ready requires exactly one validated runtime revision",
            ));
        }
        let has_checked = self.checked_at_unix_seconds.is_some();
        let has_plan = self.plan_generation.is_some();
        let has_packages = !self.packages.is_empty();
        match self.status {
            UpdateStatus::NeverChecked => {
                if has_checked
                    || has_plan
                    || has_packages
                    || !self.repository_errors.is_empty()
                    || self.download_size != 0
                    || self.progress.is_some()
                    || self.repair_available
                    || !self.restart_reasons.is_empty()
                {
                    return Err(StateValidationError::UpdateInvariant(
                        "never_checked must not carry check results",
                    ));
                }
            }
            UpdateStatus::Checking => {
                if has_checked
                    || has_plan
                    || has_packages
                    || !self.repository_errors.is_empty()
                    || self.download_size != 0
                    || !matches!(
                        self.progress.as_ref().map(|progress| progress.phase),
                        Some(UpdateProgressPhase::Refreshing | UpdateProgressPhase::Resolving)
                    )
                    || self.failure.is_some()
                    || self.repair_available
                    || !self.restart_reasons.is_empty()
                {
                    return Err(StateValidationError::UpdateInvariant(
                        "checking must not carry a completed check result",
                    ));
                }
            }
            UpdateStatus::UpToDate => {
                if !has_checked
                    || has_plan
                    || has_packages
                    || !self.repository_errors.is_empty()
                    || self.download_size != 0
                    || self.progress.is_some()
                    || self.failure.is_some()
                    || self.repair_available
                    || !self.restart_reasons.is_empty()
                {
                    return Err(StateValidationError::UpdateInvariant(
                        "up_to_date requires a check time and no plan, packages, errors, or download",
                    ));
                }
            }
            UpdateStatus::Available => {
                if !has_checked || !has_plan || !has_packages {
                    return Err(StateValidationError::UpdateInvariant(
                        "actionable update states require a check time, packages, and plan generation",
                    ));
                }
                if self.progress.is_some()
                    || self.failure.is_some()
                    || self.repair_available
                    || !self.repository_errors.is_empty()
                    || !self.restart_reasons.is_empty()
                {
                    return Err(StateValidationError::UpdateInvariant(
                        "available state carries operation-only fields",
                    ));
                }
            }
            UpdateStatus::Installing => {
                if !has_checked || !has_plan || !has_packages {
                    return Err(StateValidationError::UpdateInvariant(
                        "installing requires a check time, packages, and plan generation",
                    ));
                }
                if !matches!(
                    self.progress.as_ref().map(|progress| progress.phase),
                    Some(
                        UpdateProgressPhase::Downloading
                            | UpdateProgressPhase::Installing
                            | UpdateProgressPhase::Repairing
                    )
                ) || self.failure.is_some()
                    || self.repair_available
                    || !self.repository_errors.is_empty()
                    || !self.restart_reasons.is_empty()
                {
                    return Err(StateValidationError::UpdateInvariant(
                        "installing state carries incoherent operation fields",
                    ));
                }
            }
            UpdateStatus::RestartRecommended => {
                if !has_checked || !has_plan || !has_packages || self.restart_reasons.is_empty() {
                    return Err(StateValidationError::UpdateInvariant(
                        "restart_recommended requires the attempted plan and restart evidence",
                    ));
                }
                if self.progress.is_some()
                    || self.failure.is_some()
                    || self.repair_available
                    || !self.repository_errors.is_empty()
                {
                    return Err(StateValidationError::UpdateInvariant(
                        "restart_recommended carries operation-only fields",
                    ));
                }
            }
            UpdateStatus::Failed => {
                if self.repository_errors.is_empty() && self.failure.is_none() {
                    return Err(StateValidationError::UpdateInvariant(
                        "failed requires repository errors or a concrete failure",
                    ));
                }
                if (has_plan || has_packages) && !has_checked {
                    return Err(StateValidationError::UpdateInvariant(
                        "a failed checked plan requires a check time",
                    ));
                }
                if has_packages && !has_plan {
                    return Err(StateValidationError::UpdateInvariant(
                        "failed package details require an attempted plan",
                    ));
                }
                if self.progress.is_some() || !self.restart_reasons.is_empty() {
                    return Err(StateValidationError::UpdateInvariant(
                        "failed state must not retain active progress or restart evidence",
                    ));
                }
                if self.repair_available && !has_plan {
                    return Err(StateValidationError::UpdateInvariant(
                        "repair requires an updater-generated attempted plan",
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, UpdateStateError> {
        let bytes = read_bounded(path, MAX_MANAGED_JSON_BYTES)?;
        Self::from_json_bytes(&bytes)
    }

    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self, UpdateStateError> {
        if bytes.len() > MAX_MANAGED_JSON_BYTES {
            return Err(UpdateStateError::TooLarge {
                maximum: MAX_MANAGED_JSON_BYTES,
            });
        }
        let value: Value = serde_json::from_slice(bytes)?;
        let object = value.as_object().ok_or(UpdateStateError::NotObject)?;
        let version = update_schema_version(object)?;
        if version > UPDATE_STATE_SCHEMA_VERSION {
            return Err(UpdateStateError::NewerSchema {
                found: version,
                current: UPDATE_STATE_SCHEMA_VERSION,
            });
        }
        if version < UPDATE_STATE_SCHEMA_VERSION {
            return Err(UpdateStateError::OlderSchema {
                found: version,
                current: UPDATE_STATE_SCHEMA_VERSION,
            });
        }
        exact_update_keys(
            object,
            &[
                "schema_version",
                "state_generation",
                "status",
                "checked_at_unix_seconds",
                "repository_errors",
                "packages",
                "download_size",
                "plan_generation",
                "progress",
                "failure",
                "repair_available",
                "restart_reasons",
                "last_log_id",
                "runtime_revision",
                "runtime_ready",
            ],
        )?;
        let packages = value
            .get("packages")
            .and_then(Value::as_array)
            .ok_or_else(|| UpdateStateError::UnexpectedFields {
                expected: "packages must be an array".into(),
            })?;
        for package in packages {
            let package =
                package
                    .as_object()
                    .ok_or_else(|| UpdateStateError::UnexpectedFields {
                        expected: "each package must be an object".into(),
                    })?;
            exact_update_keys(
                package,
                &[
                    "name",
                    "installed_version",
                    "candidate_version",
                    "download_size",
                    "security_origin",
                    "action",
                ],
            )?;
        }
        // Deserialize from the original bytes as well as inspecting Value.
        // serde's struct decoder rejects duplicate fields, while Value alone
        // would silently retain the final duplicate key.
        let state: Self = serde_json::from_slice(bytes)?;
        state.validate()?;
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<(), UpdateStateError> {
        self.validate()?;
        atomic_write_json(path, self)?;
        Ok(())
    }
}

fn update_schema_version(object: &Map<String, Value>) -> Result<u32, UpdateStateError> {
    object
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(UpdateStateError::MissingSchemaVersion)
}

fn exact_update_keys(
    object: &Map<String, Value>,
    expected: &[&str],
) -> Result<(), UpdateStateError> {
    let actual: BTreeSet<_> = object.keys().map(String::as_str).collect();
    let expected_set: BTreeSet<_> = expected.iter().copied().collect();
    if actual == expected_set {
        Ok(())
    } else {
        Err(UpdateStateError::UnexpectedFields {
            expected: expected.join(", "),
        })
    }
}

fn validate_text(
    field: &'static str,
    text: &str,
    require_nonempty: bool,
) -> Result<(), StateValidationError> {
    if require_nonempty && text.is_empty() {
        return Err(StateValidationError::Empty { field });
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(StateValidationError::TextTooLong {
            field,
            maximum: MAX_TEXT_BYTES,
        });
    }
    if text.chars().any(char::is_control) {
        return Err(StateValidationError::ControlCharacter { field });
    }
    Ok(())
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_debian_package_name(value: &str) -> bool {
    value.len() <= 256
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b':' | b'~' | b'-')
        })
}

fn is_runtime_revision(value: &str) -> bool {
    value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'~' | b'-')
        })
}

fn is_log_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("attempt-") else {
        return false;
    };
    let Some((generation, suffix)) = rest.split_once('-') else {
        return false;
    };
    !generation.is_empty()
        && !generation.starts_with('0')
        && generation.bytes().all(|byte| byte.is_ascii_digit())
        && suffix
            .strip_suffix(".log")
            .is_some_and(|digest| is_lower_hex(digest, 16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_presets_are_exact_120ths() {
        assert_eq!(GuestScalePreset::Automatic.scale_120(), None);
        assert_eq!(GuestScalePreset::Percent125.scale_120(), Some(150));
        assert_eq!(GuestScalePreset::from_percent(133), None);
    }

    #[test]
    fn backgrounds_are_solid_typed_values_only() {
        assert_eq!(
            BackgroundChoice::DarkPlain.solid_color().to_string(),
            "#202225"
        );
        assert_eq!(
            BackgroundChoice::LightPlain.solid_color().to_string(),
            "#FAFAFA"
        );
        for hostile in [
            "red",
            "#fff",
            "#FFFFFFFF",
            "url(https://example.test/a)",
            "linear-gradient(red, blue)",
        ] {
            assert!(hostile.parse::<SolidColor>().is_err());
        }
        let custom: BackgroundChoice =
            serde_json::from_str(r##"{"kind":"custom_solid","color":"#c0ffee"}"##).unwrap();
        assert_eq!(
            serde_json::to_string(&custom).unwrap(),
            r##"{"kind":"custom_solid","color":"#C0FFEE"}"##
        );
    }

    #[test]
    fn geometry_rejects_stale_generation_and_transforms_both_directions() {
        let geometry = DisplayGeometry {
            physical_width: 1595,
            physical_height: 940,
            host_surface_scale_120: 160,
            guest_ui_scale_120: 150,
            logical_width: 1276,
            logical_height: 752,
            geometry_generation: 9,
        };
        geometry.validate().unwrap();
        assert!(matches!(
            geometry.physical_to_logical(8, 0.0, 0.0),
            Err(StateValidationError::StaleGeometry { .. })
        ));
        assert_eq!(
            geometry.physical_to_logical(9, 797.5, 470.0),
            Ok((638.0, 376.0))
        );
        assert_eq!(
            geometry.logical_to_physical(9, 638.0, 376.0),
            Ok((797.5, 470.0))
        );
    }

    #[test]
    fn fractional_geometry_uses_guest_scale_rounding_and_maps_edges_exactly() {
        let geometry = DisplayGeometry {
            physical_width: 1595,
            physical_height: 941,
            host_surface_scale_120: 213,
            guest_ui_scale_120: 160,
            logical_width: 1196,
            logical_height: 705,
            geometry_generation: 41,
        };
        geometry.validate().unwrap();
        assert_eq!(
            geometry.physical_to_logical(41, 1595.0, 941.0),
            Ok((1196.0, 705.0))
        );
        assert_eq!(
            geometry.logical_to_physical(41, 1196.0, 705.0),
            Ok((1595.0, 941.0))
        );
        assert!(matches!(
            geometry.physical_to_logical(41, 1595.0001, 0.0),
            Err(StateValidationError::CoordinateOutOfBounds { .. })
        ));
        assert!(matches!(
            geometry.logical_to_physical(41, f64::NAN, 0.0),
            Err(StateValidationError::NonFiniteCoordinate { .. })
        ));
    }

    #[test]
    fn incoherent_logical_extent_is_rejected_without_using_host_scale() {
        let geometry = DisplayGeometry {
            physical_width: 1595,
            physical_height: 941,
            host_surface_scale_120: 240,
            guest_ui_scale_120: 160,
            logical_width: 1197,
            logical_height: 705,
            geometry_generation: 1,
        };
        assert!(matches!(
            geometry.validate(),
            Err(StateValidationError::IncoherentGeometry {
                axis: "horizontal",
                expected: 1196,
                actual: 1197,
            })
        ));
    }

    #[test]
    fn actionable_update_state_requires_an_opaque_plan() {
        let state = UpdateState {
            status: UpdateStatus::Available,
            checked_at_unix_seconds: Some(1),
            packages: vec![UpdatePackage {
                name: "example".into(),
                installed_version: "1".into(),
                candidate_version: "2".into(),
                download_size: 10,
                security_origin: None,
                action: UpdateAction::Upgrade,
            }],
            download_size: 10,
            ..UpdateState::default()
        };
        assert!(matches!(
            state.validate(),
            Err(StateValidationError::UpdateInvariant(_))
        ));
    }

    fn available_update_state() -> UpdateState {
        UpdateState {
            schema_version: UPDATE_STATE_SCHEMA_VERSION,
            status: UpdateStatus::Available,
            checked_at_unix_seconds: Some(10),
            repository_errors: Vec::new(),
            packages: vec![
                UpdatePackage {
                    name: "alpha".into(),
                    installed_version: "1".into(),
                    candidate_version: "2".into(),
                    download_size: 20,
                    security_origin: Some("Debian-Security".into()),
                    action: UpdateAction::Upgrade,
                },
                UpdatePackage {
                    name: "beta".into(),
                    installed_version: "3".into(),
                    candidate_version: "4".into(),
                    download_size: 30,
                    security_origin: None,
                    action: UpdateAction::Upgrade,
                },
            ],
            download_size: 50,
            plan_generation: Some("a".repeat(64)),
            ..UpdateState::default()
        }
    }

    #[test]
    fn update_state_has_bounded_strict_validated_persistence() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        let state = available_update_state();
        state.save(&path).unwrap();
        assert_eq!(UpdateState::load(&path).unwrap(), state);

        let mut value = serde_json::to_value(&state).unwrap();
        value["packages"][0]
            .as_object_mut()
            .unwrap()
            .remove("security_origin");
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            UpdateState::load(&path),
            Err(UpdateStateError::UnexpectedFields { .. })
        ));

        std::fs::write(&path, vec![b' '; MAX_MANAGED_JSON_BYTES + 1]).unwrap();
        assert!(matches!(
            UpdateState::load(&path),
            Err(UpdateStateError::Persistence(
                PersistenceError::TooLarge { .. }
            ))
        ));
    }

    #[test]
    fn newer_update_schema_is_rejected_and_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        let original = b"{\"schema_version\":99,\"future\":true}\n";
        std::fs::write(&path, original).unwrap();
        assert!(matches!(
            UpdateState::load(&path),
            Err(UpdateStateError::NewerSchema { found: 99, .. })
        ));
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn update_state_rejects_duplicates_overflow_controls_and_bad_totals() {
        let mut duplicate = available_update_state();
        duplicate.packages[1].name = "alpha".into();
        assert!(matches!(
            duplicate.validate(),
            Err(StateValidationError::DuplicatePackage(name)) if name == "alpha"
        ));

        let mut overflow = available_update_state();
        overflow.packages[0].download_size = u64::MAX;
        overflow.packages[1].download_size = 1;
        assert!(matches!(
            overflow.validate(),
            Err(StateValidationError::DownloadSizeOverflow)
        ));

        let mut control = available_update_state();
        control.packages[0].candidate_version = "2\nmalicious".into();
        assert!(matches!(
            control.validate(),
            Err(StateValidationError::ControlCharacter {
                field: "package.candidate_version"
            })
        ));

        let mut plan_control = available_update_state();
        plan_control.plan_generation = Some("opaque\rreplacement".into());
        assert!(matches!(
            plan_control.validate(),
            Err(StateValidationError::ControlCharacter {
                field: "plan_generation"
            })
        ));

        let mut wrong_total = available_update_state();
        wrong_total.download_size = 49;
        assert!(matches!(
            wrong_total.validate(),
            Err(StateValidationError::UpdateInvariant(_))
        ));

        let wrong_progress_unit = UpdateState {
            status: UpdateStatus::Checking,
            progress: Some(UpdateProgress {
                phase: UpdateProgressPhase::Refreshing,
                completed: 1,
                total: 2,
                unit: UpdateProgressUnit::Packages,
                detail: None,
                cancellable: false,
            }),
            ..UpdateState::default()
        };
        assert!(matches!(
            wrong_progress_unit.validate(),
            Err(StateValidationError::UpdateInvariant(
                "update progress unit does not match its phase"
            ))
        ));
    }

    #[test]
    fn update_wire_rejects_duplicate_fields_and_oversized_dbus_payloads() {
        let state = available_update_state();
        let mut json = serde_json::to_string(&state).unwrap();
        json = json.replacen(
            "{\"schema_version\":2,",
            "{\"schema_version\":2,\"schema_version\":2,",
            1,
        );
        assert!(matches!(
            UpdateState::from_json_bytes(json.as_bytes()),
            Err(UpdateStateError::Schema(_))
        ));
        let oversized = vec![b' '; MAX_MANAGED_JSON_BYTES + 1];
        assert!(matches!(
            UpdateState::from_json_bytes(&oversized),
            Err(UpdateStateError::TooLarge { .. })
        ));
    }

    #[test]
    fn update_status_requires_coherent_time_plan_and_evidence() {
        let mut up_to_date = UpdateState {
            status: UpdateStatus::UpToDate,
            ..UpdateState::default()
        };
        assert!(matches!(
            up_to_date.validate(),
            Err(StateValidationError::UpdateInvariant(_))
        ));
        up_to_date.checked_at_unix_seconds = Some(1);
        up_to_date.validate().unwrap();

        let failed_without_evidence = UpdateState {
            status: UpdateStatus::Failed,
            checked_at_unix_seconds: Some(1),
            ..UpdateState::default()
        };
        assert!(matches!(
            failed_without_evidence.validate(),
            Err(StateValidationError::UpdateInvariant(_))
        ));

        let checking_with_old_time = UpdateState {
            status: UpdateStatus::Checking,
            checked_at_unix_seconds: Some(1),
            ..UpdateState::default()
        };
        assert!(matches!(
            checking_with_old_time.validate(),
            Err(StateValidationError::UpdateInvariant(_))
        ));
    }
}
