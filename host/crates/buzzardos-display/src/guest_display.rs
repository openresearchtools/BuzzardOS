// SPDX-License-Identifier: AGPL-3.0-or-later

//! Private Wayland server presented to the nested guest compositor.
//!
//! This is deliberately a server, not a byte proxy to the host compositor.
//! The only client permitted here is Sway. Its final output buffer becomes a
//! [`DmabufFrame`] for the GTK monitor; guest-created xdg objects never become
//! host xdg objects.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use wayland_protocols::wp::linux_dmabuf::zv1::server::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1, zwp_linux_dmabuf_v1,
};
use wayland_protocols::wp::linux_drm_syncobj::v1::server::{
    wp_linux_drm_syncobj_manager_v1, wp_linux_drm_syncobj_surface_v1,
    wp_linux_drm_syncobj_timeline_v1,
};
use wayland_protocols::wp::presentation_time::server::{wp_presentation, wp_presentation_feedback};
use wayland_protocols::wp::viewporter::server::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::server::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use wayland_server::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_region, wl_seat, wl_shm,
    wl_shm_pool, wl_surface,
};
use wayland_server::{
    Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New, Resource,
};
use xkbcommon::xkb;

use crate::drm_syncobj::{SyncobjDevice, SyncobjTimeline};
use crate::gateway::{
    CursorImage, CursorStorage, DmabufFormat, DmabufFrame, DmabufPlane, EventSender,
    GatewayCommand, GatewayEvent, OutputMode,
};
use crate::keyboard::{
    CompiledKeymap, KeyboardMapFailure, KeyboardMapReply, KeyboardMapRequest, KeyboardMapResponse,
    KeyboardMapSpec, KeyboardMapState,
};

const MAX_DMABUF_PLANES: usize = 4;
const MAX_PENDING_KEY_EVENTS: usize = 256;
#[cfg(test)]
const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
#[cfg(test)]
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
#[cfg(test)]
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

#[derive(Debug)]
struct GuestClientData {
    connected: AtomicBool,
    events: EventSender,
    socket_path: PathBuf,
}

impl ClientData for GuestClientData {
    fn initialized(&self, _client_id: ClientId) {
        self.connected.store(true, Ordering::Release);
        if let Err(error) = set_private_display_socket_connected(&self.socket_path, true) {
            let _ = self.events.send(GatewayEvent::GuestFailed(format!(
                "locking private compositor socket: {error:#}"
            )));
            return;
        }
        let _ = self.events.send(GatewayEvent::GuestConnected);
    }

    fn disconnected(&self, _client_id: ClientId, reason: DisconnectReason) {
        self.connected.store(false, Ordering::Release);
        if let Err(error) = set_private_display_socket_connected(&self.socket_path, false) {
            let _ = self.events.send(GatewayEvent::GuestFailed(format!(
                "unlocking private compositor socket for a replacement compositor: {error:#}"
            )));
        }
        if !matches!(reason, DisconnectReason::ConnectionClosed) {
            let _ = self.events.send(GatewayEvent::GuestFailed(format!(
                "nested compositor disconnected: {reason:?}"
            )));
        }
        let _ = self.events.send(GatewayEvent::GuestDisconnected);
    }
}

pub(crate) fn run(
    listener: UnixListener,
    events: EventSender,
    commands: Receiver<GatewayCommand>,
    command_notify: UnixStream,
    sync_drm_device: Option<PathBuf>,
    xkb_config_root: PathBuf,
) -> Result<()> {
    let socket_path = listener
        .local_addr()
        .context("reading private compositor listener address")?
        .as_pathname()
        .map(PathBuf::from)
        .context("private compositor listener has no filesystem path")?;
    let (formats, mode) = wait_for_configuration(&commands, &command_notify)?;
    let sync_device = sync_drm_device
        .as_deref()
        .map(SyncobjDevice::open)
        .transpose()?;
    let mut display = Display::<GuestState>::new().context("creating private Wayland display")?;
    let handle = display.handle();
    create_globals(&handle, sync_device.is_some());
    let mut state = GuestState::new(events.clone(), formats, mode, sync_device, xkb_config_root)?;

    loop {
        let (connection, _) = listener
            .accept()
            .context("accepting nested compositor connection")?;
        let client_data = Arc::new(GuestClientData {
            connected: AtomicBool::new(false),
            events: events.clone(),
            socket_path: socket_path.clone(),
        });
        handle
            .clone()
            .insert_client(connection, client_data.clone())
            .context("registering nested compositor client")?;

        drain_commands(&commands, &command_notify, &mut state);
        while client_data.connected.load(Ordering::Acquire) {
            poll_once(&mut display, &commands, &command_notify, &mut state)?;
        }
        state.drop_client_leases();
    }
}

fn set_private_display_socket_connected(path: &std::path::Path, connected: bool) -> Result<()> {
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(private_display_socket_mode(connected)),
    )
    .with_context(|| format!("setting permissions on {}", path.display()))
}

fn private_display_socket_mode(connected: bool) -> u32 {
    if connected { 0o000 } else { 0o600 }
}

fn wait_for_configuration(
    commands: &Receiver<GatewayCommand>,
    command_notify: &UnixStream,
) -> Result<(Vec<DmabufFormat>, OutputMode)> {
    loop {
        match commands
            .recv()
            .context("native application stopped before configuring display")?
        {
            GatewayCommand::Configure { formats, mode } => {
                drain_notification(command_notify);
                if formats.is_empty() {
                    bail!("host Wayland display advertises no importable dmabuf formats");
                }
                return Ok((formats, mode));
            }
            GatewayCommand::KeyboardMap { reply, .. } => {
                let _ = reply.send(Err(KeyboardMapFailure::new(
                    "display_not_ready",
                    "guest display owner has not completed initial configuration",
                )));
            }
            _ => continue,
        }
    }
}

fn create_globals(handle: &DisplayHandle, explicit_sync: bool) {
    handle.create_global::<GuestState, wl_compositor::WlCompositor, _>(4, ());
    handle.create_global::<GuestState, wl_shm::WlShm, _>(2, ());
    handle.create_global::<GuestState, wl_seat::WlSeat, _>(9, ());
    handle.create_global::<GuestState, xdg_wm_base::XdgWmBase, _>(1, ());
    handle.create_global::<GuestState, wp_viewporter::WpViewporter, _>(1, ());
    handle.create_global::<GuestState, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, _>(4, ());
    handle.create_global::<GuestState, wp_presentation::WpPresentation, _>(1, ());
    if explicit_sync {
        handle.create_global::<GuestState, wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1, _>(
            1,
            (),
        );
    }
}

fn poll_once(
    display: &mut Display<GuestState>,
    commands: &Receiver<GatewayCommand>,
    command_notify: &UnixStream,
    state: &mut GuestState,
) -> Result<()> {
    let mut descriptors = [
        libc::pollfd {
            fd: display.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: command_notify.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: both pollfd values contain live descriptors for the duration of
    // the call and the array length is exact.
    let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
    if result < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(());
        }
        return Err(error).context("polling private Wayland display");
    }
    if descriptors[1].revents != 0 {
        drain_commands(commands, command_notify, state);
    }
    if descriptors[0].revents != 0 {
        display
            .dispatch_clients(state)
            .context("dispatching nested compositor requests")?;
    }
    display
        .flush_clients()
        .context("flushing nested compositor events")
}

fn drain_commands(
    commands: &Receiver<GatewayCommand>,
    command_notify: &UnixStream,
    state: &mut GuestState,
) {
    drain_notification(command_notify);
    while let Ok(command) = commands.try_recv() {
        if let Err(error) = state.apply_command(command) {
            let _ = state
                .events
                .send(GatewayEvent::GuestFailed(format!("{error:#}")));
        }
    }
}

fn drain_notification(stream: &UnixStream) {
    let mut stream = stream;
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

struct GuestState {
    events: EventSender,
    formats: Vec<DmabufFormat>,
    mode: OutputMode,
    serial: u32,
    next_frame_id: u64,
    next_cursor_id: u64,
    leases: HashMap<u64, FrameLease>,
    cursor_leases: HashMap<u64, CursorLease>,
    idle_frame_callbacks: Vec<wl_callback::WlCallback>,
    idle_presentation_feedback: Vec<wp_presentation_feedback::WpPresentationFeedback>,
    toplevels: Vec<ToplevelHandle>,
    pointers: Vec<wl_pointer::WlPointer>,
    keyboards: Vec<wl_keyboard::WlKeyboard>,
    focused_surface: Option<wl_surface::WlSurface>,
    pointer_position: Option<(f64, f64)>,
    pointer_entered: bool,
    cursor_surface: Option<wl_surface::WlSurface>,
    cursor_hotspot: (i32, i32),
    keyboard_focused: bool,
    keyboard_entered: bool,
    keymap: CompiledKeymap,
    xkb_config_root: PathBuf,
    pending_keymap: Option<PendingKeymap>,
    completed_keymap: Option<CompletedKeymap>,
    sync_device: Option<Arc<SyncobjDevice>>,
    pressed_keys: BTreeSet<u32>,
    suppressed_keys: BTreeSet<u32>,
}

struct PendingKeymap {
    token: String,
    digest: String,
    keymap: CompiledKeymap,
    input_queue: VecDeque<QueuedKey>,
    queued_pressed: BTreeSet<u32>,
    input_overflow: bool,
}

#[derive(Clone, Copy)]
struct QueuedKey {
    key: u32,
    pressed: bool,
    host_modifiers: u32,
}

struct CompletedKeymap {
    token: String,
    digest: String,
    state: KeyboardMapState,
}

impl GuestState {
    fn new(
        events: EventSender,
        formats: Vec<DmabufFormat>,
        mode: OutputMode,
        sync_device: Option<Arc<SyncobjDevice>>,
        xkb_config_root: PathBuf,
    ) -> Result<Self> {
        let keymap = CompiledKeymap::compile(&xkb_config_root, &KeyboardMapSpec::standard_us())?;
        Ok(Self {
            events,
            formats,
            mode,
            serial: 1,
            next_frame_id: 1,
            next_cursor_id: 1,
            leases: HashMap::new(),
            cursor_leases: HashMap::new(),
            idle_frame_callbacks: Vec::new(),
            idle_presentation_feedback: Vec::new(),
            toplevels: Vec::new(),
            pointers: Vec::new(),
            keyboards: Vec::new(),
            focused_surface: None,
            pointer_position: None,
            pointer_entered: false,
            cursor_surface: None,
            cursor_hotspot: (0, 0),
            keyboard_focused: false,
            keyboard_entered: false,
            keymap,
            xkb_config_root,
            pending_keymap: None,
            completed_keymap: None,
            sync_device,
            pressed_keys: BTreeSet::new(),
            suppressed_keys: BTreeSet::new(),
        })
    }

    fn next_serial(&mut self) -> u32 {
        let serial = self.serial;
        self.serial = self.serial.wrapping_add(1).max(1);
        serial
    }

    fn apply_command(&mut self, command: GatewayCommand) -> Result<()> {
        match command {
            GatewayCommand::Configure { formats, mode } => {
                self.formats = formats;
                self.configure_output(mode);
            }
            GatewayCommand::SetOutputMode(mode) => self.configure_output(mode),
            GatewayCommand::ReleaseFrame {
                id,
                released_monotonic_us,
            } => self.release_frame(id, released_monotonic_us),
            GatewayCommand::ReleaseCursor { id } => self.release_cursor(id),
            GatewayCommand::FramePainted { id, frame_time_us } => {
                self.paint_frame(id, frame_time_us)
            }
            GatewayCommand::FramePresented {
                id,
                presentation_time_us,
                refresh_interval_us,
                sequence,
                offloaded,
            } => self.present_frame(
                id,
                presentation_time_us,
                refresh_interval_us,
                sequence,
                offloaded,
            ),
            GatewayCommand::FrameTick { frame_time_us } => {
                self.frame_tick(frame_time_us);
            }
            GatewayCommand::PointerEnter {
                x,
                y,
                geometry_generation,
            } if geometry_generation == self.mode.geometry_generation => self.pointer_enter(x, y),
            GatewayCommand::PointerEnter { .. } => {}
            GatewayCommand::PointerLeave => self.pointer_leave(),
            GatewayCommand::PointerMotion {
                x,
                y,
                geometry_generation,
            } if geometry_generation == self.mode.geometry_generation => self.pointer_motion(x, y),
            GatewayCommand::PointerMotion { .. } => {}
            GatewayCommand::PointerButton {
                button,
                pressed,
                geometry_generation,
            } if geometry_generation == self.mode.geometry_generation => {
                self.pointer_button(button, pressed)
            }
            GatewayCommand::PointerButton { .. } => {}
            GatewayCommand::PointerAxis {
                horizontal,
                vertical,
                geometry_generation,
            } if geometry_generation == self.mode.geometry_generation => {
                self.pointer_axis(horizontal, vertical)
            }
            GatewayCommand::PointerAxis { .. } => {}
            GatewayCommand::KeyboardEnter => self.keyboard_enter(),
            GatewayCommand::KeyboardLeave => self.keyboard_leave(),
            GatewayCommand::KeyboardKey {
                key,
                pressed,
                modifiers,
            } => self.keyboard_key(key, pressed, modifiers),
            GatewayCommand::KeyboardMap { request, reply } => {
                let _ = reply.send(self.keyboard_map_request(request));
            }
        }
        Ok(())
    }

    fn frame_tick(&mut self, frame_time_us: i64) {
        let frame_time_ms = frame_time_us_to_protocol_ms(frame_time_us);
        for callback in self.idle_frame_callbacks.drain(..) {
            if callback.is_alive() {
                callback.done(frame_time_ms);
            }
        }
        // A commit with no attached buffer has no new dmabuf whose actual
        // presentation GTK can correlate. Do not leave its feedback attached
        // to a later buffer commit and falsely report that later presentation.
        for feedback in self.idle_presentation_feedback.drain(..) {
            if feedback.is_alive() {
                feedback.discarded();
            }
        }
    }

    fn pointer_enter(&mut self, x: f64, y: f64) {
        self.pointer_position = Some((x, y));
        self.ensure_pointer_enter();
    }

    fn ensure_pointer_enter(&mut self) {
        if self.pointer_entered {
            return;
        }
        let Some((x, y)) = self.pointer_position else {
            return;
        };
        let Some(surface) = self.focused_surface.clone().filter(Resource::is_alive) else {
            return;
        };
        self.pointers.retain(Resource::is_alive);
        if self.pointers.is_empty() {
            return;
        }
        let serial = self.next_serial();
        for pointer in &self.pointers {
            pointer.enter(serial, &surface, x, y);
            if pointer.version() >= 5 {
                pointer.frame();
            }
        }
        self.pointer_entered = true;
    }

    fn pointer_leave(&mut self) {
        self.pointer_position = None;
        if !self.pointer_entered {
            return;
        }
        let surface = self.focused_surface.clone().filter(Resource::is_alive);
        let serial = self.next_serial();
        self.pointers.retain(Resource::is_alive);
        if let Some(surface) = surface {
            for pointer in &self.pointers {
                pointer.leave(serial, &surface);
                if pointer.version() >= 5 {
                    pointer.frame();
                }
            }
        }
        self.pointer_entered = false;
    }

    fn pointer_motion(&mut self, x: f64, y: f64) {
        self.pointer_position = Some((x, y));
        self.ensure_pointer_enter();
        if !self.pointer_entered {
            return;
        }
        let time = monotonic_ms();
        self.pointers.retain(Resource::is_alive);
        for pointer in &self.pointers {
            pointer.motion(time, x, y);
            if pointer.version() >= 5 {
                pointer.frame();
            }
        }
    }

    fn pointer_button(&mut self, button: u32, pressed: bool) {
        self.ensure_pointer_enter();
        if !self.pointer_entered {
            return;
        }
        let serial = self.next_serial();
        let state = if pressed {
            wl_pointer::ButtonState::Pressed
        } else {
            wl_pointer::ButtonState::Released
        };
        let time = monotonic_ms();
        self.pointers.retain(Resource::is_alive);
        for pointer in &self.pointers {
            pointer.button(serial, time, button, state);
            if pointer.version() >= 5 {
                pointer.frame();
            }
        }
    }

    fn pointer_axis(&mut self, horizontal: f64, vertical: f64) {
        self.ensure_pointer_enter();
        if !self.pointer_entered {
            return;
        }
        let time = monotonic_ms();
        self.pointers.retain(Resource::is_alive);
        for pointer in &self.pointers {
            if pointer.version() >= 5 {
                pointer.axis_source(wl_pointer::AxisSource::Wheel);
            }
            if horizontal != 0.0 {
                pointer.axis(time, wl_pointer::Axis::HorizontalScroll, horizontal * 15.0);
                if pointer.version() >= 8 {
                    pointer.axis_value120(
                        wl_pointer::Axis::HorizontalScroll,
                        (horizontal * 120.0).round() as i32,
                    );
                }
            }
            if vertical != 0.0 {
                pointer.axis(time, wl_pointer::Axis::VerticalScroll, vertical * 15.0);
                if pointer.version() >= 8 {
                    pointer.axis_value120(
                        wl_pointer::Axis::VerticalScroll,
                        (vertical * 120.0).round() as i32,
                    );
                }
            }
            if pointer.version() >= 5 {
                pointer.frame();
            }
        }
    }

    fn keyboard_enter(&mut self) {
        self.keyboard_focused = true;
        self.ensure_keyboard_enter();
    }

    fn ensure_keyboard_enter(&mut self) {
        if self.keyboard_entered || !self.keyboard_focused {
            return;
        }
        let Some(surface) = self.focused_surface.clone().filter(Resource::is_alive) else {
            return;
        };
        self.keyboards.retain(Resource::is_alive);
        if self.keyboards.is_empty() {
            return;
        }
        let serial = self.next_serial();
        let mut keys = Vec::with_capacity(self.pressed_keys.len().saturating_mul(4));
        for key in &self.pressed_keys {
            keys.extend_from_slice(&key.to_ne_bytes());
        }
        for keyboard in &self.keyboards {
            keyboard.enter(serial, &surface, keys.clone());
            keyboard.modifiers(
                serial,
                self.keymap.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                self.keymap.state.serialize_mods(xkb::STATE_MODS_LATCHED),
                self.keymap.state.serialize_mods(xkb::STATE_MODS_LOCKED),
                self.keymap
                    .state
                    .serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
            );
        }
        self.keyboard_entered = true;
    }

    fn keyboard_leave(&mut self) {
        self.keyboard_focused = false;
        if let Some(pending) = &mut self.pending_keymap {
            // Input belongs to the focus epoch in which it was received. A
            // leave is delivered immediately and invalidates that epoch; held
            // queued keys remain suppressed until their real releases.
            self.suppressed_keys.append(&mut pending.queued_pressed);
            pending.input_queue.clear();
        }
        if !self.keyboard_entered {
            return;
        }
        let surface = self.focused_surface.clone().filter(Resource::is_alive);
        let serial = self.next_serial();
        self.keyboards.retain(Resource::is_alive);
        if let Some(surface) = surface {
            for keyboard in &self.keyboards {
                keyboard.leave(serial, &surface);
            }
        }
        self.keyboard_entered = false;
        for key in std::mem::take(&mut self.pressed_keys) {
            self.keymap.state.update_key(
                xkb::Keycode::new(key.saturating_add(8)),
                xkb::KeyDirection::Up,
            );
        }
    }

    fn keyboard_key(&mut self, key: u32, pressed: bool, _host_modifiers: u32) {
        // A prepared keymap means Sway may already be using the new map while
        // this parent keyboard still owns the old one. Do not interpret a key
        // across that boundary. Keys already held when Prepare arrived remain
        // suppressed through release; new focused events are bounded and
        // replayed in order only after Commit/Abort selects the matching map.
        // CUA's separate virtual keyboard never enters this path.
        if self.suppressed_keys.contains(&key) {
            if pressed {
                self.suppressed_keys.insert(key);
            } else {
                self.suppressed_keys.remove(&key);
            }
            return;
        }
        if let Some(pending) = self.pending_keymap.as_mut() {
            if !self.keyboard_focused {
                if pressed {
                    self.suppressed_keys.insert(key);
                }
                return;
            }
            if pending.input_overflow || pending.input_queue.len() >= MAX_PENDING_KEY_EVENTS {
                self.suppressed_keys.append(&mut pending.queued_pressed);
                pending.input_queue.clear();
                pending.input_overflow = true;
                if pressed {
                    self.suppressed_keys.insert(key);
                } else {
                    self.suppressed_keys.remove(&key);
                }
                return;
            }
            pending.input_queue.push_back(QueuedKey {
                key,
                pressed,
                host_modifiers: _host_modifiers,
            });
            if pressed {
                pending.queued_pressed.insert(key);
            } else {
                pending.queued_pressed.remove(&key);
            }
            return;
        }
        self.forward_keyboard_key(key, pressed, _host_modifiers);
    }

    fn forward_keyboard_key(&mut self, key: u32, pressed: bool, _host_modifiers: u32) {
        self.keyboard_focused = true;
        self.ensure_keyboard_enter();
        if !self.keyboard_entered {
            return;
        }
        if pressed {
            self.pressed_keys.insert(key);
        } else {
            self.pressed_keys.remove(&key);
        }
        self.keymap.state.update_key(
            xkb::Keycode::new(key.saturating_add(8)),
            if pressed {
                xkb::KeyDirection::Down
            } else {
                xkb::KeyDirection::Up
            },
        );
        let serial = self.next_serial();
        let state = if pressed {
            wl_keyboard::KeyState::Pressed
        } else {
            wl_keyboard::KeyState::Released
        };
        let time = monotonic_ms();
        let depressed = self.keymap.state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
        let latched = self.keymap.state.serialize_mods(xkb::STATE_MODS_LATCHED);
        let locked = self.keymap.state.serialize_mods(xkb::STATE_MODS_LOCKED);
        let group = self
            .keymap
            .state
            .serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
        self.keyboards.retain(Resource::is_alive);
        for keyboard in &self.keyboards {
            keyboard.key(serial, time, key, state);
            keyboard.modifiers(serial, depressed, latched, locked, group);
        }
    }

    fn keyboard_map_request(&mut self, request: KeyboardMapRequest) -> KeyboardMapReply {
        let method = request.method();
        match request {
            KeyboardMapRequest::Prepare {
                token,
                spec,
                keymap_sha256,
            } => self.prepare_keyboard_map(method, token, spec, keymap_sha256),
            KeyboardMapRequest::Status { token } => self.keyboard_map_status(method, &token),
            KeyboardMapRequest::Commit {
                token,
                keymap_sha256,
            } => self.commit_keyboard_map(method, token, keymap_sha256),
            KeyboardMapRequest::Abort {
                token,
                keymap_sha256,
            } => self.abort_keyboard_map(method, token, keymap_sha256),
        }
    }

    fn prepare_keyboard_map(
        &mut self,
        method: crate::keyboard::KeyboardMapMethod,
        token: String,
        spec: KeyboardMapSpec,
        requested_digest: String,
    ) -> KeyboardMapReply {
        if let Some(pending) = &self.pending_keymap {
            if pending.token == token && pending.digest == requested_digest {
                if pending.input_overflow {
                    return Err(KeyboardMapFailure::new(
                        "input_overflow",
                        "physical keyboard queue overflowed; restore the prior Sway map and abort this transaction",
                    ));
                }
                return Ok(self.keyboard_map_response(method, KeyboardMapState::Prepared));
            }
            return Err(KeyboardMapFailure::new(
                "transaction_busy",
                "another keyboard-map transaction is already prepared",
            ));
        }
        if let Some(completed) = &self.completed_keymap
            && completed.token == token
        {
            if completed.digest != requested_digest {
                return Err(KeyboardMapFailure::new(
                    "transaction_conflict",
                    "keyboard-map token was previously used with another digest",
                ));
            }
            return Ok(self.keyboard_map_response(method, completed.state));
        }
        let keymap = CompiledKeymap::compile(&self.xkb_config_root, &spec)
            .map_err(|error| KeyboardMapFailure::new("invalid_keymap", format!("{error:#}")))?;
        if keymap.digest != requested_digest {
            return Err(KeyboardMapFailure::new(
                "digest_mismatch",
                format!(
                    "requested keymap digest does not match the bundled definitions (host {})",
                    keymap.digest
                ),
            ));
        }
        self.neutralize_physical_keyboard();
        self.pending_keymap = Some(PendingKeymap {
            token,
            digest: requested_digest,
            keymap,
            input_queue: VecDeque::new(),
            queued_pressed: BTreeSet::new(),
            input_overflow: false,
        });
        Ok(self.keyboard_map_response(method, KeyboardMapState::Prepared))
    }

    fn keyboard_map_status(
        &self,
        method: crate::keyboard::KeyboardMapMethod,
        token: &str,
    ) -> KeyboardMapReply {
        // Status is always a reconciliation operation. Even after an input
        // queue overflow it must disclose the authoritative prepared token
        // and digest so a restarted guest can restore its prior Sway map and
        // issue the one Abort that unfreezes physical input. Commit remains
        // fail-closed and reports the overflow.
        let state = self
            .pending_keymap
            .as_ref()
            .filter(|pending| pending.token == token)
            .map(|_| KeyboardMapState::Prepared)
            .or_else(|| {
                self.completed_keymap
                    .as_ref()
                    .filter(|completed| completed.token == token)
                    .map(|completed| completed.state)
            })
            .unwrap_or(KeyboardMapState::Unknown);
        Ok(self.keyboard_map_response(method, state))
    }

    fn commit_keyboard_map(
        &mut self,
        method: crate::keyboard::KeyboardMapMethod,
        token: String,
        digest: String,
    ) -> KeyboardMapReply {
        if let Some(completed) = &self.completed_keymap
            && completed.token == token
        {
            if completed.digest == digest && completed.state == KeyboardMapState::Committed {
                return Ok(self.keyboard_map_response(method, KeyboardMapState::Committed));
            }
            return Err(KeyboardMapFailure::new(
                "transaction_conflict",
                "keyboard-map transaction already finished with different parameters",
            ));
        }
        let Some(pending) = self.pending_keymap.as_ref() else {
            return Err(KeyboardMapFailure::new(
                "transaction_unknown",
                "keyboard-map transaction is not prepared",
            ));
        };
        if pending.token != token || pending.digest != digest {
            return Err(KeyboardMapFailure::new(
                "transaction_conflict",
                "keyboard-map commit does not match the prepared transaction",
            ));
        }
        if pending.input_overflow {
            return Err(KeyboardMapFailure::new(
                "input_overflow",
                "physical keyboard queue overflowed; commit is fail-closed until the guest restores and aborts",
            ));
        }
        let pending = self
            .pending_keymap
            .take()
            .expect("pending keyboard map was checked above");
        self.keymap = pending.keymap;
        self.publish_keymap_and_neutral_modifiers();
        self.replay_queued_keys(pending.input_queue);
        self.completed_keymap = Some(CompletedKeymap {
            token,
            digest,
            state: KeyboardMapState::Committed,
        });
        Ok(self.keyboard_map_response(method, KeyboardMapState::Committed))
    }

    fn abort_keyboard_map(
        &mut self,
        method: crate::keyboard::KeyboardMapMethod,
        token: String,
        digest: String,
    ) -> KeyboardMapReply {
        if let Some(completed) = &self.completed_keymap
            && completed.token == token
        {
            if completed.digest == digest && completed.state == KeyboardMapState::Aborted {
                return Ok(self.keyboard_map_response(method, KeyboardMapState::Aborted));
            }
            return Err(KeyboardMapFailure::new(
                "transaction_conflict",
                "keyboard-map transaction already finished with different parameters",
            ));
        }
        let Some(pending) = self.pending_keymap.as_ref() else {
            return Err(KeyboardMapFailure::new(
                "transaction_unknown",
                "keyboard-map transaction is not prepared",
            ));
        };
        if pending.token != token || pending.digest != digest {
            return Err(KeyboardMapFailure::new(
                "transaction_conflict",
                "keyboard-map abort does not match the prepared transaction",
            ));
        }
        let pending = self
            .pending_keymap
            .take()
            .expect("pending keyboard map was checked above");
        if pending.input_overflow {
            // The queue was discarded and every still-held queued key was
            // moved to suppressed_keys when overflow was detected.
            self.publish_neutral_modifiers();
        } else {
            self.replay_queued_keys(pending.input_queue);
        }
        self.completed_keymap = Some(CompletedKeymap {
            token,
            digest,
            state: KeyboardMapState::Aborted,
        });
        Ok(self.keyboard_map_response(method, KeyboardMapState::Aborted))
    }

    fn keyboard_map_response(
        &self,
        method: crate::keyboard::KeyboardMapMethod,
        state: KeyboardMapState,
    ) -> KeyboardMapResponse {
        KeyboardMapResponse::success(
            method,
            state,
            self.keymap.digest.clone(),
            self.pending_keymap
                .as_ref()
                .map(|pending| (pending.token.as_str(), pending.digest.as_str())),
        )
    }

    fn neutralize_physical_keyboard(&mut self) {
        let released = std::mem::take(&mut self.pressed_keys);
        let time = monotonic_ms();
        for key in &released {
            self.keymap.state.update_key(
                xkb::Keycode::new(key.saturating_add(8)),
                xkb::KeyDirection::Up,
            );
            if self.keyboard_entered {
                let serial = self.next_serial();
                self.keyboards.retain(Resource::is_alive);
                for keyboard in &self.keyboards {
                    keyboard.key(serial, time, *key, wl_keyboard::KeyState::Released);
                }
            }
        }
        // update_key releases clear depressed keys, but locks and active
        // groups can survive. Recreate the state from the already verified
        // immutable keymap so the parent reports exactly neutral masks while
        // Sway changes its downstream map.
        self.keymap.reset_state();
        self.suppressed_keys.extend(released);
        self.publish_neutral_modifiers();
    }

    fn publish_keymap_and_neutral_modifiers(&mut self) {
        self.keyboards.retain(Resource::is_alive);
        for keyboard in &self.keyboards {
            keyboard.keymap(
                wl_keyboard::KeymapFormat::XkbV1,
                self.keymap.fd.as_fd(),
                self.keymap.size,
            );
        }
        self.publish_neutral_modifiers();
    }

    fn publish_neutral_modifiers(&mut self) {
        if !self.keyboard_entered {
            return;
        }
        let serial = self.next_serial();
        self.keyboards.retain(Resource::is_alive);
        for keyboard in &self.keyboards {
            keyboard.modifiers(serial, 0, 0, 0, 0);
        }
    }

    fn replay_queued_keys(&mut self, queue: VecDeque<QueuedKey>) {
        if !self.keyboard_focused {
            for event in queue {
                if event.pressed {
                    self.suppressed_keys.insert(event.key);
                } else {
                    self.suppressed_keys.remove(&event.key);
                }
            }
            return;
        }
        for event in queue {
            self.forward_keyboard_key(event.key, event.pressed, event.host_modifiers);
        }
    }

    fn configure_output(&mut self, mode: OutputMode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.toplevels.retain(|handle| handle.toplevel.is_alive());
        self.resend_output_configure();
    }

    fn send_toplevel_configure(&mut self, handle: &ToplevelHandle) {
        handle.toplevel.configure(
            self.mode.physical_width as i32,
            self.mode.physical_height as i32,
            Vec::new(),
        );
        handle.xdg_surface.configure(self.next_serial());
    }

    fn resend_output_configure(&mut self) {
        for handle in self.toplevels.clone() {
            self.send_toplevel_configure(&handle);
        }
    }

    fn send_dmabuf_feedback(
        &self,
        feedback: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
    ) -> Result<()> {
        let device = self
            .sync_device
            .as_ref()
            .context("dmabuf feedback requires the configured DRM render node")?;
        let (table, size) = create_dmabuf_format_table(&self.formats)?;
        let device_bytes = device.dev_t().to_ne_bytes().to_vec();
        let mut indices = Vec::with_capacity(self.formats.len().saturating_mul(2));
        for index in 0..self.formats.len() {
            let index = u16::try_from(index).context("too many host dmabuf formats")?;
            indices.extend_from_slice(&index.to_ne_bytes());
        }
        feedback.format_table(table.as_fd(), size);
        feedback.main_device(device_bytes.clone());
        feedback.tranche_target_device(device_bytes);
        feedback.tranche_flags(zwp_linux_dmabuf_feedback_v1::TrancheFlags::empty());
        feedback.tranche_formats(indices);
        feedback.tranche_done();
        feedback.done();
        Ok(())
    }

    fn commit_surface(&mut self, surface: &wl_surface::WlSurface, data: &SurfaceData) {
        let mut pending = data.pending.lock().expect("surface state poisoned");
        if let Some(xdg_surface) = data
            .xdg_surface
            .lock()
            .expect("surface role poisoned")
            .clone()
        {
            if !data.configured.swap(true, Ordering::AcqRel) {
                if let Some(handle) = self
                    .toplevels
                    .iter()
                    .find(|handle| handle.xdg_surface == xdg_surface)
                    .cloned()
                {
                    self.send_toplevel_configure(&handle);
                }
            }
        }

        let attached = pending.attached.take();
        let acquire_point = pending.acquire_point.take();
        let release_point = pending.release_point.take();
        let sync_surface = data
            .sync_surface
            .lock()
            .expect("surface sync role poisoned")
            .clone()
            .filter(Resource::is_alive);
        let Some(attached) = attached else {
            if acquire_point.is_some() || release_point.is_some() {
                if let Some(sync_surface) = sync_surface {
                    sync_surface.post_error(
                        wp_linux_drm_syncobj_surface_v1::Error::NoBuffer,
                        "syncobj points require a buffer attached in the same commit",
                    );
                }
            }
            self.idle_frame_callbacks
                .append(&mut pending.frame_callbacks);
            self.idle_presentation_feedback
                .append(&mut pending.presentation_feedback);
            return;
        };
        let callbacks = std::mem::take(&mut pending.frame_callbacks);
        let feedback = std::mem::take(&mut pending.presentation_feedback);
        drop(pending);

        let Some(buffer) = attached else {
            if acquire_point.is_some() || release_point.is_some() {
                if let Some(sync_surface) = sync_surface {
                    sync_surface.post_error(
                        wp_linux_drm_syncobj_surface_v1::Error::NoBuffer,
                        "syncobj points cannot accompany a null buffer",
                    );
                }
            }
            for callback in callbacks {
                callback.done(monotonic_ms());
            }
            for feedback in feedback {
                feedback.discarded();
            }
            return;
        };
        if acquire_point.is_none()
            && let Some(sync_surface) = sync_surface.as_ref()
        {
            sync_surface.post_error(
                wp_linux_drm_syncobj_surface_v1::Error::NoAcquirePoint,
                "dmabuf commit is missing an acquire timeline point",
            );
            self.reject_buffer(
                buffer,
                callbacks,
                feedback,
                release_point,
                true,
                "explicit-sync commit is missing an acquire timeline point",
            );
            return;
        }
        if release_point.is_none()
            && let Some(sync_surface) = sync_surface.as_ref()
        {
            sync_surface.post_error(
                wp_linux_drm_syncobj_surface_v1::Error::NoReleasePoint,
                "dmabuf commit is missing a release timeline point",
            );
            self.reject_buffer(
                buffer,
                callbacks,
                feedback,
                None,
                true,
                "explicit-sync commit is missing a release timeline point",
            );
            return;
        }
        if let (Some(acquire), Some(release)) = (&acquire_point, &release_point)
            && acquire.timeline.same_timeline(&release.timeline)
            && acquire.point >= release.point
        {
            sync_surface
                .as_ref()
                .expect("timeline points require a sync surface")
                .post_error(
                    wp_linux_drm_syncobj_surface_v1::Error::ConflictingPoints,
                    "acquire point must precede release point on the same timeline",
                );
            self.reject_buffer(
                buffer,
                callbacks,
                feedback,
                Some(release.clone()),
                true,
                "explicit-sync timeline points conflict",
            );
            return;
        }
        let acquire_wait_us = match acquire_point {
            Some(acquire) => match acquire.timeline.wait(acquire.point) {
                Ok(wait_us) => wait_us,
                Err(error) => {
                    self.reject_buffer(
                        buffer,
                        callbacks,
                        feedback,
                        release_point,
                        sync_surface.is_some(),
                        &format!("{error:#}"),
                    );
                    return;
                }
            },
            None => 0,
        };
        let explicit_sync = sync_surface.is_some();
        if self
            .cursor_surface
            .as_ref()
            .is_some_and(|cursor| cursor == surface)
        {
            let cursor_id = self.next_cursor_id;
            match cursor_image_from_buffer(cursor_id, &buffer, self.cursor_hotspot) {
                Ok(image) => {
                    let leased = matches!(image.storage, CursorStorage::Dmabuf { .. });
                    if leased {
                        self.next_cursor_id = self.next_cursor_id.wrapping_add(1).max(1);
                        self.cursor_leases.insert(
                            cursor_id,
                            CursorLease {
                                buffer: buffer.clone(),
                                release_point: release_point.clone(),
                                explicit_sync,
                            },
                        );
                    }
                    if let Err(error) = self.events.send(GatewayEvent::GuestCursor(image)) {
                        eprintln!("buzzardos-display: forwarding guest cursor: {error:#}");
                        if leased {
                            self.release_cursor(cursor_id);
                        }
                    }
                    if !leased {
                        self.release_cursor_commit(buffer.clone(), release_point, explicit_sync);
                    }
                }
                Err(error) => {
                    // Cursor format support is independent of the primary
                    // output. Never detach a valid guest scanout because one
                    // cursor image cannot be imported.
                    eprintln!(
                        "buzzardos-display: using host default cursor after guest cursor \
                         import failure: {error:#}"
                    );
                    let _ = self.events.send(GatewayEvent::GuestCursorFallback);
                    self.release_cursor_commit(buffer.clone(), release_point, explicit_sync);
                }
            }
            for callback in callbacks {
                callback.done(monotonic_ms());
            }
            for feedback in feedback {
                feedback.discarded();
            }
            return;
        }

        // wlroots' Wayland backend deliberately keeps its compiled 1280x720
        // bootstrap mode during the first xdg configure while the output is
        // disabled. Once the first buffer enables it, a second configure is
        // what turns the parent size into a wlr_output request-state event.
        // Send that configure before rejecting the bootstrap frame so the
        // next rendered frame has the exact native physical mode.
        if buffer
            .data::<BufferData>()
            .and_then(|data| match data {
                BufferData::Dmabuf(data) => Some((data.width, data.height)),
                BufferData::Shm(_) => None,
            })
            .is_some_and(|size| size != (self.mode.physical_width, self.mode.physical_height))
        {
            self.resend_output_configure();
        }

        match frame_from_buffer(
            self.next_frame_id,
            &buffer,
            self.mode,
            self.formats.as_slice(),
            explicit_sync,
            acquire_wait_us,
        ) {
            Ok(frame) => {
                let id = self.next_frame_id;
                self.next_frame_id = self.next_frame_id.wrapping_add(1).max(1);
                self.leases.insert(
                    id,
                    FrameLease {
                        buffer,
                        callbacks,
                        feedback,
                        release_point,
                        explicit_sync,
                        submitted: Instant::now(),
                        callback_sent: false,
                    },
                );
                if let Err(error) = self.events.send(GatewayEvent::GuestFrame(frame)) {
                    self.release_frame(id, monotonic_us());
                    let _ = self
                        .events
                        .send(GatewayEvent::GuestFailed(format!("{error:#}")));
                }
            }
            Err(error) => {
                let explicit_sync = release_point.is_some();
                if let Some(point) = release_point
                    && let Err(signal_error) = point.timeline.signal(point.point)
                {
                    let _ = self.events.send(GatewayEvent::GuestFailed(format!(
                        "{error:#}; additionally failed to signal release point: {signal_error:#}"
                    )));
                }
                if !explicit_sync {
                    buffer.release();
                }
                for callback in callbacks {
                    callback.done(monotonic_ms());
                }
                for feedback in feedback {
                    feedback.discarded();
                }
                let _ = self
                    .events
                    .send(GatewayEvent::GuestFailed(format!("{error:#}")));
            }
        }
        let _ = surface;
    }

    fn reject_buffer(
        &self,
        buffer: wl_buffer::WlBuffer,
        callbacks: Vec<wl_callback::WlCallback>,
        feedback: Vec<wp_presentation_feedback::WpPresentationFeedback>,
        release_point: Option<TimelinePoint>,
        explicit_sync: bool,
        error: &str,
    ) {
        if let Some(point) = release_point
            && let Err(signal_error) = point.timeline.signal(point.point)
        {
            let _ = self.events.send(GatewayEvent::GuestFailed(format!(
                "{error}; additionally failed to signal release point: {signal_error:#}"
            )));
        }
        if !explicit_sync {
            buffer.release();
        }
        for callback in callbacks {
            callback.done(monotonic_ms());
        }
        for feedback in feedback {
            feedback.discarded();
        }
        let _ = self
            .events
            .send(GatewayEvent::GuestFailed(error.to_owned()));
    }

    fn present_frame(
        &mut self,
        id: u64,
        presentation_time_us: i64,
        refresh_interval_us: i64,
        sequence: u64,
        offloaded: bool,
    ) {
        let Some(lease) = self.leases.get_mut(&id) else {
            return;
        };
        if presentation_time_us <= 0 {
            for feedback in lease.feedback.drain(..) {
                feedback.discarded();
            }
            return;
        }
        let seconds = (presentation_time_us.max(0) as u64) / 1_000_000;
        let nanoseconds =
            ((presentation_time_us.max(0) as u64) % 1_000_000).saturating_mul(1_000) as u32;
        let refresh_ns = refresh_interval_us.max(0).saturating_mul(1_000) as u32;
        let flags = if offloaded {
            wp_presentation_feedback::Kind::Vsync
                | wp_presentation_feedback::Kind::HwClock
                | wp_presentation_feedback::Kind::HwCompletion
        } else {
            wp_presentation_feedback::Kind::Vsync
        };
        for feedback in lease.feedback.drain(..) {
            feedback.presented(
                (seconds >> 32) as u32,
                seconds as u32,
                nanoseconds,
                refresh_ns,
                (sequence >> 32) as u32,
                sequence as u32,
                flags,
            );
        }
    }

    fn paint_frame(&mut self, id: u64, frame_time_us: i64) {
        let Some(lease) = self.leases.get_mut(&id) else {
            return;
        };
        if lease.callback_sent {
            return;
        }
        lease.callback_sent = true;
        let milliseconds = frame_time_us.max(0) as u64 / 1_000;
        for callback in lease.callbacks.drain(..) {
            callback.done(milliseconds as u32);
        }
    }

    fn release_frame(&mut self, id: u64, released_monotonic_us: u64) {
        let Some(mut lease) = self.leases.remove(&id) else {
            return;
        };
        let held_us = lease
            .submitted
            .elapsed()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        if let Some(point) = lease.release_point.take()
            && let Err(error) = point.timeline.signal(point.point)
        {
            let _ = self
                .events
                .send(GatewayEvent::GuestFailed(format!("{error:#}")));
        }
        // linux-drm-syncobj-v1 makes wl_buffer.release delivery undefined
        // while its per-surface object is alive. The release timeline point
        // is the only reuse signal for explicit-sync frames.
        if !lease.explicit_sync {
            lease.buffer.release();
        }
        if !lease.callback_sent {
            for callback in lease.callbacks.drain(..) {
                callback.done(monotonic_ms());
            }
            for feedback in lease.feedback.drain(..) {
                feedback.discarded();
            }
        }
        let _ = released_monotonic_us;
        let _ = self
            .events
            .send(GatewayEvent::FrameReleased { id, held_us });
    }

    fn release_cursor(&mut self, id: u64) {
        let Some(lease) = self.cursor_leases.remove(&id) else {
            return;
        };
        self.release_cursor_commit(lease.buffer, lease.release_point, lease.explicit_sync);
    }

    fn release_cursor_commit(
        &self,
        buffer: wl_buffer::WlBuffer,
        release_point: Option<TimelinePoint>,
        explicit_sync: bool,
    ) {
        if let Some(point) = release_point
            && let Err(error) = point.timeline.signal(point.point)
        {
            eprintln!("buzzardos-display: signaling guest cursor release: {error:#}");
        }
        if !explicit_sync {
            buffer.release();
        }
    }

    fn drop_client_leases(&mut self) {
        for (_, mut lease) in self.leases.drain() {
            if let Some(point) = lease.release_point.take() {
                let _ = point.timeline.signal(point.point);
            }
        }
        for (_, mut lease) in self.cursor_leases.drain() {
            if let Some(point) = lease.release_point.take() {
                let _ = point.timeline.signal(point.point);
            }
        }
        self.toplevels.clear();
        self.idle_frame_callbacks.clear();
        self.idle_presentation_feedback.clear();
        self.pointers.clear();
        self.keyboards.clear();
        self.focused_surface = None;
        self.pointer_entered = false;
        self.keyboard_focused = false;
        self.keyboard_entered = false;
        self.suppressed_keys.append(&mut self.pressed_keys);
        self.keymap.reset_state();
        if let Some(pending) = &mut self.pending_keymap {
            // A queued event belongs to the dead client's focus epoch and may
            // never be replayed into a replacement compositor connection.
            // Still-held keys remain suppressed until their physical release.
            self.suppressed_keys.append(&mut pending.queued_pressed);
            pending.input_queue.clear();
        }
    }
}

#[derive(Clone)]
struct ToplevelHandle {
    toplevel: xdg_toplevel::XdgToplevel,
    xdg_surface: xdg_surface::XdgSurface,
}

struct FrameLease {
    buffer: wl_buffer::WlBuffer,
    callbacks: Vec<wl_callback::WlCallback>,
    feedback: Vec<wp_presentation_feedback::WpPresentationFeedback>,
    release_point: Option<TimelinePoint>,
    explicit_sync: bool,
    submitted: Instant,
    callback_sent: bool,
}

struct CursorLease {
    buffer: wl_buffer::WlBuffer,
    release_point: Option<TimelinePoint>,
    explicit_sync: bool,
}

#[derive(Default)]
struct SurfaceData {
    pending: Mutex<SurfacePending>,
    xdg_surface: Mutex<Option<xdg_surface::XdgSurface>>,
    sync_surface: Mutex<Option<wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1>>,
    configured: AtomicBool,
}

#[derive(Default)]
struct SurfacePending {
    attached: Option<Option<wl_buffer::WlBuffer>>,
    frame_callbacks: Vec<wl_callback::WlCallback>,
    presentation_feedback: Vec<wp_presentation_feedback::WpPresentationFeedback>,
    acquire_point: Option<TimelinePoint>,
    release_point: Option<TimelinePoint>,
}

struct XdgSurfaceData {
    surface: wl_surface::WlSurface,
}

struct ToplevelData {
    xdg_surface: xdg_surface::XdgSurface,
}

struct ViewportData {
    surface: wl_surface::WlSurface,
}

struct SyncSurfaceData {
    surface: wl_surface::WlSurface,
}

#[derive(Clone)]
struct TimelinePoint {
    timeline: SyncobjTimeline,
    point: u64,
}

struct ShmPoolData {
    fd: OwnedFd,
    size: Mutex<i32>,
}

struct ShmBufferData {
    fd: OwnedFd,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: wl_shm::Format,
}

struct DmabufPlaneData {
    fd: OwnedFd,
    plane: u32,
    offset: u32,
    stride: u32,
    modifier: u64,
}

#[derive(Default)]
struct DmabufParamsData {
    planes: Mutex<Vec<DmabufPlaneData>>,
    used: AtomicBool,
}

struct DmabufBufferData {
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    planes: Vec<DmabufPlaneData>,
}

enum BufferData {
    Dmabuf(DmabufBufferData),
    Shm(ShmBufferData),
}

fn create_dmabuf_format_table(formats: &[DmabufFormat]) -> Result<(File, u32)> {
    let size = formats
        .len()
        .checked_mul(16)
        .and_then(|size| u32::try_from(size).ok())
        .context("dmabuf format table is too large")?;
    let name = b"buzzardos-dmabuf-formats\0";
    // SAFETY: the name is NUL-terminated and flags are valid.
    let raw = unsafe {
        libc::memfd_create(
            name.as_ptr().cast(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("creating dmabuf format table");
    }
    // SAFETY: memfd_create returned a new owned descriptor.
    let mut file = unsafe { File::from_raw_fd(raw) };
    for format in formats {
        file.write_all(&format.fourcc.to_ne_bytes())
            .context("writing dmabuf format")?;
        file.write_all(&[0_u8; 4])
            .context("writing dmabuf format padding")?;
        file.write_all(&format.modifier.to_ne_bytes())
            .context("writing dmabuf modifier")?;
    }
    file.seek(SeekFrom::Start(0))
        .context("rewinding dmabuf format table")?;
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
    // SAFETY: `file` is a sealable memfd and the seal mask is valid.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(std::io::Error::last_os_error()).context("sealing dmabuf format table");
    }
    Ok((file, size))
}

fn frame_from_buffer(
    id: u64,
    buffer: &wl_buffer::WlBuffer,
    mode: OutputMode,
    formats: &[DmabufFormat],
    explicit_sync: bool,
    acquire_wait_us: u64,
) -> Result<DmabufFrame> {
    let data = buffer
        .data::<BufferData>()
        .context("nested compositor attached an unknown wl_buffer")?;
    let data = match data {
        BufferData::Dmabuf(data) => data,
        BufferData::Shm(data) => {
            let _ = (
                data.offset,
                data.width,
                data.height,
                data.stride,
                data.format,
            );
            bail!(
                "nested compositor attached shared memory to its primary output; fast path failed"
            );
        }
    };
    if data.width != mode.physical_width || data.height != mode.physical_height {
        bail!(
            "nested output buffer {}x{} does not match native physical mode {}x{}",
            data.width,
            data.height,
            mode.physical_width,
            mode.physical_height
        );
    }
    if !formats
        .iter()
        .any(|format| format.fourcc == data.fourcc && format.modifier == data.modifier)
    {
        bail!(
            "guest dmabuf format {:#010x}/{:#018x} is not importable by the host display",
            data.fourcc,
            data.modifier
        );
    }
    let planes = data
        .planes
        .iter()
        .map(|plane| {
            Ok(DmabufPlane {
                fd: plane.fd.try_clone().context("duplicating dmabuf plane")?,
                offset: plane.offset,
                stride: plane.stride,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(DmabufFrame {
        id,
        geometry_generation: mode.geometry_generation,
        width: data.width,
        height: data.height,
        fourcc: data.fourcc,
        modifier: data.modifier,
        planes,
        submitted_monotonic_us: monotonic_us(),
        explicit_sync,
        acquire_wait_us,
    })
}

fn cursor_image_from_buffer(
    id: u64,
    buffer: &wl_buffer::WlBuffer,
    hotspot: (i32, i32),
) -> Result<CursorImage> {
    let data = buffer
        .data::<BufferData>()
        .context("nested compositor attached an unknown cursor buffer")?;
    let (width, height) = match data {
        BufferData::Shm(data) => (data.width, data.height),
        BufferData::Dmabuf(data) => (
            i32::try_from(data.width).context("cursor width overflow")?,
            i32::try_from(data.height).context("cursor height overflow")?,
        ),
    };
    anyhow::ensure!(
        width > 0 && height > 0 && width <= 512 && height <= 512,
        "cursor dimensions {}x{} are invalid",
        width,
        height
    );

    let (id, storage) = match data {
        BufferData::Shm(data) => {
            anyhow::ensure!(
                data.offset >= 0 && data.stride >= data.width.saturating_mul(4),
                "cursor offset/stride is invalid"
            );
            anyhow::ensure!(
                matches!(
                    data.format,
                    wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
                ),
                "cursor wl_shm format {:?} is unsupported",
                data.format
            );
            let stride = usize::try_from(data.stride).context("cursor stride overflow")?;
            let length = stride
                .checked_mul(usize::try_from(data.height).context("cursor height overflow")?)
                .context("cursor byte length overflow")?;
            let mut pixels = vec![0_u8; length];
            // SAFETY: `pixels` owns `length` writable bytes, the retained pool
            // fd is valid, and the validated non-negative offset fits off_t.
            let read = unsafe {
                libc::pread(
                    data.fd.as_raw_fd(),
                    pixels.as_mut_ptr().cast(),
                    pixels.len(),
                    i64::from(data.offset),
                )
            };
            if read < 0 {
                return Err(std::io::Error::last_os_error())
                    .context("reading cursor wl_shm buffer");
            }
            anyhow::ensure!(
                usize::try_from(read).ok() == Some(length),
                "cursor wl_shm buffer was truncated"
            );
            if data.format == wl_shm::Format::Xrgb8888 {
                for row in pixels.chunks_exact_mut(stride) {
                    for pixel in row[..usize::try_from(data.width).unwrap_or_default() * 4]
                        .chunks_exact_mut(4)
                    {
                        pixel[3] = 255;
                    }
                }
            }
            (0, CursorStorage::Shm { stride, pixels })
        }
        BufferData::Dmabuf(data) => (
            id,
            CursorStorage::Dmabuf {
                fourcc: data.fourcc,
                modifier: data.modifier,
                planes: data
                    .planes
                    .iter()
                    .map(|plane| {
                        Ok(DmabufPlane {
                            fd: plane
                                .fd
                                .try_clone()
                                .context("duplicating cursor dmabuf plane")?,
                            offset: plane.offset,
                            stride: plane.stride,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            },
        ),
    };
    Ok(CursorImage {
        id,
        width: width as u32,
        height: height as u32,
        hotspot_x: hotspot.0.clamp(0, width.saturating_sub(1)),
        hotspot_y: hotspot.1.clamp(0, height.saturating_sub(1)),
        storage,
    })
}

fn validate_dmabuf(
    params: &DmabufParamsData,
    width: i32,
    height: i32,
    format: u32,
    formats: &[DmabufFormat],
) -> Result<DmabufBufferData> {
    if params.used.swap(true, Ordering::AcqRel) {
        bail!("linux-dmabuf params object was already used");
    }
    if width <= 0 || height <= 0 {
        bail!("linux-dmabuf buffer dimensions must be positive");
    }
    let mut planes = params.planes.lock().expect("dmabuf params poisoned");
    if planes.is_empty() || planes.len() > MAX_DMABUF_PLANES {
        bail!("linux-dmabuf requires between one and four planes");
    }
    planes.sort_by_key(|plane| plane.plane);
    for (expected, plane) in planes.iter().enumerate() {
        if plane.plane as usize != expected {
            bail!("linux-dmabuf plane indexes must be contiguous from zero");
        }
    }
    let modifier = planes[0].modifier;
    if planes.iter().any(|plane| plane.modifier != modifier) {
        bail!("linux-dmabuf planes use inconsistent modifiers");
    }
    if !formats
        .iter()
        .any(|candidate| candidate.fourcc == format && candidate.modifier == modifier)
    {
        bail!("linux-dmabuf format/modifier was not advertised");
    }
    Ok(DmabufBufferData {
        width: width as u32,
        height: height as u32,
        fourcc: format,
        modifier,
        planes: std::mem::take(&mut *planes),
    })
}

fn monotonic_ms() -> u32 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a valid writable timespec.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
    }
    ((time.tv_sec as u64 * 1_000) + (time.tv_nsec as u64 / 1_000_000)) as u32
}

fn frame_time_us_to_protocol_ms(frame_time_us: i64) -> u32 {
    if frame_time_us <= 0 {
        monotonic_ms()
    } else {
        (frame_time_us as u64 / 1_000) as u32
    }
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

macro_rules! simple_global {
    ($interface:ty) => {
        impl GlobalDispatch<$interface, ()> for GuestState {
            fn bind(
                _state: &mut Self,
                _handle: &DisplayHandle,
                _client: &Client,
                resource: New<$interface>,
                _global_data: &(),
                data_init: &mut DataInit<'_, Self>,
            ) {
                data_init.init(resource, ());
            }
        }
    };
}

simple_global!(wl_compositor::WlCompositor);
simple_global!(xdg_wm_base::XdgWmBase);
simple_global!(wp_viewporter::WpViewporter);
simple_global!(wp_presentation::WpPresentation);
simple_global!(wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1);

impl GlobalDispatch<wl_shm::WlShm, ()> for GuestState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_shm::WlShm>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let shm = data_init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl GlobalDispatch<wl_seat::WlSeat, ()> for GuestState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_seat::WlSeat>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, ());
        seat.capabilities(wl_seat::Capability::Pointer | wl_seat::Capability::Keyboard);
        if seat.version() >= 2 {
            seat.name("buzzardos-seat".into());
        }
    }
}

impl GlobalDispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for GuestState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let dmabuf = data_init.init(resource, ());
        if dmabuf.version() < 4 {
            for format in &state.formats {
                dmabuf.modifier(
                    format.fourcc,
                    (format.modifier >> 32) as u32,
                    format.modifier as u32,
                );
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                data_init.init(id, SurfaceData::default());
            }
            wl_compositor::Request::CreateRegion { id } => {
                data_init.init(id, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_region::WlRegion, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_region::WlRegion,
        _request: wl_region::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, SurfaceData> for GuestState {
    fn request(
        state: &mut Self,
        _client: &Client,
        surface: &wl_surface::WlSurface,
        request: wl_surface::Request,
        data: &SurfaceData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Attach { buffer, .. } => {
                data.pending
                    .lock()
                    .expect("surface state poisoned")
                    .attached = Some(buffer);
            }
            wl_surface::Request::Frame { callback } => {
                let callback = data_init.init(callback, ());
                data.pending
                    .lock()
                    .expect("surface state poisoned")
                    .frame_callbacks
                    .push(callback);
            }
            wl_surface::Request::Commit => state.commit_surface(surface, data),
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_callback::WlCallback,
        _request: wl_callback::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wl_shm::WlShm, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_shm::WlShm,
        request: wl_shm::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, fd, size } = request {
            data_init.init(
                id,
                ShmPoolData {
                    fd,
                    size: Mutex::new(size),
                },
            );
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ShmPoolData> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        data: &ShmPoolData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_shm_pool::Request::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                let Ok(format) = format.into_result() else {
                    data_init.post_error(id, 0_u32, "unsupported shared-memory format");
                    return;
                };
                let Ok(fd) = data.fd.try_clone() else {
                    data_init.post_error(id, 0_u32, "could not retain shared-memory pool");
                    return;
                };
                data_init.init(
                    id,
                    BufferData::Shm(ShmBufferData {
                        fd,
                        offset,
                        width,
                        height,
                        stride,
                        format,
                    }),
                );
            }
            wl_shm_pool::Request::Resize { size } => {
                *data.size.lock().expect("shm pool poisoned") = size;
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, BufferData> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_buffer::WlBuffer,
        _request: wl_buffer::Request,
        _data: &BufferData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &xdg_wm_base::XdgWmBase,
        request: xdg_wm_base::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_base::Request::CreatePositioner { id } => {
                data_init.init(id, ());
            }
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                let xdg_surface = data_init.init(
                    id,
                    XdgSurfaceData {
                        surface: surface.clone(),
                    },
                );
                if let Some(data) = surface.data::<SurfaceData>() {
                    *data.xdg_surface.lock().expect("surface role poisoned") = Some(xdg_surface);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_positioner::XdgPositioner, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &xdg_positioner::XdgPositioner,
        _request: xdg_positioner::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<xdg_surface::XdgSurface, XdgSurfaceData> for GuestState {
    fn request(
        state: &mut Self,
        _client: &Client,
        surface: &xdg_surface::XdgSurface,
        request: xdg_surface::Request,
        data: &XdgSurfaceData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_surface::Request::GetToplevel { id } => {
                let toplevel = data_init.init(
                    id,
                    ToplevelData {
                        xdg_surface: surface.clone(),
                    },
                );
                let handle = ToplevelHandle {
                    toplevel,
                    xdg_surface: surface.clone(),
                };
                state.toplevels.push(handle);
                state.focused_surface = Some(data.surface.clone());
                state.pointer_entered = false;
                state.keyboard_entered = false;
                state.ensure_pointer_enter();
                state.ensure_keyboard_enter();
            }
            xdg_surface::Request::GetPopup { id, .. } => {
                data_init.init(id, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ToplevelData> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &xdg_toplevel::XdgToplevel,
        _request: xdg_toplevel::Request,
        data: &ToplevelData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let _ = &data.xdg_surface;
    }
}

impl Dispatch<xdg_popup::XdgPopup, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &xdg_popup::XdgPopup,
        _request: xdg_popup::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wp_viewporter::WpViewporter, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wp_viewporter::WpViewporter,
        request: wp_viewporter::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_viewporter::Request::GetViewport { id, surface } = request {
            data_init.init(id, ViewportData { surface });
        }
    }
}

impl Dispatch<wp_viewport::WpViewport, ViewportData> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wp_viewport::WpViewport,
        _request: wp_viewport::Request,
        data: &ViewportData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let _ = &data.surface;
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for GuestState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        request: zwp_linux_dmabuf_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_linux_dmabuf_v1::Request::CreateParams { params_id } => {
                data_init.init(params_id, DmabufParamsData::default());
            }
            zwp_linux_dmabuf_v1::Request::GetDefaultFeedback { id }
            | zwp_linux_dmabuf_v1::Request::GetSurfaceFeedback { id, .. } => {
                let feedback = data_init.init(id, ());
                if let Err(error) = state.send_dmabuf_feedback(&feedback) {
                    resource.post_error(0_u32, format!("{error:#}"));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        _request: zwp_linux_dmabuf_feedback_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, DmabufParamsData> for GuestState {
    fn request(
        state: &mut Self,
        client: &Client,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        request: zwp_linux_buffer_params_v1::Request,
        data: &DmabufParamsData,
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            zwp_linux_buffer_params_v1::Request::Add {
                fd,
                plane_idx,
                offset,
                stride,
                modifier_hi,
                modifier_lo,
            } => {
                let mut planes = data.planes.lock().expect("dmabuf params poisoned");
                if planes.iter().any(|plane| plane.plane == plane_idx)
                    || plane_idx as usize >= MAX_DMABUF_PLANES
                {
                    params.post_error(0_u32, "invalid or duplicate dmabuf plane");
                    return;
                }
                planes.push(DmabufPlaneData {
                    fd,
                    plane: plane_idx,
                    offset,
                    stride,
                    modifier: ((modifier_hi as u64) << 32) | modifier_lo as u64,
                });
            }
            zwp_linux_buffer_params_v1::Request::Create {
                width,
                height,
                format,
                ..
            } => match validate_dmabuf(data, width, height, format, &state.formats) {
                Ok(buffer_data) => {
                    match client.create_resource::<wl_buffer::WlBuffer, _, GuestState>(
                        handle,
                        1,
                        BufferData::Dmabuf(buffer_data),
                    ) {
                        Ok(buffer) => params.created(&buffer),
                        Err(_error) => params.failed(),
                    }
                }
                Err(_) => params.failed(),
            },
            zwp_linux_buffer_params_v1::Request::CreateImmed {
                buffer_id,
                width,
                height,
                format,
                ..
            } => match validate_dmabuf(data, width, height, format, &state.formats) {
                Ok(buffer_data) => {
                    data_init.init(buffer_id, BufferData::Dmabuf(buffer_data));
                }
                Err(error) => {
                    data_init.post_error(buffer_id, 0_u32, format!("{error:#}"));
                }
            },
            _ => {}
        }
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wp_presentation::WpPresentation,
        request: wp_presentation::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_presentation::Request::Feedback { surface, callback } = request {
            let feedback = data_init.init(callback, ());
            if let Some(data) = surface.data::<SurfaceData>() {
                data.pending
                    .lock()
                    .expect("surface state poisoned")
                    .presentation_feedback
                    .push(feedback);
            } else {
                feedback.discarded();
            }
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wp_presentation_feedback::WpPresentationFeedback,
        _request: wp_presentation_feedback::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1, ()> for GuestState {
    fn request(
        state: &mut Self,
        _client: &Client,
        manager: &wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1,
        request: wp_linux_drm_syncobj_manager_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wp_linux_drm_syncobj_manager_v1::Request::GetSurface { id, surface } => {
                let Some(surface_data) = surface.data::<SurfaceData>() else {
                    manager.post_error(
                        wp_linux_drm_syncobj_manager_v1::Error::SurfaceExists,
                        "unknown wl_surface",
                    );
                    return;
                };
                let mut current = surface_data
                    .sync_surface
                    .lock()
                    .expect("surface sync role poisoned");
                if current.as_ref().is_some_and(Resource::is_alive) {
                    manager.post_error(
                        wp_linux_drm_syncobj_manager_v1::Error::SurfaceExists,
                        "wl_surface already has an explicit-sync object",
                    );
                    return;
                }
                let sync_surface = data_init.init(
                    id,
                    SyncSurfaceData {
                        surface: surface.clone(),
                    },
                );
                *current = Some(sync_surface);
            }
            wp_linux_drm_syncobj_manager_v1::Request::ImportTimeline { id, fd } => {
                let Some(device) = state.sync_device.as_ref() else {
                    manager.post_error(
                        wp_linux_drm_syncobj_manager_v1::Error::InvalidTimeline,
                        "explicit synchronization is not configured",
                    );
                    return;
                };
                match device.import_timeline(fd) {
                    Ok(timeline) => {
                        data_init.init(id, timeline);
                    }
                    Err(error) => {
                        manager.post_error(
                            wp_linux_drm_syncobj_manager_v1::Error::InvalidTimeline,
                            format!("{error:#}"),
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1, SyncobjTimeline>
    for GuestState
{
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1,
        _request: wp_linux_drm_syncobj_timeline_v1::Request,
        _data: &SyncobjTimeline,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1, SyncSurfaceData>
    for GuestState
{
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1,
        request: wp_linux_drm_syncobj_surface_v1::Request,
        data: &SyncSurfaceData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(surface_data) = data.surface.data::<SurfaceData>() else {
            resource.post_error(
                wp_linux_drm_syncobj_surface_v1::Error::NoSurface,
                "associated wl_surface no longer exists",
            );
            return;
        };
        match request {
            wp_linux_drm_syncobj_surface_v1::Request::SetAcquirePoint {
                timeline,
                point_hi,
                point_lo,
            } => {
                let Some(timeline) = timeline.data::<SyncobjTimeline>().cloned() else {
                    resource.post_error(
                        wp_linux_drm_syncobj_surface_v1::Error::NoAcquirePoint,
                        "unknown acquire timeline",
                    );
                    return;
                };
                surface_data
                    .pending
                    .lock()
                    .expect("surface state poisoned")
                    .acquire_point = Some(TimelinePoint {
                    timeline,
                    point: ((point_hi as u64) << 32) | point_lo as u64,
                });
            }
            wp_linux_drm_syncobj_surface_v1::Request::SetReleasePoint {
                timeline,
                point_hi,
                point_lo,
            } => {
                let Some(timeline) = timeline.data::<SyncobjTimeline>().cloned() else {
                    resource.post_error(
                        wp_linux_drm_syncobj_surface_v1::Error::NoReleasePoint,
                        "unknown release timeline",
                    );
                    return;
                };
                surface_data
                    .pending
                    .lock()
                    .expect("surface state poisoned")
                    .release_point = Some(TimelinePoint {
                    timeline,
                    point: ((point_hi as u64) << 32) | point_lo as u64,
                });
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for GuestState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => {
                let pointer = data_init.init(id, ());
                state.pointers.push(pointer);
                state.pointer_entered = false;
                state.ensure_pointer_enter();
            }
            wl_seat::Request::GetKeyboard { id } => {
                let keyboard = data_init.init(id, ());
                keyboard.keymap(
                    wl_keyboard::KeymapFormat::XkbV1,
                    state.keymap.fd.as_fd(),
                    state.keymap.size,
                );
                if keyboard.version() >= 4 {
                    keyboard.repeat_info(33, 500);
                }
                state.keyboards.push(keyboard);
                state.keyboard_entered = false;
                state.ensure_keyboard_enter();
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for GuestState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &wl_pointer::WlPointer,
        request: wl_pointer::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_pointer::Request::SetCursor {
            surface,
            hotspot_x,
            hotspot_y,
            ..
        } = request
        {
            state.cursor_surface = surface;
            state.cursor_hotspot = (hotspot_x, hotspot_y);
            if state.cursor_surface.is_none() {
                let _ = state.events.send(GatewayEvent::GuestCursorHidden);
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for GuestState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_keyboard::WlKeyboard,
        _request: wl_keyboard::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> GuestState {
        let (_event_read, event_write) = UnixStream::pair().unwrap();
        let (event_sender, _event_receiver) = std::sync::mpsc::channel();
        let events = EventSender {
            sender: event_sender,
            wake: Arc::new(event_write),
        };
        GuestState::new(
            events,
            vec![DmabufFormat {
                fourcc: DRM_FORMAT_XRGB8888,
                modifier: DRM_FORMAT_MOD_INVALID,
            }],
            OutputMode {
                logical_width: 1280,
                logical_height: 800,
                physical_width: 1280,
                physical_height: 800,
                host_surface_scale_120: 120,
                guest_ui_scale_120: 120,
                geometry_generation: 1,
                refresh_mhz: 60_000,
            },
            None,
            PathBuf::from("/usr/share/X11/xkb"),
        )
        .unwrap()
    }

    fn german_prepare(token: &str) -> KeyboardMapRequest {
        let spec = KeyboardMapSpec {
            model: "pc105".into(),
            layout: "de".into(),
            variant: String::new(),
            options: String::new(),
        };
        let digest = CompiledKeymap::compile(PathBuf::from("/usr/share/X11/xkb").as_path(), &spec)
            .unwrap()
            .digest;
        KeyboardMapRequest::Prepare {
            token: token.into(),
            spec,
            keymap_sha256: digest,
        }
    }

    #[test]
    fn host_frame_time_converts_to_wayland_callback_milliseconds() {
        assert_eq!(frame_time_us_to_protocol_ms(12_345_678), 12_345);
        assert_eq!(
            frame_time_us_to_protocol_ms(i64::from(u32::MAX) * 1_000 + 9_000),
            8
        );
    }

    #[test]
    fn private_display_accepts_only_during_compositor_reconnect() {
        assert_eq!(private_display_socket_mode(false), 0o600);
        assert_eq!(private_display_socket_mode(true), 0o000);
    }

    #[test]
    fn delayed_configuration_unblocks_a_preconnected_guest() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("guest.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let (notification_reader, mut notification_writer) = UnixStream::pair().unwrap();
        notification_reader.set_nonblocking(true).unwrap();
        let (command_sender, command_receiver) = std::sync::mpsc::channel();
        let (waiting_sender, waiting_receiver) = std::sync::mpsc::channel();

        let guest_thread = std::thread::spawn(move || {
            waiting_sender.send(()).unwrap();
            let configured = wait_for_configuration(&command_receiver, &notification_reader)
                .expect("delayed native configuration");
            listener.accept().expect("queued guest connection");
            configured
        });

        waiting_receiver.recv().unwrap();
        // The private socket is bound before GTK knows its exact monitor
        // allocation. A compositor that connects in that interval queues in
        // the listener backlog while the gateway waits for Configure.
        let _queued_guest = UnixStream::connect(&socket_path).unwrap();
        let format = DmabufFormat {
            fourcc: DRM_FORMAT_XRGB8888,
            modifier: DRM_FORMAT_MOD_INVALID,
        };
        let mode = OutputMode {
            logical_width: 1280,
            logical_height: 800,
            physical_width: 1600,
            physical_height: 1000,
            host_surface_scale_120: 160,
            guest_ui_scale_120: 150,
            geometry_generation: 9,
            refresh_mhz: 60_000,
        };
        command_sender
            .send(GatewayCommand::Configure {
                formats: vec![format],
                mode,
            })
            .unwrap();
        notification_writer.write_all(&[1]).unwrap();

        assert_eq!(guest_thread.join().unwrap(), (vec![format], mode));
    }

    #[test]
    fn known_drm_fourcc_values_are_little_endian() {
        assert_eq!(DRM_FORMAT_ARGB8888, 0x3432_5241);
        assert_eq!(DRM_FORMAT_XRGB8888, 0x3432_5258);
        assert_eq!(DRM_FORMAT_MOD_INVALID, 0x00ff_ffff_ffff_ffff);
    }

    #[test]
    fn keyboard_commit_is_atomic_idempotent_and_suppresses_held_keys() {
        let token = "0123456789abcdef0123456789abcdef";
        let mut state = test_state();
        state.keyboard_focused = true;
        state.keyboard_entered = true;
        state.pressed_keys.insert(14);
        state
            .keymap
            .state
            .update_key(xkb::Keycode::new(22), xkb::KeyDirection::Down);
        state
            .keymap
            .state
            .update_key(xkb::Keycode::new(66), xkb::KeyDirection::Down);
        state
            .keymap
            .state
            .update_key(xkb::Keycode::new(66), xkb::KeyDirection::Up);
        assert_ne!(state.keymap.state.serialize_mods(xkb::STATE_MODS_LOCKED), 0);

        let unknown = state
            .keyboard_map_request(KeyboardMapRequest::Status {
                token: token.into(),
            })
            .unwrap();
        let unknown = serde_json::to_value(unknown).unwrap();
        assert_eq!(unknown["state"], "unknown");
        assert!(unknown.get("pending_token").is_none());

        let prepare = german_prepare(token);
        let digest = match &prepare {
            KeyboardMapRequest::Prepare { keymap_sha256, .. } => keymap_sha256.clone(),
            _ => unreachable!(),
        };
        let prepared = serde_json::to_value(state.keyboard_map_request(prepare).unwrap()).unwrap();
        assert_eq!(prepared["state"], "prepared");
        assert_eq!(prepared["pending_token"], token);
        assert_eq!(prepared["pending_keymap_sha256"], digest);
        assert!(state.pressed_keys.is_empty());
        assert!(state.suppressed_keys.contains(&14));
        assert_eq!(state.keymap.state.serialize_mods(xkb::STATE_MODS_LOCKED), 0);

        // Right Alt then Q must replay in this exact order under German, so
        // the level-three modifier and letter state agree after Commit.
        state.keyboard_key(100, true, 0);
        state.keyboard_key(16, true, 0);

        let committed = state
            .keyboard_map_request(KeyboardMapRequest::Commit {
                token: token.into(),
                keymap_sha256: digest.clone(),
            })
            .unwrap();
        assert_eq!(
            serde_json::to_value(committed).unwrap()["state"],
            "committed"
        );
        assert_eq!(state.keymap.digest, digest);
        assert!(
            state
                .keymap
                .state
                .mod_name_is_active(xkb::MOD_NAME_ISO_LEVEL3_SHIFT, xkb::STATE_MODS_EFFECTIVE)
        );
        assert_eq!(state.keymap.state.key_get_utf8(xkb::Keycode::new(24)), "@");
        assert_eq!(state.pressed_keys, BTreeSet::from([16, 100]));
        state.keyboard_key(16, false, 0);
        state.keyboard_key(100, false, 0);

        // A repeat from the still-held old-map key and its eventual release
        // cannot leak into the new-map stream.
        state.keyboard_key(14, true, 0);
        assert!(state.pressed_keys.is_empty());
        state.keyboard_key(14, false, 0);
        assert!(!state.suppressed_keys.contains(&14));

        let retry = state
            .keyboard_map_request(KeyboardMapRequest::Commit {
                token: token.into(),
                keymap_sha256: digest,
            })
            .unwrap();
        assert_eq!(serde_json::to_value(retry).unwrap()["state"], "committed");
        let status = state
            .keyboard_map_request(KeyboardMapRequest::Status {
                token: token.into(),
            })
            .unwrap();
        assert_eq!(serde_json::to_value(status).unwrap()["state"], "committed");
    }

    #[test]
    fn keyboard_abort_preserves_the_old_map_and_is_reconcilable() {
        let token = "fedcba9876543210fedcba9876543210";
        let mut state = test_state();
        state.keyboard_focused = true;
        state.keyboard_entered = true;
        let old_digest = state.keymap.digest.clone();
        let prepare = german_prepare(token);
        let digest = match &prepare {
            KeyboardMapRequest::Prepare { keymap_sha256, .. } => keymap_sha256.clone(),
            _ => unreachable!(),
        };
        state.keyboard_map_request(prepare).unwrap();
        // Shift then A are queued without being interpreted by the pending
        // German map. Abort replays them under the old US map.
        state.keyboard_key(42, true, 0);
        state.keyboard_key(30, true, 0);
        let aborted = state
            .keyboard_map_request(KeyboardMapRequest::Abort {
                token: token.into(),
                keymap_sha256: digest.clone(),
            })
            .unwrap();
        assert_eq!(serde_json::to_value(aborted).unwrap()["state"], "aborted");
        assert_eq!(state.keymap.digest, old_digest);
        assert!(
            state
                .keymap
                .state
                .mod_name_is_active(xkb::MOD_NAME_SHIFT, xkb::STATE_MODS_EFFECTIVE)
        );
        assert_eq!(state.keymap.state.key_get_utf8(xkb::Keycode::new(38)), "A");
        state.keyboard_key(30, false, 0);
        state.keyboard_key(42, false, 0);
        let retry = state
            .keyboard_map_request(KeyboardMapRequest::Abort {
                token: token.into(),
                keymap_sha256: digest,
            })
            .unwrap();
        assert_eq!(serde_json::to_value(retry).unwrap()["state"], "aborted");
    }

    #[test]
    fn keyboard_focus_loss_discards_queued_input_without_stuck_keys() {
        let token = "11111111111111111111111111111111";
        let mut state = test_state();
        state.keyboard_focused = true;
        state.keyboard_entered = true;
        let prepare = german_prepare(token);
        let digest = match &prepare {
            KeyboardMapRequest::Prepare { keymap_sha256, .. } => keymap_sha256.clone(),
            _ => unreachable!(),
        };
        state.keyboard_map_request(prepare).unwrap();
        state.keyboard_key(30, true, 0);
        state.keyboard_leave();
        assert!(!state.keyboard_focused);
        assert!(state.suppressed_keys.contains(&30));
        state
            .keyboard_map_request(KeyboardMapRequest::Commit {
                token: token.into(),
                keymap_sha256: digest,
            })
            .unwrap();
        assert!(state.pressed_keys.is_empty());
        state.keyboard_key(30, false, 0);
        assert!(!state.suppressed_keys.contains(&30));
    }

    #[test]
    fn keyboard_queue_overflow_fails_closed_until_abort() {
        let token = "22222222222222222222222222222222";
        let mut state = test_state();
        state.keyboard_focused = true;
        state.keyboard_entered = true;
        let prepare = german_prepare(token);
        let digest = match &prepare {
            KeyboardMapRequest::Prepare { keymap_sha256, .. } => keymap_sha256.clone(),
            _ => unreachable!(),
        };
        state.keyboard_map_request(prepare).unwrap();
        for _ in 0..(MAX_PENDING_KEY_EVENTS / 2) {
            state.keyboard_key(30, true, 0);
            state.keyboard_key(30, false, 0);
        }
        state.keyboard_key(31, true, 0);
        let status = serde_json::to_value(
            state
                .keyboard_map_request(KeyboardMapRequest::Status {
                    token: token.into(),
                })
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status["state"], "prepared");
        assert_eq!(status["pending_token"], token);
        assert_eq!(status["pending_keymap_sha256"], digest);
        let commit = state.keyboard_map_request(KeyboardMapRequest::Commit {
            token: token.into(),
            keymap_sha256: digest.clone(),
        });
        assert_eq!(commit.unwrap_err().code, "input_overflow");
        state
            .keyboard_map_request(KeyboardMapRequest::Abort {
                token: token.into(),
                keymap_sha256: digest,
            })
            .unwrap();
        assert!(state.pressed_keys.is_empty());
        assert!(state.suppressed_keys.contains(&31));
        state.keyboard_key(31, false, 0);
        assert!(!state.suppressed_keys.contains(&31));
    }

    #[test]
    fn keyboard_disconnect_neutralizes_and_discards_the_dead_focus_epoch() {
        let token = "33333333333333333333333333333333";
        let mut state = test_state();
        state.keyboard_focused = true;
        state.keyboard_entered = true;
        state.pressed_keys.insert(42);
        state
            .keymap
            .state
            .update_key(xkb::Keycode::new(50), xkb::KeyDirection::Down);
        assert_ne!(
            state.keymap.state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            0
        );

        let prepare = german_prepare(token);
        state.keyboard_map_request(prepare).unwrap();
        // Shift was neutralized at Prepare; A belongs to the now-dead
        // replacement focus epoch and must not replay after reconnect.
        state.keyboard_key(30, true, 0);
        state.drop_client_leases();

        assert!(!state.keyboard_focused);
        assert!(!state.keyboard_entered);
        assert!(state.pressed_keys.is_empty());
        assert_eq!(
            state.keymap.state.serialize_mods(xkb::STATE_MODS_EFFECTIVE),
            0
        );
        assert!(state.suppressed_keys.contains(&42));
        assert!(state.suppressed_keys.contains(&30));
        let pending = state.pending_keymap.as_ref().unwrap();
        assert!(pending.input_queue.is_empty());
        assert!(pending.queued_pressed.is_empty());

        state.keyboard_key(30, false, 0);
        state.keyboard_key(42, false, 0);
        assert!(state.suppressed_keys.is_empty());
    }
}
