// SPDX-License-Identifier: AGPL-3.0-or-later

use std::cell::{Cell, RefCell};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use buzzardos_clipboard_protocol::{MAX_IMAGE_BYTES, MAX_TEXT_BYTES, Mime};
use futures_util::future::{Either, select};
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use uuid::Uuid;
use wb_core::{
    HostMediaDevice, HostMediaKind, MachineConfig, MachineState, NetworkMode, PortDirection,
    PortForward, PortProtocol, PresentationDiagnostics, ResourceLocator, RuntimeState,
    WindowDiagnostics, discover_host_media,
};

use crate::clipboard::{self, ClipboardValue};
use crate::frame_paintable::FramePaintable;
use crate::gateway::{
    CursorImage, CursorStorage, DmabufFormat, DmabufFrame, GatewayCommand, GatewayCommandSender,
    GatewayConnection, GatewayEvent, GatewaySockets, GuestScalePreset, GuestScaleReply,
    GuestScaleRequest, HostCommand, OutputMode,
};
use crate::launch::Launch;

const RUNTIME_POLL_INTERVAL: Duration = Duration::from_millis(200);
const INITIAL_MONITOR_SIZE_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_GUEST_MONITOR_WIDTH: u32 = 320;
const MIN_GUEST_MONITOR_HEIGHT: u32 = 240;
const BACKGROUND_CLOCK_GRACE: Duration = Duration::from_millis(50);
const DEFAULT_REFRESH_MHZ: u32 = 60_000;
const WAYLAND_SCALE_DENOMINATOR: u32 = 120;
const WAYLAND_FIXED_DENOMINATOR: i64 = 256;
const MAX_WAYLAND_FIXED_EXTENT: u32 =
    ((i32::MAX as u64 + 1) / WAYLAND_FIXED_DENOMINATOR as u64) as u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

impl MonitorState {
    fn label(self) -> &'static str {
        match self {
            Self::Stopped => "Stopped",
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Stopping => "Stopping",
            Self::Failed => "Failed",
        }
    }

    fn css_class(self) -> &'static str {
        match self {
            Self::Stopped => "dim-label",
            Self::Starting | Self::Stopping => "warning",
            Self::Running => "success",
            Self::Failed => "error",
        }
    }
}

fn lifecycle_clipboard_epoch(current: u64, previous: MonitorState, next: MonitorState) -> u64 {
    if previous == next {
        current
    } else {
        current.wrapping_add(1)
    }
}

fn clipboard_transfer_is_live(current_epoch: u64, transfer_epoch: u64, available: bool) -> bool {
    current_epoch == transfer_epoch && available
}

pub(crate) struct HostApplication {
    launch: Launch,
    connection: GatewayConnection,
}

impl HostApplication {
    pub(crate) fn connect(launch: Launch, connection: GatewayConnection) -> Result<Self> {
        Ok(Self { launch, connection })
    }

    pub(crate) fn run(self, _gateway: GatewaySockets) -> Result<()> {
        let application = gtk::Application::builder()
            .application_id(&self.launch.app_id)
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        let system_theme = Rc::new(RefCell::new(None::<gio::Settings>));
        let system_theme_for_activation = Rc::clone(&system_theme);
        let activation = Rc::new(RefCell::new(Some((self.launch, self.connection))));
        application.connect_activate(move |application| {
            if system_theme_for_activation.borrow().is_none() {
                system_theme_for_activation
                    .replace(crate::host_theme::follow_system_color_scheme());
            }
            let Some((launch, connection)) = activation.borrow_mut().take() else {
                if let Some(window) = application.active_window() {
                    window.present();
                }
                return;
            };
            match NativeWindow::build(application, launch, connection) {
                Ok(window) => window.present(),
                Err(error) => {
                    eprintln!("buzzardos-display: creating native host application: {error:#}");
                    application.quit();
                }
            }
        });

        let status = application.run_with_args(&["buzzardos-display"]);
        if status != glib::ExitCode::SUCCESS {
            anyhow::bail!("native host application exited with {status:?}");
        }
        drop(system_theme);
        Ok(())
    }
}

struct NativeWindow {
    application: gtk::Application,
    launch: Launch,
    events: RefCell<Receiver<GatewayEvent>>,
    event_notify: GatewayConnectionNotifier,
    commands: GatewayCommandSender,

    window: gtk::ApplicationWindow,
    status_label: gtk::Label,
    state_title: gtk::Label,
    detail_label: gtk::Label,
    spinner: gtk::Spinner,
    monitor_view: gtk::Overlay,
    state_overlay: gtk::Box,
    picture: gtk::Picture,
    frame_paintable: FramePaintable,
    offload: gtk::GraphicsOffload,

    state: Cell<MonitorState>,
    close_requested: Cell<bool>,
    clipboard_busy: Cell<bool>,
    clipboard_epoch: Cell<u64>,
    clipboard_connection: RefCell<Option<(u64, UnixStream)>>,
    viewport_width: Cell<u32>,
    viewport_height: Cell<u32>,
    /// Fractional scale of the native host surface.
    host_surface_scale_120: Cell<u32>,
    /// Independently selected guest desktop UI scale.
    guest_ui_scale_120: Cell<u32>,
    /// Automatic follows the current host surface scale; manual presets do
    /// not change when the native window moves between monitors.
    guest_scale_preset: Cell<GuestScalePreset>,
    /// Changes exactly once for every committed physical/logical geometry or
    /// guest-scale selection transition.
    geometry_generation: Cell<u64>,
    refresh_mhz: Cell<u32>,
    initial_monitor_sizing: RefCell<InitialMonitorSizing>,
    gateway_configured: Cell<bool>,
    failure: RefCell<Option<String>>,
    last_runtime_check: Cell<Instant>,
    last_host_frame_tick: Cell<Instant>,
    last_background_frame_tick: Cell<Instant>,
    pending_frame: RefCell<Option<PendingFrame>>,
    pending_presentations: RefCell<VecDeque<PendingPresentation>>,
    presentation: RefCell<PresentationDiagnostics>,
    presentation_dirty: Cell<bool>,
    offload_geometry: RefCell<OffloadGeometryDiagnostics>,
    continuity: RefCell<MonitorContinuityDiagnostics>,
    pressed_pointer_buttons: RefCell<BTreeSet<u32>>,
    /// Sway may recommit an unchanged cursor surface while the pointer moves.
    /// Reinstalling an identical GDK cursor invalidates host-side state and can
    /// make the embedded monitor visibly flash. Keep the last complete image
    /// and update GTK only when the cursor shape or hotspot actually changes.
    last_cursor: RefCell<Option<CursorFingerprint>>,
    cursor_state: Cell<u8>,
}

#[derive(Clone)]
struct PortEditorRow {
    id: uuid::Uuid,
    row: gtk::ListBoxRow,
    enabled: gtk::Switch,
    direction: gtk::DropDown,
    protocol: gtk::DropDown,
    host_address: gtk::Entry,
    host_port: gtk::SpinButton,
    guest_address: gtk::Entry,
    guest_port: gtk::SpinButton,
}

struct PendingFrame {
    id: u64,
    submitted_monotonic_us: u64,
    metadata: FrameMetadata,
}

struct PendingPresentation {
    id: u64,
    submitted_monotonic_us: u64,
    frame_counter: i64,
    offloaded: bool,
    metadata: FrameMetadata,
}

struct FrameMetadata {
    geometry_generation: u64,
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    planes: u32,
    explicit_sync: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InitialSizeRequest {
    viewport_width: u32,
    viewport_height: u32,
    target_width: u32,
    target_height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InitialSizeDecision {
    AlreadySettled,
    AlreadyFailed,
    Settled,
    Waiting,
    Request {
        window_width: i32,
        window_height: i32,
    },
    TimedOut {
        target_width: u32,
        target_height: u32,
        viewport_width: u32,
        viewport_height: u32,
    },
}

#[derive(Debug)]
struct InitialMonitorSizing {
    configured_width: u32,
    configured_height: u32,
    started_at: Option<Instant>,
    settled: bool,
    failed: bool,
    last_request: Option<InitialSizeRequest>,
}

impl InitialMonitorSizing {
    fn new(configured_width: u32, configured_height: u32) -> Self {
        Self {
            configured_width,
            configured_height,
            started_at: None,
            settled: false,
            failed: false,
            last_request: None,
        }
    }

    fn begin(&mut self, now: Instant) {
        self.started_at.get_or_insert(now);
    }

    fn check_timeout(
        &mut self,
        now: Instant,
        viewport_width: u32,
        viewport_height: u32,
        scale_denominator: u32,
    ) -> Option<InitialSizeDecision> {
        if self.settled || self.failed {
            return None;
        }
        let started_at = self.started_at?;
        if now.saturating_duration_since(started_at) < INITIAL_MONITOR_SIZE_TIMEOUT {
            return None;
        }
        let target_width = align_extent_up(self.configured_width, scale_denominator)
            .unwrap_or(self.configured_width);
        let target_height = align_extent_up(self.configured_height, scale_denominator)
            .unwrap_or(self.configured_height);
        self.failed = true;
        self.last_request = None;
        Some(InitialSizeDecision::TimedOut {
            target_width,
            target_height,
            viewport_width,
            viewport_height,
        })
    }

    fn observe(
        &mut self,
        now: Instant,
        window_size: (i32, i32),
        viewport_size: (u32, u32),
        scale_denominator: u32,
        accept_compositor_granted_size: bool,
    ) -> InitialSizeDecision {
        let (window_width, window_height) = window_size;
        let (viewport_width, viewport_height) = viewport_size;
        if self.settled {
            return InitialSizeDecision::AlreadySettled;
        }
        if self.failed {
            return InitialSizeDecision::AlreadyFailed;
        }
        let target_width = align_extent_up(self.configured_width, scale_denominator)
            .unwrap_or(self.configured_width);
        let target_height = align_extent_up(self.configured_height, scale_denominator)
            .unwrap_or(self.configured_height);
        if viewport_width == target_width && viewport_height == target_height {
            self.settled = true;
            self.last_request = None;
            return InitialSizeDecision::Settled;
        }
        // A compositor may maximize or otherwise constrain a requested native
        // toplevel when the configured guest monitor plus host chrome cannot
        // fit in the work area.  The resulting child allocation is still the
        // real, pixel-aligned monitor; rejecting it would make a 1280x800 host
        // unable to boot a nominal 1280x800 machine merely because two logical
        // rows were consumed to align a 150% subsurface.  Never accept GTK's
        // tiny bootstrap allocation, and keep the exact configured-size
        // handshake for an unconstrained window.
        if accept_compositor_granted_size
            && viewport_width >= MIN_GUEST_MONITOR_WIDTH
            && viewport_height >= MIN_GUEST_MONITOR_HEIGHT
        {
            self.settled = true;
            self.last_request = None;
            return InitialSizeDecision::Settled;
        }
        if let Some(timeout) =
            self.check_timeout(now, viewport_width, viewport_height, scale_denominator)
        {
            return timeout;
        }
        self.begin(now);

        let request = InitialSizeRequest {
            viewport_width,
            viewport_height,
            target_width,
            target_height,
        };
        // A Wayland resize request is asynchronous. At high refresh rates the
        // same stale allocation can be observed by many frame callbacks
        // before the compositor answers. Reissuing and counting every frame
        // can abandon the configured size before the first response arrives.
        if self.last_request == Some(request) {
            return InitialSizeDecision::Waiting;
        }
        self.last_request = Some(request);
        let (window_width, window_height) = corrected_window_size(
            window_width,
            window_height,
            viewport_width,
            viewport_height,
            target_width,
            target_height,
        );
        InitialSizeDecision::Request {
            window_width,
            window_height,
        }
    }

    fn failed(&self) -> bool {
        self.failed
    }

    fn pending(&self) -> bool {
        !self.settled && !self.failed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CursorFingerprint {
    width: u32,
    height: u32,
    hotspot_x: i32,
    hotspot_y: i32,
    content: u64,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
struct OffloadGeometryDiagnostics {
    schema: u32,
    scale_120: u32,
    scale_denominator: u32,
    surface_transform_x: f64,
    surface_transform_y: f64,
    wrapper_x: f64,
    wrapper_y: f64,
    wrapper_width: i32,
    wrapper_height: i32,
    margin_start: i32,
    margin_end: i32,
    margin_top: i32,
    margin_bottom: i32,
    child_origin_x: f64,
    child_origin_y: f64,
    child_width: i32,
    child_height: i32,
    logical_origin_integral: bool,
    device_origin_integral: bool,
    device_extent_integral: bool,
    allocation_settled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorContent {
    Placeholder,
    Frame,
    Blank,
}

#[derive(serde::Serialize)]
struct MonitorContinuityDiagnostics {
    schema: u32,
    stable_paintable_identity: bool,
    paintable_identity_changes: u64,
    frames_installed: u64,
    frame_replacements: u64,
    attachments: u64,
    detachments: u64,
    observations: u64,
    cursor_observations: u64,
    placeholder_exposures_between_frames: u64,
    blank_exposures_between_frames: u64,
    attachment_active: bool,
    current_frame_id: Option<u64>,
    placeholder_visible: bool,
    frame_available: bool,
    last_observation: &'static str,
    last_observation_monotonic_us: u64,
    last_violation: Option<String>,
    #[serde(skip)]
    last_content: MonitorContent,
}

impl Default for MonitorContinuityDiagnostics {
    fn default() -> Self {
        Self {
            schema: 1,
            stable_paintable_identity: true,
            paintable_identity_changes: 0,
            frames_installed: 0,
            frame_replacements: 0,
            attachments: 0,
            detachments: 0,
            observations: 0,
            cursor_observations: 0,
            placeholder_exposures_between_frames: 0,
            blank_exposures_between_frames: 0,
            attachment_active: false,
            current_frame_id: None,
            placeholder_visible: true,
            frame_available: false,
            last_observation: "constructed",
            last_observation_monotonic_us: 0,
            last_violation: None,
            last_content: MonitorContent::Placeholder,
        }
    }
}

impl MonitorContinuityDiagnostics {
    fn record_frame_installed(&mut self, id: u64) {
        self.frames_installed = self.frames_installed.saturating_add(1);
        if self.attachment_active {
            self.frame_replacements = self.frame_replacements.saturating_add(1);
        } else {
            self.attachments = self.attachments.saturating_add(1);
        }
        self.attachment_active = true;
        self.current_frame_id = Some(id);
        // A newly attached interval promises frame content immediately. Seed
        // the expected content so an initial placeholder/blank observation is
        // counted as an exposure, not mistaken for pre-attachment state.
        self.last_content = MonitorContent::Frame;
    }

    fn detach(&mut self, source: &'static str) {
        if self.attachment_active {
            self.detachments = self.detachments.saturating_add(1);
        }
        self.attachment_active = false;
        self.current_frame_id = None;
        self.last_observation = source;
        self.last_observation_monotonic_us = monotonic_us();
    }

    fn paintable_identity_changed(&mut self) {
        self.paintable_identity_changes = self.paintable_identity_changes.saturating_add(1);
        self.stable_paintable_identity = false;
        self.last_violation =
            Some("GtkPicture paintable identity changed after construction".into());
    }

    fn observe(
        &mut self,
        source: &'static str,
        placeholder_visible: bool,
        frame_available: bool,
    ) -> bool {
        self.observations = self.observations.saturating_add(1);
        if source == "cursor" {
            self.cursor_observations = self.cursor_observations.saturating_add(1);
        }
        self.placeholder_visible = placeholder_visible;
        self.frame_available = frame_available;
        self.last_observation = source;
        self.last_observation_monotonic_us = monotonic_us();
        let content = if placeholder_visible {
            MonitorContent::Placeholder
        } else if frame_available {
            MonitorContent::Frame
        } else {
            MonitorContent::Blank
        };
        let mut violation = false;
        if self.attachment_active && content != MonitorContent::Frame {
            if content != self.last_content {
                match content {
                    MonitorContent::Placeholder => {
                        self.placeholder_exposures_between_frames =
                            self.placeholder_exposures_between_frames.saturating_add(1);
                    }
                    MonitorContent::Blank => {
                        self.blank_exposures_between_frames =
                            self.blank_exposures_between_frames.saturating_add(1);
                    }
                    MonitorContent::Frame => {}
                }
            }
            self.last_violation = Some(format!(
                "{source} observed {content:?} while frame {} was attached",
                self.current_frame_id.unwrap_or_default()
            ));
            violation = true;
        }
        self.last_content = content;
        violation
    }
}

struct GatewayConnectionNotifier(std::os::unix::net::UnixStream);

impl GatewayConnectionNotifier {
    fn fd(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.0.as_raw_fd()
    }

    fn drain(&self) {
        use std::io::Read;
        let mut stream = &self.0;
        let mut bytes = [0_u8; 256];
        loop {
            match stream.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }
}

impl NativeWindow {
    fn build(
        application: &gtk::Application,
        launch: Launch,
        connection: GatewayConnection,
    ) -> Result<Rc<Self>> {
        let obsolete_click_state = launch.output_state_dir.join("pointer-click.json");
        if let Err(error) = fs::remove_file(&obsolete_click_state)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error).with_context(|| {
                format!(
                    "removing obsolete guest-readable click state {}",
                    obsolete_click_state.display()
                )
            });
        }
        let initial_guest_scale_preset = GuestScalePreset::from_scale_120(launch.guest_scale_120)
            .context("validated guest scale has no typed preset")?;
        let initial_guest_ui_scale_120 = initial_guest_scale_preset.resolve(120);
        let initial_monitor_sizing =
            InitialMonitorSizing::new(launch.initial_width, launch.initial_height);
        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .title(&launch.title)
            .icon_name("buzzardos")
            .default_width(launch.initial_width as i32)
            .default_height(launch.initial_height as i32)
            .resizable(true)
            .decorated(true)
            .build();
        window.set_size_request(360, 320);

        let header = gtk::HeaderBar::builder().show_title_buttons(true).build();
        window.set_titlebar(Some(&header));

        let header_controls = build_header_controls();
        header.pack_start(&header_controls);

        let status_label = gtk::Label::new(Some(MonitorState::Starting.label()));
        status_label.add_css_class("caption");
        status_label.add_css_class("warning");
        status_label.set_tooltip_text(Some("Machine lifecycle state"));
        header.pack_end(&status_label);

        // Keep an explicitly painted, permanently black parent surface below
        // the dmabuf subsurface.  GraphicsOffload is presented by GTK as a
        // child Wayland subsurface; the host compositor may momentarily omit
        // that subsurface while its cursor/input or geometry state changes.
        // The parent pixels exposed in that interval must be black, never the
        // cached Starting/Failed lifecycle page.
        let monitor_view = gtk::Overlay::builder().hexpand(true).vexpand(true).build();
        // The configured size describes the embedded monitor, not the outer
        // toplevel. Prime GTK's layout with a temporary child minimum so the
        // header bar and client-side frame are added outside that monitor.
        // The request is removed after the compositor grants the first exact
        // allocation, preserving unrestricted user resizing thereafter.
        monitor_view.set_size_request(launch.initial_width as i32, launch.initial_height as i32);
        monitor_view.set_direction(gtk::TextDirection::Ltr);
        let monitor_backing = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        monitor_backing.set_draw_func(|_, cairo, width, height| {
            cairo.set_source_rgb(0.0, 0.0, 0.0);
            cairo.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
            let _ = cairo.fill();
        });

        let spinner = gtk::Spinner::new();
        spinner.set_spinning(true);
        spinner.set_size_request(36, 36);
        let state_title = gtk::Label::new(Some("Starting machine"));
        state_title.add_css_class("title-2");
        let detail_label = gtk::Label::new(Some("Waiting for the guest display"));
        detail_label.add_css_class("dim-label");
        detail_label.set_wrap(true);
        detail_label.set_justify(gtk::Justification::Center);
        detail_label.set_max_width_chars(72);
        let state_overlay = gtk::Box::new(gtk::Orientation::Vertical, 12);
        // Keep lifecycle information as a compact dark monitor OSD.  The
        // generic `view` class painted a glaring content-sized white panel on
        // light themes, while applying any background to `monitor_view`
        // itself can disturb the exact offload allocation.
        state_overlay.add_css_class("osd");
        state_overlay.set_margin_start(24);
        state_overlay.set_margin_end(24);
        state_overlay.set_margin_top(24);
        state_overlay.set_margin_bottom(24);
        state_overlay.set_halign(gtk::Align::Center);
        state_overlay.set_valign(gtk::Align::Center);
        state_overlay.append(&spinner);
        state_overlay.append(&state_title);
        state_overlay.append(&detail_label);

        // The picture receives dmabuf textures from the display server. It is
        // kept opaque, rectangular, unclipped, and unfiltered so GTK can place
        // it on a Wayland subsurface instead of sending it through GSK.
        let frame_paintable = FramePaintable::new();
        let picture = gtk::Picture::builder()
            .hexpand(true)
            .vexpand(true)
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Fill)
            .paintable(&frame_paintable)
            .alternative_text("Guest machine display")
            .build();
        picture.set_focusable(true);
        picture.set_can_target(true);
        let offload = gtk::GraphicsOffload::new(Some(&picture));
        offload.set_direction(gtk::TextDirection::Ltr);
        offload.set_enabled(gtk::GraphicsOffloadEnabled::Enabled);
        offload.set_hexpand(true);
        offload.set_vexpand(true);
        monitor_view.set_child(Some(&monitor_backing));
        monitor_view.add_overlay(&offload);
        monitor_view.add_overlay(&state_overlay);

        window.set_child(Some(&monitor_view));

        let native = Rc::new(Self {
            application: application.clone(),
            launch,
            events: RefCell::new(connection.events),
            event_notify: GatewayConnectionNotifier(connection.event_notify),
            commands: connection.commands,
            window,
            status_label,
            state_title,
            detail_label,
            spinner,
            monitor_view,
            state_overlay,
            picture,
            frame_paintable,
            offload,
            state: Cell::new(MonitorState::Starting),
            close_requested: Cell::new(false),
            clipboard_busy: Cell::new(false),
            clipboard_epoch: Cell::new(1),
            clipboard_connection: RefCell::new(None),
            viewport_width: Cell::new(1),
            viewport_height: Cell::new(1),
            host_surface_scale_120: Cell::new(120),
            guest_ui_scale_120: Cell::new(initial_guest_ui_scale_120),
            guest_scale_preset: Cell::new(initial_guest_scale_preset),
            geometry_generation: Cell::new(1),
            refresh_mhz: Cell::new(0),
            initial_monitor_sizing: RefCell::new(initial_monitor_sizing),
            gateway_configured: Cell::new(false),
            failure: RefCell::new(None),
            last_runtime_check: Cell::new(Instant::now() - RUNTIME_POLL_INTERVAL),
            last_host_frame_tick: Cell::new(Instant::now()),
            last_background_frame_tick: Cell::new(Instant::now()),
            pending_frame: RefCell::new(None),
            pending_presentations: RefCell::new(VecDeque::new()),
            presentation: RefCell::new(PresentationDiagnostics {
                presentation_feedback: true,
                viewport_width: 1,
                viewport_height: 1,
                ..PresentationDiagnostics::default()
            }),
            presentation_dirty: Cell::new(false),
            offload_geometry: RefCell::new(OffloadGeometryDiagnostics::default()),
            continuity: RefCell::new(MonitorContinuityDiagnostics::default()),
            pressed_pointer_buttons: RefCell::new(BTreeSet::new()),
            last_cursor: RefCell::new(None),
            cursor_state: Cell::new(0),
        });

        native.install_actions();
        native.install_handlers();
        native.update_state_ui();
        native.save_window()?;
        native.save_output_state()?;
        native.save_presentation()?;
        Ok(native)
    }

    fn present(self: &Rc<Self>) {
        // This is the first point at which GTK's configured child minimum and
        // toplevel default size become a concrete request to the compositor.
        self.initial_monitor_sizing
            .borrow_mut()
            .begin(Instant::now());
        self.window.present();
    }

    fn install_handlers(self: &Rc<Self>) {
        let this = Rc::clone(self);
        self.window.connect_close_request(move |_| {
            this.request_close();
            glib::Propagation::Stop
        });

        let this = Rc::clone(self);
        self.window.connect_maximized_notify(move |_| {
            if let Err(error) = this.save_window() {
                eprintln!("buzzardos-display: saving maximize state: {error:#}");
            }
        });

        // GTK may retain focus on the Picture while the native toplevel loses
        // activation (for example while the host compositor handles a system
        // shortcut). In that case EventControllerFocus emits no leave and a
        // physical modifier release can be consumed by the host, leaving the
        // parent wl_keyboard state latched in Sway. End the physical seat0
        // focus epoch on deactivation so GuestState releases every held key
        // through the active guest XKB map. Numbered CUA keyboards use their
        // own Wayland connections/seats and never enter this path.
        //
        // The same boundary releases pointer buttons so a Sway move/resize
        // operation cannot remain latched either.
        let this = Rc::clone(self);
        self.window.connect_is_active_notify(move |window| {
            if !window.is_active() {
                if let Some(toplevel) = this.gdk_toplevel() {
                    toplevel.restore_system_shortcuts();
                }
                this.release_pressed_pointer_buttons();
                this.send_guest_input(GatewayCommand::KeyboardLeave);
            } else if this.picture.has_focus() {
                this.send_guest_input(GatewayCommand::KeyboardEnter);
            }
        });

        let this = Rc::clone(self);
        self.picture.connect_paintable_notify(move |_| {
            this.continuity.borrow_mut().paintable_identity_changed();
            this.observe_monitor_continuity("paintable-identity");
        });

        // GDK's fractional `scale` property is the frontend's authoritative
        // view of the Wayland preferred-scale protocol state. A surface can
        // change scale while its logical allocation stays unchanged (for
        // example when moved between monitors), so do not wait for a resize
        // or infer a scale from captured pixels.
        let this = Rc::clone(self);
        self.window.connect_realize(move |_| {
            let Some(surface) = this.window.surface() else {
                return;
            };
            this.reconcile_monitor_allocation();
            let scale_this = Rc::clone(&this);
            surface.connect_scale_notify(move |_| {
                scale_this.reconcile_monitor_allocation();
            });
        });

        let this = Rc::clone(self);
        self.window.connect_map(move |_| {
            let Some(toplevel) = this.gdk_toplevel() else {
                return;
            };
            let state_this = Rc::clone(&this);
            toplevel.connect_state_notify(move |_| {
                if let Err(error) = state_this.save_window() {
                    eprintln!("buzzardos-display: saving native toplevel state: {error:#}");
                }
            });

            if let Some(clock) = this.window.frame_clock() {
                let this = Rc::clone(&this);
                clock.connect_after_paint(move |clock| this.after_paint(clock));
            }
        });

        let this = Rc::clone(self);
        self.monitor_view
            .add_tick_callback(move |_widget, frame_clock| {
                this.last_host_frame_tick.set(Instant::now());
                this.reconcile_monitor_allocation();
                this.finish_presentation_feedback(frame_clock);
                // Return the actual host Wayland frame clock to the nested
                // compositor. This completes parent wl_surface.frame requests
                // after idle without generating, copying, or streaming a
                // guest image. A stopped gateway is already surfaced through
                // its lifecycle channel, so shutdown send failures are benign.
                let _ = this.commands.send(GatewayCommand::FrameTick {
                    frame_time_us: frame_clock.frame_time(),
                });
                glib::ControlFlow::Continue
            });

        // A host compositor is allowed to stop GTK frame callbacks when the
        // application is minimized, occluded, or on another workspace. The
        // guest monitor must nevertheless remain a live scanout for in-guest
        // CUA, screenshots, animations, and applications. This timer advances
        // that same output only after the real host frame clock has gone
        // silent. It consumes the newest dmabuf and discards its presentation
        // feedback honestly; it never invents a host vblank or creates a
        // second display/stream.
        self.schedule_background_frame_clock();

        let this = Rc::clone(self);
        glib::source::unix_fd_add_local(
            self.event_notify.fd(),
            glib::IOCondition::IN | glib::IOCondition::HUP | glib::IOCondition::ERR,
            move |_, _| {
                this.event_notify.drain();
                this.poll();
                glib::ControlFlow::Continue
            },
        );

        let this = Rc::clone(self);
        glib::timeout_add_local(RUNTIME_POLL_INTERVAL, move || {
            // Tick callbacks may stop for an occluded or minimized native
            // window. Continue driving only the bootstrap sizing handshake so
            // an ignored/clamped request still reaches its wall-clock failure
            // instead of leaving startup blocked forever.
            if this.initial_monitor_sizing.borrow().pending() {
                this.reconcile_monitor_allocation();
            }
            this.poll();
            glib::ControlFlow::Continue
        });

        self.install_input_handlers();
    }

    /// Returns true only when an allocation is safe to publish as the guest
    /// monitor. In particular, the bootstrap allocation never becomes a
    /// transient guest mode or satisfies machine readiness.
    fn settle_initial_monitor_size(&self, viewport_width: u32, viewport_height: u32) -> bool {
        let denominator = self.initial_monitor_scale_denominator();
        let decision = self.initial_monitor_sizing.borrow_mut().observe(
            Instant::now(),
            (self.window.width().max(1), self.window.height().max(1)),
            (viewport_width, viewport_height),
            denominator,
            self.window.is_maximized() || self.window.is_fullscreen(),
        );
        match decision {
            InitialSizeDecision::AlreadySettled => true,
            InitialSizeDecision::AlreadyFailed => false,
            InitialSizeDecision::Settled => {
                self.monitor_view.set_size_request(-1, -1);
                true
            }
            InitialSizeDecision::Waiting => false,
            InitialSizeDecision::Request {
                window_width,
                window_height,
            } => {
                self.window.set_default_size(window_width, window_height);
                false
            }
            timeout @ InitialSizeDecision::TimedOut { .. } => {
                self.report_initial_monitor_timeout(
                    timeout,
                    "the host allocation remained different from the requested native viewport",
                );
                false
            }
        }
    }

    fn initial_monitor_scale_denominator(&self) -> u32 {
        let scale_120 = self
            .window
            .surface()
            .and_then(|surface| {
                effective_scale_120(self.launch.test_fractional_scale_120, surface.scale()).ok()
            })
            .unwrap_or_else(|| self.host_surface_scale_120.get());
        scale_denominator(scale_120).unwrap_or(1)
    }

    fn expire_initial_monitor_size(
        &self,
        viewport_width: u32,
        viewport_height: u32,
        reason: &'static str,
    ) {
        let denominator = self.initial_monitor_scale_denominator();
        let decision = self.initial_monitor_sizing.borrow_mut().check_timeout(
            Instant::now(),
            viewport_width,
            viewport_height,
            denominator,
        );
        if let Some(timeout) = decision {
            self.report_initial_monitor_timeout(timeout, reason);
        }
    }

    fn report_initial_monitor_timeout(&self, timeout: InitialSizeDecision, reason: &'static str) {
        let InitialSizeDecision::TimedOut {
            target_width,
            target_height,
            viewport_width,
            viewport_height,
        } = timeout
        else {
            return;
        };
        let message = format!(
            "host compositor did not grant a stable native {target_width}x{target_height} initial \
             monitor within {} seconds (last child allocation \
             {viewport_width}x{viewport_height}; {reason}); refusing to start the guest at a \
             reduced or resampled resolution",
            INITIAL_MONITOR_SIZE_TIMEOUT.as_secs()
        );
        eprintln!("buzzardos-display: {message}");
        *self.failure.borrow_mut() = Some(message);
        self.set_state(MonitorState::Failed);
    }

    fn update_allocated_viewport(&self) {
        let width = self.picture.width().max(1) as u32;
        let height = self.picture.height().max(1) as u32;
        if !self.offload_geometry.borrow().allocation_settled {
            self.expire_initial_monitor_size(
                width,
                height,
                "the offload child never settled on an integral native rectangle",
            );
            return;
        }
        if self.settle_initial_monitor_size(width, height) {
            self.update_viewport(width, height);
        }
    }

    fn reconcile_monitor_allocation(&self) {
        match self.align_monitor_offload() {
            Ok(true) => {
                self.reset_offload_claim();
                // Margin changes queue a new allocation. Do not publish the
                // pre-alignment child size as a guest output mode.
                self.expire_initial_monitor_size(
                    self.picture.width().max(1) as u32,
                    self.picture.height().max(1) as u32,
                    "the offload child geometry kept changing before a native rectangle settled",
                );
            }
            Ok(false) => {
                self.reset_offload_claim();
                self.update_allocated_viewport();
            }
            Err(error) => {
                *self.failure.borrow_mut() = Some(error.to_string());
                self.set_state(MonitorState::Failed);
            }
        }
    }

    fn install_input_handlers(self: &Rc<Self>) {
        let motion = gtk::EventControllerMotion::new();
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        motion.connect_enter(move |_, x, y| {
            let (x, y) = this.to_guest_surface(x, y);
            this.send_guest_input(GatewayCommand::PointerEnter {
                x,
                y,
                geometry_generation: this.geometry_generation.get(),
            });
        });
        let this = Rc::clone(self);
        motion.connect_motion(move |_, x, y| {
            let (x, y) = this.to_guest_surface(x, y);
            this.send_guest_input(GatewayCommand::PointerMotion {
                x,
                y,
                geometry_generation: this.geometry_generation.get(),
            });
        });
        let this = Rc::clone(self);
        motion.connect_leave(move |_| {
            this.release_pressed_pointer_buttons();
            this.send_guest_input(GatewayCommand::PointerLeave);
        });
        self.picture.add_controller(motion);

        // GestureClick deliberately stops recognizing a sequence once motion
        // exceeds GTK's drag threshold.  It is therefore unsuitable for
        // transporting raw pointer buttons to a compositor: treating
        // `stopped` as a release truncates every Sway move/resize to a few
        // pixels, while ignoring it can lose the eventual release and latch a
        // compositor seat operation.  EventControllerLegacy exposes the
        // underlying GDK button events, including the release delivered by
        // the implicit pointer grab, without imposing click semantics.
        let buttons = gtk::EventControllerLegacy::new();
        buttons.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        buttons.connect_event(move |_, event| {
            let pressed = match event.event_type() {
                gdk::EventType::ButtonPress => true,
                gdk::EventType::ButtonRelease => false,
                _ => return glib::Propagation::Proceed,
            };
            let Some(event) = event.downcast_ref::<gdk::ButtonEvent>() else {
                return glib::Propagation::Proceed;
            };
            let Some(button) = linux_pointer_button(event.button()) else {
                return glib::Propagation::Proceed;
            };

            if pressed {
                if let Some(toplevel) = this.gdk_toplevel() {
                    toplevel.inhibit_system_shortcuts(Some(event.as_ref()));
                }
                this.picture.grab_focus();
                if this.pressed_pointer_buttons.borrow_mut().insert(button) {
                    this.send_guest_input(GatewayCommand::PointerButton {
                        button,
                        pressed: true,
                        geometry_generation: this.geometry_generation.get(),
                    });
                }
            } else if this.pressed_pointer_buttons.borrow_mut().remove(&button) {
                this.send_guest_input(GatewayCommand::PointerButton {
                    button,
                    pressed: false,
                    geometry_generation: this.geometry_generation.get(),
                });
            }

            glib::Propagation::Stop
        });
        self.picture.add_controller(buttons);

        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
        );
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        scroll.connect_scroll(move |_, horizontal, vertical| {
            this.send_guest_input(GatewayCommand::PointerAxis {
                horizontal,
                vertical,
                geometry_generation: this.geometry_generation.get(),
            });
            glib::Propagation::Stop
        });
        self.picture.add_controller(scroll);

        let focus = gtk::EventControllerFocus::new();
        focus.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        focus.connect_enter(move |_| {
            this.send_guest_input(GatewayCommand::KeyboardEnter);
        });
        let this = Rc::clone(self);
        focus.connect_leave(move |_| {
            if let Some(toplevel) = this.gdk_toplevel() {
                toplevel.restore_system_shortcuts();
            }
            this.release_pressed_pointer_buttons();
            this.send_guest_input(GatewayCommand::KeyboardLeave);
        });
        self.picture.add_controller(focus);

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        keys.connect_key_pressed(move |_, _, keycode, modifiers| {
            this.send_guest_input(GatewayCommand::KeyboardKey {
                key: keycode.saturating_sub(8),
                pressed: true,
                modifiers: xkb_modifiers(modifiers),
            });
            glib::Propagation::Stop
        });
        let this = Rc::clone(self);
        keys.connect_key_released(move |_, _, keycode, modifiers| {
            this.send_guest_input(GatewayCommand::KeyboardKey {
                key: keycode.saturating_sub(8),
                pressed: false,
                modifiers: xkb_modifiers(modifiers),
            });
        });
        self.picture.add_controller(keys);
    }

    fn release_pressed_pointer_buttons(&self) {
        let buttons = std::mem::take(&mut *self.pressed_pointer_buttons.borrow_mut());
        for button in buttons {
            self.send_guest_input(GatewayCommand::PointerButton {
                button,
                pressed: false,
                geometry_generation: self.geometry_generation.get(),
            });
        }
    }

    fn to_guest_surface(&self, x: f64, y: f64) -> (f64, f64) {
        let mode = self.output_mode();
        (
            map_monitor_coordinate(x, self.viewport_width.get(), mode.physical_width),
            map_monitor_coordinate(y, self.viewport_height.get(), mode.physical_height),
        )
    }

    fn send_guest_input(&self, command: GatewayCommand) {
        let stale_geometry = match &command {
            GatewayCommand::PointerEnter {
                geometry_generation,
                ..
            }
            | GatewayCommand::PointerMotion {
                geometry_generation,
                ..
            }
            | GatewayCommand::PointerButton {
                geometry_generation,
                ..
            }
            | GatewayCommand::PointerAxis {
                geometry_generation,
                ..
            } => *geometry_generation != self.geometry_generation.get(),
            _ => false,
        };
        if stale_geometry {
            return;
        }
        if self.state.get() != MonitorState::Running {
            return;
        }
        if let Err(error) = self.commands.send(command) {
            eprintln!("buzzardos-display: forwarding guest input: {error:#}");
        }
    }

    fn install_actions(self: &Rc<Self>) {
        self.add_action("start", {
            let this = Rc::clone(self);
            move || this.request_start()
        });
        self.add_action("stop", {
            let this = Rc::clone(self);
            move || this.request_stop(false)
        });
        self.add_action("restart", {
            let this = Rc::clone(self);
            move || this.request_stop(true)
        });
        self.add_action("shutdown", {
            let this = Rc::clone(self);
            move || this.request_stop(false)
        });
        self.add_action("close", {
            let this = Rc::clone(self);
            move || this.request_close()
        });
        self.add_action("settings", {
            let this = Rc::clone(self);
            move || this.open_settings()
        });
        self.add_action("ports", {
            let this = Rc::clone(self);
            move || this.open_ports()
        });
        self.add_action("media", {
            let this = Rc::clone(self);
            move || this.open_media()
        });
        self.add_action("clipboard-to-guest", {
            let this = Rc::clone(self);
            move || this.send_host_clipboard_to_guest()
        });
        self.add_action("clipboard-to-host", {
            let this = Rc::clone(self);
            move || this.copy_guest_clipboard_to_host()
        });
        self.add_action("diagnostics", {
            let this = Rc::clone(self);
            move || this.open_diagnostics()
        });

        self.application
            .set_accels_for_action("app.close", &["<Primary>q"]);
        self.application
            .set_accels_for_action("app.settings", &["<Primary>comma"]);
        self.application
            .set_accels_for_action("app.restart", &["<Primary><Shift>r"]);
        self.update_clipboard_action_state();
    }

    fn add_action(self: &Rc<Self>, name: &str, callback: impl Fn() + 'static) {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| callback());
        self.application.add_action(&action);
    }

    fn poll(&self) {
        while let Ok(event) = self.events.borrow_mut().try_recv() {
            match event {
                GatewayEvent::HostCommand(command) => self.apply_host_command(command),
                GatewayEvent::GuestConnected => {
                    self.detach_monitor("guest-connected");
                    self.failure.borrow_mut().take();
                    self.set_state(MonitorState::Starting);
                }
                GatewayEvent::GuestDisconnected => {
                    self.detach_monitor("guest-disconnected");
                    if self.state.get() != MonitorState::Failed {
                        self.set_state(MonitorState::Stopped);
                    }
                }
                GatewayEvent::GuestFailed(error) => {
                    self.detach_monitor("guest-failed");
                    *self.failure.borrow_mut() = Some(error);
                    self.set_state(MonitorState::Failed);
                }
                GatewayEvent::GuestFrame(frame) => {
                    if let Err(error) = self.install_frame(frame) {
                        *self.failure.borrow_mut() = Some(format!("{error:#}"));
                        self.set_state(MonitorState::Failed);
                    }
                }
                GatewayEvent::GuestCursor(cursor) => {
                    self.install_cursor(cursor);
                    self.observe_monitor_continuity("cursor");
                }
                GatewayEvent::GuestCursorFallback => {
                    self.fallback_cursor();
                    self.observe_monitor_continuity("cursor");
                }
                GatewayEvent::GuestCursorHidden => {
                    self.hide_cursor();
                    self.observe_monitor_continuity("cursor");
                }
                GatewayEvent::FrameReleased { id, held_us } => {
                    let mut stats = self.presentation.borrow_mut();
                    stats.released_frames = stats.released_frames.saturating_add(1);
                    stats.last_released_frame_id = id;
                    stats.last_buffer_residency_us = held_us;
                    stats.maximum_buffer_residency_us =
                        stats.maximum_buffer_residency_us.max(held_us);
                    drop(stats);
                    self.presentation_dirty.set(true);
                }
                GatewayEvent::GuestScaleRequest { request, reply } => {
                    let _ = reply.send(self.apply_guest_scale_request(request));
                }
            }
        }

        if self.last_runtime_check.get().elapsed() >= RUNTIME_POLL_INTERVAL {
            self.last_runtime_check.set(Instant::now());
            self.refresh_runtime_state();
            if self.presentation_dirty.replace(false)
                && let Err(error) = self.save_presentation()
            {
                self.presentation_dirty.set(true);
                eprintln!("buzzardos-display: saving presentation diagnostics: {error:#}");
            }
        }
        // The guest agent becomes ready after the machine reaches Running, so
        // refresh independently of lifecycle-state transitions.
        self.update_clipboard_action_state();
    }

    fn refresh_runtime_state(&self) {
        let Ok(Some(runtime)) = RuntimeState::load(&self.launch.machine_dir) else {
            return;
        };
        let state = if self.initial_monitor_sizing.borrow().failed() {
            MonitorState::Failed
        } else {
            match runtime.state {
                MachineState::Starting => MonitorState::Starting,
                MachineState::Running => MonitorState::Running,
                MachineState::Stopping => MonitorState::Stopping,
                MachineState::Stopped => MonitorState::Stopped,
                MachineState::Failed => MonitorState::Failed,
            }
        };
        if state == MonitorState::Failed
            && self.failure.borrow().is_none()
            && let Some(detail) = runtime
                .detail
                .as_deref()
                .filter(|detail| !detail.is_empty())
        {
            *self.failure.borrow_mut() = Some(detail.to_owned());
        }
        if matches!(state, MonitorState::Stopped | MonitorState::Failed) {
            self.detach_monitor("runtime-terminal-state");
        }
        if state != self.state.get() {
            self.set_state(state);
        }
        let microphone_active = runtime
            .integrations
            .as_ref()
            .is_some_and(|integrations| integrations.host_microphone.active);
        let camera_active = runtime
            .integrations
            .as_ref()
            .is_some_and(|integrations| integrations.host_camera.active);
        self.update_header_status(microphone_active, camera_active);
        if self.close_requested.get()
            && matches!(runtime.state, MachineState::Stopped | MachineState::Failed)
        {
            self.application.quit();
        }
    }

    fn install_frame(&self, frame: DmabufFrame) -> Result<()> {
        let DmabufFrame {
            id,
            geometry_generation,
            width,
            height,
            fourcc,
            modifier,
            planes,
            submitted_monotonic_us,
            explicit_sync,
            acquire_wait_us,
        } = frame;
        if geometry_generation != self.geometry_generation.get() {
            self.release_rejected_frame(id)?;
            let mut stats = self.presentation.borrow_mut();
            stats.dropped_frames = stats.dropped_frames.saturating_add(1);
            drop(stats);
            self.presentation_dirty.set(true);
            return Ok(());
        }
        if planes.is_empty() || planes.len() > 4 {
            self.release_rejected_frame(id)?;
            anyhow::bail!("guest dmabuf frame has {} planes", planes.len());
        }
        let plane_count = planes.len() as u32;
        let metadata = FrameMetadata {
            geometry_generation,
            width,
            height,
            fourcc,
            modifier,
            planes: plane_count,
            explicit_sync,
        };

        let display = gtk::prelude::WidgetExt::display(&self.window);
        let mut builder = gdk::DmabufTextureBuilder::new()
            .set_display(&display)
            .set_width(width)
            .set_height(height)
            .set_fourcc(fourcc)
            .set_modifier(modifier)
            .set_n_planes(planes.len() as u32)
            .set_premultiplied(true);
        for (index, plane) in planes.iter().enumerate() {
            // SAFETY: every descriptor remains owned by `planes`, which is
            // captured by the texture release callback below.
            builder = unsafe { builder.set_fd(index as u32, plane.fd.as_raw_fd()) }
                .set_offset(index as u32, plane.offset)
                .set_stride(index as u32, plane.stride);
        }

        let release_commands = self.commands.clone();
        let rejected_commands = self.commands.clone();
        let texture = match unsafe {
            builder.build_with_release_func(move || {
                drop(planes);
                let _ = release_commands.send(GatewayCommand::ReleaseFrame {
                    id,
                    released_monotonic_us: monotonic_us(),
                });
            })
        } {
            Ok(texture) => texture,
            Err(error) => {
                rejected_commands.send(GatewayCommand::ReleaseFrame {
                    id,
                    released_monotonic_us: monotonic_us(),
                })?;
                return Err(error).context("GTK rejected the guest dmabuf");
            }
        };

        self.reset_offload_claim();
        self.frame_paintable.set_texture(&texture);
        self.continuity.borrow_mut().record_frame_installed(id);
        self.update_state_ui();
        let superseded = self
            .pending_frame
            .replace(Some(PendingFrame {
                id,
                submitted_monotonic_us,
                metadata,
            }))
            .is_some();
        {
            let mut stats = self.presentation.borrow_mut();
            stats.submitted_frames = stats.submitted_frames.saturating_add(1);
            if superseded {
                // This is the only event currently called a dropped frame:
                // GTK received a newer guest buffer before it painted the
                // prior one. Quiet/idle intervals are never inferred drops.
                stats.superseded_before_paint = stats.superseded_before_paint.saturating_add(1);
                stats.dropped_frames = stats.dropped_frames.saturating_add(1);
            }
            if explicit_sync {
                stats.explicit_sync_frames = stats.explicit_sync_frames.saturating_add(1);
                stats.last_acquire_wait_us = acquire_wait_us;
                stats.maximum_acquire_wait_us = stats.maximum_acquire_wait_us.max(acquire_wait_us);
            }
        }
        self.picture.queue_draw();
        self.observe_monitor_continuity("frame-installed");
        self.presentation_dirty.set(true);
        Ok(())
    }

    fn install_cursor(&self, cursor: CursorImage) {
        let content = match &cursor.storage {
            CursorStorage::Shm { stride, pixels } => {
                let mut hasher = DefaultHasher::new();
                stride.hash(&mut hasher);
                pixels.hash(&mut hasher);
                hasher.finish()
            }
            // Every dmabuf cursor id owns a distinct guest buffer lease. GTK
            // must import it even if its geometry matches the prior cursor.
            CursorStorage::Dmabuf { .. } => cursor.id,
        };
        let fingerprint = CursorFingerprint {
            width: cursor.width,
            height: cursor.height,
            hotspot_x: cursor.hotspot_x,
            hotspot_y: cursor.hotspot_y,
            content,
        };
        if self.cursor_state.get() == 1
            && self.last_cursor.borrow().as_ref() == Some(&fingerprint)
            && matches!(&cursor.storage, CursorStorage::Shm { .. })
        {
            return;
        }
        let CursorImage {
            id,
            width,
            height,
            hotspot_x,
            hotspot_y,
            storage,
        } = cursor;
        let gdk_cursor = match storage {
            CursorStorage::Shm { stride, pixels } => {
                let bytes = glib::Bytes::from_owned(pixels);
                let texture = gdk::MemoryTexture::new(
                    width as i32,
                    height as i32,
                    gdk::MemoryFormat::B8g8r8a8Premultiplied,
                    &bytes,
                    stride,
                );
                gdk::Cursor::from_texture(&texture, hotspot_x, hotspot_y, None)
            }
            CursorStorage::Dmabuf {
                fourcc,
                modifier,
                planes,
            } => {
                let display = gtk::prelude::WidgetExt::display(&self.window);
                let mut builder = gdk::DmabufTextureBuilder::new()
                    .set_display(&display)
                    .set_width(width)
                    .set_height(height)
                    .set_fourcc(fourcc)
                    .set_modifier(modifier)
                    .set_n_planes(planes.len() as u32)
                    .set_premultiplied(true);
                for (index, plane) in planes.iter().enumerate() {
                    // SAFETY: the owned descriptors remain captured by the
                    // release callback for the texture's complete lifetime.
                    builder = unsafe { builder.set_fd(index as u32, plane.fd.as_raw_fd()) }
                        .set_offset(index as u32, plane.offset)
                        .set_stride(index as u32, plane.stride);
                }
                let release_commands = self.commands.clone();
                let rejected_commands = self.commands.clone();
                let texture = match unsafe {
                    builder.build_with_release_func(move || {
                        drop(planes);
                        let _ = release_commands.send(GatewayCommand::ReleaseCursor { id });
                    })
                } {
                    Ok(texture) => texture,
                    Err(error) => {
                        let _ = rejected_commands.send(GatewayCommand::ReleaseCursor { id });
                        eprintln!(
                            "buzzardos-display: GTK rejected guest cursor dmabuf; \
                             using host default cursor: {error}"
                        );
                        self.fallback_cursor();
                        return;
                    }
                };
                gdk::Cursor::from_texture(&texture, hotspot_x, hotspot_y, None)
            }
        };
        *self.last_cursor.borrow_mut() = Some(fingerprint);
        self.picture.set_cursor(Some(&gdk_cursor));
        self.cursor_state.set(1);
    }

    fn fallback_cursor(&self) {
        self.last_cursor.borrow_mut().take();
        if self.cursor_state.replace(3) != 3 {
            self.picture.set_cursor_from_name(Some("default"));
        }
    }

    fn hide_cursor(&self) {
        if self.cursor_state.replace(2) != 2 {
            self.picture.set_cursor_from_name(Some("none"));
        }
    }

    fn release_rejected_frame(&self, id: u64) -> Result<()> {
        self.commands.send(GatewayCommand::ReleaseFrame {
            id,
            released_monotonic_us: monotonic_us(),
        })
    }

    fn after_paint(&self, clock: &gdk::FrameClock) {
        self.last_host_frame_tick.set(Instant::now());
        if let Some(frame) = self.pending_frame.borrow_mut().take() {
            // GTK's GraphicsOffload remains enabled, but Buzzard deliberately
            // does not enable or parse GDK debug logging. Without an explicit
            // protocol acknowledgement, do not claim that this frame was
            // offloaded or zero-copy.
            let offloaded = false;
            if let Err(error) = self.commands.send(GatewayCommand::FramePainted {
                id: frame.id,
                frame_time_us: clock.frame_time(),
            }) {
                *self.failure.borrow_mut() = Some(format!("{error:#}"));
                self.set_state(MonitorState::Failed);
                return;
            }
            self.pending_presentations
                .borrow_mut()
                .push_back(PendingPresentation {
                    id: frame.id,
                    submitted_monotonic_us: frame.submitted_monotonic_us,
                    frame_counter: clock.frame_counter(),
                    offloaded,
                    metadata: frame.metadata,
                });
            let mut stats = self.presentation.borrow_mut();
            stats.painted_frames = stats.painted_frames.saturating_add(1);
            stats.gtk_subsurface_offload = offloaded;
            stats.last_pacing_source = "host-vblank".into();
            self.presentation_dirty.set(true);
        }
        self.finish_presentation_feedback(clock);
        self.observe_monitor_continuity("after-paint");
        self.frame_paintable.release_retired();
    }

    fn schedule_background_frame_clock(self: &Rc<Self>) {
        let delay = refresh_interval(self.refresh_mhz.get());
        let this = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            if let Some(this) = this.upgrade() {
                this.background_frame_clock_tick();
                this.schedule_background_frame_clock();
            }
        });
    }

    fn background_frame_clock_tick(&self) {
        let now = Instant::now();
        let interval = refresh_interval(self.refresh_mhz.get());
        if now.duration_since(self.last_host_frame_tick.get()) < BACKGROUND_CLOCK_GRACE
            || now.duration_since(self.last_background_frame_tick.get()) < interval
        {
            return;
        }
        self.last_background_frame_tick.set(now);

        let frame_time_us = monotonic_us().min(i64::MAX as u64) as i64;
        if self
            .commands
            .send(GatewayCommand::FrameTick { frame_time_us })
            .is_err()
        {
            return;
        }

        let mut discarded = 0_u64;
        while let Some(frame) = self.pending_presentations.borrow_mut().pop_front() {
            if self
                .commands
                .send(GatewayCommand::FramePresented {
                    id: frame.id,
                    presentation_time_us: 0,
                    refresh_interval_us: 0,
                    sequence: 0,
                    offloaded: false,
                })
                .is_err()
            {
                return;
            }
            discarded = discarded.saturating_add(1);
        }

        let Some(frame) = self.pending_frame.borrow_mut().take() else {
            if discarded > 0 {
                let mut stats = self.presentation.borrow_mut();
                stats.background_feedback_discarded = stats
                    .background_feedback_discarded
                    .saturating_add(discarded);
                stats.presentation_feedback_unavailable = stats
                    .presentation_feedback_unavailable
                    .saturating_add(discarded);
                stats.last_pacing_source = "internal-hidden-window-clock".into();
                drop(stats);
                self.presentation_dirty.set(true);
            }
            return;
        };

        if let Err(error) = self.commands.send(GatewayCommand::FramePainted {
            id: frame.id,
            frame_time_us,
        }) {
            *self.failure.borrow_mut() = Some(format!("{error:#}"));
            self.set_state(MonitorState::Failed);
            return;
        }
        if let Err(error) = self.commands.send(GatewayCommand::FramePresented {
            id: frame.id,
            presentation_time_us: 0,
            refresh_interval_us: 0,
            sequence: 0,
            offloaded: false,
        }) {
            *self.failure.borrow_mut() = Some(format!("{error:#}"));
            self.set_state(MonitorState::Failed);
            return;
        }

        let mut stats = self.presentation.borrow_mut();
        stats.painted_frames = stats.painted_frames.saturating_add(1);
        stats.background_paced_frames = stats.background_paced_frames.saturating_add(1);
        stats.background_feedback_discarded = stats
            .background_feedback_discarded
            .saturating_add(discarded.saturating_add(1));
        stats.presentation_feedback_unavailable = stats
            .presentation_feedback_unavailable
            .saturating_add(discarded.saturating_add(1));
        stats.last_pacing_source = "internal-hidden-window-clock".into();
        drop(stats);
        self.presentation_dirty.set(true);
        self.observe_monitor_continuity("background-paint");
        self.frame_paintable.release_retired();
    }

    fn finish_presentation_feedback(&self, clock: &gdk::FrameClock) {
        let mut changed = false;
        loop {
            let ready = {
                let queue = self.pending_presentations.borrow();
                let Some(frame) = queue.front() else {
                    break;
                };
                match clock.timings(frame.frame_counter) {
                    Some(timings) if timings.is_complete() => Some((
                        timings.presentation_time(),
                        timings.refresh_interval(),
                        timings.frame_counter().max(0) as u64,
                    )),
                    None if frame.frame_counter < clock.history_start() => Some((0, 0, 0)),
                    _ => None,
                }
            };
            let Some((presentation_time_us, refresh_interval_us, sequence)) = ready else {
                break;
            };
            let Some(frame) = self.pending_presentations.borrow_mut().pop_front() else {
                break;
            };
            if let Err(error) = self.commands.send(GatewayCommand::FramePresented {
                id: frame.id,
                presentation_time_us,
                refresh_interval_us,
                sequence,
                offloaded: frame.offloaded,
            }) {
                eprintln!("buzzardos-display: returning presentation feedback: {error:#}");
                break;
            }
            self.record_presentation(&frame, presentation_time_us, refresh_interval_us, sequence);
            changed = true;
        }
        if changed {
            self.presentation_dirty.set(true);
        }
    }

    fn record_presentation(
        &self,
        frame: &PendingPresentation,
        presentation_time_us: i64,
        refresh_interval_us: i64,
        sequence: u64,
    ) {
        let mut stats = self.presentation.borrow_mut();
        let same_presented_path = stats.presented
            && frame.metadata.geometry_generation == self.geometry_generation.get()
            && stats.width == frame.metadata.width
            && stats.height == frame.metadata.height
            && stats.format == frame.metadata.fourcc
            && stats.modifier == format!("0x{:016x}", frame.metadata.modifier)
            && stats.planes == frame.metadata.planes
            && stats.scale_120 == self.host_surface_scale_120.get()
            && stats.viewport_width == self.viewport_width.get()
            && stats.viewport_height == self.viewport_height.get();
        stats.transport = "dmabuf".into();
        stats.width = frame.metadata.width;
        stats.height = frame.metadata.height;
        stats.format = frame.metadata.fourcc;
        stats.modifier = format!("0x{:016x}", frame.metadata.modifier);
        stats.planes = frame.metadata.planes;
        stats.scale_120 = self.host_surface_scale_120.get();
        stats.viewport_width = self.viewport_width.get();
        stats.viewport_height = self.viewport_height.get();
        // Re-evaluate against the current protocol state. A frame may have
        // been queued immediately before a host resize or monitor-scale
        // transition; its submission-time value must never resurrect a stale
        // native-resolution or zero-copy claim.
        let exact_native_mapping = frame.metadata.geometry_generation
            == self.geometry_generation.get()
            && frame_has_exact_native_mapping(
                frame.metadata.width,
                frame.metadata.height,
                self.viewport_width.get(),
                self.viewport_height.get(),
                self.host_surface_scale_120.get(),
            );
        stats.native_resolution = exact_native_mapping;
        stats.presentation_feedback = true;
        stats.gtk_subsurface_offload = frame.offloaded;
        stats.last_pacing_source = "host-vblank".into();
        stats.explicit_sync = if frame.metadata.explicit_sync {
            "linux-drm-syncobj-v1/gateway-wait/gtk-host-sync".into()
        } else {
            "implicit-dmabuf".into()
        };
        if presentation_time_us <= 0 {
            stats.discarded = !same_presented_path;
            if !same_presented_path {
                stats.presented = false;
                stats.vsync = false;
                stats.zero_copy = false;
            }
            stats.presentation_feedback_unavailable =
                stats.presentation_feedback_unavailable.saturating_add(1);
            return;
        }
        if stats.last_presentation_time_us > 0 {
            stats.last_presented_frame_interval_us =
                presentation_time_us.saturating_sub(stats.last_presentation_time_us);
        }
        let submission_to_presentation_us =
            (presentation_time_us.max(0) as u64).saturating_sub(frame.submitted_monotonic_us);
        stats.presented = true;
        stats.discarded = false;
        stats.vsync = refresh_interval_us > 0;
        stats.zero_copy = frame.offloaded && exact_native_mapping;
        stats.sequence = sequence;
        stats.refresh_ns = refresh_interval_us
            .max(0)
            .saturating_mul(1_000)
            .min(i64::from(u32::MAX)) as u32;
        stats.timestamp_ns = (presentation_time_us as u64).saturating_mul(1_000);
        stats.presented_frames = stats.presented_frames.saturating_add(1);
        stats.last_presentation_time_us = presentation_time_us;
        stats.last_refresh_interval_us = refresh_interval_us;
        stats.last_submission_time_us = frame.submitted_monotonic_us;
        stats.last_submission_to_presentation_us = submission_to_presentation_us;
        stats.maximum_submission_to_presentation_us = stats
            .maximum_submission_to_presentation_us
            .max(submission_to_presentation_us);
    }

    fn set_state(&self, state: MonitorState) {
        let previous = self.state.replace(state);
        if previous != state {
            // Each clipboard transaction belongs to one live machine
            // lifecycle. Invalidate it before exposing a replacement state,
            // so an old response cannot change the host clipboard after
            // Stop/Restart or interfere with a newer transaction.
            self.clipboard_epoch.set(lifecycle_clipboard_epoch(
                self.clipboard_epoch.get(),
                previous,
                state,
            ));
            if let Some((_, connection)) = self.clipboard_connection.borrow_mut().take() {
                let _ = connection.shutdown(Shutdown::Both);
            }
            self.clipboard_busy.set(false);
        }
        self.update_state_ui();
        if let Err(error) = self.save_window() {
            eprintln!("buzzardos-display: saving native window state: {error:#}");
        }
        if let Err(error) = self.save_output_state() {
            eprintln!("buzzardos-display: saving monitor state: {error:#}");
        }
    }

    fn update_state_ui(&self) {
        self.update_header_status(false, false);

        match self.state.get() {
            MonitorState::Running => {
                self.spinner.stop();
                self.state_title.set_label("Machine running");
            }
            MonitorState::Stopped => {
                self.spinner.stop();
                self.state_title.set_label("Machine stopped");
                self.detail_label
                    .set_label("Use Machine → Start to boot this persistent desktop.");
            }
            MonitorState::Starting => {
                self.spinner.start();
                self.state_title.set_label("Starting machine");
                self.detail_label
                    .set_label("Starting systemd, Sway, desktop services, and CUA driver…");
            }
            MonitorState::Stopping => {
                self.spinner.start();
                self.state_title.set_label("Stopping machine");
                self.detail_label
                    .set_label("Waiting for orderly guest shutdown and state persistence…");
            }
            MonitorState::Failed => {
                self.spinner.stop();
                self.state_title.set_label("Machine failed");
                self.detail_label.set_label(
                    self.failure
                        .borrow()
                        .as_deref()
                        .unwrap_or("Machine startup failed. Open Diagnostics for details."),
                );
            }
        }
        // Lifecycle labels and display attachment are separate state
        // machines. Once a frame is attached, stale runtime.json values must
        // not put the Starting overlay back over the live monitor.
        self.state_overlay
            .set_visible(!self.frame_paintable.has_frame());
        self.update_clipboard_action_state();
        self.observe_monitor_continuity("lifecycle-ui");
    }

    fn update_header_status(&self, microphone_active: bool, camera_active: bool) {
        for class in ["dim-label", "warning", "success", "error"] {
            self.status_label.remove_css_class(class);
        }
        self.status_label.set_label(&header_status_text(
            self.state.get(),
            microphone_active,
            camera_active,
        ));
        self.status_label
            .add_css_class(self.state.get().css_class());
        self.status_label
            .set_tooltip_text(Some(if microphone_active || camera_active {
                "Host media sharing is active continuously until disabled in Devices"
            } else {
                "Machine lifecycle state"
            }));
    }

    fn reset_offload_claim(&self) {
        for pending in self.pending_presentations.borrow_mut().iter_mut() {
            pending.offloaded = false;
        }
        {
            let mut stats = self.presentation.borrow_mut();
            stats.gtk_subsurface_offload = false;
            stats.zero_copy = false;
        }
        self.presentation_dirty.set(true);
    }

    fn detach_monitor(&self, source: &'static str) {
        if !self.frame_paintable.has_frame() && !self.continuity.borrow().attachment_active {
            return;
        }
        self.continuity.borrow_mut().detach(source);
        self.reset_offload_claim();
        self.frame_paintable.clear();
        self.update_state_ui();
    }

    fn observe_monitor_continuity(&self, source: &'static str) {
        let placeholder_visible = self.state_overlay.is_visible();
        let frame_available = self.frame_paintable.has_frame();
        let violation =
            self.continuity
                .borrow_mut()
                .observe(source, placeholder_visible, frame_available);
        if violation {
            eprintln!(
                "buzzardos-display: monitor continuity violation: source={source}, \
                 placeholder={placeholder_visible}, frame={frame_available}"
            );
        }
    }

    /// Keep the GraphicsOffload child on an integral device-pixel rectangle.
    ///
    /// GTK's Wayland subsurface backend refuses scanout/offload when any of
    /// destination x, y, width, or height multiplied by the fractional scale
    /// is non-integral. More importantly, allowing such a rectangle through a
    /// compositor would filter a native dmabuf. The permanent Overlay remains
    /// the expanding wrapper; small symmetric-looking margins align the child
    /// origin and quantize its extent. Guest mode and input are always derived
    /// from the resulting Picture allocation, never from the wrapper.
    fn align_monitor_offload(&self) -> Result<bool> {
        let Some(wrapper_bounds) = self.monitor_view.compute_bounds(&self.window) else {
            return Ok(false);
        };
        let host_scale = self
            .window
            .surface()
            .map(|surface| surface.scale())
            .unwrap_or(1.0);
        let scale_120 = effective_scale_120(self.launch.test_fractional_scale_120, host_scale)
            .map_err(anyhow::Error::msg)?;
        let denominator = scale_denominator(scale_120)
            .context("host fractional scale has no valid denominator")?
            as i32;
        let (surface_x, surface_y) = self.window.surface_transform();
        // Wrapper bounds never include the child's current margins, so this
        // base cannot oscillate after a previous alignment allocation.
        let base_x = surface_x + f64::from(wrapper_bounds.x());
        let base_y = surface_y + f64::from(wrapper_bounds.y());
        let wrapper_width = self.monitor_view.width().max(1);
        let wrapper_height = self.monitor_view.height().max(1);
        if wrapper_width <= denominator || wrapper_height <= denominator {
            // Realize can precede the first useful allocation.
            return Ok(false);
        }
        let start = aligned_origin_margin(base_x, scale_120).with_context(|| {
            format!("monitor x origin {base_x:.6} cannot be aligned at {scale_120}/120 scale")
        })?;
        let top = aligned_origin_margin(base_y, scale_120).with_context(|| {
            format!("monitor y origin {base_y:.6} cannot be aligned at {scale_120}/120 scale")
        })?;
        let end = trailing_extent_margin(wrapper_width, start, denominator)
            .context("monitor wrapper is too narrow for an aligned offload child")?;
        let bottom = trailing_extent_margin(wrapper_height, top, denominator)
            .context("monitor wrapper is too short for an aligned offload child")?;
        let child_origin_x = base_x + f64::from(start);
        let child_origin_y = base_y + f64::from(top);
        let child_width = wrapper_width - start - end;
        let child_height = wrapper_height - top - bottom;
        let logical_origin_integral =
            is_integral_coordinate(child_origin_x) && is_integral_coordinate(child_origin_y);
        let device_origin_integral = is_integral_device_coordinate(child_origin_x, scale_120)
            && is_integral_device_coordinate(child_origin_y, scale_120);
        let device_extent_integral = i64::from(child_width) * i64::from(scale_120)
            % i64::from(WAYLAND_SCALE_DENOMINATOR)
            == 0
            && i64::from(child_height) * i64::from(scale_120)
                % i64::from(WAYLAND_SCALE_DENOMINATOR)
                == 0;
        *self.offload_geometry.borrow_mut() = OffloadGeometryDiagnostics {
            schema: 1,
            scale_120,
            scale_denominator: denominator as u32,
            surface_transform_x: surface_x,
            surface_transform_y: surface_y,
            wrapper_x: f64::from(wrapper_bounds.x()),
            wrapper_y: f64::from(wrapper_bounds.y()),
            wrapper_width,
            wrapper_height,
            margin_start: start,
            margin_end: end,
            margin_top: top,
            margin_bottom: bottom,
            child_origin_x,
            child_origin_y,
            child_width,
            child_height,
            logical_origin_integral,
            device_origin_integral,
            device_extent_integral,
            allocation_settled: self.picture.width() == child_width
                && self.picture.height() == child_height,
        };
        if !logical_origin_integral || !device_origin_integral || !device_extent_integral {
            anyhow::bail!(
                "monitor offload geometry is not integral: origin=({child_origin_x:.6}, \
                 {child_origin_y:.6}), child={child_width}x{child_height}, scale={scale_120}/120"
            );
        }
        let changed = self.offload.margin_start() != start
            || self.offload.margin_end() != end
            || self.offload.margin_top() != top
            || self.offload.margin_bottom() != bottom;
        if changed {
            self.offload.set_margin_start(start);
            self.offload.set_margin_end(end);
            self.offload.set_margin_top(top);
            self.offload.set_margin_bottom(bottom);
        }
        Ok(changed)
    }

    fn update_viewport(&self, width: u32, height: u32) {
        let scale = self
            .window
            .surface()
            .map(|surface| surface.scale())
            .unwrap_or(1.0);
        let host_surface_scale_120 =
            match effective_scale_120(self.launch.test_fractional_scale_120, scale) {
                Ok(scale_120) => scale_120,
                Err(error) => {
                    *self.failure.borrow_mut() = Some(error);
                    self.set_state(MonitorState::Failed);
                    return;
                }
            };
        let Some(host_mapping) = PixelMapping::new(width, height, host_surface_scale_120) else {
            *self.failure.borrow_mut() = Some(format!(
                "host viewport {width}x{height} at {host_surface_scale_120}/120 scale exceeds the supported \
                 Wayland buffer or fixed-point coordinate dimensions"
            ));
            self.set_state(MonitorState::Failed);
            return;
        };
        let guest_ui_scale_120 = self
            .guest_scale_preset
            .get()
            .resolve(host_surface_scale_120);
        let refresh_mhz = self
            .window
            .surface()
            .and_then(|surface| {
                let display = surface.display();
                display.monitor_at_surface(&surface)
            })
            .map(|monitor| monitor.refresh_rate().max(0) as u32)
            .unwrap_or(0);
        let geometry_changed = width != self.viewport_width.get()
            || height != self.viewport_height.get()
            || host_surface_scale_120 != self.host_surface_scale_120.get()
            || guest_ui_scale_120 != self.guest_ui_scale_120.get();
        let refresh_changed = refresh_mhz != self.refresh_mhz.get();
        if !geometry_changed && !refresh_changed {
            return;
        }

        if geometry_changed {
            // The release is queued with the old generation before the new
            // SetOutputMode command. This closes any compositor move/resize
            // grab even if the physical extent changes while a button is
            // held; a later stale press can then be ignored safely.
            self.release_pressed_pointer_buttons();
        }
        self.viewport_width.set(width);
        self.viewport_height.set(height);
        self.host_surface_scale_120.set(host_surface_scale_120);
        self.guest_ui_scale_120.set(guest_ui_scale_120);
        self.refresh_mhz.set(refresh_mhz);
        if geometry_changed {
            self.advance_geometry_generation();
        }
        self.update_presentation_geometry(host_mapping);
        if let Err(error) = self.publish_output_mode() {
            *self.failure.borrow_mut() = Some(format!(
                "publishing native guest monitor mode {width}x{height}: {error:#}"
            ));
            self.set_state(MonitorState::Failed);
            return;
        }
        if let Err(error) = self.save_output_state() {
            eprintln!("buzzardos-display: saving resized guest output: {error:#}");
        }
        if let Err(error) = self.save_window() {
            eprintln!("buzzardos-display: saving resized host window: {error:#}");
        }
        self.presentation_dirty.set(true);
    }

    fn advance_geometry_generation(&self) {
        let current = self.geometry_generation.get();
        self.geometry_generation
            .set(current.checked_add(1).unwrap_or(1));
    }

    fn update_presentation_geometry(&self, _host_mapping: PixelMapping) {
        let mut stats = self.presentation.borrow_mut();
        stats.scale_120 = self.host_surface_scale_120.get();
        stats.viewport_width = self.viewport_width.get();
        stats.viewport_height = self.viewport_height.get();
        stats.native_resolution = stats.width > 0
            && frame_has_exact_native_mapping(
                stats.width,
                stats.height,
                self.viewport_width.get(),
                self.viewport_height.get(),
                self.host_surface_scale_120.get(),
            );
        if !stats.native_resolution {
            stats.zero_copy = false;
        }
    }

    fn apply_guest_scale_request(&self, request: GuestScaleRequest) -> GuestScaleReply {
        let current = self.output_mode().geometry();
        if request.current_geometry_generation != current.geometry_generation {
            return GuestScaleReply::Rejected {
                code: "stale_geometry",
                message: format!(
                    "geometry generation {} is stale; current generation is {}",
                    request.current_geometry_generation, current.geometry_generation
                ),
                current_geometry: current,
            };
        }
        let guest_ui_scale_120 = request.preset.resolve(self.host_surface_scale_120.get());
        let preset_changed = request.preset != self.guest_scale_preset.get();
        let scale_changed = guest_ui_scale_120 != self.guest_ui_scale_120.get();
        if !preset_changed && !scale_changed {
            return GuestScaleReply::Applied {
                preset: request.preset,
                geometry: current,
            };
        }

        // End pointer grabs in the old coordinate epoch. Gateway commands
        // use one FIFO channel, so these releases reach the guest before the
        // new generation's SetOutputMode command.
        self.release_pressed_pointer_buttons();
        self.guest_scale_preset.set(request.preset);
        self.guest_ui_scale_120.set(guest_ui_scale_120);
        self.advance_geometry_generation();
        self.update_presentation_geometry(self.host_pixel_mapping());
        if let Err(error) = self
            .publish_output_mode()
            .and_then(|()| self.save_output_state())
        {
            return GuestScaleReply::Rejected {
                code: "runtime_failure",
                message: format!("could not commit guest UI scale: {error:#}"),
                current_geometry: self.output_mode().geometry(),
            };
        }
        self.presentation_dirty.set(true);
        GuestScaleReply::Applied {
            preset: request.preset,
            geometry: self.output_mode().geometry(),
        }
    }

    fn publish_output_mode(&self) -> Result<()> {
        let mode = self.output_mode();
        if self.gateway_configured.get() {
            return self.commands.send(GatewayCommand::SetOutputMode(mode));
        }
        let display = gdk::Display::default().context("GTK has no active Wayland display")?;
        let advertised = display.dmabuf_formats();
        let formats = (0..advertised.n_formats())
            .map(|index| advertised.format(index))
            .map(|(fourcc, modifier)| DmabufFormat { fourcc, modifier })
            .collect();
        self.commands
            .send(GatewayCommand::Configure { formats, mode })?;
        self.gateway_configured.set(true);
        Ok(())
    }

    fn output_mode(&self) -> OutputMode {
        let guest_ui_scale_120 = self.guest_ui_scale_120.get();
        let mapping = self.host_pixel_mapping();
        OutputMode {
            logical_width: guest_logical_dimension(mapping.physical_width, guest_ui_scale_120),
            logical_height: guest_logical_dimension(mapping.physical_height, guest_ui_scale_120),
            physical_width: mapping.physical_width,
            physical_height: mapping.physical_height,
            host_surface_scale_120: self.host_surface_scale_120.get(),
            guest_ui_scale_120,
            geometry_generation: self.geometry_generation.get(),
            refresh_mhz: self.refresh_mhz.get(),
        }
    }

    fn host_pixel_mapping(&self) -> PixelMapping {
        PixelMapping::new(
            self.viewport_width.get(),
            self.viewport_height.get(),
            self.host_surface_scale_120.get(),
        )
        .expect("validated host viewport must fit Wayland fixed-point coordinates")
    }

    fn apply_host_command(&self, command: HostCommand) {
        match command {
            HostCommand::Minimize => self.window.minimize(),
            HostCommand::Maximize => self.window.maximize(),
            HostCommand::Restore => {
                self.window.unminimize();
                self.window.unmaximize();
                self.window.present();
            }
            HostCommand::FocusMonitor => {
                self.window.unminimize();
                self.window.present();
                self.picture.grab_focus();
            }
            HostCommand::ToggleMaximize if self.window.is_maximized() => self.window.unmaximize(),
            HostCommand::ToggleMaximize => self.window.maximize(),
            HostCommand::Close => self.request_close(),
            HostCommand::Start => self.request_start(),
            HostCommand::Stop | HostCommand::ShutDown => self.request_stop(false),
            HostCommand::Restart => self.request_stop(true),
            HostCommand::OpenSettings => self.open_settings(),
            HostCommand::OpenDiagnostics => self.open_diagnostics(),
            HostCommand::CaptureUi => {
                if let Err(error) = self.capture_ui() {
                    self.show_error("Could not capture native application", &error);
                }
            }
        }
    }

    fn request_close(&self) {
        if matches!(
            self.state.get(),
            MonitorState::Stopped | MonitorState::Failed
        ) {
            self.application.quit();
            return;
        }
        self.close_requested.set(true);
        self.request_stop(false);
    }

    fn request_start(&self) {
        // The lifecycle supervisor consumes this fixed host-only request. It
        // never contains an arbitrary command or a guest-provided path.
        if let Err(error) = self.save_host_request("start") {
            self.show_error("Could not request machine start", &error);
            return;
        }
        self.set_state(MonitorState::Starting);
    }

    fn request_stop(&self, restart: bool) {
        let action = if restart { "restart" } else { "stop" };
        if let Err(error) = self.save_host_request(action) {
            self.show_error("Could not request orderly shutdown", &error);
            return;
        }
        self.set_state(MonitorState::Stopping);
        self.close_requested.set(!restart);
    }

    fn save_host_request(&self, action: &str) -> Result<()> {
        // The selected machine directory is deliberately independent from
        // the human-facing machine name.  Authenticate lifecycle requests
        // with the name in the self-describing machine metadata, never with
        // the directory basename.
        let config = MachineConfig::load(&self.launch.machine_dir)?;
        let value = serde_json::json!({
            "schema": 1,
            "action": action,
            "machine": config.name,
        });
        atomic_json(&self.launch.status_dir.join("host-request.json"), &value)
    }

    fn clipboard_ready_path(&self) -> PathBuf {
        self.launch
            .guest_clipboard_control
            .parent()
            .expect("validated clipboard socket has a parent")
            .join("clipboard-ready")
    }

    fn clipboard_available(&self) -> bool {
        self.state.get() == MonitorState::Running
            && clipboard::agent_ready(
                &self.launch.guest_clipboard_control,
                &self.clipboard_ready_path(),
            )
    }

    fn update_clipboard_action_state(&self) {
        let enabled = self.clipboard_available() && !self.clipboard_busy.get();
        for name in ["clipboard-to-guest", "clipboard-to-host"] {
            if let Some(action) = self
                .application
                .lookup_action(name)
                .and_then(|action| action.downcast::<gio::SimpleAction>().ok())
            {
                action.set_enabled(enabled);
            }
        }
    }

    fn begin_clipboard_transfer(
        &self,
    ) -> Result<(
        u64,
        clipboard::EndpointSnapshot,
        clipboard::PendingEndpointConnection,
    )> {
        if self.state.get() != MonitorState::Running {
            anyhow::bail!(
                "the machine must be Running and its private clipboard agent must be ready"
            );
        }
        if self.clipboard_busy.get() {
            anyhow::bail!("another clipboard transfer is already in progress");
        }
        // Capture the exact endpoint/ready inodes in the click callback. The
        // asynchronous worker may connect only to this identity; it cannot
        // resolve a replacement listener installed by Stop/Restart.
        let endpoint = clipboard::EndpointSnapshot::capture(
            &self.launch.guest_clipboard_control,
            &self.clipboard_ready_path(),
        )?;
        let pending_connection = endpoint.clone().begin_connect()?;
        self.clipboard_busy.set(true);
        self.update_clipboard_action_state();
        Ok((self.clipboard_epoch.get(), endpoint, pending_connection))
    }

    fn require_live_clipboard_transfer(
        &self,
        epoch: u64,
        endpoint: &clipboard::EndpointSnapshot,
    ) -> Result<()> {
        if !clipboard_transfer_is_live(
            self.clipboard_epoch.get(),
            epoch,
            self.state.get() == MonitorState::Running && endpoint.is_current(),
        ) {
            anyhow::bail!("clipboard transfer was cancelled by a machine lifecycle change");
        }
        Ok(())
    }

    fn retain_clipboard_cancellation_handle(
        &self,
        epoch: u64,
        endpoint: &clipboard::EndpointSnapshot,
        connection: &clipboard::ConnectedEndpoint,
    ) -> Result<()> {
        self.require_live_clipboard_transfer(epoch, endpoint)?;
        let cancel = connection.cancel_handle()?;
        if let Some((_, previous)) = self
            .clipboard_connection
            .borrow_mut()
            .replace((epoch, cancel))
        {
            let _ = previous.shutdown(Shutdown::Both);
        }
        Ok(())
    }

    fn release_clipboard_cancellation_handle(&self, epoch: u64) {
        let should_release = self
            .clipboard_connection
            .borrow()
            .as_ref()
            .is_some_and(|(connection_epoch, _)| *connection_epoch == epoch);
        if should_release
            && let Some((_, connection)) = self.clipboard_connection.borrow_mut().take()
        {
            let _ = connection.shutdown(Shutdown::Both);
        }
    }

    fn send_host_clipboard_to_guest(self: &Rc<Self>) {
        let (epoch, endpoint, pending_connection) = match self.begin_clipboard_transfer() {
            Ok(transfer) => transfer,
            Err(error) => {
                self.show_error("Could not send clipboard to guest", &error);
                return;
            }
        };
        let this = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let result = async {
                let connection = gio::spawn_blocking(move || pending_connection.finish())
                    .await
                    .map_err(|_| anyhow::anyhow!("clipboard connection worker terminated"))??;
                this.retain_clipboard_cancellation_handle(epoch, &endpoint, &connection)?;
                // This is the only host clipboard read in the complete
                // transport, and this future exists only because the user
                // activated the native header action above.
                let value = read_host_clipboard_snapshot().await?;
                this.require_live_clipboard_transfer(epoch, &endpoint)?;
                let mime = value.mime();
                let bytes = value.bytes().len();
                let nonce = *Uuid::new_v4().as_bytes();
                gio::spawn_blocking(move || clipboard::put(connection, nonce, value))
                    .await
                    .map_err(|_| anyhow::anyhow!("clipboard transport worker terminated"))??;
                this.require_live_clipboard_transfer(epoch, &endpoint)?;
                Ok::<_, anyhow::Error>((mime, bytes))
            }
            .await;
            this.finish_clipboard_transfer(epoch, "host-to-guest", result);
        });
    }

    fn copy_guest_clipboard_to_host(self: &Rc<Self>) {
        let (epoch, endpoint, pending_connection) = match self.begin_clipboard_transfer() {
            Ok(transfer) => transfer,
            Err(error) => {
                self.show_error("Could not copy guest clipboard to host", &error);
                return;
            }
        };
        let this = Rc::clone(self);
        glib::MainContext::default().spawn_local(async move {
            let result = async {
                let connection = gio::spawn_blocking(move || pending_connection.finish())
                    .await
                    .map_err(|_| anyhow::anyhow!("clipboard connection worker terminated"))??;
                this.retain_clipboard_cancellation_handle(epoch, &endpoint, &connection)?;
                let nonce = *Uuid::new_v4().as_bytes();
                let value = gio::spawn_blocking(move || clipboard::get(connection, nonce))
                    .await
                    .map_err(|_| anyhow::anyhow!("clipboard transport worker terminated"))??;
                this.require_live_clipboard_transfer(epoch, &endpoint)?;
                let mut value = value;
                let mime = value.mime();
                let bytes = value.bytes().len();
                if mime == Mime::Png {
                    let decoded = decode_host_clipboard_image(value.take_bytes()).await?;
                    value.install_decoded_image(decoded)?;
                }
                this.require_live_clipboard_transfer(epoch, &endpoint)?;
                this.install_host_clipboard(value)?;
                Ok((mime, bytes))
            }
            .await;
            this.finish_clipboard_transfer(epoch, "guest-to-host", result);
        });
    }

    fn install_host_clipboard(&self, mut value: ClipboardValue) -> Result<()> {
        let display = gdk::Display::default().context("GTK has no active host display")?;
        let host_clipboard = display.clipboard();
        match value.mime() {
            Mime::Text => {
                let bytes = ZeroizingBytes(value.take_bytes());
                let text = std::str::from_utf8(&bytes.0)
                    .context("validated guest clipboard text became invalid")?;
                // This sets a new host-owned value. It does not give the guest
                // a reference to the host clipboard object or provider.
                host_clipboard.set_text(text);
                Ok(())
            }
            Mime::Png => {
                let mut decoded = value
                    .take_decoded_image()
                    .context("validated guest clipboard image lacks decoded pixels")?;
                let width = i32::try_from(decoded.width)
                    .context("validated clipboard image width is not representable")?;
                let height = i32::try_from(decoded.height)
                    .context("validated clipboard image height is not representable")?;
                let pixels = glib::Bytes::from_owned(ZeroizingBytes(decoded.take_rgba()));
                // Untrusted PNG decode and conversion happened in the
                // bounded transport worker. Constructing a MemoryTexture is
                // only a constant-time wrapper around already decoded RGBA.
                let texture = gdk::MemoryTexture::new(
                    width,
                    height,
                    gdk::MemoryFormat::R8g8b8a8,
                    &pixels,
                    decoded.stride,
                );
                host_clipboard.set_texture(&texture);
                Ok(())
            }
            Mime::None => anyhow::bail!("validated clipboard value has no supported MIME"),
        }
    }

    fn finish_clipboard_transfer(
        &self,
        epoch: u64,
        direction: &'static str,
        result: Result<(Mime, usize)>,
    ) {
        self.release_clipboard_cancellation_handle(epoch);
        if self.clipboard_epoch.get() != epoch {
            // A newer machine lifecycle may already have its own transfer.
            // The stale completion must not clear its busy flag, show a
            // dialog, or overwrite its diagnostics.
            return;
        }
        self.clipboard_busy.set(false);
        self.update_clipboard_action_state();
        match result {
            Ok((_mime, _bytes)) => {
                let destination = if direction == "host-to-guest" {
                    "The selected host clipboard snapshot is now available inside the guest."
                } else {
                    "The selected guest clipboard snapshot is now available on the host."
                };
                show_info_dialog(&self.window, "Clipboard transferred", destination);
            }
            Err(error) => {
                self.show_error("Clipboard transfer failed", &error);
            }
        }
    }

    fn open_settings(&self) {
        let config = match MachineConfig::load(&self.launch.machine_dir) {
            Ok(config) => config,
            Err(error) => {
                self.show_error("Could not load machine settings", &error);
                return;
            }
        };
        let dialog = gtk::Window::builder()
            .title("Machine Settings")
            .transient_for(&self.window)
            .modal(true)
            .destroy_with_parent(true)
            .default_width(520)
            .default_height(520)
            .resizable(false)
            .build();
        dialog.set_titlebar(Some(&gtk::HeaderBar::new()));
        let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        let grid = gtk::Grid::builder()
            .row_spacing(12)
            .column_spacing(16)
            .hexpand(true)
            .build();
        content.append(&grid);

        let width = gtk::SpinButton::with_range(320.0, 16_384.0, 1.0);
        width.set_value(config.width as f64);
        let height = gtk::SpinButton::with_range(240.0, 16_384.0, 1.0);
        height.set_value(config.height as f64);
        let network = gtk::DropDown::from_strings(&[
            "Private user-mode network",
            "Host network (reduced isolation)",
            "No network",
        ]);
        network.set_selected(match config.network {
            NetworkMode::User => 0,
            NetworkMode::Host => 1,
            NetworkMode::None => 2,
        });
        let guest_scale = gtk::DropDown::from_strings(&[
            "Follow host (pixel-perfect default)",
            "100%",
            "125%",
            "150%",
            "175%",
            "200%",
        ]);
        guest_scale.set_selected(match config.guest_scale_120 {
            None => 0,
            Some(120) => 1,
            Some(150) => 2,
            Some(180) => 3,
            Some(210) => 4,
            Some(240) => 5,
            Some(_) => 0,
        });
        let gpus = gtk::Entry::builder()
            .text(config.gpus.join(","))
            .placeholder_text("all, index, or GPU UUIDs")
            .hexpand(true)
            .build();
        let machine_location = machine_location_control(&dialog, &self.launch.machine_dir);
        let restart = gtk::Label::new(Some(
            "Display, network, and GPU changes take effect on the next machine start.",
        ));
        restart.add_css_class("dim-label");
        restart.set_wrap(true);
        restart.set_xalign(0.0);

        attach_setting(&grid, 0, "Machine location", &machine_location);
        attach_setting(&grid, 1, "Initial monitor width", &width);
        attach_setting(&grid, 2, "Initial monitor height", &height);
        attach_setting(&grid, 3, "Desktop scale", &guest_scale);
        attach_setting(&grid, 4, "Network mode", &network);
        attach_setting(&grid, 5, "GPU passthrough", &gpus);
        grid.attach(&restart, 0, 6, 2, 1);

        let actions = gtk::ActionBar::new();
        let cancel = gtk::Button::with_label("Cancel");
        let save = gtk::Button::with_label("Save");
        save.set_receives_default(true);
        actions.pack_end(&save);
        actions.pack_end(&cancel);
        content.append(&actions);
        dialog.set_child(Some(&content));
        dialog.set_default_widget(Some(&save));

        let cancel_dialog = dialog.clone();
        cancel.connect_clicked(move |_| cancel_dialog.close());
        let machine_dir = self.launch.machine_dir.clone();
        let parent = self.window.clone();
        let save_dialog = dialog.clone();
        save.connect_clicked(move |_| {
            let mut updated = config.clone();
            updated.width = width.value_as_int() as u32;
            updated.height = height.value_as_int() as u32;
            updated.guest_scale_120 = match guest_scale.selected() {
                1 => Some(120),
                2 => Some(150),
                3 => Some(180),
                4 => Some(210),
                5 => Some(240),
                _ => None,
            };
            updated.network = match network.selected() {
                1 => NetworkMode::Host,
                2 => NetworkMode::None,
                _ => NetworkMode::User,
            };
            updated.gpus = gpus
                .text()
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect();
            let result = MachineConfig::validate_gpus(&updated.gpus)
                .and_then(|_| updated.save(&machine_dir));
            if let Err(error) = result {
                show_error_dialog(&parent, "Could not save machine settings", &error);
                return;
            }
            save_dialog.close();
        });
        dialog.present();
    }

    fn open_ports(&self) {
        let config = match MachineConfig::load(&self.launch.machine_dir) {
            Ok(config) => config,
            Err(error) => {
                self.show_error("Could not load port settings", &error);
                return;
            }
        };
        let dialog = gtk::Window::builder()
            .title("Live Port Mappings")
            .transient_for(&self.window)
            .modal(true)
            .destroy_with_parent(true)
            .default_width(940)
            .default_height(560)
            .resizable(true)
            .build();
        dialog.set_titlebar(Some(&gtk::HeaderBar::new()));
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(16);
        content.set_margin_bottom(16);
        content.set_margin_start(16);
        content.set_margin_end(16);

        let explanation = gtk::Label::new(Some(
            "Mappings apply while the machine is running—no restart. Host → Guest publishes a guest service. Guest → Host exposes only the selected host destination through a private relay; host loopback otherwise remains blocked.",
        ));
        explanation.set_wrap(true);
        explanation.set_xalign(0.0);
        explanation.add_css_class("dim-label");
        content.append(&explanation);

        if !matches!(config.network, NetworkMode::User) {
            let warning = gtk::Label::new(Some(
                "Live mappings require Private user-mode network in Machine Settings.",
            ));
            warning.set_xalign(0.0);
            warning.add_css_class("warning");
            content.append(&warning);
        }

        let header = gtk::Grid::builder().column_spacing(8).hexpand(true).build();
        for (column, (label, width)) in [
            ("On", 4),
            ("Direction", 15),
            ("Protocol", 8),
            ("Host address", 16),
            ("Host port", 8),
            ("Guest address", 16),
            ("Guest port", 8),
            ("", 4),
        ]
        .into_iter()
        .enumerate()
        {
            let widget = gtk::Label::new(Some(label));
            widget.set_xalign(0.0);
            widget.set_width_chars(width);
            widget.add_css_class("dim-label");
            header.attach(&widget, column as i32, 0, 1, 1);
        }
        content.append(&header);

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        let rows: Rc<RefCell<Vec<PortEditorRow>>> = Rc::new(RefCell::new(Vec::new()));
        for mapping in &config.integrations.ports {
            append_port_editor(&list, &rows, mapping.clone());
        }
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&list)
            .build();
        content.append(&scroll);

        let add_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let add_inbound = gtk::Button::with_label("Add Host → Guest");
        let add_reverse = gtk::Button::with_label("Add Guest → Host");
        add_bar.append(&add_inbound);
        add_bar.append(&add_reverse);
        content.append(&add_bar);

        let list_for_inbound = list.clone();
        let rows_for_inbound = Rc::clone(&rows);
        add_inbound.connect_clicked(move |_| {
            append_port_editor(
                &list_for_inbound,
                &rows_for_inbound,
                PortForward::new(PortDirection::HostToGuest),
            );
        });
        let list_for_reverse = list.clone();
        let rows_for_reverse = Rc::clone(&rows);
        add_reverse.connect_clicked(move |_| {
            append_port_editor(
                &list_for_reverse,
                &rows_for_reverse,
                PortForward::new(PortDirection::GuestToHost),
            );
        });

        let actions = gtk::ActionBar::new();
        let cancel = gtk::Button::with_label("Cancel");
        let apply = gtk::Button::with_label("Apply Live");
        actions.pack_end(&apply);
        actions.pack_end(&cancel);
        content.append(&actions);
        dialog.set_child(Some(&content));

        let cancel_dialog = dialog.clone();
        cancel.connect_clicked(move |_| cancel_dialog.close());
        let machine_dir = self.launch.machine_dir.clone();
        let parent = self.window.clone();
        let save_dialog = dialog.clone();
        apply.connect_clicked(move |_| {
            let mut updated = config.clone();
            updated.integrations.ports = rows
                .borrow()
                .iter()
                .map(|row| PortForward {
                    id: row.id,
                    enabled: row.enabled.is_active(),
                    direction: if row.direction.selected() == 1 {
                        PortDirection::GuestToHost
                    } else {
                        PortDirection::HostToGuest
                    },
                    protocol: if row.protocol.selected() == 1 {
                        PortProtocol::Udp
                    } else {
                        PortProtocol::Tcp
                    },
                    host_address: row.host_address.text().trim().to_owned(),
                    host_port: row.host_port.value_as_int() as u16,
                    guest_address: row.guest_address.text().trim().to_owned(),
                    guest_port: row.guest_port.value_as_int() as u16,
                })
                .collect();
            if let Err(error) = updated.save(&machine_dir) {
                show_error_dialog(&parent, "Could not apply live port mappings", &error);
                return;
            }
            save_dialog.close();
        });
        dialog.present();
    }

    fn open_media(&self) {
        let config = match MachineConfig::load(&self.launch.machine_dir) {
            Ok(config) => config,
            Err(error) => {
                self.show_error("Could not load media sharing settings", &error);
                return;
            }
        };
        let devices = match ResourceLocator::discover()
            .and_then(|resources| discover_host_media(&resources))
        {
            Ok(devices) => devices,
            Err(error) => {
                self.show_error("Could not discover host media devices", &error);
                return;
            }
        };
        let dialog = gtk::Window::builder()
            .title("Audio, Microphone and Camera")
            .transient_for(&self.window)
            .modal(true)
            .destroy_with_parent(true)
            .default_width(640)
            .default_height(520)
            .resizable(true)
            .build();
        dialog.set_titlebar(Some(&gtk::HeaderBar::new()));
        let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        let explanation = gtk::Label::new(Some(
            "Each switch is independent and applies live. Microphone and Camera authorize continuous capture while enabled, and the native Buzzard OS header labels each active input. Microphone activation completes only after the host PipeWire-Pulse session reports Buzzard OS as a running, uncorked recording stream, which drives GNOME and compatible desktop privacy indicators. Turning either switch off terminates capture, removes its private mapping, and removes the guest source. The host PipeWire socket is never mounted into the machine.",
        ));
        explanation.set_wrap(true);
        explanation.set_xalign(0.0);
        content.append(&explanation);

        let grid = gtk::Grid::builder()
            .row_spacing(14)
            .column_spacing(16)
            .hexpand(true)
            .build();
        let audio = gtk::Switch::builder()
            .active(config.integrations.media.guest_audio_output)
            .halign(gtk::Align::End)
            .build();
        let microphone = gtk::Switch::builder()
            .active(config.integrations.media.host_microphone)
            .halign(gtk::Align::End)
            .build();
        let camera = gtk::Switch::builder()
            .active(config.integrations.media.host_camera)
            .halign(gtk::Align::End)
            .build();
        let (audio_target, audio_targets) = media_device_dropdown(
            &devices,
            HostMediaKind::AudioSink,
            config.integrations.media.audio_target.as_deref(),
        );
        let (microphone_target, microphone_targets) = media_device_dropdown(
            &devices,
            HostMediaKind::Microphone,
            config.integrations.media.microphone_target.as_deref(),
        );
        let (camera_target, camera_targets) = media_device_dropdown(
            &devices,
            HostMediaKind::Camera,
            config.integrations.media.camera_target.as_deref(),
        );
        attach_setting(&grid, 0, "Guest audio → host speakers", &audio);
        attach_setting(&grid, 1, "Host audio output", &audio_target);
        attach_setting(&grid, 2, "Host microphone → guest", &microphone);
        attach_setting(&grid, 3, "Microphone device", &microphone_target);
        attach_setting(&grid, 4, "Host camera → guest", &camera);
        attach_setting(&grid, 5, "Camera device", &camera_target);
        content.append(&grid);

        let note = gtk::Label::new(Some(
            "Devices are discovered from the live host PipeWire graph. Microphone capture always uses the selected host PipeWire source through its desktop-accounted PipeWire-Pulse recording service, even when that source is backed by ALSA; no ALSA device is opened directly or exposed to the guest. Camera capture uses its validated host-advertised backend. All media bridges require Private user-mode network.",
        ));
        note.set_wrap(true);
        note.set_xalign(0.0);
        note.add_css_class("dim-label");
        content.append(&note);

        let actions = gtk::ActionBar::new();
        let cancel = gtk::Button::with_label("Cancel");
        let apply = gtk::Button::with_label("Apply Live");
        actions.pack_end(&apply);
        actions.pack_end(&cancel);
        content.append(&actions);
        dialog.set_child(Some(&content));

        let cancel_dialog = dialog.clone();
        cancel.connect_clicked(move |_| cancel_dialog.close());
        let machine_dir = self.launch.machine_dir.clone();
        let parent = self.window.clone();
        let save_dialog = dialog.clone();
        apply.connect_clicked(move |_| {
            let mut updated = config.clone();
            updated.integrations.media.guest_audio_output = audio.is_active();
            updated.integrations.media.host_microphone = microphone.is_active();
            updated.integrations.media.host_camera = camera.is_active();
            updated.integrations.media.audio_target =
                selected_media_target(&audio_target, &audio_targets);
            updated.integrations.media.microphone_target =
                selected_media_target(&microphone_target, &microphone_targets);
            updated.integrations.media.camera_target =
                selected_media_target(&camera_target, &camera_targets);
            if let Err(error) = updated.save(&machine_dir) {
                show_error_dialog(&parent, "Could not apply live media sharing", &error);
                return;
            }
            save_dialog.close();
        });
        dialog.present();
    }

    fn open_diagnostics(&self) {
        let stats = self.presentation.borrow().clone();
        let continuity = self.continuity.borrow();
        let mode = self.output_mode();
        let logical = format!("{} × {}", mode.logical_width, mode.logical_height);
        let physical = format!("{} × {}", mode.physical_width, mode.physical_height);
        let refresh = if self.refresh_mhz.get() == 0 {
            "Unknown".to_owned()
        } else {
            format!("{:.3} Hz", self.refresh_mhz.get() as f64 / 1000.0)
        };
        let dialog = gtk::Window::builder()
            .title("Display Diagnostics")
            .transient_for(&self.window)
            .modal(true)
            .destroy_with_parent(true)
            .default_width(720)
            .default_height(760)
            .resizable(true)
            .build();
        dialog.set_titlebar(Some(&gtk::HeaderBar::new()));
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        let grid = gtk::Grid::builder()
            .row_spacing(10)
            .column_spacing(16)
            .hexpand(true)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(&grid)
            .build();
        content.append(&scroll);
        add_diagnostic(&grid, 0, "Window boundary", "Native GTK4 application");
        add_diagnostic(&grid, 1, "Guest logical monitor", &logical);
        add_diagnostic(&grid, 2, "Physical dmabuf", &physical);
        add_diagnostic(
            &grid,
            3,
            "Host surface scale",
            &format!("{:.0}%", self.host_surface_scale_120.get() as f64 / 1.2),
        );
        add_diagnostic(
            &grid,
            4,
            "Guest desktop scale",
            &format!("{:.0}%", self.guest_ui_scale_120.get() as f64 / 1.2),
        );
        add_diagnostic(&grid, 5, "Host refresh", &refresh);
        add_diagnostic(
            &grid,
            6,
            "Transport",
            if stats.transport == "dmabuf" {
                "Direct Wayland dmabuf import (not a video/network stream)"
            } else {
                &stats.transport
            },
        );
        add_diagnostic(
            &grid,
            7,
            "Buffer",
            &format!(
                "{} × {}, DRM format {}, modifier {}, {} plane(s)",
                stats.width, stats.height, stats.format, stats.modifier, stats.planes
            ),
        );
        add_diagnostic(&grid, 8, "Explicit synchronization", &stats.explicit_sync);
        add_diagnostic(
            &grid,
            9,
            "GTK subsurface offload",
            if stats.gtk_subsurface_offload {
                "Active and observed in the rendered frame"
            } else if self.offload.enabled() == gtk::GraphicsOffloadEnabled::Enabled {
                "Requested, but not observed (fast path failed)"
            } else {
                "Disabled (fast path failed)"
            },
        );
        add_diagnostic(
            &grid,
            10,
            "Presentation",
            &format!(
                "presented={}, vblank={}, sequence={}, refresh={} µs",
                stats.presented, stats.vsync, stats.sequence, stats.last_refresh_interval_us
            ),
        );
        add_diagnostic(
            &grid,
            11,
            "Zero-copy fast path",
            if stats.presented
                && stats.vsync
                && stats.zero_copy
                && stats.gtk_subsurface_offload
                && stats.native_resolution
            {
                if stats.last_pacing_source == "internal-hidden-window-clock" {
                    "Proven for visible frames; the hidden host surface is not currently presenting"
                } else {
                    "Active: unchanged native-resolution dmabuf was offloaded and presented"
                }
            } else {
                "Failed or not yet proven; inspect the fields above"
            },
        );
        add_diagnostic(
            &grid,
            12,
            "Frame lifecycle",
            &format!(
                "submitted={}, consumed={}, host-presented={}, released={}",
                stats.submitted_frames,
                stats.painted_frames,
                stats.presented_frames,
                stats.released_frames
            ),
        );
        add_diagnostic(
            &grid,
            13,
            "Superseded before paint",
            &format!(
                "{} (a newer guest buffer replaced an unpainted one; idle time is never counted)",
                stats.superseded_before_paint
            ),
        );
        add_diagnostic(
            &grid,
            14,
            "Feedback unavailable",
            &format!(
                "{} frame timing record(s) aged out before Mutter completed them",
                stats.presentation_feedback_unavailable
            ),
        );
        add_diagnostic(
            &grid,
            15,
            "Acquire-fence wait",
            &format!(
                "last={} µs, maximum={} µs across {} explicit-sync frame(s)",
                stats.last_acquire_wait_us,
                stats.maximum_acquire_wait_us,
                stats.explicit_sync_frames
            ),
        );
        add_diagnostic(
            &grid,
            16,
            "Submit → presentation",
            &format!(
                "last={} µs, maximum={} µs",
                stats.last_submission_to_presentation_us,
                stats.maximum_submission_to_presentation_us
            ),
        );
        add_diagnostic(
            &grid,
            17,
            "Guest scanout pacing",
            &format!(
                "source={}, hidden-window frames={}, discarded feedback={}",
                stats.last_pacing_source,
                stats.background_paced_frames,
                stats.background_feedback_discarded
            ),
        );
        add_diagnostic(
            &grid,
            18,
            "Presented-frame interval",
            &format!(
                "last={} µs; compositor refresh interval={} µs",
                stats.last_presented_frame_interval_us, stats.last_refresh_interval_us
            ),
        );
        add_diagnostic(
            &grid,
            19,
            "Buffer release residency",
            &format!(
                "last={} µs, maximum={} µs; last released frame={}",
                stats.last_buffer_residency_us,
                stats.maximum_buffer_residency_us,
                stats.last_released_frame_id
            ),
        );
        add_diagnostic(
            &grid,
            20,
            "Monitor continuity",
            &format!(
                "stable-paintable={}, frames={}, replacements={}, cursor-observations={}, \
                 placeholder-exposures={}, blank-exposures={}",
                continuity.stable_paintable_identity,
                continuity.frames_installed,
                continuity.frame_replacements,
                continuity.cursor_observations,
                continuity.placeholder_exposures_between_frames,
                continuity.blank_exposures_between_frames,
            ),
        );
        let close = gtk::Button::with_label("Close");
        close.set_halign(gtk::Align::End);
        content.append(&close);
        dialog.set_child(Some(&content));
        dialog.set_default_widget(Some(&close));
        let close_dialog = dialog.clone();
        close.connect_clicked(move |_| close_dialog.close());
        dialog.present();
    }

    fn show_error(&self, heading: &str, error: &anyhow::Error) {
        show_error_dialog(&self.window, heading, error);
    }

    /// Captures only this application's GTK widget tree for deterministic
    /// visual QA. It cannot read surrounding host windows or the host desktop.
    fn capture_ui(&self) -> Result<()> {
        let logical_width = self.window.width().max(1);
        let logical_height = self.window.height().max(1);
        let paintable = gtk::WidgetPaintable::new(Some(&self.window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, logical_width as f64, logical_height as f64);
        let node = snapshot
            .to_node()
            .context("native application produced no render node")?;
        // GskRenderer::render_texture otherwise rasterizes at one pixel per
        // logical unit. That makes host chrome look plausible but downsamples
        // the native guest dmabuf in QA artifacts at fractional scale. Render
        // the complete host application at the surface's physical pixel size
        // so a 1600px guest buffer occupies exactly 1600 captured pixels.
        let scale_120 = self.host_surface_scale_120.get();
        let mapping = PixelMapping::new(logical_width as u32, logical_height as u32, scale_120)
            .context("host application dimensions exceed Wayland fixed-point coordinates")?;
        let physical_width = mapping.physical_width as i32;
        let physical_height = mapping.physical_height as i32;
        let physical_snapshot = gtk::Snapshot::new();
        physical_snapshot.scale(
            physical_width as f32 / logical_width as f32,
            physical_height as f32 / logical_height as f32,
        );
        physical_snapshot.append_node(&node);
        let physical_node = physical_snapshot
            .to_node()
            .context("native application produced no physical render node")?;
        let renderer = self
            .window
            .renderer()
            .context("native application has no active renderer")?;
        let viewport =
            gtk::graphene::Rect::new(0.0, 0.0, physical_width as f32, physical_height as f32);
        let texture = renderer.render_texture(&physical_node, Some(&viewport));
        let destination = self.launch.status_dir.join("host-ui.png");
        texture
            .save_to_png(&destination)
            .with_context(|| format!("saving {}", destination.display()))
    }

    fn save_window(&self) -> Result<()> {
        let state = self
            .gdk_toplevel()
            .map(|toplevel| toplevel.state())
            .unwrap_or_else(gdk::ToplevelState::empty);
        let value = WindowDiagnostics {
            schema: 3,
            boundary: "native-gtk4-application/graphics-offload-monitor".into(),
            toplevels: 1,
            width: self.viewport_width.get(),
            height: self.viewport_height.get(),
            title: self.launch.title.clone(),
            app_id: self.launch.app_id.clone(),
            decorations: "gtk4-native".into(),
            close_requested: self.close_requested.get(),
            maximized: state.contains(gdk::ToplevelState::MAXIMIZED),
            minimized: state.contains(gdk::ToplevelState::MINIMIZED),
            fullscreen: state.contains(gdk::ToplevelState::FULLSCREEN),
            focused: state.contains(gdk::ToplevelState::FOCUSED),
        };
        atomic_json(&self.launch.status_dir.join("window.json"), &value)
    }

    fn gdk_toplevel(&self) -> Option<gdk::Toplevel> {
        self.window
            .surface()
            .and_then(|surface| surface.dynamic_cast::<gdk::Toplevel>().ok())
    }

    fn save_output_state(&self) -> Result<()> {
        let host_surface_scale_120 = self.host_surface_scale_120.get();
        let guest_ui_scale_120 = self.guest_ui_scale_120.get();
        let mapping = PixelMapping::new(
            self.viewport_width.get(),
            self.viewport_height.get(),
            host_surface_scale_120,
        )
        .context("host viewport exceeds supported Wayland buffer dimensions")?;
        let physical_width = mapping.physical_width;
        let physical_height = mapping.physical_height;
        let logical_width = guest_logical_dimension(physical_width, guest_ui_scale_120);
        let logical_height = guest_logical_dimension(physical_height, guest_ui_scale_120);
        let value = serde_json::json!({
            "schema": 7,
            "physical_width": physical_width,
            "physical_height": physical_height,
            "host_surface_scale_120": host_surface_scale_120,
            "guest_ui_scale_120": guest_ui_scale_120,
            "logical_width": logical_width,
            "logical_height": logical_height,
            "geometry_generation": self.geometry_generation.get(),
        });
        atomic_json(
            &self.launch.output_state_dir.join("output-state.json"),
            &value,
        )
    }

    fn save_presentation(&self) -> Result<()> {
        atomic_json(
            &self.launch.status_dir.join("presentation.json"),
            &*self.presentation.borrow(),
        )
    }
}

const HOST_CLIPBOARD_DEADLINE: Duration = Duration::from_secs(5);

struct ZeroizingBytes(Vec<u8>);

impl AsRef<[u8]> for ZeroizingBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for ZeroizingBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct KillSubprocessOnDrop {
    process: gio::Subprocess,
    armed: bool,
}

impl KillSubprocessOnDrop {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for KillSubprocessOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.process.force_exit();
        }
    }
}

const HOST_TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];
const HOST_IMAGE_MIMES: [&str; 9] = [
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/webp",
    "image/bmp",
    "image/x-bmp",
    "image/tiff",
    "image/tif",
    "image/x-tiff",
];

async fn read_host_clipboard_snapshot() -> Result<ClipboardValue> {
    let display = gdk::Display::default().context("GTK has no active host display")?;
    let host_clipboard = display.clipboard();
    let formats = host_clipboard.formats();
    let offered = formats.mime_types();
    let offers_text = offered
        .iter()
        .any(|mime| is_supported_text_offer(mime.as_str()));
    // GTK clipboard owners are allowed to advertise an in-process GdkTexture
    // instead of a pre-encoded image MIME. Add every MIME for which GDK has a
    // registered serializer before deciding whether this ordinary clipboard
    // value is a supported image. The subsequent read remains asynchronous,
    // so encoding a native screenshot cannot freeze the host window.
    let serializable_formats = formats.union_serialize_mime_types();
    let offers_image = HOST_IMAGE_MIMES
        .iter()
        .any(|mime| serializable_formats.contain_mime_type(mime));

    let operation = async move {
        if offers_text {
            let text = host_clipboard
                .read_future(&HOST_TEXT_MIMES, glib::Priority::DEFAULT)
                .await
                .context("reading the clicked host plain-text clipboard snapshot");
            match text {
                Ok((stream, actual_mime)) => {
                    let value = if is_supported_text_offer(actual_mime.as_str()) {
                        read_bounded_stream(stream, MAX_TEXT_BYTES)
                            .await
                            .and_then(clipboard::validated_text)
                    } else {
                        Err(anyhow::anyhow!(
                            "host clipboard returned an unrequested text representation"
                        ))
                    };
                    if value.is_ok() || !offers_image {
                        return value;
                    }
                }
                Err(error) if !offers_image => return Err(error),
                Err(_) => {}
            }
        }

        if !offers_image {
            anyhow::bail!("the host clipboard has no supported plain text or still image");
        }

        // Request an ordinary serialized still image. GDK performs registered
        // GdkTexture/Pixbuf serialization when the source is a native toolkit
        // screenshot, and selects a directly offered MIME for JPEG/WebP/BMP/
        // TIFF sources. The source therefore never needs to have originated
        // as a PNG file.
        let (stream, actual_mime) = host_clipboard
            .read_future(&HOST_IMAGE_MIMES, glib::Priority::DEFAULT)
            .await
            .context("the host clipboard has no supported plain text or still image")?;
        if !is_supported_image_offer(actual_mime.as_str()) {
            anyhow::bail!("host clipboard returned an unrequested image representation");
        }
        let source = read_bounded_stream(stream, MAX_IMAGE_BYTES).await?;
        canonicalize_host_clipboard_image(source).await
    };
    let timeout = glib::timeout_future(HOST_CLIPBOARD_DEADLINE);
    futures_util::pin_mut!(operation, timeout);
    match select(operation, timeout).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => {
            anyhow::bail!("host clipboard source did not answer within 5 seconds")
        }
    }
}

async fn canonicalize_host_clipboard_image(source: Vec<u8>) -> Result<ClipboardValue> {
    let output =
        run_image_subprocess(clipboard::IMAGE_WORKER_PNG_ARG, source, MAX_IMAGE_BYTES).await?;
    clipboard::png_from_image_worker(output)
}

async fn decode_host_clipboard_image(source: Vec<u8>) -> Result<clipboard::DecodedImage> {
    let raw_limit = 32_usize
        .checked_add(
            usize::try_from(buzzardos_clipboard_protocol::MAX_IMAGE_PIXELS)
                .context("clipboard pixel limit is not representable")?
                .checked_mul(4)
                .context("clipboard raw-image limit overflow")?,
        )
        .context("clipboard raw-image limit overflow")?;
    let output = run_image_subprocess(clipboard::IMAGE_WORKER_RAW_ARG, source, raw_limit).await?;
    clipboard::decoded_from_image_worker(output)
}

async fn run_image_subprocess(mode: &str, source: Vec<u8>, output_limit: usize) -> Result<Vec<u8>> {
    let executable = std::env::current_exe().context("locating clipboard image worker")?;
    let arguments = [executable.as_os_str(), std::ffi::OsStr::new(mode)];
    let process = gio::Subprocess::newv(
        &arguments,
        gio::SubprocessFlags::STDIN_PIPE
            | gio::SubprocessFlags::STDOUT_PIPE
            | gio::SubprocessFlags::STDERR_SILENCE,
    )
    .context("starting confined clipboard image worker")?;
    let mut guard = KillSubprocessOnDrop {
        process: process.clone(),
        armed: true,
    };
    let input = glib::Bytes::from_owned(ZeroizingBytes(source));
    let communication = process.communicate_future(Some(&input));
    let timeout = glib::timeout_future(HOST_CLIPBOARD_DEADLINE);
    futures_util::pin_mut!(communication, timeout);
    let output = match select(communication, timeout).await {
        Either::Left((result, _)) => {
            let (stdout, _) = result.context("communicating with clipboard image worker")?;
            if !process.is_successful() {
                anyhow::bail!("clipboard image worker rejected the still image");
            }
            guard.disarm();
            stdout.context("clipboard image worker returned no output")?
        }
        Either::Right(((), _)) => {
            process.force_exit();
            // Reap the fixed child after SIGKILL. This future normally
            // resolves immediately; a second bound prevents any platform
            // anomaly from stalling the GTK main context.
            let wait = process.wait_future();
            let reap_timeout = glib::timeout_future(Duration::from_secs(1));
            futures_util::pin_mut!(wait, reap_timeout);
            let _ = select(wait, reap_timeout).await;
            anyhow::bail!("clipboard image conversion exceeded its 5-second deadline");
        }
    };
    let mut output = output.into_data();
    if output.len() > output_limit {
        output.fill(0);
        anyhow::bail!("clipboard image worker exceeded its bounded output size");
    }
    let value = output.to_vec();
    output.fill(0);
    Ok(value)
}

fn is_supported_text_offer(offered: &str) -> bool {
    offered.eq_ignore_ascii_case("text/plain;charset=utf-8")
        || offered.eq_ignore_ascii_case("text/plain")
        || matches!(offered, "UTF8_STRING" | "TEXT" | "STRING")
}

fn is_supported_image_offer(offered: &str) -> bool {
    HOST_IMAGE_MIMES
        .iter()
        .any(|supported| offered.eq_ignore_ascii_case(supported))
}

async fn read_bounded_stream(stream: gio::InputStream, limit: usize) -> Result<Vec<u8>> {
    const CHUNK: usize = 64 * 1024;
    let mut value = ZeroizingBytes(Vec::new());
    loop {
        let allowance = limit.saturating_add(1).saturating_sub(value.0.len());
        if allowance == 0 {
            anyhow::bail!("clipboard source exceeds its {limit}-byte limit");
        }
        let chunk = stream
            .read_bytes_future(allowance.min(CHUNK), glib::Priority::DEFAULT)
            .await
            .context("reading clipboard source bytes")?;
        if chunk.is_empty() {
            break;
        }
        value.0.extend_from_slice(&chunk);
        if value.0.len() > limit {
            anyhow::bail!("clipboard source exceeds its {limit}-byte limit");
        }
    }
    Ok(std::mem::take(&mut value.0))
}

const HEADER_MENU_LABELS: [&str; 4] = ["Machine", "Ports", "Devices", "Clipboard"];
const MACHINE_LIFECYCLE_MENU_ITEMS: [(&str, &str); 3] = [
    ("Start", "app.start"),
    ("Stop", "app.stop"),
    ("Restart", "app.restart"),
];
const MACHINE_WINDOW_MENU_ITEMS: [(&str, &str); 2] = [
    ("Shut Down Machine", "app.shutdown"),
    ("Close Window", "app.close"),
];
const PORTS_MENU_ITEMS: [(&str, &str); 1] = [("Configure Live Port Mappings…", "app.ports")];
const DEVICES_MENU_ITEMS: [(&str, &str); 1] = [("Audio, Microphone and Camera…", "app.media")];
const CLIPBOARD_MENU_ITEMS: [(&str, &str); 2] = [
    ("Send Host Clipboard to Guest", "app.clipboard-to-guest"),
    ("Copy Guest Clipboard to Host", "app.clipboard-to-host"),
];
const SETTINGS_MENU_ITEMS: [(&str, &str); 2] = [
    ("Machine Settings…", "app.settings"),
    ("Display Diagnostics…", "app.diagnostics"),
];

fn build_header_controls() -> gtk::Box {
    let machine = gio::Menu::new();
    append_menu_items(&machine, &MACHINE_LIFECYCLE_MENU_ITEMS);
    let shutdown = gio::Menu::new();
    append_menu_items(&shutdown, &MACHINE_WINDOW_MENU_ITEMS);
    machine.append_section(None, &shutdown);
    let settings = gio::Menu::new();
    append_menu_items(&settings, &SETTINGS_MENU_ITEMS);
    machine.append_section(None, &settings);

    let ports = gio::Menu::new();
    append_menu_items(&ports, &PORTS_MENU_ITEMS);

    let devices = gio::Menu::new();
    append_menu_items(&devices, &DEVICES_MENU_ITEMS);

    let clipboard = gio::Menu::new();
    append_menu_items(&clipboard, &CLIPBOARD_MENU_ITEMS);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    controls.set_valign(gtk::Align::Center);
    for (label, tooltip, model) in [
        (
            HEADER_MENU_LABELS[0],
            "Machine lifecycle and window actions",
            machine,
        ),
        (HEADER_MENU_LABELS[1], "Configure live port mappings", ports),
        (
            HEADER_MENU_LABELS[2],
            "Configure guest audio, microphone and camera access",
            devices,
        ),
        (
            HEADER_MENU_LABELS[3],
            "Explicit one-shot text or image clipboard transfer",
            clipboard,
        ),
    ] {
        let button = gtk::MenuButton::builder()
            .label(label)
            .menu_model(&model)
            .can_shrink(true)
            .has_frame(false)
            .tooltip_text(tooltip)
            .build();
        controls.append(&button);
    }
    controls
}

fn append_menu_items(menu: &gio::Menu, items: &[(&str, &str)]) {
    for (label, action) in items {
        menu.append(Some(label), Some(action));
    }
}

fn append_port_editor(
    list: &gtk::ListBox,
    rows: &Rc<RefCell<Vec<PortEditorRow>>>,
    mapping: PortForward,
) {
    let row = gtk::ListBoxRow::new();
    let grid = gtk::Grid::builder()
        .column_spacing(8)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    let enabled = gtk::Switch::builder().active(mapping.enabled).build();
    let direction = gtk::DropDown::from_strings(&["Host → Guest", "Guest → Host"]);
    direction.set_selected(if mapping.direction == PortDirection::GuestToHost {
        1
    } else {
        0
    });
    let protocol = gtk::DropDown::from_strings(&["TCP", "UDP"]);
    protocol.set_selected(if mapping.protocol == PortProtocol::Udp {
        1
    } else {
        0
    });
    let host_address = gtk::Entry::builder()
        .text(&mapping.host_address)
        .width_chars(16)
        .build();
    let host_port = gtk::SpinButton::with_range(1.0, 65_535.0, 1.0);
    host_port.set_value(mapping.host_port as f64);
    host_port.set_width_chars(7);
    let guest_address = gtk::Entry::builder()
        .text(&mapping.guest_address)
        .width_chars(16)
        .build();
    let guest_port = gtk::SpinButton::with_range(1.0, 65_535.0, 1.0);
    guest_port.set_value(mapping.guest_port as f64);
    guest_port.set_width_chars(7);
    let remove = gtk::Button::from_icon_name("edit-delete-symbolic");
    remove.set_tooltip_text(Some("Remove mapping"));
    for (column, widget) in [
        enabled.clone().upcast::<gtk::Widget>(),
        direction.clone().upcast(),
        protocol.clone().upcast(),
        host_address.clone().upcast(),
        host_port.clone().upcast(),
        guest_address.clone().upcast(),
        guest_port.clone().upcast(),
        remove.clone().upcast(),
    ]
    .into_iter()
    .enumerate()
    {
        grid.attach(&widget, column as i32, 0, 1, 1);
    }
    row.set_child(Some(&grid));
    list.append(&row);
    let editor = PortEditorRow {
        id: mapping.id,
        row: row.clone(),
        enabled,
        direction,
        protocol,
        host_address,
        host_port,
        guest_address,
        guest_port,
    };
    rows.borrow_mut().push(editor);
    let rows_for_remove = Rc::clone(rows);
    let list_for_remove = list.clone();
    let id = mapping.id;
    remove.connect_clicked(move |_| {
        if let Some(index) = rows_for_remove
            .borrow()
            .iter()
            .position(|candidate| candidate.id == id)
        {
            let removed = rows_for_remove.borrow_mut().remove(index);
            list_for_remove.remove(&removed.row);
        }
    });
}

fn media_device_dropdown(
    devices: &[HostMediaDevice],
    kind: HostMediaKind,
    current: Option<&str>,
) -> (gtk::DropDown, Vec<Option<String>>) {
    let matching: Vec<_> = devices
        .iter()
        .filter(|device| device.kind == kind)
        .collect();
    let default = matching.iter().find(|device| device.is_default);
    let mut labels = vec![default.map_or_else(
        || "System default — no device currently advertised".to_owned(),
        |device| format!("System default — {}", device.description),
    )];
    let mut targets = vec![None];
    for device in matching {
        let duplicate_description = devices.iter().any(|other| {
            other.kind == kind
                && other.node_name != device.node_name
                && other.description == device.description
        });
        let mut label = if duplicate_description {
            format!("{} — {}", device.description, device.node_name)
        } else {
            device.description.clone()
        };
        if device.is_default {
            label.push_str(" (default)");
        }
        labels.push(label);
        targets.push(Some(device.node_name.clone()));
    }
    if let Some(current) = current
        && !targets.iter().flatten().any(|target| target == current)
    {
        labels.push(format!("Unavailable — {current}"));
        targets.push(Some(current.to_owned()));
    }
    let labels: Vec<_> = labels.iter().map(String::as_str).collect();
    let dropdown = gtk::DropDown::from_strings(&labels);
    dropdown.set_hexpand(true);
    let selected = current
        .and_then(|current| {
            targets
                .iter()
                .position(|target| target.as_deref() == Some(current))
        })
        .unwrap_or(0);
    dropdown.set_selected(u32::try_from(selected).unwrap_or(0));
    (dropdown, targets)
}

fn selected_media_target(dropdown: &gtk::DropDown, targets: &[Option<String>]) -> Option<String> {
    usize::try_from(dropdown.selected())
        .ok()
        .and_then(|index| targets.get(index))
        .cloned()
        .flatten()
}

fn attach_setting(grid: &gtk::Grid, row: i32, name: &str, value: &impl IsA<gtk::Widget>) {
    let label = gtk::Label::new(Some(name));
    label.set_xalign(0.0);
    label.set_mnemonic_widget(Some(value));
    grid.attach(&label, 0, row, 1, 1);
    grid.attach(value, 1, row, 1, 1);
}

fn machine_location_control(parent: &gtk::Window, machine_dir: &std::path::Path) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let location = gtk::Entry::builder()
        .text(machine_dir.to_string_lossy())
        .editable(false)
        .hexpand(true)
        .tooltip_text("The machine location is fixed; move a stopped machine folder and re-register it to change this path")
        .build();
    let open = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .label("Open Folder")
        .tooltip_text("Open this machine folder in the system file manager")
        .build();
    row.append(&location);
    row.append(&open);

    let parent = parent.clone();
    let machine_dir = machine_dir.to_path_buf();
    open.connect_clicked(move |_| {
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&machine_dir)));
        let parent = parent.clone();
        glib::spawn_future_local(async move {
            if let Err(error) = launcher.launch_future(Some(&parent)).await {
                show_error_dialog(
                    &parent,
                    "Could not open the machine folder",
                    &anyhow::Error::new(error),
                );
            }
        });
    });
    row
}

fn add_diagnostic(grid: &gtk::Grid, row: i32, name: &str, value: &str) {
    let name = gtk::Label::new(Some(name));
    name.set_xalign(0.0);
    name.add_css_class("dim-label");
    let value = gtk::Label::new(Some(value));
    value.set_xalign(0.0);
    value.set_selectable(true);
    value.set_wrap(true);
    grid.attach(&name, 0, row, 1, 1);
    grid.attach(&value, 1, row, 1, 1);
}

fn show_error_dialog(parent: &impl IsA<gtk::Window>, heading: &str, error: &anyhow::Error) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(heading)
        .detail(format!("{error:#}"))
        .buttons(["Close"])
        .cancel_button(0)
        .default_button(0)
        .build();
    dialog.show(Some(parent));
}

fn show_info_dialog(parent: &gtk::ApplicationWindow, heading: &str, detail: &str) {
    let dialog = gtk::AlertDialog::builder()
        // A modal GTK alert temporarily prevents the application-owned GDK
        // clipboard provider from answering a host client's data request.
        // Successful clipboard transfer feedback must never make the newly
        // installed clipboard appear to hang until the dialog is dismissed.
        .modal(false)
        .message(heading)
        .detail(detail)
        .buttons(["Close"])
        .cancel_button(0)
        .default_button(0)
        .build();
    dialog.show(Some(parent));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelMapping {
    physical_width: u32,
    physical_height: u32,
    width_remainder: u32,
    height_remainder: u32,
}

impl PixelMapping {
    fn new(logical_width: u32, logical_height: u32, scale_120: u32) -> Option<Self> {
        if logical_width > MAX_WAYLAND_FIXED_EXTENT || logical_height > MAX_WAYLAND_FIXED_EXTENT {
            return None;
        }
        let (physical_width, width_remainder) = scaled_axis(logical_width, scale_120)?;
        let (physical_height, height_remainder) = scaled_axis(logical_height, scale_120)?;
        if physical_width > MAX_WAYLAND_FIXED_EXTENT || physical_height > MAX_WAYLAND_FIXED_EXTENT {
            return None;
        }
        Some(Self {
            physical_width,
            physical_height,
            width_remainder,
            height_remainder,
        })
    }

    #[cfg(test)]
    fn is_integral(self) -> bool {
        self.width_remainder == 0 && self.height_remainder == 0
    }
}

fn scaled_axis(logical: u32, scale_120: u32) -> Option<(u32, u32)> {
    if logical == 0 || scale_120 == 0 {
        return None;
    }
    let product = u64::from(logical).checked_mul(u64::from(scale_120))?;
    let physical = product.div_ceil(u64::from(WAYLAND_SCALE_DENOMINATOR));
    let physical = u32::try_from(physical).ok()?;
    (physical <= MAX_WAYLAND_FIXED_EXTENT).then_some((
        physical,
        (product % u64::from(WAYLAND_SCALE_DENOMINATOR)) as u32,
    ))
}

fn frame_has_exact_native_mapping(
    frame_width: u32,
    frame_height: u32,
    viewport_width: u32,
    viewport_height: u32,
    scale_120: u32,
) -> bool {
    scale_120 != 0
        && u64::from(frame_width) * u64::from(WAYLAND_SCALE_DENOMINATOR)
            == u64::from(viewport_width) * u64::from(scale_120)
        && u64::from(frame_height) * u64::from(WAYLAND_SCALE_DENOMINATOR)
            == u64::from(viewport_height) * u64::from(scale_120)
}

fn guest_logical_dimension(physical: u32, guest_scale_120: u32) -> u32 {
    u64::from(physical)
        .saturating_mul(120)
        .checked_div(u64::from(guest_scale_120.max(1)))
        .unwrap_or(1)
        .clamp(1, u64::from(u32::MAX)) as u32
}

fn effective_scale_120(test_override: Option<u32>, host_scale: f64) -> Result<u32, String> {
    if let Some(scale_120) = test_override {
        return (scale_120 != 0)
            .then_some(scale_120)
            .ok_or_else(|| "test Wayland scale override must not be zero".into());
    }
    if !host_scale.is_finite() || host_scale <= 0.0 {
        return Err(format!(
            "host Wayland surface reported invalid scale {host_scale}"
        ));
    }
    let protocol_units = host_scale * f64::from(WAYLAND_SCALE_DENOMINATOR);
    let rounded = protocol_units.round();
    // GDK exposes the protocol's integer 1/120 unit through an f64 scale.
    // Permit only IEEE-754 round-trip error, not an arbitrary near-by scale.
    let round_trip_tolerance = f64::EPSILON * rounded.abs().max(1.0) * 2.0;
    if (protocol_units - rounded).abs() > round_trip_tolerance
        || rounded < 1.0
        || rounded > f64::from(u32::MAX)
    {
        return Err(format!(
            "host Wayland surface scale {host_scale:.12} is not representable in exact 1/120 units"
        ));
    }
    Ok(rounded as u32)
}

fn greatest_common_divisor(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

fn scale_denominator(scale_120: u32) -> Option<u32> {
    (scale_120 != 0).then(|| {
        WAYLAND_SCALE_DENOMINATOR / greatest_common_divisor(scale_120, WAYLAND_SCALE_DENOMINATOR)
    })
}

fn is_integral_coordinate(value: f64) -> bool {
    value.is_finite() && (value - value.round()).abs() <= 1.0e-6
}

fn is_integral_device_coordinate(logical: f64, scale_120: u32) -> bool {
    if scale_120 == 0 {
        return false;
    }
    is_integral_coordinate(logical * f64::from(scale_120) / f64::from(WAYLAND_SCALE_DENOMINATOR))
}

fn aligned_origin_margin(origin: f64, scale_120: u32) -> Option<i32> {
    if !origin.is_finite() {
        return None;
    }
    let denominator = scale_denominator(scale_120)?;
    (0..denominator).find_map(|margin| {
        let logical = origin + f64::from(margin);
        (is_integral_coordinate(logical) && is_integral_device_coordinate(logical, scale_120))
            .then_some(margin as i32)
    })
}

fn align_extent_up(extent: u32, denominator: u32) -> Option<u32> {
    if extent == 0 || denominator == 0 {
        return None;
    }
    let remainder = extent % denominator;
    if remainder == 0 {
        Some(extent)
    } else {
        extent.checked_add(denominator - remainder)
    }
}

fn trailing_extent_margin(wrapper_extent: i32, leading: i32, denominator: i32) -> Option<i32> {
    if wrapper_extent <= leading || denominator <= 0 {
        return None;
    }
    let available = wrapper_extent - leading;
    let trailing = available.rem_euclid(denominator);
    (available - trailing > 0).then_some(trailing)
}

fn map_monitor_coordinate(value: f64, from_extent: u32, to_extent: u32) -> f64 {
    assert!((1..=MAX_WAYLAND_FIXED_EXTENT).contains(&from_extent));
    assert!((1..=MAX_WAYLAND_FIXED_EXTENT).contains(&to_extent));
    let from_fixed = coordinate_to_fixed(value, from_extent);
    let numerator = i128::from(from_fixed) * i128::from(to_extent);
    let denominator = i128::from(from_extent);
    let to_fixed =
        ((numerator + denominator / 2) / denominator).clamp(0, i128::from(i32::MAX)) as i32;
    fixed_to_coordinate(to_fixed.min(max_fixed_coordinate(to_extent)))
}

fn coordinate_to_fixed(value: f64, extent: u32) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    (value * WAYLAND_FIXED_DENOMINATOR as f64)
        .round()
        .clamp(0.0, f64::from(max_fixed_coordinate(extent))) as i32
}

fn fixed_to_coordinate(value: i32) -> f64 {
    value as f64 / WAYLAND_FIXED_DENOMINATOR as f64
}

fn max_fixed_coordinate(extent: u32) -> i32 {
    i64::from(extent.min(MAX_WAYLAND_FIXED_EXTENT))
        .saturating_mul(WAYLAND_FIXED_DENOMINATOR)
        .saturating_sub(1)
        .min(i64::from(i32::MAX)) as i32
}

fn corrected_window_size(
    window_width: i32,
    window_height: i32,
    viewport_width: u32,
    viewport_height: u32,
    target_width: u32,
    target_height: u32,
) -> (i32, i32) {
    let width_delta = i64::from(target_width) - i64::from(viewport_width);
    let height_delta = i64::from(target_height) - i64::from(viewport_height);
    (
        (i64::from(window_width) + width_delta).clamp(1, i64::from(i32::MAX)) as i32,
        (i64::from(window_height) + height_delta).clamp(1, i64::from(i32::MAX)) as i32,
    )
}

fn linux_pointer_button(button: u32) -> Option<u32> {
    match button {
        1 => Some(0x110), // BTN_LEFT
        2 => Some(0x112), // BTN_MIDDLE
        3 => Some(0x111), // BTN_RIGHT
        4..=8 => Some(0x113 + (button - 4)),
        _ => None,
    }
}

fn xkb_modifiers(modifiers: gdk::ModifierType) -> u32 {
    let mut mask = 0_u32;
    if modifiers.contains(gdk::ModifierType::SHIFT_MASK) {
        mask |= 1 << 0;
    }
    if modifiers.contains(gdk::ModifierType::LOCK_MASK) {
        mask |= 1 << 1;
    }
    if modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
        mask |= 1 << 2;
    }
    if modifiers.contains(gdk::ModifierType::ALT_MASK) {
        mask |= 1 << 3;
    }
    if modifiers.intersects(gdk::ModifierType::SUPER_MASK | gdk::ModifierType::META_MASK) {
        mask |= 1 << 6;
    }
    mask
}

fn refresh_interval(refresh_mhz: u32) -> Duration {
    let refresh_mhz = if refresh_mhz == 0 {
        DEFAULT_REFRESH_MHZ
    } else {
        refresh_mhz
    };
    Duration::from_nanos(1_000_000_000_000_u64 / u64::from(refresh_mhz))
}

fn monotonic_us() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a valid writable timespec.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
    }
    (time.tv_sec as u64 * 1_000_000) + (time.tv_nsec as u64 / 1_000)
}

fn header_status_text(state: MonitorState, microphone_active: bool, camera_active: bool) -> String {
    let mut text = state.label().to_owned();
    if microphone_active {
        text.push_str(" · Microphone recording");
    }
    if camera_active {
        text.push_str(" · Camera recording");
    }
    text
}

fn atomic_json(path: &std::path::Path, value: &impl serde::Serialize) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).context("serializing display state")?;
    let mut output = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("opening {}", temporary.display()))?;
    output
        .set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("securing {}", temporary.display()))?;
    output
        .write_all(&bytes)
        .with_context(|| format!("writing {}", temporary.display()))?;
    output
        .sync_all()
        .with_context(|| format!("syncing {}", temporary.display()))?;
    drop(output);
    fs::rename(&temporary, path).with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_json_is_private_even_with_a_permissive_process_umask() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("output-state.json");
        atomic_json(&path, &serde_json::json!({"schema": 7})).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn header_menus_keep_every_host_action_in_the_requested_order() {
        assert_eq!(
            HEADER_MENU_LABELS,
            ["Machine", "Ports", "Devices", "Clipboard"]
        );
        assert_eq!(
            MACHINE_LIFECYCLE_MENU_ITEMS,
            [
                ("Start", "app.start"),
                ("Stop", "app.stop"),
                ("Restart", "app.restart"),
            ]
        );
        assert_eq!(
            MACHINE_WINDOW_MENU_ITEMS,
            [
                ("Shut Down Machine", "app.shutdown"),
                ("Close Window", "app.close"),
            ]
        );
        assert_eq!(
            PORTS_MENU_ITEMS,
            [("Configure Live Port Mappings…", "app.ports")]
        );
        assert_eq!(
            DEVICES_MENU_ITEMS,
            [("Audio, Microphone and Camera…", "app.media")]
        );
        assert_eq!(
            CLIPBOARD_MENU_ITEMS,
            [
                ("Send Host Clipboard to Guest", "app.clipboard-to-guest"),
                ("Copy Guest Clipboard to Host", "app.clipboard-to-host"),
            ]
        );
        assert_eq!(
            SETTINGS_MENU_ITEMS,
            [
                ("Machine Settings…", "app.settings"),
                ("Display Diagnostics…", "app.diagnostics"),
            ]
        );
    }

    #[test]
    fn header_always_discloses_active_host_capture() {
        assert_eq!(
            header_status_text(MonitorState::Running, true, true),
            "Running · Microphone recording · Camera recording"
        );
        assert_eq!(
            header_status_text(MonitorState::Running, true, false),
            "Running · Microphone recording"
        );
        assert_eq!(
            header_status_text(MonitorState::Stopped, false, false),
            "Stopped"
        );
    }

    #[test]
    fn attached_monitor_never_exposes_placeholder_or_blank_across_frames() {
        let mut continuity = MonitorContinuityDiagnostics::default();
        continuity.record_frame_installed(1);
        assert!(!continuity.observe("frame-installed", false, true));
        for _ in 0..128 {
            assert!(!continuity.observe("cursor", false, true));
        }
        continuity.record_frame_installed(2);
        assert!(!continuity.observe("frame-installed", false, true));
        assert_eq!(continuity.frames_installed, 2);
        assert_eq!(continuity.frame_replacements, 1);
        assert_eq!(continuity.cursor_observations, 128);
        assert_eq!(continuity.placeholder_exposures_between_frames, 0);
        assert_eq!(continuity.blank_exposures_between_frames, 0);
        assert!(continuity.stable_paintable_identity);
    }

    #[test]
    fn continuity_instrumentation_detects_both_regression_classes() {
        let mut continuity = MonitorContinuityDiagnostics::default();
        continuity.record_frame_installed(41);
        assert!(continuity.observe("lifecycle-ui", true, true));
        assert!(continuity.observe("paintable-identity", false, false));
        continuity.paintable_identity_changed();
        assert_eq!(continuity.placeholder_exposures_between_frames, 1);
        assert_eq!(continuity.blank_exposures_between_frames, 1);
        assert_eq!(continuity.paintable_identity_changes, 1);
        assert!(!continuity.stable_paintable_identity);

        // Intentional teardown ends the continuity interval before lifecycle
        // content replaces the monitor, so it is not a false positive.
        continuity.detach("guest-disconnected");
        assert!(!continuity.observe("lifecycle-ui", true, false));
        assert_eq!(continuity.placeholder_exposures_between_frames, 1);
        assert_eq!(continuity.blank_exposures_between_frames, 1);
    }

    #[test]
    fn fractional_scale_is_recovered_from_exact_protocol_units() {
        assert_eq!(effective_scale_120(None, 4.0 / 3.0).unwrap(), 160);
        assert_eq!(effective_scale_120(Some(180), 4.0 / 3.0).unwrap(), 180);
        assert!(effective_scale_120(Some(0), 1.0).is_err());
        assert!(effective_scale_120(None, f64::NAN).is_err());
        assert!(effective_scale_120(None, 1.331).is_err());
        assert!(effective_scale_120(None, 160.25 / 120.0).is_err());
        for protocol_units in 120..=960 {
            let gdk_scale = f64::from(protocol_units) / 120.0;
            assert_eq!(
                effective_scale_120(None, gdk_scale).unwrap(),
                protocol_units
            );
        }
        assert_eq!(guest_logical_dimension(1600, 150), 1280);
        // Sway floors a fractional output's logical rectangle. Mirror that
        // exact value so output synchronization converges instead of
        // reapplying the same mode after every output event.
        assert_eq!(guest_logical_dimension(1707, 150), 1365);
    }

    #[test]
    fn scale_matrix_distinguishes_allocation_from_integral_pixel_identity() {
        let cases = [
            (120, 1280, 800, true),
            (150, 1600, 1000, true),
            (160, 1707, 1067, false),
            (180, 1920, 1200, true),
            (210, 2240, 1400, true),
            (240, 2560, 1600, true),
        ];
        for (scale_120, physical_width, physical_height, integral) in cases {
            let mapping = PixelMapping::new(1280, 800, scale_120).unwrap();
            assert_eq!(
                (mapping.physical_width, mapping.physical_height),
                (physical_width, physical_height)
            );
            assert_eq!(mapping.is_integral(), integral);
            assert_eq!(
                frame_has_exact_native_mapping(
                    physical_width,
                    physical_height,
                    1280,
                    800,
                    scale_120,
                ),
                integral
            );
        }
    }

    #[test]
    fn host_and_guest_scale_matrix_keeps_one_native_physical_framebuffer() {
        let presets = [
            GuestScalePreset::Automatic,
            GuestScalePreset::Percent100,
            GuestScalePreset::Percent125,
            GuestScalePreset::Percent150,
            GuestScalePreset::Percent175,
            GuestScalePreset::Percent200,
        ];
        for host_scale_120 in [120_u32, 150, 160, 180, 210, 240] {
            let mapping = PixelMapping::new(1279, 799, host_scale_120).unwrap();
            let physical = (mapping.physical_width, mapping.physical_height);
            for preset in presets {
                let guest_ui_scale_120 = preset.resolve(host_scale_120);
                assert_eq!(
                    (mapping.physical_width, mapping.physical_height),
                    physical,
                    "guest preset {preset:?} changed the host-derived framebuffer"
                );
                assert_eq!(
                    guest_logical_dimension(mapping.physical_width, guest_ui_scale_120),
                    u64::from(mapping.physical_width)
                        .saturating_mul(120)
                        .checked_div(u64::from(guest_ui_scale_120))
                        .unwrap() as u32
                );
                assert_eq!(
                    guest_logical_dimension(mapping.physical_height, guest_ui_scale_120),
                    u64::from(mapping.physical_height)
                        .saturating_mul(120)
                        .checked_div(u64::from(guest_ui_scale_120))
                        .unwrap() as u32
                );
            }
        }
    }

    #[test]
    fn arbitrary_resizes_use_ceiling_allocation_but_exact_identity_only() {
        let scales = [120, 150, 160, 180, 210, 240];
        let viewports = [
            (1, 1),
            (319, 241),
            (853, 479),
            (1279, 799),
            (1280, 800),
            (4093, 2161),
        ];
        for scale_120 in scales {
            for (width, height) in viewports {
                let mapping = PixelMapping::new(width, height, scale_120).unwrap();
                let width_product = u64::from(width) * u64::from(scale_120);
                let height_product = u64::from(height) * u64::from(scale_120);
                assert_eq!(
                    u64::from(mapping.physical_width),
                    width_product.div_ceil(120)
                );
                assert_eq!(
                    u64::from(mapping.physical_height),
                    height_product.div_ceil(120)
                );
                let exact = width_product % 120 == 0 && height_product % 120 == 0;
                assert_eq!(mapping.is_integral(), exact);
                assert_eq!(
                    frame_has_exact_native_mapping(
                        mapping.physical_width,
                        mapping.physical_height,
                        width,
                        height,
                        scale_120,
                    ),
                    exact
                );
            }
        }
    }

    #[test]
    fn resize_invalidates_native_identity_until_both_axes_are_exact() {
        assert!(frame_has_exact_native_mapping(1600, 1000, 1280, 800, 150));
        assert!(!frame_has_exact_native_mapping(1600, 1000, 1279, 799, 150));
        let resized = PixelMapping::new(1279, 799, 150).unwrap();
        assert_eq!(
            (resized.physical_width, resized.physical_height),
            (1599, 999)
        );
        assert!(!resized.is_integral());
        assert!(!frame_has_exact_native_mapping(
            resized.physical_width,
            resized.physical_height,
            1279,
            799,
            150,
        ));
        assert!(!frame_has_exact_native_mapping(1600, 1000, 1280, 800, 160,));
        assert!(!frame_has_exact_native_mapping(0, 0, 0, 0, 0,));
    }

    #[test]
    fn mapping_rejects_dimension_and_fixed_point_overflow() {
        let maximum =
            PixelMapping::new(MAX_WAYLAND_FIXED_EXTENT, MAX_WAYLAND_FIXED_EXTENT, 120).unwrap();
        assert_eq!(maximum.physical_width, MAX_WAYLAND_FIXED_EXTENT);
        assert_eq!(max_fixed_coordinate(MAX_WAYLAND_FIXED_EXTENT), i32::MAX);
        assert!(PixelMapping::new(MAX_WAYLAND_FIXED_EXTENT + 1, 1, 120).is_none());
        assert!(PixelMapping::new(MAX_WAYLAND_FIXED_EXTENT, 1, 121).is_none());
        assert!(PixelMapping::new(u32::MAX, u32::MAX, u32::MAX).is_none());
        assert!(PixelMapping::new(0, 1, 120).is_none());
        assert!(PixelMapping::new(1, 1, 0).is_none());
    }

    #[test]
    fn input_coordinates_are_quantized_and_transformed_exactly_once() {
        assert_eq!(coordinate_to_fixed(939.0, 1920), 939 * 256);
        assert_eq!(coordinate_to_fixed(-1.0, 1920), 0);
        assert_eq!(coordinate_to_fixed(1920.0, 1920), 1920 * 256 - 1);
        assert_eq!(map_monitor_coordinate(0.0, 1280, 1707), 0.0);
        assert_eq!(map_monitor_coordinate(640.0, 1280, 1707), 853.5);
        // Host and guest UI scales are independent: host logical input is
        // mapped exactly once to the physical nested surface.
        let guest_surface = map_monitor_coordinate(640.0, 1280, 1600);
        assert_eq!(guest_surface, 800.0);
    }

    #[test]
    fn host_monitor_coordinates_cover_the_complete_fractional_surface() {
        let far_edge = map_monitor_coordinate(1280.0, 1280, 1707);
        assert!(far_edge > 1706.99 && far_edge < 1707.0);
    }

    #[test]
    fn monitor_origin_is_physical_pixel_aligned_at_acceptance_scales() {
        let origins = [(14.0, 50.0), (14.0, 86.0), (17.0, 91.0)];
        for scale_120 in [120_u32, 150, 160, 180, 210, 240] {
            for (x, y) in origins {
                for origin in [x, y] {
                    let margin = aligned_origin_margin(origin, scale_120).unwrap();
                    let physical = (origin + f64::from(margin)) * f64::from(scale_120)
                        / f64::from(WAYLAND_SCALE_DENOMINATOR);
                    assert_eq!(
                        physical,
                        physical.round(),
                        "monitor origin is fractional at {scale_120}/120 scale"
                    );
                }
            }
        }
        assert_eq!(aligned_origin_margin(14.0, 150), Some(2));
        assert_eq!(aligned_origin_margin(86.0, 150), Some(2));
        assert_eq!(aligned_origin_margin(14.0, 160), Some(1));
        assert_eq!(aligned_origin_margin(14.5, 240), None);
        assert_eq!(aligned_origin_margin(86.5, 120), None);
        assert_eq!(align_extent_up(1280, 3), Some(1281));
        assert_eq!(align_extent_up(800, 3), Some(801));
        assert_eq!(align_extent_up(1280, 4), Some(1280));
    }

    #[test]
    fn arbitrary_resizes_quantize_offload_extent_without_stretching() {
        for scale_120 in [120_u32, 150, 160, 180, 210, 240] {
            let denominator = scale_denominator(scale_120).unwrap() as i32;
            for wrapper_extent in 320..=16384 {
                let leading = (wrapper_extent % denominator).min(denominator - 1);
                let trailing =
                    trailing_extent_margin(wrapper_extent, leading, denominator).unwrap();
                let content = wrapper_extent - leading - trailing;
                assert!(content > 0);
                assert_eq!(content % denominator, 0);
                assert!((0..denominator).contains(&trailing));
                assert_eq!(
                    content * scale_120 as i32 % WAYLAND_SCALE_DENOMINATOR as i32,
                    0
                );
            }
        }
    }

    #[test]
    fn initial_window_correction_targets_monitor_not_host_chrome() {
        assert_eq!(
            corrected_window_size(1280, 878, 1280, 769, 1280, 800),
            (1280, 909)
        );
        assert_eq!(
            corrected_window_size(1400, 900, 1320, 820, 1280, 800),
            (1360, 880)
        );
    }

    #[test]
    fn configured_initial_monitor_aligns_up_for_exact_fractional_pixels() {
        // 160/120 is 4/3, so both logical axes must be divisible by three.
        // The configured size is a minimum: alignment grows 1280x800 to
        // 1281x801 instead of shrinking it or accepting resampled pixels.
        let denominator = scale_denominator(160).unwrap();
        assert_eq!(denominator, 3);
        assert_eq!(align_extent_up(1280, denominator), Some(1281));
        assert_eq!(align_extent_up(800, denominator), Some(801));

        let now = Instant::now();
        let mut sizing = InitialMonitorSizing::new(1280, 800);
        assert_eq!(
            sizing.observe(now, (1280, 800), (1280, 800), denominator, false),
            InitialSizeDecision::Request {
                window_width: 1281,
                window_height: 801,
            }
        );
        assert_eq!(
            sizing.observe(now, (1281, 801), (1281, 801), denominator, false),
            InitialSizeDecision::Settled
        );
        let mapping = PixelMapping::new(1281, 801, 160).unwrap();
        assert_eq!(
            (mapping.physical_width, mapping.physical_height),
            (1708, 1068)
        );
        assert!(mapping.is_integral());
    }

    #[test]
    fn initial_window_correction_waits_for_wayland_resize_response() {
        let started_at = Instant::now();
        let mut sizing = InitialMonitorSizing::new(1280, 800);
        assert_eq!(
            sizing.observe(started_at, (1280, 800), (1276, 752), 4, false),
            InitialSizeDecision::Request {
                window_width: 1284,
                window_height: 848,
            }
        );
        for _ in 0..32 {
            assert_eq!(
                sizing.observe(started_at, (1280, 800), (1276, 752), 4, false),
                InitialSizeDecision::Waiting
            );
        }
        assert_eq!(
            sizing.observe(started_at, (1284, 848), (1280, 800), 4, false),
            InitialSizeDecision::Settled
        );
        assert_eq!(
            sizing.observe(started_at, (1284, 848), (1280, 800), 4, false),
            InitialSizeDecision::AlreadySettled
        );
    }

    #[test]
    fn initial_window_correction_reacts_to_a_distinct_allocation() {
        let started_at = Instant::now();
        let mut sizing = InitialMonitorSizing::new(1280, 800);
        assert!(matches!(
            sizing.observe(started_at, (1280, 800), (1276, 752), 4, false),
            InitialSizeDecision::Request { .. }
        ));
        assert_eq!(
            sizing.observe(started_at, (1284, 848), (1278, 798), 4, false),
            InitialSizeDecision::Request {
                window_width: 1286,
                window_height: 850,
            }
        );
    }

    #[test]
    fn compositor_constrained_initial_monitor_accepts_native_aligned_viewport() {
        let started_at = Instant::now();
        let mut sizing = InitialMonitorSizing::new(1280, 800);
        assert_eq!(
            sizing.observe(started_at, (1280, 800), (1280, 798), 2, true),
            InitialSizeDecision::Settled
        );
        assert_eq!(
            sizing.observe(started_at, (1280, 800), (1280, 798), 2, true),
            InitialSizeDecision::AlreadySettled
        );

        let mut bootstrap = InitialMonitorSizing::new(1280, 800);
        assert!(matches!(
            bootstrap.observe(started_at, (1, 1), (1, 1), 2, true),
            InitialSizeDecision::Request { .. }
        ));
    }

    #[test]
    fn ignored_initial_resize_fails_on_wall_clock_without_reduced_mode() {
        let started_at = Instant::now();
        let mut sizing = InitialMonitorSizing::new(1280, 800);
        sizing.begin(started_at);
        assert!(matches!(
            sizing.observe(started_at, (1280, 800), (1276, 752), 4, false),
            InitialSizeDecision::Request { .. }
        ));
        assert_eq!(
            sizing.observe(
                started_at + INITIAL_MONITOR_SIZE_TIMEOUT - Duration::from_nanos(1),
                (1280, 800),
                (1276, 752),
                4,
                false,
            ),
            InitialSizeDecision::Waiting
        );
        assert_eq!(
            sizing.observe(
                started_at + INITIAL_MONITOR_SIZE_TIMEOUT,
                (1280, 800),
                (1276, 752),
                4,
                false,
            ),
            InitialSizeDecision::TimedOut {
                target_width: 1280,
                target_height: 800,
                viewport_width: 1276,
                viewport_height: 752,
            }
        );
        assert_eq!(
            sizing.observe(
                started_at + INITIAL_MONITOR_SIZE_TIMEOUT + Duration::from_secs(1),
                (1280, 800),
                (1276, 752),
                4,
                false,
            ),
            InitialSizeDecision::AlreadyFailed
        );
        assert!(sizing.failed());
    }

    #[test]
    fn initial_window_deadline_expires_before_geometry_settles() {
        let started_at = Instant::now();
        let mut sizing = InitialMonitorSizing::new(1280, 800);
        sizing.begin(started_at);
        assert_eq!(
            sizing.check_timeout(
                started_at + INITIAL_MONITOR_SIZE_TIMEOUT - Duration::from_nanos(1),
                1,
                1,
                4,
            ),
            None
        );
        assert_eq!(
            sizing.check_timeout(started_at + INITIAL_MONITOR_SIZE_TIMEOUT, 1, 1, 4,),
            Some(InitialSizeDecision::TimedOut {
                target_width: 1280,
                target_height: 800,
                viewport_width: 1,
                viewport_height: 1,
            })
        );
        assert!(sizing.failed());
    }

    #[test]
    fn idle_time_is_not_a_dropped_frame() {
        let stats = PresentationDiagnostics {
            submitted_frames: 2,
            painted_frames: 2,
            presented_frames: 2,
            last_presented_frame_interval_us: 30_000_000,
            dropped_frames: 0,
            ..PresentationDiagnostics::default()
        };
        assert_eq!(stats.dropped_frames, 0);
        assert_eq!(stats.superseded_before_paint, 0);
    }

    #[test]
    fn hidden_window_clock_matches_the_output_refresh() {
        assert_eq!(refresh_interval(0), Duration::from_nanos(16_666_666));
        assert_eq!(refresh_interval(165_000), Duration::from_nanos(6_060_606));
        assert_eq!(refresh_interval(265_000), Duration::from_nanos(3_773_584));
    }

    #[test]
    fn clipboard_completion_is_bound_to_one_machine_lifecycle() {
        let running_epoch = 17;
        assert!(clipboard_transfer_is_live(
            running_epoch,
            running_epoch,
            true
        ));
        assert!(!clipboard_transfer_is_live(
            running_epoch,
            running_epoch,
            false
        ));

        let stopping_epoch =
            lifecycle_clipboard_epoch(running_epoch, MonitorState::Running, MonitorState::Stopping);
        assert_ne!(stopping_epoch, running_epoch);
        assert!(!clipboard_transfer_is_live(
            stopping_epoch,
            running_epoch,
            true
        ));
        assert_eq!(
            lifecycle_clipboard_epoch(
                stopping_epoch,
                MonitorState::Stopping,
                MonitorState::Stopping,
            ),
            stopping_epoch
        );
    }

    #[test]
    fn clipboard_text_aliases_are_exact_and_do_not_shadow_images() {
        for supported in [
            "text/plain;charset=utf-8",
            "TEXT/PLAIN;CHARSET=UTF-8",
            "text/plain",
            "UTF8_STRING",
            "TEXT",
            "STRING",
        ] {
            assert!(is_supported_text_offer(supported), "{supported}");
        }
        for unsupported in [
            "text/plain-evil",
            "text/plain;charset=utf-8;payload=html",
            "UTF8_STRING_EXTRA",
            "text/html",
            "text/rtf",
            "text/uri-list",
            "utf8_string",
        ] {
            assert!(!is_supported_text_offer(unsupported), "{unsupported}");
        }
        for supported in HOST_IMAGE_MIMES {
            assert!(is_supported_image_offer(supported), "{supported}");
        }
        for unsupported in [
            "image/svg+xml",
            "image/png;profile=host",
            "image/apng",
            "image/gif",
            "image/png-evil",
        ] {
            assert!(!is_supported_image_offer(unsupported), "{unsupported}");
        }
    }
}
