// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;
use wb_core::{
    GUEST_AUDIO_PORT, HOST_CAMERA_PORT, HOST_MICROPHONE_PORT, MachineConfig, Podman,
    PodmanDefinition, PodmanRuntimePaths, ResourceLocator, WaylandCapabilities,
    host_control_socket,
};


pub(crate) struct PreparedDisplay {
    pub(crate) session_token: String,
}

pub(crate) fn prepare_and_launch(
    resources: &ResourceLocator,
    machine_dir: &Path,
    config: &MachineConfig,
    runtime: &PodmanRuntimePaths,
) -> Result<PreparedDisplay> {
    runtime.prepare()?;
    let existing_window = send_control(machine_dir, "focus-monitor").is_ok();
    prepare_runtime_files(config, runtime, !existing_window)?;

    let host_wayland = host_wayland_socket()?;
    let capabilities = WaylandCapabilities::probe(&host_wayland)?;
    if !capabilities.linux_dmabuf || capabilities.linux_dmabuf_version < 4 {
        bail!(
            "the host Wayland compositor does not advertise linux-dmabuf version 4 required by the Buzzard OS display"
        );
    }
    if capabilities.dmabuf_main_device.is_none() {
        bail!("the host Wayland compositor supplied no linux-dmabuf main device");
    }

    if existing_window {
        return Ok(PreparedDisplay {
            session_token: read_session_token(runtime)?,
        });
    }

    let control = host_control_socket(machine_dir)?;
    if let Some(parent) = control.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating display control directory {}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!("protecting display control directory {}", parent.display())
        })?;
    }

    let display = resources.helper_or_path("buzzardos-display")?;
    let xkb = xkb_root(resources)?;
    let listen = runtime.host_exchange.join("wayland-0");
    let scale = runtime.host_exchange.join("display-scale-host.sock");
    let clipboard = runtime.host_exchange.join("clipboard-agent.sock");
    let mut command = Command::new(&display);
    command
        .arg("--host")
        .arg(&host_wayland)
        .arg("--listen")
        .arg(&listen)
        .arg("--control")
        .arg(&control)
        .arg("--guest-scale-control")
        .arg(&scale)
        .arg("--guest-clipboard-control")
        .arg(&clipboard)
        .arg("--xkb-config-root")
        .arg(&xkb)
        .arg("--machine-dir")
        .arg(machine_dir)
        .arg("--status-dir")
        .arg(&runtime.host_status)
        .arg("--output-state-dir")
        .arg(&runtime.display_state)
        .arg("--initial-width")
        .arg(config.width.to_string())
        .arg("--initial-height")
        .arg(config.height.to_string())
        .arg("--dmabuf-version")
        .arg("4")
        .arg("--title")
        .arg(&config.name)
        .arg("--app-id")
        .arg(wb_core::host_identity().application_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(scale_120) = config.guest_scale_120 {
        command.arg("--guest-scale-120").arg(scale_120.to_string());
    }
    if let Some(render_node) = render_node_for_device(capabilities.dmabuf_main_device) {
        command.arg("--sync-drm-device").arg(render_node);
    }
    // The native window outlives the short launcher invocation that started
    // or focused the persistent Podman machine. Give it an independent
    // process group so a terminal, manager worker, or automation harness
    // completing the launcher command cannot deliver its job-control signal
    // to the window. The window continues to own orderly container shutdown
    // through its native lifecycle controls.
    command.process_group(0);

    let mut child = command
        .spawn()
        .with_context(|| format!("starting native machine window with {}", display.display()))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if socket_exists(&listen) && socket_exists(&scale) && socket_exists(&control) {
            return Ok(PreparedDisplay {
                session_token: read_session_token(runtime)?,
            });
        }
        if let Some(status) = child
            .try_wait()
            .context("checking native machine-window startup")?
        {
            bail!("native machine window exited during startup with {status}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("native machine window did not create its private endpoints");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn prepare_runtime_files(
    config: &MachineConfig,
    runtime: &PodmanRuntimePaths,
    remove_display_endpoints: bool,
) -> Result<()> {
    for name in ["desktop-ready", "clipboard-ready", "desktop-stopped"] {
        remove_ephemeral(&runtime.host_exchange.join(name))?;
    }
    if remove_display_endpoints {
        for name in ["wayland-0", "display-scale-host.sock"] {
            remove_ephemeral(&runtime.host_exchange.join(name))?;
        }
    }
    for name in ["window.json", "presentation.json", "media-worker.json"] {
        remove_ephemeral(&runtime.host_status.join(name))?;
    }
    remove_ephemeral(&runtime.host_status.join("media-endpoints.json"))?;

    write_runtime_file(
        &runtime.host_exchange.join("initial-output.conf"),
        format!("output * mode {}x{}\n", config.width, config.height).as_bytes(),
    )?;
    let session_token = Uuid::new_v4().simple().to_string();
    let environment = format!(
        "BUZZARDOS_SESSION_TOKEN={session_token}\n\
         BUZZARDOS_MACHINE_ID={}\n\
         BUZZARDOS_MACHINE_NAME={}\n\
         BUZZARDOS_WINDOW_TITLE=Buzzard OS — {}\n\
         BUZZARDOS_WINDOW_APP_ID={}\n",
        config.id, config.name, config.name, wb_core::host_identity().application_id
    );
    write_runtime_file(
        &runtime.host_exchange.join("driver.env"),
        environment.as_bytes(),
    )?;
    let integration = serde_json::json!({
        "schema": 1,
        "generation": 1,
        "media": config.integrations.media,
    });
    write_runtime_file(
        &runtime.display_state.join("integration.json"),
        &serde_json::to_vec_pretty(&integration).context("serializing guest media intent")?,
    )?;
    Ok(())
}

pub(crate) fn publish_media_endpoints(
    podman: &Podman,
    definition: &PodmanDefinition,
    config: &MachineConfig,
    runtime: &PodmanRuntimePaths,
) -> Result<()> {
    let media = &config.integrations.media;
    let audio = media
        .guest_audio_output
        .then(|| mapped_host_port(podman, &definition.container_name, GUEST_AUDIO_PORT))
        .transpose()?;
    let microphone = media
        .host_microphone
        .then(|| mapped_host_port(podman, &definition.container_name, HOST_MICROPHONE_PORT))
        .transpose()?;
    let camera = media
        .host_camera
        .then(|| mapped_host_port(podman, &definition.container_name, HOST_CAMERA_PORT))
        .transpose()?;
    let endpoints = serde_json::json!({
        "schema": 1,
        "container": definition.container_name,
        "guest_audio_output": audio,
        "host_microphone": microphone,
        "host_camera": camera,
    });
    let path = runtime.host_status.join("media-endpoints.json");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&endpoints).context("serializing host media endpoints")?,
    )
    .with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protecting {}", path.display()))
}

fn mapped_host_port(podman: &Podman, container: &str, guest_port: u16) -> Result<u16> {
    let mapping = podman.port(container, guest_port, wb_core::PortProtocol::Tcp)?;
    mapping
        .lines()
        .find_map(|line| line.trim().rsplit_once(':').map(|(_, port)| port))
        .context("Podman returned no media port mapping")?
        .parse::<u16>()
        .with_context(|| format!("Podman returned an invalid media port mapping: {mapping}"))
}

fn write_runtime_file(path: &Path, contents: &[u8]) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting permissions on {}", path.display()))
}

fn remove_ephemeral(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_socket() => {
            fs::remove_file(path).with_context(|| format!("removing stale {}", path.display()))
        }
        Ok(_) => bail!(
            "refusing to replace unexpected runtime path {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn read_session_token(runtime: &PodmanRuntimePaths) -> Result<String> {
    let contents = fs::read_to_string(runtime.host_exchange.join("driver.env"))?;
    contents
        .lines()
        .find_map(|line| line.strip_prefix("BUZZARDOS_SESSION_TOKEN="))
        .map(ToOwned::to_owned)
        .context("runtime environment contains no session token")
}

fn host_wayland_socket() -> Result<PathBuf> {
    let display = std::env::var_os("WAYLAND_DISPLAY")
        .context("WAYLAND_DISPLAY is required to open a Buzzard OS machine window")?;
    let display = PathBuf::from(display);
    let socket = if display.is_absolute() {
        display
    } else {
        PathBuf::from(
            std::env::var_os("XDG_RUNTIME_DIR")
                .context("XDG_RUNTIME_DIR is required to resolve WAYLAND_DISPLAY")?,
        )
        .join(display)
    };
    let metadata = fs::symlink_metadata(&socket)
        .with_context(|| format!("inspecting host Wayland socket {}", socket.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
        bail!(
            "host Wayland endpoint {} is not a real socket",
            socket.display()
        );
    }
    socket
        .canonicalize()
        .with_context(|| format!("resolving host Wayland socket {}", socket.display()))
}

fn xkb_root(resources: &ResourceLocator) -> Result<PathBuf> {
    resources.asset_directory("xkb").or_else(|_| {
        Path::new("/usr/share/X11/xkb")
            .canonicalize()
            .context("resolving XKB data")
    })
}

fn socket_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        !metadata.file_type().is_symlink() && metadata.file_type().is_socket()
    })
}

pub(crate) fn send_control(machine_dir: &Path, action: &str) -> Result<()> {
    let socket = host_control_socket(machine_dir)?;
    let mut connection = UnixStream::connect(&socket)
        .with_context(|| format!("connecting to native window control {}", socket.display()))?;
    connection
        .write_all(format!("{action}\n").as_bytes())
        .context("sending native window action")?;
    let mut response = String::new();
    connection
        .read_to_string(&mut response)
        .context("reading native window response")?;
    if response.trim() == "ok" {
        Ok(())
    } else {
        bail!("native window rejected action: {}", response.trim())
    }
}

fn render_node_for_device(device: Option<u64>) -> Option<PathBuf> {
    let device = device?;
    fs::read_dir("/dev/dri")
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("renderD"))
                && fs::metadata(path).is_ok_and(|metadata| {
                    metadata.file_type().is_char_device()
                        && (metadata.rdev() == device
                            || drm_devices_share_backing_device(device, metadata.rdev()))
                })
        })
}

fn drm_devices_share_backing_device(first: u64, second: u64) -> bool {
    let backing = |device| {
        Path::new("/sys/dev/char")
            .join(format!(
                "{}:{}/device",
                libc::major(device),
                libc::minor(device)
            ))
            .canonicalize()
            .ok()
    };
    matches!((backing(first), backing(second)), (Some(first), Some(second)) if first == second)
}
