// SPDX-License-Identifier: AGPL-3.0-or-later

mod appimage_lease;
mod idmap;
mod machine;
mod media;
mod paths;
mod resources;
mod wayland;

pub use appimage_lease::{APPIMAGE_LEASE_FD_ENV, AppImageRuntimeLease};
pub use idmap::IdMap;
pub use machine::{
    DisplayDiagnostics, IntegrationDiagnostics, IntegrationSettings, MachineConfig, MachineState,
    MediaIntegrationDiagnostics, MediaSharing, NetworkMode, PortDirection, PortForward,
    PortIntegrationDiagnostics, PortProtocol, PresentationDiagnostics, RuntimeState,
    WindowDiagnostics,
};
pub use media::{HostMediaBackend, HostMediaDevice, HostMediaKind, discover_host_media};
pub use paths::{WbPaths, host_control_socket};
pub use resources::ResourceLocator;
pub use wayland::WaylandCapabilities;

/// Stable machine-state prefix used only when the broker's bounded desktop
/// readiness deadline expires. Launchers may key guarded runtime rollback on
/// this code without mistaking an arbitrary startup failure for a timeout.
pub const DESKTOP_READINESS_DEADLINE_DETAIL_PREFIX: &str = "desktop-readiness-deadline:";
