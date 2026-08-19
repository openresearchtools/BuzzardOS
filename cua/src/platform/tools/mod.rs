//! Linux tool implementations.
//!
//! On Linux: delegates to real x11/atspi/input/capture implementations.
//! On other platforms: returns "not implemented" stubs so the crate compiles.

use crate::core::tool::ToolRegistry;

#[cfg(target_os = "linux")]
mod impl_;

pub fn build_registry() -> ToolRegistry {
    impl_::build_registry()
}

// Keep register_all as alias for backwards compat.
pub fn register_all() -> ToolRegistry {
    build_registry()
}
