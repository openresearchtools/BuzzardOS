// SPDX-License-Identifier: AGPL-3.0-or-later

mod icons;
mod model;
mod sway_ipc;

use accesskit::{
    Action, ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, Node as A11yNode,
    NodeId, Rect as A11yRect, Role, Tree, TreeId, TreeUpdate,
};
use accesskit_unix::Adapter as A11yAdapter;
use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
use icons::{AppIcon, load_application_icons};
use model::{
    APPLICATIONS_MENU_FOOTER_HEIGHT, APPLICATIONS_MENU_HEADER_HEIGHT,
    APPLICATIONS_MENU_SECTION_HEIGHT, Application, GuestWindow, HitTarget, MENU_ROW_HEIGHT,
    PANEL_HEIGHT, Rect, ShellAction, applications_menu_close_target, desktop_targets, menu_targets,
    panel_targets, scan_applications, window_menu_targets,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_foreign_toplevel_list, delegate_keyboard, delegate_layer,
    delegate_output, delegate_pointer, delegate_registry, delegate_seat, delegate_shm,
    foreign_toplevel_list::{ForeignToplevelList, ForeignToplevelListHandler},
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{BTN_LEFT, BTN_RIGHT, PointerEvent, PointerEventKind, PointerHandler},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::{fs::PermissionsExt, net::UnixDatagram};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, Sender},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{Event as FractionalScaleEvent, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

const SHELL_NAME: &str = "Wild Buzzard Desktop";
const REPAINT_REQUEST: &str = "wildbuzzard-shell-repaint";
const REPAINT_ACKNOWLEDGEMENT: &str = "wildbuzzard-shell-repaint-ack";
const SHELL_READY: &str = "shell-ready";
const SHELL_CONTROL_SOCKET: &str = "wildbuzzard-shell-control.sock";
const REQUEST_FOCUSED_WINDOW_MENU: &str = "--request-focused-window-menu";
const HOST_POINTER_CLICK_STATE: &str = "/run/wildbuzzard-display-state/pointer-click.json";
const CUA_POINTER_CLICK_STATE: &str = "wildbuzzard-cua-pointer-click.json";
const POINTER_CLICK_MAX_AGE: Duration = Duration::from_secs(3);
const OUTPUT_SETTLE_REPAINT_FRAMES: u8 = 90;
const OUTPUT_SETTLE_DEBOUNCE: Duration = Duration::from_millis(80);
const WINDOW_MENU_WIDTH: u32 = 260;
const WINDOW_MENU_HEIGHT: u32 = 44 + 5 * MENU_ROW_HEIGHT as u32;

mod theme {
    pub const CANVAS: [u8; 4] = [24, 24, 24, 255];
    pub const DESKTOP: [u8; 4] = [32, 34, 37, 255];
    pub const MENU: [u8; 4] = [34, 34, 34, 255];
    pub const SURFACE: [u8; 4] = [40, 40, 40, 255];
    pub const RAISED: [u8; 4] = [48, 48, 48, 255];
    pub const HOVER: [u8; 4] = [63, 63, 63, 255];
    pub const BORDER: [u8; 4] = [84, 84, 84, 255];
    pub const TEXT: [u8; 4] = [230, 230, 230, 255];
    pub const SELECTED_TEXT: [u8; 4] = [255, 255, 255, 255];
    pub const TEXT_SECONDARY: [u8; 4] = [184, 184, 184, 255];
    pub const TEXT_MUTED: [u8; 4] = [152, 152, 152, 255];
    pub const SELECTION: [u8; 4] = [255, 113, 57, 255];
    pub const FOCUS: [u8; 4] = [255, 113, 57, 255];
    pub const FOLDER: [u8; 4] = [255, 113, 57, 255];
    pub const FOLDER_TAB: [u8; 4] = [255, 155, 115, 255];
    pub const DESTRUCTIVE: [u8; 4] = [92, 40, 40, 255];
    pub const DESTRUCTIVE_ICON: [u8; 4] = [240, 122, 122, 255];
}

#[derive(Debug, Clone, Copy)]
enum ShellSurface {
    Desktop,
    Panel,
    Menu,
}

fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(REQUEST_FOCUSED_WINDOW_MENU)) {
        if let Err(error) = request_focused_window_menu() {
            eprintln!("wildbuzzard-shell: titlebar menu request failed: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = run() {
        eprintln!("wildbuzzard-shell: {error:#}");
        std::process::exit(1);
    }
}

fn shell_control_socket_path() -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is unavailable")?,
    )
    .join(SHELL_CONTROL_SOCKET))
}

fn request_focused_window_menu() -> Result<()> {
    let focused = sway_ipc::list_windows()?
        .into_iter()
        .find(|window| window.focused && !window.minimized)
        .context("Sway has no focused visible toplevel")?;
    let pointer = recent_titlebar_pointer(&focused);
    let payload = serde_json::json!({
        "schema": 1,
        "identifier": focused.identifier,
        "x": pointer.map(|point| point.0),
        "y": pointer.map(|point| point.1),
    });
    let socket = UnixDatagram::unbound().context("creating shell-control datagram")?;
    socket
        .send_to(
            &serde_json::to_vec(&payload).context("encoding focused titlebar menu request")?,
            shell_control_socket_path()?,
        )
        .context("sending focused titlebar menu request")?;
    Ok(())
}

fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn pointer_click(path: &std::path::Path) -> Option<(u64, f64, f64)> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    if value.get("button").and_then(serde_json::Value::as_u64) != Some(3) {
        return None;
    }
    let timestamp = value
        .get("timestamp_ms")
        .and_then(serde_json::Value::as_u64)?;
    let x = value.get("x").and_then(serde_json::Value::as_f64)?;
    let y = value.get("y").and_then(serde_json::Value::as_f64)?;
    (x.is_finite() && y.is_finite()).then_some((timestamp, x, y))
}

fn recent_titlebar_pointer(window: &sway_ipc::WindowState) -> Option<(f64, f64)> {
    let runtime_pointer = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|runtime| runtime.join(CUA_POINTER_CLICK_STATE));
    let mut latest = pointer_click(std::path::Path::new(HOST_POINTER_CLICK_STATE));
    if let Some(candidate) = runtime_pointer.as_deref().and_then(pointer_click)
        && latest.is_none_or(|current| candidate.0 > current.0)
    {
        latest = Some(candidate);
    }
    let (timestamp, x, y) = latest?;
    let age = unix_time_millis().saturating_sub(timestamp);
    if age > POINTER_CLICK_MAX_AGE.as_millis() as u64 {
        return None;
    }
    let titlebar_bottom = window
        .rect
        .y
        .saturating_add(window.decoration_height.max(1));
    let window_right = window.rect.x.saturating_add(window.rect.width.max(1));
    (x >= f64::from(window.rect.x)
        && x < f64::from(window_right)
        && y >= f64::from(window.rect.y)
        && y < f64::from(titlebar_bottom))
    .then_some((x, y))
}

fn parse_window_menu_request(bytes: &[u8]) -> Option<(String, Option<(f64, f64)>)> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes)
        && let Some(identifier) = value
            .get("identifier")
            .and_then(serde_json::Value::as_str)
            .filter(|identifier| !identifier.is_empty())
    {
        let pointer = value
            .get("x")
            .and_then(serde_json::Value::as_f64)
            .zip(value.get("y").and_then(serde_json::Value::as_f64))
            .filter(|(x, y)| x.is_finite() && y.is_finite());
        return Some((identifier.to_owned(), pointer));
    }
    std::str::from_utf8(bytes)
        .ok()
        .filter(|identifier| !identifier.is_empty())
        .map(|identifier| (identifier.to_owned(), None))
}

fn titlebar_menu_origin(
    frame: sway_ipc::Rect,
    decoration_height: i32,
    desktop_size: (u32, u32),
    pointer: Option<(f64, f64)>,
) -> (i32, i32) {
    let desktop_width = i32::try_from(desktop_size.0).unwrap_or(i32::MAX);
    let desktop_height = i32::try_from(desktop_size.1).unwrap_or(i32::MAX);
    let maximum_left = desktop_width.saturating_sub(WINDOW_MENU_WIDTH as i32);
    let maximum_top = desktop_height
        .saturating_sub(PANEL_HEIGHT)
        .saturating_sub(WINDOW_MENU_HEIGHT as i32);
    let requested_left = pointer
        .filter(|(x, y)| {
            let titlebar_bottom = frame.y.saturating_add(decoration_height.max(1));
            *x >= f64::from(frame.x)
                && *x < f64::from(frame.x.saturating_add(frame.width.max(1)))
                && *y >= f64::from(frame.y)
                && *y < f64::from(titlebar_bottom)
        })
        .map_or(frame.x, |(x, _)| x.floor() as i32);
    (
        requested_left.clamp(0, maximum_left.max(0)),
        frame
            .y
            .saturating_add(decoration_height.max(1))
            .clamp(0, maximum_top.max(0)),
    )
}

fn dispatch_with_timeout(
    event_queue: &mut EventQueue<Shell>,
    shell: &mut Shell,
    timeout: Duration,
) -> Result<()> {
    event_queue
        .dispatch_pending(shell)
        .context("dispatching pending guest Wayland events")?;
    event_queue
        .flush()
        .context("flushing guest Wayland requests")?;

    let Some(guard) = event_queue.prepare_read() else {
        event_queue
            .dispatch_pending(shell)
            .context("dispatching guest Wayland events before polling")?;
        return Ok(());
    };
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let mut descriptor = libc::pollfd {
        fd: guard.connection_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: `descriptor` is one initialized pollfd, and the Wayland
        // read guard keeps its borrowed connection fd valid for this call.
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result > 0 {
            guard
                .read()
                .context("reading guest Wayland events after poll")?;
            break;
        }
        if result == 0 {
            // Dropping a prepared read guard cancels it. This timeout is what
            // lets the shell process repaint files, AT-SPI actions, and newly
            // installed .desktop entries while the compositor is otherwise
            // completely idle.
            drop(guard);
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error).context("polling guest Wayland connection");
    }

    event_queue
        .dispatch_pending(shell)
        .context("dispatching guest Wayland events after polling")?;
    Ok(())
}

fn run() -> Result<()> {
    let connection = Connection::connect_to_env().context("connecting to guest compositor")?;
    let (globals, mut event_queue) =
        registry_queue_init(&connection).context("reading guest Wayland globals")?;
    let qh = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).context("guest has no wl_compositor")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("guest has no wlr layer-shell")?;
    let shm = Shm::bind(&globals, &qh).context("guest has no wl_shm")?;
    let foreign_toplevel_list = ForeignToplevelList::new(&globals, &qh);
    let fractional_manager: WpFractionalScaleManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .context("guest compositor has no fractional-scale protocol")?;
    let viewporter: WpViewporter = globals
        .bind(&qh, 1..=1, ())
        .context("guest compositor has no viewporter protocol")?;

    let desktop_surface = compositor.create_surface(&qh);
    let desktop = layer_shell.create_layer_surface(
        &qh,
        desktop_surface,
        Layer::Background,
        Some("wildbuzzard-desktop"),
        None,
    );
    desktop.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    desktop.set_keyboard_interactivity(KeyboardInteractivity::None);
    desktop.set_exclusive_zone(-1);
    desktop.set_size(0, 0);
    let desktop_fractional =
        fractional_manager.get_fractional_scale(desktop.wl_surface(), &qh, ShellSurface::Desktop);
    let desktop_viewport = viewporter.get_viewport(desktop.wl_surface(), &qh, ());
    desktop.commit();

    let panel_surface = compositor.create_surface(&qh);
    let panel = layer_shell.create_layer_surface(
        &qh,
        panel_surface,
        Layer::Top,
        Some("wildbuzzard-panel"),
        None,
    );
    panel.set_anchor(Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    panel.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    panel.set_exclusive_zone(PANEL_HEIGHT);
    panel.set_size(0, PANEL_HEIGHT as u32);
    let panel_fractional =
        fractional_manager.get_fractional_scale(panel.wl_surface(), &qh, ShellSurface::Panel);
    let panel_viewport = viewporter.get_viewport(panel.wl_surface(), &qh, ());
    panel.commit();

    let menu_surface = compositor.create_surface(&qh);
    let menu = layer_shell.create_layer_surface(
        &qh,
        menu_surface,
        Layer::Overlay,
        Some("wildbuzzard-applications-menu"),
        None,
    );
    menu.set_anchor(Anchor::BOTTOM | Anchor::LEFT);
    menu.set_margin(0, 0, PANEL_HEIGHT, 0);
    menu.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    menu.set_exclusive_zone(-1);
    menu.set_size(1, 1);
    let menu_fractional =
        fractional_manager.get_fractional_scale(menu.wl_surface(), &qh, ShellSurface::Menu);
    let menu_viewport = viewporter.get_viewport(menu.wl_surface(), &qh, ());
    let empty_input = Region::new(&compositor).context("creating hidden menu input region")?;
    menu.set_input_region(Some(empty_input.wl_region()));
    menu.commit();

    let control_socket_path = shell_control_socket_path()?;
    if let Err(error) = fs::remove_file(&control_socket_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error).context("removing stale shell-control socket");
    }
    let control_socket =
        UnixDatagram::bind(&control_socket_path).context("binding shell-control socket")?;
    control_socket
        .set_nonblocking(true)
        .context("making shell-control socket nonblocking")?;
    fs::set_permissions(&control_socket_path, fs::Permissions::from_mode(0o600))
        .context("restricting shell-control socket")?;

    let applications = scan_applications().unwrap_or_default();
    let application_icons = load_application_icons(&applications);
    let pool = SlotPool::new(3840 * 2160 * 4, &shm).context("creating shell render pool")?;
    let repaint_request = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|runtime| runtime.join(REPAINT_REQUEST));
    let mut shell = Shell {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        compositor,
        foreign_toplevel_list,
        _fractional_manager: fractional_manager,
        _viewporter: viewporter,
        _fractional_scales: [desktop_fractional, panel_fractional, menu_fractional],
        viewports: [desktop_viewport, panel_viewport, menu_viewport],
        desktop,
        panel,
        menu,
        pool,
        font: load_font(),
        desktop_size: (1280, 800),
        panel_size: (1280, PANEL_HEIGHT as u32),
        menu_size: (1, 1),
        menu_origin: (0, 0),
        desktop_configured: false,
        panel_configured: false,
        menu_configured: false,
        menu_open: false,
        menu_kind: MenuKind::Applications,
        menu_scroll: 0,
        scale_120: 120,
        task_page: 0,
        hovered: None,
        exit: false,
        dirty: true,
        pointer: None,
        keyboard: None,
        seat: None,
        applications,
        application_icons,
        exact_toplevels: BTreeMap::new(),
        sway_window_changes: sway_ipc::subscribe_window_changes()
            .context("subscribing to authoritative Sway window events")?,
        last_application_scan: Instant::now(),
        repaint_request,
        // An output-sync request may predate the shell process by a few
        // milliseconds. Treat the first observed generation as pending.
        repaint_generation: None,
        repaint_frames: 0,
        full_repaint_after: None,
        accessibility: None,
        control_socket,
        control_socket_path,
    };
    shell.set_desktop_input_region()?;
    shell.accessibility = Some(Accessibility::new(shell.accessibility_tree()));
    let shell_ready = std::env::var_os("WILDBUZZARD_STATUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/wildbuzzard-host"))
        .join(SHELL_READY);
    let mut ready_published = false;

    while !shell.exit {
        dispatch_with_timeout(&mut event_queue, &mut shell, Duration::from_millis(16))?;
        shell.poll();
        if shell.dirty {
            shell.draw()?;
            shell.dirty = false;
            shell.repaint_frames = shell.repaint_frames.saturating_sub(1);
            if !ready_published && shell.desktop_configured && shell.panel_configured {
                fs::write(&shell_ready, b"ready\n").with_context(|| {
                    format!("publishing shell readiness at {}", shell_ready.display())
                })?;
                ready_published = true;
                eprintln!(
                    "wildbuzzard-shell: ready at {}x{} logical pixels",
                    shell.desktop_size.0, shell.desktop_size.1
                );
            }
        } else if shell.repaint_frames > 0 && shell.panel_configured {
            // Once the debounced full redraw establishes the new geometry,
            // cheap panel commits keep frames flowing until the host accepts
            // one. Repeating the full desktop render would be unnecessarily
            // expensive at very large monitor sizes.
            shell.draw_panel()?;
            shell.repaint_frames -= 1;
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ExactToplevel {
    identifier: String,
    window: GuestWindow,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MenuKind {
    Applications,
    Window(u32),
}

struct Shell {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    compositor: CompositorState,
    foreign_toplevel_list: ForeignToplevelList,
    _fractional_manager: WpFractionalScaleManagerV1,
    _viewporter: WpViewporter,
    _fractional_scales: [WpFractionalScaleV1; 3],
    viewports: [WpViewport; 3],
    desktop: LayerSurface,
    panel: LayerSurface,
    menu: LayerSurface,
    pool: SlotPool,
    font: Option<Font>,
    desktop_size: (u32, u32),
    panel_size: (u32, u32),
    menu_size: (u32, u32),
    menu_origin: (i32, i32),
    desktop_configured: bool,
    panel_configured: bool,
    menu_configured: bool,
    menu_open: bool,
    menu_kind: MenuKind,
    menu_scroll: usize,
    scale_120: u32,
    task_page: usize,
    hovered: Option<ShellAction>,
    exit: bool,
    dirty: bool,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    seat: Option<wl_seat::WlSeat>,
    applications: Vec<Application>,
    application_icons: BTreeMap<String, AppIcon>,
    exact_toplevels: BTreeMap<u32, ExactToplevel>,
    sway_window_changes: Receiver<()>,
    last_application_scan: Instant,
    repaint_request: Option<PathBuf>,
    repaint_generation: Option<String>,
    repaint_frames: u8,
    full_repaint_after: Option<Instant>,
    accessibility: Option<Accessibility>,
    control_socket: UnixDatagram,
    control_socket_path: PathBuf,
}

impl Drop for Shell {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.control_socket_path);
    }
}

#[derive(Debug, Clone)]
enum AccessibleTarget {
    Activate {
        action: ShellAction,
        menu_index: Option<usize>,
    },
    ScrollMenu,
}

struct Accessibility {
    adapter: A11yAdapter,
    snapshot: Arc<Mutex<TreeUpdate>>,
    requests: Receiver<ActionRequest>,
    targets: BTreeMap<NodeId, AccessibleTarget>,
}

#[derive(Clone)]
struct TreeProvider(Arc<Mutex<TreeUpdate>>);

impl ActivationHandler for TreeProvider {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        self.0.lock().ok().map(|tree| tree.clone())
    }
}

struct ActionQueue(Sender<ActionRequest>);

impl ActionHandler for ActionQueue {
    fn do_action(&mut self, request: ActionRequest) {
        let _ = self.0.send(request);
    }
}

struct KeepAccessibilityActive;

impl DeactivationHandler for KeepAccessibilityActive {
    fn deactivate_accessibility(&mut self) {}
}

impl Accessibility {
    fn new((tree, targets): (TreeUpdate, BTreeMap<NodeId, AccessibleTarget>)) -> Self {
        let snapshot = Arc::new(Mutex::new(tree));
        let (sender, requests) = mpsc::channel();
        let adapter = A11yAdapter::new(
            TreeProvider(Arc::clone(&snapshot)),
            ActionQueue(sender),
            KeepAccessibilityActive,
        );
        Self {
            adapter,
            snapshot,
            requests,
            targets,
        }
    }
}

impl Shell {
    fn update_exact_toplevel(&mut self, handle: &ExtForeignToplevelHandleV1) {
        let Some(info) = self.foreign_toplevel_list.info(handle) else {
            return;
        };
        let id = handle.id().protocol_id();
        let existing = self
            .exact_toplevels
            .get(&id)
            .map(|entry| entry.window.clone());
        self.exact_toplevels.insert(
            id,
            ExactToplevel {
                identifier: info.identifier,
                window: GuestWindow {
                    id,
                    title: info.title,
                    app_id: info.app_id,
                    focused: existing.as_ref().is_some_and(|window| window.focused),
                    minimized: existing.as_ref().is_some_and(|window| window.minimized),
                    maximized: existing.as_ref().is_some_and(|window| window.maximized),
                },
            },
        );
        self.refresh_window_states();
        self.dirty = true;
    }

    fn poll_control_socket(&mut self) {
        let mut requests = Vec::new();
        loop {
            let mut buffer = [0_u8; 4096];
            match self.control_socket.recv(&mut buffer) {
                Ok(length) => {
                    if let Some(request) = parse_window_menu_request(&buffer[..length]) {
                        requests.push(request);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    eprintln!("wildbuzzard-shell: reading shell-control socket failed: {error}");
                    break;
                }
            }
        }
        for (identifier, pointer) in requests {
            self.show_titlebar_window_menu(&identifier, pointer);
        }
    }

    fn poll(&mut self) {
        self.poll_control_socket();
        if let Some(generation) = self
            .repaint_request
            .as_ref()
            .and_then(|path| fs::read_to_string(path).ok())
            && self.repaint_generation.as_ref() != Some(&generation)
        {
            if let Some(request) = self.repaint_request.as_ref() {
                let acknowledgement = request.with_file_name(REPAINT_ACKNOWLEDGEMENT);
                let _ = fs::write(acknowledgement, generation.as_bytes());
            }
            self.repaint_generation = Some(generation);
            // Continue briefly after the output transaction. Host
            // compositors are allowed to discard frames while a native
            // toplevel is being maximized, restored, or interactively
            // resized, so one early commit is not sufficient.
            self.repaint_frames = OUTPUT_SETTLE_REPAINT_FRAMES;
            // Some nested wlroots compositors do not treat damage to the panel alone as an output
            // geometry update. Redraw the desktop once after resize events
            // settle; during an interactive drag, later generations debounce
            // this expensive full-surface render.
            self.full_repaint_after = Some(Instant::now() + OUTPUT_SETTLE_DEBOUNCE);
        }
        if self
            .full_repaint_after
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.full_repaint_after = None;
            self.dirty = true;
        }
        if self.last_application_scan.elapsed() >= Duration::from_secs(2) {
            self.last_application_scan = Instant::now();
            if let Ok(applications) = scan_applications()
                && applications != self.applications
            {
                self.application_icons = load_application_icons(&applications);
                self.applications = applications;
                self.clamp_menu_scroll();
                if self.menu_open && self.menu_kind == MenuKind::Applications {
                    self.apply_applications_menu_geometry();
                }
                self.dirty = true;
            }
        }
        if self.sway_window_changes.try_recv().is_ok() {
            while self.sway_window_changes.try_recv().is_ok() {}
            self.refresh_window_states();
        }
        let requests: Vec<_> = self
            .accessibility
            .as_ref()
            .map(|accessibility| accessibility.requests.try_iter().collect())
            .unwrap_or_default();
        for request in requests {
            let Some(target) = self
                .accessibility
                .as_ref()
                .and_then(|accessibility| accessibility.targets.get(&request.target_node))
                .cloned()
            else {
                continue;
            };
            match (request.action, target) {
                (
                    Action::Click,
                    AccessibleTarget::Activate {
                        action,
                        menu_index: _,
                    },
                ) => self.activate(action),
                (Action::ScrollDown, AccessibleTarget::ScrollMenu) => self.scroll_menu(1.0),
                (Action::ScrollUp, AccessibleTarget::ScrollMenu) => self.scroll_menu(-1.0),
                (
                    Action::ScrollIntoView,
                    AccessibleTarget::Activate {
                        menu_index: Some(index),
                        ..
                    },
                ) => self.scroll_menu_item_into_view(index),
                _ => {}
            }
        }
    }

    fn windows(&self) -> Vec<GuestWindow> {
        let mut windows: Vec<_> = self
            .exact_toplevels
            .values()
            .map(|toplevel| toplevel.window.clone())
            .collect();
        // Focus changes must only update the active-button styling. Reordering
        // the focused window to the front made task buttons jump underneath
        // the pointer as soon as a view received focus.
        windows.sort_by_key(|window| window.id);
        windows
    }

    fn refresh_window_states(&mut self) {
        let Ok(states) = sway_ipc::list_windows() else {
            return;
        };
        let states = states
            .into_iter()
            .map(|state| (state.identifier.clone(), state))
            .collect::<BTreeMap<_, _>>();
        let mut changed = false;
        for toplevel in self.exact_toplevels.values_mut() {
            let Some(state) = states.get(&toplevel.identifier) else {
                continue;
            };
            let before = toplevel.window.clone();
            toplevel.window.focused = state.focused;
            toplevel.window.minimized = state.minimized;
            toplevel.window.maximized = state.maximized;
            changed |= before != toplevel.window;
        }
        if changed {
            self.dirty = true;
        }
    }

    fn set_desktop_input_region(&self) -> Result<()> {
        let region = Region::new(&self.compositor).context("creating desktop icon input region")?;
        for target in desktop_targets() {
            region.add(
                target.rect.x,
                target.rect.y,
                target.rect.width,
                target.rect.height,
            );
        }
        self.desktop.set_input_region(Some(region.wl_region()));
        self.desktop.commit();
        Ok(())
    }

    fn set_menu_input_region(&self) -> Result<()> {
        let region = Region::new(&self.compositor).context("creating menu input region")?;
        if self.menu_open {
            region.add(
                0,
                0,
                i32::try_from(self.menu_size.0).unwrap_or(i32::MAX),
                i32::try_from(self.menu_size.1).unwrap_or(i32::MAX),
            );
        }
        self.menu.set_input_region(Some(region.wl_region()));
        self.menu.commit();
        Ok(())
    }

    fn toggle_menu(&mut self) {
        if self.menu_open && self.menu_kind == MenuKind::Applications {
            self.hide_menu();
            return;
        }
        self.menu_open = true;
        self.menu_kind = MenuKind::Applications;
        self.menu_scroll = 0;
        // Update hit-testing before the configure round-trip. Reusing the
        // former 1x1 hidden extent makes a freshly opened menu visible but
        // temporarily unclickable, so a quick human/CUA click reaches the
        // application below it.
        self.apply_applications_menu_geometry();
        self.dirty = true;
    }

    fn apply_applications_menu_geometry(&mut self) {
        let requested_size = self.preferred_menu_size();
        self.menu_size = requested_size;
        self.menu_origin = (
            0,
            i32::try_from(self.desktop_size.1)
                .unwrap_or(i32::MAX)
                .saturating_sub(PANEL_HEIGHT)
                .saturating_sub(i32::try_from(requested_size.1).unwrap_or(i32::MAX)),
        );
        self.menu.set_anchor(Anchor::BOTTOM | Anchor::LEFT);
        self.menu.set_margin(0, 0, PANEL_HEIGHT, 0);
        self.menu.set_size(requested_size.0, requested_size.1);
        let _ = self.set_menu_input_region();
        self.menu.commit();
    }

    fn hide_menu(&mut self) {
        if self.menu_open {
            self.menu_open = false;
            self.menu_size = (1, 1);
            self.menu.set_size(1, 1);
            let _ = self.set_menu_input_region();
            self.menu.commit();
            self.dirty = true;
        }
    }

    fn open_window_menu(&mut self, id: u32, origin: (i32, i32), top_anchored: bool) {
        // Pointer-driven floating resize does not emit a stock Sway window
        // event. Refresh synchronously before choosing the context-menu label
        // so a window resized away from our classic maximized frame offers
        // Maximize, never a stale Restore action.
        self.refresh_window_states();
        let Some((title, identifier)) = self
            .exact_toplevels
            .get(&id)
            .map(|window| (window.window.title.clone(), window.identifier.clone()))
        else {
            return;
        };
        self.menu_open = true;
        self.menu_kind = MenuKind::Window(id);
        self.menu_scroll = 0;
        self.menu_size = (WINDOW_MENU_WIDTH, WINDOW_MENU_HEIGHT);
        self.menu_origin = origin;
        if top_anchored {
            self.menu.set_anchor(Anchor::TOP | Anchor::LEFT);
            self.menu.set_margin(origin.1, 0, 0, origin.0);
        } else {
            self.menu.set_anchor(Anchor::BOTTOM | Anchor::LEFT);
            self.menu.set_margin(0, 0, PANEL_HEIGHT, origin.0);
        }
        self.menu.set_size(WINDOW_MENU_WIDTH, WINDOW_MENU_HEIGHT);
        let _ = self.set_menu_input_region();
        self.menu.commit();
        self.dirty = true;
        eprintln!(
            "wildbuzzard-shell: opened controls for {} ({})",
            title, identifier
        );
    }

    fn show_taskbar_window_menu(&mut self, id: u32, pointer_x: f64) {
        let maximum_left = self
            .panel_size
            .0
            .saturating_sub(WINDOW_MENU_WIDTH)
            .try_into()
            .unwrap_or(i32::MAX);
        let left = (pointer_x.floor() as i32).clamp(0, maximum_left);
        let top = i32::try_from(self.desktop_size.1)
            .unwrap_or(i32::MAX)
            .saturating_sub(PANEL_HEIGHT)
            .saturating_sub(WINDOW_MENU_HEIGHT as i32);
        self.open_window_menu(id, (left, top), false);
    }

    fn show_titlebar_window_menu(&mut self, identifier: &str, pointer: Option<(f64, f64)>) {
        self.refresh_window_states();
        let Some(id) = self
            .exact_toplevels
            .iter()
            .find_map(|(id, toplevel)| (toplevel.identifier == identifier).then_some(*id))
        else {
            return;
        };
        let Ok(state) = sway_ipc::window(identifier) else {
            return;
        };
        let origin = titlebar_menu_origin(
            state.rect,
            state.decoration_height,
            self.desktop_size,
            pointer,
        );
        self.open_window_menu(id, origin, true);
    }

    fn preferred_menu_size(&self) -> (u32, u32) {
        (
            applications_menu_width(self.font.as_ref(), &self.applications, self.desktop_size.0),
            applications_menu_height(self.applications.len(), self.desktop_size.1),
        )
    }

    fn visible_menu_rows(&self) -> usize {
        let used = APPLICATIONS_MENU_HEADER_HEIGHT
            + APPLICATIONS_MENU_SECTION_HEIGHT
            + APPLICATIONS_MENU_FOOTER_HEIGHT;
        usize::try_from(
            (i32::try_from(self.menu_size.1)
                .unwrap_or(i32::MAX)
                .saturating_sub(used)
                / MENU_ROW_HEIGHT)
                .max(0),
        )
        .unwrap_or_default()
    }

    fn clamp_menu_scroll(&mut self) {
        self.menu_scroll = self.menu_scroll.min(
            self.applications
                .len()
                .saturating_sub(self.visible_menu_rows()),
        );
    }

    fn activate(&mut self, action: ShellAction) {
        match action {
            ShellAction::ToggleApplications => self.toggle_menu(),
            ShellAction::OpenFiles => {
                spawn("thunar", ["/home/wildbuzzard"]);
                self.hide_menu();
            }
            ShellAction::OpenShared => {
                spawn("thunar", ["/shared"]);
                self.hide_menu();
            }
            ShellAction::LaunchApplication(id) => {
                if let Some(application) = self
                    .applications
                    .iter()
                    .find(|application| application.id == id)
                {
                    launch_application(application);
                }
                self.hide_menu();
            }
            ShellAction::ActivateWindow(id) => {
                if let Some(toplevel) = self.exact_toplevels.get(&id)
                    && let Err(error) = sway_ipc::focus(&toplevel.identifier)
                {
                    eprintln!("wildbuzzard-shell: focus failed: {error:#}");
                }
                self.hide_menu();
            }
            ShellAction::BringIntoViewWindow(id) => {
                if let Some(toplevel) = self.exact_toplevels.get(&id)
                    && let Err(error) = sway_ipc::bring_into_view(&toplevel.identifier)
                {
                    eprintln!("wildbuzzard-shell: bring into view failed: {error:#}");
                }
                self.hide_menu();
            }
            ShellAction::MinimizeWindow(id) => {
                if let Some(toplevel) = self.exact_toplevels.get(&id)
                    && let Err(error) = sway_ipc::minimize(&toplevel.identifier)
                {
                    eprintln!("wildbuzzard-shell: minimize failed: {error:#}");
                }
                self.hide_menu();
            }
            ShellAction::ToggleMaximizeWindow(id) => {
                if let Some(toplevel) = self.exact_toplevels.get(&id) {
                    if let Err(error) = sway_ipc::toggle_maximize(&toplevel.identifier) {
                        eprintln!("wildbuzzard-shell: maximize/restore failed: {error:#}");
                    }
                }
                self.hide_menu();
            }
            ShellAction::CloseWindow(id) => {
                if let Some(toplevel) = self.exact_toplevels.get(&id)
                    && let Err(error) = sway_ipc::close(&toplevel.identifier)
                {
                    eprintln!("wildbuzzard-shell: close failed: {error:#}");
                }
                self.hide_menu();
            }
            ShellAction::TaskbarPrevious => {
                self.task_page = self.task_page.saturating_sub(1);
                self.dirty = true;
            }
            ShellAction::TaskbarNext => {
                self.task_page = self.task_page.saturating_add(1);
                self.dirty = true;
            }
            ShellAction::ShowDesktop => {
                if let Err(error) = sway_ipc::minimize_all_visible() {
                    eprintln!("wildbuzzard-shell: show desktop failed: {error:#}");
                }
                self.hide_menu();
            }
            ShellAction::CloseApplicationsMenu => self.hide_menu(),
            ShellAction::ShutdownMachine => {
                spawn("sudo", ["-n", "systemctl", "poweroff"]);
                self.hide_menu();
            }
        }
    }

    fn target_at_surface(
        &self,
        surface: &wl_surface::WlSurface,
        x: f64,
        y: f64,
    ) -> Option<HitTarget> {
        if surface == self.panel.wl_surface() {
            panel_targets(self.panel_size.0, &self.windows(), self.task_page)
                .into_iter()
                .find(|target| target.rect.contains(x, y))
        } else if surface == self.menu.wl_surface() && self.menu_open {
            match self.menu_kind {
                MenuKind::Applications => {
                    std::iter::once(applications_menu_close_target(self.menu_size.0))
                        .chain(menu_targets(
                            self.menu_size.0,
                            self.menu_size.1,
                            &self.applications,
                            self.menu_scroll,
                        ))
                        .find(|target| target.rect.contains(x, y))
                }
                MenuKind::Window(id) => self.exact_toplevels.get(&id).and_then(|window| {
                    window_menu_targets(&window.window)
                        .into_iter()
                        .find(|target| target.rect.contains(x, y))
                }),
            }
        } else if surface == self.desktop.wl_surface() {
            desktop_targets()
                .into_iter()
                .find(|target| target.rect.contains(x, y))
        } else {
            None
        }
    }

    fn click_surface(&mut self, surface: &wl_surface::WlSurface, x: f64, y: f64) {
        let target = self.target_at_surface(surface, x, y);
        if let Some(target) = target {
            self.activate(target.action);
        }
    }

    fn update_hover(&mut self, surface: &wl_surface::WlSurface, x: f64, y: f64) {
        let hovered = self
            .target_at_surface(surface, x, y)
            .map(|target| target.action);
        if hovered != self.hovered {
            self.hovered = hovered;
            self.dirty = true;
        }
    }

    fn secondary_click_panel(&mut self, x: f64, y: f64) {
        let target = panel_targets(self.panel_size.0, &self.windows(), self.task_page)
            .into_iter()
            .find(|target| target.rect.contains(x, y));
        if let Some(HitTarget {
            action: ShellAction::ActivateWindow(id),
            ..
        }) = target
        {
            self.show_taskbar_window_menu(id, x);
        } else {
            self.hide_menu();
        }
    }

    fn scroll_menu(&mut self, amount: f64) {
        if !self.menu_open || self.menu_kind != MenuKind::Applications || amount == 0.0 {
            return;
        }
        if amount > 0.0 {
            self.menu_scroll = self.menu_scroll.saturating_add(1);
        } else {
            self.menu_scroll = self.menu_scroll.saturating_sub(1);
        }
        self.clamp_menu_scroll();
        self.dirty = true;
    }

    fn scroll_menu_item_into_view(&mut self, index: usize) {
        if !self.menu_open {
            return;
        }
        let visible = self.visible_menu_rows();
        if visible == 0 {
            return;
        }
        if index < self.menu_scroll {
            self.menu_scroll = index;
        } else if index >= self.menu_scroll.saturating_add(visible) {
            self.menu_scroll = index.saturating_add(1).saturating_sub(visible);
        }
        self.clamp_menu_scroll();
        self.dirty = true;
    }

    fn draw(&mut self) -> Result<()> {
        if self.desktop_configured {
            self.draw_desktop()?;
        }
        if self.panel_configured {
            self.draw_panel()?;
        }
        if self.menu_configured {
            self.draw_menu()?;
        }
        self.update_accessibility();
        Ok(())
    }

    fn draw_desktop(&mut self) -> Result<()> {
        let (logical_width, logical_height) = nonzero_size(self.desktop_size);
        let (width, height) = physical_size((logical_width, logical_height), self.scale_120);
        let (buffer, canvas) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                width as i32 * 4,
                wl_shm::Format::Argb8888,
            )
            .context("allocating desktop frame")?;
        clear(canvas, theme::DESKTOP);
        for target in desktop_targets() {
            if self.hovered.as_ref() == Some(&target.action) {
                fill_rect(
                    canvas,
                    width,
                    height,
                    scale_rect(inset(target.rect, 3), self.scale_120),
                    theme::HOVER,
                );
            }
            draw_desktop_shortcut(
                canvas,
                width,
                height,
                self.font.as_ref(),
                &target,
                self.scale_120,
            );
        }
        attach(
            &self.desktop,
            &self.viewports[0],
            buffer,
            width,
            height,
            logical_width,
            logical_height,
        )?;
        Ok(())
    }

    fn draw_panel(&mut self) -> Result<()> {
        let (logical_width, logical_height) = nonzero_size(self.panel_size);
        let (width, height) = physical_size((logical_width, logical_height), self.scale_120);
        let windows = self.windows();
        let (buffer, canvas) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                width as i32 * 4,
                wl_shm::Format::Argb8888,
            )
            .context("allocating panel frame")?;
        clear(canvas, theme::CANVAS);
        fill_rect(
            canvas,
            width,
            height,
            scale_rect(
                Rect {
                    x: 0,
                    y: 0,
                    width: logical_width as i32,
                    height: 1,
                },
                self.scale_120,
            ),
            theme::BORDER,
        );
        for target in panel_targets(logical_width, &windows, self.task_page) {
            let hovered = self.hovered.as_ref() == Some(&target.action);
            let (color, label, active) = match target.action {
                ShellAction::ToggleApplications => {
                    let active = self.menu_open && self.menu_kind == MenuKind::Applications;
                    (
                        if active {
                            theme::SELECTION
                        } else if hovered {
                            theme::HOVER
                        } else {
                            theme::SURFACE
                        },
                        "Applications".to_owned(),
                        active,
                    )
                }
                ShellAction::OpenFiles => (
                    if hovered {
                        theme::HOVER
                    } else {
                        theme::SURFACE
                    },
                    "Files".to_owned(),
                    false,
                ),
                ShellAction::OpenShared => (
                    if hovered {
                        theme::HOVER
                    } else {
                        theme::SURFACE
                    },
                    "Share".to_owned(),
                    false,
                ),
                ShellAction::ActivateWindow(id) => {
                    let window = windows.iter().find(|window| window.id == id);
                    let focused = window.is_some_and(|window| window.focused);
                    let title = window
                        .map(|window| elide(&window.title, 25))
                        .unwrap_or_else(|| target.label.clone());
                    (
                        if focused {
                            theme::RAISED
                        } else {
                            if hovered {
                                theme::HOVER
                            } else {
                                theme::SURFACE
                            }
                        },
                        title,
                        focused,
                    )
                }
                ShellAction::TaskbarPrevious => (
                    if hovered {
                        theme::HOVER
                    } else {
                        theme::SURFACE
                    },
                    "‹".to_owned(),
                    false,
                ),
                ShellAction::TaskbarNext => (
                    if hovered {
                        theme::HOVER
                    } else {
                        theme::SURFACE
                    },
                    "›".to_owned(),
                    false,
                ),
                ShellAction::ShowDesktop => (
                    if hovered {
                        theme::HOVER
                    } else {
                        theme::SURFACE
                    },
                    String::new(),
                    false,
                ),
                _ => continue,
            };
            let button_rect = scale_rect(inset(target.rect, 2), self.scale_120);
            fill_rect(canvas, width, height, button_rect, color);
            if active {
                fill_rect(
                    canvas,
                    width,
                    height,
                    Rect {
                        x: button_rect.x,
                        y: button_rect.y
                            + button_rect
                                .height
                                .saturating_sub(scale_coord(2, self.scale_120)),
                        width: button_rect.width,
                        height: scale_coord(2, self.scale_120),
                    },
                    theme::FOCUS,
                );
            }
            draw_text_centered(
                canvas,
                width,
                height,
                self.font.as_ref(),
                &label,
                scale_rect(target.rect, self.scale_120),
                scale_font(13.0, self.scale_120),
                if color == theme::SELECTION {
                    theme::SELECTED_TEXT
                } else {
                    theme::TEXT
                },
            );
        }
        attach(
            &self.panel,
            &self.viewports[1],
            buffer,
            width,
            height,
            logical_width,
            logical_height,
        )?;
        Ok(())
    }

    fn draw_menu(&mut self) -> Result<()> {
        let (logical_width, logical_height) = nonzero_size(self.menu_size);
        let (width, height) = physical_size((logical_width, logical_height), self.scale_120);
        let visible_menu_rows = self.visible_menu_rows();
        let (buffer, canvas) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                width as i32 * 4,
                wl_shm::Format::Argb8888,
            )
            .context("allocating applications menu frame")?;
        clear(
            canvas,
            if self.menu_open {
                theme::MENU
            } else {
                [0, 0, 0, 0]
            },
        );
        if self.menu_open && self.menu_kind == MenuKind::Applications {
            fill_rect(
                canvas,
                width,
                height,
                scale_rect(
                    Rect {
                        x: 0,
                        y: 0,
                        width: logical_width as i32,
                        height: APPLICATIONS_MENU_HEADER_HEIGHT,
                    },
                    self.scale_120,
                ),
                theme::RAISED,
            );
            fill_rect(
                canvas,
                width,
                height,
                scale_rect(
                    Rect {
                        x: 0,
                        y: APPLICATIONS_MENU_HEADER_HEIGHT - 2,
                        width: logical_width as i32,
                        height: 2,
                    },
                    self.scale_120,
                ),
                theme::SELECTION,
            );
            draw_text(
                canvas,
                width,
                height,
                self.font.as_ref(),
                &elide_to_width(
                    self.font.as_ref(),
                    "Applications",
                    17.0,
                    (logical_width as i32)
                        .saturating_sub(18)
                        .saturating_sub(46)
                        .saturating_sub(8)
                        .max(0) as f32,
                ),
                scale_coord(18, self.scale_120),
                scale_coord(16, self.scale_120),
                scale_font(17.0, self.scale_120),
                theme::TEXT,
            );
            let close = applications_menu_close_target(logical_width);
            let close_hovered = self.hovered.as_ref() == Some(&close.action);
            if close_hovered {
                fill_rect(
                    canvas,
                    width,
                    height,
                    scale_rect(inset(close.rect, 3), self.scale_120),
                    theme::HOVER,
                );
            }
            draw_text_centered(
                canvas,
                width,
                height,
                self.font.as_ref(),
                "×",
                scale_rect(close.rect, self.scale_120),
                scale_font(18.0, self.scale_120),
                theme::TEXT,
            );
            let app_section_y = APPLICATIONS_MENU_HEADER_HEIGHT;
            draw_text(
                canvas,
                width,
                height,
                self.font.as_ref(),
                "ALL APPLICATIONS",
                scale_coord(16, self.scale_120),
                scale_coord(app_section_y + 7, self.scale_120),
                scale_font(10.0, self.scale_120),
                theme::TEXT_MUTED,
            );
            for target in menu_targets(
                logical_width,
                logical_height,
                &self.applications,
                self.menu_scroll,
            ) {
                let footer = matches!(target.action, ShellAction::ShutdownMachine);
                let hovered = self.hovered.as_ref() == Some(&target.action);
                fill_rect(
                    canvas,
                    width,
                    height,
                    scale_rect(inset(target.rect, 2), self.scale_120),
                    if hovered {
                        if footer {
                            theme::DESTRUCTIVE
                        } else {
                            theme::HOVER
                        }
                    } else {
                        theme::MENU
                    },
                );
                let icon_rect = scale_rect(
                    Rect {
                        x: target.rect.x + 9,
                        y: target.rect.y + 8,
                        width: 20,
                        height: 20,
                    },
                    self.scale_120,
                );
                let app_icon = match &target.action {
                    ShellAction::LaunchApplication(id) => self
                        .applications
                        .iter()
                        .find(|application| &application.id == id)
                        .and_then(|application| application.icon.as_ref())
                        .and_then(|icon| self.application_icons.get(icon)),
                    _ => None,
                };
                if let Some(icon) = app_icon {
                    draw_app_icon(canvas, width, height, icon_rect, icon);
                } else {
                    draw_menu_icon(canvas, width, height, icon_rect, &target.action);
                }
                draw_text(
                    canvas,
                    width,
                    height,
                    self.font.as_ref(),
                    &elide_to_width(
                        self.font.as_ref(),
                        &target.label,
                        13.0,
                        target.rect.width.saturating_sub(54) as f32,
                    ),
                    scale_coord(target.rect.x + 38, self.scale_120),
                    scale_coord(target.rect.y + 10, self.scale_120),
                    scale_font(13.0, self.scale_120),
                    theme::TEXT,
                );
            }
            if self.menu_scroll > 0 {
                draw_text(
                    canvas,
                    width,
                    height,
                    self.font.as_ref(),
                    "▲ more",
                    scale_coord(logical_width as i32 - 72, self.scale_120),
                    scale_coord(app_section_y + 7, self.scale_120),
                    scale_font(9.0, self.scale_120),
                    theme::TEXT_SECONDARY,
                );
            }
            if self.menu_scroll + visible_menu_rows < self.applications.len() {
                draw_text(
                    canvas,
                    width,
                    height,
                    self.font.as_ref(),
                    "scroll ▼",
                    scale_coord(logical_width as i32 - 72, self.scale_120),
                    scale_coord(logical_height as i32 - 62, self.scale_120),
                    scale_font(9.0, self.scale_120),
                    theme::TEXT_SECONDARY,
                );
            }
        } else if self.menu_open
            && let MenuKind::Window(id) = self.menu_kind
            && let Some(toplevel) = self.exact_toplevels.get(&id)
        {
            fill_rect(
                canvas,
                width,
                height,
                scale_rect(
                    Rect {
                        x: 0,
                        y: 0,
                        width: logical_width as i32,
                        height: 44,
                    },
                    self.scale_120,
                ),
                theme::RAISED,
            );
            fill_rect(
                canvas,
                width,
                height,
                scale_rect(
                    Rect {
                        x: 0,
                        y: 42,
                        width: logical_width as i32,
                        height: 2,
                    },
                    self.scale_120,
                ),
                theme::SELECTION,
            );
            draw_text(
                canvas,
                width,
                height,
                self.font.as_ref(),
                &elide(&toplevel.window.title, 28),
                scale_coord(14, self.scale_120),
                scale_coord(13, self.scale_120),
                scale_font(14.0, self.scale_120),
                theme::TEXT,
            );
            for target in window_menu_targets(&toplevel.window) {
                let hovered = self.hovered.as_ref() == Some(&target.action);
                fill_rect(
                    canvas,
                    width,
                    height,
                    scale_rect(inset(target.rect, 2), self.scale_120),
                    if hovered {
                        if matches!(target.action, ShellAction::CloseWindow(_)) {
                            theme::DESTRUCTIVE
                        } else {
                            theme::HOVER
                        }
                    } else {
                        theme::MENU
                    },
                );
                draw_text(
                    canvas,
                    width,
                    height,
                    self.font.as_ref(),
                    &target.label,
                    scale_coord(target.rect.x + 12, self.scale_120),
                    scale_coord(target.rect.y + 10, self.scale_120),
                    scale_font(13.0, self.scale_120),
                    theme::TEXT,
                );
            }
        }
        attach(
            &self.menu,
            &self.viewports[2],
            buffer,
            width,
            height,
            logical_width,
            logical_height,
        )?;
        Ok(())
    }

    fn update_accessibility(&mut self) {
        let (tree, targets) = self.accessibility_tree();
        if let Some(accessibility) = self.accessibility.as_mut() {
            if let Ok(mut snapshot) = accessibility.snapshot.lock() {
                *snapshot = tree.clone();
            }
            accessibility.targets = targets;
            accessibility.adapter.update_if_active(|| tree);
        }
    }

    fn accessibility_tree(&self) -> (TreeUpdate, BTreeMap<NodeId, AccessibleTarget>) {
        const ROOT: NodeId = NodeId(1);
        const MENU: NodeId = NodeId(9_000);
        let mut nodes = Vec::new();
        let mut children = Vec::new();
        let mut targets = BTreeMap::new();
        let panel_y = self.desktop_size.1.saturating_sub(self.panel_size.1) as i32;
        let (menu_x, menu_y) = self.menu_origin;
        let windows = self.windows();
        let applications_menu_open = self.menu_open && self.menu_kind == MenuKind::Applications;

        for (index, target) in desktop_targets().into_iter().enumerate() {
            add_accessible_target(
                &mut nodes,
                &mut children,
                &mut targets,
                NodeId(100 + index as u64),
                target,
                0,
                0,
            );
        }
        let panel_targets = panel_targets(self.panel_size.0, &windows, self.task_page);
        if let Some(target) = panel_targets
            .iter()
            .find(|target| matches!(target.action, ShellAction::ToggleApplications))
            .cloned()
        {
            add_accessible_target(
                &mut nodes,
                &mut children,
                &mut targets,
                NodeId(1_000),
                target,
                0,
                panel_y,
            );
        }
        if let Some(target) = panel_targets
            .iter()
            .find(|target| matches!(target.action, ShellAction::ShowDesktop))
            .cloned()
        {
            add_accessible_target(
                &mut nodes,
                &mut children,
                &mut targets,
                NodeId(1_001),
                target,
                0,
                panel_y,
            );
        }
        for (index, window) in windows.iter().enumerate() {
            add_accessible_target(
                &mut nodes,
                &mut children,
                &mut targets,
                NodeId(2_000 + index as u64),
                HitTarget {
                    rect: Rect {
                        x: 126 + i32::try_from(index).unwrap_or(i32::MAX).saturating_mul(148),
                        y: 0,
                        width: 148,
                        height: PANEL_HEIGHT,
                    },
                    label: format!("Switch to {}", window.title),
                    action: ShellAction::ActivateWindow(window.id),
                },
                0,
                panel_y,
            );
            for (control_index, target) in window_menu_targets(window).into_iter().enumerate() {
                let id = NodeId(
                    30_000
                        + u64::try_from(index).unwrap_or_default() * 10
                        + u64::try_from(control_index).unwrap_or_default(),
                );
                let mut node = A11yNode::new(Role::Button);
                node.set_label(format!("{} {}", target.label, window.title));
                if self.menu_open && self.menu_kind == MenuKind::Window(window.id) {
                    node.set_bounds(a11y_rect(target.rect, menu_x, menu_y));
                }
                node.add_action(Action::Click);
                children.push(id);
                targets.insert(
                    id,
                    AccessibleTarget::Activate {
                        action: target.action,
                        menu_index: None,
                    },
                );
                nodes.push((id, node));
            }
        }
        // Keep every installed application in the accessibility tree even
        // while the visual menu is closed or scrolled. An in-guest agent can
        // therefore invoke any application directly without first opening or
        // paging the visible menu.
        let visible_rows = self.visible_menu_rows();
        let mut menu_children = Vec::new();
        for (index, application) in self.applications.iter().enumerate() {
            let id = NodeId(10_000 + index as u64);
            let relative_row = i32::try_from(index)
                .unwrap_or(i32::MAX)
                .saturating_sub(i32::try_from(self.menu_scroll).unwrap_or(i32::MAX));
            let mut node = A11yNode::new(Role::MenuItem);
            node.set_label(application.name.clone());
            if applications_menu_open
                && index >= self.menu_scroll
                && index < self.menu_scroll.saturating_add(visible_rows)
            {
                node.set_bounds(a11y_rect(
                    Rect {
                        x: 8,
                        y: APPLICATIONS_MENU_HEADER_HEIGHT
                            + APPLICATIONS_MENU_SECTION_HEIGHT
                            + relative_row.saturating_mul(MENU_ROW_HEIGHT),
                        width: i32::try_from(self.menu_size.0)
                            .unwrap_or(i32::MAX)
                            .saturating_sub(16),
                        height: MENU_ROW_HEIGHT,
                    },
                    menu_x,
                    menu_y,
                ));
            }
            node.set_position_in_set(index + 1);
            node.add_action(Action::Click);
            if applications_menu_open {
                node.add_action(Action::ScrollIntoView);
            }
            nodes.push((id, node));
            menu_children.push(id);
            targets.insert(
                id,
                AccessibleTarget::Activate {
                    action: ShellAction::LaunchApplication(application.id.clone()),
                    menu_index: Some(index),
                },
            );
        }
        let shutdown_id = NodeId(20_000);
        let mut shutdown = A11yNode::new(Role::MenuItem);
        shutdown.set_label("Shut Down Machine");
        if applications_menu_open {
            shutdown.set_bounds(a11y_rect(
                Rect {
                    x: 8,
                    y: i32::try_from(self.menu_size.1)
                        .unwrap_or(i32::MAX)
                        .saturating_sub(44),
                    width: i32::try_from(self.menu_size.0)
                        .unwrap_or(i32::MAX)
                        .saturating_sub(16),
                    height: MENU_ROW_HEIGHT,
                },
                menu_x,
                menu_y,
            ));
        }
        shutdown.add_action(Action::Click);
        nodes.push((shutdown_id, shutdown));
        menu_children.push(shutdown_id);
        targets.insert(
            shutdown_id,
            AccessibleTarget::Activate {
                action: ShellAction::ShutdownMachine,
                menu_index: None,
            },
        );
        let close_id = NodeId(20_001);
        let close_target = applications_menu_close_target(self.menu_size.0);
        let mut close = A11yNode::new(Role::Button);
        close.set_label(close_target.label.clone());
        if applications_menu_open {
            close.set_bounds(a11y_rect(close_target.rect, menu_x, menu_y));
        } else {
            // AccessKit's AT-SPI adapter keeps an accessible object's
            // supported interface set stable for that object's lifetime.
            // Give this conditionally visible control a component interface
            // from its first snapshot so its real bounds become queryable
            // when the menu opens.
            close.set_bounds(A11yRect::new(0.0, 0.0, 0.0, 0.0));
        }
        close.add_action(Action::Click);
        nodes.push((close_id, close));
        menu_children.push(close_id);
        targets.insert(
            close_id,
            AccessibleTarget::Activate {
                action: close_target.action,
                menu_index: None,
            },
        );

        let mut menu = A11yNode::new(Role::Menu);
        menu.set_label("Applications menu");
        menu.set_expanded(applications_menu_open);
        if applications_menu_open {
            menu.set_bounds(A11yRect::new(
                f64::from(menu_x),
                f64::from(menu_y),
                f64::from(menu_x) + f64::from(self.menu_size.0),
                f64::from(menu_y) + f64::from(self.menu_size.1),
            ));
        }
        menu.set_children(menu_children);
        menu.set_size_of_set(self.applications.len());
        if applications_menu_open {
            menu.set_scroll_y(self.menu_scroll as f64);
            menu.set_scroll_y_min(0.0);
            menu.set_scroll_y_max(self.applications.len().saturating_sub(visible_rows) as f64);
            menu.add_action(Action::ScrollDown);
            menu.add_action(Action::ScrollUp);
        }
        nodes.push((MENU, menu));
        targets.insert(MENU, AccessibleTarget::ScrollMenu);
        children.push(MENU);
        let mut root = A11yNode::new(Role::Window);
        root.set_label(SHELL_NAME);
        root.set_bounds(A11yRect::new(
            0.0,
            0.0,
            f64::from(self.desktop_size.0),
            f64::from(self.desktop_size.1),
        ));
        root.set_children(children);
        nodes.push((ROOT, root));
        (
            TreeUpdate {
                nodes,
                tree: Some(Tree::new(ROOT)),
                tree_id: TreeId::ROOT,
                focus: ROOT,
            },
            targets,
        )
    }
}

fn add_accessible_target(
    nodes: &mut Vec<(NodeId, A11yNode)>,
    children: &mut Vec<NodeId>,
    targets: &mut BTreeMap<NodeId, AccessibleTarget>,
    id: NodeId,
    target: HitTarget,
    offset_x: i32,
    offset_y: i32,
) {
    let mut node = A11yNode::new(Role::Button);
    node.set_label(target.label);
    node.set_bounds(a11y_rect(target.rect, offset_x, offset_y));
    node.add_action(Action::Click);
    children.push(id);
    targets.insert(
        id,
        AccessibleTarget::Activate {
            action: target.action,
            menu_index: None,
        },
    );
    nodes.push((id, node));
}

impl ForeignToplevelListHandler for Shell {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelList {
        &mut self.foreign_toplevel_list
    }

    fn new_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        handle: ExtForeignToplevelHandleV1,
    ) {
        self.update_exact_toplevel(&handle);
    }

    fn update_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        handle: ExtForeignToplevelHandleV1,
    ) {
        self.update_exact_toplevel(&handle);
    }

    fn toplevel_closed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        handle: ExtForeignToplevelHandleV1,
    ) {
        let id = handle.id().protocol_id();
        self.exact_toplevels.remove(&id);
        if self.menu_kind == MenuKind::Window(id) {
            self.hide_menu();
        }
        self.dirty = true;
    }
}

impl Dispatch<WpFractionalScaleV1, ShellSurface> for Shell {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: FractionalScaleEvent,
        _: &ShellSurface,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let FractionalScaleEvent::PreferredScale { scale } = event else {
            return;
        };
        if state.scale_120 != scale {
            state.scale_120 = scale.max(120);
            state.dirty = true;
        }
    }
}

impl CompositorHandler for Shell {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
        self.dirty = true;
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
        self.dirty = true;
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for Shell {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let size = (configure.new_size.0.max(1), configure.new_size.1.max(1));
        if layer == &self.desktop {
            let desktop_size_changed = self.desktop_size != size;
            self.desktop_size = size;
            self.desktop_configured = true;
            eprintln!(
                "wildbuzzard-shell: desktop configured {}x{}",
                size.0, size.1
            );
            if self.menu_open && desktop_size_changed {
                match self.menu_kind {
                    MenuKind::Applications => {
                        self.apply_applications_menu_geometry();
                    }
                    // A context menu is transient state tied to the old output
                    // geometry. Dismiss it rather than leave it detached from
                    // its titlebar or task button after an output resize.
                    MenuKind::Window(_) => self.hide_menu(),
                }
            }
        } else if layer == &self.panel {
            self.panel_size = size;
            self.panel_configured = true;
            eprintln!("wildbuzzard-shell: panel configured {}x{}", size.0, size.1);
        } else if layer == &self.menu {
            self.menu_size = size;
            self.menu_configured = true;
            self.clamp_menu_scroll();
            let _ = self.set_menu_input_region();
        }
        self.dirty = true;
    }
}

impl SeatHandler for Shell {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.is_none() {
            self.seat = Some(seat);
        }
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        self.seat.get_or_insert_with(|| seat.clone());
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer
            && let Some(pointer) = self.pointer.take()
        {
            pointer.release();
        }
        if capability == Capability::Keyboard
            && let Some(keyboard) = self.keyboard.take()
        {
            keyboard.release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        if self.seat.as_ref() == Some(&seat) {
            self.seat = None;
        }
    }
}

impl PointerHandler for Shell {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {
                    self.update_hover(&event.surface, event.position.0, event.position.1);
                }
                PointerEventKind::Leave { .. } => {
                    if self.hovered.take().is_some() {
                        self.dirty = true;
                    }
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    self.click_surface(&event.surface, event.position.0, event.position.1);
                }
                PointerEventKind::Press { button, .. }
                    if button == BTN_RIGHT && event.surface == *self.panel.wl_surface() =>
                {
                    self.secondary_click_panel(event.position.0, event.position.1);
                }
                PointerEventKind::Axis { vertical, .. }
                    if event.surface == *self.menu.wl_surface() =>
                {
                    let amount = if vertical.value120 != 0 {
                        f64::from(vertical.value120)
                    } else if vertical.discrete != 0 {
                        f64::from(vertical.discrete)
                    } else {
                        vertical.absolute
                    };
                    self.scroll_menu(amount);
                }
                _ => {}
            }
        }
    }
}

impl KeyboardHandler for Shell {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if let Some(accessibility) = self.accessibility.as_mut() {
            accessibility.adapter.update_window_focus_state(true);
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(accessibility) = self.accessibility.as_mut() {
            accessibility.adapter.update_window_focus_state(false);
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.menu_open && event.keysym == Keysym::Escape {
            self.hide_menu();
        }
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
}

impl OutputHandler for Shell {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for Shell {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Shell {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(Shell);
delegate_output!(Shell);
delegate_shm!(Shell);
delegate_seat!(Shell);
delegate_keyboard!(Shell);
delegate_pointer!(Shell);
delegate_layer!(Shell);
delegate_foreign_toplevel_list!(Shell);
delegate_registry!(Shell);
wayland_client::delegate_noop!(Shell: ignore WpFractionalScaleManagerV1);
wayland_client::delegate_noop!(Shell: ignore WpViewporter);
wayland_client::delegate_noop!(Shell: ignore WpViewport);

fn spawn<I, S>(program: &str, arguments: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    if let Err(error) = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        eprintln!("wildbuzzard-shell: launching {program} failed: {error}");
    }
}

fn launch_application(application: &Application) {
    let Some((program, arguments)) = application.command.split_first() else {
        return;
    };
    if let Err(error) = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        eprintln!(
            "wildbuzzard-shell: launching {} from {} failed: {error}",
            application.name,
            application.source.display()
        );
    }
}

fn attach(
    layer: &LayerSurface,
    viewport: &WpViewport,
    buffer: smithay_client_toolkit::shm::slot::Buffer,
    buffer_width: u32,
    buffer_height: u32,
    logical_width: u32,
    logical_height: u32,
) -> Result<()> {
    viewport.set_destination(logical_width as i32, logical_height as i32);
    layer
        .wl_surface()
        .damage_buffer(0, 0, buffer_width as i32, buffer_height as i32);
    buffer
        .attach_to(layer.wl_surface())
        .context("attaching shell frame")?;
    layer.commit();
    Ok(())
}

fn nonzero_size(size: (u32, u32)) -> (u32, u32) {
    (size.0.max(1), size.1.max(1))
}

fn physical_size(logical: (u32, u32), scale_120: u32) -> (u32, u32) {
    let scale = |value: u32| {
        value
            .saturating_mul(scale_120)
            .saturating_add(60)
            .checked_div(120)
            .unwrap_or(1)
            .max(1)
    };
    (scale(logical.0), scale(logical.1))
}

fn scale_coord(value: i32, scale_120: u32) -> i32 {
    let value = i64::from(value);
    let scale = i64::from(scale_120);
    i32::try_from((value * scale + 60) / 120).unwrap_or(if value.is_negative() {
        i32::MIN
    } else {
        i32::MAX
    })
}

fn scale_rect(rect: Rect, scale_120: u32) -> Rect {
    let x1 = scale_coord(rect.x, scale_120);
    let y1 = scale_coord(rect.y, scale_120);
    let x2 = scale_coord(rect.x.saturating_add(rect.width), scale_120);
    let y2 = scale_coord(rect.y.saturating_add(rect.height), scale_120);
    Rect {
        x: x1,
        y: y1,
        width: x2.saturating_sub(x1),
        height: y2.saturating_sub(y1),
    }
}

fn scale_font(size: f32, scale_120: u32) -> f32 {
    size * scale_120 as f32 / 120.0
}

fn load_font() -> Option<Font> {
    for path in [
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(path)
            && let Ok(font) = Font::from_bytes(bytes, FontSettings::default())
        {
            return Some(font);
        }
    }
    None
}

fn applications_menu_height(application_count: usize, desktop_height: u32) -> u32 {
    let fixed = APPLICATIONS_MENU_HEADER_HEIGHT
        + APPLICATIONS_MENU_SECTION_HEIGHT
        + APPLICATIONS_MENU_FOOTER_HEIGHT;
    let row_count = i32::try_from(application_count).unwrap_or(i32::MAX);
    let content_height = fixed.saturating_add(row_count.saturating_mul(MENU_ROW_HEIGHT));
    let available_height = desktop_height.saturating_sub(PANEL_HEIGHT as u32).max(1);
    u32::try_from(content_height.max(1))
        .unwrap_or(u32::MAX)
        .min(available_height)
}

fn text_width(font: Option<&Font>, text: &str, size: f32) -> f32 {
    font.map_or_else(
        || text.chars().count() as f32 * size * 0.62,
        |font| {
            text.chars()
                .map(|character| font.metrics(character, size).advance_width)
                .sum()
        },
    )
}

fn applications_menu_width(
    font: Option<&Font>,
    applications: &[Application],
    desktop_width: u32,
) -> u32 {
    let longest_application = applications
        .iter()
        .map(|application| text_width(font, &application.name, 13.0))
        .fold(0.0_f32, f32::max);
    // The measured row width includes the menu inset, icon, icon/text gap,
    // and enough trailing space to keep glyph antialiasing out of the edge.
    let row_width = longest_application.max(text_width(font, "Shut Down Machine", 13.0)) + 64.0;
    let header_width = text_width(font, "Applications", 17.0) + 82.0;
    let measured = row_width.max(header_width).ceil().max(1.0) as u32;

    // The comfort floor follows the current logical output instead of a
    // package-time pixel width. UI scale therefore changes its physical size
    // through the compositor, while small windows still reserve most of
    // their area for applications rather than the launcher.
    let comfort = desktop_width
        .saturating_mul(3)
        .checked_div(16)
        .unwrap_or(1)
        .clamp(220, 360);
    let screen_cap = desktop_width
        .saturating_mul(2)
        .checked_div(3)
        .unwrap_or(1)
        .max(1)
        .min(desktop_width.max(1));
    measured.max(comfort).min(screen_cap)
}

fn inset(rect: Rect, amount: i32) -> Rect {
    Rect {
        x: rect.x + amount,
        y: rect.y + amount,
        width: rect.width.saturating_sub(amount * 2),
        height: rect.height.saturating_sub(amount * 2),
    }
}

fn a11y_rect(rect: Rect, offset_x: i32, offset_y: i32) -> A11yRect {
    A11yRect::new(
        f64::from(rect.x + offset_x),
        f64::from(rect.y + offset_y),
        f64::from(rect.x + offset_x + rect.width),
        f64::from(rect.y + offset_y + rect.height),
    )
}

fn clear(canvas: &mut [u8], color: [u8; 4]) {
    for pixel in canvas.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[color[2], color[1], color[0], color[3]]);
    }
}

fn fill_rect(canvas: &mut [u8], width: u32, height: u32, rect: Rect, color: [u8; 4]) {
    let x0 = rect.x.max(0) as u32;
    let y0 = rect.y.max(0) as u32;
    let x1 = rect.x.saturating_add(rect.width).max(0) as u32;
    let y1 = rect.y.saturating_add(rect.height).max(0) as u32;
    for y in y0.min(height)..y1.min(height) {
        for x in x0.min(width)..x1.min(width) {
            let index = ((y * width + x) * 4) as usize;
            if let Some(pixel) = canvas.get_mut(index..index + 4) {
                pixel.copy_from_slice(&[color[2], color[1], color[0], color[3]]);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    font: Option<&Font>,
    text: &str,
    x: i32,
    y: i32,
    size: f32,
    color: [u8; 4],
) {
    let Some(font) = font else {
        return;
    };
    let ascent = font
        .horizontal_line_metrics(size)
        .map_or(size, |metrics| metrics.ascent)
        .ceil() as i32;
    let mut cursor = x;
    for character in text.chars() {
        let (metrics, bitmap) = font.rasterize(character, size);
        let glyph_top = y
            .saturating_add(ascent)
            .saturating_sub(metrics.ymin)
            .saturating_sub(metrics.height as i32);
        for row in 0..metrics.height {
            for column in 0..metrics.width {
                let alpha = bitmap[row * metrics.width + column];
                if alpha == 0 {
                    continue;
                }
                blend_pixel(
                    canvas,
                    width,
                    height,
                    cursor + metrics.xmin + column as i32,
                    glyph_top + row as i32,
                    color,
                    alpha,
                );
            }
        }
        cursor += metrics.advance_width.ceil() as i32;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text_centered(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    font: Option<&Font>,
    text: &str,
    rect: Rect,
    size: f32,
    color: [u8; 4],
) {
    let Some(font) = font else {
        return;
    };
    let text_width: f32 = text
        .chars()
        .map(|character| font.metrics(character, size).advance_width)
        .sum();
    let x = rect.x + ((rect.width as f32 - text_width).max(0.0) / 2.0) as i32;
    let line_height = font
        .horizontal_line_metrics(size)
        .map_or(size, |metrics| metrics.new_line_size);
    let y = rect.y + ((rect.height as f32 - line_height).max(0.0) / 2.0) as i32;
    draw_text(canvas, width, height, Some(font), text, x, y, size, color);
}

#[allow(clippy::too_many_arguments)]
fn blend_pixel(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    color: [u8; 4],
    alpha: u8,
) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let index = ((y as u32 * width + x as u32) * 4) as usize;
    let Some(pixel) = canvas.get_mut(index..index + 4) else {
        return;
    };
    let source_alpha = u16::from(alpha);
    let inverse = 255 - source_alpha;
    for (channel, source) in [(0, color[2]), (1, color[1]), (2, color[0])] {
        pixel[channel] =
            ((u16::from(source) * source_alpha + u16::from(pixel[channel]) * inverse) / 255) as u8;
    }
    pixel[3] = 255;
}

fn draw_app_icon(
    canvas: &mut [u8],
    canvas_width: u32,
    canvas_height: u32,
    rect: Rect,
    icon: &AppIcon,
) {
    if rect.width <= 0 || rect.height <= 0 || icon.width == 0 || icon.height == 0 {
        return;
    }
    for target_y in 0..rect.height {
        let source_y = u32::try_from(target_y)
            .unwrap_or_default()
            .saturating_mul(icon.height)
            / u32::try_from(rect.height).unwrap_or(1);
        for target_x in 0..rect.width {
            let source_x = u32::try_from(target_x)
                .unwrap_or_default()
                .saturating_mul(icon.width)
                / u32::try_from(rect.width).unwrap_or(1);
            let index = usize::try_from(
                (source_y.min(icon.height - 1) * icon.width + source_x.min(icon.width - 1)) * 4,
            )
            .unwrap_or(usize::MAX);
            let Some(rgba) = icon.rgba.get(index..index.saturating_add(4)) else {
                continue;
            };
            blend_pixel(
                canvas,
                canvas_width,
                canvas_height,
                rect.x + target_x,
                rect.y + target_y,
                [rgba[0], rgba[1], rgba[2], rgba[3]],
                rgba[3],
            );
        }
    }
}

fn draw_desktop_shortcut(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    font: Option<&Font>,
    target: &HitTarget,
    scale_120: u32,
) {
    let rect = target.rect;
    let folder = Rect {
        x: rect.x + 18,
        y: rect.y + 8,
        width: 52,
        height: 42,
    };
    fill_rect(
        canvas,
        width,
        height,
        scale_rect(
            Rect {
                x: folder.x + 4,
                y: folder.y,
                width: 23,
                height: 10,
            },
            scale_120,
        ),
        theme::FOLDER_TAB,
    );
    fill_rect(
        canvas,
        width,
        height,
        scale_rect(folder, scale_120),
        theme::FOLDER,
    );
    if matches!(target.action, ShellAction::OpenShared) {
        fill_rect(
            canvas,
            width,
            height,
            scale_rect(
                Rect {
                    x: folder.x + 19,
                    y: folder.y + 13,
                    width: 14,
                    height: 15,
                },
                scale_120,
            ),
            theme::SURFACE,
        );
    }
    draw_text_centered(
        canvas,
        width,
        height,
        font,
        &target.label,
        scale_rect(
            Rect {
                x: rect.x,
                y: rect.y + 57,
                width: rect.width,
                height: 26,
            },
            scale_120,
        ),
        scale_font(13.0, scale_120),
        theme::TEXT,
    );
}

fn draw_menu_icon(canvas: &mut [u8], width: u32, height: u32, rect: Rect, action: &ShellAction) {
    let color = match action {
        ShellAction::OpenFiles | ShellAction::OpenShared => theme::FOLDER,
        ShellAction::ShutdownMachine => theme::DESTRUCTIVE_ICON,
        ShellAction::LaunchApplication(_) => theme::SELECTION,
        _ => theme::TEXT_SECONDARY,
    };
    fill_rect(canvas, width, height, rect, color);
}

fn elide(text: &str, maximum: usize) -> String {
    if text.chars().count() <= maximum {
        return text.to_owned();
    }
    let keep = maximum.saturating_sub(1);
    format!("{}…", text.chars().take(keep).collect::<String>())
}

fn elide_to_width(font: Option<&Font>, text: &str, size: f32, maximum_width: f32) -> String {
    if maximum_width <= 0.0 {
        return String::new();
    }
    if text_width(font, text, size) <= maximum_width {
        return text.to_owned();
    }
    let ellipsis = "…";
    let ellipsis_width = text_width(font, ellipsis, size);
    if ellipsis_width > maximum_width {
        return String::new();
    }
    let mut output = String::new();
    let mut width = 0.0;
    for character in text.chars() {
        let character_width = text_width(font, &character.to_string(), size);
        if width + character_width + ellipsis_width > maximum_width {
            break;
        }
        output.push(character);
        width += character_width;
    }
    output.push('…');
    output
}

#[cfg(test)]
mod scale_tests {
    use super::{
        PANEL_HEIGHT, WINDOW_MENU_HEIGHT, WINDOW_MENU_WIDTH, applications_menu_height,
        applications_menu_width, parse_window_menu_request, physical_size,
    };
    use crate::model::Application;
    use crate::sway_ipc::Rect;
    use std::path::PathBuf;

    #[test]
    fn fractional_client_buffers_use_protocol_round_half_away() {
        assert_eq!(physical_size((1, 1), 160), (1, 1));
        assert_eq!(physical_size((2, 2), 150), (3, 3));
        assert_eq!(physical_size((3, 3), 150), (4, 4));
        assert_eq!(physical_size((1280, 800), 150), (1600, 1000));
    }

    #[test]
    fn titlebar_context_menu_is_clamped_to_the_visible_guest_workspace() {
        assert_eq!(
            super::titlebar_menu_origin(
                Rect {
                    x: -50,
                    y: 20,
                    width: 640,
                    height: 480,
                },
                31,
                (1280, 800),
                None,
            ),
            (0, 51)
        );
        assert_eq!(
            super::titlebar_menu_origin(
                Rect {
                    x: 1_200,
                    y: 740,
                    width: 640,
                    height: 480,
                },
                31,
                (1280, 800),
                None,
            ),
            (
                1_280 - WINDOW_MENU_WIDTH as i32,
                800 - PANEL_HEIGHT - WINDOW_MENU_HEIGHT as i32,
            )
        );
    }

    #[test]
    fn titlebar_context_menu_uses_the_click_x_and_clamps_near_the_right_edge() {
        let frame = Rect {
            x: 100,
            y: 80,
            width: 900,
            height: 600,
        };
        assert_eq!(
            super::titlebar_menu_origin(frame, 31, (1280, 800), Some((640.75, 95.0))),
            (640, 111)
        );
        assert_eq!(
            super::titlebar_menu_origin(frame, 31, (800, 600), Some((790.0, 95.0))).0,
            800 - WINDOW_MENU_WIDTH as i32
        );
    }

    #[test]
    fn applications_menu_height_fits_content_then_scrolls_at_the_output_edge() {
        assert_eq!(applications_menu_height(3, 1000), 238);
        assert_eq!(
            applications_menu_height(100, 500),
            500 - PANEL_HEIGHT as u32
        );
    }

    #[test]
    fn applications_menu_width_tracks_output_and_measured_labels() {
        let app = |name: &str| Application {
            id: name.to_owned(),
            name: name.to_owned(),
            generic_name: None,
            command: vec!["true".to_owned()],
            icon: None,
            categories: Vec::new(),
            source: PathBuf::from("test.desktop"),
        };
        let ordinary = applications_menu_width(None, &[app("Calculator")], 1600);
        let long = applications_menu_width(
            None,
            &[app("A deliberately much longer installed application name")],
            1600,
        );
        assert_eq!(ordinary, 300);
        assert!(long > ordinary);
        assert!(long <= 1600 * 2 / 3);
        assert!(applications_menu_width(None, &[app("Calculator")], 800) < ordinary);
    }

    #[test]
    fn shell_control_request_preserves_optional_pointer_coordinates() {
        let request = parse_window_menu_request(
            br#"{"schema":1,"identifier":"window-id","x":712.5,"y":91.0}"#,
        )
        .unwrap();
        assert_eq!(request, ("window-id".to_owned(), Some((712.5, 91.0))));
        assert_eq!(
            parse_window_menu_request(b"legacy-window-id").unwrap(),
            ("legacy-window-id".to_owned(), None)
        );
    }
}
