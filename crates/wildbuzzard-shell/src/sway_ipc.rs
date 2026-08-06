// SPDX-License-Identifier: AGPL-3.0-or-later

//! Exact task-window identity and controls for the pinned Sway guest session.
//!
//! `ext_foreign_toplevel_list_v1` gives the shell Sway's opaque, per-mapping
//! identifier. The same identifier is present in Sway's IPC tree, so task
//! controls never have to guess by title, app-id, PID, or focus.

use anyhow::{Context, Result};
use serde_json::Value;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SCRATCHPAD_WORKSPACE: &str = "__i3_scratch";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    fn right(self) -> i32 {
        self.x.saturating_add(self.width.max(0))
    }

    fn bottom(self) -> i32 {
        self.y.saturating_add(self.height.max(0))
    }

    fn union(self, other: Self) -> Self {
        if other.width <= 0 || other.height <= 0 {
            return self;
        }
        if self.width <= 0 || self.height <= 0 {
            return other;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            width: self.right().max(other.right()).saturating_sub(x),
            height: self.bottom().max(other.bottom()).saturating_sub(y),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WindowState {
    pub identifier: String,
    pub container_id: u64,
    pub rect: Rect,
    pub workspace_rect: Option<Rect>,
    pub focused: bool,
    pub scratchpad: bool,
    pub minimized: bool,
    pub maximized: bool,
}

fn json_rect(value: Option<&Value>) -> Rect {
    let value = value.unwrap_or(&Value::Null);
    Rect {
        x: value
            .get("x")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or_default(),
        y: value
            .get("y")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or_default(),
        width: value
            .get("width")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or_default(),
        height: value
            .get("height")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or_default(),
    }
}

fn collect(
    node: &Value,
    parent_origin: (i32, i32),
    workspace_rect: Option<Rect>,
    scratchpad_workspace: bool,
    windows: &mut Vec<WindowState>,
) {
    let node_type = node.get("type").and_then(Value::as_str).unwrap_or_default();
    let node_name = node.get("name").and_then(Value::as_str).unwrap_or_default();
    let node_rect = json_rect(node.get("rect"));
    let (workspace_rect, scratchpad_workspace) = if node_type == "workspace" {
        (Some(node_rect), node_name == SCRATCHPAD_WORKSPACE)
    } else {
        (workspace_rect, scratchpad_workspace)
    };

    if let Some(identifier) = node
        .get("foreign_toplevel_identifier")
        .and_then(Value::as_str)
        .filter(|identifier| !identifier.is_empty())
    {
        // In Sway IPC `rect` is absolute and excludes the compositor
        // titlebar, while `deco_rect` is relative to the parent.  Task-menu
        // move/resize/maximize state must use the complete outer frame.
        let decoration = json_rect(node.get("deco_rect"));
        let decoration = Rect {
            x: parent_origin.0.saturating_add(decoration.x),
            y: parent_origin.1.saturating_add(decoration.y),
            ..decoration
        };
        let rect = node_rect.union(decoration);
        let scratchpad = node
            .get("scratchpad_state")
            .and_then(Value::as_str)
            .is_some_and(|state| state != "none");
        // A shown scratchpad member remains tagged `fresh`; it is minimized
        // only while it lives under Sway's synthetic scratch workspace.
        let minimized = scratchpad && scratchpad_workspace;
        windows.push(WindowState {
            identifier: identifier.to_owned(),
            container_id: node.get("id").and_then(Value::as_u64).unwrap_or_default(),
            rect,
            workspace_rect,
            focused: node
                .get("focused")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            scratchpad,
            minimized,
            maximized: !minimized
                && !scratchpad_workspace
                && workspace_rect.is_some_and(|workspace| rect == workspace),
        });
    }

    let child_origin = (node_rect.x, node_rect.y);
    for field in ["nodes", "floating_nodes"] {
        if let Some(children) = node.get(field).and_then(Value::as_array) {
            for child in children {
                collect(
                    child,
                    child_origin,
                    workspace_rect,
                    scratchpad_workspace,
                    windows,
                );
            }
        }
    }
}

fn parse_tree(bytes: &[u8]) -> Result<Vec<WindowState>> {
    let root: Value = serde_json::from_slice(bytes).context("parsing Sway IPC tree")?;
    let mut windows = Vec::new();
    let root_rect = json_rect(root.get("rect"));
    collect(&root, (root_rect.x, root_rect.y), None, false, &mut windows);
    Ok(windows)
}

pub fn list_windows() -> Result<Vec<WindowState>> {
    anyhow::ensure!(
        std::env::var_os("SWAYSOCK").is_some(),
        "SWAYSOCK is unavailable"
    );
    let output = Command::new("swaymsg")
        .args(["-r", "-t", "get_tree"])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("executing swaymsg get_tree")?;
    anyhow::ensure!(
        output.status.success(),
        "swaymsg get_tree failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    parse_tree(&output.stdout)
}

pub fn window(identifier: &str) -> Result<WindowState> {
    list_windows()?
        .into_iter()
        .find(|window| window.identifier == identifier)
        .with_context(|| format!("Sway has no mapped toplevel identifier {identifier}"))
}

fn run_container_command(container_id: u64, command: &str) -> Result<()> {
    let selector = format!("[con_id={container_id}]");
    let output = Command::new("swaymsg")
        .args(["-r", selector.as_str(), command])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("executing Sway command for container {container_id}"))?;
    anyhow::ensure!(
        output.status.success(),
        "Sway command failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let replies: Value =
        serde_json::from_slice(&output.stdout).context("parsing Sway command reply")?;
    let accepted = replies.as_array().is_some_and(|replies| {
        !replies.is_empty()
            && replies
                .iter()
                .all(|reply| reply.get("success").and_then(Value::as_bool) == Some(true))
    });
    anyhow::ensure!(accepted, "Sway rejected command: {replies}");
    Ok(())
}

pub fn focus(identifier: &str) -> Result<()> {
    let state = window(identifier)?;
    let command = if state.minimized {
        "scratchpad show, focus"
    } else {
        "focus"
    };
    run_container_command(state.container_id, command)?;
    wait_for(identifier, |state| state.focused && !state.minimized)?;
    Ok(())
}

pub fn minimize(identifier: &str) -> Result<()> {
    let state = window(identifier)?;
    let command = if state.scratchpad {
        "scratchpad show"
    } else {
        "move scratchpad"
    };
    run_container_command(state.container_id, command)?;
    wait_for(identifier, |state| state.minimized)?;
    Ok(())
}

pub fn maximize(identifier: &str) -> Result<(WindowState, Rect)> {
    let state = window(identifier)?;
    let workspace = state
        .workspace_rect
        .context("window is not attached to the visible workspace")?;
    let command = format!(
        "fullscreen disable, floating enable, resize set width {} px height {} px, move position {} {}",
        workspace.width, workspace.height, workspace.x, workspace.y
    );
    run_container_command(state.container_id, &command)?;
    wait_for(identifier, |state| state.maximized)?;
    Ok((state, workspace))
}

pub fn restore(identifier: &str, frame: Option<Rect>) -> Result<()> {
    let state = window(identifier)?;
    if state.minimized {
        run_container_command(state.container_id, "scratchpad show")?;
    }
    let state = window(identifier)?;
    let frame = frame.unwrap_or_else(|| {
        let workspace = state.workspace_rect.unwrap_or(Rect {
            x: 0,
            y: 0,
            width: 1280,
            height: 758,
        });
        let width = (workspace.width * 2 / 3).max(320);
        let height = (workspace.height * 2 / 3).max(240);
        Rect {
            x: workspace.x + (workspace.width - width) / 2,
            y: workspace.y + (workspace.height - height) / 2,
            width,
            height,
        }
    });
    let command = format!(
        "fullscreen disable, floating enable, resize set width {} px height {} px, move position {} {}",
        frame.width.max(1),
        frame.height.max(1),
        frame.x,
        frame.y
    );
    run_container_command(state.container_id, &command)?;
    wait_for(identifier, |state| {
        !state.minimized && !state.maximized && state.rect == frame
    })?;
    Ok(())
}

pub fn close(identifier: &str) -> Result<()> {
    let state = window(identifier)?;
    run_container_command(state.container_id, "kill")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if window(identifier).is_err() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    anyhow::bail!("Sway did not confirm that toplevel {identifier} closed")
}

fn wait_for(identifier: &str, predicate: impl Fn(&WindowState) -> bool) -> Result<WindowState> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Ok(state) = window(identifier)
            && predicate(&state)
        {
            return Ok(state);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("Sway state readback timed out for toplevel {identifier}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_identifiers_and_scratchpad_state() {
        let tree = br#"{
          "type":"root",
          "nodes":[{
            "type":"output",
            "nodes":[{
              "type":"workspace",
              "name":"1",
              "rect":{"x":0,"y":0,"width":1707,"height":1025},
              "floating_nodes":[{
                "id":7,
                "foreign_toplevel_identifier":"visible-id",
                "rect":{"x":0,"y":25,"width":1707,"height":1000},
                "deco_rect":{"x":0,"y":0,"width":1707,"height":25},
                "scratchpad_state":"none",
                "focused":true,
                "visible":true
              }]
            }]
          },{
            "type":"output",
            "nodes":[{
              "type":"workspace",
              "name":"__i3_scratch",
              "floating_nodes":[{
                "id":8,
                "foreign_toplevel_identifier":"hidden-id",
                "rect":{"x":50,"y":70,"width":800,"height":600},
                "deco_rect":{"x":50,"y":45,"width":800,"height":25},
                "scratchpad_state":"fresh",
                "visible":false
              }]
            }]
          }]
        }"#;
        let windows = parse_tree(tree).expect("parse tree");
        assert_eq!(windows.len(), 2);
        assert!(windows[0].focused);
        assert!(windows[0].maximized);
        assert!(!windows[0].minimized);
        assert_eq!(
            windows[0].rect,
            Rect {
                x: 0,
                y: 0,
                width: 1707,
                height: 1025,
            }
        );
        assert_eq!(windows[0].container_id, 7);
        assert!(windows[1].minimized);
        assert!(!windows[1].maximized);
    }
}
