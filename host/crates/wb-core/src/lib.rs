// SPDX-License-Identifier: AGPL-3.0-or-later

mod idmap;
mod machine;
mod media;
mod paths;
mod registry;
mod resources;
mod wayland;

pub use idmap::IdMap;
pub use machine::{
    DisplayDiagnostics, IntegrationDiagnostics, IntegrationSettings, MachineConfig, MachineState,
    MediaIntegrationDiagnostics, MediaSharing, NetworkMode, OciImageMetadata, PortDirection,
    PortForward, PortIntegrationDiagnostics, PortProtocol, PresentationDiagnostics,
    RetainedOciArchive, RuntimeState, SharedPath, WindowDiagnostics,
};
pub use media::{HostMediaBackend, HostMediaDevice, HostMediaKind, discover_host_media};
pub use paths::{WbPaths, host_control_socket};
pub use registry::{MachineRegistry, RegisteredMachine};
pub use resources::ResourceLocator;
pub use wayland::WaylandCapabilities;

/// Stable machine-state prefix used only when the broker's bounded desktop
/// readiness deadline expires. Launchers may key guarded runtime rollback on
/// this code without mistaking an arbitrary startup failure for a timeout.
pub const DESKTOP_READINESS_DEADLINE_DETAIL_PREFIX: &str = "desktop-readiness-deadline:";
