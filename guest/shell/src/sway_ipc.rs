// SPDX-License-Identifier: AGPL-3.0-or-later

//! Exact task-window identity and controls for the pinned Sway guest session.
//!
//! `ext_foreign_toplevel_list_v1` gives the shell Sway's opaque, per-mapping
//! identifier. The same identifier is present in Sway's IPC tree, so task
//! controls never have to guess by title, app-id, PID, or focus.

use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use wildbuzzard_desktop_core::{SolidColor, ThemePalette};

const SCRATCHPAD_WORKSPACE: &str = "__i3_scratch";
const RESTORE_MARK_PREFIX: &str = "__wildbuzzard_restore_v1_";
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
    pub rect: Rect,
    pub workspace_rect: Option<Rect>,
    pub focused: bool,
    pub scratchpad: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    pub decoration_height: i32,
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
        .name("wildbuzzard-sway-events".to_owned())
        .spawn(move || {
            loop {
                match subscription.next_event(Duration::from_secs(24 * 60 * 60)) {
                    Ok(()) if sender.send(()).is_err() => break,
                    Ok(()) => {}
                    Err(error) => {
                        eprintln!("wildbuzzard-shell: Sway event subscription ended: {error:#}");
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
    use wildbuzzard_desktop_core::ThemeMode;

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
                "marks":["__wildbuzzard_restore_v1_7_200_100_900_700"],
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
                "marks":["__wildbuzzard_restore_v1_19_1720_90_800_600"],
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
