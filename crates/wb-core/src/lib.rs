// SPDX-License-Identifier: AGPL-3.0-or-later

mod idmap;
mod machine;
mod paths;
mod resources;
mod wayland;

pub use idmap::IdMap;
pub use machine::{
    DisplayDiagnostics, MachineConfig, MachineState, NetworkMode, PresentationDiagnostics,
    RuntimeState, WindowDiagnostics,
};
pub use paths::{WbPaths, host_control_socket};
pub use resources::ResourceLocator;
pub use wayland::WaylandCapabilities;
