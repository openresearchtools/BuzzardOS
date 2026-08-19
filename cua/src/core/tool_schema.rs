//! Reused JSON-schema fragments for the daemonless Linux CLI.

use serde_json::{json, Value};

pub fn session_schema() -> Value {
    json!({
        "type": "string",
        "description": "Optional agent-session identity used by cursor and capture state."
    })
}

pub fn delivery_mode_schema_with(description: &str) -> Value {
    json!({
        "type": "string",
        "enum": ["background", "foreground"],
        "description": description
    })
}

pub fn modifier_schema() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "Modifier keys held during the action: shift, alt, or ctrl."
    })
}

pub fn button_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["left", "right", "middle"],
        "description": "Mouse button. Defaults to left."
    })
}

pub fn element_index_schema() -> Value {
    json!({
        "type": "integer",
        "description": "Element index paired with its snapshot_id. Prefer element_token."
    })
}

pub fn snapshot_id_schema() -> Value {
    json!({
        "type": "string",
        "pattern": "^s[0-9a-f]{8}$",
        "description": "Snapshot identity paired with element_index."
    })
}

pub fn element_token_schema() -> Value {
    json!({
        "type": "string",
        "description": "Opaque element handle from get_window_state; stale handles fail closed."
    })
}
