// SPDX-License-Identifier: AGPL-3.0-or-later

//! Shared, non-UI guest desktop contracts.
//!
//! This crate owns the persistent schemas, path rules, desktop-entry model,
//! application discovery, and atomic file writes used by later guest desktop
//! components. It deliberately contains no GTK widgets, shell surfaces,
//! updater service, or AppImage execution code.

pub mod appimage;
pub mod desktop_entry;
pub mod persistence;
pub mod settings;
pub mod state;
pub mod xdg;

pub use appimage::{
    APPIMAGE_REGISTRATION_SCHEMA_VERSION, AppImageIcon, AppImageRegistration, FileObservation,
    RegistrationId,
};
pub use desktop_entry::{
    ApplicationCatalog, DesktopApplication, DesktopEntryDiagnostic, DesktopEntryId,
    GeneratedAppImageDesktopEntry, discover_applications,
};
pub use persistence::{LoadOutcome, atomic_write, atomic_write_json, read_bounded};
pub use settings::{SETTINGS_SCHEMA_VERSION, Settings};
pub use state::{
    BackgroundChoice, DisplayGeometry, GuestScalePreset, SolidColor, StateValidationError,
    ThemeMode, UPDATE_STATE_SCHEMA_VERSION, UpdatePackage, UpdateState, UpdateStateError,
    UpdateStatus,
};
pub use xdg::XdgPaths;
