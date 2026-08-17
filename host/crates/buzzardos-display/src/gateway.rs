// SPDX-License-Identifier: AGPL-3.0-or-later

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::keyboard::{
    KeyboardMapFailure, KeyboardMapMethod, KeyboardMapReply, KeyboardMapRequest,
    KeyboardMapResponse, parse_request as parse_keyboard_map_request,
};
use crate::launch::Launch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostCommand {
    Minimize,
    Maximize,
    Restore,
    FocusMonitor,
    ToggleMaximize,
    Close,
    Start,
    Stop,
    Restart,
    ShutDown,
    OpenSettings,
    OpenDiagnostics,
    CaptureUi,
}

#[derive(Debug)]
pub(crate) enum GatewayEvent {
    HostCommand(HostCommand),
    GuestConnected,
    GuestDisconnected,
    GuestFailed(String),
    GuestFrame(DmabufFrame),
    GuestCursor(CursorImage),
    GuestCursorFallback,
    GuestCursorHidden,
    FrameReleased {
        id: u64,
        held_us: u64,
    },
    GuestScaleRequest {
        request: GuestScaleRequest,
        reply: SyncSender<GuestScaleReply>,
    },
}

#[derive(Debug)]
pub(crate) struct CursorImage {
    /// Non-zero only for a dmabuf cursor whose guest buffer remains leased
    /// until GTK releases the imported texture.
    pub(crate) id: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) hotspot_x: i32,
    pub(crate) hotspot_y: i32,
    pub(crate) storage: CursorStorage,
}

#[derive(Debug)]
pub(crate) enum CursorStorage {
    Shm {
        stride: usize,
        /// Premultiplied BGRA8 pixels, matching wl_shm ARGB8888 on
        /// little-endian Linux and GDK_MEMORY_B8G8R8A8_PREMULTIPLIED.
        pixels: Vec<u8>,
    },
    Dmabuf {
        fourcc: u32,
        modifier: u64,
        planes: Vec<DmabufPlane>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DmabufFormat {
    pub(crate) fourcc: u32,
    pub(crate) modifier: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputMode {
    pub(crate) logical_width: u32,
    pub(crate) logical_height: u32,
    pub(crate) physical_width: u32,
    pub(crate) physical_height: u32,
    pub(crate) host_surface_scale_120: u32,
    pub(crate) guest_ui_scale_120: u32,
    pub(crate) geometry_generation: u64,
    pub(crate) refresh_mhz: u32,
}

impl OutputMode {
    pub(crate) fn geometry(self) -> DisplayGeometry {
        DisplayGeometry {
            physical_width: self.physical_width,
            physical_height: self.physical_height,
            host_surface_scale_120: self.host_surface_scale_120,
            guest_ui_scale_120: self.guest_ui_scale_120,
            logical_width: self.logical_width,
            logical_height: self.logical_height,
            geometry_generation: self.geometry_generation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum GuestScalePreset {
    #[serde(rename = "automatic")]
    Automatic,
    #[serde(rename = "100")]
    Percent100,
    #[serde(rename = "125")]
    Percent125,
    #[serde(rename = "150")]
    Percent150,
    #[serde(rename = "175")]
    Percent175,
    #[serde(rename = "200")]
    Percent200,
}

impl GuestScalePreset {
    pub(crate) fn from_scale_120(scale_120: Option<u32>) -> Option<Self> {
        match scale_120 {
            None => Some(Self::Automatic),
            Some(120) => Some(Self::Percent100),
            Some(150) => Some(Self::Percent125),
            Some(180) => Some(Self::Percent150),
            Some(210) => Some(Self::Percent175),
            Some(240) => Some(Self::Percent200),
            Some(_) => None,
        }
    }

    pub(crate) fn resolve(self, host_surface_scale_120: u32) -> u32 {
        match self {
            Self::Automatic => host_surface_scale_120,
            Self::Percent100 => 120,
            Self::Percent125 => 150,
            Self::Percent150 => 180,
            Self::Percent175 => 210,
            Self::Percent200 => 240,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DisplayGeometry {
    pub(crate) physical_width: u32,
    pub(crate) physical_height: u32,
    pub(crate) host_surface_scale_120: u32,
    pub(crate) guest_ui_scale_120: u32,
    pub(crate) logical_width: u32,
    pub(crate) logical_height: u32,
    pub(crate) geometry_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestScaleRequest {
    schema: u32,
    method: GuestScaleMethod,
    pub(crate) preset: GuestScalePreset,
    pub(crate) current_geometry_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
enum GuestScaleMethod {
    SetGuestScale,
}

#[derive(Debug)]
pub(crate) enum GuestScaleReply {
    Applied {
        preset: GuestScalePreset,
        geometry: DisplayGeometry,
    },
    Rejected {
        code: &'static str,
        message: String,
        current_geometry: DisplayGeometry,
    },
}

#[derive(serde::Serialize)]
struct GuestScaleResponse {
    schema: u32,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    preset: Option<GuestScalePreset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    geometry: Option<DisplayGeometry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<GuestScaleError>,
}

#[derive(serde::Serialize)]
struct GuestScaleError {
    code: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_geometry: Option<DisplayGeometry>,
}

#[derive(Debug)]
pub(crate) struct DmabufPlane {
    pub(crate) fd: OwnedFd,
    pub(crate) offset: u32,
    pub(crate) stride: u32,
}

#[derive(Debug)]
pub(crate) struct DmabufFrame {
    pub(crate) id: u64,
    pub(crate) geometry_generation: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) fourcc: u32,
    pub(crate) modifier: u64,
    pub(crate) planes: Vec<DmabufPlane>,
    pub(crate) submitted_monotonic_us: u64,
    pub(crate) explicit_sync: bool,
    pub(crate) acquire_wait_us: u64,
}

#[derive(Debug)]
pub(crate) enum GatewayCommand {
    Configure {
        formats: Vec<DmabufFormat>,
        mode: OutputMode,
    },
    SetOutputMode(OutputMode),
    ReleaseFrame {
        id: u64,
        released_monotonic_us: u64,
    },
    ReleaseCursor {
        id: u64,
    },
    FramePainted {
        id: u64,
        frame_time_us: i64,
    },
    FramePresented {
        id: u64,
        presentation_time_us: i64,
        refresh_interval_us: i64,
        sequence: u64,
        offloaded: bool,
    },
    /// One tick of the native host window's actual GDK/Wayland frame clock.
    ///
    /// This is pacing for the nested compositor's parent Wayland output, not
    /// a video-streaming timer. It lets wl_surface.frame requests made without
    /// a new buffer wake on the host compositor's vblank-driven clock.
    FrameTick {
        frame_time_us: i64,
    },
    PointerEnter {
        x: f64,
        y: f64,
        geometry_generation: u64,
    },
    PointerLeave,
    PointerMotion {
        x: f64,
        y: f64,
        geometry_generation: u64,
    },
    PointerButton {
        button: u32,
        pressed: bool,
        geometry_generation: u64,
    },
    PointerAxis {
        horizontal: f64,
        vertical: f64,
        geometry_generation: u64,
    },
    KeyboardEnter,
    KeyboardLeave,
    KeyboardKey {
        key: u32,
        pressed: bool,
        modifiers: u32,
    },
    KeyboardMap {
        request: KeyboardMapRequest,
        reply: SyncSender<KeyboardMapReply>,
    },
}

#[derive(Clone)]
pub(crate) struct GatewayCommandSender {
    sender: Sender<GatewayCommand>,
    wake: Arc<UnixStream>,
}

impl GatewayCommandSender {
    pub(crate) fn send(&self, command: GatewayCommand) -> Result<()> {
        self.sender
            .send(command)
            .context("guest display server stopped")?;
        notify(&self.wake).context("waking guest display server")
    }
}

pub(crate) struct GatewayConnection {
    pub(crate) events: Receiver<GatewayEvent>,
    pub(crate) event_notify: UnixStream,
    pub(crate) commands: GatewayCommandSender,
}

#[derive(Clone, Debug)]
pub(super) struct EventSender {
    pub(super) sender: Sender<GatewayEvent>,
    pub(super) wake: Arc<UnixStream>,
}

impl EventSender {
    pub(super) fn send(&self, event: GatewayEvent) -> Result<()> {
        self.sender
            .send(event)
            .context("native host application stopped")?;
        notify(&self.wake).context("waking native host application")
    }
}

/// Owns both private Unix socket paths. This object does not own the host
/// Wayland connection and cannot create or mutate host xdg-shell objects.
pub(crate) struct GatewaySockets {
    listen_path: PathBuf,
    control_path: PathBuf,
    guest_scale_control_path: PathBuf,
    _guest_thread: JoinHandle<()>,
    _control_thread: JoinHandle<()>,
    _guest_scale_control_thread: JoinHandle<()>,
}

impl GatewaySockets {
    pub(crate) fn bind(launch: &Launch) -> Result<(Self, GatewayConnection)> {
        remove_stale_socket(&launch.listen)?;
        remove_stale_socket(&launch.control)?;
        remove_stale_socket(&launch.guest_scale_control)?;

        let guest = bind_private(&launch.listen, "guest display")?;
        let control = match bind_private(&launch.control, "host control") {
            Ok(listener) => listener,
            Err(error) => {
                let _ = fs::remove_file(&launch.listen);
                return Err(error);
            }
        };
        let guest_scale_control =
            match bind_private(&launch.guest_scale_control, "guest display-scale control") {
                Ok(listener) => listener,
                Err(error) => {
                    let _ = fs::remove_file(&launch.listen);
                    let _ = fs::remove_file(&launch.control);
                    return Err(error);
                }
            };
        let (event_read, event_write) =
            UnixStream::pair().context("creating native event notifier")?;
        event_read
            .set_nonblocking(true)
            .context("making native event notifier nonblocking")?;
        event_write
            .set_nonblocking(true)
            .context("making native event wakeup nonblocking")?;
        let (events_tx, events_rx) = mpsc::channel();
        let events = EventSender {
            sender: events_tx,
            wake: Arc::new(event_write),
        };

        let (command_read, command_write) =
            UnixStream::pair().context("creating guest command notifier")?;
        command_read
            .set_nonblocking(true)
            .context("making guest command notifier nonblocking")?;
        command_write
            .set_nonblocking(true)
            .context("making guest command wakeup nonblocking")?;
        let (commands_tx, commands_rx) = mpsc::channel();
        let command_sender = GatewayCommandSender {
            sender: commands_tx,
            wake: Arc::new(command_write),
        };

        let guest_events = events.clone();
        let sync_drm_device = launch.sync_drm_device.clone();
        let xkb_config_root = launch.xkb_config_root.clone();
        let guest_thread = thread::Builder::new()
            .name("buzzardos-guest-display".into())
            .spawn(move || {
                accept_guest(
                    guest,
                    guest_events,
                    commands_rx,
                    command_read,
                    sync_drm_device,
                    xkb_config_root,
                )
            })
            .context("starting guest display server thread")?;
        let control_events = events.clone();
        let control_thread = thread::Builder::new()
            .name("buzzardos-host-control".into())
            .spawn(move || accept_controls(control, control_events))
            .context("starting host control thread")?;
        let guest_commands = command_sender.clone();
        let guest_scale_control_thread = thread::Builder::new()
            .name("buzzardos-guest-scale-control".into())
            .spawn(move || accept_guest_scale_controls(guest_scale_control, events, guest_commands))
            .context("starting guest display-scale control thread")?;

        Ok((
            Self {
                listen_path: launch.listen.clone(),
                control_path: launch.control.clone(),
                guest_scale_control_path: launch.guest_scale_control.clone(),
                _guest_thread: guest_thread,
                _control_thread: control_thread,
                _guest_scale_control_thread: guest_scale_control_thread,
            },
            GatewayConnection {
                events: events_rx,
                event_notify: event_read,
                commands: command_sender,
            },
        ))
    }
}

impl Drop for GatewaySockets {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.listen_path);
        let _ = fs::remove_file(&self.control_path);
        let _ = fs::remove_file(&self.guest_scale_control_path);
    }
}

fn bind_private(path: &PathBuf, description: &str) -> Result<UnixListener> {
    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding {description} {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("securing {description} {}", path.display()))?;
    Ok(listener)
}

fn remove_stale_socket(path: &PathBuf) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() && !metadata.file_type().is_symlink() => {
            fs::remove_file(path)
                .with_context(|| format!("removing stale socket {}", path.display()))
        }
        Ok(_) => bail!("refusing to replace non-socket path {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn accept_controls(listener: UnixListener, events: EventSender) {
    for connection in listener.incoming() {
        let result = connection
            .context("accepting host control connection")
            .and_then(|mut connection| handle_control(&mut connection, &events));
        if let Err(error) = result {
            eprintln!("buzzardos-display: host control: {error:#}");
        }
    }
}

fn accept_guest_scale_controls(
    listener: UnixListener,
    events: EventSender,
    commands: GatewayCommandSender,
) {
    for connection in listener.incoming() {
        let mut connection = match connection {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("buzzardos-display: accepting guest display-scale request: {error}");
                continue;
            }
        };
        let response = handle_guest_display_control(&mut connection, &events, &commands)
            .unwrap_or_else(|error| GuestDisplayControlResponse::KeyboardError {
                schema: 1,
                ok: false,
                method: None,
                error: GuestKeyboardError {
                    code: "invalid_request".into(),
                    message: format!("{error:#}"),
                },
            });
        if let Ok(bytes) = serde_json::to_vec(&response) {
            let _ = connection.write_all(&bytes);
            let _ = connection.write_all(b"\n");
        }
    }
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum GuestDisplayControlResponse {
    Scale(GuestScaleResponse),
    Keyboard(KeyboardMapResponse),
    KeyboardError {
        schema: u32,
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        method: Option<KeyboardMapMethod>,
        error: GuestKeyboardError,
    },
}

#[derive(serde::Serialize)]
struct GuestKeyboardError {
    code: String,
    message: String,
}

fn handle_guest_display_control(
    connection: &mut UnixStream,
    events: &EventSender,
    commands: &GatewayCommandSender,
) -> Result<GuestDisplayControlResponse> {
    const MAX_REQUEST_BYTES: u64 = 4096;
    connection
        .set_read_timeout(Some(Duration::from_secs(2)))
        .context("setting guest display control timeout")?;
    let mut request = String::new();
    BufReader::new(&mut *connection)
        .take(MAX_REQUEST_BYTES + 1)
        .read_line(&mut request)
        .context("reading guest display control request")?;
    if request.len() as u64 > MAX_REQUEST_BYTES {
        bail!("guest display control request exceeds {MAX_REQUEST_BYTES} bytes");
    }
    if !request.ends_with('\n') {
        bail!("guest display control request is not newline terminated");
    }
    let value: serde_json::Value = serde_json::from_str(request.trim_end())
        .context("parsing guest display control request")?;
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .context("guest display control request has no method")?;
    if method == "SetGuestScale" {
        let request: GuestScaleRequest =
            serde_json::from_value(value).context("parsing guest display-scale request")?;
        return handle_guest_scale_request(request, events).map(GuestDisplayControlResponse::Scale);
    }
    let method = KeyboardMapMethod::parse(method);
    let request = match parse_keyboard_map_request(value) {
        Ok(request) => request,
        Err(error) => {
            return Ok(GuestDisplayControlResponse::KeyboardError {
                schema: 1,
                ok: false,
                method,
                error: GuestKeyboardError {
                    code: "invalid_request".into(),
                    message: format!("{error:#}"),
                },
            });
        }
    };
    let request_method = request.method();
    let (reply, receiver) = mpsc::sync_channel(1);
    commands.send(GatewayCommand::KeyboardMap { request, reply })?;
    let response = receiver
        .recv_timeout(Duration::from_secs(5))
        .context("guest display owner did not answer keyboard-map request")?;
    match response {
        Ok(response) => Ok(GuestDisplayControlResponse::Keyboard(response)),
        Err(KeyboardMapFailure { code, message }) => {
            Ok(GuestDisplayControlResponse::KeyboardError {
                schema: 1,
                ok: false,
                method: Some(request_method),
                error: GuestKeyboardError {
                    code: code.into(),
                    message,
                },
            })
        }
    }
}

#[cfg(test)]
fn handle_guest_scale_control(
    connection: &mut UnixStream,
    events: &EventSender,
) -> Result<GuestScaleResponse> {
    const MAX_REQUEST_BYTES: u64 = 4096;
    connection
        .set_read_timeout(Some(Duration::from_secs(2)))
        .context("setting guest display-scale control timeout")?;
    let mut request = String::new();
    BufReader::new(&mut *connection)
        .take(MAX_REQUEST_BYTES + 1)
        .read_line(&mut request)
        .context("reading guest display-scale request")?;
    if request.len() as u64 > MAX_REQUEST_BYTES {
        bail!("guest display-scale request exceeds {MAX_REQUEST_BYTES} bytes");
    }
    let request: GuestScaleRequest =
        serde_json::from_str(request.trim()).context("parsing guest display-scale request")?;
    if request.schema != 1 {
        bail!(
            "unsupported guest display-scale request schema {}",
            request.schema
        );
    }
    let (reply, receiver) = mpsc::sync_channel(1);
    events.send(GatewayEvent::GuestScaleRequest { request, reply })?;
    match receiver
        .recv_timeout(Duration::from_secs(5))
        .context("native display did not answer guest display-scale request")?
    {
        GuestScaleReply::Applied { preset, geometry } => Ok(GuestScaleResponse {
            schema: 1,
            ok: true,
            preset: Some(preset),
            geometry: Some(geometry),
            error: None,
        }),
        GuestScaleReply::Rejected {
            code,
            message,
            current_geometry,
        } => Ok(GuestScaleResponse {
            schema: 1,
            ok: false,
            preset: None,
            geometry: None,
            error: Some(GuestScaleError {
                code: code.into(),
                message,
                current_geometry: Some(current_geometry),
            }),
        }),
    }
}

fn handle_guest_scale_request(
    request: GuestScaleRequest,
    events: &EventSender,
) -> Result<GuestScaleResponse> {
    if request.schema != 1 {
        bail!(
            "unsupported guest display-scale request schema {}",
            request.schema
        );
    }
    let (reply, receiver) = mpsc::sync_channel(1);
    events.send(GatewayEvent::GuestScaleRequest { request, reply })?;
    match receiver
        .recv_timeout(Duration::from_secs(5))
        .context("native display did not answer guest display-scale request")?
    {
        GuestScaleReply::Applied { preset, geometry } => Ok(GuestScaleResponse {
            schema: 1,
            ok: true,
            preset: Some(preset),
            geometry: Some(geometry),
            error: None,
        }),
        GuestScaleReply::Rejected {
            code,
            message,
            current_geometry,
        } => Ok(GuestScaleResponse {
            schema: 1,
            ok: false,
            preset: None,
            geometry: None,
            error: Some(GuestScaleError {
                code: code.into(),
                message,
                current_geometry: Some(current_geometry),
            }),
        }),
    }
}

fn handle_control(connection: &mut UnixStream, events: &EventSender) -> Result<()> {
    connection
        .set_read_timeout(Some(Duration::from_secs(2)))
        .context("setting host control timeout")?;
    let mut command = String::new();
    BufReader::new(&mut *connection)
        .take(65)
        .read_line(&mut command)
        .context("reading host control command")?;
    let command = parse_window_command(command.trim())?;
    events.send(GatewayEvent::HostCommand(command))?;
    connection
        .write_all(b"ok\n")
        .context("confirming host control command")
}

fn parse_window_command(command: &str) -> Result<HostCommand> {
    match command {
        "minimize" => Ok(HostCommand::Minimize),
        "maximize" => Ok(HostCommand::Maximize),
        "restore" => Ok(HostCommand::Restore),
        "focus-monitor" => Ok(HostCommand::FocusMonitor),
        "toggle-maximize" => Ok(HostCommand::ToggleMaximize),
        "close" => Ok(HostCommand::Close),
        "start" => Ok(HostCommand::Start),
        "stop" => Ok(HostCommand::Stop),
        "restart" => Ok(HostCommand::Restart),
        "shutdown" => Ok(HostCommand::ShutDown),
        "settings" => Ok(HostCommand::OpenSettings),
        "diagnostics" => Ok(HostCommand::OpenDiagnostics),
        "capture-ui" => Ok(HostCommand::CaptureUi),
        _ => bail!("unsupported host window command '{command}'"),
    }
}

fn accept_guest(
    listener: UnixListener,
    events: EventSender,
    commands: Receiver<GatewayCommand>,
    command_notify: UnixStream,
    sync_drm_device: Option<PathBuf>,
    xkb_config_root: PathBuf,
) {
    if let Err(error) = crate::guest_display::run(
        listener,
        events.clone(),
        commands,
        command_notify,
        sync_drm_device,
        xkb_config_root,
    ) {
        let _ = events.send(GatewayEvent::GuestFailed(format!("{error:#}")));
    }
}

fn notify(stream: &UnixStream) -> std::io::Result<()> {
    let mut stream = stream;
    match stream.write(&[1]) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_protocol_is_closed() {
        assert_eq!(
            parse_window_command("toggle-maximize").unwrap(),
            HostCommand::ToggleMaximize
        );
        assert_eq!(
            parse_window_command("focus-monitor").unwrap(),
            HostCommand::FocusMonitor
        );
        assert!(parse_window_command("run arbitrary command").is_err());
    }

    #[test]
    fn guest_scale_thread_round_trips_through_the_native_event_loop() {
        let (event_read, event_write) = UnixStream::pair().unwrap();
        let (sender, receiver) = mpsc::channel();
        let events = EventSender {
            sender,
            wake: Arc::new(event_write),
        };
        let (mut server, mut client) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || handle_guest_scale_control(&mut server, &events));

        client
            .write_all(
                br#"{"schema":1,"method":"SetGuestScale","preset":"125","current_geometry_generation":7}
"#,
            )
            .unwrap();
        let mut wake = [0_u8; 1];
        (&event_read).read_exact(&mut wake).unwrap();
        let GatewayEvent::GuestScaleRequest { request, reply } = receiver.recv().unwrap() else {
            panic!("native event loop received the wrong event");
        };
        assert_eq!(request.preset, GuestScalePreset::Percent125);
        assert_eq!(request.current_geometry_generation, 7);
        let geometry = DisplayGeometry {
            physical_width: 1707,
            physical_height: 1067,
            host_surface_scale_120: 160,
            guest_ui_scale_120: 150,
            logical_width: 1365,
            logical_height: 853,
            geometry_generation: 8,
        };
        reply
            .send(GuestScaleReply::Applied {
                preset: GuestScalePreset::Percent125,
                geometry,
            })
            .unwrap();
        let response = worker.join().unwrap().unwrap();
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["preset"], "125");
        assert_eq!(value["geometry"]["geometry_generation"], 8);
        assert!(value.get("error").is_none());
    }

    #[test]
    fn guest_scale_request_rejects_unknown_fields_and_commands() {
        assert!(
            serde_json::from_str::<GuestScaleRequest>(
                r#"{"schema":1,"method":"RunCommand","preset":"100","current_geometry_generation":1}"#,
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<GuestScaleRequest>(
                r#"{"schema":1,"method":"SetGuestScale","preset":"100","current_geometry_generation":1,"command":"swaymsg"}"#,
            )
            .is_err()
        );
    }
}
