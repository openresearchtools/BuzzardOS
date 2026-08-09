// SPDX-License-Identifier: AGPL-3.0-or-later

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::launch::Launch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostCommand {
    Minimize,
    Maximize,
    Restore,
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
    FrameReleased { id: u64, held_us: u64 },
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
    pub(crate) scale_120: u32,
    pub(crate) refresh_mhz: u32,
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
    },
    PointerLeave,
    PointerMotion {
        x: f64,
        y: f64,
    },
    PointerButton {
        button: u32,
        pressed: bool,
    },
    PointerAxis {
        horizontal: f64,
        vertical: f64,
    },
    KeyboardEnter,
    KeyboardLeave,
    KeyboardKey {
        key: u32,
        pressed: bool,
        modifiers: u32,
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
    sender: Sender<GatewayEvent>,
    wake: Arc<UnixStream>,
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
    _guest_thread: JoinHandle<()>,
    _control_thread: JoinHandle<()>,
}

impl GatewaySockets {
    pub(crate) fn bind(launch: &Launch) -> Result<(Self, GatewayConnection)> {
        remove_stale_socket(&launch.listen)?;
        remove_stale_socket(&launch.control)?;

        let guest = bind_private(&launch.listen, "guest display")?;
        let control = match bind_private(&launch.control, "host control") {
            Ok(listener) => listener,
            Err(error) => {
                let _ = fs::remove_file(&launch.listen);
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
        let guest_thread = thread::Builder::new()
            .name("wildbuzzard-guest-display".into())
            .spawn(move || {
                accept_guest(
                    guest,
                    guest_events,
                    commands_rx,
                    command_read,
                    sync_drm_device,
                )
            })
            .context("starting guest display server thread")?;
        let control_thread = thread::Builder::new()
            .name("wildbuzzard-host-control".into())
            .spawn(move || accept_controls(control, events))
            .context("starting host control thread")?;

        Ok((
            Self {
                listen_path: launch.listen.clone(),
                control_path: launch.control.clone(),
                _guest_thread: guest_thread,
                _control_thread: control_thread,
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
            eprintln!("wildbuzzard-display: host control: {error:#}");
        }
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
) {
    if let Err(error) = crate::guest_display::run(
        listener,
        events.clone(),
        commands,
        command_notify,
        sync_drm_device,
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
        assert!(parse_window_command("run arbitrary command").is_err());
    }
}
