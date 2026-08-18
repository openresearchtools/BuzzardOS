// SPDX-License-Identifier: AGPL-3.0-or-later

mod desktop;
mod icons;
mod model;
mod sway_ipc;
mod watch;

use accesskit::{
    Action, ActionData, ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler,
    Node as A11yNode, NodeId, Rect as A11yRect, Role, Tree, TreeId, TreeUpdate,
};
use accesskit_unix::Adapter as A11yAdapter;
use anyhow::{Context, Result};
use buzzardos_desktop_core::{
    CollisionChoice, DeleteConsequence, DesktopDirectory, DesktopItemKind, RegistrationId,
    Settings, ThemeConfigSet, ThemeMode, ThemePalette, XdgPaths, apply_theme_files, atomic_write,
    read_bounded,
};
use buzzardos_shortcut_helper::{
    HELPER_EXECUTABLE, RegistrationFlags, RegistrationStore, extract_and_launch, launch_path,
};
use fontdue::{Font, FontSettings};
use gio::prelude::*;
use icons::{AppIcon, load_application_icons, load_icon};
use model::{
    APPLICATIONS_MENU_FOOTER_HEIGHT, APPLICATIONS_MENU_HEADER_HEIGHT,
    APPLICATIONS_MENU_SECTION_HEIGHT, Application, GuestWindow, HitTarget, MENU_ROW_HEIGHT,
    PANEL_HEIGHT, Rect, ShellAction, TASK_PAGE_STEP, application_context_targets,
    applications_menu_close_target, builtin_desktop_targets, menu_targets, panel_targets,
    scan_applications, taskbar_max_offset, window_menu_targets,
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
        pointer::{
            BTN_LEFT, BTN_RIGHT, CursorIcon, PointerEvent, PointerEventKind, PointerHandler,
            ThemeSpec, ThemedPointer,
        },
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
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::{fs::PermissionsExt, net::UnixDatagram};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, Sender},
};
use std::time::{Duration, Instant};
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
use wl_clipboard_rs::{copy as clipboard_copy, paste as clipboard_paste};

use crate::desktop::DesktopModel;
use crate::watch::DirectoryWatcher;

const SHELL_NAME: &str = "Buzzard OS Desktop";
const REPAINT_REQUEST: &str = "buzzardos-shell-repaint";
const REPAINT_ACKNOWLEDGEMENT: &str = "buzzardos-shell-repaint-ack";
const SHELL_READY: &str = "shell-ready";
const SHELL_CONTROL_SOCKET: &str = "buzzardos-shell-control.sock";
const REQUEST_FOCUSED_WINDOW_MENU: &str = "--request-focused-window-menu";
const OUTPUT_SETTLE_DEBOUNCE: Duration = Duration::from_millis(80);
const WINDOW_MENU_POINTER_REFRESH_DELAY: Duration = Duration::from_millis(16);
const SETTINGS_POLL_INTERVAL: Duration = Duration::from_millis(250);
const FILE_MODEL_DEBOUNCE: Duration = Duration::from_millis(180);
const WINDOW_MENU_WIDTH: u32 = 260;
const WINDOW_MENU_HEIGHT: u32 = 44 + 5 * MENU_ROW_HEIGHT as u32;
const APPLICATION_CONTEXT_WIDTH: u32 = 252;
const DESKTOP_CONTEXT_WIDTH: u32 = 272;
const DESKTOP_DIALOG_WIDTH: u32 = 430;
const DESKTOP_DIALOG_HEIGHT: u32 = 190;
const DESKTOP_CLIPBOARD_MIME: &str = "application/x-buzzardos-desktop-operation+json";
const URI_LIST_MIME: &str = "text/uri-list";
const MAX_DESKTOP_CLIPBOARD_BYTES: usize = 1024 * 1024;
const DOUBLE_CLICK_MILLIS: u32 = 400;
const DRAG_THRESHOLD: f64 = 6.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellSurface {
    Desktop,
    Panel,
    Menu,
    Context,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ClipboardOperation {
    Copy,
    Cut,
}

#[derive(Debug, Clone)]
struct GuestClipboardRecord {
    generation: u64,
    operation: ClipboardOperation,
    sources: Vec<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ClipboardToken {
    schema: u32,
    generation: u64,
    operation: ClipboardOperation,
}

#[derive(Debug, Clone)]
struct PasteSession {
    operation: ClipboardOperation,
    sources: Vec<PathBuf>,
    index: usize,
}

#[derive(Debug, Clone)]
enum EditOperation {
    NewFolder,
    Rename(PathBuf),
    RenameApplication(RegistrationId),
}

#[derive(Debug, Clone)]
struct EditDialog {
    operation: EditOperation,
    input: String,
    replace_on_type: bool,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct DeleteDialog {
    items: Vec<PathBuf>,
    consequences: Vec<DeleteConsequence>,
    detail: String,
    error: Option<String>,
}

#[derive(Debug, Clone)]
enum ContextState {
    Hidden,
    Application(String),
    DesktopMenu,
    ItemMenu { appimage_registered: Option<bool> },
    Edit(EditDialog),
    Delete(DeleteDialog),
    Collision(PasteSession),
    Error { title: String, detail: String },
}

impl ContextState {
    fn is_visible(&self) -> bool {
        !matches!(self, Self::Hidden)
    }

    fn is_dialog(&self) -> bool {
        matches!(
            self,
            Self::Edit(_) | Self::Delete(_) | Self::Collision(_) | Self::Error { .. }
        )
    }
}

#[derive(Debug, Clone)]
enum DesktopPointerGesture {
    Item {
        path: PathBuf,
        start: (f64, f64),
        current: (f64, f64),
        time: u32,
    },
    RubberBand {
        start: (f64, f64),
        current: (f64, f64),
        base: BTreeSet<PathBuf>,
    },
}

fn main() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--version" || argument == "-V")
    {
        println!("Buzzard OS Desktop {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if std::env::args_os().nth(1).as_deref() == Some(OsStr::new(REQUEST_FOCUSED_WINDOW_MENU)) {
        if let Err(error) = request_focused_window_menu() {
            eprintln!("buzzardos-shell: titlebar menu request failed: {error:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = run() {
        eprintln!("buzzardos-shell: {error:#}");
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
    let payload = serde_json::json!({
        "schema": 1,
        "identifier": focused.identifier,
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

fn parse_window_menu_request(bytes: &[u8]) -> Option<String> {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes)
        && let Some(identifier) = value
            .get("identifier")
            .and_then(serde_json::Value::as_str)
            .filter(|identifier| !identifier.is_empty())
    {
        return Some(identifier.to_owned());
    }
    std::str::from_utf8(bytes)
        .ok()
        .filter(|identifier| !identifier.is_empty())
        .map(str::to_owned)
}

fn titlebar_menu_origin(
    frame: sway_ipc::Rect,
    decoration_height: i32,
    desktop_size: (u32, u32),
    pointer_x: f64,
) -> (i32, i32) {
    let desktop_width = i32::try_from(desktop_size.0).unwrap_or(i32::MAX);
    let desktop_height = i32::try_from(desktop_size.1).unwrap_or(i32::MAX);
    let maximum_left = desktop_width.saturating_sub(WINDOW_MENU_WIDTH as i32);
    let maximum_top = desktop_height
        .saturating_sub(PANEL_HEIGHT)
        .saturating_sub(WINDOW_MENU_HEIGHT as i32);
    let requested_left = if pointer_x.is_finite() {
        pointer_x.floor() as i32
    } else {
        frame.x
    };
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

#[derive(Debug)]
struct SettingsTracker {
    path: PathBuf,
    applied: Settings,
    last_check: Instant,
    last_error: Option<String>,
}

impl SettingsTracker {
    fn new(path: PathBuf, applied: Settings) -> Self {
        Self {
            path,
            applied,
            last_check: Instant::now(),
            last_error: None,
        }
    }

    fn candidate(&mut self) -> Option<Settings> {
        if self.last_check.elapsed() < SETTINGS_POLL_INTERVAL {
            return None;
        }
        self.last_check = Instant::now();
        let candidate = match load_settings(&self.path) {
            Ok(settings) => settings,
            Err(error) => {
                self.report_error(format!("cannot reload {}: {error:#}", self.path.display()));
                return None;
            }
        };
        if candidate == self.applied {
            self.last_error = None;
            return None;
        }
        if candidate.generation < self.applied.generation {
            self.report_error(format!(
                "refusing stale settings generation {}; current generation is {}",
                candidate.generation, self.applied.generation
            ));
            return None;
        }
        if candidate.generation == self.applied.generation {
            self.report_error(format!(
                "settings content changed without advancing generation {}",
                candidate.generation
            ));
            return None;
        }
        Some(candidate)
    }

    fn commit(&mut self, settings: Settings) {
        self.applied = settings;
        self.last_error = None;
    }

    fn reject(&mut self, error: String) {
        self.report_error(error);
    }

    fn report_error(&mut self, error: String) {
        if self.last_error.as_ref() != Some(&error) {
            eprintln!("buzzardos-shell: {error}");
            self.last_error = Some(error);
        }
    }
}

fn load_settings(path: &std::path::Path) -> Result<Settings> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(Settings::load(path)
            .with_context(|| format!("loading persisted settings from {}", path.display()))?
            .value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(error)
            .with_context(|| format!("inspecting persisted settings at {}", path.display())),
    }
}

fn run_required(command: &mut Command, description: &str) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("starting {description}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{description} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

const GNOME_INTERFACE_SCHEMA: &str =
    "/usr/share/glib-2.0/schemas/org.gnome.desktop.interface.gschema.xml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GSettingsAvailability {
    Available,
    MissingSchema,
    MissingTool,
}

fn gsettings_availability(
    schema_path: &std::path::Path,
    executable: &OsStr,
) -> Result<GSettingsAvailability> {
    if !schema_path.is_file() {
        return Ok(GSettingsAvailability::MissingSchema);
    }
    let output = match Command::new(executable)
        .args(["list-keys", "org.gnome.desktop.interface"])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GSettingsAvailability::MissingTool);
        }
        Err(error) => return Err(error).context("starting gsettings schema probe"),
    };
    anyhow::ensure!(
        output.status.success(),
        "gsettings schema probe failed despite {GNOME_INTERFACE_SCHEMA} being present: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(GSettingsAvailability::Available)
}

fn apply_runtime_theme(config_home: &std::path::Path, mode: ThemeMode) -> Result<()> {
    let configs = ThemeConfigSet::for_mode(mode);
    apply_theme_files(config_home, &configs).context("writing per-user theme configuration")?;

    match gsettings_availability(
        std::path::Path::new(GNOME_INTERFACE_SCHEMA),
        OsStr::new("gsettings"),
    )? {
        GSettingsAvailability::Available => {
            for (key, value) in [
                ("gtk-theme", mode.gtk_theme_name()),
                ("icon-theme", "BuzzardOS"),
                ("color-scheme", mode.color_scheme_preference()),
            ] {
                run_required(
                    Command::new("gsettings").args([
                        "set",
                        "org.gnome.desktop.interface",
                        key,
                        value,
                    ]),
                    &format!("updating guest {key}"),
                )?;
            }
        }
        GSettingsAvailability::MissingSchema => eprintln!(
            "buzzardos-shell: theme compatibility warning: org.gnome.desktop.interface is absent; GTK portal propagation is degraded. Inside this persistent guest, run: sudo apt install gsettings-desktop-schemas dconf-gsettings-backend"
        ),
        GSettingsAvailability::MissingTool => eprintln!(
            "buzzardos-shell: theme compatibility warning: gsettings is absent; GTK portal propagation is degraded. Inside this persistent guest, run: sudo apt install libglib2.0-bin gsettings-desktop-schemas dconf-gsettings-backend"
        ),
    }
    sway_ipc::apply_theme(mode.palette()).context("updating Sway decoration palette")?;

    // Broadcast the standard KDE colour-change signal without naming or
    // activating a service. Existing compatible Qt/KDE clients may reload;
    // clients that do not support live recolouring use the new config when
    // reopened. No Plasma or wallet process is involved.
    let _ = Command::new("dbus-send")
        .args([
            "--session",
            "--type=signal",
            "/KGlobalSettings",
            "org.kde.KGlobalSettings.notifyChange",
            "int32:0",
            "int32:0",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // Mako and Foot have fixed, guest-local reload interfaces. Absence of a
    // running client is not an error; newly launched processes read the files.
    let _ = Command::new("makoctl")
        .arg("reload")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = Command::new("pkill")
        .args(["-USR1", "-x", "foot"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok(())
}

fn run() -> Result<()> {
    let config_home = PathBuf::from(
        std::env::var_os("XDG_CONFIG_HOME").context("XDG_CONFIG_HOME is unavailable")?,
    );
    let settings_path = config_home.join("buzzardos/settings.json");
    let initial_settings = match load_settings(&settings_path) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!(
                "buzzardos-shell: persisted settings are unusable and were preserved: {error:#}"
            );
            Settings::default()
        }
    };
    apply_runtime_theme(&config_home, initial_settings.appearance.theme)
        .context("applying the persisted startup theme")?;

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
        Some("buzzardos-desktop"),
        None,
    );
    desktop.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    desktop.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
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
        Some("buzzardos-panel"),
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
        Some("buzzardos-applications-menu"),
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

    let context_surface = compositor.create_surface(&qh);
    let context = layer_shell.create_layer_surface(
        &qh,
        context_surface,
        Layer::Overlay,
        Some("buzzardos-application-context"),
        None,
    );
    context.set_anchor(Anchor::TOP | Anchor::LEFT);
    context.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
    context.set_exclusive_zone(-1);
    context.set_size(1, 1);
    let context_fractional =
        fractional_manager.get_fractional_scale(context.wl_surface(), &qh, ShellSurface::Context);
    let context_viewport = viewporter.get_viewport(context.wl_surface(), &qh, ());
    let empty_context_input =
        Region::new(&compositor).context("creating hidden context input region")?;
    context.set_input_region(Some(empty_context_input.wl_region()));
    context.commit();

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

    let xdg_paths = XdgPaths::discover().context("discovering shell XDG paths")?;
    xdg_paths
        .ensure_private_directories()
        .context("creating private shell state directories")?;
    let application_watch_roots = xdg_paths.application_dirs();
    let application_watcher = DirectoryWatcher::new(&application_watch_roots)
        .context("watching installed applications")?;
    let desktop_model = DesktopModel::discover().context("initializing desktop items")?;
    let desktop_watcher = DirectoryWatcher::new(&[desktop_model.directory_path().to_path_buf()])
        .context("watching XDG Desktop")?;
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
        _fractional_scales: [
            desktop_fractional,
            panel_fractional,
            menu_fractional,
            context_fractional,
        ],
        viewports: [
            desktop_viewport,
            panel_viewport,
            menu_viewport,
            context_viewport,
        ],
        desktop,
        panel,
        menu,
        context,
        pool,
        font: load_font(),
        desktop_size: (1280, 800),
        panel_size: (1280, PANEL_HEIGHT as u32),
        menu_size: (1, 1),
        menu_origin: (0, 0),
        context_size: (1, 1),
        context_origin: (0, 0),
        desktop_configured: false,
        panel_configured: false,
        menu_configured: false,
        context_configured: false,
        menu_open: false,
        menu_kind: MenuKind::Applications,
        window_menu_pending_pointer: false,
        window_menu_pointer_refresh_at: None,
        menu_scroll: 0,
        context_state: ContextState::Hidden,
        desktop_selection: BTreeSet::new(),
        desktop_selection_anchor: None,
        desktop_pointer_gesture: None,
        last_desktop_click: None,
        keyboard_focus: None,
        modifiers: Modifiers::default(),
        guest_clipboard: None,
        clipboard_generation: 0,
        paste_available: false,
        scale_120: 120,
        task_offset: 0,
        capped_task_buttons: initial_settings.appearance.capped_task_buttons,
        pinned_applications: initial_settings
            .appearance
            .pinned_applications
            .iter()
            .cloned()
            .collect(),
        application_search: String::new(),
        hovered: None,
        exit: false,
        dirty: true,
        pointer: None,
        keyboard: None,
        seat: None,
        applications,
        application_icons,
        desktop_icons: BTreeMap::new(),
        application_watch_roots,
        application_watcher,
        application_rescan_after: None,
        desktop_model,
        desktop_hit_targets: Vec::new(),
        desktop_accessible_targets: Vec::new(),
        desktop_watcher,
        desktop_rescan_after: None,
        exact_toplevels: BTreeMap::new(),
        sway_window_changes: sway_ipc::subscribe_window_changes()
            .context("subscribing to authoritative Sway window events")?,
        repaint_request,
        // An output-sync request may predate the shell process by a few
        // milliseconds. Treat the first observed generation as pending.
        repaint_generation: None,
        full_repaint_after: None,
        palette: *initial_settings.appearance.theme.palette(),
        desktop_background: initial_settings.appearance.background.solid_color().rgba(),
        settings_tracker: SettingsTracker::new(settings_path, initial_settings),
        config_home,
        accessibility: None,
        control_socket,
        control_socket_path,
    };
    shell.rebuild_desktop_targets()?;
    shell.set_desktop_input_region()?;
    shell.accessibility = Some(Accessibility::new(shell.accessibility_tree()));
    let shell_ready = std::env::var_os("BUZZARDOS_STATUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/buzzardos-host"))
        .join(SHELL_READY);
    let mut ready_published = false;

    while !shell.exit {
        dispatch_with_timeout(&mut event_queue, &mut shell, Duration::from_millis(16))?;
        shell.poll();
        if shell.dirty {
            shell.draw()?;
            shell.dirty = false;
            if !ready_published && shell.desktop_configured && shell.panel_configured {
                fs::write(&shell_ready, b"ready\n").with_context(|| {
                    format!("publishing shell readiness at {}", shell_ready.display())
                })?;
                ready_published = true;
                eprintln!(
                    "buzzardos-shell: ready at {}x{} logical pixels",
                    shell.desktop_size.0, shell.desktop_size.1
                );
            }
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
    _fractional_scales: [WpFractionalScaleV1; 4],
    viewports: [WpViewport; 4],
    desktop: LayerSurface,
    panel: LayerSurface,
    menu: LayerSurface,
    context: LayerSurface,
    pool: SlotPool,
    font: Option<Font>,
    desktop_size: (u32, u32),
    panel_size: (u32, u32),
    menu_size: (u32, u32),
    menu_origin: (i32, i32),
    context_size: (u32, u32),
    context_origin: (i32, i32),
    desktop_configured: bool,
    panel_configured: bool,
    menu_configured: bool,
    context_configured: bool,
    menu_open: bool,
    menu_kind: MenuKind,
    window_menu_pending_pointer: bool,
    window_menu_pointer_refresh_at: Option<Instant>,
    menu_scroll: usize,
    context_state: ContextState,
    desktop_selection: BTreeSet<PathBuf>,
    desktop_selection_anchor: Option<PathBuf>,
    desktop_pointer_gesture: Option<DesktopPointerGesture>,
    last_desktop_click: Option<(PathBuf, u32)>,
    keyboard_focus: Option<ShellSurface>,
    modifiers: Modifiers,
    guest_clipboard: Option<GuestClipboardRecord>,
    clipboard_generation: u64,
    paste_available: bool,
    scale_120: u32,
    task_offset: usize,
    capped_task_buttons: bool,
    pinned_applications: BTreeSet<String>,
    application_search: String,
    hovered: Option<ShellAction>,
    exit: bool,
    dirty: bool,
    pointer: Option<ThemedPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    seat: Option<wl_seat::WlSeat>,
    applications: Vec<Application>,
    application_icons: BTreeMap<String, AppIcon>,
    desktop_icons: BTreeMap<PathBuf, AppIcon>,
    application_watch_roots: Vec<PathBuf>,
    application_watcher: DirectoryWatcher,
    application_rescan_after: Option<Instant>,
    desktop_model: DesktopModel,
    desktop_hit_targets: Vec<HitTarget>,
    desktop_accessible_targets: Vec<HitTarget>,
    desktop_watcher: DirectoryWatcher,
    desktop_rescan_after: Option<Instant>,
    exact_toplevels: BTreeMap<u32, ExactToplevel>,
    sway_window_changes: Receiver<()>,
    repaint_request: Option<PathBuf>,
    repaint_generation: Option<String>,
    full_repaint_after: Option<Instant>,
    palette: ThemePalette,
    desktop_background: [u8; 4],
    settings_tracker: SettingsTracker,
    config_home: PathBuf,
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
    DesktopAction {
        path: PathBuf,
        action: ShellAction,
    },
    ToggleDesktopSelection(PathBuf),
    EditValue,
    ApplicationSearch,
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
                    eprintln!("buzzardos-shell: reading shell-control socket failed: {error}");
                    break;
                }
            }
        }
        for identifier in requests {
            self.show_titlebar_window_menu(&identifier);
        }
    }

    fn poll(&mut self) {
        self.poll_control_socket();
        if self
            .window_menu_pointer_refresh_at
            .is_some_and(|deadline| self.window_menu_pending_pointer && Instant::now() >= deadline)
        {
            self.window_menu_pointer_refresh_at = None;
            if let Err(error) = sway_ipc::refresh_cursor_focus() {
                eprintln!("buzzardos-shell: refreshing titlebar menu pointer focus: {error:#}");
                self.hide_menu();
            }
        }
        if let Some(settings) = self.settings_tracker.candidate() {
            match apply_runtime_theme(&self.config_home, settings.appearance.theme) {
                Ok(()) => {
                    self.palette = *settings.appearance.theme.palette();
                    self.desktop_background = settings.appearance.background.solid_color().rgba();
                    self.capped_task_buttons = settings.appearance.capped_task_buttons;
                    self.pinned_applications = settings
                        .appearance
                        .pinned_applications
                        .iter()
                        .cloned()
                        .collect();
                    self.task_offset = self.task_offset.min(taskbar_max_offset(
                        self.panel_size.0,
                        self.windows().len(),
                        self.capped_task_buttons,
                    ));
                    let generation = settings.generation;
                    self.settings_tracker.commit(settings);
                    self.dirty = true;
                    eprintln!(
                        "buzzardos-shell: applied appearance settings generation {generation}"
                    );
                }
                Err(error) => self.settings_tracker.reject(format!(
                    "appearance settings generation {} was not applied: {error:#}",
                    settings.generation
                )),
            }
        }
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
            // Redraw the complete shell once after resize events settle;
            // later generations coalesce into this same bounded debounce.
            self.full_repaint_after = Some(Instant::now() + OUTPUT_SETTLE_DEBOUNCE);
        }
        if self
            .full_repaint_after
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.full_repaint_after = None;
            self.dirty = true;
        }
        match self.application_watcher.changed() {
            Ok(true) => self.application_rescan_after = Some(Instant::now() + FILE_MODEL_DEBOUNCE),
            Ok(false) => {}
            Err(error) => {
                eprintln!("buzzardos-shell: application watch failed: {error:#}");
                self.application_rescan_after = Some(Instant::now());
            }
        }
        if self
            .application_rescan_after
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.application_rescan_after = None;
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
            match DirectoryWatcher::new(&self.application_watch_roots) {
                Ok(watcher) => self.application_watcher = watcher,
                Err(error) => {
                    eprintln!("buzzardos-shell: rearming application watch failed: {error:#}")
                }
            }
        }
        match self.desktop_watcher.changed() {
            Ok(true) => self.desktop_rescan_after = Some(Instant::now() + FILE_MODEL_DEBOUNCE),
            Ok(false) => {}
            Err(error) => {
                eprintln!("buzzardos-shell: desktop watch failed: {error:#}");
                self.desktop_rescan_after = Some(Instant::now());
            }
        }
        if self
            .desktop_rescan_after
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.desktop_rescan_after = None;
            match self.desktop_model.rescan() {
                Ok(changed) => {
                    if changed {
                        if let Err(error) = self.rebuild_desktop_targets() {
                            eprintln!("buzzardos-shell: rebuilding desktop failed: {error:#}");
                        }
                        self.dirty = true;
                    }
                }
                Err(error) => eprintln!("buzzardos-shell: desktop rescan failed: {error:#}"),
            }
            match DirectoryWatcher::new(&[self.desktop_model.directory_path().to_path_buf()]) {
                Ok(watcher) => self.desktop_watcher = watcher,
                Err(error) => {
                    eprintln!("buzzardos-shell: rearming desktop watch failed: {error:#}")
                }
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
                (Action::Click, AccessibleTarget::DesktopAction { path, action }) => {
                    self.select_only(path);
                    self.activate(action);
                }
                (Action::Click, AccessibleTarget::ToggleDesktopSelection(path)) => {
                    if !self.desktop_selection.remove(&path) {
                        self.desktop_selection.insert(path.clone());
                    }
                    self.desktop_selection_anchor = Some(path);
                    self.dirty = true;
                }
                (Action::SetValue | Action::ReplaceSelectedText, AccessibleTarget::EditValue) => {
                    if let Some(ActionData::Value(value)) = request.data
                        && let ContextState::Edit(dialog) = &mut self.context_state
                        && !value.chars().any(char::is_control)
                    {
                        dialog.input = value.into();
                        dialog.replace_on_type = false;
                        dialog.error = None;
                        self.dirty = true;
                    }
                }
                (
                    Action::SetValue | Action::ReplaceSelectedText,
                    AccessibleTarget::ApplicationSearch,
                ) => {
                    if let Some(ActionData::Value(value)) = request.data
                        && !value.chars().any(char::is_control)
                    {
                        self.application_search = value.into();
                        self.menu_scroll = 0;
                        self.apply_applications_menu_geometry();
                        self.dirty = true;
                    }
                }
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

    fn rebuild_desktop_targets(&mut self) -> Result<()> {
        self.desktop_icons.clear();
        let mut visual = builtin_desktop_targets();
        let mut accessible = visual.clone();
        for positioned in self.desktop_model.positioned(self.desktop_size)? {
            if positioned.item.kind == DesktopItemKind::Launcher
                && let Some(file_name) = positioned.item.path.file_name().and_then(OsStr::to_str)
                && let Some(icon_name) = self
                    .applications
                    .iter()
                    .find(|application| application.id == file_name)
                    .and_then(|application| application.icon.as_deref())
                && let Some(icon) = load_icon(icon_name)
            {
                self.desktop_icons
                    .insert(positioned.item.path.clone(), icon);
            }
            let target = HitTarget {
                rect: positioned.rect,
                label: positioned.item.display_name,
                action: ShellAction::OpenDesktopItem(positioned.item.path, positioned.item.kind),
            };
            accessible.push(target.clone());
            if positioned.page == self.desktop_model.page() {
                visual.push(target);
            }
        }
        self.desktop_hit_targets = visual;
        self.desktop_accessible_targets = accessible;
        let live = self
            .desktop_accessible_targets
            .iter()
            .filter_map(|target| match &target.action {
                ShellAction::OpenDesktopItem(path, _) => Some(path.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        self.desktop_selection.retain(|path| live.contains(path));
        self.set_desktop_input_region()?;
        Ok(())
    }

    fn set_desktop_input_region(&self) -> Result<()> {
        let region = Region::new(&self.compositor).context("creating desktop icon input region")?;
        // The background layer remains below every application, so accepting
        // input across it does not steal events from client windows. It does
        // make empty-desktop context menus and rubber-band selection possible.
        region.add(
            0,
            0,
            i32::try_from(self.desktop_size.0).unwrap_or(i32::MAX),
            i32::try_from(self.desktop_size.1).unwrap_or(i32::MAX),
        );
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

    fn context_targets(&self) -> Vec<HitTarget> {
        let rows = |entries: Vec<(&str, ShellAction)>| {
            entries
                .into_iter()
                .enumerate()
                .map(|(index, (label, action))| HitTarget {
                    rect: Rect {
                        x: 6,
                        y: 6 + i32::try_from(index).unwrap_or_default() * MENU_ROW_HEIGHT,
                        width: i32::try_from(self.context_size.0)
                            .unwrap_or(i32::MAX)
                            .saturating_sub(12),
                        height: MENU_ROW_HEIGHT,
                    },
                    label: label.to_owned(),
                    action,
                })
                .collect()
        };
        match &self.context_state {
            ContextState::Application(id) => self
                .applications
                .iter()
                .find(|application| &application.id == id)
                .map(|application| {
                    application_context_targets(
                        application,
                        self.pinned_applications.contains(&application.id),
                        managed_appimage_registration_id(application).is_some(),
                    )
                })
                .unwrap_or_default(),
            ContextState::DesktopMenu => rows(vec![
                ("Paste", ShellAction::DesktopPaste),
                ("New Folder", ShellAction::DesktopNewFolder),
                ("Arrange Icons", ShellAction::DesktopArrangeIcons),
            ]),
            ContextState::ItemMenu {
                appimage_registered,
            } => {
                let mut entries = vec![
                    ("Open", ShellAction::DesktopOpenSelection),
                    ("Cut", ShellAction::DesktopCut),
                    ("Copy", ShellAction::DesktopCopy),
                ];
                if self.desktop_selection.len() == 1 {
                    entries.push(("Rename", ShellAction::DesktopRename));
                }
                entries.push(("Delete", ShellAction::DesktopDelete));
                if matches!(appimage_registered, Some(false)) {
                    entries.push((
                        "Add AppImage to Applications",
                        ShellAction::DesktopAddToApplications,
                    ));
                }
                rows(entries)
            }
            ContextState::Edit(dialog) => vec![
                HitTarget {
                    rect: Rect {
                        x: 220,
                        y: 136,
                        width: 96,
                        height: 38,
                    },
                    label: match dialog.operation {
                        EditOperation::NewFolder => "Create",
                        EditOperation::Rename(_) | EditOperation::RenameApplication(_) => "Rename",
                    }
                    .to_owned(),
                    action: ShellAction::DesktopEditConfirm,
                },
                HitTarget {
                    rect: Rect {
                        x: 322,
                        y: 136,
                        width: 96,
                        height: 38,
                    },
                    label: "Cancel".to_owned(),
                    action: ShellAction::DismissContext,
                },
            ],
            ContextState::Delete(_) => vec![
                HitTarget {
                    rect: Rect {
                        x: 220,
                        y: 136,
                        width: 96,
                        height: 38,
                    },
                    label: "Delete".to_owned(),
                    action: ShellAction::DesktopDeleteConfirm,
                },
                HitTarget {
                    rect: Rect {
                        x: 322,
                        y: 136,
                        width: 96,
                        height: 38,
                    },
                    label: "Cancel".to_owned(),
                    action: ShellAction::DismissContext,
                },
            ],
            ContextState::Collision(_) => vec![
                HitTarget {
                    rect: Rect {
                        x: 110,
                        y: 136,
                        width: 96,
                        height: 38,
                    },
                    label: "Replace".to_owned(),
                    action: ShellAction::DesktopCollisionReplace,
                },
                HitTarget {
                    rect: Rect {
                        x: 212,
                        y: 136,
                        width: 104,
                        height: 38,
                    },
                    label: "Keep Both".to_owned(),
                    action: ShellAction::DesktopCollisionKeepBoth,
                },
                HitTarget {
                    rect: Rect {
                        x: 322,
                        y: 136,
                        width: 96,
                        height: 38,
                    },
                    label: "Cancel".to_owned(),
                    action: ShellAction::DesktopCollisionCancel,
                },
            ],
            ContextState::Error { .. } => vec![HitTarget {
                rect: Rect {
                    x: 322,
                    y: 136,
                    width: 96,
                    height: 38,
                },
                label: "Close".to_owned(),
                action: ShellAction::DismissContext,
            }],
            ContextState::Hidden => Vec::new(),
        }
    }

    fn set_context_input_region(&self) -> Result<()> {
        let region = Region::new(&self.compositor).context("creating context input region")?;
        if self.context_state.is_visible() {
            region.add(
                0,
                0,
                i32::try_from(self.context_size.0).unwrap_or(i32::MAX),
                i32::try_from(self.context_size.1).unwrap_or(i32::MAX),
            );
        }
        self.context.set_input_region(Some(region.wl_region()));
        self.context.commit();
        Ok(())
    }

    fn show_application_context(&mut self, id: String, local_x: f64, local_y: f64) {
        let rows = self
            .applications
            .iter()
            .find(|application| application.id == id)
            .map_or(3, |application| {
                if managed_appimage_registration_id(application).is_some() {
                    7
                } else {
                    3
                }
            });
        let height = 12 + rows * MENU_ROW_HEIGHT as u32;
        let maximum_left = self
            .desktop_size
            .0
            .saturating_sub(APPLICATION_CONTEXT_WIDTH) as i32;
        let maximum_top = self
            .desktop_size
            .1
            .saturating_sub(PANEL_HEIGHT as u32)
            .saturating_sub(height) as i32;
        let left = (self.menu_origin.0 + local_x.floor() as i32).clamp(0, maximum_left.max(0));
        let top = (self.menu_origin.1 + local_y.floor() as i32).clamp(0, maximum_top.max(0));
        self.show_context(
            ContextState::Application(id),
            (APPLICATION_CONTEXT_WIDTH, height),
            (left, top),
        );
    }

    fn show_desktop_context(&mut self, item: bool, x: f64, y: f64) {
        if !item {
            self.paste_available = clipboard_has_supported_contents();
        }
        let appimage_registered = item
            .then(|| self.single_selected_path())
            .flatten()
            .filter(|path| self.selected_item_kind(path) == Some(DesktopItemKind::AppImage))
            .filter(|path| buzzardos_shortcut_helper::validate_appimage(path).is_ok())
            .map(|path| {
                RegistrationStore::discover()
                    .and_then(|store| store.find_by_target(&path))
                    .ok()
                    .flatten()
                    .is_some_and(|registration| registration.applications_launcher)
            });
        let rows = if item {
            4 + usize::from(self.desktop_selection.len() == 1)
                + usize::from(matches!(appimage_registered, Some(false)))
        } else {
            3
        };
        let size = (
            DESKTOP_CONTEXT_WIDTH,
            12 + u32::try_from(rows).unwrap_or(u32::MAX) * MENU_ROW_HEIGHT as u32,
        );
        let maximum_left = self.desktop_size.0.saturating_sub(size.0) as i32;
        let maximum_top = self
            .desktop_size
            .1
            .saturating_sub(PANEL_HEIGHT as u32)
            .saturating_sub(size.1) as i32;
        let origin = (
            (x.floor() as i32).clamp(0, maximum_left.max(0)),
            (y.floor() as i32).clamp(0, maximum_top.max(0)),
        );
        self.show_context(
            if item {
                ContextState::ItemMenu {
                    appimage_registered,
                }
            } else {
                ContextState::DesktopMenu
            },
            size,
            origin,
        );
    }

    fn show_dialog(&mut self, state: ContextState) {
        let size = (DESKTOP_DIALOG_WIDTH, DESKTOP_DIALOG_HEIGHT);
        let origin = (
            i32::try_from(self.desktop_size.0.saturating_sub(size.0) / 2).unwrap_or_default(),
            i32::try_from(
                self.desktop_size
                    .1
                    .saturating_sub(PANEL_HEIGHT as u32)
                    .saturating_sub(size.1)
                    / 2,
            )
            .unwrap_or_default(),
        );
        self.show_context(state, size, origin);
    }

    fn show_context(&mut self, state: ContextState, size: (u32, u32), origin: (i32, i32)) {
        self.context_state = state;
        self.context_size = size;
        self.context_origin = origin;
        self.context.set_anchor(Anchor::TOP | Anchor::LEFT);
        self.context.set_margin(origin.1, 0, 0, origin.0);
        self.context.set_size(size.0, size.1);
        self.context
            .set_keyboard_interactivity(if self.context_state.is_dialog() {
                KeyboardInteractivity::Exclusive
            } else {
                KeyboardInteractivity::OnDemand
            });
        let _ = self.set_context_input_region();
        self.context.commit();
        self.dirty = true;
    }

    fn hide_context(&mut self) {
        if self.context_state.is_visible() {
            self.context_state = ContextState::Hidden;
            self.context_size = (1, 1);
            self.context.set_size(1, 1);
            self.context
                .set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
            let _ = self.set_context_input_region();
            self.context.commit();
            self.dirty = true;
        }
    }

    fn toggle_menu(&mut self) {
        if self.menu_open && self.menu_kind == MenuKind::Applications {
            self.hide_menu();
            return;
        }
        self.menu_open = true;
        self.menu_kind = MenuKind::Applications;
        self.window_menu_pending_pointer = false;
        self.window_menu_pointer_refresh_at = None;
        self.menu_scroll = 0;
        self.menu
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        // Update hit-testing before the configure round-trip. Reusing the
        // former 1x1 hidden extent makes a freshly opened menu visible but
        // temporarily unclickable, so a quick human/CUA click reaches the
        // application below it.
        self.apply_applications_menu_geometry();
        self.dirty = true;
    }

    fn apply_applications_menu_geometry(&mut self) {
        let content_size = self.preferred_menu_size();
        self.menu_size = self.menu_overlay_size();
        self.menu_origin = (
            0,
            i32::try_from(self.menu_size.1.saturating_sub(content_size.1)).unwrap_or_default(),
        );
        self.apply_menu_overlay_geometry();
    }

    fn menu_overlay_size(&self) -> (u32, u32) {
        (
            self.desktop_size.0.max(1),
            self.desktop_size
                .1
                .saturating_sub(PANEL_HEIGHT as u32)
                .max(1),
        )
    }

    fn apply_menu_overlay_geometry(&self) {
        // The transparent surface receives input only while a menu is open.
        // It provides both click-away dismissal and, for titlebar menus, the
        // normal Wayland pointer-enter position after the titlebar binding
        // opens it. No global pointer monitor or click-history file is used.
        // This is the reliable Wayland implementation of click-away dismissal
        // because the shell cannot observe events delivered to another client.
        self.menu
            .set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        self.menu.set_margin(0, 0, PANEL_HEIGHT, 0);
        self.menu.set_size(0, 0);
        let _ = self.set_menu_input_region();
        self.menu.commit();
    }

    fn hide_menu(&mut self) {
        self.hide_context();
        if self.menu_open {
            if self.menu_kind == MenuKind::Applications {
                self.application_search.clear();
                self.menu_scroll = 0;
            }
            self.menu_open = false;
            self.window_menu_pending_pointer = false;
            self.window_menu_pointer_refresh_at = None;
            self.menu_size = (1, 1);
            self.menu.set_anchor(Anchor::BOTTOM | Anchor::LEFT);
            self.menu.set_margin(0, 0, PANEL_HEIGHT, 0);
            self.menu.set_size(1, 1);
            self.menu
                .set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
            let _ = self.set_menu_input_region();
            self.menu.commit();
            self.dirty = true;
        }
    }

    fn open_window_menu(&mut self, id: u32, origin: (i32, i32), await_pointer: bool) {
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
        self.window_menu_pending_pointer = await_pointer;
        self.window_menu_pointer_refresh_at =
            await_pointer.then(|| Instant::now() + WINDOW_MENU_POINTER_REFRESH_DELAY);
        self.menu_scroll = 0;
        self.menu_size = self.menu_overlay_size();
        self.menu_origin = origin;
        self.menu
            .set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        self.apply_menu_overlay_geometry();
        self.dirty = true;
        eprintln!(
            "buzzardos-shell: opened controls for {} ({})",
            title, identifier
        );
    }

    fn position_pending_window_menu(&mut self, pointer_x: f64) {
        if !self.window_menu_pending_pointer {
            return;
        }
        let MenuKind::Window(id) = self.menu_kind else {
            self.window_menu_pending_pointer = false;
            self.window_menu_pointer_refresh_at = None;
            return;
        };
        let Some(identifier) = self
            .exact_toplevels
            .get(&id)
            .map(|toplevel| toplevel.identifier.clone())
        else {
            self.hide_menu();
            return;
        };
        let Ok(state) = sway_ipc::window(&identifier) else {
            self.hide_menu();
            return;
        };
        self.menu_origin = titlebar_menu_origin(
            state.rect,
            state.decoration_height,
            self.desktop_size,
            pointer_x,
        );
        self.window_menu_pending_pointer = false;
        self.window_menu_pointer_refresh_at = None;
        self.dirty = true;
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

    fn show_titlebar_window_menu(&mut self, identifier: &str) {
        self.refresh_window_states();
        let Some(id) = self
            .exact_toplevels
            .iter()
            .find_map(|(id, toplevel)| (toplevel.identifier == identifier).then_some(*id))
        else {
            return;
        };
        // The full-output surface receives a standard pointer-enter event at
        // the current titlebar click position. Keep its contents transparent
        // until that event supplies the horizontal anchor.
        self.open_window_menu(id, (0, 0), true);
    }

    fn preferred_menu_size(&self) -> (u32, u32) {
        let applications = self.filtered_applications();
        (
            applications_menu_width(self.font.as_ref(), &applications, self.desktop_size.0),
            applications_menu_height(applications.len(), self.desktop_size.1),
        )
    }

    fn filtered_applications(&self) -> Vec<Application> {
        let query = self.application_search.trim().to_lowercase();
        let (mut regular, pinned): (Vec<_>, Vec<_>) = self
            .applications
            .iter()
            .filter(|application| {
                query.is_empty()
                    || application.name.to_lowercase().contains(&query)
                    || application
                        .generic_name
                        .as_deref()
                        .is_some_and(|name| name.to_lowercase().contains(&query))
                    || application
                        .categories
                        .iter()
                        .any(|category| category.to_lowercase().contains(&query))
            })
            .cloned()
            .partition(|application| !self.pinned_applications.contains(&application.id));
        regular.extend(pinned);
        regular
    }

    fn set_application_pinned(&mut self, id: &str, pinned: bool) -> Result<()> {
        anyhow::ensure!(
            self.applications
                .iter()
                .any(|application| application.id == id),
            "application no longer exists"
        );
        let mut settings = load_settings(&self.settings_tracker.path)?;
        let already_pinned = settings
            .appearance
            .pinned_applications
            .iter()
            .any(|candidate| candidate == id);
        if already_pinned == pinned {
            return Ok(());
        }
        if pinned {
            settings.appearance.pinned_applications.push(id.to_owned());
        } else {
            settings
                .appearance
                .pinned_applications
                .retain(|candidate| candidate != id);
        }
        settings.generation = settings
            .generation
            .checked_add(1)
            .context("settings generation overflow")?;
        settings
            .save(&self.settings_tracker.path)
            .context("saving pinned Applications entries")?;
        if pinned {
            self.pinned_applications.insert(id.to_owned());
        } else {
            self.pinned_applications.remove(id);
        }
        self.menu_scroll = 0;
        self.dirty = true;
        Ok(())
    }

    fn visible_menu_rows(&self) -> usize {
        let menu_height = if self.menu_open && self.menu_kind == MenuKind::Applications {
            self.preferred_menu_size().1
        } else {
            self.menu_size.1
        };
        let used = APPLICATIONS_MENU_HEADER_HEIGHT
            + APPLICATIONS_MENU_SECTION_HEIGHT
            + APPLICATIONS_MENU_FOOTER_HEIGHT;
        usize::try_from(
            (i32::try_from(menu_height)
                .unwrap_or(i32::MAX)
                .saturating_sub(used)
                / MENU_ROW_HEIGHT)
                .max(0),
        )
        .unwrap_or_default()
    }

    fn clamp_menu_scroll(&mut self) {
        let application_count = self.filtered_applications().len();
        self.menu_scroll = self
            .menu_scroll
            .min(application_count.saturating_sub(self.visible_menu_rows()));
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        self.desktop_accessible_targets
            .iter()
            .filter_map(|target| match &target.action {
                ShellAction::OpenDesktopItem(path, _) if self.desktop_selection.contains(path) => {
                    Some(path.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn single_selected_path(&self) -> Option<PathBuf> {
        (self.desktop_selection.len() == 1)
            .then(|| self.desktop_selection.iter().next().cloned())
            .flatten()
    }

    fn selected_item_kind(&self, path: &Path) -> Option<DesktopItemKind> {
        self.desktop_accessible_targets
            .iter()
            .find_map(|target| match &target.action {
                ShellAction::OpenDesktopItem(candidate, kind) if candidate == path => Some(*kind),
                _ => None,
            })
    }

    fn select_only(&mut self, path: PathBuf) {
        self.desktop_selection.clear();
        self.desktop_selection.insert(path.clone());
        self.desktop_selection_anchor = Some(path);
        self.dirty = true;
    }

    fn select_desktop_item(&mut self, path: PathBuf) {
        if self.modifiers.shift {
            let ordered = self
                .desktop_accessible_targets
                .iter()
                .filter_map(|target| match &target.action {
                    ShellAction::OpenDesktopItem(candidate, _) => Some(candidate.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let anchor = self
                .desktop_selection_anchor
                .as_ref()
                .and_then(|anchor| ordered.iter().position(|candidate| candidate == anchor));
            let current = ordered.iter().position(|candidate| candidate == &path);
            if let (Some(anchor), Some(current)) = (anchor, current) {
                if !self.modifiers.ctrl {
                    self.desktop_selection.clear();
                }
                for candidate in &ordered[anchor.min(current)..=anchor.max(current)] {
                    self.desktop_selection.insert(candidate.clone());
                }
            } else {
                self.select_only(path);
                return;
            }
        } else if self.modifiers.ctrl {
            if !self.desktop_selection.remove(&path) {
                self.desktop_selection.insert(path.clone());
            }
            self.desktop_selection_anchor = Some(path);
        } else {
            self.select_only(path);
            return;
        }
        self.dirty = true;
    }

    fn open_selection(&mut self) {
        for path in self.selected_paths() {
            if let Some(kind) = self.selected_item_kind(&path)
                && let Err(error) = open_desktop_item(&path, kind)
            {
                self.show_operation_error("Could not open item", error);
                return;
            }
        }
        self.hide_context();
    }

    fn copy_selection_to_clipboard(&mut self, operation: ClipboardOperation) {
        let sources = self.selected_paths();
        if sources.is_empty() {
            return;
        }
        self.clipboard_generation = self.clipboard_generation.saturating_add(1);
        let token = ClipboardToken {
            schema: 1,
            generation: self.clipboard_generation,
            operation,
        };
        let result = (|| -> Result<()> {
            let token = serde_json::to_vec(&token).context("encoding desktop clipboard token")?;
            let mut uri_list = String::new();
            for path in &sources {
                uri_list.push_str(&gio::File::for_path(path).uri());
                uri_list.push_str("\r\n");
            }
            clipboard_copy::Options::new()
                .copy_multi(vec![
                    clipboard_copy::MimeSource {
                        source: clipboard_copy::Source::Bytes(token.into_boxed_slice()),
                        mime_type: clipboard_copy::MimeType::Specific(
                            DESKTOP_CLIPBOARD_MIME.to_owned(),
                        ),
                    },
                    clipboard_copy::MimeSource {
                        source: clipboard_copy::Source::Bytes(
                            uri_list.into_bytes().into_boxed_slice(),
                        ),
                        mime_type: clipboard_copy::MimeType::Specific(URI_LIST_MIME.to_owned()),
                    },
                ])
                .context("publishing the private guest clipboard")?;
            Ok(())
        })();
        if let Err(error) = result {
            self.show_operation_error("Could not update clipboard", error);
            return;
        }
        self.guest_clipboard = Some(GuestClipboardRecord {
            generation: self.clipboard_generation,
            operation,
            sources,
        });
        self.paste_available = true;
        self.hide_context();
    }

    fn start_paste(&mut self) {
        match self.paste_session_from_clipboard() {
            Ok(Some(session)) => self.continue_paste(session, None),
            Ok(None) => self.show_operation_error(
                "Nothing to paste",
                anyhow::anyhow!("the guest clipboard does not contain a local file URI list"),
            ),
            Err(error) => self.show_operation_error("Could not read clipboard", error),
        }
    }

    fn paste_session_from_clipboard(&self) -> Result<Option<PasteSession>> {
        let mime_types = clipboard_paste::get_mime_types(
            clipboard_paste::ClipboardType::Regular,
            clipboard_paste::Seat::Unspecified,
        )
        .context("reading clipboard MIME types")?;
        if mime_types.contains(DESKTOP_CLIPBOARD_MIME) {
            let bytes = read_clipboard_mime(DESKTOP_CLIPBOARD_MIME)?;
            let token: ClipboardToken =
                serde_json::from_slice(&bytes).context("decoding desktop clipboard token")?;
            if token.schema != 1 {
                anyhow::bail!("unsupported desktop clipboard token schema");
            }
            if let Some(record) = self.guest_clipboard.as_ref()
                && record.generation == token.generation
                && record.operation == token.operation
            {
                return Ok(Some(PasteSession {
                    operation: record.operation,
                    sources: record.sources.clone(),
                    index: 0,
                }));
            }
        }
        if !mime_types.contains(URI_LIST_MIME) {
            return Ok(None);
        }
        let sources = parse_uri_list(&read_clipboard_mime(URI_LIST_MIME)?)?;
        Ok((!sources.is_empty()).then_some(PasteSession {
            operation: ClipboardOperation::Copy,
            sources,
            index: 0,
        }))
    }

    fn continue_paste(&mut self, mut session: PasteSession, mut choice: Option<CollisionChoice>) {
        let destination_path = self.desktop_model.directory_path().to_path_buf();
        let destination = match DesktopDirectory::open(&destination_path) {
            Ok(directory) => directory,
            Err(error) => {
                self.show_operation_error("Could not open Desktop", error.into());
                return;
            }
        };
        while session.index < session.sources.len() {
            let source_path = session.sources[session.index].clone();
            let Some(source_name) = source_path.file_name().map(OsStr::to_owned) else {
                self.show_operation_error(
                    "Could not paste item",
                    anyhow::anyhow!("source has no file name: {}", source_path.display()),
                );
                return;
            };
            let Some(source_parent) = source_path.parent() else {
                self.show_operation_error(
                    "Could not paste item",
                    anyhow::anyhow!("source has no parent directory: {}", source_path.display()),
                );
                return;
            };
            if session.operation == ClipboardOperation::Cut && source_parent == destination_path {
                session.index += 1;
                continue;
            }
            let collision = fs::symlink_metadata(destination_path.join(&source_name)).is_ok();
            let resolution = if collision {
                match choice {
                    Some(resolution) => resolution,
                    None => {
                        self.show_dialog(ContextState::Collision(session));
                        return;
                    }
                }
            } else {
                CollisionChoice::Cancel
            };
            let source = match DesktopDirectory::open(source_parent) {
                Ok(directory) => directory,
                Err(error) => {
                    self.show_operation_error("Could not open source folder", error.into());
                    return;
                }
            };
            let transfer = match session.operation {
                ClipboardOperation::Copy => destination.copy_from(
                    &source,
                    &source_name,
                    &source_name,
                    if collision {
                        resolution
                    } else {
                        CollisionChoice::Cancel
                    },
                ),
                ClipboardOperation::Cut => destination.move_from(
                    &source,
                    &source_name,
                    &source_name,
                    if collision {
                        resolution
                    } else {
                        CollisionChoice::Cancel
                    },
                ),
            };
            if let Err(error) = transfer {
                self.show_operation_error(
                    "Could not paste item",
                    anyhow::anyhow!("{}: {error}", source_path.display()),
                );
                let _ = self.refresh_desktop_items();
                return;
            }
            session.index += 1;
            // A collision choice applies to exactly one item; another
            // collision must always ask again.
            choice = None;
        }
        if session.operation == ClipboardOperation::Cut {
            self.guest_clipboard = None;
            self.paste_available = false;
            if let Err(error) = clipboard_copy::clear(
                clipboard_copy::ClipboardType::Regular,
                clipboard_copy::Seat::All,
            ) {
                eprintln!("buzzardos-shell: clearing completed cut clipboard failed: {error}");
            }
        }
        let _ = self.refresh_desktop_items();
        self.hide_context();
    }

    fn begin_new_folder(&mut self) {
        self.show_dialog(ContextState::Edit(EditDialog {
            operation: EditOperation::NewFolder,
            input: "New Folder".to_owned(),
            replace_on_type: true,
            error: None,
        }));
    }

    fn begin_rename(&mut self) {
        let Some(path) = self.single_selected_path() else {
            return;
        };
        let input = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.show_dialog(ContextState::Edit(EditDialog {
            operation: EditOperation::Rename(path),
            input,
            replace_on_type: true,
            error: None,
        }));
    }

    fn begin_application_rename(&mut self, application_id: &str) {
        let Some(application) = self
            .applications
            .iter()
            .find(|application| application.id == application_id)
        else {
            return;
        };
        let Some(registration_id) = managed_appimage_registration_id(application) else {
            return;
        };
        let input = application.name.clone();
        self.show_dialog(ContextState::Edit(EditDialog {
            operation: EditOperation::RenameApplication(registration_id),
            input,
            replace_on_type: true,
            error: None,
        }));
    }

    fn commit_edit(&mut self) {
        let ContextState::Edit(dialog) = self.context_state.clone() else {
            return;
        };
        // Unix Desktop basenames may intentionally contain leading or
        // trailing spaces. Preserve the user's exact edit; descriptor-bound
        // Desktop operations provide the authoritative empty/dot/slash/NUL
        // validation.
        let name = dialog.input.as_str();
        let renaming_application = matches!(&dialog.operation, EditOperation::RenameApplication(_));
        let result = (|| -> Result<Option<PathBuf>> {
            match dialog.operation {
                EditOperation::NewFolder => {
                    let desktop = DesktopDirectory::open(self.desktop_model.directory_path())?;
                    let created = desktop.create_folder(OsStr::new(name))?;
                    Ok(Some(desktop.path().join(created)))
                }
                EditOperation::Rename(path) => {
                    let old = path.file_name().context("desktop item has no name")?;
                    // One helper transaction owns both the descriptor-bound
                    // Desktop rename and a registered AppImage's target-path
                    // update. Never recreate the former rename/relink/
                    // best-effort-rollback sequence here: it is not recoverable
                    // after process termination or power loss.
                    let renamed = RegistrationStore::discover()?
                        .rename_desktop_item(old, OsStr::new(name))?;
                    Ok(Some(renamed))
                }
                EditOperation::RenameApplication(id) => {
                    RegistrationStore::discover()?.rename_application(id, name)?;
                    Ok(None)
                }
            }
        })();
        match result {
            Ok(selected) => {
                if renaming_application {
                    self.refresh_applications_now();
                }
                let _ = self.refresh_desktop_items();
                if let Some(selected) = selected {
                    self.select_only(selected);
                }
                self.hide_context();
            }
            Err(error) => {
                if let ContextState::Edit(dialog) = &mut self.context_state {
                    dialog.error = Some(format!("{error:#}"));
                    dialog.replace_on_type = false;
                }
                self.dirty = true;
            }
        }
    }

    fn begin_delete(&mut self) {
        let items = self.selected_paths();
        if items.is_empty() {
            return;
        }
        let consequences = (|| -> Result<Vec<DeleteConsequence>> {
            let desktop = DesktopDirectory::open(self.desktop_model.directory_path())?;
            items
                .iter()
                .map(|path| {
                    desktop
                        .consequence(path.file_name().context("desktop item has no name")?)
                        .map_err(Into::into)
                })
                .collect()
        })();
        let Ok(consequences) = consequences else {
            self.show_operation_error(
                "Could not inspect selected items",
                consequences.unwrap_err(),
            );
            return;
        };
        let detail = delete_dialog_detail_from_consequences(&items, &consequences);
        self.show_dialog(ContextState::Delete(DeleteDialog {
            items,
            consequences,
            detail,
            error: None,
        }));
    }

    fn confirm_delete(&mut self) {
        let ContextState::Delete(dialog) = self.context_state.clone() else {
            return;
        };
        let result = (|| -> Result<()> {
            let desktop = DesktopDirectory::open(self.desktop_model.directory_path())?;
            for (path, expected) in dialog.items.iter().zip(&dialog.consequences) {
                let name = path.file_name().context("desktop item has no name")?;
                // Recompute immediately before each confirmed operation so a
                // changed item cannot inherit stale confirmation text.
                let current = desktop.consequence(name)?;
                if &current != expected {
                    anyhow::bail!(
                        "{} changed after confirmation; nothing further was deleted",
                        path.display()
                    );
                }
                desktop.delete_confirmed(name)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.desktop_selection.clear();
                let _ = self.refresh_desktop_items();
                self.hide_context();
            }
            Err(error) => {
                if let ContextState::Delete(dialog) = &mut self.context_state {
                    dialog.error = Some(format!("{error:#}"));
                }
                let _ = self.refresh_desktop_items();
                self.dirty = true;
            }
        }
    }

    fn add_selected_appimage_to_applications(&mut self) {
        let Some(path) = self.single_selected_path() else {
            return;
        };
        let result = (|| -> Result<()> {
            let store = RegistrationStore::discover()?;
            if let Some(registration) = store.find_by_target(&path)? {
                store.add_applications(registration.id)?;
            } else {
                store.register(&path, RegistrationFlags::APPLICATIONS)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => self.hide_context(),
            Err(error) => self.show_operation_error("Could not update Applications", error),
        }
    }

    fn extract_registered_application(&mut self, application_id: &str, no_sandbox: bool) {
        let result = (|| -> Result<()> {
            let application = self
                .applications
                .iter()
                .find(|application| application.id == application_id)
                .context("application no longer exists")?;
            let id = managed_appimage_registration_id(application)
                .context("application is not a managed AppImage")?;
            let registration = RegistrationStore::discover()?.load(id)?;
            extract_and_launch(&registration.target_path, no_sandbox)?;
            Ok(())
        })();
        match result {
            Ok(()) => self.hide_context(),
            Err(error) => self.show_operation_error("Could not extract and run AppImage", error),
        }
    }

    fn delete_application_registration(&mut self, application_id: &str) {
        let result = (|| -> Result<()> {
            let application = self
                .applications
                .iter()
                .find(|application| application.id == application_id)
                .context("application no longer exists")?;
            let id = managed_appimage_registration_id(application)
                .context("application is not a managed AppImage")?;
            RegistrationStore::discover()?.remove_applications(id)?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                if let Err(error) = self.set_application_pinned(application_id, false) {
                    eprintln!(
                        "buzzardos-shell: clearing removed application pin failed: {error:#}"
                    );
                }
                self.refresh_applications_now();
                self.hide_context();
            }
            Err(error) => self.show_operation_error("Could not delete from Applications", error),
        }
    }

    fn refresh_applications_now(&mut self) {
        if let Ok(applications) = scan_applications() {
            self.application_icons = load_application_icons(&applications);
            self.applications = applications;
            self.clamp_menu_scroll();
            if self.menu_open && self.menu_kind == MenuKind::Applications {
                self.apply_applications_menu_geometry();
            }
            self.dirty = true;
        }
    }

    fn refresh_desktop_items(&mut self) -> Result<()> {
        self.desktop_model.rescan()?;
        self.rebuild_desktop_targets()?;
        let live = self
            .desktop_accessible_targets
            .iter()
            .filter_map(|target| match &target.action {
                ShellAction::OpenDesktopItem(path, _) => Some(path.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        self.desktop_selection.retain(|path| live.contains(path));
        self.dirty = true;
        Ok(())
    }

    fn show_operation_error(&mut self, title: &str, error: anyhow::Error) {
        self.show_dialog(ContextState::Error {
            title: title.to_owned(),
            detail: format!("{error:#}"),
        });
    }

    fn handle_key(&mut self, event: KeyEvent) {
        if event.keysym == Keysym::Escape {
            if self.context_state.is_visible() {
                self.hide_context();
            } else if self.menu_open {
                self.hide_menu();
            } else if !self.desktop_selection.is_empty() {
                self.desktop_selection.clear();
                self.desktop_selection_anchor = None;
                self.dirty = true;
            }
            return;
        }
        if let ContextState::Edit(dialog) = &mut self.context_state {
            match event.keysym {
                Keysym::Return | Keysym::KP_Enter => self.commit_edit(),
                Keysym::BackSpace | Keysym::Delete => {
                    dialog.input.pop();
                    dialog.replace_on_type = false;
                    dialog.error = None;
                    self.dirty = true;
                }
                _ => {
                    if !self.modifiers.ctrl
                        && !self.modifiers.alt
                        && !self.modifiers.logo
                        && let Some(text) = event.utf8
                        && !text.chars().any(char::is_control)
                    {
                        if dialog.replace_on_type {
                            dialog.input.clear();
                            dialog.replace_on_type = false;
                        }
                        dialog.input.push_str(&text);
                        dialog.error = None;
                        self.dirty = true;
                    }
                }
            }
            return;
        }
        if self.context_state.is_dialog() {
            // Cancel is the safe default in destructive and collision
            // dialogs. An explicit pointer/AT-SPI action is required for
            // Delete, Replace, or Keep Both.
            if matches!(event.keysym, Keysym::Return | Keysym::KP_Enter) {
                self.hide_context();
            }
            return;
        }
        if self.menu_open
            && self.menu_kind == MenuKind::Applications
            && !self.context_state.is_visible()
        {
            match event.keysym {
                Keysym::BackSpace => {
                    self.application_search.pop();
                    self.menu_scroll = 0;
                    self.apply_applications_menu_geometry();
                    self.dirty = true;
                }
                Keysym::Return | Keysym::KP_Enter => {
                    if let Some(application) = self.filtered_applications().first() {
                        launch_application(application);
                        self.hide_menu();
                    }
                }
                _ => {
                    if !self.modifiers.ctrl
                        && !self.modifiers.alt
                        && !self.modifiers.logo
                        && let Some(text) = event.utf8
                        && !text.chars().any(char::is_control)
                    {
                        self.application_search.push_str(&text);
                        self.menu_scroll = 0;
                        self.apply_applications_menu_geometry();
                        self.dirty = true;
                    }
                }
            }
            return;
        }
        if self.keyboard_focus != Some(ShellSurface::Desktop) {
            return;
        }
        if self.modifiers.ctrl {
            match event.keysym {
                Keysym::c | Keysym::C => self.copy_selection_to_clipboard(ClipboardOperation::Copy),
                Keysym::x | Keysym::X => self.copy_selection_to_clipboard(ClipboardOperation::Cut),
                Keysym::v | Keysym::V => self.start_paste(),
                _ => {}
            }
        } else {
            match event.keysym {
                Keysym::F2 => self.begin_rename(),
                Keysym::Delete => self.begin_delete(),
                Keysym::Return | Keysym::KP_Enter => self.open_selection(),
                _ => {}
            }
        }
    }

    fn activate(&mut self, action: ShellAction) {
        match action {
            ShellAction::ToggleApplications => self.toggle_menu(),
            ShellAction::OpenFiles => {
                if let Some(home) = std::env::var_os("HOME") {
                    spawn("thunar", [home]);
                } else {
                    eprintln!("buzzardos-shell: HOME is unavailable; Files was not opened");
                }
                self.hide_menu();
            }
            ShellAction::OpenShared => {
                spawn("thunar", ["/shared"]);
                self.hide_menu();
            }
            ShellAction::OpenDesktopItem(path, kind) => {
                if let Err(error) = open_desktop_item(&path, kind) {
                    eprintln!(
                        "buzzardos-shell: opening desktop item {} failed: {error:#}",
                        path.display()
                    );
                }
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
            ShellAction::AddApplicationDesktopShortcut(id) => {
                if let Some(application) = self
                    .applications
                    .iter()
                    .find(|application| application.id == id)
                    .cloned()
                    && let Err(error) = add_application_desktop_shortcut(&application)
                {
                    eprintln!("buzzardos-shell: add desktop shortcut failed: {error:#}");
                }
                let _ = self.desktop_model.rescan();
                self.desktop_model.show_first_page();
                let _ = self.rebuild_desktop_targets();
                self.hide_context();
            }
            ShellAction::ExtractApplication(id) => self.extract_registered_application(&id, false),
            ShellAction::ExtractApplicationNoSandbox(id) => {
                self.extract_registered_application(&id, true)
            }
            ShellAction::PinApplication(id) => {
                if let Err(error) = self.set_application_pinned(&id, true) {
                    self.show_operation_error("Could not pin application", error);
                } else {
                    self.hide_context();
                }
            }
            ShellAction::UnpinApplication(id) => {
                if let Err(error) = self.set_application_pinned(&id, false) {
                    self.show_operation_error("Could not unpin application", error);
                } else {
                    self.hide_context();
                }
            }
            ShellAction::RenameApplication(id) => self.begin_application_rename(&id),
            ShellAction::DeleteApplication(id) => self.delete_application_registration(&id),
            ShellAction::DesktopOpenSelection => self.open_selection(),
            ShellAction::DesktopCut => self.copy_selection_to_clipboard(ClipboardOperation::Cut),
            ShellAction::DesktopCopy => self.copy_selection_to_clipboard(ClipboardOperation::Copy),
            ShellAction::DesktopPaste => self.start_paste(),
            ShellAction::DesktopRename => self.begin_rename(),
            ShellAction::DesktopDelete => self.begin_delete(),
            ShellAction::DesktopNewFolder => self.begin_new_folder(),
            ShellAction::DesktopArrangeIcons => {
                if let Err(error) = self
                    .desktop_model
                    .arrange_icons()
                    .and_then(|()| self.rebuild_desktop_targets())
                {
                    self.show_operation_error("Could not arrange icons", error);
                } else {
                    self.hide_context();
                    self.dirty = true;
                }
            }
            ShellAction::DesktopAddToApplications => self.add_selected_appimage_to_applications(),
            ShellAction::DesktopEditConfirm => self.commit_edit(),
            ShellAction::DesktopDeleteConfirm => self.confirm_delete(),
            ShellAction::DesktopCollisionReplace => {
                if let ContextState::Collision(session) = self.context_state.clone() {
                    self.continue_paste(session, Some(CollisionChoice::Replace));
                }
            }
            ShellAction::DesktopCollisionKeepBoth => {
                if let ContextState::Collision(session) = self.context_state.clone() {
                    self.continue_paste(session, Some(CollisionChoice::KeepBoth));
                }
            }
            ShellAction::DesktopCollisionCancel | ShellAction::DismissContext => {
                self.hide_context()
            }
            ShellAction::ActivateWindow(id) => {
                if let Some(toplevel) = self.exact_toplevels.get(&id)
                    && let Err(error) = sway_ipc::focus(&toplevel.identifier)
                {
                    eprintln!("buzzardos-shell: focus failed: {error:#}");
                }
                self.hide_menu();
            }
            ShellAction::BringIntoViewWindow(id) => {
                if let Some(toplevel) = self.exact_toplevels.get(&id)
                    && let Err(error) = sway_ipc::bring_into_view(&toplevel.identifier)
                {
                    eprintln!("buzzardos-shell: bring into view failed: {error:#}");
                }
                self.hide_menu();
            }
            ShellAction::MinimizeWindow(id) => {
                if let Some(toplevel) = self.exact_toplevels.get(&id)
                    && let Err(error) = sway_ipc::minimize(&toplevel.identifier)
                {
                    eprintln!("buzzardos-shell: minimize failed: {error:#}");
                }
                self.hide_menu();
            }
            ShellAction::ToggleMaximizeWindow(id) => {
                if let Some(toplevel) = self.exact_toplevels.get(&id) {
                    if let Err(error) = sway_ipc::toggle_maximize(&toplevel.identifier) {
                        eprintln!("buzzardos-shell: maximize/restore failed: {error:#}");
                    }
                }
                self.hide_menu();
            }
            ShellAction::CloseWindow(id) => {
                if let Some(toplevel) = self.exact_toplevels.get(&id)
                    && let Err(error) = sway_ipc::close(&toplevel.identifier)
                {
                    eprintln!("buzzardos-shell: close failed: {error:#}");
                }
                self.hide_menu();
            }
            ShellAction::TaskbarPrevious => {
                self.task_offset = self.task_offset.saturating_sub(TASK_PAGE_STEP);
                self.dirty = true;
            }
            ShellAction::TaskbarNext => {
                self.task_offset =
                    self.task_offset
                        .saturating_add(TASK_PAGE_STEP)
                        .min(taskbar_max_offset(
                            self.panel_size.0,
                            self.windows().len(),
                            self.capped_task_buttons,
                        ));
                self.dirty = true;
            }
            ShellAction::ShowDesktop => {
                if let Err(error) = sway_ipc::minimize_all_visible() {
                    eprintln!("buzzardos-shell: show desktop failed: {error:#}");
                }
                self.hide_menu();
            }
            ShellAction::CloseApplicationsMenu => self.hide_menu(),
        }
    }

    fn target_at_surface(
        &self,
        surface: &wl_surface::WlSurface,
        x: f64,
        y: f64,
    ) -> Option<HitTarget> {
        if surface == self.panel.wl_surface() {
            panel_targets(
                self.panel_size.0,
                &self.windows(),
                self.task_offset,
                self.capped_task_buttons,
            )
            .into_iter()
            .find(|target| target.rect.contains(x, y))
        } else if surface == self.menu.wl_surface() && self.menu_open {
            match self.menu_kind {
                MenuKind::Applications => {
                    let applications = self.filtered_applications();
                    let content_size = self.preferred_menu_size();
                    let content_y = f64::from(
                        i32::try_from(self.menu_size.1.saturating_sub(content_size.1))
                            .unwrap_or_default(),
                    );
                    if x < 0.0
                        || y < content_y
                        || x >= f64::from(content_size.0)
                        || y >= content_y + f64::from(content_size.1)
                    {
                        return None;
                    }
                    let local_y = y - content_y;
                    std::iter::once(applications_menu_close_target(content_size.0))
                        .chain(menu_targets(
                            content_size.0,
                            content_size.1,
                            &applications,
                            self.menu_scroll,
                        ))
                        .find(|target| target.rect.contains(x, local_y))
                }
                MenuKind::Window(id) => {
                    if self.window_menu_pending_pointer {
                        return None;
                    }
                    let local_x = x - f64::from(self.menu_origin.0);
                    let local_y = y - f64::from(self.menu_origin.1);
                    if local_x < 0.0
                        || local_y < 0.0
                        || local_x >= f64::from(WINDOW_MENU_WIDTH)
                        || local_y >= f64::from(WINDOW_MENU_HEIGHT)
                    {
                        return None;
                    }
                    self.exact_toplevels.get(&id).and_then(|window| {
                        window_menu_targets(&window.window)
                            .into_iter()
                            .find(|target| target.rect.contains(local_x, local_y))
                    })
                }
            }
        } else if surface == self.desktop.wl_surface() {
            self.desktop_hit_targets
                .iter()
                .cloned()
                .into_iter()
                .find(|target| target.rect.contains(x, y))
        } else if surface == self.context.wl_surface() && self.context_state.is_visible() {
            self.context_targets()
                .into_iter()
                .find(|target| target.rect.contains(x, y))
                .filter(|target| {
                    !matches!(target.action, ShellAction::DesktopPaste) || self.paste_available
                })
        } else {
            None
        }
    }

    fn click_surface(&mut self, surface: &wl_surface::WlSurface, x: f64, y: f64) {
        let target = self.target_at_surface(surface, x, y);
        if let Some(target) = target {
            self.activate(target.action);
        } else if surface == self.menu.wl_surface() && self.menu_open {
            self.hide_menu();
        }
    }

    fn desktop_file_target_at(&self, x: f64, y: f64) -> Option<(PathBuf, DesktopItemKind)> {
        self.desktop_hit_targets
            .iter()
            .find(|target| target.rect.contains(x, y))
            .and_then(|target| match &target.action {
                ShellAction::OpenDesktopItem(path, kind) => Some((path.clone(), *kind)),
                _ => None,
            })
    }

    fn desktop_pointer_press(&mut self, x: f64, y: f64, time: u32) {
        self.hide_context();
        if let Some((path, _)) = self.desktop_file_target_at(x, y) {
            self.select_desktop_item(path.clone());
            self.desktop_pointer_gesture = Some(DesktopPointerGesture::Item {
                path,
                start: (x, y),
                current: (x, y),
                time,
            });
        } else if let Some(target) = self
            .desktop_hit_targets
            .iter()
            .find(|target| target.rect.contains(x, y))
            .cloned()
        {
            self.activate(target.action);
        } else {
            let base = if self.modifiers.ctrl {
                self.desktop_selection.clone()
            } else {
                self.desktop_selection.clear();
                BTreeSet::new()
            };
            self.desktop_selection_anchor = None;
            self.desktop_pointer_gesture = Some(DesktopPointerGesture::RubberBand {
                start: (x, y),
                current: (x, y),
                base,
            });
            self.dirty = true;
        }
    }

    fn desktop_pointer_motion(&mut self, x: f64, y: f64) {
        let Some(gesture) = &mut self.desktop_pointer_gesture else {
            return;
        };
        match gesture {
            DesktopPointerGesture::Item { current, .. } => *current = (x, y),
            DesktopPointerGesture::RubberBand {
                start,
                current,
                base,
            } => {
                *current = (x, y);
                let rubber = rect_between(*start, *current);
                self.desktop_selection = base.clone();
                for target in &self.desktop_hit_targets {
                    if rects_intersect(rubber, target.rect)
                        && let ShellAction::OpenDesktopItem(path, _) = &target.action
                    {
                        self.desktop_selection.insert(path.clone());
                    }
                }
            }
        }
        self.dirty = true;
    }

    fn desktop_pointer_release(&mut self, x: f64, y: f64) {
        let Some(gesture) = self.desktop_pointer_gesture.take() else {
            return;
        };
        if let DesktopPointerGesture::Item {
            path,
            start,
            current: _,
            time,
        } = gesture
        {
            let moved = distance(start, (x, y)) >= DRAG_THRESHOLD;
            if moved {
                match self
                    .desktop_model
                    .move_item(&path, (x, y), self.desktop_size)
                {
                    Ok(true) => {
                        if let Err(error) = self.rebuild_desktop_targets() {
                            self.show_operation_error("Could not move desktop icon", error);
                        }
                    }
                    Ok(false) => {}
                    Err(error) => self.show_operation_error("Could not move desktop icon", error),
                }
            } else {
                let double_click =
                    self.last_desktop_click
                        .as_ref()
                        .is_some_and(|(previous, previous_time)| {
                            previous == &path
                                && time.wrapping_sub(*previous_time) <= DOUBLE_CLICK_MILLIS
                        });
                if double_click {
                    self.last_desktop_click = None;
                    if let Some(kind) = self.selected_item_kind(&path)
                        && let Err(error) = open_desktop_item(&path, kind)
                    {
                        self.show_operation_error("Could not open item", error);
                    }
                } else {
                    self.last_desktop_click = Some((path, time));
                }
            }
        }
        self.dirty = true;
    }

    fn secondary_click_desktop(&mut self, x: f64, y: f64) {
        if let Some((path, _)) = self.desktop_file_target_at(x, y) {
            if !self.desktop_selection.contains(&path) {
                self.select_only(path);
            }
            self.show_desktop_context(true, x, y);
        } else {
            self.desktop_selection.clear();
            self.desktop_selection_anchor = None;
            self.show_desktop_context(false, x, y);
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
        let target = panel_targets(
            self.panel_size.0,
            &self.windows(),
            self.task_offset,
            self.capped_task_buttons,
        )
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

    fn secondary_click_applications_menu(&mut self, x: f64, y: f64) {
        let target = self.target_at_surface(self.menu.wl_surface(), x, y);
        if let Some(HitTarget {
            action: ShellAction::LaunchApplication(id),
            ..
        }) = target
        {
            self.show_application_context(id, x, y - f64::from(self.menu_origin.1));
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

    fn scroll_desktop(&mut self, amount: f64) {
        if self.desktop_model.scroll_page(amount) {
            if let Err(error) = self.rebuild_desktop_targets() {
                eprintln!("buzzardos-shell: changing desktop page failed: {error:#}");
            }
            self.dirty = true;
        }
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
        if self.context_configured {
            self.draw_context()?;
        }
        self.update_accessibility();
        Ok(())
    }

    fn draw_desktop(&mut self) -> Result<()> {
        let theme = self.palette;
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
        clear(canvas, self.desktop_background);
        for target in &self.desktop_hit_targets {
            let selected = matches!(
                &target.action,
                ShellAction::OpenDesktopItem(path, _) if self.desktop_selection.contains(path)
            );
            if selected {
                fill_rect(
                    canvas,
                    width,
                    height,
                    scale_rect(inset(target.rect, 2), self.scale_120),
                    theme.selection.rgba(),
                );
            }
            if self.hovered.as_ref() == Some(&target.action) {
                fill_rect(
                    canvas,
                    width,
                    height,
                    scale_rect(inset(target.rect, 3), self.scale_120),
                    theme.hover.rgba(),
                );
            }
            draw_desktop_shortcut(
                canvas,
                (width, height),
                self.font.as_ref(),
                target,
                self.scale_120,
                theme,
                match &target.action {
                    ShellAction::OpenDesktopItem(path, DesktopItemKind::Launcher) => {
                        self.desktop_icons.get(path)
                    }
                    _ => None,
                },
            );
        }
        if let Some(DesktopPointerGesture::RubberBand { start, current, .. }) =
            &self.desktop_pointer_gesture
        {
            draw_outline(
                canvas,
                width,
                height,
                scale_rect(rect_between(*start, *current), self.scale_120),
                scale_coord(2, self.scale_120).max(1),
                theme.focus.rgba(),
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
        let theme = self.palette;
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
        clear(canvas, theme.canvas.rgba());
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
            theme.border.rgba(),
        );
        for target in panel_targets(
            logical_width,
            &windows,
            self.task_offset,
            self.capped_task_buttons,
        ) {
            let hovered = self.hovered.as_ref() == Some(&target.action);
            let (color, label, active) = match target.action {
                ShellAction::ToggleApplications => {
                    let active = self.menu_open && self.menu_kind == MenuKind::Applications;
                    (
                        if active {
                            theme.selection.rgba()
                        } else if hovered {
                            theme.hover.rgba()
                        } else {
                            theme.surface.rgba()
                        },
                        "Applications".to_owned(),
                        active,
                    )
                }
                ShellAction::OpenFiles => (
                    if hovered {
                        theme.hover.rgba()
                    } else {
                        theme.surface.rgba()
                    },
                    "Files".to_owned(),
                    false,
                ),
                ShellAction::OpenShared => (
                    if hovered {
                        theme.hover.rgba()
                    } else {
                        theme.surface.rgba()
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
                            theme.raised.rgba()
                        } else {
                            if hovered {
                                theme.hover.rgba()
                            } else {
                                theme.surface.rgba()
                            }
                        },
                        title,
                        focused,
                    )
                }
                ShellAction::TaskbarPrevious => (
                    if hovered {
                        theme.hover.rgba()
                    } else {
                        theme.surface.rgba()
                    },
                    "<".to_owned(),
                    false,
                ),
                ShellAction::TaskbarNext => (
                    if hovered {
                        theme.hover.rgba()
                    } else {
                        theme.surface.rgba()
                    },
                    ">".to_owned(),
                    false,
                ),
                ShellAction::ShowDesktop => (
                    if hovered {
                        theme.hover.rgba()
                    } else {
                        theme.surface.rgba()
                    },
                    String::new(),
                    false,
                ),
                _ => continue,
            };
            let button_rect = scale_rect(target.rect, self.scale_120);
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
                                .saturating_sub(scale_coord(3, self.scale_120)),
                        width: button_rect.width,
                        height: scale_coord(3, self.scale_120),
                    },
                    theme.focus.rgba(),
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
                if color == theme.selection.rgba() {
                    theme.selected_text.rgba()
                } else {
                    theme.text.rgba()
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
        let theme = self.palette;
        let (surface_logical_width, surface_logical_height) = nonzero_size(self.menu_size);
        let (logical_width, logical_height) = if self.menu_open {
            match self.menu_kind {
                MenuKind::Applications => nonzero_size(self.preferred_menu_size()),
                MenuKind::Window(_) => (WINDOW_MENU_WIDTH, WINDOW_MENU_HEIGHT),
            }
        } else {
            (surface_logical_width, surface_logical_height)
        };
        let (surface_width, surface_height) = physical_size(
            (surface_logical_width, surface_logical_height),
            self.scale_120,
        );
        let (width, height) = physical_size((logical_width, logical_height), self.scale_120);
        let visible_menu_rows = self.visible_menu_rows();
        let filtered_applications = self.filtered_applications();
        let (buffer, surface_canvas) = self
            .pool
            .create_buffer(
                surface_width as i32,
                surface_height as i32,
                surface_width as i32 * 4,
                wl_shm::Format::Argb8888,
            )
            .context("allocating applications menu frame")?;
        clear(surface_canvas, [0, 0, 0, 0]);
        let content_len = usize::try_from(width)
            .unwrap_or(usize::MAX)
            .saturating_mul(usize::try_from(height).unwrap_or(usize::MAX))
            .saturating_mul(4);
        let mut content_canvas = vec![0_u8; content_len];
        let canvas = content_canvas.as_mut_slice();
        clear(
            canvas,
            if self.menu_open && !self.window_menu_pending_pointer {
                theme.menu.rgba()
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
                theme.raised.rgba(),
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
                theme.selection.rgba(),
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
                theme.text.rgba(),
            );
            let close = applications_menu_close_target(logical_width);
            let close_hovered = self.hovered.as_ref() == Some(&close.action);
            if close_hovered {
                fill_rect(
                    canvas,
                    width,
                    height,
                    scale_rect(inset(close.rect, 3), self.scale_120),
                    theme.hover.rgba(),
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
                theme.text.rgba(),
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
                theme.text_muted.rgba(),
            );
            let search_rect = Rect {
                x: 8,
                y: i32::try_from(logical_height)
                    .unwrap_or(i32::MAX)
                    .saturating_sub(APPLICATIONS_MENU_FOOTER_HEIGHT)
                    .saturating_add(6),
                width: i32::try_from(logical_width)
                    .unwrap_or(i32::MAX)
                    .saturating_sub(16),
                height: MENU_ROW_HEIGHT,
            };
            fill_rect(
                canvas,
                width,
                height,
                scale_rect(search_rect, self.scale_120),
                theme.raised.rgba(),
            );
            draw_outline(
                canvas,
                width,
                height,
                scale_rect(search_rect, self.scale_120),
                scale_coord(1, self.scale_120),
                theme.border.rgba(),
            );
            draw_text(
                canvas,
                width,
                height,
                self.font.as_ref(),
                if self.application_search.is_empty() {
                    "Search applications"
                } else {
                    &self.application_search
                },
                scale_coord(search_rect.x + 10, self.scale_120),
                scale_coord(search_rect.y + 10, self.scale_120),
                scale_font(13.0, self.scale_120),
                if self.application_search.is_empty() {
                    theme.text_muted.rgba()
                } else {
                    theme.text.rgba()
                },
            );
            for target in menu_targets(
                logical_width,
                logical_height,
                &filtered_applications,
                self.menu_scroll,
            ) {
                let hovered = self.hovered.as_ref() == Some(&target.action);
                fill_rect(
                    canvas,
                    width,
                    height,
                    scale_rect(inset(target.rect, 2), self.scale_120),
                    if hovered {
                        theme.hover.rgba()
                    } else {
                        theme.menu.rgba()
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
                    draw_menu_icon(canvas, width, height, icon_rect, &target.action, theme);
                }
                let update_badge: Option<usize> = None;
                let display_label = match &target.action {
                    ShellAction::LaunchApplication(id) if self.pinned_applications.contains(id) => {
                        format!("★ {}", target.label)
                    }
                    _ => target.label.clone(),
                };
                draw_text(
                    canvas,
                    width,
                    height,
                    self.font.as_ref(),
                    &elide_to_width(
                        self.font.as_ref(),
                        &display_label,
                        13.0,
                        target.rect.width.saturating_sub(if update_badge.is_some() {
                            96
                        } else {
                            54
                        }) as f32,
                    ),
                    scale_coord(target.rect.x + 38, self.scale_120),
                    scale_coord(target.rect.y + 10, self.scale_120),
                    scale_font(13.0, self.scale_120),
                    theme.text.rgba(),
                );
                if let Some(count) = update_badge {
                    let badge = Rect {
                        x: target.rect.x + target.rect.width - 46,
                        y: target.rect.y + 7,
                        width: 38,
                        height: 22,
                    };
                    fill_rect(
                        canvas,
                        width,
                        height,
                        scale_rect(badge, self.scale_120),
                        theme.selection.rgba(),
                    );
                    draw_text_centered(
                        canvas,
                        width,
                        height,
                        self.font.as_ref(),
                        &if count > 99 {
                            "99+".to_owned()
                        } else {
                            count.to_string()
                        },
                        scale_rect(badge, self.scale_120),
                        scale_font(11.0, self.scale_120),
                        theme.selected_text.rgba(),
                    );
                }
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
                    theme.text_secondary.rgba(),
                );
            }
            if self.menu_scroll + visible_menu_rows < filtered_applications.len() {
                draw_text(
                    canvas,
                    width,
                    height,
                    self.font.as_ref(),
                    "scroll ▼",
                    scale_coord(logical_width as i32 - 72, self.scale_120),
                    scale_coord(logical_height as i32 - 62, self.scale_120),
                    scale_font(9.0, self.scale_120),
                    theme.text_secondary.rgba(),
                );
            }
        } else if self.menu_open
            && !self.window_menu_pending_pointer
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
                theme.raised.rgba(),
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
                theme.selection.rgba(),
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
                theme.text.rgba(),
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
                            theme.destructive.rgba()
                        } else {
                            theme.hover.rgba()
                        }
                    } else {
                        theme.menu.rgba()
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
                    theme.text.rgba(),
                );
            }
        }
        let destination_x = usize::try_from(
            scale_coord(self.menu_origin.0, self.scale_120)
                .max(0)
                .min(i32::try_from(surface_width.saturating_sub(width)).unwrap_or(i32::MAX)),
        )
        .unwrap_or_default();
        let destination_y = usize::try_from(
            scale_coord(self.menu_origin.1, self.scale_120)
                .max(0)
                .min(i32::try_from(surface_height.saturating_sub(height)).unwrap_or(i32::MAX)),
        )
        .unwrap_or_default();
        let row_bytes = usize::try_from(width)
            .unwrap_or(usize::MAX)
            .saturating_mul(4);
        let surface_stride = usize::try_from(surface_width)
            .unwrap_or(usize::MAX)
            .saturating_mul(4);
        for row in 0..usize::try_from(height).unwrap_or_default() {
            let source_start = row.saturating_mul(row_bytes);
            let source_end = source_start.saturating_add(row_bytes);
            let destination_start = destination_y
                .saturating_add(row)
                .saturating_mul(surface_stride)
                .saturating_add(destination_x.saturating_mul(4));
            let destination_end = destination_start.saturating_add(row_bytes);
            if source_end <= content_canvas.len() && destination_end <= surface_canvas.len() {
                surface_canvas[destination_start..destination_end]
                    .copy_from_slice(&content_canvas[source_start..source_end]);
            }
        }
        attach(
            &self.menu,
            &self.viewports[2],
            buffer,
            surface_width,
            surface_height,
            surface_logical_width,
            surface_logical_height,
        )?;
        Ok(())
    }

    fn draw_context(&mut self) -> Result<()> {
        let theme = self.palette;
        let targets = self.context_targets();
        let (logical_width, logical_height) = nonzero_size(self.context_size);
        let (width, height) = physical_size((logical_width, logical_height), self.scale_120);
        let (buffer, canvas) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                width as i32 * 4,
                wl_shm::Format::Argb8888,
            )
            .context("allocating application context frame")?;
        clear(
            canvas,
            if self.context_state.is_visible() {
                theme.menu.rgba()
            } else {
                [0, 0, 0, 0]
            },
        );
        match &self.context_state {
            ContextState::Edit(dialog) => {
                let title = match dialog.operation {
                    EditOperation::NewFolder => "New Folder",
                    EditOperation::Rename(_) => "Rename Item",
                    EditOperation::RenameApplication(_) => "Rename Application",
                };
                draw_text(
                    canvas,
                    width,
                    height,
                    self.font.as_ref(),
                    title,
                    scale_coord(16, self.scale_120),
                    scale_coord(18, self.scale_120),
                    scale_font(17.0, self.scale_120),
                    theme.text.rgba(),
                );
                let input = Rect {
                    x: 16,
                    y: 54,
                    width: 402,
                    height: 42,
                };
                fill_rect(
                    canvas,
                    width,
                    height,
                    scale_rect(input, self.scale_120),
                    theme.surface.rgba(),
                );
                draw_text(
                    canvas,
                    width,
                    height,
                    self.font.as_ref(),
                    &elide(&dialog.input, 48),
                    scale_coord(26, self.scale_120),
                    scale_coord(67, self.scale_120),
                    scale_font(14.0, self.scale_120),
                    theme.text.rgba(),
                );
                if let Some(error) = &dialog.error {
                    draw_text(
                        canvas,
                        width,
                        height,
                        self.font.as_ref(),
                        &elide(error, 58),
                        scale_coord(16, self.scale_120),
                        scale_coord(108, self.scale_120),
                        scale_font(11.0, self.scale_120),
                        theme.destructive.rgba(),
                    );
                }
            }
            ContextState::Delete(dialog) => {
                draw_dialog_text(
                    canvas,
                    width,
                    height,
                    self.font.as_ref(),
                    "Delete permanently?",
                    &dialog.detail,
                    dialog.error.as_deref(),
                    self.scale_120,
                    theme,
                );
            }
            ContextState::Collision(session) => {
                let name = session
                    .sources
                    .get(session.index)
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "item".to_owned());
                draw_dialog_text(
                    canvas,
                    width,
                    height,
                    self.font.as_ref(),
                    "An item already exists",
                    &format!("Choose how to paste ‘{}’.", elide(&name, 34)),
                    None,
                    self.scale_120,
                    theme,
                );
            }
            ContextState::Error { title, detail } => {
                draw_dialog_text(
                    canvas,
                    width,
                    height,
                    self.font.as_ref(),
                    title,
                    detail,
                    None,
                    self.scale_120,
                    theme,
                );
            }
            _ => {}
        }
        for target in targets {
            let hovered = self.hovered.as_ref() == Some(&target.action);
            let disabled = matches!(target.action, ShellAction::DesktopPaste)
                && matches!(self.context_state, ContextState::DesktopMenu)
                && !self.paste_available;
            let destructive = matches!(
                target.action,
                ShellAction::DesktopDelete | ShellAction::DesktopDeleteConfirm
            );
            fill_rect(
                canvas,
                width,
                height,
                scale_rect(inset(target.rect, 2), self.scale_120),
                if destructive {
                    theme.destructive.rgba()
                } else if hovered {
                    theme.hover.rgba()
                } else {
                    theme.menu.rgba()
                },
            );
            draw_text(
                canvas,
                width,
                height,
                self.font.as_ref(),
                &target.label,
                scale_coord(target.rect.x + 10, self.scale_120),
                scale_coord(target.rect.y + 10, self.scale_120),
                scale_font(13.0, self.scale_120),
                if destructive {
                    theme.selected_text.rgba()
                } else if disabled {
                    theme.text_secondary.rgba()
                } else {
                    theme.text.rgba()
                },
            );
        }
        attach(
            &self.context,
            &self.viewports[3],
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
        let mut focus = ROOT;
        let panel_y = self.desktop_size.1.saturating_sub(self.panel_size.1) as i32;
        let (menu_x, menu_y) = self.menu_origin;
        let windows = self.windows();
        let applications_menu_open = self.menu_open && self.menu_kind == MenuKind::Applications;

        for (index, target) in self.desktop_accessible_targets.iter().cloned().enumerate() {
            let id = NodeId(100 + index as u64);
            let mut node = A11yNode::new(Role::Button);
            node.set_label(target.label.clone());
            node.set_bounds(a11y_rect(target.rect, 0, 0));
            node.add_action(Action::Click);
            if let ShellAction::OpenDesktopItem(path, _) = &target.action {
                node.set_selected(self.desktop_selection.contains(path));
            }
            children.push(id);
            targets.insert(
                id,
                AccessibleTarget::Activate {
                    action: target.action.clone(),
                    menu_index: None,
                },
            );
            nodes.push((id, node));

            if let ShellAction::OpenDesktopItem(path, kind) = target.action {
                let selection_id = NodeId(75_000 + u64::try_from(index).unwrap_or_default());
                let mut selection = A11yNode::new(Role::Button);
                selection.set_label(if self.desktop_selection.contains(&path) {
                    format!("Remove {} from selection", target.label)
                } else {
                    format!("Add {} to selection", target.label)
                });
                selection.add_action(Action::Click);
                nodes.push((selection_id, selection));
                children.push(selection_id);
                targets.insert(
                    selection_id,
                    AccessibleTarget::ToggleDesktopSelection(path.clone()),
                );
                let mut operations = vec![
                    ("Open", ShellAction::DesktopOpenSelection),
                    ("Cut", ShellAction::DesktopCut),
                    ("Copy", ShellAction::DesktopCopy),
                    ("Rename", ShellAction::DesktopRename),
                    ("Delete", ShellAction::DesktopDelete),
                ];
                if kind == DesktopItemKind::AppImage {
                    let registered = RegistrationStore::discover()
                        .and_then(|store| store.find_by_target(&path))
                        .ok()
                        .flatten()
                        .is_some_and(|registration| registration.applications_launcher);
                    if !registered {
                        operations.push((
                            "Add AppImage to Applications",
                            ShellAction::DesktopAddToApplications,
                        ));
                    }
                }
                for (operation_index, (label, action)) in operations.into_iter().enumerate() {
                    let operation_id = NodeId(
                        70_000
                            + u64::try_from(index).unwrap_or_default() * 10
                            + u64::try_from(operation_index).unwrap_or_default(),
                    );
                    let mut operation = A11yNode::new(Role::Button);
                    operation.set_label(format!("{label} {}", target.label));
                    operation.add_action(Action::Click);
                    nodes.push((operation_id, operation));
                    children.push(operation_id);
                    targets.insert(
                        operation_id,
                        AccessibleTarget::DesktopAction {
                            path: path.clone(),
                            action,
                        },
                    );
                }
            }
        }
        for (index, (label, action)) in [
            ("Paste onto Desktop", ShellAction::DesktopPaste),
            ("New Folder on Desktop", ShellAction::DesktopNewFolder),
            ("Arrange Desktop Icons", ShellAction::DesktopArrangeIcons),
        ]
        .into_iter()
        .enumerate()
        {
            let id = NodeId(69_000 + u64::try_from(index).unwrap_or_default());
            let mut node = A11yNode::new(Role::Button);
            node.set_label(label);
            if matches!(action, ShellAction::DesktopPaste) && !self.paste_available {
                node.set_disabled();
            }
            node.add_action(Action::Click);
            nodes.push((id, node));
            children.push(id);
            targets.insert(
                id,
                AccessibleTarget::Activate {
                    action,
                    menu_index: None,
                },
            );
        }
        if self.context_state.is_visible() {
            for (index, target) in self.context_targets().into_iter().enumerate() {
                add_accessible_target(
                    &mut nodes,
                    &mut children,
                    &mut targets,
                    NodeId(80_000 + u64::try_from(index).unwrap_or_default()),
                    target,
                    self.context_origin.0,
                    self.context_origin.1,
                );
            }
            if let ContextState::Delete(dialog) = &self.context_state {
                let id = NodeId(80_100);
                let mut description = A11yNode::new(Role::Label);
                description.set_label(format!(
                    "{} selected. {}",
                    dialog.items.len(),
                    dialog.detail
                ));
                children.push(id);
                nodes.push((id, description));
                // Delete is never the default. This node is the Cancel button
                // from `context_targets` above.
                focus = NodeId(80_001);
            } else if let ContextState::Collision(_) = &self.context_state {
                focus = NodeId(80_002);
            } else if let ContextState::Error { .. } = &self.context_state {
                focus = NodeId(80_000);
            } else if let ContextState::Edit(dialog) = &self.context_state {
                let id = NodeId(80_100);
                let mut input = A11yNode::new(Role::TextInput);
                input.set_label("Name");
                input.set_value(dialog.input.clone());
                input.set_bounds(a11y_rect(
                    Rect {
                        x: 16,
                        y: 54,
                        width: 402,
                        height: 42,
                    },
                    self.context_origin.0,
                    self.context_origin.1,
                ));
                input.add_action(Action::SetValue);
                input.add_action(Action::ReplaceSelectedText);
                children.push(id);
                nodes.push((id, input));
                targets.insert(id, AccessibleTarget::EditValue);
                focus = id;
            }
        }
        let panel_targets = panel_targets(
            self.panel_size.0,
            &windows,
            self.task_offset,
            self.capped_task_buttons,
        );
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
        let filtered_applications = self.filtered_applications();
        let content_size = self.preferred_menu_size();
        let search_id = NodeId(20_002);
        let mut search = A11yNode::new(Role::TextInput);
        search.set_label("Search applications");
        search.set_value(self.application_search.clone());
        if applications_menu_open {
            search.set_bounds(a11y_rect(
                Rect {
                    x: 8,
                    y: i32::try_from(content_size.1)
                        .unwrap_or(i32::MAX)
                        .saturating_sub(APPLICATIONS_MENU_FOOTER_HEIGHT)
                        .saturating_add(6),
                    width: i32::try_from(content_size.0)
                        .unwrap_or(i32::MAX)
                        .saturating_sub(16),
                    height: MENU_ROW_HEIGHT,
                },
                menu_x,
                menu_y,
            ));
        }
        search.add_action(Action::SetValue);
        search.add_action(Action::ReplaceSelectedText);
        nodes.push((search_id, search));
        menu_children.push(search_id);
        targets.insert(search_id, AccessibleTarget::ApplicationSearch);
        for (index, application) in self.applications.iter().enumerate() {
            let id = NodeId(10_000 + index as u64);
            let filtered_index = filtered_applications
                .iter()
                .position(|candidate| candidate.id == application.id);
            let relative_row = filtered_index
                .map(|index| {
                    i32::try_from(index)
                        .unwrap_or(i32::MAX)
                        .saturating_sub(i32::try_from(self.menu_scroll).unwrap_or(i32::MAX))
                })
                .unwrap_or(i32::MAX);
            let mut node = A11yNode::new(Role::MenuItem);
            node.set_label(application.name.clone());
            if applications_menu_open
                && filtered_index.is_some_and(|index| {
                    index >= self.menu_scroll
                        && index < self.menu_scroll.saturating_add(visible_rows)
                })
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
            node.set_position_in_set(filtered_index.map_or(index + 1, |position| position + 1));
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
                    menu_index: filtered_index,
                },
            );

            // Expose shortcut creation as a direct AT-SPI action for every
            // installed application. Agents do not have to open the visual
            // context menu, find a row, or scroll it into view first.
            let shortcut_id = NodeId(40_000 + index as u64);
            let mut shortcut = A11yNode::new(Role::Button);
            shortcut.set_label(format!("Add {} to Desktop", application.name));
            shortcut.add_action(Action::Click);
            nodes.push((shortcut_id, shortcut));
            menu_children.push(shortcut_id);
            targets.insert(
                shortcut_id,
                AccessibleTarget::Activate {
                    action: ShellAction::AddApplicationDesktopShortcut(application.id.clone()),
                    menu_index: None,
                },
            );
        }
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
                f64::from(menu_x) + f64::from(content_size.0),
                f64::from(menu_y) + f64::from(content_size.1),
            ));
        }
        menu.set_children(menu_children);
        menu.set_size_of_set(filtered_applications.len());
        if applications_menu_open {
            menu.set_scroll_y(self.menu_scroll as f64);
            menu.set_scroll_y_min(0.0);
            menu.set_scroll_y_max(filtered_applications.len().saturating_sub(visible_rows) as f64);
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
                focus,
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
            eprintln!("buzzardos-shell: desktop configured {}x{}", size.0, size.1);
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
            if desktop_size_changed && let Err(error) = self.rebuild_desktop_targets() {
                eprintln!("buzzardos-shell: desktop reflow failed: {error:#}");
            }
        } else if layer == &self.panel {
            self.panel_size = size;
            self.panel_configured = true;
            eprintln!("buzzardos-shell: panel configured {}x{}", size.0, size.1);
        } else if layer == &self.menu {
            self.menu_size = size;
            self.menu_configured = true;
            self.clamp_menu_scroll();
            let _ = self.set_menu_input_region();
        } else if layer == &self.context {
            self.context_size = size;
            self.context_configured = true;
            let _ = self.set_context_input_region();
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
            let cursor_surface = self.compositor.create_surface(qh);
            self.pointer = self
                .seat_state
                .get_pointer_with_theme(
                    qh,
                    &seat,
                    self.shm.wl_shm(),
                    cursor_surface,
                    ThemeSpec::default(),
                )
                .ok();
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
        if capability == Capability::Pointer && self.pointer.is_some() {
            self.pointer.take();
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
        connection: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    if let Some(pointer) = self.pointer.as_ref()
                        && let Err(error) = pointer.set_cursor(connection, CursorIcon::Default)
                    {
                        eprintln!("buzzardos-shell: restoring the default pointer: {error}");
                    }
                    if event.surface == *self.menu.wl_surface() {
                        self.position_pending_window_menu(event.position.0);
                    }
                    self.update_hover(&event.surface, event.position.0, event.position.1);
                }
                PointerEventKind::Motion { .. } => {
                    self.update_hover(&event.surface, event.position.0, event.position.1);
                    if event.surface == *self.desktop.wl_surface() {
                        self.desktop_pointer_motion(event.position.0, event.position.1);
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.hovered.take().is_some() {
                        self.dirty = true;
                    }
                }
                PointerEventKind::Press { button, time, .. }
                    if button == BTN_LEFT && event.surface == *self.desktop.wl_surface() =>
                {
                    self.desktop_pointer_press(event.position.0, event.position.1, time);
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    self.click_surface(&event.surface, event.position.0, event.position.1);
                }
                PointerEventKind::Release { button, .. }
                    if button == BTN_LEFT && event.surface == *self.desktop.wl_surface() =>
                {
                    self.desktop_pointer_release(event.position.0, event.position.1);
                }
                PointerEventKind::Press { button, .. }
                    if button == BTN_RIGHT && event.surface == *self.desktop.wl_surface() =>
                {
                    self.secondary_click_desktop(event.position.0, event.position.1);
                }
                PointerEventKind::Press { button, .. }
                    if button == BTN_RIGHT && event.surface == *self.panel.wl_surface() =>
                {
                    self.secondary_click_panel(event.position.0, event.position.1);
                }
                PointerEventKind::Press { button, .. }
                    if button == BTN_RIGHT && event.surface == *self.menu.wl_surface() =>
                {
                    self.secondary_click_applications_menu(event.position.0, event.position.1);
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
                PointerEventKind::Axis { vertical, .. }
                    if event.surface == *self.desktop.wl_surface() =>
                {
                    let amount = if vertical.value120 != 0 {
                        f64::from(vertical.value120)
                    } else if vertical.discrete != 0 {
                        f64::from(vertical.discrete)
                    } else {
                        vertical.absolute
                    };
                    self.scroll_desktop(amount);
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
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.keyboard_focus = if surface == self.desktop.wl_surface() {
            Some(ShellSurface::Desktop)
        } else if surface == self.panel.wl_surface() {
            Some(ShellSurface::Panel)
        } else if surface == self.menu.wl_surface() {
            Some(ShellSurface::Menu)
        } else if surface == self.context.wl_surface() {
            Some(ShellSurface::Context)
        } else {
            None
        };
        if let Some(accessibility) = self.accessibility.as_mut() {
            accessibility.adapter.update_window_focus_state(true);
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.keyboard_focus.is_some_and(|focused| match focused {
            ShellSurface::Desktop => surface == self.desktop.wl_surface(),
            ShellSurface::Panel => surface == self.panel.wl_surface(),
            ShellSurface::Menu => surface == self.menu.wl_surface(),
            ShellSurface::Context => surface == self.context.wl_surface(),
        }) {
            self.keyboard_focus = None;
        }
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
        self.handle_key(event);
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if matches!(event.keysym, Keysym::BackSpace | Keysym::Delete)
            && (matches!(self.context_state, ContextState::Edit(_))
                || (self.menu_open && self.menu_kind == MenuKind::Applications))
        {
            self.handle_key(event);
        }
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
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
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
        eprintln!("buzzardos-shell: launching {program} failed: {error}");
    }
}

fn launch_application(application: &Application) {
    let result = gio::DesktopAppInfo::from_filename(&application.source)
        .context("desktop entry disappeared")
        .and_then(|info| {
            info.launch(&[], gio::AppLaunchContext::NONE)
                .context("GIO launch failed")
        });
    if let Err(error) = result {
        eprintln!(
            "buzzardos-shell: launching {} from {} failed: {error:#}",
            application.name,
            application.source.display()
        );
    }
}

fn managed_appimage_registration_id(application: &Application) -> Option<RegistrationId> {
    let value = application
        .id
        .strip_prefix("buzzardos-appimage-")?
        .strip_suffix(".desktop")?;
    RegistrationId::from_str(value).ok()
}

fn add_application_desktop_shortcut(application: &Application) -> Result<()> {
    if let Some(id) = managed_appimage_registration_id(application) {
        RegistrationStore::discover()?.add_desktop(id)?;
        return Ok(());
    }
    // Re-run the authoritative discovery immediately before copying. This
    // prevents an application path changed after the menu scan from being
    // projected without passing the same FreeDesktop validation again.
    let paths = XdgPaths::discover()?;
    let current = buzzardos_desktop_core::discover_applications(&paths)
        .applications
        .into_iter()
        .find(|candidate| {
            candidate.id.as_str() == application.id && candidate.source == application.source
        })
        .context("application changed before shortcut creation")?;
    let bytes = read_bounded(&current.source, 1024 * 1024)?;
    atomic_write(&paths.desktop_dir.join(&application.id), &bytes, 0o755)?;
    Ok(())
}

fn open_desktop_item(path: &std::path::Path, kind: DesktopItemKind) -> Result<()> {
    match kind {
        DesktopItemKind::AppImage => {
            let store = RegistrationStore::discover()?;
            if let Some(registration) = store.find_by_target(path)? {
                // Use the same chooser-enabled executable as Applications,
                // Settings, AT-SPI, and CUA activation. Keeping GTK out of the
                // long-running layer-shell process avoids a second relink
                // implementation while still giving desktop activation the
                // required native missing-target flow.
                let id = registration.id.to_string();
                spawn(HELPER_EXECUTABLE, [OsStr::new("launch"), OsStr::new(&id)]);
            } else {
                let _ = launch_path(path)?;
            }
            Ok(())
        }
        DesktopItemKind::Launcher => {
            let info = gio::DesktopAppInfo::from_filename(path)
                .context("desktop launcher is malformed or disappeared")?;
            info.launch(&[], gio::AppLaunchContext::NONE)
                .context("launching desktop shortcut")
        }
        DesktopItemKind::RegularFile
        | DesktopItemKind::Directory
        | DesktopItemKind::SymbolicLink => {
            let uri = gio::File::for_path(path).uri();
            gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
                .context("opening desktop item")
        }
    }
}

fn read_clipboard_mime(mime: &str) -> Result<Vec<u8>> {
    let (mut reader, _) = clipboard_paste::get_contents(
        clipboard_paste::ClipboardType::Regular,
        clipboard_paste::Seat::Unspecified,
        clipboard_paste::MimeType::Specific(mime),
    )
    .with_context(|| format!("requesting {mime} from the guest clipboard"))?;
    let descriptor = reader.as_raw_fd();
    // The clipboard owner is another guest process and may be unresponsive.
    // A nonblocking descriptor plus a fixed deadline keeps the shell's input
    // loop from being held indefinitely by a hostile or crashed owner.
    // SAFETY: fcntl operates on the live pipe descriptor owned by `reader`.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error()).context("making clipboard pipe nonblocking");
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if bytes.len().saturating_add(count) > MAX_DESKTOP_CLIPBOARD_BYTES {
                    anyhow::bail!(
                        "desktop clipboard data exceeds {MAX_DESKTOP_CLIPBOARD_BYTES} bytes"
                    );
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let now = Instant::now();
                if now >= deadline {
                    anyhow::bail!("guest clipboard read timed out");
                }
                let remaining = deadline.saturating_duration_since(now);
                let timeout = i32::try_from(remaining.as_millis().min(100)).unwrap_or(100);
                let mut poll_fd = libc::pollfd {
                    fd: descriptor,
                    events: libc::POLLIN | libc::POLLHUP,
                    revents: 0,
                };
                // SAFETY: `poll_fd` is valid for the duration of this call.
                let result = unsafe { libc::poll(&mut poll_fd, 1, timeout) };
                if result < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        return Err(error).context("waiting for guest clipboard data");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("reading guest clipboard data"),
        }
    }
    Ok(bytes)
}

fn clipboard_has_supported_contents() -> bool {
    clipboard_paste::get_mime_types(
        clipboard_paste::ClipboardType::Regular,
        clipboard_paste::Seat::Unspecified,
    )
    .is_ok_and(|types| types.contains(DESKTOP_CLIPBOARD_MIME) || types.contains(URI_LIST_MIME))
}

fn parse_uri_list(bytes: &[u8]) -> Result<Vec<PathBuf>> {
    let text = std::str::from_utf8(bytes).context("file URI list is not UTF-8")?;
    if text.contains('\0') {
        anyhow::bail!("file URI list contains NUL");
    }
    let mut paths = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !line.starts_with("file://") {
            anyhow::bail!("clipboard URI is not a local file URI");
        }
        let path = gio::File::for_uri(line)
            .path()
            .context("clipboard file URI has no local path")?;
        if !path.is_absolute() {
            anyhow::bail!("clipboard file URI is not absolute");
        }
        paths.push(path);
        if paths.len() > 4096 {
            anyhow::bail!("clipboard URI list contains too many items");
        }
    }
    Ok(paths)
}

#[cfg(test)]
fn delete_dialog_detail(desktop_path: &Path, items: &[PathBuf]) -> Result<String> {
    let desktop = DesktopDirectory::open(desktop_path)?;
    let consequences = items
        .iter()
        .map(|path| {
            desktop
                .consequence(path.file_name().context("desktop item has no name")?)
                .map_err(Into::into)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(delete_dialog_detail_from_consequences(items, &consequences))
}

fn delete_dialog_detail_from_consequences(
    items: &[PathBuf],
    consequences: &[DeleteConsequence],
) -> String {
    if items.len() != 1 || consequences.len() != 1 {
        let shortcuts = consequences
            .iter()
            .filter(|value| **value == DeleteConsequence::ShortcutOnly)
            .count();
        let links = consequences
            .iter()
            .filter(|value| **value == DeleteConsequence::LinkOnly)
            .count();
        let folders = consequences
            .iter()
            .filter(|value| **value == DeleteConsequence::DirectoryTree)
            .count();
        return format!(
            "This permanently removes {} selected items ({} shortcuts, {} links, {} folders). Shortcut and link targets are not deleted; folder contents are.",
            items.len(),
            shortcuts,
            links,
            folders
        );
    }
    let name = items[0].file_name().unwrap_or_else(|| OsStr::new("item"));
    let display = name.to_string_lossy();
    match consequences[0] {
        DeleteConsequence::ShortcutOnly => {
            "This removes only the shortcut. The target will not be deleted.".to_owned()
        }
        DeleteConsequence::LinkOnly => {
            "This removes the link only. Its target will not be deleted.".to_owned()
        }
        DeleteConsequence::RegularFile => format!("This permanently deletes ‘{display}’."),
        DeleteConsequence::DirectoryTree => {
            format!("This permanently deletes ‘{display}’ and everything inside it.")
        }
    }
}

fn distance(left: (f64, f64), right: (f64, f64)) -> f64 {
    (left.0 - right.0).hypot(left.1 - right.1)
}

fn rect_between(start: (f64, f64), end: (f64, f64)) -> Rect {
    let left = start.0.min(end.0).floor() as i32;
    let top = start.1.min(end.1).floor() as i32;
    let right = start.0.max(end.0).ceil() as i32;
    let bottom = start.1.max(end.1).ceil() as i32;
    Rect {
        x: left,
        y: top,
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
    }
}

fn rects_intersect(left: Rect, right: Rect) -> bool {
    left.x < right.x.saturating_add(right.width)
        && left.x.saturating_add(left.width) > right.x
        && left.y < right.y.saturating_add(right.height)
        && left.y.saturating_add(left.height) > right.y
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
    let row_width = longest_application.max(text_width(font, "Search applications", 13.0)) + 64.0;
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

fn draw_outline(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    rect: Rect,
    thickness: i32,
    color: [u8; 4],
) {
    let thickness = thickness.max(1);
    fill_rect(
        canvas,
        width,
        height,
        Rect {
            height: thickness,
            ..rect
        },
        color,
    );
    fill_rect(
        canvas,
        width,
        height,
        Rect {
            y: rect.y.saturating_add(rect.height).saturating_sub(thickness),
            height: thickness,
            ..rect
        },
        color,
    );
    fill_rect(
        canvas,
        width,
        height,
        Rect {
            width: thickness,
            ..rect
        },
        color,
    );
    fill_rect(
        canvas,
        width,
        height,
        Rect {
            x: rect.x.saturating_add(rect.width).saturating_sub(thickness),
            width: thickness,
            ..rect
        },
        color,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_dialog_text(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    font: Option<&Font>,
    title: &str,
    detail: &str,
    error: Option<&str>,
    scale_120: u32,
    theme: ThemePalette,
) {
    draw_text(
        canvas,
        width,
        height,
        font,
        title,
        scale_coord(16, scale_120),
        scale_coord(18, scale_120),
        scale_font(17.0, scale_120),
        theme.text.rgba(),
    );
    draw_text(
        canvas,
        width,
        height,
        font,
        &elide(detail, 62),
        scale_coord(16, scale_120),
        scale_coord(58, scale_120),
        scale_font(13.0, scale_120),
        theme.text.rgba(),
    );
    if let Some(error) = error {
        draw_text(
            canvas,
            width,
            height,
            font,
            &elide(error, 62),
            scale_coord(16, scale_120),
            scale_coord(94, scale_120),
            scale_font(11.0, scale_120),
            theme.destructive.rgba(),
        );
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
    canvas_size: (u32, u32),
    font: Option<&Font>,
    target: &HitTarget,
    scale_120: u32,
    theme: ThemePalette,
    application_icon: Option<&AppIcon>,
) {
    let (width, height) = canvas_size;
    let rect = target.rect;
    let item_kind = match target.action {
        ShellAction::OpenDesktopItem(_, kind) => Some(kind),
        _ => None,
    };
    let is_folder = matches!(
        target.action,
        ShellAction::OpenFiles | ShellAction::OpenShared
    ) || item_kind == Some(DesktopItemKind::Directory);
    if is_folder {
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
            theme.folder_tab.rgba(),
        );
        fill_rect(
            canvas,
            width,
            height,
            scale_rect(folder, scale_120),
            theme.folder.rgba(),
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
                theme.surface.rgba(),
            );
        }
    } else if let Some(icon) = application_icon {
        draw_app_icon(
            canvas,
            width,
            height,
            scale_rect(
                Rect {
                    x: rect.x + 18,
                    y: rect.y + 4,
                    width: 52,
                    height: 52,
                },
                scale_120,
            ),
            icon,
        );
    } else {
        let document = Rect {
            x: rect.x + 24,
            y: rect.y + 5,
            width: 40,
            height: 48,
        };
        fill_rect(
            canvas,
            width,
            height,
            scale_rect(document, scale_120),
            if item_kind == Some(DesktopItemKind::AppImage) {
                theme.selection.rgba()
            } else {
                theme.raised.rgba()
            },
        );
        if item_kind == Some(DesktopItemKind::SymbolicLink) {
            draw_text_centered(
                canvas,
                width,
                height,
                font,
                "↗",
                scale_rect(
                    Rect {
                        x: document.x + 18,
                        y: document.y + 21,
                        width: 20,
                        height: 22,
                    },
                    scale_120,
                ),
                scale_font(15.0, scale_120),
                theme.text.rgba(),
            );
        }
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
        theme.text.rgba(),
    );
}

fn draw_menu_icon(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    rect: Rect,
    action: &ShellAction,
    theme: ThemePalette,
) {
    let color = match action {
        ShellAction::OpenFiles | ShellAction::OpenShared => theme.folder.rgba(),
        ShellAction::LaunchApplication(_) => theme.selection.rgba(),
        _ => theme.text_secondary.rgba(),
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
        ClipboardOperation, ClipboardToken, GSettingsAvailability, PANEL_HEIGHT,
        SETTINGS_POLL_INTERVAL, SettingsTracker, WINDOW_MENU_HEIGHT, WINDOW_MENU_WIDTH,
        applications_menu_height, applications_menu_width, delete_dialog_detail,
        gsettings_availability, load_settings, parse_uri_list, parse_window_menu_request,
        physical_size, rect_between, rects_intersect,
    };
    use crate::model::Application;
    use crate::model::Rect as ShellRect;
    use crate::sway_ipc::Rect;
    use buzzardos_desktop_core::{BackgroundChoice, Settings, ThemeMode};
    use gio::prelude::FileExt;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Instant;

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
                -50.0,
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
                1_200.0,
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
            super::titlebar_menu_origin(frame, 31, (1280, 800), 640.75),
            (640, 111)
        );
        assert_eq!(
            super::titlebar_menu_origin(frame, 31, (800, 600), 790.0).0,
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
    fn shell_control_request_carries_only_the_window_identity() {
        let request = parse_window_menu_request(
            br#"{"schema":1,"identifier":"window-id","x":712.5,"y":91.0}"#,
        )
        .unwrap();
        assert_eq!(request, "window-id");
        assert_eq!(
            parse_window_menu_request(b"legacy-window-id").unwrap(),
            "legacy-window-id"
        );
    }

    #[test]
    fn startup_reads_persisted_theme_without_rewriting_it() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("buzzardos");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.json");
        let mut settings = Settings {
            generation: 7,
            ..Settings::default()
        };
        settings.appearance.theme = ThemeMode::Light;
        settings.appearance.background = BackgroundChoice::DarkPlain;
        settings.save(&path).unwrap();
        let before = fs::read(&path).unwrap();
        assert_eq!(load_settings(&path).unwrap(), settings);
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn missing_gsettings_compatibility_is_boot_safe_but_broken_install_is_not() {
        let temp = tempfile::tempdir().unwrap();
        let missing_schema = temp.path().join("missing.gschema.xml");
        assert_eq!(
            gsettings_availability(&missing_schema, OsStr::new("gsettings")).unwrap(),
            GSettingsAvailability::MissingSchema
        );

        let schema = temp.path().join("org.gnome.desktop.interface.gschema.xml");
        fs::write(&schema, b"fixture").unwrap();
        assert_eq!(
            gsettings_availability(
                &schema,
                OsStr::new("/definitely/missing/buzzardos-gsettings")
            )
            .unwrap(),
            GSettingsAvailability::MissingTool
        );
        assert!(gsettings_availability(&schema, OsStr::new("/bin/false")).is_err());
    }

    #[test]
    fn settings_tracker_accepts_only_new_generations_and_preserves_last_confirmed() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("buzzardos");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("settings.json");
        let mut tracker = SettingsTracker::new(path.clone(), Settings::default());
        let mut light = Settings {
            generation: 1,
            ..Settings::default()
        };
        light.appearance.theme = ThemeMode::Light;
        light.save(&path).unwrap();
        tracker.last_check = Instant::now() - SETTINGS_POLL_INTERVAL;
        assert_eq!(tracker.candidate(), Some(light.clone()));
        tracker.commit(light.clone());

        let mut invalid_same_generation = light.clone();
        invalid_same_generation.appearance.theme = ThemeMode::Dark;
        invalid_same_generation.save(&path).unwrap();
        tracker.last_check = Instant::now() - SETTINGS_POLL_INTERVAL;
        assert_eq!(tracker.candidate(), None);
        assert_eq!(tracker.applied, light);
        assert!(
            tracker
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("without advancing generation"))
        );
    }

    #[test]
    fn desktop_uri_clipboard_accepts_only_local_file_uris() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("odd name 日本語.txt");
        fs::write(&path, b"fixture").unwrap();
        let uri = gio::File::for_path(&path).uri();
        assert_eq!(
            parse_uri_list(format!("# comment\r\n{uri}\r\n").as_bytes()).unwrap(),
            vec![path]
        );
        assert!(parse_uri_list(b"https://example.invalid/file\r\n").is_err());
        assert!(parse_uri_list(b"file:///tmp/valid\0file:///tmp/hidden").is_err());
    }

    #[test]
    fn desktop_clipboard_token_preserves_cut_semantics_without_paths() {
        let token = ClipboardToken {
            schema: 1,
            generation: 42,
            operation: ClipboardOperation::Cut,
        };
        let encoded = serde_json::to_vec(&token).unwrap();
        assert!(!String::from_utf8_lossy(&encoded).contains("/Desktop/"));
        let decoded: ClipboardToken = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.generation, 42);
        assert_eq!(decoded.operation, ClipboardOperation::Cut);
    }

    #[test]
    fn rubber_band_geometry_is_direction_independent() {
        let forward = rect_between((10.2, 15.7), (90.1, 110.9));
        assert_eq!(forward, rect_between((90.1, 110.9), (10.2, 15.7)));
        assert!(rects_intersect(
            forward,
            ShellRect {
                x: 80,
                y: 100,
                width: 30,
                height: 30,
            }
        ));
        assert!(!rects_intersect(
            forward,
            ShellRect {
                x: 100,
                y: 120,
                width: 30,
                height: 30,
            }
        ));
    }

    #[test]
    fn delete_confirmation_distinguishes_shortcuts_links_and_directory_trees() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let desktop = temp.path().join("Desktop");
        fs::create_dir(&desktop).unwrap();
        fs::write(
            desktop.join("Browser.desktop"),
            b"[Desktop Entry]\nType=Application\nName=Browser\nExec=true\n",
        )
        .unwrap();
        fs::create_dir(desktop.join("Folder")).unwrap();
        symlink("Folder", desktop.join("Folder link")).unwrap();

        assert!(
            delete_dialog_detail(&desktop, &[desktop.join("Browser.desktop")])
                .unwrap()
                .contains("only the shortcut")
        );
        assert!(
            delete_dialog_detail(&desktop, &[desktop.join("Folder")])
                .unwrap()
                .contains("everything inside")
        );
        assert!(
            delete_dialog_detail(&desktop, &[desktop.join("Folder link")])
                .unwrap()
                .contains("link only")
        );
    }
}
