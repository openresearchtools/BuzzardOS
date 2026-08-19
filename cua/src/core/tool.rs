// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.
// Buzzard modifications: AGPL-3.0-or-later

//! Daemonless tool definitions and direct in-process dispatch.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::Value;

use crate::core::protocol::ToolResult;

tokio::task_local! {
    static DISPATCH_RUNTIME_SCOPE: String;
}

/// Runtime scope for state which must survive nested calls inside one CLI
/// invocation. It is never caller-selectable transport metadata.
pub fn current_dispatch_runtime_scope() -> Option<String> {
    DISPATCH_RUNTIME_SCOPE.try_with(Clone::clone).ok()
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
    pub open_world: bool,
}

impl ToolDef {
    pub fn from_contract(contract: &crate::contract::ToolContract) -> Self {
        Self {
            name: contract.name.clone(),
            description: contract.description.clone(),
            input_schema: contract.input_schema.clone(),
            read_only: contract.annotations.read_only,
            destructive: contract.annotations.destructive,
            idempotent: contract.annotations.idempotent,
            open_world: contract.annotations.open_world,
        }
    }

    pub fn to_list_entry(&self) -> Value {
        let capabilities = crate::contract::tool_capabilities(&self.name).unwrap_or_default();
        let mut entry = serde_json::json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
            "annotations": {
                "readOnlyHint": self.read_only,
                "destructiveHint": self.destructive,
                "idempotentHint": self.idempotent,
                "openWorldHint": self.open_world,
            },
            "capabilities": capabilities,
        });
        if let Some(schema) = crate::contract::tool_success_output_schema(&self.name) {
            entry["outputSchema"] = schema;
        }
        entry
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn def(&self) -> &ToolDef;

    async fn invoke(&self, args: Value) -> ToolResult;
}

struct RuntimeCleanup(Option<Box<dyn FnOnce() + Send + Sync>>);

impl Drop for RuntimeCleanup {
    fn drop(&mut self) {
        if let Some(cleanup) = self.0.take() {
            cleanup();
        }
    }
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
    order: Vec<String>,
    runtime_cleanups: Vec<RuntimeCleanup>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
            order: Vec::new(),
            runtime_cleanups: Vec::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.def().name.clone();
        self.order.push(name.clone());
        self.tools.insert(name, tool);
    }

    pub fn retain_runtime_cleanup(&mut self, cleanup: impl FnOnce() + Send + Sync + 'static) {
        self.runtime_cleanups
            .push(RuntimeCleanup(Some(Box::new(cleanup))));
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.order.clone()
    }

    pub fn get_def(&self, name: &str) -> Option<&ToolDef> {
        self.tools.get(name).map(|tool| tool.def())
    }

    pub fn tools_list(&self) -> Value {
        serde_json::json!({
            "tools": self.order.iter().filter_map(|name| {
                self.tools.get(name).map(|tool| tool.def().to_list_entry())
            }).collect::<Vec<_>>()
        })
    }

    pub async fn invoke_direct(&self, name: &str, args: Value) -> ToolResult {
        let Some(tool) = self.tools.get(name) else {
            return ToolResult::error(format!("Unknown tool: {name}"));
        };
        let scope = format!("cua{}", crate::core::seat_context::current_index());
        DISPATCH_RUNTIME_SCOPE.scope(scope, tool.invoke(args)).await
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
