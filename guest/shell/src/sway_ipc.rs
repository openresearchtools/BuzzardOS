// SPDX-License-Identifier: AGPL-3.0-or-later

//! Exact task-window identity and controls for the pinned Sway guest session.
//!
//! `ext_foreign_toplevel_list_v1` gives the shell Sway's opaque, per-mapping
//! identifier. The same identifier is present in Sway's IPC tree, so task
//! controls never have to guess by title, app-id, PID, or focus.

use anyhow::{Context, Result};
use buzzardos_desktop_core::{SolidColor, ThemePalette};
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const SCRATCHPAD_WORKSPACE: &str = "__i3_scratch";
const RESTORE_MARK_PREFIX: &str = "__buzzardos_restore_v1_";
const IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const IPC_SUBSCRIBE: u32 = 2;
const IPC_EVENT_MASK: u32 = 1 << 31;
const MAX_IPC_PAYLOAD: usize = 16 * 1024 * 1024;

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
    pub pid: u32,
    pub rect: Rect,
    pub workspace_rect: Option<Rect>,
    pub workspace: String,
    pub output: String,
    pub focused: bool,
    pub scratchpad: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    pub decoration_height: i32,
    restore_frame: Option<Rect>,
    restore_marks: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceState {
    pub name: String,
    pub output: String,
    pub focused: bool,
    pub visible: bool,
    pub rect: Rect,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct OutputState {
    id: u64,
    name: String,
    active: bool,
    rect: Rect,
    physical_width: u32,
    physical_height: u32,
    refresh_millihz: u32,
    scale_milli: u32,
}

fn restore_mark(container_id: u64, rect: Rect) -> String {
    format!(
        "{RESTORE_MARK_PREFIX}{container_id}_{}_{}_{}_{}",
        rect.x, rect.y, rect.width, rect.height
    )
}

fn parse_restore_mark(mark: &str, container_id: u64) -> Option<Rect> {
    let mut fields = mark.strip_prefix(RESTORE_MARK_PREFIX)?.split('_');
    let marked_id = fields.next()?.parse::<u64>().ok()?;
    let rect = Rect {
        x: fields.next()?.parse::<i32>().ok()?,
        y: fields.next()?.parse::<i32>().ok()?,
        width: fields.next()?.parse::<i32>().ok()?,
        height: fields.next()?.parse::<i32>().ok()?,
    };
    if fields.next().is_some() || marked_id != container_id || rect.width <= 0 || rect.height <= 0 {
        return None;
    }
    (restore_mark(container_id, rect) == mark).then_some(rect)
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
    workspace_name: Option<&str>,
    output_name: Option<&str>,
    scratchpad_workspace: bool,
    windows: &mut Vec<WindowState>,
) {
    let node_type = node.get("type").and_then(Value::as_str).unwrap_or_default();
    let node_name = node.get("name").and_then(Value::as_str).unwrap_or_default();
    let node_rect = json_rect(node.get("rect"));
    let output_name = if node_type == "output" {
        Some(node_name)
    } else {
        output_name
    };
    let (workspace_rect, workspace_name, scratchpad_workspace) = if node_type == "workspace" {
        (
            Some(node_rect),
            Some(node_name),
            node_name == SCRATCHPAD_WORKSPACE,
        )
    } else {
        (workspace_rect, workspace_name, scratchpad_workspace)
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
        let container_id = node.get("id").and_then(Value::as_u64).unwrap_or_default();
        let restore_marks = node
            .get("marks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|mark| parse_restore_mark(mark, container_id).is_some())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let restore_frame = restore_marks
            .iter()
            .find_map(|mark| parse_restore_mark(mark, container_id));
        // A shown scratchpad member remains tagged `fresh`; it is minimized
        // only while it lives under Sway's synthetic scratch workspace.
        let minimized = scratchpad && scratchpad_workspace;
        windows.push(WindowState {
            identifier: identifier.to_owned(),
            container_id,
            pid: node
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok())
                .unwrap_or_default(),
            rect,
            workspace_rect,
            workspace: workspace_name.unwrap_or_default().to_owned(),
            output: output_name.unwrap_or_default().to_owned(),
            focused: node
                .get("focused")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            scratchpad,
            minimized,
            maximized: !minimized
                && !scratchpad_workspace
                && restore_frame.is_some()
                && workspace_rect.is_some_and(|workspace| rect == workspace),
            fullscreen: node
                .get("fullscreen_mode")
                .and_then(Value::as_i64)
                .is_some_and(|mode| mode != 0),
            decoration_height: decoration.height.max(0),
            restore_frame,
            restore_marks,
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
                    workspace_name,
                    output_name,
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
    collect(
        &root,
        (root_rect.x, root_rect.y),
        None,
        None,
        None,
        false,
        &mut windows,
    );
    Ok(windows)
}

fn swaymsg_json(message_type: &str) -> Result<Value> {
    anyhow::ensure!(
        std::env::var_os("SWAYSOCK").is_some(),
        "SWAYSOCK is unavailable"
    );
    let output = Command::new("swaymsg")
        .args(["-r", "-t", message_type])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("executing swaymsg {message_type}"))?;
    anyhow::ensure!(
        output.status.success(),
        "swaymsg {message_type} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).with_context(|| format!("parsing Sway {message_type}"))
}

pub fn list_workspaces() -> Result<Vec<WorkspaceState>> {
    let value = swaymsg_json("get_workspaces")?;
    let mut workspaces = value
        .as_array()
        .context("Sway get_workspaces did not return an array")?
        .iter()
        .filter_map(|value| {
            let name = value.get("name")?.as_str()?.to_owned();
            (name != SCRATCHPAD_WORKSPACE).then(|| WorkspaceState {
                name,
                output: value
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                focused: value
                    .get("focused")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                visible: value
                    .get("visible")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                rect: json_rect(value.get("rect")),
            })
        })
        .collect::<Vec<_>>();
    workspaces.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(workspaces)
}

fn list_outputs() -> Result<Vec<OutputState>> {
    let value = swaymsg_json("get_outputs")?;
    Ok(value
        .as_array()
        .context("Sway get_outputs did not return an array")?
        .iter()
        .filter_map(|value| {
            let name = value.get("name")?.as_str()?.to_owned();
            let mode = value.get("current_mode").unwrap_or(&Value::Null);
            let scale = value.get("scale").and_then(Value::as_f64).unwrap_or(1.0);
            Some(OutputState {
                id: value.get("id").and_then(Value::as_u64)?,
                name,
                active: value
                    .get("active")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                rect: json_rect(value.get("rect")),
                physical_width: mode
                    .get("width")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or_default(),
                physical_height: mode
                    .get("height")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or_default(),
                refresh_millihz: mode
                    .get("refresh")
                    .and_then(Value::as_u64)
                    .and_then(|v| u32::try_from(v).ok())
                    .unwrap_or(60_000),
                scale_milli: (scale * 1000.0).round().clamp(1.0, f64::from(u32::MAX)) as u32,
            })
        })
        .collect())
}

fn quote_sway(value: &str) -> Result<String> {
    anyhow::ensure!(
        !value.is_empty()
            && value
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || matches!(character, ' ' | '_' | '-')),
        "unsafe Sway identifier"
    );
    Ok(format!("\"{value}\""))
}

pub fn desktop_output() -> Result<String> {
    let outputs = list_outputs()?;
    outputs
        .iter()
        .filter(|output| output.active)
        .min_by_key(|output| output.id)
        .map(|output| output.name.clone())
        .context("Sway has no active host-facing output")
}

pub fn current_desktop_workspace() -> Result<String> {
    let output = desktop_output()?;
    list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.visible && workspace.output == output)
        .map(|workspace| workspace.name)
        .context("host-facing Sway output has no visible workspace")
}

pub fn ensure_desktop_workspace() -> Result<()> {
    if list_workspaces()?
        .iter()
        .any(|workspace| workspace.name == "Desktop")
    {
        return Ok(());
    }
    run_global_command("workspace \"Desktop\"")
}

pub fn ensure_workspace(index: u32) -> Result<WorkspaceState> {
    anyhow::ensure!(index > 0, "Desktop does not need a virtual output");
    ensure_numbered_workspace(index, crate::model::is_cua_workspace(index))
}

fn ensure_numbered_workspace(index: u32, cua_seat: bool) -> Result<WorkspaceState> {
    let name = crate::model::workspace_name(index);
    if let Some(workspace) = list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.name == name)
    {
        if cua_seat {
            run_global_command(&format!("seat \"seat{index}\" fallback false"))?;
        }
        return Ok(workspace);
    }
    ensure_desktop_workspace()?;
    let before = list_outputs()?;
    let primary_name = before
        .iter()
        .filter(|output| output.active)
        .min_by_key(|output| output.id)
        .map(|output| output.name.clone())
        .context("Sway has no host-facing output to mirror")?;
    let before_names = before
        .iter()
        .map(|output| output.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    run_global_command("create_output")?;
    let outputs = list_outputs()?;
    let created = outputs
        .iter()
        .find(|output| !before_names.contains(output.name.as_str()))
        .context("Sway accepted create_output but exposed no new output")?;
    let primary = outputs
        .iter()
        .find(|output| output.active && output.name == primary_name)
        .context("Sway has no host-facing output to mirror")?;
    let x = outputs
        .iter()
        .filter(|output| output.active && output.name != primary.name)
        .fold(
            primary.rect.x.saturating_add(primary.rect.width),
            |right, output| right.max(output.rect.x.saturating_add(output.rect.width)),
        );
    let refresh_hz = f64::from(primary.refresh_millihz) / 1000.0;
    let scale = f64::from(primary.scale_milli) / 1000.0;
    let current = current_desktop_workspace()?;
    let seat_command = if cua_seat {
        format!("seat \"seat{index}\" fallback false; ")
    } else {
        String::new()
    };
    let command = format!(
        "output {} mode {}x{}@{refresh_hz:.3}Hz scale {scale:.3} pos {x} {}; {seat_command}workspace {}; move workspace to output {}; workspace {}",
        quote_sway(&created.name)?,
        primary.physical_width.max(1),
        primary.physical_height.max(1),
        primary.rect.y,
        quote_sway(&name)?,
        quote_sway(&created.name)?,
        quote_sway(&current)?,
    );
    run_global_command(&command)?;
    list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.name == name)
        .with_context(|| format!("Sway did not create workspace {name}"))
}

fn runtime_lock_root() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is unavailable")?;
    let root = PathBuf::from(runtime).join("buzzardoscua");
    match fs::create_dir(&root) {
        Ok(()) => fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("creating Buzzard CUA runtime directory"),
    }
    let metadata = fs::symlink_metadata(&root)?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "Buzzard CUA runtime directory is not private to the guest user"
    );
    Ok(root)
}

fn lock_cua_workspaces(names: &[&str]) -> Result<Vec<File>> {
    let mut indices = names
        .iter()
        .filter_map(|name| crate::model::workspace_index(name))
        .filter(|index| crate::model::is_cua_workspace(*index))
        .collect::<Vec<_>>();
    indices.sort_unstable();
    indices.dedup();
    if indices.is_empty() {
        return Ok(Vec::new());
    }
    let root = runtime_lock_root()?;
    let mut locks = Vec::with_capacity(indices.len());
    for index in indices {
        let path = root.join(format!("seat{index}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        let metadata = file.metadata()?;
        anyhow::ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o077 == 0,
            "Buzzard CUA seat lock is not private to the guest user"
        );
        // Never freeze the shell behind a long-running agent operation.
        // The user can retry the selector after that one bounded CLI call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            anyhow::bail!(
                "{} is busy with an active CUA operation: {error}",
                crate::model::workspace_name(index)
            );
        }
        locks.push(file);
    }
    Ok(locks)
}

pub fn switch_workspace(name: &str) -> Result<()> {
    ensure_desktop_workspace()?;
    let host_output = desktop_output()?;
    let current = current_desktop_workspace()?;
    if current == name {
        return Ok(());
    }
    let _locks = lock_cua_workspaces(&[&current, name])?;
    switch_workspace_unlocked(name, &host_output, &current)
}

fn switch_workspace_unlocked(name: &str, host_output: &str, current: &str) -> Result<()> {
    let target = list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.name == name)
        .with_context(|| format!("unknown workspace {name}"))?;
    if target.output == host_output {
        return run_global_command(&format!("workspace {}", quote_sway(name)?));
    }
    run_global_command(&format!(
        "workspace {}; move workspace to output {}; workspace {}; move workspace to output {}; workspace {}",
        quote_sway(name)?,
        quote_sway(host_output)?,
        quote_sway(current)?,
        quote_sway(&target.output)?,
        quote_sway(name)?
    ))?;

    // Moving a workspace between the off-screen agent output and the fixed
    // human output changes its usable rectangle: only the human output owns
    // the 28px navigation bar. Re-clamp every normal mapped window after the
    // atomic swap so an off-screen frame at y=14 cannot appear underneath the
    // human bar, and so the workspace moved away may use its full height.
    let affected = list_windows()?
        .into_iter()
        .filter(|window| window.workspace == name || window.workspace == current)
        .map(|window| window.identifier)
        .collect::<Vec<_>>();
    for identifier in affected {
        constrain_new_window(&identifier)
            .with_context(|| format!("constraining {identifier} after workspace swap"))?;
    }
    Ok(())
}

pub fn move_window_to_workspace(identifier: &str, workspace: &str, focus: bool) -> Result<()> {
    let state = window(identifier)?;
    let _locks = lock_cua_workspaces(&[&state.workspace, workspace])?;
    let workspace = quote_sway(workspace)?;
    let mut commands = vec![format!("move container to workspace {workspace}")];
    if focus {
        commands.push(format!("workspace {workspace}"));
        commands.push("focus".to_owned());
    }
    run_commands(state.container_id, commands)?;
    let after = window(identifier)?;
    anyhow::ensure!(
        after.workspace == workspace.trim_matches('"'),
        "Sway did not move the window to the requested workspace"
    );
    Ok(())
}

pub fn close_workspace(index: u32) -> Result<()> {
    anyhow::ensure!(index > 0, "Desktop cannot be closed");
    let name = crate::model::workspace_name(index);
    let _locks = lock_cua_workspaces(&[&name])?;
    let workspace = list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.name == name)
        .with_context(|| format!("unknown workspace {name}"))?;
    let host_output = desktop_output()?;
    if workspace.output == host_output {
        switch_workspace_unlocked("Desktop", &host_output, &name)?;
    }
    let output = list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.name == name)
        .map(|workspace| workspace.output)
        .with_context(|| format!("workspace {name} disappeared before close"))?;
    anyhow::ensure!(
        output != host_output,
        "refusing to unplug host output {output}"
    );
    let windows = list_windows()?
        .into_iter()
        .filter(|window| window.workspace == name)
        .collect::<Vec<_>>();
    for window in windows {
        run_confirmed(
            &window.identifier,
            window.container_id,
            "move to Desktop before workspace close",
            vec!["move container to workspace \"Desktop\"".to_owned()],
            |state| state.workspace == "Desktop",
        )?;
    }
    anyhow::ensure!(
        list_windows()?
            .iter()
            .all(|window| window.workspace != name),
        "workspace {name} still owns windows; leaving its output intact"
    );
    run_global_command(&format!("output {} unplug", quote_sway(&output)?))
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

fn run_global_command(command: &str) -> Result<()> {
    let output = Command::new("swaymsg")
        .args(["-r", command])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("executing batched Sway command")?;
    anyhow::ensure!(
        output.status.success(),
        "batched Sway command failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let replies: Value =
        serde_json::from_slice(&output.stdout).context("parsing batched Sway command reply")?;
    let accepted = replies.as_array().is_some_and(|replies| {
        !replies.is_empty()
            && replies
                .iter()
                .all(|reply| reply.get("success").and_then(Value::as_bool) == Some(true))
    });
    anyhow::ensure!(accepted, "Sway rejected batched command: {replies}");
    Ok(())
}

/// Ask stock Sway to recompute pointer focus without moving the cursor.
///
/// A newly mapped layer surface does not receive `wl_pointer.enter` until the
/// seat processes a cursor update. Zero leaves both documented coordinates
/// unchanged while causing that normal focus transition; no pointer position
/// is returned through IPC or persisted anywhere.
pub fn refresh_cursor_focus() -> Result<()> {
    run_global_command("seat - cursor move 0 0")
}

fn css(color: SolidColor) -> String {
    color.to_string().to_ascii_lowercase()
}

/// Apply compositor-owned decoration colours from the same typed palette as
/// the shell. Geometry remains entirely in the pinned Sway configuration.
fn theme_command(palette: &ThemePalette) -> String {
    let focused = format!(
        "client.focused {} {} {} {} {}",
        css(palette.raised),
        css(palette.raised),
        css(palette.text),
        css(palette.focus),
        css(palette.raised),
    );
    let focused_inactive = format!(
        "client.focused_inactive {} {} {} {} {}",
        css(palette.menu),
        css(palette.menu),
        css(palette.text_secondary),
        css(palette.border),
        css(palette.menu),
    );
    let unfocused = format!(
        "client.unfocused {} {} {} {} {}",
        css(palette.menu),
        css(palette.menu),
        css(palette.text_muted),
        css(palette.border),
        css(palette.menu),
    );
    let urgent = format!(
        "client.urgent {} {} {} {} {}",
        css(palette.destructive),
        css(palette.destructive),
        css(palette.text),
        css(palette.destructive_icon),
        css(palette.destructive),
    );
    [focused, focused_inactive, unfocused, urgent].join("; ")
}

pub fn apply_theme(palette: &ThemePalette) -> Result<()> {
    run_global_command(&theme_command(palette))
}

fn remove_restore_mark_commands(state: &WindowState, commands: &mut Vec<String>) {
    commands.extend(
        state
            .restore_marks
            .iter()
            .map(|mark| format!("unmark {mark}")),
    );
}

fn frame_commands(frame: Rect, commands: &mut Vec<String>) {
    commands.push("fullscreen disable".to_owned());
    commands.push("floating enable".to_owned());
    commands.push(format!(
        "resize set width {} px height {} px",
        frame.width.max(1),
        frame.height.max(1)
    ));
    commands.push(format!(
        "move absolute position {} px {} px",
        frame.x, frame.y
    ));
}

fn run_commands(container_id: u64, commands: Vec<String>) -> Result<()> {
    run_container_command(container_id, &commands.join(", "))
}

fn run_confirmed(
    identifier: &str,
    container_id: u64,
    operation: &str,
    commands: Vec<String>,
    predicate: impl Fn(&WindowState) -> bool,
) -> Result<WindowState> {
    // Subscribe before mutating so asynchronous focus and scratchpad events
    // cannot race watcher setup. Stock Sway emits no resize event, so the
    // first authoritative tree read is also necessary for frame mutations.
    let mut events = EventSubscription::connect(&["window", "workspace", "output"])?;
    run_commands(container_id, commands)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let state = window(identifier)?;
        if predicate(&state) {
            return Ok(state);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "Sway accepted {operation} for toplevel {identifier}, but its authoritative tree \
             readback did not confirm it: {state:?}"
        );
        if let Err(error) = events.next_event(remaining) {
            if Instant::now() >= deadline {
                continue;
            }
            return Err(error).with_context(|| format!("waiting for Sway to confirm {operation}"));
        }
    }
}

fn clamp_to_workspace(frame: Rect, workspace: Rect) -> Rect {
    let width = frame.width.max(1).min(workspace.width.max(1));
    let height = frame.height.max(1).min(workspace.height.max(1));
    let maximum_x = workspace
        .x
        .saturating_add(workspace.width.saturating_sub(width));
    let maximum_y = workspace
        .y
        .saturating_add(workspace.height.saturating_sub(height));
    Rect {
        x: frame.x.clamp(workspace.x, maximum_x),
        y: frame.y.clamp(workspace.y, maximum_y),
        width,
        height,
    }
}

pub fn focus(identifier: &str) -> Result<()> {
    let before = window(identifier)?;
    if before.minimized {
        let shown = run_confirmed(
            identifier,
            before.container_id,
            "scratchpad restore",
            vec!["scratchpad show".to_owned()],
            |state| !state.minimized,
        )?;
        if shown.restore_frame.is_some() {
            let workspace = shown
                .workspace_rect
                .context("restored window has no usable workspace")?;
            let mut commands = Vec::new();
            frame_commands(workspace, &mut commands);
            run_confirmed(
                identifier,
                shown.container_id,
                "restored maximized frame",
                commands,
                |state| state.rect == workspace,
            )?;
        }
    }
    let state = window(identifier)?;
    run_confirmed(
        identifier,
        state.container_id,
        "focus",
        vec!["focus".to_owned()],
        |state| state.focused && !state.minimized,
    )?;
    Ok(())
}

pub fn bring_into_view(identifier: &str) -> Result<()> {
    focus(identifier)?;
    let state = window(identifier)?;
    if state.maximized || state.fullscreen {
        return Ok(());
    }
    let workspace = state
        .workspace_rect
        .context("window is not attached to the visible workspace")?;
    let visible_frame = clamp_to_workspace(state.rect, workspace);
    if visible_frame == state.rect {
        return Ok(());
    }
    let mut commands = Vec::new();
    remove_restore_mark_commands(&state, &mut commands);
    frame_commands(visible_frame, &mut commands);
    commands.push("focus".to_owned());
    run_confirmed(
        identifier,
        state.container_id,
        "bring into view",
        commands,
        |state| state.rect == visible_frame && state.focused && !state.minimized,
    )?;
    Ok(())
}

/// Clamp one newly mapped floating toplevel into its current usable workspace.
///
/// This is intentionally separate from `bring_into_view`: initial placement
/// must respect layer-shell exclusive zones without focusing the window or
/// changing the active workspace/seat. The shell calls it only for a new
/// foreign-toplevel identity, so later user-directed off-edge placement is not
/// continuously overridden.
pub fn constrain_new_window(identifier: &str) -> Result<()> {
    let state = window(identifier)?;
    if state.minimized || state.fullscreen {
        return Ok(());
    }
    let workspace = state
        .workspace_rect
        .context("new window is not attached to a usable workspace")?;
    let visible_frame = clamp_to_workspace(state.rect, workspace);
    if visible_frame == state.rect {
        return Ok(());
    }
    let mut commands = Vec::new();
    remove_restore_mark_commands(&state, &mut commands);
    frame_commands(visible_frame, &mut commands);
    run_confirmed(
        identifier,
        state.container_id,
        "constrain new window to usable workspace",
        commands,
        |state| state.rect == visible_frame && !state.minimized && !state.fullscreen,
    )?;
    Ok(())
}

/// Keep a new secondary window with the one unambiguous workspace already
/// owned by its application process, then constrain its outer frame there.
/// Modal dialogs from Thunar/Firefox/Blender otherwise land on Sway's globally
/// focused workspace even when the application itself belongs to a numbered
/// CUA output. Multiple existing workspaces are deliberately ambiguous and
/// are not guessed.
pub fn place_new_window(identifier: &str, preferred_workspace: Option<&str>) -> Result<()> {
    let windows = list_windows()?;
    let state = windows
        .iter()
        .find(|window| window.identifier == identifier)
        .with_context(|| format!("Sway has no mapped toplevel identifier {identifier}"))?;
    let destination = preferred_workspace.map(str::to_owned).or_else(|| {
        if state.pid == 0 {
            return None;
        }
        let workspaces = windows
            .iter()
            .filter(|window| {
                window.identifier != identifier
                    && window.pid == state.pid
                    && !window.minimized
                    && !window.workspace.is_empty()
                    && window.workspace != SCRATCHPAD_WORKSPACE
            })
            .map(|window| window.workspace.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        (workspaces.len() == 1)
            .then(|| (*workspaces.iter().next().expect("one workspace")).to_owned())
    });
    if let Some(workspace) = destination
        && workspace != state.workspace
    {
        move_window_to_workspace(identifier, &workspace, false)?;
    }
    constrain_new_window(identifier)
}

pub fn minimize(identifier: &str) -> Result<()> {
    let state = window(identifier)?;
    if state.minimized {
        return Ok(());
    }
    let mut commands = Vec::new();
    // A window manually moved or resized away from a prior maximized frame is
    // normal again. Do not let an old restore mark resurrect maximized state
    // when that normal window is later restored from the scratchpad.
    if !state.maximized {
        remove_restore_mark_commands(&state, &mut commands);
    }
    commands.push(if state.scratchpad {
        "scratchpad show".to_owned()
    } else {
        "move scratchpad".to_owned()
    });
    run_confirmed(
        identifier,
        state.container_id,
        "minimize",
        commands,
        |state| state.minimized,
    )?;
    Ok(())
}

pub fn minimize_all_visible() -> Result<()> {
    let states = list_windows()?
        .into_iter()
        .filter(|state| !state.minimized)
        .collect::<Vec<_>>();
    if states.is_empty() {
        return Ok(());
    }
    let identifiers = states
        .iter()
        .map(|state| state.identifier.clone())
        .collect::<Vec<_>>();
    let command = states
        .iter()
        .map(|state| {
            let mut commands = Vec::new();
            if !state.maximized {
                remove_restore_mark_commands(state, &mut commands);
            }
            commands.push(if state.scratchpad {
                "scratchpad show".to_owned()
            } else {
                "move scratchpad".to_owned()
            });
            format!("[con_id={}] {}", state.container_id, commands.join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ");

    let mut events = EventSubscription::connect(&["window", "workspace"])?;
    run_global_command(&command)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining_visible = list_windows()?
            .into_iter()
            .any(|state| identifiers.contains(&state.identifier) && !state.minimized);
        if !remaining_visible {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "Sway accepted Show Desktop, but one or more windows remained visible"
        );
        if let Err(error) = events.next_event(remaining) {
            if Instant::now() >= deadline {
                continue;
            }
            return Err(error).context("waiting for Sway to confirm Show Desktop");
        }
    }
}

pub fn maximize(identifier: &str) -> Result<()> {
    let initial = window(identifier)?;
    if initial.maximized {
        return Ok(());
    }
    if initial.minimized {
        run_confirmed(
            identifier,
            initial.container_id,
            "scratchpad restore before maximize",
            vec!["scratchpad show".to_owned()],
            |state| !state.minimized,
        )?;
    }
    let mut state = window(identifier)?;
    if state.fullscreen {
        state = run_confirmed(
            identifier,
            state.container_id,
            "fullscreen exit before maximize",
            vec!["fullscreen disable".to_owned()],
            |state| !state.fullscreen,
        )?;
    }
    let workspace = state
        .workspace_rect
        .context("window is not attached to the visible workspace")?;
    // If the window was minimized while maximized, its mark still contains
    // the true normal frame. Otherwise the current frame is the frame a later
    // Restore must recover, including after a human move or edge resize.
    let restore = if initial.minimized {
        state.restore_frame.unwrap_or(state.rect)
    } else {
        state.rect
    };
    let mut commands = Vec::new();
    remove_restore_mark_commands(&state, &mut commands);
    commands.push(format!(
        "mark --add {}",
        restore_mark(state.container_id, restore)
    ));
    frame_commands(workspace, &mut commands);
    run_confirmed(
        identifier,
        state.container_id,
        "maximize",
        commands,
        |state| state.maximized && state.rect == workspace,
    )?;
    Ok(())
}

pub fn toggle_maximize(identifier: &str) -> Result<()> {
    let state = window(identifier)?;
    if state.minimized || state.maximized {
        restore(identifier)
    } else {
        maximize(identifier)
    }
}

pub fn restore(identifier: &str) -> Result<()> {
    let initial = window(identifier)?;
    if initial.minimized {
        run_confirmed(
            identifier,
            initial.container_id,
            "scratchpad restore",
            vec!["scratchpad show".to_owned()],
            |state| !state.minimized,
        )?;
    }
    let state = window(identifier)?;
    let mut commands = Vec::new();
    if let Some(restore_frame) = state.restore_frame {
        let workspace = state
            .workspace_rect
            .context("restored window has no usable workspace")?;
        let frame = clamp_to_workspace(restore_frame, workspace);
        remove_restore_mark_commands(&state, &mut commands);
        frame_commands(frame, &mut commands);
    } else if state.fullscreen {
        commands.push("fullscreen disable".to_owned());
    }
    commands.push("focus".to_owned());
    run_confirmed(
        identifier,
        state.container_id,
        "restore",
        commands,
        |state| !state.minimized && !state.maximized && state.focused,
    )?;
    Ok(())
}

pub fn close(identifier: &str) -> Result<()> {
    let state = window(identifier)?;
    let mut events = EventSubscription::connect(&["window"])?;
    run_container_command(state.container_id, "kill")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let present = list_windows()?
            .into_iter()
            .any(|window| window.identifier == identifier);
        if !present {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(
            !remaining.is_zero(),
            "Sway did not confirm that toplevel {identifier} closed"
        );
        if let Err(error) = events.next_event(remaining) {
            if Instant::now() >= deadline {
                continue;
            }
            return Err(error).context("waiting for Sway to confirm close");
        }
    }
}

struct EventSubscription {
    stream: UnixStream,
}

impl EventSubscription {
    fn connect(events: &[&str]) -> Result<Self> {
        let socket = std::env::var_os("SWAYSOCK").context("SWAYSOCK is unavailable")?;
        let mut stream = UnixStream::connect(&socket).with_context(|| {
            format!("connecting to Sway IPC socket {}", socket.to_string_lossy())
        })?;
        let payload = serde_json::to_vec(events).context("encoding Sway subscription")?;
        write_ipc_message(&mut stream, IPC_SUBSCRIBE, &payload)?;
        let (message_type, response) = read_ipc_message(&mut stream)?;
        anyhow::ensure!(
            message_type == IPC_SUBSCRIBE,
            "unexpected Sway subscribe response type {message_type}"
        );
        let response: Value =
            serde_json::from_slice(&response).context("parsing Sway subscribe response")?;
        anyhow::ensure!(
            response.get("success").and_then(Value::as_bool) == Some(true),
            "Sway rejected event subscription: {response}"
        );
        Ok(Self { stream })
    }

    fn next_event(&mut self, timeout: Duration) -> Result<()> {
        self.stream
            .set_read_timeout(Some(timeout))
            .context("setting Sway event timeout")?;
        let (message_type, _) =
            read_ipc_message(&mut self.stream).context("waiting for Sway window event")?;
        anyhow::ensure!(
            message_type & IPC_EVENT_MASK != 0,
            "unexpected non-event Sway IPC message type {message_type}"
        );
        Ok(())
    }
}

fn write_ipc_message(stream: &mut UnixStream, message_type: u32, payload: &[u8]) -> Result<()> {
    let length = u32::try_from(payload.len()).context("Sway IPC payload is too large")?;
    stream.write_all(IPC_MAGIC)?;
    stream.write_all(&length.to_ne_bytes())?;
    stream.write_all(&message_type.to_ne_bytes())?;
    stream.write_all(payload)?;
    Ok(())
}

fn read_ipc_message(stream: &mut UnixStream) -> Result<(u32, Vec<u8>)> {
    let mut header = [0_u8; 14];
    stream.read_exact(&mut header)?;
    anyhow::ensure!(&header[..6] == IPC_MAGIC, "invalid Sway IPC magic");
    let length = u32::from_ne_bytes(header[6..10].try_into().expect("four bytes")) as usize;
    anyhow::ensure!(
        length <= MAX_IPC_PAYLOAD,
        "Sway IPC payload length {length} exceeds the safety limit"
    );
    let message_type = u32::from_ne_bytes(header[10..14].try_into().expect("four bytes"));
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload)?;
    Ok((message_type, payload))
}

pub fn subscribe_window_changes() -> Result<Receiver<()>> {
    let mut subscription = EventSubscription::connect(&["window", "workspace", "output"])?;
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("buzzardos-sway-events".to_owned())
        .spawn(move || {
            loop {
                match subscription.next_event(Duration::from_secs(24 * 60 * 60)) {
                    Ok(()) if sender.send(()).is_err() => break,
                    Ok(()) => {}
                    Err(error) => {
                        eprintln!("buzzardos-shell: Sway event subscription ended: {error:#}");
                        break;
                    }
                }
            }
        })
        .context("starting Sway event subscription")?;
    Ok(receiver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzzardos_desktop_core::ThemeMode;

    #[test]
    fn decoration_commands_change_only_typed_palette_values() {
        let dark = theme_command(ThemeMode::Dark.palette());
        let light = theme_command(ThemeMode::Light.palette());
        for command in [
            "client.focused",
            "client.focused_inactive",
            "client.unfocused",
            "client.urgent",
        ] {
            assert!(dark.contains(command));
            assert!(light.contains(command));
        }
        assert!(dark.contains("#ff9b73"));
        assert!(light.contains("#b53b12"));
        assert!(!dark.contains("#ffffff"));
        assert!(!light.contains("#ffffff"));
    }

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
                "marks":["__buzzardos_restore_v1_7_200_100_900_700"],
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
        assert_eq!(windows[0].decoration_height, 25);
        assert!(windows[1].minimized);
        assert!(!windows[1].maximized);
    }

    #[test]
    fn nonzero_workspace_geometry_uses_absolute_outer_frame_and_restore_mark() {
        let tree = br#"{
          "type":"root","rect":{"x":0,"y":0,"width":2880,"height":900},
          "nodes":[{"type":"output","rect":{"x":1600,"y":0,"width":1280,"height":900},
            "nodes":[{"type":"workspace","name":"1",
              "rect":{"x":1600,"y":0,"width":1280,"height":858},
              "floating_nodes":[{
                "id":19,"foreign_toplevel_identifier":"xwayland-id",
                "rect":{"x":1600,"y":25,"width":1280,"height":833},
                "deco_rect":{"x":0,"y":0,"width":1280,"height":25},
                "marks":["__buzzardos_restore_v1_19_1720_90_800_600"],
                "scratchpad_state":"none","fullscreen_mode":0
              }]
            }]
          }]
        }"#;
        let window = parse_tree(tree).unwrap().remove(0);
        assert_eq!(
            window.rect,
            Rect {
                x: 1600,
                y: 0,
                width: 1280,
                height: 858,
            }
        );
        assert_eq!(window.rect, window.workspace_rect.unwrap());
        assert!(window.maximized);
        assert_eq!(
            window.restore_frame,
            Some(Rect {
                x: 1720,
                y: 90,
                width: 800,
                height: 600,
            })
        );
    }

    #[test]
    fn frame_mutation_uses_root_absolute_coordinates() {
        let mut commands = Vec::new();
        frame_commands(
            Rect {
                x: 1600,
                y: 12,
                width: 1280,
                height: 846,
            },
            &mut commands,
        );
        assert_eq!(
            commands,
            [
                "fullscreen disable",
                "floating enable",
                "resize set width 1280 px height 846 px",
                "move absolute position 1600 px 12 px",
            ]
        );
    }

    #[test]
    fn restore_frame_is_clamped_into_resized_usable_workspace() {
        assert_eq!(
            clamp_to_workspace(
                Rect {
                    x: 1500,
                    y: -20,
                    width: 1400,
                    height: 900,
                },
                Rect {
                    x: 1600,
                    y: 0,
                    width: 1000,
                    height: 700,
                },
            ),
            Rect {
                x: 1600,
                y: 0,
                width: 1000,
                height: 700,
            }
        );
    }

    #[test]
    fn offscreen_window_is_clamped_back_into_the_usable_workspace() {
        assert_eq!(
            clamp_to_workspace(
                Rect {
                    x: 1900,
                    y: 900,
                    width: 640,
                    height: 480,
                },
                Rect {
                    x: 0,
                    y: 0,
                    width: 1600,
                    height: 958,
                },
            ),
            Rect {
                x: 960,
                y: 478,
                width: 640,
                height: 480,
            }
        );
    }

    #[test]
    fn a_workspace_sized_window_without_our_restore_mark_is_not_maximized() {
        let tree = br#"{
          "type":"root","nodes":[{"type":"output","nodes":[{
            "type":"workspace","name":"1","rect":{"x":0,"y":0,"width":800,"height":558},
            "floating_nodes":[{
              "id":4,"foreign_toplevel_identifier":"plain",
              "rect":{"x":0,"y":25,"width":800,"height":533},
              "deco_rect":{"x":0,"y":0,"width":800,"height":25},
              "marks":[],"scratchpad_state":"none"
            }]
          }]}]
        }"#;
        let window = parse_tree(tree).unwrap().remove(0);
        assert_eq!(window.rect, window.workspace_rect.unwrap());
        assert!(!window.maximized);
    }
}
