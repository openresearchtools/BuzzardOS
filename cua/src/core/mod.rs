//! Shared implementation used by the daemonless Buzzard CUA command.

pub mod action_record;
pub mod capture_mode;
pub mod capture_scope;
pub mod clipboard;
pub mod cursor_events;
pub mod cursor_sampler;
pub mod element_cache;
pub mod element_query;
pub mod element_token;
pub mod expectation;
pub mod health_report;
pub mod image_utils;
pub mod protocol;
pub mod session;
pub mod session_tools;
pub mod text_sanitize;
pub mod tool;
pub mod tool_args;
pub mod tool_schema;
pub mod window_inspection;
pub mod window_target;

pub use crate::contract::{CaptureScope, EscalationReason};
