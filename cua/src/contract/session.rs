// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

use crate::contract::{
    EndSessionInput, EndSessionOutput, EscalateSessionInput, GetSessionStateInput,
    SessionStateOutput, StartSessionInput, StartSessionOutput, ToolAnnotations, ToolContract,
    ToolInput, ToolOutput,
};

pub fn contracts() -> Vec<ToolContract> {
    vec![start(), escalate(), get_state(), end()]
}

fn contract<I: ToolInput, O: ToolOutput>(
    name: &str,
    description: &str,
    capabilities: &[&str],
    annotations: ToolAnnotations,
) -> ToolContract {
    assert_eq!(name, I::TOOL_NAME, "typed input is bound to the wrong tool");
    ToolContract {
        name: name.into(),
        description: description.into(),
        capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
        annotations,
        input_schema: I::input_schema(),
        success_output_schema: Some(O::output_schema()),
    }
}

fn start() -> ToolContract {
    contract::<StartSessionInput, StartSessionOutput>(
        "start_session",
        "Declare a named agent session and its capture scope. Reusing the same session id refreshes it; end it explicitly with end_session.",
        &["session.lifecycle.start", "session.capture_scope"],
        ToolAnnotations {
            read_only: false,
            destructive: false,
            idempotent: true,
            open_world: false,
        },
    )
}

fn escalate() -> ToolContract {
    contract::<EscalateSessionInput, SessionStateOutput>(
        "escalate_session",
        "Unlock the desktop phase of an auto capture-scope session after the window action ladder has been exhausted and verified. This is a one-way transition for the live session and records a bounded reason.",
        &["session.capture_scope.escalate"],
        ToolAnnotations {
            read_only: false,
            destructive: false,
            idempotent: false,
            open_world: false,
        },
    )
}

fn get_state() -> ToolContract {
    contract::<GetSessionStateInput, SessionStateOutput>(
        "get_session_state",
        "Read the live session's capture policy and effective scope.",
        &["session.capture_scope.read"],
        ToolAnnotations {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        },
    )
}

fn end() -> ToolContract {
    contract::<EndSessionInput, EndSessionOutput>(
        "end_session",
        "End a declared session and clear its runtime-only cursor and capture state. Idempotent.",
        &["session.lifecycle.end"],
        ToolAnnotations {
            read_only: false,
            destructive: true,
            idempotent: true,
            open_world: false,
        },
    )
}
