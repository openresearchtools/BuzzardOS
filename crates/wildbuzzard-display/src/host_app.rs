// SPDX-License-Identifier: AGPL-3.0-or-later

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use wb_core::{
    MachineConfig, MachineState, NetworkMode, PresentationDiagnostics, RuntimeState,
    WindowDiagnostics,
};

use crate::gateway::{
    DmabufFormat, DmabufFrame, GatewayCommand, GatewayCommandSender, GatewayConnection,
    GatewayEvent, GatewaySockets, HostCommand, OutputMode,
};
use crate::launch::Launch;

const CHROME_HEIGHT_ESTIMATE: u32 = 78;
const MAX_INITIAL_SIZE_CORRECTIONS: u8 = 8;
const RUNTIME_POLL_INTERVAL: Duration = Duration::from_millis(200);
const BACKGROUND_CLOCK_GRACE: Duration = Duration::from_millis(50);
const DEFAULT_REFRESH_MHZ: u32 = 60_000;

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

pub(crate) struct HostApplication {
    launch: Launch,
    connection: GatewayConnection,
}

impl HostApplication {
    pub(crate) fn connect(launch: Launch, connection: GatewayConnection) -> Result<Self> {
        Ok(Self { launch, connection })
    }

    pub(crate) fn run(self, _gateway: GatewaySockets) -> Result<()> {
        // Wild Buzzard does not use host file choosers, screenshot portals, or
        // inhibit portals. Disabling them before GTK initialization keeps the
        // native window independent of a wedged or unavailable desktop portal
        // and avoids creating unnecessary host-service capabilities.
        gtk::disable_portals();
        let application = gtk::Application::builder()
            .application_id(&self.launch.app_id)
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        let activation = Rc::new(RefCell::new(Some((self.launch, self.connection))));
        application.connect_activate(move |application| {
            let Some((launch, connection)) = activation.borrow_mut().take() else {
                if let Some(window) = application.active_window() {
                    window.present();
                }
                return;
            };
            match NativeWindow::build(application, launch, connection) {
                Ok(window) => window.present(),
                Err(error) => {
                    eprintln!("wildbuzzard-display: creating native host application: {error:#}");
                    application.quit();
                }
            }
        });

        let status = application.run_with_args(&["wildbuzzard-display"]);
        if status != glib::ExitCode::SUCCESS {
            anyhow::bail!("native host application exited with {status:?}");
        }
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
    monitor_stack: gtk::Stack,
    picture: gtk::Picture,
    offload: gtk::GraphicsOffload,

    state: Cell<MonitorState>,
    close_requested: Cell<bool>,
    viewport_width: Cell<u32>,
    viewport_height: Cell<u32>,
    /// Fractional scale of the native host surface.
    scale_120: Cell<u32>,
    /// Independently selected guest desktop UI scale.
    guest_scale_120: Cell<u32>,
    refresh_mhz: Cell<u32>,
    initial_monitor_target: Cell<Option<(u32, u32)>>,
    initial_size_corrections: Cell<u8>,
    failure: RefCell<Option<String>>,
    last_runtime_check: Cell<Instant>,
    last_host_frame_tick: Cell<Instant>,
    last_background_frame_tick: Cell<Instant>,
    pending_frame: RefCell<Option<PendingFrame>>,
    pending_presentations: RefCell<VecDeque<PendingPresentation>>,
    presentation: RefCell<PresentationDiagnostics>,
    input: RefCell<InputStats>,
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
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    planes: u32,
    explicit_sync: bool,
    native_resolution: bool,
}

#[derive(Default, serde::Serialize)]
struct InputStats {
    schema: u32,
    received_events: u64,
    forwarded_events: u64,
    ignored_events: u64,
    send_failures: u64,
    shortcut_inhibit_requests: u64,
    shortcut_inhibit_grants: u64,
    shortcut_inhibit_revocations: u64,
    host_shortcuts_inhibited: bool,
    last_event: String,
    last_event_monotonic_us: u64,
    last_guest_logical_x: Option<f64>,
    last_guest_logical_y: Option<f64>,
    last_guest_surface_x: Option<f64>,
    last_guest_surface_y: Option<f64>,
    last_horizontal_scroll: Option<f64>,
    last_vertical_scroll: Option<f64>,
    last_button: Option<u32>,
    last_button_pressed: Option<bool>,
    last_key: Option<u32>,
    last_key_pressed: Option<bool>,
    last_modifiers: Option<u32>,
    monitor_focused: bool,
    scale_120: u32,
    logical_width: u64,
    logical_height: u64,
    physical_width: u64,
    physical_height: u64,
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
        let initial_monitor_target = (launch.initial_width, launch.initial_height);
        let initial_guest_scale_120 = launch.guest_scale_120.unwrap_or(120);
        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .title(&launch.title)
            .default_width(launch.initial_width as i32)
            .default_height(launch.initial_height.saturating_add(CHROME_HEIGHT_ESTIMATE) as i32)
            .resizable(true)
            .decorated(true)
            .build();
        window.set_size_request(360, 320);

        let header = gtk::HeaderBar::builder().show_title_buttons(true).build();
        window.set_titlebar(Some(&header));

        let status_label = gtk::Label::new(Some(MonitorState::Starting.label()));
        status_label.add_css_class("caption");
        status_label.add_css_class("warning");
        status_label.set_tooltip_text(Some("Machine lifecycle state"));
        header.pack_end(&status_label);

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let menu_bar = build_menu_bar();
        root.append(&menu_bar);

        let monitor_stack = gtk::Stack::builder()
            .hexpand(true)
            .vexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(120)
            .build();
        monitor_stack.add_css_class("view");

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
        let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 12);
        placeholder.set_halign(gtk::Align::Center);
        placeholder.set_valign(gtk::Align::Center);
        placeholder.append(&spinner);
        placeholder.append(&state_title);
        placeholder.append(&detail_label);
        monitor_stack.add_named(&placeholder, Some("state"));

        // The picture receives dmabuf textures from the display server. It is
        // kept opaque, rectangular, unclipped, and unfiltered so GTK can place
        // it on a Wayland subsurface instead of sending it through GSK.
        let picture = gtk::Picture::builder()
            .hexpand(true)
            .vexpand(true)
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Fill)
            .alternative_text("Guest machine display")
            .build();
        picture.set_focusable(true);
        picture.set_can_target(true);
        let offload = gtk::GraphicsOffload::new(Some(&picture));
        offload.set_enabled(gtk::GraphicsOffloadEnabled::Enabled);
        offload.set_black_background(true);
        offload.set_hexpand(true);
        offload.set_vexpand(true);
        monitor_stack.add_named(&offload, Some("monitor"));
        monitor_stack.set_visible_child_name("state");

        let status_bar = gtk::CenterBox::new();
        status_bar.add_css_class("toolbar");
        let boundary = gtk::Label::new(Some(
            "Guest input and screenshots are confined to this monitor",
        ));
        boundary.add_css_class("caption");
        boundary.add_css_class("dim-label");
        boundary.set_margin_start(10);
        boundary.set_margin_end(10);
        boundary.set_margin_top(5);
        boundary.set_margin_bottom(5);
        status_bar.set_start_widget(Some(&boundary));
        root.append(&monitor_stack);
        root.append(&status_bar);
        window.set_child(Some(&root));

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
            monitor_stack,
            picture,
            offload,
            state: Cell::new(MonitorState::Starting),
            close_requested: Cell::new(false),
            viewport_width: Cell::new(1),
            viewport_height: Cell::new(1),
            scale_120: Cell::new(120),
            guest_scale_120: Cell::new(initial_guest_scale_120),
            refresh_mhz: Cell::new(0),
            initial_monitor_target: Cell::new(Some(initial_monitor_target)),
            initial_size_corrections: Cell::new(0),
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
            input: RefCell::new(InputStats {
                schema: 3,
                scale_120: 120,
                logical_width: 1,
                logical_height: 1,
                physical_width: 1,
                physical_height: 1,
                ..InputStats::default()
            }),
        });

        native.install_actions();
        native.install_handlers();
        native.update_state_ui();
        native.save_window()?;
        native.save_output_state()?;
        native.save_presentation()?;
        native.save_input()?;
        native.configure_gateway()?;
        Ok(native)
    }

    fn present(self: &Rc<Self>) {
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
                eprintln!("wildbuzzard-display: saving maximize state: {error:#}");
            }
        });

        let this = Rc::clone(self);
        self.window.connect_map(move |_| {
            let Some(toplevel) = this.gdk_toplevel() else {
                return;
            };
            let state_this = Rc::clone(&this);
            toplevel.connect_state_notify(move |_| {
                if let Err(error) = state_this.save_window() {
                    eprintln!("wildbuzzard-display: saving native toplevel state: {error:#}");
                }
            });

            if let Some(clock) = this.window.frame_clock() {
                let this = Rc::clone(&this);
                clock.connect_after_paint(move |clock| this.after_paint(clock));
            }
        });

        let this = Rc::clone(self);
        self.monitor_stack
            .add_tick_callback(move |widget, frame_clock| {
                this.last_host_frame_tick.set(Instant::now());
                let width = widget.width().max(1) as u32;
                let height = widget.height().max(1) as u32;
                this.correct_initial_monitor_size(width, height);
                this.update_viewport(width, height);
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
            this.poll();
            glib::ControlFlow::Continue
        });

        self.install_input_handlers();
    }

    fn correct_initial_monitor_size(&self, viewport_width: u32, viewport_height: u32) {
        let Some((target_width, target_height)) = self.initial_monitor_target.get() else {
            return;
        };
        if viewport_width == target_width && viewport_height == target_height {
            self.initial_monitor_target.set(None);
            return;
        }
        let attempt = self.initial_size_corrections.get();
        if attempt >= MAX_INITIAL_SIZE_CORRECTIONS {
            eprintln!(
                "wildbuzzard-display: host compositor did not grant the configured \
                 {target_width}x{target_height} initial monitor after {attempt} corrections; \
                 using {viewport_width}x{viewport_height}"
            );
            self.initial_monitor_target.set(None);
            return;
        }
        let (requested_width, requested_height) = corrected_window_size(
            self.window.width().max(1),
            self.window.height().max(1),
            viewport_width,
            viewport_height,
            target_width,
            target_height,
        );
        self.initial_size_corrections.set(attempt + 1);
        self.window
            .set_default_size(requested_width, requested_height);
    }

    fn install_input_handlers(self: &Rc<Self>) {
        let motion = gtk::EventControllerMotion::new();
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        motion.connect_enter(move |_, x, y| {
            let (x, y) = this.to_guest_surface(x, y);
            this.send_guest_input(GatewayCommand::PointerEnter { x, y });
        });
        let this = Rc::clone(self);
        motion.connect_motion(move |_, x, y| {
            let (x, y) = this.to_guest_surface(x, y);
            this.send_guest_input(GatewayCommand::PointerMotion { x, y });
        });
        let this = Rc::clone(self);
        motion.connect_leave(move |_| {
            this.send_guest_input(GatewayCommand::PointerLeave);
        });
        self.picture.add_controller(motion);

        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        click.connect_pressed(move |gesture, _, x, y| {
            if let Some(toplevel) = this.gdk_toplevel() {
                toplevel.inhibit_system_shortcuts(gesture.current_event().as_ref());
                let mut stats = this.input.borrow_mut();
                stats.shortcut_inhibit_requests = stats.shortcut_inhibit_requests.saturating_add(1);
            }
            this.picture.grab_focus();
            this.refresh_shortcut_inhibition();
            let (x, y) = this.to_guest_surface(x, y);
            this.send_guest_input(GatewayCommand::PointerMotion { x, y });
            if let Some(button) = linux_pointer_button(gesture.current_button()) {
                this.send_guest_input(GatewayCommand::PointerButton {
                    button,
                    pressed: true,
                });
            }
        });
        let this = Rc::clone(self);
        click.connect_released(move |gesture, _, x, y| {
            let (x, y) = this.to_guest_surface(x, y);
            this.send_guest_input(GatewayCommand::PointerMotion { x, y });
            if let Some(button) = linux_pointer_button(gesture.current_button()) {
                this.send_guest_input(GatewayCommand::PointerButton {
                    button,
                    pressed: false,
                });
            }
        });
        self.picture.add_controller(click);

        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
        );
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = Rc::clone(self);
        scroll.connect_scroll(move |_, horizontal, vertical| {
            this.send_guest_input(GatewayCommand::PointerAxis {
                horizontal,
                vertical,
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

    fn to_guest_surface(&self, x: f64, y: f64) -> (f64, f64) {
        let mode = self.output_mode();
        (
            scale_monitor_coordinate(x, self.viewport_width.get(), mode.physical_width),
            scale_monitor_coordinate(y, self.viewport_height.get(), mode.physical_height),
        )
    }

    fn send_guest_input(&self, command: GatewayCommand) {
        self.refresh_shortcut_inhibition();
        {
            let mut stats = self.input.borrow_mut();
            stats.received_events = stats.received_events.saturating_add(1);
            stats.last_event_monotonic_us = monotonic_us();
            stats.scale_120 = self.scale_120.get();
            stats.logical_width = u64::from(self.viewport_width.get());
            stats.logical_height = u64::from(self.viewport_height.get());
            stats.physical_width = scale_dimension(self.viewport_width.get(), self.scale_120.get());
            stats.physical_height =
                scale_dimension(self.viewport_height.get(), self.scale_120.get());
            match &command {
                GatewayCommand::PointerEnter { x, y } => {
                    stats.last_event = "pointer-enter".into();
                    stats.last_guest_surface_x = Some(*x);
                    stats.last_guest_surface_y = Some(*y);
                    stats.last_guest_logical_x = Some(unscale_monitor_coordinate(
                        *x,
                        stats.physical_width,
                        stats.logical_width,
                    ));
                    stats.last_guest_logical_y = Some(unscale_monitor_coordinate(
                        *y,
                        stats.physical_height,
                        stats.logical_height,
                    ));
                }
                GatewayCommand::PointerLeave => {
                    stats.last_event = "pointer-leave".into();
                }
                GatewayCommand::PointerMotion { x, y } => {
                    stats.last_event = "pointer-motion".into();
                    stats.last_guest_surface_x = Some(*x);
                    stats.last_guest_surface_y = Some(*y);
                    stats.last_guest_logical_x = Some(unscale_monitor_coordinate(
                        *x,
                        stats.physical_width,
                        stats.logical_width,
                    ));
                    stats.last_guest_logical_y = Some(unscale_monitor_coordinate(
                        *y,
                        stats.physical_height,
                        stats.logical_height,
                    ));
                }
                GatewayCommand::PointerButton { button, pressed } => {
                    stats.last_event = "pointer-button".into();
                    stats.last_button = Some(*button);
                    stats.last_button_pressed = Some(*pressed);
                }
                GatewayCommand::PointerAxis {
                    horizontal,
                    vertical,
                } => {
                    stats.last_event = "pointer-axis".into();
                    stats.last_horizontal_scroll = Some(*horizontal);
                    stats.last_vertical_scroll = Some(*vertical);
                }
                GatewayCommand::KeyboardEnter => {
                    stats.last_event = "keyboard-enter".into();
                    stats.monitor_focused = true;
                }
                GatewayCommand::KeyboardLeave => {
                    stats.last_event = "keyboard-leave".into();
                    stats.monitor_focused = false;
                }
                GatewayCommand::KeyboardKey {
                    key,
                    pressed,
                    modifiers,
                } => {
                    stats.last_event = "keyboard-key".into();
                    stats.last_key = Some(*key);
                    stats.last_key_pressed = Some(*pressed);
                    stats.last_modifiers = Some(*modifiers);
                }
                _ => {
                    stats.last_event = "unexpected-non-input-command".into();
                }
            }
        }
        if self.state.get() != MonitorState::Running {
            let mut stats = self.input.borrow_mut();
            stats.ignored_events = stats.ignored_events.saturating_add(1);
            drop(stats);
            if let Err(error) = self.save_input() {
                eprintln!("wildbuzzard-display: saving input diagnostics: {error:#}");
            }
            return;
        }
        if let Err(error) = self.commands.send(command) {
            let mut stats = self.input.borrow_mut();
            stats.send_failures = stats.send_failures.saturating_add(1);
            eprintln!("wildbuzzard-display: forwarding guest input: {error:#}");
        } else {
            let mut stats = self.input.borrow_mut();
            stats.forwarded_events = stats.forwarded_events.saturating_add(1);
        }
        if let Err(error) = self.save_input() {
            eprintln!("wildbuzzard-display: saving input diagnostics: {error:#}");
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
    }

    fn add_action(self: &Rc<Self>, name: &str, callback: impl Fn() + 'static) {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| callback());
        self.application.add_action(&action);
    }

    fn poll(&self) {
        self.refresh_shortcut_inhibition();
        while let Ok(event) = self.events.borrow_mut().try_recv() {
            match event {
                GatewayEvent::HostCommand(command) => self.apply_host_command(command),
                GatewayEvent::GuestConnected => {
                    self.failure.borrow_mut().take();
                    self.set_state(MonitorState::Starting);
                }
                GatewayEvent::GuestDisconnected => {
                    if self.state.get() != MonitorState::Failed {
                        self.set_state(MonitorState::Stopped);
                    }
                }
                GatewayEvent::GuestFailed(error) => {
                    *self.failure.borrow_mut() = Some(error);
                    self.set_state(MonitorState::Failed);
                }
                GatewayEvent::GuestFrame(frame) => {
                    if let Err(error) = self.install_frame(frame) {
                        *self.failure.borrow_mut() = Some(format!("{error:#}"));
                        self.set_state(MonitorState::Failed);
                    }
                }
                GatewayEvent::FrameReleased { id, held_us } => {
                    let mut stats = self.presentation.borrow_mut();
                    stats.released_frames = stats.released_frames.saturating_add(1);
                    stats.last_released_frame_id = id;
                    stats.last_buffer_residency_us = held_us;
                    stats.maximum_buffer_residency_us =
                        stats.maximum_buffer_residency_us.max(held_us);
                    drop(stats);
                    if let Err(error) = self.save_presentation() {
                        eprintln!("wildbuzzard-display: saving frame release timing: {error:#}");
                    }
                }
            }
        }

        if self.last_runtime_check.get().elapsed() >= RUNTIME_POLL_INTERVAL {
            self.last_runtime_check.set(Instant::now());
            self.refresh_runtime_state();
        }
    }

    fn refresh_runtime_state(&self) {
        let Ok(Some(runtime)) = RuntimeState::load(&self.launch.machine_dir) else {
            return;
        };
        let state = match runtime.state {
            MachineState::Starting => MonitorState::Starting,
            MachineState::Running => MonitorState::Running,
            MachineState::Stopping => MonitorState::Stopping,
            MachineState::Stopped => MonitorState::Stopped,
            MachineState::Failed => MonitorState::Failed,
        };
        if state != self.state.get() {
            self.set_state(state);
        }
        if self.close_requested.get()
            && matches!(runtime.state, MachineState::Stopped | MachineState::Failed)
        {
            self.application.quit();
        }
    }

    fn install_frame(&self, frame: DmabufFrame) -> Result<()> {
        let DmabufFrame {
            id,
            width,
            height,
            fourcc,
            modifier,
            planes,
            submitted_monotonic_us,
            explicit_sync,
            acquire_wait_us,
        } = frame;
        if planes.is_empty() || planes.len() > 4 {
            self.release_rejected_frame(id)?;
            anyhow::bail!("guest dmabuf frame has {} planes", planes.len());
        }
        let plane_count = planes.len() as u32;
        let output_mode = self.output_mode();
        let metadata = FrameMetadata {
            width,
            height,
            fourcc,
            modifier,
            planes: plane_count,
            explicit_sync,
            native_resolution: width == output_mode.physical_width
                && height == output_mode.physical_height,
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

        self.picture.set_paintable(Some(&texture));
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
        if self.state.get() != MonitorState::Running {
            self.set_state(MonitorState::Running);
        }
        self.save_presentation()
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
            let offloaded = self.has_subsurface_offload();
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
        }
        self.finish_presentation_feedback(clock);
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
                if let Err(error) = self.save_presentation() {
                    eprintln!(
                        "wildbuzzard-display: saving background pacing diagnostics: {error:#}"
                    );
                }
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
        if let Err(error) = self.save_presentation() {
            eprintln!("wildbuzzard-display: saving background pacing diagnostics: {error:#}");
        }
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
                eprintln!("wildbuzzard-display: returning presentation feedback: {error:#}");
                break;
            }
            self.record_presentation(&frame, presentation_time_us, refresh_interval_us, sequence);
            changed = true;
        }
        if changed {
            if let Err(error) = self.save_presentation() {
                eprintln!("wildbuzzard-display: saving presentation diagnostics: {error:#}");
            }
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
            && stats.width == frame.metadata.width
            && stats.height == frame.metadata.height
            && stats.format == frame.metadata.fourcc
            && stats.modifier == format!("0x{:016x}", frame.metadata.modifier)
            && stats.planes == frame.metadata.planes
            && stats.scale_120 == self.scale_120.get()
            && stats.viewport_width == self.viewport_width.get()
            && stats.viewport_height == self.viewport_height.get();
        stats.transport = "dmabuf".into();
        stats.width = frame.metadata.width;
        stats.height = frame.metadata.height;
        stats.format = frame.metadata.fourcc;
        stats.modifier = format!("0x{:016x}", frame.metadata.modifier);
        stats.planes = frame.metadata.planes;
        stats.scale_120 = self.scale_120.get();
        stats.viewport_width = self.viewport_width.get();
        stats.viewport_height = self.viewport_height.get();
        stats.native_resolution = frame.metadata.native_resolution;
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
        stats.zero_copy = frame.offloaded && frame.metadata.native_resolution;
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

    fn has_subsurface_offload(&self) -> bool {
        let width = self.offload.width().max(1);
        let height = self.offload.height().max(1);
        let paintable = gtk::WidgetPaintable::new(Some(&self.offload));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, width as f64, height as f64);
        snapshot
            .to_node()
            .is_some_and(|node| contains_subsurface_node(&node))
    }

    fn set_state(&self, state: MonitorState) {
        self.state.set(state);
        self.update_state_ui();
        if let Err(error) = self.save_window() {
            eprintln!("wildbuzzard-display: saving native window state: {error:#}");
        }
        if let Err(error) = self.save_output_state() {
            eprintln!("wildbuzzard-display: saving monitor state: {error:#}");
        }
    }

    fn update_state_ui(&self) {
        for class in ["dim-label", "warning", "success", "error"] {
            self.status_label.remove_css_class(class);
        }
        self.status_label.set_label(self.state.get().label());
        self.status_label
            .add_css_class(self.state.get().css_class());

        match self.state.get() {
            MonitorState::Running => {
                self.spinner.stop();
                self.state_title.set_label("Machine running");
                self.monitor_stack.set_visible_child_name("monitor");
            }
            MonitorState::Stopped => {
                self.spinner.stop();
                self.state_title.set_label("Machine stopped");
                self.detail_label
                    .set_label("Use Machine → Start to boot this persistent desktop.");
                self.monitor_stack.set_visible_child_name("state");
            }
            MonitorState::Starting => {
                self.spinner.start();
                self.state_title.set_label("Starting machine");
                self.detail_label
                    .set_label("Starting systemd, Sway, desktop services, and CUA driver…");
                self.monitor_stack.set_visible_child_name("state");
            }
            MonitorState::Stopping => {
                self.spinner.start();
                self.state_title.set_label("Stopping machine");
                self.detail_label
                    .set_label("Waiting for orderly guest shutdown and state persistence…");
                self.monitor_stack.set_visible_child_name("state");
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
                self.monitor_stack.set_visible_child_name("state");
            }
        }
    }

    fn update_viewport(&self, width: u32, height: u32) {
        let scale = self
            .window
            .surface()
            .map(|surface| surface.scale())
            .unwrap_or(1.0);
        let scale_120 = effective_scale_120(self.launch.test_fractional_scale_120, scale);
        let guest_scale_120 = self.launch.guest_scale_120.unwrap_or(scale_120);
        let refresh_mhz = self
            .window
            .surface()
            .and_then(|surface| {
                let display = surface.display();
                display.monitor_at_surface(&surface)
            })
            .map(|monitor| monitor.refresh_rate().max(0) as u32)
            .unwrap_or(0);
        if self.viewport_width.replace(width) != width
            || self.viewport_height.replace(height) != height
            || self.scale_120.replace(scale_120) != scale_120
            || self.guest_scale_120.replace(guest_scale_120) != guest_scale_120
            || self.refresh_mhz.replace(refresh_mhz) != refresh_mhz
        {
            {
                let mut stats = self.input.borrow_mut();
                stats.scale_120 = scale_120;
                stats.logical_width = u64::from(width);
                stats.logical_height = u64::from(height);
                stats.physical_width = scale_dimension(width, scale_120);
                stats.physical_height = scale_dimension(height, scale_120);
            }
            {
                let mut stats = self.presentation.borrow_mut();
                stats.scale_120 = scale_120;
                stats.viewport_width = width;
                stats.viewport_height = height;
                stats.native_resolution = stats.width > 0
                    && u64::from(stats.width) == scale_dimension(width, scale_120)
                    && u64::from(stats.height) == scale_dimension(height, scale_120);
                if !stats.native_resolution {
                    stats.zero_copy = false;
                }
            }
            if let Err(error) = self.save_input() {
                eprintln!("wildbuzzard-display: saving resized input coordinates: {error:#}");
            }
            if let Err(error) = self.save_presentation() {
                eprintln!("wildbuzzard-display: saving resized presentation state: {error:#}");
            }
            if let Err(error) = self.save_output_state() {
                eprintln!("wildbuzzard-display: saving resized guest output: {error:#}");
            }
            if let Err(error) = self.save_window() {
                eprintln!("wildbuzzard-display: saving resized host window: {error:#}");
            }
            if let Err(error) = self
                .commands
                .send(GatewayCommand::SetOutputMode(self.output_mode()))
            {
                eprintln!("wildbuzzard-display: sending resized guest output: {error:#}");
            }
        }
    }

    fn configure_gateway(&self) -> Result<()> {
        let display = gdk::Display::default().context("GTK has no active Wayland display")?;
        let advertised = display.dmabuf_formats();
        let formats = (0..advertised.n_formats())
            .map(|index| advertised.format(index))
            .map(|(fourcc, modifier)| DmabufFormat { fourcc, modifier })
            .collect();
        self.commands.send(GatewayCommand::Configure {
            formats,
            mode: self.output_mode(),
        })
    }

    fn output_mode(&self) -> OutputMode {
        let host_scale_120 = self.scale_120.get();
        let guest_scale_120 = self.guest_scale_120.get();
        let physical_width = scale_dimension(self.viewport_width.get(), host_scale_120) as u32;
        let physical_height = scale_dimension(self.viewport_height.get(), host_scale_120) as u32;
        OutputMode {
            logical_width: guest_logical_dimension(physical_width, guest_scale_120),
            logical_height: guest_logical_dimension(physical_height, guest_scale_120),
            physical_width,
            physical_height,
            scale_120: guest_scale_120,
            refresh_mhz: self.refresh_mhz.get(),
        }
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
        if !restart {
            self.close_requested.set(self.close_requested.get());
        }
    }

    fn save_host_request(&self, action: &str) -> Result<()> {
        let value = serde_json::json!({
            "schema": 1,
            "action": action,
            "machine": self.launch.machine_dir.file_name(),
        });
        atomic_json(&self.launch.status_dir.join("host-request.json"), &value)
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
        let restart = gtk::Label::new(Some(
            "Display, network, and GPU changes take effect on the next machine start.",
        ));
        restart.add_css_class("dim-label");
        restart.set_wrap(true);
        restart.set_xalign(0.0);

        attach_setting(&grid, 0, "Initial monitor width", &width);
        attach_setting(&grid, 1, "Initial monitor height", &height);
        attach_setting(&grid, 2, "Desktop scale", &guest_scale);
        attach_setting(&grid, 3, "Network mode", &network);
        attach_setting(&grid, 4, "GPU passthrough", &gpus);
        grid.attach(&restart, 0, 5, 2, 1);

        let actions = gtk::ActionBar::new();
        let cancel = gtk::Button::with_label("Cancel");
        let save = gtk::Button::with_label("Save");
        save.add_css_class("suggested-action");
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

    fn open_diagnostics(&self) {
        let stats = self.presentation.borrow().clone();
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
            &format!("{:.0}%", self.scale_120.get() as f64 / 1.2),
        );
        add_diagnostic(
            &grid,
            4,
            "Guest desktop scale",
            &format!("{:.0}%", self.guest_scale_120.get() as f64 / 1.2),
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
        let scale_120 = self.scale_120.get();
        let physical_width = scale_dimension(logical_width as u32, scale_120) as i32;
        let physical_height = scale_dimension(logical_height as u32, scale_120) as i32;
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

    fn refresh_shortcut_inhibition(&self) {
        let Some(toplevel) = self.gdk_toplevel() else {
            return;
        };
        let inhibited = toplevel.is_shortcuts_inhibited();
        let mut stats = self.input.borrow_mut();
        if stats.host_shortcuts_inhibited == inhibited {
            return;
        }
        if inhibited {
            stats.shortcut_inhibit_grants = stats.shortcut_inhibit_grants.saturating_add(1);
        } else {
            stats.shortcut_inhibit_revocations =
                stats.shortcut_inhibit_revocations.saturating_add(1);
        }
        stats.host_shortcuts_inhibited = inhibited;
        drop(stats);
        if let Err(error) = self.save_input() {
            eprintln!("wildbuzzard-display: saving shortcut inhibition state: {error:#}");
        }
    }

    fn save_output_state(&self) -> Result<()> {
        let host_scale_120 = self.scale_120.get();
        let guest_scale_120 = self.guest_scale_120.get();
        let physical_width = scale_dimension(self.viewport_width.get(), host_scale_120) as u32;
        let physical_height = scale_dimension(self.viewport_height.get(), host_scale_120) as u32;
        let guest_logical_width = guest_logical_dimension(physical_width, guest_scale_120);
        let guest_logical_height = guest_logical_dimension(physical_height, guest_scale_120);
        let value = serde_json::json!({
            "schema": 5,
            "scale_120": guest_scale_120,
            "host_scale_120": host_scale_120,
            "host_viewport_width": self.viewport_width.get(),
            "host_viewport_height": self.viewport_height.get(),
            "logical_width": guest_logical_width,
            "logical_height": guest_logical_height,
            "guest_logical_width": guest_logical_width,
            "guest_logical_height": guest_logical_height,
            "physical_width": physical_width,
            "physical_height": physical_height,
            "refresh_mhz": self.refresh_mhz.get(),
            "state": self.state.get().label(),
            "monitor_transport": "gtk4-graphics-offload-dmabuf",
            "cua_coordinate_space": "guest-output-native-physical-dmabuf-pixels",
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

    fn save_input(&self) -> Result<()> {
        atomic_json(
            &self.launch.status_dir.join("input.json"),
            &*self.input.borrow(),
        )
    }
}

fn build_menu_bar() -> gtk::PopoverMenuBar {
    let machine = gio::Menu::new();
    machine.append(Some("Start"), Some("app.start"));
    machine.append(Some("Stop"), Some("app.stop"));
    machine.append(Some("Restart"), Some("app.restart"));
    let shutdown = gio::Menu::new();
    shutdown.append(Some("Shut Down Machine"), Some("app.shutdown"));
    shutdown.append(Some("Close Window"), Some("app.close"));
    machine.append_section(None, &shutdown);

    let settings = gio::Menu::new();
    settings.append(Some("Machine Settings…"), Some("app.settings"));
    settings.append(Some("Display Diagnostics…"), Some("app.diagnostics"));

    let root = gio::Menu::new();
    root.append_submenu(Some("Machine"), &machine);
    root.append_submenu(Some("Settings"), &settings);
    let menu = gtk::PopoverMenuBar::from_model(Some(&root));
    menu.set_hexpand(true);
    menu
}

fn attach_setting(grid: &gtk::Grid, row: i32, name: &str, value: &impl IsA<gtk::Widget>) {
    let label = gtk::Label::new(Some(name));
    label.set_xalign(0.0);
    label.set_mnemonic_widget(Some(value));
    grid.attach(&label, 0, row, 1, 1);
    grid.attach(value, 1, row, 1, 1);
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

fn show_error_dialog(parent: &gtk::ApplicationWindow, heading: &str, error: &anyhow::Error) {
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

fn scale_dimension(logical: u32, scale_120: u32) -> u64 {
    (logical as u64 * scale_120 as u64).div_ceil(120)
}

fn guest_logical_dimension(physical: u32, guest_scale_120: u32) -> u32 {
    u64::from(physical)
        .saturating_mul(120)
        .saturating_add(u64::from(guest_scale_120.max(1)) / 2)
        .checked_div(u64::from(guest_scale_120.max(1)))
        .unwrap_or(1)
        .clamp(1, u64::from(u32::MAX)) as u32
}

fn effective_scale_120(test_override: Option<u32>, host_scale: f64) -> u32 {
    test_override.unwrap_or_else(|| (host_scale * 120.0).round().clamp(120.0, 960.0) as u32)
}

fn clamp_guest_logical_coordinate(value: f64, extent: u32) -> f64 {
    // Wayland surface coordinates use wl_fixed (24.8), so one 1/256 logical
    // pixel step is the smallest representable point inside the far edge.
    value.clamp(0.0, (f64::from(extent) - (1.0 / 256.0)).max(0.0))
}

fn scale_monitor_coordinate(value: f64, monitor_extent: u32, surface_extent: u32) -> f64 {
    let monitor_extent = monitor_extent.max(1);
    let surface_extent = surface_extent.max(1);
    let monitor_coordinate = clamp_guest_logical_coordinate(value, monitor_extent);
    let surface_coordinate =
        monitor_coordinate * f64::from(surface_extent) / f64::from(monitor_extent);
    clamp_guest_logical_coordinate(surface_coordinate, surface_extent)
}

fn unscale_monitor_coordinate(value: f64, surface_extent: u64, logical_extent: u64) -> f64 {
    if surface_extent == 0 {
        return 0.0;
    }
    value * logical_extent as f64 / surface_extent as f64
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

fn contains_subsurface_node(node: &gtk::gsk::RenderNode) -> bool {
    use gtk::gsk::{ContainerNode, RenderNodeType};

    match node.node_type() {
        RenderNodeType::SubsurfaceNode => true,
        RenderNodeType::ContainerNode => {
            node.downcast_ref::<ContainerNode>()
                .is_some_and(|container| {
                    (0..container.n_children())
                        .any(|index| contains_subsurface_node(&container.child(index)))
                })
        }
        _ => false,
    }
}

fn atomic_json(path: &std::path::Path, value: &impl serde::Serialize) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value).context("serializing display state")?;
    fs::write(&temporary, bytes).with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_scale_is_applied_once() {
        assert_eq!(scale_dimension(2000, 180), 3000);
        assert_eq!(scale_dimension(1, 160), 2);
        assert_eq!(effective_scale_120(None, 4.0 / 3.0), 160);
        assert_eq!(effective_scale_120(Some(180), 4.0 / 3.0), 180);
        assert_eq!(guest_logical_dimension(1600, 150), 1280);
        assert_eq!(guest_logical_dimension(1707, 150), 1366);
    }

    #[test]
    fn input_coordinates_remain_in_guest_logical_space() {
        assert_eq!(clamp_guest_logical_coordinate(939.0, 1920), 939.0);
        assert_eq!(clamp_guest_logical_coordinate(-1.0, 1920), 0.0);
        assert!(clamp_guest_logical_coordinate(1920.0, 1920) < 1920.0);
    }

    #[test]
    fn host_monitor_coordinates_cover_the_complete_fractional_surface() {
        assert_eq!(scale_monitor_coordinate(0.0, 1280, 1707), 0.0);
        assert_eq!(scale_monitor_coordinate(640.0, 1280, 1707), 853.5);
        let far_edge = scale_monitor_coordinate(1280.0, 1280, 1707);
        assert!(far_edge > 1706.99 && far_edge < 1707.0);
        assert_eq!(unscale_monitor_coordinate(853.5, 1707, 1280), 640.0);
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
}
