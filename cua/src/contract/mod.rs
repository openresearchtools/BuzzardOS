// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

mod cursor;
mod cursor_tools;
mod desktop;
mod inputs;
mod outputs;
pub(crate) mod verification;

pub use cursor::{CursorAction, CursorDelivery, CursorPlayback, CursorReducedMotion, CursorTarget};
pub use inputs::{
    ClickButton, ClickInput, ClipboardReadInput, ClipboardWriteInput, CloseWindowInput, DragInput,
    GetAgentCursorStateInput, GetCursorPositionInput, GetDesktopStateInput, GetScreenSizeInput,
    HotkeyInput, InvokeMenuInput, MaximizeWindowInput, MinimizeWindowInput, MoveCursorInput,
    PressKeyInput, RestoreWindowInput, ScrollBy, ScrollInput, SetAgentCursorEnabledInput,
    SetAgentCursorMotionInput, SetAgentCursorThemeInput, SetWindowFrameInput, ToolInput,
    TypeTextInput, MAX_TYPE_TEXT_CHARS,
};
pub use outputs::{
    ActionResult, ClipboardReadOutput, ClipboardWriteOutput, CursorPositionOutput,
    DesktopStateOutput, GetAgentCursorStateOutput, ScreenSizeOutput, SetAgentCursorEnabledOutput,
    SetAgentCursorMotionOutput, SetAgentCursorThemeOutput, ToolOutput,
};
pub use verification::{
    ElementPredicate, ElementSelector, PredicateOutcome, StatePredicate, UnknownReason,
    VerificationStatus, VerifyStateInput, VerifyStateOutput, WindowPredicate,
    VERIFY_STATE_DEFAULT_TIMEOUT_MS,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolAnnotations {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
}

#[derive(Debug, Clone)]
pub struct ToolContract {
    pub name: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub annotations: ToolAnnotations,
    pub input_schema: Value,
    pub success_output_schema: Option<Value>,
}

#[derive(
    Debug, Clone, Copy, serde::Serialize, serde::Deserialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Linux,
}

fn contracts() -> Vec<ToolContract> {
    let mut tools = desktop::contracts();
    tools.extend(cursor_tools::contracts());
    tools.extend(verification::contracts());
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    tools
}

pub fn tool_contract(name: &str) -> Option<ToolContract> {
    contracts().into_iter().find(|tool| tool.name == name)
}

#[derive(Debug)]
struct ToolIndexEntry {
    capabilities: Vec<String>,
    input_fields: BTreeSet<String>,
}

fn tool_index() -> &'static BTreeMap<String, ToolIndexEntry> {
    static INDEX: OnceLock<BTreeMap<String, ToolIndexEntry>> = OnceLock::new();
    INDEX.get_or_init(|| {
        contracts()
            .into_iter()
            .map(|tool| {
                let input_fields = tool
                    .input_schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .map(|(name, _)| name.clone())
                    .collect();
                (
                    tool.name,
                    ToolIndexEntry {
                        capabilities: tool.capabilities,
                        input_fields,
                    },
                )
            })
            .collect()
    })
}

pub fn tool_capabilities(name: &str) -> Option<Vec<String>> {
    tool_index()
        .get(name)
        .map(|entry| entry.capabilities.clone())
}

pub fn tool_input_fields(name: &str) -> Option<&'static BTreeSet<String>> {
    tool_index().get(name).map(|entry| &entry.input_fields)
}

pub fn tool_success_output_schema(name: &str) -> Option<Value> {
    if name == "list_windows" {
        return Some(desktop::list_windows_success_output_schema());
    }
    tool_contract(name).and_then(|contract| contract.success_output_schema)
}
