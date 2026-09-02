// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared, non-UI guest desktop contracts.
//!
//! This crate owns the persistent schemas, path rules, desktop-entry model,
//! application discovery, and atomic file writes used by later guest desktop
//! components. It deliberately contains no GTK widgets, shell surfaces,
//! updater service, or AppImage execution code.

pub mod appimage;
#[cfg(feature = "xdg-discovery")]
pub mod desktop_entry;
pub mod desktop_files;
pub mod persistence;
pub mod settings;
pub mod state;
pub mod theme;
#[cfg(feature = "xdg-discovery")]
pub mod xdg;

pub use appimage::{
    APPIMAGE_REGISTRATION_SCHEMA_VERSION, AppImageIcon, AppImageRegistration, FileObservation,
    RegistrationId,
};
#[cfg(feature = "xdg-discovery")]
pub use desktop_entry::{
    ApplicationCatalog, DesktopApplication, DesktopEntryDiagnostic, DesktopEntryId,
    GeneratedAppImageDesktopEntry, discover_applications,
};
pub use desktop_files::{
    CollisionChoice, DESKTOP_LAYOUT_SCHEMA_VERSION, DeleteConsequence, DesktopDirectory,
    DesktopFileError, DesktopItem, DesktopItemKind, DesktopLayout, DesktopPosition, FileIdentity,
    PasteResult,
};
pub use persistence::{
    LoadOutcome, atomic_write, atomic_write_json, effective_user_id, read_bounded,
};
pub use settings::{KeyboardSettings, SETTINGS_SCHEMA_VERSION, Settings};
pub use state::{
    BackgroundChoice, DARK_WALLPAPER, DisplayGeometry, GuestScalePreset, LIGHT_WALLPAPER,
    SolidColor, StateValidationError, ThemeMode, UPDATE_STATE_SCHEMA_VERSION, UpdateAction,
    UpdatePackage, UpdateProgress, UpdateProgressPhase, UpdateProgressUnit, UpdateState,
    UpdateStateError, UpdateStatus,
};
pub use theme::{
    AppliedThemeFiles, ThemeApplyError, ThemeConfigSet, ThemePalette, apply_theme_files,
};
#[cfg(feature = "xdg-discovery")]
pub use xdg::XdgPaths;
