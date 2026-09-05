//! Exact Sway IPC metadata and window operations for Buzzard OS.
//!
//! Sway's IPC tree is the authority for global frame geometry and state.  The
//! public CUA id is bound to Sway's opaque `foreign_toplevel_identifier`; title,
//! app-id, PID, and Wayland object-id heuristics are never used to retarget it.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

const SCRATCHPAD_WORKSPACE: &str = "__i3_scratch";
const RESTORE_MARK_PREFIX: &str = "__buzzardos_restore_v1_";
const IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const IPC_SUBSCRIBE: u32 = 2;
const IPC_EVENT_MASK: u32 = 1 << 31;
const MAX_IPC_PAYLOAD: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
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

fn null_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Clone, Debug, Default, Deserialize)]
struct Node {
    id: u64,
    #[serde(default, rename = "type", deserialize_with = "null_string")]
    node_type: String,
    #[serde(default, deserialize_with = "null_string")]
    name: String,
    #[serde(default, deserialize_with = "null_string")]
    app_id: String,
    #[serde(default, deserialize_with = "null_string")]
    foreign_toplevel_identifier: String,
    pid: Option<u32>,
    #[serde(default)]
    rect: Rect,
    #[serde(default)]
    window_rect: Rect,
    #[serde(default)]
    deco_rect: Rect,
    #[serde(default)]
    current_border_width: i32,
    #[serde(default)]
    focused: bool,
    #[serde(default = "default_visible")]
    visible: bool,
    #[serde(default)]
    fullscreen_mode: i32,
    #[serde(default, deserialize_with = "null_string")]
    scratchpad_state: String,
    #[serde(default)]
    marks: Vec<String>,
    #[serde(default)]
    nodes: Vec<Node>,
    #[serde(default)]
    floating_nodes: Vec<Node>,
}

fn default_visible() -> bool {
    true
}

#[derive(Clone, Debug, Default)]
struct Workspace {
    rect: Rect,
    scratchpad: bool,
    name: String,
    output: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Window {
    /// Sway's live internal container id, valid only for exact IPC criteria.
    pub id: u64,
    /// Sway/ext-foreign-toplevel's opaque identity for this mapped lifetime.
    pub foreign_toplevel_identifier: String,
    pub pid: u32,
    pub title: String,
    pub app_id: String,
    /// Complete compositor frame in Sway logical coordinates.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Client content relative to the complete compositor frame.
    pub content_x: i32,
    pub content_y: i32,
    pub content_width: u32,
    pub content_height: u32,
    pub border_width: u32,
    pub workspace_x: i32,
    pub workspace_y: i32,
    pub workspace_width: u32,
    pub workspace_height: u32,
    pub workspace: String,
    pub output: String,
    pub focused: bool,
    pub visible: bool,
    pub scratchpad: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    restore_frame: Option<Rect>,
    restore_marks: Vec<String>,
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

fn collect(
    node: &Node,
    parent_origin: (i32, i32),
    workspace: Option<Workspace>,
    output: Option<&str>,
    windows: &mut Vec<Window>,
) {
    let output = if node.node_type == "output" {
        Some(node.name.as_str())
    } else {
        output
    };
    let workspace = if node.node_type == "workspace" {
        Some(Workspace {
            rect: node.rect,
            scratchpad: node.name == SCRATCHPAD_WORKSPACE,
            name: node.name.clone(),
            output: output.unwrap_or_default().to_owned(),
        })
    } else {
        workspace
    };

    if let Some(pid) = node.pid {
        if !node.name.is_empty()
            || !node.app_id.is_empty()
            || !node.foreign_toplevel_identifier.is_empty()
        {
            // sway-ipc(7) defines `rect` as absolute and decoration-excluding,
            // while `deco_rect` is relative to the parent node.  Reconstruct
            // the absolute decoration before taking the exact bounding union.
            let decoration = Rect {
                x: parent_origin.0.saturating_add(node.deco_rect.x),
                y: parent_origin.1.saturating_add(node.deco_rect.y),
                width: node.deco_rect.width,
                height: node.deco_rect.height,
            };
            let outer = node.rect.union(decoration);
            let content_absolute_x = node.rect.x.saturating_add(node.window_rect.x);
            let content_absolute_y = node.rect.y.saturating_add(node.window_rect.y);
            let workspace = workspace.clone().unwrap_or_default();
            let scratchpad = node.scratchpad_state == "fresh";
            let restore_marks = node
                .marks
                .iter()
                .filter(|mark| parse_restore_mark(mark, node.id).is_some())
                .cloned()
                .collect::<Vec<_>>();
            let restore_frame = restore_marks
                .iter()
                .find_map(|mark| parse_restore_mark(mark, node.id));
            // A shown scratchpad window remains tagged "fresh".  Only the
            // synthetic __i3_scratch workspace means it is actually hidden.
            let minimized = scratchpad && workspace.scratchpad;
            let maximized = !minimized
                && !workspace.scratchpad
                && restore_frame.is_some()
                && workspace.rect.width > 0
                && workspace.rect.height > 0
                && outer == workspace.rect;
            windows.push(Window {
                id: node.id,
                foreign_toplevel_identifier: node.foreign_toplevel_identifier.clone(),
                pid,
                title: node.name.clone(),
                app_id: node.app_id.clone(),
                x: outer.x,
                y: outer.y,
                width: outer.width.max(0) as u32,
                height: outer.height.max(0) as u32,
                content_x: content_absolute_x.saturating_sub(outer.x),
                content_y: content_absolute_y.saturating_sub(outer.y),
                content_width: node.window_rect.width.max(0) as u32,
                content_height: node.window_rect.height.max(0) as u32,
                border_width: node.current_border_width.max(0) as u32,
                workspace_x: workspace.rect.x,
                workspace_y: workspace.rect.y,
                workspace_width: workspace.rect.width.max(0) as u32,
                workspace_height: workspace.rect.height.max(0) as u32,
                workspace: workspace.name.clone(),
                output: workspace.output.clone(),
                focused: node.focused,
                visible: node.visible && !minimized,
                scratchpad,
                minimized,
                maximized,
                fullscreen: node.fullscreen_mode != 0,
                restore_frame,
                restore_marks,
            });
        }
    }

    // A container with children has no view/titlebar in the supported Sway
    // tree shape, so its absolute rect origin is the parent origin Sway uses
    // for each child's parent-relative deco_rect.
    let child_origin = (node.rect.x, node.rect.y);
    for child in node.nodes.iter().chain(&node.floating_nodes) {
        collect(child, child_origin, workspace.clone(), output, windows);
    }
}

fn parse_tree(bytes: &[u8]) -> anyhow::Result<Vec<Window>> {
    let root: Node = serde_json::from_slice(bytes)
        .map_err(|error| anyhow::anyhow!("invalid Sway tree: {error}"))?;
    let mut windows = Vec::new();
    collect(&root, (root.rect.x, root.rect.y), None, None, &mut windows);
    Ok(windows)
}

fn list_windows_result() -> anyhow::Result<Vec<Window>> {
    if std::env::var_os("SWAYSOCK").is_none() {
        anyhow::bail!("SWAYSOCK is unavailable");
    }
    let output = Command::new("swaymsg")
        .args(["-r", "-t", "get_tree"])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| anyhow::anyhow!("could not execute swaymsg get_tree: {error}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "swaymsg get_tree failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_tree(&output.stdout)
}

pub fn list_windows() -> Option<Vec<Window>> {
    list_windows_result().ok()
}

#[derive(Clone, Debug, Default)]
struct WorkspaceInfo {
    name: String,
    output: String,
}

#[derive(Clone, Debug, Default)]
struct OutputInfo {
    id: u64,
    name: String,
    active: bool,
    rect: Rect,
    physical_width: u32,
    physical_height: u32,
    refresh_millihz: u32,
    scale_milli: u32,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct SeatInfo {
    name: String,
    focus: u64,
}

fn swaymsg_value(message_type: &str) -> anyhow::Result<serde_json::Value> {
    let output = Command::new("swaymsg")
        .args(["-r", "-t", message_type])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| anyhow::anyhow!("could not execute swaymsg {message_type}: {error}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "swaymsg {message_type} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| anyhow::anyhow!("invalid Sway {message_type} response: {error}"))
}

fn workspaces() -> anyhow::Result<Vec<WorkspaceInfo>> {
    let value = swaymsg_value("get_workspaces")?;
    Ok(value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Sway get_workspaces did not return an array"))?
        .iter()
        .filter_map(|value| {
            Some(WorkspaceInfo {
                name: value.get("name")?.as_str()?.to_owned(),
                output: value.get("output")?.as_str()?.to_owned(),
            })
        })
        .collect())
}

fn json_rect(value: Option<&serde_json::Value>) -> Rect {
    let value = value.unwrap_or(&serde_json::Value::Null);
    Rect {
        x: value
            .get("x")
            .and_then(serde_json::Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or_default(),
        y: value
            .get("y")
            .and_then(serde_json::Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or_default(),
        width: value
            .get("width")
            .and_then(serde_json::Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or_default(),
        height: value
            .get("height")
            .and_then(serde_json::Value::as_i64)
            .and_then(|value| i32::try_from(value).ok())
            .unwrap_or_default(),
    }
}

fn outputs() -> anyhow::Result<Vec<OutputInfo>> {
    let value = swaymsg_value("get_outputs")?;
    Ok(value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Sway get_outputs did not return an array"))?
        .iter()
        .filter_map(|value| {
            let mode = value
                .get("current_mode")
                .unwrap_or(&serde_json::Value::Null);
            let scale = value
                .get("scale")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(1.0);
            Some(OutputInfo {
                id: value.get("id").and_then(serde_json::Value::as_u64)?,
                name: value.get("name")?.as_str()?.to_owned(),
                active: value
                    .get("active")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                rect: json_rect(value.get("rect")),
                physical_width: mode
                    .get("width")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or_default(),
                physical_height: mode
                    .get("height")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or_default(),
                refresh_millihz: mode
                    .get("refresh")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or(60_000),
                scale_milli: (scale * 1000.0).round().clamp(1.0, f64::from(u32::MAX)) as u32,
            })
        })
        .collect())
}

/// Return the Sway-logical origin of the output selected for this numbered
/// CUA invocation. Sway exposes all virtual outputs in one global layout,
/// while screencopy and output-bound virtual input both use coordinates local
/// to the selected output. Keeping this translation beside the authoritative
/// `get_outputs` parser prevents every input/capture caller from inventing its
/// own multi-output arithmetic.
pub fn caller_output_origin() -> anyhow::Result<(i32, i32)> {
    let expected = std::env::var(crate::core::seat_context::CUA_OUTPUT_ENV)
        .map_err(|_| anyhow::anyhow!("numbered CUA output identity is unavailable"))?;
    let output = outputs()?
        .into_iter()
        .find(|output| output.active && output.name == expected)
        .ok_or_else(|| anyhow::anyhow!("Sway did not expose active caller output {expected}"))?;
    Ok((output.rect.x, output.rect.y))
}

/// Return each public window's origin in its own output's Sway-logical space.
/// Global window discovery spans every CUA output, but each published frame is
/// defined in the native `(0,0)` coordinate space of the `output` named beside
/// it. This prevents a cuaN caller from receiving large negative coordinates
/// for otherwise valid windows on another agent output.
pub fn public_window_output_origins() -> anyhow::Result<std::collections::HashMap<u64, (i32, i32)>>
{
    let outputs = outputs()?
        .into_iter()
        .filter(|output| output.active)
        .map(|output| (output.name, (output.rect.x, output.rect.y)))
        .collect::<std::collections::HashMap<_, _>>();
    Ok(list_windows_result()?
        .into_iter()
        .filter_map(|window| {
            outputs
                .get(&window.output)
                .copied()
                .map(|origin| (window.id, origin))
        })
        .collect())
}

fn seats() -> anyhow::Result<Vec<SeatInfo>> {
    let value = swaymsg_value("get_seats")?;
    Ok(value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Sway get_seats did not return an array"))?
        .iter()
        .filter_map(|value| {
            Some(SeatInfo {
                name: value.get("name")?.as_str()?.to_owned(),
                focus: value.get("focus")?.as_u64()?,
            })
        })
        .collect())
}

/// Refuse focus-bound input unless Sway confirms that this invocation's
/// numbered seat currently focuses a real window on its own CUA output.
///
/// Sway retains a seat's container focus when that container is moved to a
/// different output. Without this readback, a later untargeted `cuaN`
/// keystroke could follow the stale focus into another agent's workspace.
pub fn require_caller_seat_focus(expected_window_id: Option<u64>) -> anyhow::Result<Window> {
    let index = crate::core::seat_context::current_index();
    let seat_name = format!("seat{index}");
    let expected_workspace = cua_workspace_name(index)?;
    let expected_output = workspaces()?
        .into_iter()
        .find(|workspace| workspace.name == expected_workspace)
        .map(|workspace| workspace.output)
        .ok_or_else(|| anyhow::anyhow!("{expected_workspace} has no active output"))?;
    let focused_id = seats()?
        .into_iter()
        .find(|seat| seat.name == seat_name)
        .map(|seat| seat.focus)
        .ok_or_else(|| anyhow::anyhow!("Sway did not expose {seat_name}"))?;
    if let Some(expected) = expected_window_id {
        anyhow::ensure!(
            focused_id == expected,
            "{seat_name} focus is {focused_id}, not requested window_id {expected}"
        );
    }
    let focused = resolve_public_window(focused_id, None).map_err(|_| {
        anyhow::anyhow!(
            "{seat_name} has no focused application on {expected_workspace}; refusing ambiguous input"
        )
    })?;
    anyhow::ensure!(
        focused.workspace == expected_workspace && focused.output == expected_output,
        "{seat_name} focus points to {} on {}; refusing input outside {expected_workspace} on {expected_output}",
        focused.workspace,
        focused.output
    );
    Ok(focused)
}

/// Confirm the exact per-seat container identity independently of geometry.
/// The full caller-output check follows successful activation.
pub fn require_caller_seat_exact_focus(expected_window_id: u64) -> anyhow::Result<Window> {
    let index = crate::core::seat_context::current_index();
    let seat_name = format!("seat{index}");
    let focused_id = seats()?
        .into_iter()
        .find(|seat| seat.name == seat_name)
        .map(|seat| seat.focus)
        .ok_or_else(|| anyhow::anyhow!("Sway did not expose {seat_name}"))?;
    anyhow::ensure!(
        focused_id == expected_window_id,
        "{seat_name} focus is {focused_id}, not requested window_id {expected_window_id}"
    );
    resolve_public_window(focused_id, None)
}

fn find_node(node: &Node, id: u64) -> Option<&Node> {
    if node.id == id {
        return Some(node);
    }
    node.nodes
        .iter()
        .chain(&node.floating_nodes)
        .find_map(|child| find_node(child, id))
}

fn fullscreen_obstruction(
    root: &Node,
    target_id: u64,
    workspace: &str,
) -> anyhow::Result<Option<u64>> {
    fn find_workspace<'a>(node: &'a Node, name: &str) -> Option<&'a Node> {
        if node.node_type == "workspace" && node.name == name {
            return Some(node);
        }
        node.nodes
            .iter()
            .chain(&node.floating_nodes)
            .find_map(|child| find_workspace(child, name))
    }
    fn obstruction(node: &Node, target_id: u64) -> Option<u64> {
        // Workspace nodes also report fullscreen_mode=1 even without a
        // fullscreen client. Only a container can obstruct another container.
        if matches!(node.node_type.as_str(), "con" | "floating_con") && node.fullscreen_mode != 0 {
            return find_node(node, target_id).is_none().then_some(node.id);
        }
        node.nodes
            .iter()
            .chain(&node.floating_nodes)
            .find_map(|child| obstruction(child, target_id))
    }
    let workspace = find_workspace(root, workspace)
        .ok_or_else(|| anyhow::anyhow!("caller workspace {workspace} is unavailable"))?;
    anyhow::ensure!(
        find_node(workspace, target_id).is_some(),
        "window_id {target_id} is not on caller workspace {}",
        workspace.name
    );
    Ok(obstruction(workspace, target_id))
}

/// Recover a seat-specific activation rejected by workspace fullscreen.
/// Stock Sway's ordinary `focus` command performs this step, but its
/// foreign-toplevel activation handler does not. Never issue IPC `focus`
/// (which selects the default seat), and never clear another workspace's
/// fullscreen state. Called only after native activation failed, so permitted
/// transient dialogs keep their parent's fullscreen state.
pub fn exit_caller_fullscreen_obstruction(
    id: u64,
    expected_pid: Option<u32>,
) -> anyhow::Result<bool> {
    let workspace = cua_workspace_name(crate::core::seat_context::current_index())?;
    let target = resolve_public_window(id, expected_pid)?;
    anyhow::ensure!(
        target.workspace == workspace,
        "refusing fullscreen changes outside caller workspace {workspace}"
    );
    let tree: Node = serde_json::from_value(swaymsg_value("get_tree")?)?;
    let Some(blocker) = fullscreen_obstruction(&tree, id, &workspace)? else {
        return Ok(false);
    };
    run_container_command(blocker, "fullscreen disable")?;
    let after: Node = serde_json::from_value(swaymsg_value("get_tree")?)?;
    anyhow::ensure!(
        find_node(&after, blocker).is_none_or(|node| node.fullscreen_mode == 0),
        "Sway did not confirm fullscreen exit for container {blocker}"
    );
    let current = resolve_public_window(id, expected_pid)?;
    anyhow::ensure!(
        current.workspace == workspace
            && current.foreign_toplevel_identifier == target.foreign_toplevel_identifier,
        "focus target changed while exiting fullscreen"
    );
    Ok(true)
}

fn safe_name(value: &str) -> anyhow::Result<String> {
    if value.is_empty()
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, ' ' | '_' | '-')
        })
    {
        anyhow::bail!("unsafe Sway identifier");
    }
    Ok(format!("\"{value}\""))
}

fn run_global_command(command: &str) -> anyhow::Result<()> {
    let output = Command::new("swaymsg")
        .args(["-r", command])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| anyhow::anyhow!("could not execute swaymsg command: {error}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "swaymsg command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let replies: Vec<CommandReply> = serde_json::from_slice(&output.stdout)
        .map_err(|error| anyhow::anyhow!("invalid swaymsg command reply: {error}"))?;
    if replies.is_empty()
        || replies
            .iter()
            .any(|reply| !reply.success || reply.parse_error)
    {
        anyhow::bail!(
            "Sway rejected command: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Ok(())
}

fn lock_workspace_creation() -> anyhow::Result<File> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR is unavailable"))?;
    let root = std::path::PathBuf::from(runtime).join("buzzardoscua");
    let metadata = fs::symlink_metadata(&root)?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "Buzzard CUA runtime directory is not private to the guest user"
    );
    // Output creation is the only shared transaction. Existing numbered
    // workspaces retain their independent per-seat operation locks.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(root.join("workspace-layout.lock"))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "Buzzard CUA workspace lock is not private to the guest user"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let error = std::io::Error::last_os_error();
        anyhow::ensure!(
            error.kind() == std::io::ErrorKind::WouldBlock,
            "locking CUA workspace creation: {error}"
        );
        anyhow::ensure!(
            Instant::now() < deadline,
            "CUA workspace creation is busy with another bounded operation"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn new_output_commands(
    index: u32,
    before: &[OutputInfo],
    created: &OutputInfo,
    initial_workspace: &str,
) -> anyhow::Result<Vec<String>> {
    let workspace_name = cua_workspace_name(index)?;
    let primary = before
        .iter()
        .filter(|output| output.active)
        .min_by_key(|output| output.id)
        .ok_or_else(|| anyhow::anyhow!("Sway has no host-facing output to mirror"))?;
    anyhow::ensure!(
        created.active && before.iter().all(|output| output.name != created.name),
        "refusing to reconfigure an existing or inactive output for CUA"
    );
    let x = before
        .iter()
        .filter(|output| output.active)
        .map(|output| output.rect.right())
        .max()
        .ok_or_else(|| anyhow::anyhow!("Sway has no active output layout"))?;
    Ok(vec![
        format!(
            "output {} mode {}x{}@{:.3}Hz scale {:.3} pos {x} 0",
            safe_name(&created.name)?,
            primary.physical_width.max(1),
            primary.physical_height.max(1),
            f64::from(primary.refresh_millihz) / 1000.0,
            f64::from(primary.scale_milli) / 1000.0,
        ),
        format!("seat \"seat{index}\" fallback false"),
        agent_cursor_command(index)?,
        // Stock Sway creates a workspace on a newly enabled output. Rename
        // that exact workspace in place; `workspace NAME` would select it
        // through the IPC default seat and disturb human focus.
        format!(
            "rename workspace {} to {}",
            safe_name(initial_workspace)?,
            safe_name(&workspace_name)?
        ),
    ])
}

pub fn cua_workspace_name(index: u32) -> anyhow::Result<String> {
    if index == 0 {
        anyhow::bail!("seat0 is reserved for the human Desktop");
    }
    Ok(if index == 1 {
        "CUA".to_owned()
    } else {
        format!("CUA{index}")
    })
}

fn cua_workspace_index(name: &str) -> Option<u32> {
    match name {
        "CUA" => Some(1),
        _ => name
            .strip_prefix("CUA")
            .filter(|suffix| !suffix.is_empty() && !suffix.starts_with('0'))
            .and_then(|suffix| suffix.parse::<u32>().ok())
            .filter(|index| *index >= 2),
    }
}

fn agent_cursor_command(index: u32) -> anyhow::Result<String> {
    cua_workspace_name(index)?; // seat0 must never receive the agent theme.
    Ok(format!(
        "seat \"seat{index}\" xcursor_theme BuzzardOS-Agent 24"
    ))
}

pub fn ensure_cua_workspace(index: u32) -> anyhow::Result<String> {
    let workspace_name = cua_workspace_name(index)?;
    if let Some(workspace) = workspaces()?
        .into_iter()
        .find(|workspace| workspace.name == workspace_name)
    {
        return Ok(workspace.output);
    }
    let _creation_lock = lock_workspace_creation()?;
    let before_workspaces = workspaces()?;
    if let Some(workspace) = before_workspaces
        .iter()
        .find(|workspace| workspace.name == workspace_name)
    {
        return Ok(workspace.output.clone());
    }
    let before = outputs()?;
    anyhow::ensure!(
        before.iter().any(|output| output.active),
        "Sway has no active output"
    );
    let before_names = before
        .iter()
        .map(|output| output.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    run_global_command("create_output")?;
    let after = outputs()?;
    let created = after
        .iter()
        .filter(|output| !before_names.contains(output.name.as_str()))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        created.len() == 1,
        "Sway output creation did not identify exactly one new output"
    );
    let created = created[0];
    let initial_workspaces = workspaces()?
        .into_iter()
        .filter(|workspace| workspace.output == created.name)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        initial_workspaces.len() == 1
            && before_workspaces
                .iter()
                .all(|workspace| { workspace.name != initial_workspaces[0].name }),
        "new CUA output does not own one newly created workspace"
    );
    let commands = new_output_commands(index, &before, created, &initial_workspaces[0].name)?;
    run_global_command(&commands.join("; "))?;
    let workspace = workspaces()?
        .into_iter()
        .find(|workspace| workspace.name == workspace_name)
        .ok_or_else(|| anyhow::anyhow!("Sway did not create {workspace_name}"))?;
    anyhow::ensure!(
        workspace.output == created.name,
        "Sway did not preserve {workspace_name} on its new output {}",
        created.name
    );
    Ok(workspace.output)
}

pub fn move_public_window_to_cua(
    id: u64,
    expected_pid: Option<u32>,
    index: u32,
) -> anyhow::Result<Window> {
    let workspace_name = cua_workspace_name(index)?;
    ensure_cua_workspace(index)?;
    let window = resolve_public_window(id, expected_pid)?;
    let _source_lock = cua_workspace_index(&window.workspace)
        .filter(|source| *source != index)
        .map(crate::core::seat_context::try_lock_other)
        .transpose()?
        .flatten();
    if window.workspace != workspace_name {
        run_container_command(
            window.id,
            &format!(
                "move container to workspace {}",
                safe_name(&workspace_name)?
            ),
        )?;
    }
    let moved = resolve_public_window(id, expected_pid)?;
    anyhow::ensure!(
        moved.workspace == workspace_name,
        "Sway accepted the move but window_id {id} remains on {}",
        moved.workspace
    );
    if moved.fullscreen {
        return Ok(moved);
    }
    let current = Rect {
        x: moved.x,
        y: moved.y,
        width: i32::try_from(moved.width).unwrap_or(i32::MAX),
        height: i32::try_from(moved.height).unwrap_or(i32::MAX),
    };
    let visible = clamp_restore_frame(current, &moved);
    if visible != current {
        set_window_frame_checked(
            moved.id,
            visible.x,
            visible.y,
            u32::try_from(visible.width).unwrap_or(1),
            u32::try_from(visible.height).unwrap_or(1),
        )?;
    }
    resolve_public_window(id, expected_pid)
}

pub fn is_public_window_id(id: u64) -> bool {
    list_windows_result()
        .ok()
        .is_some_and(|windows| windows.iter().any(|window| window.id == id))
}

pub fn list_public_windows() -> anyhow::Result<Vec<(u64, Window)>> {
    Ok(list_windows_result()?
        .into_iter()
        .map(|window| (window.id, window))
        .collect())
}

/// Resolve one CUA id through Sway's opaque identifier and verify its owner.
pub fn resolve_public_window(id: u64, expected_pid: Option<u32>) -> anyhow::Result<Window> {
    let mut matches = list_windows_result()?
        .into_iter()
        .filter(|window| window.id == id);
    let window = matches
        .next()
        .ok_or_else(|| anyhow::anyhow!("unknown or stale Sway window_id {id}"))?;
    if matches.next().is_some() {
        anyhow::bail!("Sway published duplicate container id {id}");
    }
    if expected_pid.is_some_and(|pid| window.pid != pid) {
        anyhow::bail!(
            "window_id {id} belongs to pid {}, not requested pid {}",
            window.pid,
            expected_pid.expect("checked Some")
        );
    }
    Ok(window)
}

/// Ask the optional Buzzard desktop shell to open controls for an exact
/// titlebar target. Numbered CUA seats do not change Sway's legacy global
/// `focused` flag (that belongs to human seat0), so the human titlebar helper
/// cannot discover a CUA2/CUA3 target through that flag. The request contains
/// only Sway's opaque mapped-window identity; the shell's transient surface
/// still receives its horizontal position from an ordinary pointer-enter.
pub fn request_titlebar_menu_if_hit(id: u64, local_y: i32) -> anyhow::Result<bool> {
    let window = resolve_public_window(id, None)?;
    if local_y < 0 || local_y >= window.content_y.max(0) {
        return Ok(false);
    }
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return Ok(false);
    };
    let destination = std::path::PathBuf::from(runtime).join("buzzardos-shell-control.sock");
    if !destination.exists() {
        return Ok(false);
    }
    let payload = serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "identifier": window.foreign_toplevel_identifier,
    }))?;
    std::os::unix::net::UnixDatagram::unbound()?.send_to(&payload, destination)?;
    Ok(true)
}

pub fn window_for_id(id: u64) -> Option<Window> {
    window_for_id_result(id).ok().flatten()
}

fn window_for_id_result(id: u64) -> anyhow::Result<Option<Window>> {
    Ok(list_windows_result()?
        .into_iter()
        .find(|window| window.id == id))
}

pub fn window_for_pid(pid: u32) -> Option<Window> {
    let mut matches = list_windows()?
        .into_iter()
        .filter(|window| window.pid == pid && window.width > 0 && window.height > 0);
    let one = matches.next()?;
    matches.next().is_none().then_some(one)
}

pub fn window_for_title(title: &str) -> Option<Window> {
    let mut matches = list_windows()?.into_iter().filter(|window| {
        window.width > 0 && window.height > 0 && !title.is_empty() && window.title == title
    });
    let one = matches.next()?;
    matches.next().is_none().then_some(one)
}

pub fn window_for_app_id(app_id: &str) -> Option<Window> {
    let mut matches = list_windows()?.into_iter().filter(|window| {
        window.width > 0 && window.height > 0 && !app_id.is_empty() && window.app_id == app_id
    });
    let one = matches.next()?;
    matches.next().is_none().then_some(one)
}

#[derive(Debug, Deserialize)]
struct CommandReply {
    success: bool,
    #[serde(default, deserialize_with = "null_string")]
    error: String,
    #[serde(default)]
    parse_error: bool,
}

fn validate_command_reply(bytes: &[u8], id: u64) -> anyhow::Result<()> {
    let replies: Vec<CommandReply> = serde_json::from_slice(bytes)
        .map_err(|error| anyhow::anyhow!("invalid swaymsg command reply: {error}"))?;
    if replies.is_empty() {
        anyhow::bail!("swaymsg returned no command result");
    }
    if let Some(reply) = replies
        .iter()
        .find(|reply| !reply.success || reply.parse_error)
    {
        anyhow::bail!(
            "Sway rejected command for exact container {id}: {}",
            if reply.error.is_empty() {
                "unspecified compositor error"
            } else {
                &reply.error
            }
        );
    }
    Ok(())
}

fn run_container_command(id: u64, command: &str) -> anyhow::Result<()> {
    let selector = format!("[con_id={id}]");
    let output = Command::new("swaymsg")
        .args(["-r", selector.as_str(), command])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| anyhow::anyhow!("could not execute swaymsg: {error}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "swaymsg exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    validate_command_reply(&output.stdout, id)
}

fn remove_restore_mark_commands(window: &Window, commands: &mut Vec<String>) {
    commands.extend(
        window
            .restore_marks
            .iter()
            .map(|mark| format!("unmark {mark}")),
    );
}

fn frame_commands(x: i32, y: i32, width: u32, height: u32, commands: &mut Vec<String>) {
    commands.push("fullscreen disable".to_owned());
    commands.push("floating enable".to_owned());
    commands.push(format!("resize set width {width} px height {height} px"));
    commands.push(format!("move absolute position {x} px {y} px"));
}

fn run_commands(id: u64, commands: Vec<String>) -> anyhow::Result<()> {
    if commands.is_empty() {
        return Ok(());
    }
    run_container_command(id, &commands.join(", "))
}

fn run_confirmed(
    id: u64,
    operation: &str,
    commands: Vec<String>,
    predicate: impl Fn(&Window) -> bool,
) -> anyhow::Result<Window> {
    // Subscribe before mutation. Sway's floating resize path does not emit a
    // resize event, so the initial authoritative tree read is also required.
    let mut events = EventSubscription::connect(&["window", "workspace", "output"])?;
    run_commands(id, commands)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let window = window_for_id_result(id)?
            .ok_or_else(|| anyhow::anyhow!("Sway container {id} is no longer present"))?;
        if predicate(&window) {
            return Ok(window);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            anyhow::bail!(
                "Sway accepted {operation} for container {id}, but authoritative tree readback \
                 did not confirm it: {window:?}"
            );
        }
        if let Err(error) = events.next_event(remaining) {
            if Instant::now() >= deadline {
                continue;
            }
            return Err(anyhow::anyhow!(
                "waiting for Sway to confirm {operation}: {error}"
            ));
        }
    }
}

pub fn set_window_frame_checked(
    id: u64,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> anyhow::Result<()> {
    if width == 0 || height == 0 {
        anyhow::bail!("Sway frame width and height must be positive");
    }
    let window = window_for_id(id)
        .ok_or_else(|| anyhow::anyhow!("Sway container {id} is no longer present"))?;
    let mut commands = Vec::new();
    remove_restore_mark_commands(&window, &mut commands);
    frame_commands(x, y, width, height, &mut commands);
    run_confirmed(id, "set frame", commands, |window| {
        (window.x, window.y, window.width, window.height) == (x, y, width, height)
            && !window.maximized
            && !window.fullscreen
    })?;
    Ok(())
}

fn clamp_restore_frame(frame: Rect, workspace: &Window) -> Rect {
    let workspace_width = i32::try_from(workspace.workspace_width).unwrap_or(i32::MAX);
    let workspace_height = i32::try_from(workspace.workspace_height).unwrap_or(i32::MAX);
    let width = frame.width.max(1).min(workspace_width.max(1));
    let height = frame.height.max(1).min(workspace_height.max(1));
    let maximum_x = workspace
        .workspace_x
        .saturating_add(workspace_width.saturating_sub(width));
    let maximum_y = workspace
        .workspace_y
        .saturating_add(workspace_height.saturating_sub(height));
    Rect {
        x: frame.x.clamp(workspace.workspace_x, maximum_x),
        y: frame.y.clamp(workspace.workspace_y, maximum_y),
        width,
        height,
    }
}

fn maximize_commands(window: &Window, restore: Rect) -> Vec<String> {
    let mut commands = Vec::new();
    remove_restore_mark_commands(window, &mut commands);
    commands.push(format!("mark --add {}", restore_mark(window.id, restore)));
    frame_commands(
        window.workspace_x,
        window.workspace_y,
        window.workspace_width,
        window.workspace_height,
        &mut commands,
    );
    commands
}

fn minimize_commands(window: &Window) -> Vec<String> {
    let mut commands = Vec::new();
    if !window.maximized {
        remove_restore_mark_commands(window, &mut commands);
    }
    // Stock Sway's `move scratchpad` hides both ordinary windows and already
    // shown scratchpad members. A toggle command could instead show a window.
    commands.push("move scratchpad".to_owned());
    commands
}

fn restore_commands(window: &Window) -> Vec<String> {
    let mut commands = Vec::new();
    if let Some(frame) = window.restore_frame {
        let frame = clamp_restore_frame(frame, window);
        remove_restore_mark_commands(window, &mut commands);
        frame_commands(
            frame.x,
            frame.y,
            u32::try_from(frame.width).unwrap_or(1),
            u32::try_from(frame.height).unwrap_or(1),
            &mut commands,
        );
    } else if window.fullscreen {
        commands.push("fullscreen disable".to_owned());
    }
    // IPC `focus` uses Sway's default seat, not the caller's numbered seat.
    // Restoring geometry/state does not require changing keyboard focus.
    commands
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowControlAction {
    Close,
    Minimize,
    Maximize,
    Restore,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowControlState {
    pub present: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    pub focused: bool,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl WindowControlState {
    fn from_window(window: Option<&Window>) -> Self {
        match window {
            Some(window) => Self {
                present: true,
                minimized: window.minimized,
                maximized: window.maximized,
                fullscreen: window.fullscreen,
                focused: window.focused,
                x: window.x,
                y: window.y,
                width: window.width,
                height: window.height,
            },
            None => Self::default(),
        }
    }
}

fn control_satisfied(action: WindowControlAction, state: WindowControlState) -> bool {
    match action {
        WindowControlAction::Close => !state.present,
        WindowControlAction::Minimize => state.present && state.minimized,
        WindowControlAction::Maximize => {
            state.present && state.maximized && !state.minimized && !state.fullscreen
        }
        WindowControlAction::Restore => {
            state.present && !state.minimized && !state.maximized && !state.fullscreen
        }
    }
}

fn read_control_state(id: u64) -> WindowControlState {
    WindowControlState::from_window(window_for_id(id).as_ref())
}

/// Perform an exact Sway window operation and wait for authoritative readback.
pub fn control_window(
    id: u64,
    action: WindowControlAction,
) -> anyhow::Result<(WindowControlState, WindowControlState)> {
    let before_window = window_for_id(id)
        .ok_or_else(|| anyhow::anyhow!("Sway container {id} is no longer present"))?;
    let before = WindowControlState::from_window(Some(&before_window));
    if control_satisfied(action, before) {
        return Ok((before, before));
    }

    match action {
        WindowControlAction::Close => {
            let mut events = EventSubscription::connect(&["window"])?;
            run_container_command(id, "kill")?;
            let deadline = Instant::now() + Duration::from_secs(2);
            while window_for_id_result(id)?.is_some() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    anyhow::bail!("Sway did not confirm that container {id} closed");
                }
                if let Err(error) = events.next_event(remaining) {
                    if Instant::now() >= deadline {
                        continue;
                    }
                    return Err(anyhow::anyhow!(
                        "waiting for Sway to confirm close: {error}"
                    ));
                }
            }
        }
        WindowControlAction::Minimize => {
            let commands = minimize_commands(&before_window);
            run_confirmed(id, "minimize", commands, |window| window.minimized)?;
        }
        WindowControlAction::Maximize => {
            let was_minimized = before_window.minimized;
            let mut current = before_window.clone();
            if before_window.minimized {
                current = run_confirmed(
                    id,
                    "scratchpad restore before maximize",
                    vec!["scratchpad show".to_owned()],
                    |window| !window.minimized,
                )?;
            }
            if current.fullscreen {
                current = run_confirmed(
                    id,
                    "fullscreen exit before maximize",
                    vec!["fullscreen disable".to_owned()],
                    |window| !window.fullscreen,
                )?;
            }
            if current.workspace_width == 0 || current.workspace_height == 0 {
                anyhow::bail!("Sway did not report a usable workspace for container {id}");
            }
            let restore = if was_minimized {
                current.restore_frame.unwrap_or(Rect {
                    x: current.x,
                    y: current.y,
                    width: i32::try_from(current.width).unwrap_or(i32::MAX),
                    height: i32::try_from(current.height).unwrap_or(i32::MAX),
                })
            } else {
                Rect {
                    x: current.x,
                    y: current.y,
                    width: i32::try_from(current.width).unwrap_or(i32::MAX),
                    height: i32::try_from(current.height).unwrap_or(i32::MAX),
                }
            };
            let commands = maximize_commands(&current, restore);
            run_confirmed(id, "maximize", commands, |window| {
                window.maximized
                    && (window.x, window.y, window.width, window.height)
                        == (
                            window.workspace_x,
                            window.workspace_y,
                            window.workspace_width,
                            window.workspace_height,
                        )
            })?;
        }
        WindowControlAction::Restore => {
            let mut current = before_window.clone();
            if current.minimized {
                current = run_confirmed(
                    id,
                    "scratchpad restore",
                    vec!["scratchpad show".to_owned()],
                    |window| !window.minimized,
                )?;
            }
            let commands = restore_commands(&current);
            run_confirmed(id, "restore", commands, |window| {
                !window.minimized && !window.maximized && !window.fullscreen
            })?;
        }
    }

    let after = if action == WindowControlAction::Close {
        WindowControlState::default()
    } else {
        read_control_state(id)
    };
    if !control_satisfied(action, after) {
        anyhow::bail!(
            "Sway accepted {action:?} for container {id}, but readback did not confirm it: \
             before={before:?}, after={after:?}"
        );
    }
    Ok((before, after))
}

struct EventSubscription {
    stream: UnixStream,
}

impl EventSubscription {
    fn connect(events: &[&str]) -> anyhow::Result<Self> {
        let socket = std::env::var_os("SWAYSOCK")
            .ok_or_else(|| anyhow::anyhow!("SWAYSOCK is unavailable"))?;
        let mut stream = UnixStream::connect(&socket).map_err(|error| {
            anyhow::anyhow!(
                "connecting to Sway IPC socket {}: {error}",
                socket.to_string_lossy()
            )
        })?;
        let payload = serde_json::to_vec(events)
            .map_err(|error| anyhow::anyhow!("encoding Sway subscription: {error}"))?;
        write_ipc_message(&mut stream, IPC_SUBSCRIBE, &payload)?;
        let (message_type, response) = read_ipc_message(&mut stream)?;
        if message_type != IPC_SUBSCRIBE {
            anyhow::bail!("unexpected Sway subscribe response type {message_type}");
        }
        let response: serde_json::Value = serde_json::from_slice(&response)
            .map_err(|error| anyhow::anyhow!("parsing Sway subscribe response: {error}"))?;
        if response.get("success").and_then(serde_json::Value::as_bool) != Some(true) {
            anyhow::bail!("Sway rejected event subscription: {response}");
        }
        Ok(Self { stream })
    }

    fn next_event(&mut self, timeout: Duration) -> anyhow::Result<()> {
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(|error| anyhow::anyhow!("setting Sway event timeout: {error}"))?;
        let (message_type, _) = read_ipc_message(&mut self.stream)?;
        if message_type & IPC_EVENT_MASK == 0 {
            anyhow::bail!("unexpected non-event Sway IPC message type {message_type}");
        }
        Ok(())
    }
}

fn write_ipc_message(
    stream: &mut UnixStream,
    message_type: u32,
    payload: &[u8],
) -> anyhow::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| anyhow::anyhow!("Sway IPC payload is too large"))?;
    stream.write_all(IPC_MAGIC)?;
    stream.write_all(&length.to_ne_bytes())?;
    stream.write_all(&message_type.to_ne_bytes())?;
    stream.write_all(payload)?;
    Ok(())
}

fn read_ipc_message(stream: &mut UnixStream) -> anyhow::Result<(u32, Vec<u8>)> {
    let mut header = [0_u8; 14];
    stream.read_exact(&mut header)?;
    if &header[..6] != IPC_MAGIC {
        anyhow::bail!("invalid Sway IPC magic");
    }
    let length = u32::from_ne_bytes(
        header[6..10]
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid Sway IPC length"))?,
    ) as usize;
    if length > MAX_IPC_PAYLOAD {
        anyhow::bail!("Sway IPC payload length {length} exceeds the safety limit");
    }
    let message_type = u32::from_ne_bytes(
        header[10..14]
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid Sway IPC message type"))?,
    );
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload)?;
    Ok((message_type, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fullscreen_tree() -> Node {
        serde_json::from_value(serde_json::json!({
            "id": 1, "type": "root", "nodes": [{"id": 2, "type": "output", "nodes": [
                {"id": 3, "type": "workspace", "name": "Desktop", "fullscreen_mode": 1,
                 "nodes": [{"id": 30, "type": "con", "fullscreen_mode": 1}]},
                {"id": 4, "type": "workspace", "name": "CUA", "fullscreen_mode": 1,
                 "floating_nodes": [{"id": 40, "type": "floating_con", "fullscreen_mode": 1,
                    "nodes": [{"id": 41, "type": "con"}]}, {"id": 42, "type": "con"}]},
                {"id": 5, "type": "workspace", "name": "CUA2", "fullscreen_mode": 1,
                 "nodes": [{"id": 50, "type": "con", "fullscreen_mode": 1}, {"id": 51, "type": "con"}]}
            ]}]
        }))
        .unwrap()
    }

    #[test]
    fn fullscreen_focus_selects_only_the_callers_obstructing_container() {
        let tree = fullscreen_tree();
        assert_eq!(fullscreen_obstruction(&tree, 42, "CUA").unwrap(), Some(40));
        assert_eq!(fullscreen_obstruction(&tree, 51, "CUA2").unwrap(), Some(50));
    }

    #[test]
    fn fullscreen_focus_preserves_the_target_and_its_fullscreen_ancestor() {
        let tree = fullscreen_tree();
        assert_eq!(fullscreen_obstruction(&tree, 40, "CUA").unwrap(), None);
        assert_eq!(fullscreen_obstruction(&tree, 41, "CUA").unwrap(), None);
        assert_eq!(fullscreen_obstruction(&tree, 50, "CUA2").unwrap(), None);
    }

    #[test]
    fn fullscreen_focus_never_selects_another_workspaces_fullscreen() {
        let mut tree = fullscreen_tree();
        tree.nodes[0].nodes[1].floating_nodes[0].fullscreen_mode = 0;
        // Desktop and CUA2 both remain fullscreen. Workspace metadata alone
        // must not be mistaken for a fullscreen application.
        assert_eq!(fullscreen_obstruction(&tree, 42, "CUA").unwrap(), None);
    }

    #[test]
    fn fullscreen_focus_rejects_stale_or_foreign_targets() {
        let tree = fullscreen_tree();
        assert!(fullscreen_obstruction(&tree, 999, "CUA").is_err());
        assert!(fullscreen_obstruction(&tree, 51, "CUA").is_err());
        assert!(fullscreen_obstruction(&tree, 42, "missing").is_err());
    }

    #[test]
    fn fullscreen_focus_does_not_clear_foreign_global_fullscreen() {
        let mut tree = fullscreen_tree();
        tree.nodes[0].nodes[0].nodes[0].fullscreen_mode = 2;
        tree.nodes[0].nodes[1].floating_nodes[0].fullscreen_mode = 0;
        assert_eq!(fullscreen_obstruction(&tree, 42, "CUA").unwrap(), None);
    }

    #[test]
    fn new_output_setup_does_not_reposition_existing_outputs_or_select_a_workspace() {
        let before = [
            OutputInfo {
                id: 9,
                name: "HEADLESS-2".into(),
                active: true,
                rect: Rect {
                    x: 6400,
                    width: 1280,
                    height: 681,
                    ..Rect::default()
                },
                ..OutputInfo::default()
            },
            OutputInfo {
                id: 3,
                name: "WL-1".into(),
                active: true,
                rect: Rect {
                    x: 7680,
                    width: 1280,
                    height: 681,
                    ..Rect::default()
                },
                physical_width: 1707,
                physical_height: 908,
                scale_milli: 1333,
                refresh_millihz: 60000,
                ..OutputInfo::default()
            },
            OutputInfo {
                id: 6,
                name: "HEADLESS-1".into(),
                active: true,
                rect: Rect {
                    x: 2560,
                    width: 1280,
                    height: 681,
                    ..Rect::default()
                },
                ..OutputInfo::default()
            },
        ];
        let created = OutputInfo {
            id: 12,
            name: "HEADLESS-3".into(),
            active: true,
            ..OutputInfo::default()
        };
        assert_eq!(
            new_output_commands(3, &before, &created, "4").unwrap(),
            [
                "output \"HEADLESS-3\" mode 1707x908@60.000Hz scale 1.333 pos 8960 0",
                "seat \"seat3\" fallback false",
                "seat \"seat3\" xcursor_theme BuzzardOS-Agent 24",
                "rename workspace \"4\" to \"CUA3\"",
            ]
        );
    }

    #[test]
    fn new_output_setup_rejects_existing_inactive_or_human_seat_targets() {
        let primary = OutputInfo {
            name: "WL-1".into(),
            active: true,
            ..OutputInfo::default()
        };
        let created = OutputInfo {
            name: "HEADLESS-1".into(),
            active: true,
            ..OutputInfo::default()
        };
        assert!(new_output_commands(1, &[], &created, "2").is_err());
        assert!(new_output_commands(1, &[primary.clone()], &primary, "2").is_err());
        assert!(new_output_commands(0, &[primary.clone()], &created, "2").is_err());
        assert!(
            new_output_commands(
                1,
                &[primary],
                &OutputInfo {
                    active: false,
                    ..created
                },
                "2"
            )
            .is_err()
        );
    }

    #[test]
    fn first_numbered_output_uses_exact_workspace_rename() {
        let before = [OutputInfo {
            name: "WL-1".into(),
            active: true,
            ..OutputInfo::default()
        }];
        let created = OutputInfo {
            name: "HEADLESS-1".into(),
            active: true,
            ..OutputInfo::default()
        };
        let commands = new_output_commands(1, &before, &created, "2").unwrap();
        assert_eq!(commands[1], "seat \"seat1\" fallback false");
        assert_eq!(
            commands[2],
            "seat \"seat1\" xcursor_theme BuzzardOS-Agent 24"
        );
        assert_eq!(commands[3], "rename workspace \"2\" to \"CUA\"");
    }

    #[test]
    fn agent_cursor_configuration_never_targets_human_or_wildcard_seats() {
        assert!(agent_cursor_command(0).is_err());
        assert_eq!(
            agent_cursor_command(42).unwrap(),
            "seat \"seat42\" xcursor_theme BuzzardOS-Agent 24"
        );
    }

    fn normal_test_window() -> Window {
        parse_tree(br#"{
          "id":1,"type":"root","nodes":[{"id":2,"type":"output","name":"HEADLESS-1",
            "nodes":[{"id":3,"type":"workspace","name":"CUA",
              "rect":{"x":1600,"y":0,"width":1280,"height":800},
              "floating_nodes":[{
                "id":10,"type":"floating_con","name":"Test","pid":10,
                "foreign_toplevel_identifier":"test","rect":{"x":1700,"y":100,"width":400,"height":300},
                "scratchpad_state":"none","visible":true
              }]
            }]
          }]
        }"#).unwrap().remove(0)
    }

    #[test]
    fn minimize_hides_shown_scratchpad_member_without_toggling() {
        let mut window = normal_test_window();
        assert_eq!(minimize_commands(&window), ["move scratchpad"]);
        window.scratchpad = true;
        assert_eq!(minimize_commands(&window), ["move scratchpad"]);
    }

    #[test]
    fn restore_geometry_does_not_request_default_seat_focus() {
        let mut window = normal_test_window();
        assert!(restore_commands(&window).is_empty());
        window.fullscreen = true;
        assert_eq!(restore_commands(&window), ["fullscreen disable"]);
        window.restore_frame = Some(Rect {
            x: 1700,
            y: 100,
            width: 400,
            height: 300,
        });
        assert_eq!(
            restore_commands(&window),
            [
                "fullscreen disable",
                "floating enable",
                "resize set width 400 px height 300 px",
                "move absolute position 1700 px 100 px",
            ]
        );
    }

    #[test]
    fn reconstructs_exact_live_sway_outer_frame_and_content() {
        let tree = br#"{
          "id":1,"type":"root","rect":{"x":0,"y":0,"width":1600,"height":1000},
          "app_id":null,"scratchpad_state":null,
          "nodes":[{"id":2,"type":"output","rect":{"x":0,"y":0,"width":1600,"height":1000},
            "nodes":[{"id":3,"type":"workspace","name":"1",
              "rect":{"x":0,"y":0,"width":1600,"height":975},
              "floating_nodes":[{
                "id":10,"type":"floating_con","name":"Foot","app_id":"foot","pid":123,
                "foreign_toplevel_identifier":"sway-live-foot",
                "rect":{"x":449,"y":232,"width":702,"height":497},
                "deco_rect":{"x":449,"y":207,"width":702,"height":25},
                "window_rect":{"x":3,"y":0,"width":696,"height":494},
                "current_border_width":3,"focused":true,"visible":true,
                "scratchpad_state":"none"
              }]
            }]
          }]
        }"#;
        let windows = parse_tree(tree).expect("parse pinned Sway tree");
        assert_eq!(windows.len(), 1);
        let window = &windows[0];
        assert_eq!(
            (window.x, window.y, window.width, window.height),
            (449, 207, 702, 522)
        );
        assert_eq!(
            (
                window.content_x,
                window.content_y,
                window.content_width,
                window.content_height
            ),
            (3, 25, 696, 494)
        );
        assert_eq!(window.border_width, 3);
    }

    #[test]
    fn decoration_is_resolved_relative_to_offset_workspace() {
        let tree = br#"{
          "id":1,"type":"root","nodes":[{"id":2,"type":"output",
            "rect":{"x":1600,"y":0,"width":1280,"height":800},
            "nodes":[{"id":3,"type":"workspace","name":"2",
              "rect":{"x":1600,"y":0,"width":1280,"height":775},
              "floating_nodes":[{
                "id":11,"type":"floating_con","name":"Editor","app_id":"editor","pid":124,
                "foreign_toplevel_identifier":"offset-editor",
                "rect":{"x":1700,"y":75,"width":600,"height":400},
                "deco_rect":{"x":100,"y":50,"width":600,"height":25},
                "window_rect":{"x":3,"y":0,"width":594,"height":397},
                "current_border_width":3,"scratchpad_state":"none"
              }]
            }]
          }]
        }"#;
        let window = parse_tree(tree).unwrap().remove(0);
        assert_eq!(
            (window.x, window.y, window.width, window.height),
            (1700, 50, 600, 425)
        );
        assert_eq!((window.content_x, window.content_y), (3, 25));
    }

    #[test]
    fn marked_workspace_frame_is_classic_maximized_on_usable_area() {
        let tree = br#"{
          "id":1,"type":"root","nodes":[{"id":2,"type":"output",
            "rect":{"x":1600,"y":0,"width":1280,"height":900},
            "nodes":[{"id":3,"type":"workspace","name":"2",
              "rect":{"x":1600,"y":0,"width":1280,"height":858},
              "floating_nodes":[{
                "id":19,"type":"floating_con","name":"Xeyes","app_id":"xeyes","pid":125,
                "foreign_toplevel_identifier":"offset-x11",
                "rect":{"x":1600,"y":25,"width":1280,"height":833},
                "deco_rect":{"x":0,"y":0,"width":1280,"height":25},
                "window_rect":{"x":3,"y":0,"width":1274,"height":830},
                "marks":["__buzzardos_restore_v1_19_1720_90_800_600"],
                "current_border_width":3,"scratchpad_state":"none"
              }]
            }]
          }]
        }"#;
        let window = parse_tree(tree).unwrap().remove(0);
        assert_eq!(
            (window.x, window.y, window.width, window.height),
            (1600, 0, 1280, 858)
        );
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
    fn unmarked_workspace_sized_window_is_not_reported_maximized() {
        let tree = br#"{
          "id":1,"type":"root","nodes":[{"id":2,"type":"output","nodes":[{
            "id":3,"type":"workspace","name":"1","rect":{"x":0,"y":0,"width":800,"height":558},
            "floating_nodes":[{
              "id":4,"type":"floating_con","name":"App","app_id":"app","pid":44,
              "foreign_toplevel_identifier":"plain",
              "rect":{"x":0,"y":25,"width":800,"height":533},
              "deco_rect":{"x":0,"y":0,"width":800,"height":25},
              "window_rect":{"x":3,"y":0,"width":794,"height":530},
              "marks":[],"scratchpad_state":"none"
            }]
          }]}]
        }"#;
        let window = parse_tree(tree).unwrap().remove(0);
        assert!(!window.maximized);
    }

    #[test]
    fn frame_commands_use_outer_size_and_root_absolute_position() {
        let mut commands = Vec::new();
        frame_commands(1600, 12, 1280, 846, &mut commands);
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
    fn restore_frame_clamps_to_current_usable_workspace() {
        let workspace = Window {
            id: 7,
            foreign_toplevel_identifier: "window".into(),
            pid: 10,
            title: "Window".into(),
            app_id: "window".into(),
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            content_x: 0,
            content_y: 0,
            content_width: 100,
            content_height: 100,
            border_width: 0,
            workspace_x: 1600,
            workspace_y: 0,
            workspace_width: 1000,
            workspace_height: 700,
            workspace: "Desktop".into(),
            output: "Buzzard-1".into(),
            focused: false,
            visible: true,
            scratchpad: false,
            minimized: false,
            maximized: false,
            fullscreen: false,
            restore_frame: None,
            restore_marks: Vec::new(),
        };
        assert_eq!(
            clamp_restore_frame(
                Rect {
                    x: 1500,
                    y: -20,
                    width: 1400,
                    height: 900,
                },
                &workspace,
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
    fn shown_scratchpad_member_is_not_minimized_but_hidden_member_is() {
        let tree = br#"{
          "id":1,"type":"root","nodes":[
            {"id":2,"type":"output","nodes":[{"id":3,"type":"workspace","name":"1",
              "rect":{"x":0,"y":0,"width":1280,"height":760},"floating_nodes":[{
                "id":10,"type":"floating_con","name":"Shown","pid":10,
                "foreign_toplevel_identifier":"shown","rect":{"x":10,"y":35,"width":400,"height":300},
                "deco_rect":{"x":10,"y":10,"width":400,"height":25},
                "window_rect":{"x":2,"y":0,"width":396,"height":298},
                "scratchpad_state":"fresh","visible":true
              }]}]},
            {"id":4,"type":"output","nodes":[{"id":5,"type":"workspace","name":"__i3_scratch",
              "rect":{"x":0,"y":0,"width":1280,"height":800},"floating_nodes":[{
                "id":11,"type":"floating_con","name":"Hidden","pid":11,
                "foreign_toplevel_identifier":"hidden","rect":{"x":20,"y":45,"width":400,"height":300},
                "deco_rect":{"x":20,"y":20,"width":400,"height":25},
                "window_rect":{"x":2,"y":0,"width":396,"height":298},
                "scratchpad_state":"fresh","visible":false
              }]}]}
          ]
        }"#;
        let windows = parse_tree(tree).unwrap();
        assert!(windows[0].scratchpad);
        assert!(!windows[0].minimized);
        assert!(windows[0].visible);
        assert!(windows[1].scratchpad);
        assert!(windows[1].minimized);
        assert!(!windows[1].visible);
    }

    #[test]
    fn command_reply_requires_compositor_success() {
        validate_command_reply(br#"[{"success":true}]"#, 7).unwrap();
        assert!(
            validate_command_reply(
                br#"[{"success":false,"error":"No matching node.","parse_error":false}]"#,
                7
            )
            .is_err()
        );
    }

    #[test]
    fn control_confirmation_requires_observed_state() {
        let normal = WindowControlState {
            present: true,
            ..WindowControlState::default()
        };
        assert!(control_satisfied(
            WindowControlAction::Minimize,
            WindowControlState {
                minimized: true,
                ..normal
            }
        ));
        assert!(control_satisfied(
            WindowControlAction::Maximize,
            WindowControlState {
                maximized: true,
                ..normal
            }
        ));
        assert!(control_satisfied(WindowControlAction::Restore, normal));
        assert!(!control_satisfied(WindowControlAction::Close, normal));
    }
}
