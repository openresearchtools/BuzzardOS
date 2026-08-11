// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;
use wildbuzzard_desktop_core::{
    BackgroundChoice, DisplayGeometry, GuestScalePreset, KeyboardSettings, Settings,
    ThemeConfigSet, ThemeMode, UpdateState, XdgPaths, apply_theme_files, effective_user_id,
};

pub const OUTPUT_STATE_PATH: &str = "/run/wildbuzzard-display-state/output-state.json";
pub const UPDATE_STATE_PATH: &str = "/var/lib/wildbuzzard-updater/state.json";
const MAX_RUNTIME_STATE_BYTES: usize = 1024 * 1024;
const MAX_SCALE_MESSAGE_BYTES: usize = 4096;
const MAX_KEYBOARD_MESSAGE_BYTES: usize = 4096;
const MAX_ACTIVE_LAYOUT_NAME_BYTES: usize = 256;
const SCALE_SOCKET_NAME: &str = "wildbuzzard-display-scale.sock";
const KEYBOARD_SOCKET_NAME: &str = "wildbuzzard-keyboard-settings.sock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageId {
    Display,
    Sound,
    Keyboard,
    Appearance,
    Updates,
}

impl PageId {
    pub const ALL: [Self; 5] = [
        Self::Display,
        Self::Sound,
        Self::Keyboard,
        Self::Appearance,
        Self::Updates,
    ];

    pub const fn stack_name(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Sound => "sound",
            Self::Keyboard => "keyboard",
            Self::Appearance => "appearance",
            Self::Updates => "updates",
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Display => "Display",
            Self::Sound => "Sound",
            Self::Keyboard => "Keyboard",
            Self::Appearance => "Appearance",
            Self::Updates => "Updates",
        }
    }

    pub const fn icon_name(self) -> &'static str {
        match self {
            Self::Display => "video-display-symbolic",
            Self::Sound => "audio-volume-high-symbolic",
            Self::Keyboard => "input-keyboard-symbolic",
            Self::Appearance => "preferences-desktop-theme-symbolic",
            Self::Updates => "software-update-available-symbolic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeSection {
    Appearance,
    Display,
    Keyboard,
}

impl ChangeSection {
    pub const fn bus_name(self) -> &'static str {
        match self {
            Self::Appearance => "appearance",
            Self::Display => "display",
            Self::Keyboard => "keyboard",
        }
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cannot resolve persistent guest directories: {0}")]
    Paths(String),
    #[error("settings are read-only because the existing file could not be loaded: {0}")]
    ReadOnly(String),
    #[error("cannot write settings: {0}")]
    Settings(String),
    #[error("cannot project the selected theme: {0}")]
    Theme(String),
}

#[derive(Debug, Clone)]
pub struct SettingsStore {
    pub paths: XdgPaths,
    pub settings: Settings,
    pub diagnostic: Option<String>,
    pub writable: bool,
}

impl SettingsStore {
    pub fn discover() -> Result<Self, StoreError> {
        let paths = XdgPaths::discover().map_err(|error| StoreError::Paths(error.to_string()))?;
        Self::open(paths)
    }

    pub fn open(paths: XdgPaths) -> Result<Self, StoreError> {
        paths
            .ensure_private_directories()
            .map_err(|error| StoreError::Paths(error.to_string()))?;
        let path = paths.settings_path();
        if !path.exists() {
            return Ok(Self {
                paths,
                settings: Settings::default(),
                diagnostic: None,
                writable: true,
            });
        }
        match Settings::load_and_migrate(&path) {
            Ok(outcome) => Ok(Self {
                paths,
                settings: outcome.value,
                diagnostic: outcome
                    .migrated_from
                    .map(|version| format!("Settings schema {version} was migrated safely.")),
                writable: true,
            }),
            Err(error) => Ok(Self {
                paths,
                settings: Settings::default(),
                diagnostic: Some(format!(
                    "The existing settings file was preserved and controls are read-only: {error}"
                )),
                writable: false,
            }),
        }
    }

    pub fn set_theme(&mut self, mode: ThemeMode) -> Result<u64, StoreError> {
        self.ensure_writable()?;
        if self.settings.appearance.theme == mode {
            return Ok(self.settings.generation);
        }
        let old = self.settings.clone();
        let next_generation = old
            .generation
            .checked_add(1)
            .ok_or_else(|| StoreError::Settings("settings generation overflow".into()))?;
        let projection = ThemeConfigSet::for_mode(mode);
        apply_theme_files(&self.paths.config_home, &projection)
            .map_err(|error| StoreError::Theme(error.to_string()))?;

        let mut candidate = old.clone();
        candidate.appearance.theme = mode;
        candidate.generation = next_generation;
        if let Err(error) = candidate.save(&self.paths.settings_path()) {
            // Restore the previous projection best-effort. The returned error
            // remains truthful even if this recovery also fails.
            let _ = apply_theme_files(
                &self.paths.config_home,
                &ThemeConfigSet::for_mode(old.appearance.theme),
            );
            return Err(StoreError::Settings(error.to_string()));
        }
        self.settings = candidate;
        Ok(self.settings.generation)
    }

    pub fn set_background(&mut self, choice: BackgroundChoice) -> Result<u64, StoreError> {
        self.ensure_writable()?;
        if self.settings.appearance.background == choice {
            return Ok(self.settings.generation);
        }
        let mut candidate = self.settings.clone();
        candidate.appearance.background = choice;
        candidate.generation = candidate
            .generation
            .checked_add(1)
            .ok_or_else(|| StoreError::Settings("settings generation overflow".into()))?;
        candidate
            .save(&self.paths.settings_path())
            .map_err(|error| StoreError::Settings(error.to_string()))?;
        self.settings = candidate;
        Ok(self.settings.generation)
    }

    /// Persist only after the display runtime confirms its independent
    /// geometry generation. Settings generation is a separate monotonic
    /// sequence and must never be assigned from the coordinate epoch.
    pub fn persist_confirmed_display_scale(
        &mut self,
        preset: GuestScalePreset,
    ) -> Result<u64, StoreError> {
        self.ensure_writable()?;
        let mut candidate = self.settings.clone();
        candidate.display.guest_ui_scale = preset;
        candidate.generation = candidate
            .generation
            .checked_add(1)
            .ok_or_else(|| StoreError::Settings("settings generation overflow".into()))?;
        candidate
            .save(&self.paths.settings_path())
            .map_err(|error| StoreError::Settings(error.to_string()))?;
        self.settings = candidate;
        Ok(self.settings.generation)
    }

    /// Persist only after the private keyboard service confirms that stock
    /// Sway compiled and installed the requested XKB keymap.
    pub fn persist_confirmed_keyboard(
        &mut self,
        keyboard: KeyboardSettings,
    ) -> Result<u64, StoreError> {
        self.ensure_writable()?;
        keyboard
            .validate()
            .map_err(|error| StoreError::Settings(error.to_string()))?;
        if self.settings.keyboard == keyboard {
            return Ok(self.settings.generation);
        }
        let mut candidate = self.settings.clone();
        candidate.keyboard = keyboard;
        candidate.generation = candidate
            .generation
            .checked_add(1)
            .ok_or_else(|| StoreError::Settings("settings generation overflow".into()))?;
        candidate
            .save(&self.paths.settings_path())
            .map_err(|error| StoreError::Settings(error.to_string()))?;
        self.settings = candidate;
        Ok(self.settings.generation)
    }

    fn ensure_writable(&self) -> Result<(), StoreError> {
        if self.writable {
            Ok(())
        } else {
            Err(StoreError::ReadOnly(
                self.diagnostic
                    .clone()
                    .unwrap_or_else(|| "unknown load error".into()),
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeGeometryView {
    pub physical_width: Option<u32>,
    pub physical_height: Option<u32>,
    pub logical_width: Option<u32>,
    pub logical_height: Option<u32>,
    pub host_scale_120: Option<u32>,
    pub guest_scale_120: Option<u32>,
    pub geometry_generation: Option<u64>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputStateV7 {
    schema: u32,
    physical_width: u32,
    physical_height: u32,
    host_surface_scale_120: u32,
    guest_ui_scale_120: u32,
    logical_width: u32,
    logical_height: u32,
    geometry_generation: u64,
}

impl RuntimeGeometryView {
    pub fn geometry(&self) -> Option<DisplayGeometry> {
        Some(DisplayGeometry {
            physical_width: self.physical_width?,
            physical_height: self.physical_height?,
            host_surface_scale_120: self.host_scale_120?,
            guest_ui_scale_120: self.guest_scale_120?,
            logical_width: self.logical_width?,
            logical_height: self.logical_height?,
            geometry_generation: self.geometry_generation?,
        })
    }
}

pub fn load_runtime_geometry(path: &Path) -> RuntimeGeometryView {
    let bytes = match read_runtime_state(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return RuntimeGeometryView {
                diagnostic: Some(format!("Runtime geometry is unavailable: {error}")),
                ..RuntimeGeometryView::default()
            };
        }
    };
    let state: OutputStateV7 = match serde_json::from_slice(&bytes) {
        Ok(state) => state,
        Err(error) => {
            return RuntimeGeometryView {
                diagnostic: Some(format!("Runtime geometry is invalid: {error}")),
                ..RuntimeGeometryView::default()
            };
        }
    };
    if state.schema != 7 {
        return RuntimeGeometryView {
            diagnostic: Some(format!(
                "Runtime geometry uses unsupported schema {}; expected schema 7.",
                state.schema
            )),
            ..RuntimeGeometryView::default()
        };
    }
    let geometry = RuntimeGeometryView {
        physical_width: Some(state.physical_width),
        physical_height: Some(state.physical_height),
        logical_width: Some(state.logical_width),
        logical_height: Some(state.logical_height),
        host_scale_120: Some(state.host_surface_scale_120),
        guest_scale_120: Some(state.guest_ui_scale_120),
        geometry_generation: Some(state.geometry_generation),
        diagnostic: None,
    };
    if let Some(validated) = geometry.geometry() {
        match validated.validate() {
            Ok(()) => geometry,
            Err(error) => RuntimeGeometryView {
                diagnostic: Some(format!("Runtime geometry is incoherent: {error}")),
                ..geometry
            },
        }
    } else {
        unreachable!("required geometry fields were checked above")
    }
}

fn read_runtime_state(path: &Path) -> Result<Vec<u8>, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    let process_uid = effective_user_id();
    if !metadata.is_file() {
        return Err("output-state is not a regular file".into());
    }
    if metadata.uid() != process_uid {
        return Err("output-state is not owned by the interactive guest user".into());
    }
    if metadata.mode() & 0o022 != 0 {
        return Err("output-state is writable by the group or other users".into());
    }
    if metadata.len() > MAX_RUNTIME_STATE_BYTES as u64 {
        return Err(format!(
            "output-state exceeds the {}-byte limit",
            MAX_RUNTIME_STATE_BYTES
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file)
        .take((MAX_RUNTIME_STATE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_RUNTIME_STATE_BYTES {
        return Err(format!(
            "output-state exceeds the {}-byte limit",
            MAX_RUNTIME_STATE_BYTES
        ));
    }
    Ok(bytes)
}

#[derive(Debug, Error)]
pub enum ScaleServiceError {
    #[error("display scale service is unavailable: {0}")]
    Unavailable(String),
    #[error("display scale service protocol failed: {0}")]
    Protocol(String),
    #[error("display scale request was rejected ({code}): {message}")]
    Rejected {
        code: String,
        message: String,
        current_geometry: Option<DisplayGeometry>,
    },
}

#[derive(Debug, Serialize)]
struct ScaleRequest {
    schema: u32,
    method: &'static str,
    preset: GuestScalePreset,
    current_geometry_generation: u64,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ScaleResponse {
    Success(ScaleSuccessResponse),
    Rejected(ScaleRejectedResponse),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScaleSuccessResponse {
    schema: u32,
    ok: bool,
    preset: GuestScalePreset,
    geometry: DisplayGeometry,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScaleRejectedResponse {
    schema: u32,
    ok: bool,
    error: ScaleResponseError,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScaleResponseError {
    code: String,
    message: String,
    current_geometry: Option<DisplayGeometry>,
}

pub fn display_scale_socket_path() -> Result<PathBuf, ScaleServiceError> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| ScaleServiceError::Unavailable("XDG_RUNTIME_DIR is not set".into()))?;
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        return Err(ScaleServiceError::Unavailable(
            "XDG_RUNTIME_DIR is not absolute".into(),
        ));
    }
    Ok(runtime.join(SCALE_SOCKET_NAME))
}

pub fn validate_display_scale_socket(path: &Path) -> Result<(), ScaleServiceError> {
    validate_owner_only_socket(path).map_err(ScaleServiceError::Unavailable)
}

fn validate_owner_only_socket(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_socket() {
        return Err(format!("{} is not a Unix socket", path.display()));
    }
    if metadata.mode() & 0o777 != 0o600 {
        return Err(format!("{} must have mode 0600", path.display()));
    }
    let process_uid = effective_user_id();
    if metadata.uid() != process_uid {
        return Err(format!(
            "{} is not owned by the interactive guest user",
            path.display()
        ));
    }
    Ok(())
}

pub fn set_guest_scale(
    socket_path: &Path,
    preset: GuestScalePreset,
    current_geometry_generation: u64,
) -> Result<DisplayGeometry, ScaleServiceError> {
    validate_display_scale_socket(socket_path)?;
    let mut request = serde_json::to_vec(&ScaleRequest {
        schema: 1,
        method: "SetGuestScale",
        preset,
        current_geometry_generation,
    })
    .map_err(|error| ScaleServiceError::Protocol(error.to_string()))?;
    request.push(b'\n');
    if request.len() > MAX_SCALE_MESSAGE_BYTES {
        return Err(ScaleServiceError::Protocol(
            "scale request exceeds 4096 bytes".into(),
        ));
    }

    let mut stream = UnixStream::connect(socket_path)
        .map_err(|error| ScaleServiceError::Unavailable(error.to_string()))?;
    let read_timeout = Some(Duration::from_secs(10));
    let write_timeout = Some(Duration::from_secs(2));
    stream
        .set_read_timeout(read_timeout)
        .and_then(|()| stream.set_write_timeout(write_timeout))
        .map_err(|error| ScaleServiceError::Unavailable(error.to_string()))?;
    stream
        .write_all(&request)
        .and_then(|()| stream.flush())
        .map_err(|error| ScaleServiceError::Unavailable(error.to_string()))?;

    let mut response = Vec::new();
    BufReader::new(stream)
        .take((MAX_SCALE_MESSAGE_BYTES + 1) as u64)
        .read_until(b'\n', &mut response)
        .map_err(|error| ScaleServiceError::Protocol(error.to_string()))?;
    if response.len() > MAX_SCALE_MESSAGE_BYTES {
        return Err(ScaleServiceError::Protocol(
            "scale response exceeds 4096 bytes".into(),
        ));
    }
    if response.last() != Some(&b'\n') {
        return Err(ScaleServiceError::Protocol(
            "scale response is not newline terminated".into(),
        ));
    }
    response.pop();
    let response: ScaleResponse = serde_json::from_slice(&response)
        .map_err(|error| ScaleServiceError::Protocol(error.to_string()))?;
    match response {
        ScaleResponse::Success(response) => {
            if response.schema != 1 {
                return Err(ScaleServiceError::Protocol(format!(
                    "unsupported scale response schema {}",
                    response.schema
                )));
            }
            if !response.ok {
                return Err(ScaleServiceError::Protocol(
                    "success response has ok=false".into(),
                ));
            }
            if response.preset != preset {
                return Err(ScaleServiceError::Protocol(
                    "scale response preset does not match the request".into(),
                ));
            }
            response
                .geometry
                .validate()
                .map_err(|error| ScaleServiceError::Protocol(error.to_string()))?;
            Ok(response.geometry)
        }
        ScaleResponse::Rejected(response) => {
            if response.schema != 1 {
                return Err(ScaleServiceError::Protocol(format!(
                    "unsupported scale response schema {}",
                    response.schema
                )));
            }
            if response.ok {
                return Err(ScaleServiceError::Protocol(
                    "error response has ok=true".into(),
                ));
            }
            if let Some(geometry) = &response.error.current_geometry {
                geometry
                    .validate()
                    .map_err(|error| ScaleServiceError::Protocol(error.to_string()))?;
            }
            Err(ScaleServiceError::Rejected {
                code: response.error.code,
                message: response.error.message,
                current_geometry: response.error.current_geometry,
            })
        }
    }
}

#[derive(Debug, Error)]
pub enum KeyboardServiceError {
    #[error("keyboard settings service is unavailable: {0}")]
    Unavailable(String),
    #[error("keyboard settings service protocol failed: {0}")]
    Protocol(String),
    #[error("keyboard settings request was rejected ({code}): {message}")]
    Rejected { code: String, message: String },
}

#[derive(Debug, Serialize)]
struct KeyboardRequest<'a> {
    schema: u32,
    method: &'static str,
    keyboard: &'a KeyboardSettings,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum KeyboardResponse {
    Success(KeyboardSuccessResponse),
    Rejected(KeyboardRejectedResponse),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyboardSuccessResponse {
    schema: u32,
    ok: bool,
    keyboard: KeyboardSettings,
    active_layout_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyboardRejectedResponse {
    schema: u32,
    ok: bool,
    error: KeyboardResponseError,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyboardResponseError {
    code: String,
    message: String,
}

pub fn keyboard_settings_socket_path() -> Result<PathBuf, KeyboardServiceError> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| KeyboardServiceError::Unavailable("XDG_RUNTIME_DIR is not set".into()))?;
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        return Err(KeyboardServiceError::Unavailable(
            "XDG_RUNTIME_DIR is not absolute".into(),
        ));
    }
    Ok(runtime.join(KEYBOARD_SOCKET_NAME))
}

pub fn validate_keyboard_settings_socket(path: &Path) -> Result<(), KeyboardServiceError> {
    validate_owner_only_socket(path).map_err(KeyboardServiceError::Unavailable)
}

pub fn set_guest_keyboard(
    socket_path: &Path,
    keyboard: &KeyboardSettings,
) -> Result<String, KeyboardServiceError> {
    keyboard
        .validate()
        .map_err(|error| KeyboardServiceError::Protocol(error.to_string()))?;
    validate_keyboard_settings_socket(socket_path)?;
    let mut request = serde_json::to_vec(&KeyboardRequest {
        schema: 1,
        method: "SetGuestKeyboard",
        keyboard,
    })
    .map_err(|error| KeyboardServiceError::Protocol(error.to_string()))?;
    request.push(b'\n');
    if request.len() > MAX_KEYBOARD_MESSAGE_BYTES {
        return Err(KeyboardServiceError::Protocol(
            "keyboard request exceeds 4096 bytes".into(),
        ));
    }

    let mut stream = UnixStream::connect(socket_path)
        .map_err(|error| KeyboardServiceError::Unavailable(error.to_string()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(2))))
        .map_err(|error| KeyboardServiceError::Unavailable(error.to_string()))?;
    stream
        .write_all(&request)
        .and_then(|()| stream.flush())
        .map_err(|error| KeyboardServiceError::Unavailable(error.to_string()))?;
    let mut response = Vec::new();
    BufReader::new(stream)
        .take((MAX_KEYBOARD_MESSAGE_BYTES + 1) as u64)
        .read_until(b'\n', &mut response)
        .map_err(|error| KeyboardServiceError::Protocol(error.to_string()))?;
    if response.len() > MAX_KEYBOARD_MESSAGE_BYTES || response.last() != Some(&b'\n') {
        return Err(KeyboardServiceError::Protocol(
            "keyboard response is missing a bounded newline-terminated message".into(),
        ));
    }
    response.pop();
    match serde_json::from_slice::<KeyboardResponse>(&response)
        .map_err(|error| KeyboardServiceError::Protocol(error.to_string()))?
    {
        KeyboardResponse::Success(response) => {
            if response.schema != 1 || !response.ok || response.keyboard != *keyboard {
                return Err(KeyboardServiceError::Protocol(
                    "keyboard success response does not match the request".into(),
                ));
            }
            if response.active_layout_name.is_empty()
                || response.active_layout_name.len() > MAX_ACTIVE_LAYOUT_NAME_BYTES
                || response.active_layout_name.chars().any(char::is_control)
            {
                return Err(KeyboardServiceError::Protocol(
                    "keyboard response contains an invalid active layout name".into(),
                ));
            }
            Ok(response.active_layout_name)
        }
        KeyboardResponse::Rejected(response) => {
            if response.schema != 1 || response.ok {
                return Err(KeyboardServiceError::Protocol(
                    "keyboard rejection response is malformed".into(),
                ));
            }
            Err(KeyboardServiceError::Rejected {
                code: response.error.code,
                message: response.error.message,
            })
        }
    }
}

#[derive(Debug, Clone)]
pub struct UpdateView {
    pub state: UpdateState,
}

pub fn load_update_view(path: &Path) -> UpdateView {
    let state = if path.exists() {
        UpdateState::load(path).unwrap_or_default()
    } else {
        UpdateState::default()
    };
    UpdateView { state }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use wildbuzzard_desktop_core::{SolidColor, UpdateStatus};

    fn xdg(root: &Path) -> XdgPaths {
        XdgPaths::from_bases(
            root.join("home"),
            root.join("config"),
            root.join("data"),
            root.join("state"),
            vec![root.join("share")],
            root.join("Desktop"),
        )
        .unwrap()
    }

    #[test]
    fn page_contract_is_complete_and_stable() {
        assert_eq!(PageId::ALL.len(), 5);
        assert_eq!(PageId::ALL[0].stack_name(), "display");
        assert_eq!(PageId::ALL[2].title(), "Keyboard");
        assert_eq!(PageId::ALL[4].title(), "Updates");
        assert!(PageId::ALL.iter().all(|page| !page.icon_name().is_empty()));
    }

    #[test]
    fn settings_persist_immediately_and_keep_background_independent() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = SettingsStore::open(xdg(temp.path())).unwrap();
        assert_eq!(store.set_theme(ThemeMode::Light).unwrap(), 1);
        assert_eq!(
            store
                .set_background(BackgroundChoice::CustomSolid {
                    color: SolidColor::new(1, 2, 3),
                })
                .unwrap(),
            2
        );
        let saved = Settings::load(&store.paths.settings_path()).unwrap().value;
        assert_eq!(saved.appearance.theme, ThemeMode::Light);
        assert_eq!(
            saved.appearance.background.solid_color().to_string(),
            "#010203"
        );
        assert_eq!(saved.generation, 2);
    }

    #[test]
    fn invalid_newer_settings_are_preserved_and_ui_becomes_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let paths = xdg(temp.path());
        fs::create_dir_all(paths.settings_path().parent().unwrap()).unwrap();
        let original = b"{\"schema_version\":999,\"future\":true}\n";
        fs::write(paths.settings_path(), original).unwrap();
        let mut store = SettingsStore::open(paths.clone()).unwrap();
        assert!(!store.writable);
        assert!(store.set_theme(ThemeMode::Light).is_err());
        assert_eq!(fs::read(paths.settings_path()).unwrap(), original);
    }

    #[test]
    fn display_preference_uses_an_independent_settings_generation() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = SettingsStore::open(xdg(temp.path())).unwrap();
        store.settings.generation = 40;
        assert_eq!(
            store
                .persist_confirmed_display_scale(GuestScalePreset::Percent150)
                .unwrap(),
            41
        );
    }

    #[test]
    fn confirmed_keyboard_preference_uses_an_independent_settings_generation() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = SettingsStore::open(xdg(temp.path())).unwrap();
        store.settings.generation = 40;
        let keyboard = KeyboardSettings {
            model: "pc105".into(),
            layout: "gb".into(),
            variant: String::new(),
            options: "compose:ralt".into(),
        };
        assert_eq!(
            store.persist_confirmed_keyboard(keyboard.clone()).unwrap(),
            41
        );
        assert_eq!(store.settings.keyboard, keyboard);
    }

    #[test]
    fn every_settings_mutation_fails_without_side_effects_on_generation_overflow() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = SettingsStore::open(xdg(temp.path())).unwrap();
        store.settings.generation = u64::MAX;
        let before = store.settings.clone();

        assert!(store.set_theme(ThemeMode::Light).is_err());
        assert_eq!(store.settings, before);
        assert!(!store.paths.settings_path().exists());
        assert!(store.set_background(BackgroundChoice::LightPlain).is_err());
        assert_eq!(store.settings, before);
        assert!(!store.paths.settings_path().exists());
        assert!(
            store
                .persist_confirmed_display_scale(GuestScalePreset::Percent150)
                .is_err()
        );
        assert!(
            store
                .persist_confirmed_keyboard(KeyboardSettings {
                    layout: "gb".into(),
                    ..KeyboardSettings::default()
                })
                .is_err()
        );
        assert_eq!(store.settings, before);
        assert!(!store.paths.settings_path().exists());
    }

    #[test]
    fn geometry_inspection_uses_the_split_runtime_scale_contract() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output.json");
        fs::write(
            &path,
            br#"{"schema":7,"physical_width":1600,"physical_height":1000,"logical_width":1200,"logical_height":750,"host_surface_scale_120":160,"guest_ui_scale_120":160,"geometry_generation":9}"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let geometry = load_runtime_geometry(&path);
        assert_eq!(geometry.physical_width, Some(1600));
        assert_eq!(geometry.guest_scale_120, Some(160));
        assert_eq!(geometry.geometry_generation, Some(9));
        assert_eq!(geometry.diagnostic, None);
    }

    #[test]
    fn geometry_inspection_rejects_malformed_extra_and_wrong_schema_state() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output.json");
        for contents in [
            br#"{"schema":7,"physical_width":1600"#.as_slice(),
            br#"{"schema":7,"physical_width":1600,"physical_height":1000,"host_surface_scale_120":160,"guest_ui_scale_120":160,"logical_width":1200,"logical_height":750,"geometry_generation":9,"legacy_scale":2}"#.as_slice(),
            br#"{"schema":6,"physical_width":1600,"physical_height":1000,"host_surface_scale_120":160,"guest_ui_scale_120":160,"logical_width":1200,"logical_height":750,"geometry_generation":9}"#.as_slice(),
        ] {
            fs::write(&path, contents).unwrap();
            let geometry = load_runtime_geometry(&path);
            assert!(geometry.diagnostic.is_some());
            assert!(geometry.geometry().is_none());
        }
    }

    #[test]
    fn geometry_inspection_rejects_symlinks_and_group_writable_state() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real.json");
        let link = temp.path().join("link.json");
        fs::write(
            &real,
            br#"{"schema":7,"physical_width":1600,"physical_height":1000,"host_surface_scale_120":160,"guest_ui_scale_120":160,"logical_width":1200,"logical_height":750,"geometry_generation":9}"#,
        )
        .unwrap();
        symlink(&real, &link).unwrap();
        assert!(load_runtime_geometry(&link).geometry().is_none());

        fs::set_permissions(&real, fs::Permissions::from_mode(0o660)).unwrap();
        let geometry = load_runtime_geometry(&real);
        assert!(geometry.geometry().is_none());
        assert!(geometry.diagnostic.unwrap().contains("writable"));
    }

    #[test]
    fn scale_socket_is_owner_only_and_response_is_validated() {
        use std::io::{BufRead as _, Write as _};
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("scale.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            assert!(request.contains(r#""method":"SetGuestScale""#));
            assert!(request.contains(r#""current_geometry_generation":7"#));
            // The display service waits for both the native host reply and a
            // confirmed Sway commit. This delay proves Settings does not use
            // the former two-second timeout and race a later runtime success.
            std::thread::sleep(Duration::from_millis(2_200));
            stream
                .write_all(br#"{"schema":1,"ok":true,"preset":"125","geometry":{"physical_width":1600,"physical_height":1000,"host_surface_scale_120":160,"guest_ui_scale_120":150,"logical_width":1280,"logical_height":800,"geometry_generation":8}}"#)
                .unwrap();
            stream.write_all(b"\n").unwrap();
        });

        let geometry = set_guest_scale(&socket, GuestScalePreset::Percent125, 7).unwrap();
        assert_eq!(geometry.geometry_generation, 8);
        assert_eq!(geometry.physical_width, 1600);
        server.join().unwrap();
    }

    #[test]
    fn absent_updater_is_not_reported_as_ready() {
        let temp = tempfile::tempdir().unwrap();
        let view = load_update_view(&temp.path().join("missing.json"));
        assert_eq!(view.state.status, UpdateStatus::NeverChecked);
        assert!(!view.state.runtime_ready);
    }
}
