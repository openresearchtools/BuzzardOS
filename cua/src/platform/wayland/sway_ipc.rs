//! Exact Sway IPC metadata and window operations for Buzzard OS.
//!
//! Sway's IPC tree is the authority for global frame geometry and state.  The
//! public CUA id is bound to Sway's opaque `foreign_toplevel_identifier`; title,
//! app-id, PID, and Wayland object-id heuristics are never used to retarget it.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;

const SCRATCHPAD_WORKSPACE: &str = "__i3_scratch";
const RESTORE_MARK_PREFIX: &str = "__buzzardos_restore_v1_";
const PUBLIC_ID_START: u64 = 0xFB00_0000;
const PUBLIC_ID_END: u64 = 0xFBFF_FFFE;
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

#[derive(Clone, Copy, Debug, Default)]
struct Workspace {
    rect: Rect,
    scratchpad: bool,
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
    windows: &mut Vec<Window>,
) {
    let workspace = if node.node_type == "workspace" {
        Some(Workspace {
            rect: node.rect,
            scratchpad: node.name == SCRATCHPAD_WORKSPACE,
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
            let workspace = workspace.unwrap_or_default();
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
        collect(child, child_origin, workspace, windows);
    }
}

fn parse_tree(bytes: &[u8]) -> anyhow::Result<Vec<Window>> {
    let root: Node = serde_json::from_slice(bytes)
        .map_err(|error| anyhow::anyhow!("invalid Sway tree: {error}"))?;
    let mut windows = Vec::new();
    collect(&root, (root.rect.x, root.rect.y), None, &mut windows);
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

#[derive(Default)]
struct PublicIdRegistry {
    by_identifier: HashMap<String, u64>,
    by_id: HashMap<u64, String>,
    next: u64,
}

impl PublicIdRegistry {
    fn id_for(&mut self, identifier: &str) -> anyhow::Result<u64> {
        if identifier.is_empty() {
            anyhow::bail!("Sway did not publish foreign_toplevel_identifier for a mapped toplevel");
        }
        if let Some(id) = self.by_identifier.get(identifier).copied() {
            return Ok(id);
        }
        let next = if self.next == 0 {
            PUBLIC_ID_START
        } else {
            self.next
        };
        if next > PUBLIC_ID_END {
            anyhow::bail!("Sway public window id space is exhausted");
        }
        self.next = next.saturating_add(1);
        self.by_identifier.insert(identifier.to_owned(), next);
        self.by_id.insert(next, identifier.to_owned());
        Ok(next)
    }
}

fn public_ids() -> &'static Mutex<PublicIdRegistry> {
    static REGISTRY: OnceLock<Mutex<PublicIdRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(PublicIdRegistry::default()))
}

fn identifier_for_public_id(id: u64) -> Option<String> {
    public_ids()
        .lock()
        .ok()
        .and_then(|registry| registry.by_id.get(&id).cloned())
}

pub fn is_public_window_id(id: u64) -> bool {
    identifier_for_public_id(id).is_some()
}

pub fn list_public_windows() -> anyhow::Result<Vec<(u64, Window)>> {
    let windows = list_windows_result()?;
    let mut registry = public_ids()
        .lock()
        .map_err(|_| anyhow::anyhow!("Sway public window id registry is unavailable"))?;
    windows
        .into_iter()
        .map(|window| {
            let id = registry.id_for(&window.foreign_toplevel_identifier)?;
            Ok((id, window))
        })
        .collect()
}

/// Resolve one CUA id through Sway's opaque identifier and verify its owner.
pub fn resolve_public_window(id: u64, expected_pid: Option<u32>) -> anyhow::Result<Window> {
    let identifier = identifier_for_public_id(id)
        .ok_or_else(|| anyhow::anyhow!("unknown or stale Sway window_id {id}"))?;
    let mut matches = list_windows_result()?
        .into_iter()
        .filter(|window| window.foreign_toplevel_identifier == identifier);
    let window = matches
        .next()
        .ok_or_else(|| anyhow::anyhow!("Sway toplevel {identifier:?} is no longer mapped"))?;
    if matches.next().is_some() {
        anyhow::bail!("Sway published duplicate foreign toplevel identifier {identifier:?}");
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

pub fn focus_public_window(id: u64, expected_pid: Option<u32>) -> anyhow::Result<()> {
    let window = resolve_public_window(id, expected_pid)?;
    control_window(window.id, WindowControlAction::Focus)?;
    Ok(())
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

pub fn set_window_frame(id: u64, x: i32, y: i32, width: u32, height: u32) -> bool {
    set_window_frame_checked(id, x, y, width, height).is_ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowControlAction {
    Focus,
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
        WindowControlAction::Focus => state.present && state.focused && !state.minimized,
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
        WindowControlAction::Focus => {
            if before_window.minimized {
                let shown = run_confirmed(
                    id,
                    "scratchpad restore",
                    vec!["scratchpad show".to_owned()],
                    |window| !window.minimized,
                )?;
                if shown.restore_frame.is_some() {
                    let commands = maximize_commands(
                        &shown,
                        shown.restore_frame.expect("checked restore frame"),
                    );
                    run_confirmed(id, "restored maximized frame", commands, |window| {
                        window.maximized
                    })?;
                }
            }
            run_confirmed(id, "focus", vec!["focus".to_owned()], |window| {
                window.focused && !window.minimized
            })?;
        }
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
            let mut commands = Vec::new();
            if !before_window.maximized {
                remove_restore_mark_commands(&before_window, &mut commands);
            }
            // `move scratchpad` rejects a currently shown scratchpad member;
            // `scratchpad show` toggles that exact shown member back to hidden.
            commands.push(if before_window.scratchpad {
                "scratchpad show".to_owned()
            } else {
                "move scratchpad".to_owned()
            });
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
            let mut commands = Vec::new();
            if let Some(frame) = current.restore_frame {
                let frame = clamp_restore_frame(frame, &current);
                remove_restore_mark_commands(&current, &mut commands);
                frame_commands(
                    frame.x,
                    frame.y,
                    u32::try_from(frame.width).unwrap_or(1),
                    u32::try_from(frame.height).unwrap_or(1),
                    &mut commands,
                );
            } else if current.fullscreen {
                commands.push("fullscreen disable".to_owned());
            }
            commands.push("focus".to_owned());
            run_confirmed(id, "restore", commands, |window| {
                !window.minimized && !window.maximized && !window.fullscreen && window.focused
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

/// Briefly focus one compositor-attested container, run `body`, then restore
/// the previously focused container.
pub fn with_focused_container<T>(
    id: u64,
    body: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let prior = list_windows_result()?
        .into_iter()
        .find(|window| window.focused)
        .map(|window| window.id);
    run_confirmed(id, "temporary focus", vec!["focus".to_owned()], |window| {
        window.focused
    })?;
    let result = body();
    let restore = prior
        .filter(|prior| *prior != id)
        .map(|prior| {
            run_confirmed(
                prior,
                "prior focus restore",
                vec!["focus".to_owned()],
                |window| window.focused,
            )?;
            Ok(())
        })
        .transpose();
    match (result, restore) {
        (Ok(value), Ok(_)) => Ok(value),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(restore)) => Err(error.context(format!(
            "the prior Sway focus also could not be restored: {restore}"
        ))),
    }
}

pub fn window_origin_for_pid(pid: u32) -> Option<(i32, i32)> {
    window_for_pid(pid).map(|window| (window.x, window.y))
}

pub fn window_content_offset_for_pid(pid: u32) -> Option<(i32, i32)> {
    window_for_pid(pid).map(|window| (window.content_x, window.content_y))
}

pub fn window_origin_for_title(title: &str) -> Option<(i32, i32)> {
    let window = window_for_title(title)?;
    Some((window.x, window.y))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn opaque_identifiers_produce_distinct_stable_ids_for_duplicate_titles() {
        let mut registry = PublicIdRegistry::default();
        let first = registry.id_for("identifier-a").unwrap();
        assert_eq!(registry.id_for("identifier-a").unwrap(), first);
        let second = registry.id_for("identifier-b").unwrap();
        assert_ne!(first, second);
        assert_eq!(
            registry.by_id.get(&first).map(String::as_str),
            Some("identifier-a")
        );
    }

    #[test]
    fn command_reply_requires_compositor_success() {
        validate_command_reply(br#"[{"success":true}]"#, 7).unwrap();
        assert!(validate_command_reply(
            br#"[{"success":false,"error":"No matching node.","parse_error":false}]"#,
            7
        )
        .is_err());
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
