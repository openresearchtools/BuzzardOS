//! MCP JSON-RPC 2.0 server over stdio for the Buzzard OS Linux driver.
//!
//! Implements the Model Context Protocol (MCP) 2024-11-05 over stdio while
//! retaining the upstream wire contract used by the shared Rust SDK.
//!
//! # Protocol
//! - Line-delimited JSON-RPC 2.0 on stdin/stdout
//! - Methods: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`
//! - Each request has `jsonrpc: "2.0"`, `id` (any), `method`, optional `params`
//! - Notifications (no `id`) are silently ignored

pub const RESPONSIBILITY_DISCLAIMED_ENV: &str = "CUA_DRIVER_RS_RESPONSIBILITY_DISCLAIMED";

/// Embedded mode (`CUA_DRIVER_EMBEDDED=1` / `--embedded`): the daemon runs as
/// a direct child of its owning process. In the Linux guest this selects the
/// parent-liveness contract and never grants additional desktop capability.
pub const EMBEDDED_ENV: &str = "CUA_DRIVER_EMBEDDED";

/// Advisory label for the embedding host's bundle id, echoed in
/// `check_permissions` output. NOT a trust signal — trust comes from the
/// OS responsibility chain.
pub const HOST_BUNDLE_ID_ENV: &str = "CUA_DRIVER_HOST_BUNDLE_ID";

/// Internal embedded-host contract: when set to the exact value `1`, the
/// daemon treats EOF on stdin as proof that its owning host has exited. The
/// Rust SDK sets this only on the directly-spawned `serve` child; MCP proxies
/// continue to use stdin for JSON-RPC and never set it.
pub const PARENT_LIVENESS_STDIN_ENV: &str = "CUA_DRIVER_PARENT_LIVENESS_STDIN";

/// Only the exact value `1` counts — fail-safe for anything else.
pub fn embedded_mode() -> bool {
    std::env::var_os(EMBEDDED_ENV).is_some_and(|v| v == "1")
}

/// Parent-EOF shutdown is valid only for a directly embedded daemon. Requiring
/// both sentinels prevents an ambient variable from changing ordinary
/// standalone or MCP stdin behavior.
pub fn parent_liveness_stdin_enabled() -> bool {
    embedded_mode() && std::env::var_os(PARENT_LIVENESS_STDIN_ENV).is_some_and(|value| value == "1")
}

pub mod action_record;
pub mod authorization;
pub mod browser;
pub mod capture_mode;
pub mod capture_scope;
pub mod cdp;
pub mod clipboard;
pub mod consent;
pub mod cursor_events;
pub mod cursor_sampler;
pub mod daemon;
pub mod element_cache;
pub mod element_query;
pub mod element_token;
pub mod expectation;
pub mod ffmpeg_install;
pub mod health_report;
pub mod image_utils;
pub mod page;
pub mod pip_hook;
pub mod policy;
pub mod protocol;
pub mod recording;
pub mod recording_loader;
pub mod recording_render;
pub mod recording_tools;
pub mod recording_zoom;
pub mod server;
pub mod session;
pub mod session_authorization;
pub mod session_manifest;
pub mod session_tools;
pub mod socket_io;
pub mod text_sanitize;
pub mod tool;
pub mod tool_args;
pub mod tool_schema;
pub mod video;
pub mod video_ffmpeg;
pub mod window_inspection;
pub mod window_target;

pub use cua_driver_contract::{CaptureScope, EscalationReason};
pub use recording::RecordingSession;
