// SPDX-License-Identifier: AGPL-3.0-or-later

//! Safe AppImage registration and desktop-operation service used by the
//! guest shell and Settings application.

pub mod extracted;
pub mod inspector;
pub mod store;
pub mod thunar;

#[cfg(feature = "chooser")]
pub mod chooser;

#[cfg(feature = "chooser")]
pub use chooser::{ChooserError, RelinkOutcome, choose_relink, launch_with_relink};
pub use extracted::{extract_and_launch, extracted_path, launch_path, launch_validated};
pub use inspector::{
    InspectedAppImage, InspectedIcon, InspectionError, ValidatedAppImage, validate_appimage,
};
pub use store::{
    LaunchResult, LaunchStatus, RegistrationFlags, RegistrationStore, RelinkPreview, StoreError,
};
pub use thunar::{ThunarActionInstall, ThunarActionInstallError, install_thunar_actions};

pub const HELPER_EXECUTABLE: &str = "/usr/libexec/buzzardos-shortcut-helper";
