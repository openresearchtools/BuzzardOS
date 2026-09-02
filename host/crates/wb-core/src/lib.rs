// SPDX-License-Identifier: AGPL-3.0-or-later

mod machine;
mod media;
mod paths;
mod podman;
mod registry;
mod resources;
mod wayland;

pub use machine::{
    DEFAULT_PODMAN_ARGUMENTS, IntegrationSettings, MachineConfig, MachineState, MediaSharing,
    NetworkMode, OciImageMetadata, PortDirection, PortForward, PortProtocol,
    PresentationDiagnostics, RetainedOciArchive, RuntimeState, SharedPath, WindowDiagnostics,
};
pub use media::{HostMediaBackend, HostMediaDevice, HostMediaKind, discover_host_media};
pub use paths::{WbPaths, host_control_socket};
pub use podman::{
    GUEST_AUDIO_PORT, HOST_CAMERA_PORT, HOST_MICROPHONE_PORT, Podman, PodmanContainerState,
    PodmanDefinition, PodmanImageInspection, PodmanInspection, PodmanRuntimePaths,
};
pub use registry::{MachineRegistry, RegisteredMachine};
pub use resources::ResourceLocator;
pub use wayland::WaylandCapabilities;
