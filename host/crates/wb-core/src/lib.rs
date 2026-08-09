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
