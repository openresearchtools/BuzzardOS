// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, Uid, setsid};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use wb_core::{
    DisplayDiagnostics, IdMap, MachineConfig, MachineState, NetworkMode, PresentationDiagnostics,
    ResourceLocator, RuntimeState, WaylandCapabilities, WindowDiagnostics, host_control_socket,
};

const GUEST_POWEROFF_MARKER: &str = "guest-poweroff-requested";

#[derive(Debug, Parser)]
#[command(name = "wildbuzzard-broker", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Run {
        #[arg(long)]
        machine_dir: PathBuf,
        #[arg(long)]
        shared: PathBuf,
        #[arg(long)]
        detach: bool,
    },
    #[command(name = "__cleanup-cgroup", hide = true)]
    CleanupCgroup {
        #[arg(long)]
        path: PathBuf,
    },
    #[command(name = "__private-network-sandbox", hide = true)]
    PrivateNetworkSandbox {
        #[arg(long)]
        bwrap: PathBuf,
        #[arg(long)]
        apparmor_access: Option<PathBuf>,
        #[arg(long)]
        cgroup_source: PathBuf,
        #[arg(long)]
        cgroup_staged: PathBuf,
        #[arg(last = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("wildbuzzard-broker: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Commands::Run {
            machine_dir,
            shared,
            detach,
        } => run_machine(&machine_dir, &shared, detach),
        Commands::CleanupCgroup { path } => cleanup_mapped_cgroup_children(&path),
        Commands::PrivateNetworkSandbox {
            bwrap,
            apparmor_access,
            cgroup_source,
            cgroup_staged,
            arguments,
        } => run_private_network_sandbox(
            &bwrap,
            apparmor_access.as_deref(),
            &cgroup_source,
            &cgroup_staged,
            &arguments,
        ),
    }
}

fn run_machine(machine_dir: &Path, shared: &Path, detach: bool) -> Result<()> {
    if detach {
        setsid().context("creating detached broker session")?;
    }

    let machine_dir = canonical_real_directory(machine_dir, "machine directory")?;
    let shared = canonical_real_directory(shared, "shared folder")?;
    let rootfs = canonical_real_directory(&machine_dir.join("rootfs"), "machine rootfs")?;
    let config = MachineConfig::load(&machine_dir)?;
    validate_portable_layout(&machine_dir, &rootfs, &shared, &config)?;
    validate_rootfs(&rootfs)?;
    let _machine_lock = lock_machine(&machine_dir)?;

    let wayland = host_wayland_socket()?;
    let resources = ResourceLocator::discover()?;
    let bwrap = resources.helper_or_path("bwrap")?;
    let unshare = resources.helper_or_path("unshare")?;

    let mut state = RuntimeState::new(MachineState::Starting);
    state.detail = Some("creating namespaces".into());
    state.save(&machine_dir)?;

    let result = launch_container(
        &bwrap,
        &unshare,
        &resources,
        &config,
        &machine_dir,
        &rootfs,
        &shared,
        &wayland,
        &mut state,
    );

    match result {
        Ok(status)
            if status.success()
                || RuntimeState::load(&machine_dir)?
                    .is_some_and(|state| state.state == MachineState::Stopping) =>
        {
            let shutdown_detail = RuntimeState::load(&machine_dir)?
                .filter(|state| state.state == MachineState::Stopping)
                .and_then(|state| state.detail)
                .unwrap_or_else(|| "clean shutdown".into());
            let mut stopped = RuntimeState::new(MachineState::Stopped);
            stopped.launcher_pid = None;
            stopped.detail = Some(shutdown_detail);
            stopped.save(&machine_dir)?;
            Ok(())
        }
        Ok(status) => {
            let mut failed = RuntimeState::new(MachineState::Failed);
            failed.launcher_pid = None;
            failed.detail = Some(format!("container exited with {status}"));
            failed.save(&machine_dir)?;
            bail!("container exited with {status}")
        }
        Err(error) => {
            let mut failed = RuntimeState::new(MachineState::Failed);
            failed.launcher_pid = None;
            failed.detail = Some(format!("{error:#}"));
            failed.save(&machine_dir)?;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn launch_container(
    bwrap: &Path,
    unshare: &Path,
    resources: &ResourceLocator,
    config: &MachineConfig,
    machine_dir: &Path,
    rootfs: &Path,
    shared: &Path,
    wayland: &Path,
    state: &mut RuntimeState,
) -> Result<ExitStatus> {
    let (block_read, mut block_write) = pipe().context("creating container start barrier")?;
    let (status_read, status_write) = pipe().context("creating container status pipe")?;
    let host_wayland =
        WaylandCapabilities::probe(wayland).context("probing host Wayland capabilities")?;
    let readiness = tempfile::Builder::new()
        .prefix("wildbuzzard-runtime-")
        .tempdir()
        .context("creating ephemeral compositor readiness directory")?;
    let (readiness_path, _readiness_guard) =
        if std::env::var_os("WILDBUZZARD_KEEP_RUNTIME").is_some() {
            let path = readiness.keep();
            eprintln!(
                "Wild Buzzard development runtime evidence will remain at {}",
                path.display()
            );
            (path, None)
        } else {
            (readiness.path().to_path_buf(), Some(readiness))
        };
    let guest_runtime = readiness_path.join("guest");
    let host_status = readiness_path.join("host-status");
    let display_state = readiness_path.join("display-state");
    fs::create_dir(&guest_runtime).context("creating guest runtime directory")?;
    fs::create_dir(&host_status).context("creating host display status directory")?;
    fs::create_dir(&display_state).context("creating display state directory")?;
    fs::set_permissions(&guest_runtime, fs::Permissions::from_mode(0o777))
        .context("setting guest runtime permissions")?;
    fs::set_permissions(&host_status, fs::Permissions::from_mode(0o700))
        .context("setting host display status permissions")?;
    fs::set_permissions(&display_state, fs::Permissions::from_mode(0o755))
        .context("setting display state permissions")?;
    let resolv_conf = guest_runtime.join("resolv.conf");
    let resolv_contents = match config.network {
        NetworkMode::User => {
            "# Wild Buzzard slirp4netns DNS\nnameserver 10.0.2.3\noptions edns0\n".to_owned()
        }
        NetworkMode::Host => {
            fs::read_to_string("/etc/resolv.conf").context("reading host resolver configuration")?
        }
        NetworkMode::None => "# Networking disabled by Wild Buzzard\n".to_owned(),
    };
    fs::write(&resolv_conf, resolv_contents)
        .with_context(|| format!("writing {}", resolv_conf.display()))?;
    fs::set_permissions(&resolv_conf, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting permissions on {}", resolv_conf.display()))?;
    // OCI base images commonly ship a build-time /etc/hostname (for example
    // "debuerreotype"). systemd reads that file during boot and would
    // otherwise overwrite the UTS hostname selected above. Keep hostname
    // state ephemeral and machine-specific instead of modifying the durable
    // rootfs on every launch.
    let hostname = guest_runtime.join("hostname");
    fs::write(&hostname, format!("{}\n", config.name))
        .with_context(|| format!("writing {}", hostname.display()))?;
    fs::set_permissions(&hostname, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting permissions on {}", hostname.display()))?;
    let initial_output = guest_runtime.join("initial-output.conf");
    fs::write(
        &initial_output,
        format!("output * mode {}x{}\n", config.width, config.height),
    )
    .with_context(|| format!("writing {}", initial_output.display()))?;
    fs::set_permissions(&initial_output, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting permissions on {}", initial_output.display()))?;
    let poweroff_drop_in = guest_runtime.join("wildbuzzard-desktop-poweroff-marker.conf");
    fs::write(
        &poweroff_drop_in,
        format!(
            "[Service]\nExecStop=+/usr/bin/touch /run/wildbuzzard-host/{GUEST_POWEROFF_MARKER}\n"
        ),
    )
    .with_context(|| format!("writing {}", poweroff_drop_in.display()))?;
    fs::set_permissions(&poweroff_drop_in, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting permissions on {}", poweroff_drop_in.display()))?;
    let nvidia = prepare_nvidia_injection(
        resources,
        &guest_runtime,
        config,
        host_wayland.dmabuf_main_device,
    )?;
    let mut service_environment = vec![
        ("WILDBUZZARD_MACHINE_ID".into(), config.id.to_string()),
        ("WILDBUZZARD_MACHINE_NAME".into(), config.name.clone()),
        (
            "WILDBUZZARD_WINDOW_TITLE".into(),
            format!("Wild Buzzard — {}", config.name),
        ),
        (
            "WILDBUZZARD_WINDOW_APP_ID".into(),
            "org.openresearchtools.wildbuzzard".into(),
        ),
        // Select a known stock wlroots renderer so diagnostics describe the
        // renderer actually requested by this launch rather than inferring it
        // from library availability.
        ("WLR_RENDERER".into(), "gles2".into()),
    ];
    if config.gpus == ["all"]
        && let Some(render_node) = render_node_for_device(host_wayland.dmabuf_main_device)
    {
        service_environment.push((
            "WLR_RENDER_DRM_DEVICE".into(),
            render_node.display().to_string(),
        ));
    }
    if let Some(injection) = &nvidia {
        service_environment.push((
            "WILDBUZZARD_NVIDIA_TOOLKIT_VERSION".into(),
            injection.toolkit_version.clone(),
        ));
        service_environment.push((
            "WILDBUZZARD_NVIDIA_CDI_DEVICES".into(),
            injection.cdi_devices.join(","),
        ));
        service_environment.extend(injection.environment.iter().cloned());
    }
    write_environment_file(&guest_runtime.join("driver.env"), &service_environment)?;
    let sync_drm_device = render_node_for_device(host_wayland.dmabuf_main_device);
    let mut display = start_display_gateway(
        resources,
        DisplayGatewayPaths {
            host_wayland: wayland,
            guest_runtime: &guest_runtime,
            host_status: &host_status,
            display_state: &display_state,
            machine_dir,
        },
        config,
        sync_drm_device.as_deref(),
    )?;
    let cgroup = MachineCgroup::create(config, unshare)?;
    let staged_cgroup = if matches!(config.network, NetworkMode::Host) {
        None
    } else {
        let path = host_status.join("cgroup");
        fs::create_dir(&path).context("creating private cgroup mountpoint")?;
        Some(path)
    };
    let id_map = IdMap::discover()?;
    let host_apparmor_access = Path::new("/sys/kernel/security/apparmor/.access");
    let apparmor_access_source =
        if !matches!(config.network, NetworkMode::Host) && host_apparmor_access.exists() {
            let staged = host_status.join("apparmor-access");
            File::create(&staged).context("creating private AppArmor access mountpoint")?;
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o600))
                .context("setting private AppArmor access mountpoint permissions")?;
            Some(staged)
        } else {
            None
        };
    let mut command = Command::new(unshare);
    command.env_clear();
    id_map.configure_command(&mut command);
    command.args(id_map.unshare_args());
    match config.network {
        NetworkMode::Host => {
            command.arg(bwrap);
        }
        NetworkMode::None | NetworkMode::User => {
            command
                .arg(std::env::current_exe().context("locating broker network wrapper")?)
                .arg("__private-network-sandbox")
                .arg("--bwrap")
                .arg(bwrap);
            if let Some(source) = &apparmor_access_source {
                command.arg("--apparmor-access").arg(source);
            }
            command
                .arg("--cgroup-source")
                .arg(cgroup.path())
                .arg("--cgroup-staged")
                .arg(
                    staged_cgroup
                        .as_ref()
                        .context("missing staged cgroup path")?,
                );
            command.arg("--");
        }
    }
    command
        .args([
            "--die-with-parent",
            "--new-session",
            "--as-pid-1",
            "--cap-add",
            "ALL",
        ])
        .args([
            "--unshare-pid",
            "--unshare-uts",
            "--unshare-ipc",
            "--unshare-cgroup",
        ])
        .arg("--hostname")
        .arg(&config.name)
        .arg("--bind")
        .arg(rootfs)
        .arg("/")
        .args(["--proc", "/proc", "--dev", "/dev"])
        .args([
            "--tmpfs", "/run", "--tmpfs", "/tmp", "--chmod", "1777", "/tmp",
        ])
        .args(["--dir", "/run/systemd/system"])
        .args(["--dir", "/run/systemd/system/wildbuzzard-desktop.service.d"])
        .arg("--ro-bind")
        .arg(&poweroff_drop_in)
        .arg(
            "/run/systemd/system/wildbuzzard-desktop.service.d/\
             10-wildbuzzard-poweroff-marker.conf",
        )
        .args(["--dir", "/shared"])
        .args(["--dir", "/run/wildbuzzard-host"])
        .args(["--dir", "/run/wildbuzzard-display-state"])
        .arg("--bind")
        .arg(shared)
        .arg("/shared")
        .arg("--bind")
        .arg(&guest_runtime)
        .arg("/run/wildbuzzard-host")
        .arg("--ro-bind")
        .arg(&display_state)
        .arg("/run/wildbuzzard-display-state")
        .args(["--ro-bind-try", "/sys", "/sys"])
        .arg("--bind")
        .arg(staged_cgroup.as_deref().unwrap_or_else(|| cgroup.path()))
        .arg("/sys/fs/cgroup")
        .arg("--ro-bind")
        .arg(&resolv_conf)
        .arg("/etc/resolv.conf")
        .arg("--ro-bind")
        .arg(&hostname)
        .arg("/etc/hostname")
        .arg("--block-fd")
        .arg(block_read.to_string())
        .arg("--json-status-fd")
        .arg(status_write.as_raw_fd().to_string())
        .args(["--setenv", "container", "wildbuzzard"])
        .args([
            "--setenv",
            "WILDBUZZARD_STATUS_DIR",
            "/run/wildbuzzard-host",
        ])
        .args(["--setenv", "WILDBUZZARD_MACHINE_ID", &config.id.to_string()])
        .args([
            "--setenv",
            "WILDBUZZARD_WINDOW_TITLE",
            &format!("Wild Buzzard — {}", config.name),
        ])
        .args([
            "--setenv",
            "WILDBUZZARD_WINDOW_APP_ID",
            "org.openresearchtools.wildbuzzard",
        ]);

    add_gpu_devices(&mut command, config, nvidia.as_ref())?;
    if let Some(injection) = &nvidia {
        injection.apply(&mut command);
    }
    if matches!(config.network, NetworkMode::Host) {
        command
            .arg("--bind-try")
            .arg(host_apparmor_access)
            .arg("/sys/kernel/security/apparmor/.access");
    }
    cgroup.move_command_on_exec(&mut command)?;

    command
        .arg("--")
        .arg("/usr/libexec/wildbuzzard-init")
        .stdin(Stdio::null());

    let mut container = TerminateOnDrop {
        child: command
            .spawn()
            .with_context(|| format!("starting bundled sandbox helper {}", bwrap.display()))?,
    };
    close_fd(block_read);
    drop(status_write);

    let container_pid = read_container_pid(status_read).inspect_err(|_| {
        terminate(&mut container.child);
    })?;

    let mut network = match config.network {
        NetworkMode::User => match start_slirp(resources, container_pid) {
            Ok(child) => Some(child),
            Err(error) => {
                terminate(&mut container.child);
                return Err(error);
            }
        },
        NetworkMode::Host | NetworkMode::None => None,
    };

    block_write
        .write_all(&[1])
        .context("releasing container start barrier")?;
    drop(block_write);

    if let Err(error) = wait_for_desktop(
        &mut container.child,
        &guest_runtime.join("desktop-ready"),
        &host_status.join("window.json"),
        &host_status.join("presentation.json"),
        Duration::from_secs(90),
    ) {
        let log = fs::read_to_string(guest_runtime.join("compositor.log"))
            .unwrap_or_else(|_| "the nested compositor produced no diagnostic log".into());
        return Err(error.context(format!("nested compositor log:\n{}", log.trim())));
    }

    state.state = MachineState::Running;
    state.container_pid = Some(container_pid);
    state.detail = Some("systemd and nested compositor ready".into());
    state.display = Some(display_diagnostics(
        &host_wayland,
        cgroup.path(),
        container_pid,
        &host_status.join("window.json"),
        &host_status.join("presentation.json"),
    )?);
    state.save(machine_dir)?;
    eprintln!("Wild Buzzard desktop '{}' is ready", config.name);

    let mut window_snapshot = fs::read(host_status.join("window.json")).ok();
    let mut presentation_snapshot = fs::read(host_status.join("presentation.json")).ok();
    let mut last_diagnostics_refresh = Instant::now();
    let mut close_shutdown_requested = false;
    let mut display_exited_for_shutdown = false;
    let mut unrequested_display_exit_deadline = None;
    let mut unrequested_display_status = None;
    let status = loop {
        if let Some(status) = container
            .child
            .try_wait()
            .context("checking systemd container status")?
        {
            let guest_poweroff_requested = guest_runtime.join(GUEST_POWEROFF_MARKER).is_file();
            if unrequested_display_exit_deadline.is_some() || guest_poweroff_requested {
                state.state = MachineState::Stopping;
                state.detail = Some(if guest_poweroff_requested {
                    "guest requested an orderly systemd poweroff".into()
                } else {
                    "guest display stopped during orderly systemd poweroff".into()
                });
                state.save(machine_dir)?;
            }
            break status;
        }
        if !display_exited_for_shutdown
            && let Some(display_status) = display
                .child
                .try_wait()
                .context("checking display gateway status")?
        {
            let close_requested = read_window_diagnostics(&host_status.join("window.json"))
                .is_some_and(|window| window.close_requested);
            let stop_requested = RuntimeState::load(machine_dir)?
                .is_some_and(|runtime| runtime.state == MachineState::Stopping);
            if close_requested || stop_requested {
                display_exited_for_shutdown = true;
            } else {
                // A guest-local `systemctl poweroff` stops Sway and therefore
                // closes the gateway connection shortly before namespace PID
                // 1 exits. Depending on which Wayland socket direction closes
                // first, the gateway can report either EOF (success) or a
                // reset/broken pipe (failure). Give both forms a bounded grace
                // period while still treating a standalone gateway exit as a
                // fault if systemd remains alive.
                display_exited_for_shutdown = true;
                unrequested_display_exit_deadline = Some(Instant::now() + Duration::from_secs(5));
                unrequested_display_status = Some(display_status);
            }
        }
        if unrequested_display_exit_deadline.is_some_and(|deadline| Instant::now() >= deadline)
            && !RuntimeState::load(machine_dir)?
                .is_some_and(|runtime| runtime.state == MachineState::Stopping)
        {
            terminate(&mut container.child);
            if let Some(mut child) = network.take() {
                terminate(&mut child.child);
            }
            let display_status =
                unrequested_display_status.context("missing exited display status")?;
            if display_status.success() {
                bail!("display gateway exited while systemd remained running");
            }
            bail!("display gateway exited unexpectedly with {display_status}");
        }
        if let Some(network_child) = network.as_mut()
            && let Some(network_status) = network_child
                .child
                .try_wait()
                .context("checking network helper status")?
        {
            terminate(&mut container.child);
            bail!("user-mode network helper exited unexpectedly with {network_status}");
        }
        let current_window = fs::read(host_status.join("window.json")).ok();
        let current_presentation = fs::read(host_status.join("presentation.json")).ok();
        if current_window != window_snapshot
            || current_presentation != presentation_snapshot
            || last_diagnostics_refresh.elapsed() >= Duration::from_secs(2)
        {
            let diagnostics = display_diagnostics(
                &host_wayland,
                cgroup.path(),
                container_pid,
                &host_status.join("window.json"),
                &host_status.join("presentation.json"),
            )?;
            let changed = match state.display.as_ref() {
                Some(previous) => {
                    display_diagnostics_signature(previous)?
                        != display_diagnostics_signature(&diagnostics)?
                }
                None => true,
            };
            if changed {
                state.display = Some(diagnostics);
                save_diagnostics_preserving_stop(machine_dir, state)?;
            }
            window_snapshot = current_window;
            presentation_snapshot = current_presentation;
            last_diagnostics_refresh = Instant::now();
        }
        if !close_shutdown_requested
            && read_window_diagnostics(&host_status.join("window.json"))
                .is_some_and(|window| window.close_requested)
        {
            state.state = MachineState::Stopping;
            state.detail = Some("host window closed; orderly systemd shutdown requested".into());
            state.save(machine_dir)?;
            let result = unsafe { libc::kill(container_pid as i32, libc::SIGRTMIN() + 3) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error).context("requesting shutdown after host window close");
                }
            }
            close_shutdown_requested = true;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    if read_window_diagnostics(&host_status.join("window.json"))
        .is_some_and(|window| window.close_requested)
    {
        state.state = MachineState::Stopping;
        state.detail = Some("host window closed; guest shutdown completed".into());
        state.save(machine_dir)?;
    }
    if let Some(mut child) = network.take() {
        terminate(&mut child.child);
    }
    cgroup.cleanup();
    Ok(status)
}

fn save_diagnostics_preserving_stop(machine_dir: &Path, state: &mut RuntimeState) -> Result<()> {
    if let Some(latest) = RuntimeState::load(machine_dir)?
        && latest.state == MachineState::Stopping
    {
        // The host launcher can request a stop between this broker's
        // diagnostics read and write. Never let an older in-memory `Running`
        // snapshot erase that lifecycle transition.
        state.state = MachineState::Stopping;
        state.detail = latest.detail;
    }
    state.save(machine_dir)
}

fn display_diagnostics_signature(display: &DisplayDiagnostics) -> Result<Vec<u8>> {
    let mut stable = display.clone();
    if let Some(presentation) = stable.presentation.as_mut() {
        presentation.sequence = 0;
        presentation.timestamp_ns = 0;
    }
    serde_json::to_vec(&stable).context("serializing stable display diagnostics")
}

fn display_diagnostics(
    host: &WaylandCapabilities,
    cgroup: &Path,
    container_pid: u32,
    window_path: &Path,
    presentation_path: &Path,
) -> Result<DisplayDiagnostics> {
    let render_nodes = compositor_device_nodes(cgroup)?;
    let renderer = compositor_environment_value(cgroup, "WLR_RENDERER")?
        .unwrap_or_else(|| "unknown".to_owned());
    let selected_render_device_identity =
        compositor_environment_value(cgroup, "WLR_RENDER_DRM_DEVICE")?
            .as_deref()
            .map(Path::new)
            .and_then(drm_device_identity);
    let render_device_identities = render_nodes
        .iter()
        .map(Path::new)
        .filter_map(drm_device_identity)
        .collect();
    let host_device_identity = host.dmabuf_main_device.map(|device| {
        render_node_for_device(Some(device))
            .as_deref()
            .and_then(drm_device_identity)
            .unwrap_or_else(|| format!("dev_t=0x{device:x} (no matching host render node)"))
    });
    let presentation = read_presentation_diagnostics(presentation_path);
    let main_device_match = host.dmabuf_main_device.and_then(|main_device| {
        render_nodes
            .iter()
            .filter(|node| node.starts_with("/dev/dri/"))
            .find(|node| {
                fs::metadata(node)
                    .map(|metadata| metadata.rdev() == main_device)
                    .unwrap_or(false)
            })
            .cloned()
    });
    let (zero_copy, detail) = if !host.linux_dmabuf {
        (
            "unavailable".into(),
            "host compositor does not advertise zwp_linux_dmabuf_v1".into(),
        )
    } else {
        let sync = if host.explicit_sync {
            "host explicit-sync protocol advertised"
        } else {
            "host explicit-sync protocol not advertised"
        };
        match (
            host.dmabuf_main_device,
            main_device_match,
            presentation.as_ref(),
        ) {
            (Some(_), Some(device), Some(frame))
                if frame.transport == "dmabuf"
                    && frame.presentation_feedback
                    && frame.presented
                    && frame.vsync
                    && frame.zero_copy
                    && frame.gtk_subsurface_offload
                    && frame.native_resolution
                    && (!host.explicit_sync
                        || frame.explicit_sync.starts_with("linux-drm-syncobj-v1")) =>
            {
                let host_is_currently_presenting =
                    frame.last_pacing_source != "internal-hidden-window-clock";
                (
                    if host_is_currently_presenting {
                        "active"
                    } else {
                        "proven-when-visible"
                    }
                    .into(),
                    format!(
                        "host imported and presented the nested compositor's unchanged dmabuf from {device} through GTK subsurface offload: {}x{} buffer for {}x{} logical pixels at {:.2}x scale, DRM format {}, modifier {}, {} plane(s), refresh {} ns, vblank={}, presentation-zero-copy={}, native-resolution={}; synchronization transport is {}. {}",
                        frame.width,
                        frame.height,
                        frame.viewport_width,
                        frame.viewport_height,
                        f64::from(frame.scale_120) / 120.0,
                        frame.format,
                        frame.modifier,
                        frame.planes,
                        frame.refresh_ns,
                        frame.vsync,
                        frame.zero_copy,
                        frame.native_resolution,
                        frame.explicit_sync,
                        if host_is_currently_presenting {
                            "The host surface is currently receiving real presentation callbacks"
                        } else {
                            "The host surface is currently hidden and not physically presenting; the same guest scanout remains live on its internal output clock"
                        }
                    ),
                )
            }
            (Some(_), Some(device), Some(frame))
                if frame.transport == "dmabuf" && frame.presented =>
            {
                (
                    "unavailable".into(),
                    format!(
                        "host presented a dmabuf from {device}, but the required zero-copy contract was not proven: explicit-sync={}, presentation-feedback={}, vblank={}, GTK-subsurface-offload={}, presentation-zero-copy={}, native-resolution={} ({}x{} buffer for {}x{} at {:.2}x)",
                        frame.explicit_sync,
                        frame.presentation_feedback,
                        frame.vsync,
                        frame.gtk_subsurface_offload,
                        frame.zero_copy,
                        frame.native_resolution,
                        frame.width,
                        frame.height,
                        frame.viewport_width,
                        frame.viewport_height,
                        f64::from(frame.scale_120) / 120.0
                    ),
                )
            }
            (Some(_), Some(device), _) => (
                "eligible-unverified".into(),
                format!(
                    "the nested compositor opened the host dmabuf feedback main device {device} and {sync}; final-buffer modifier/presentation instrumentation has not yet proven every frame copy-free"
                ),
            ),
            (Some(_), None, _) => (
                "unverified".into(),
                format!(
                    "the nested compositor did not open the host dmabuf feedback main device and {sync}; cross-device import may require a copy"
                ),
            ),
            (None, _, _) => (
                "unverified".into(),
                format!(
                    "dmabuf is available and {sync}, but the host supplied no v4 main-device feedback"
                ),
            ),
        }
    };
    Ok(DisplayDiagnostics {
        host: host.clone(),
        renderer,
        selected_render_device_identity,
        exposed_devices: guest_gpu_devices(container_pid),
        render_nodes,
        render_device_identities,
        host_device_identity,
        application_devices: process_device_nodes(cgroup, |name| !is_compositor_process(name))?,
        window: read_window_diagnostics(window_path),
        presentation,
        zero_copy,
        detail,
    })
}

fn guest_gpu_devices(container_pid: u32) -> Vec<String> {
    let guest_dev = Path::new("/proc")
        .join(container_pid.to_string())
        .join("root/dev");
    let mut devices = BTreeSet::new();
    for relative in ["dri", "nvidia-caps"] {
        let directory = guest_dev.join(relative);
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| !kind.is_dir()) {
                devices.insert(format!(
                    "/dev/{relative}/{}",
                    entry.file_name().to_string_lossy()
                ));
            }
        }
    }
    if let Ok(entries) = fs::read_dir(&guest_dev) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("nvidia")
                && entry.file_type().is_ok_and(|kind| !kind.is_dir())
            {
                devices.insert(format!("/dev/{}", name.to_string_lossy()));
            }
        }
    }
    devices.into_iter().collect()
}

fn read_window_diagnostics(path: &Path) -> Option<WindowDiagnostics> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_presentation_diagnostics(path: &Path) -> Option<PresentationDiagnostics> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn compositor_device_nodes(cgroup: &Path) -> Result<Vec<String>> {
    process_device_nodes(cgroup, is_compositor_process)
}

fn compositor_environment_value(cgroup: &Path, variable: &str) -> Result<Option<String>> {
    let mut pids = Vec::new();
    collect_cgroup_pids(cgroup, &mut pids)?;
    for pid in pids {
        let process = Path::new("/proc").join(pid.to_string());
        if fs::read_to_string(process.join("comm"))
            .is_ok_and(|name| is_compositor_process(name.trim()))
        {
            let environment = match fs::read(process.join("environ")) {
                Ok(environment) => environment,
                Err(_) => continue,
            };
            let prefix = format!("{variable}=");
            if let Some(value) = environment
                .split(|byte| *byte == 0)
                .filter_map(|entry| std::str::from_utf8(entry).ok())
                .find_map(|entry| entry.strip_prefix(&prefix))
            {
                return Ok(Some(value.to_owned()));
            }
        }
    }
    Ok(None)
}

fn is_compositor_process(name: &str) -> bool {
    name == "sway"
}

fn drm_device_identity(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let device = metadata.rdev();
    let driver = path
        .file_name()
        .and_then(|name| {
            fs::read_link(Path::new("/sys/class/drm").join(name).join("device/driver")).ok()
        })
        .and_then(|driver| {
            driver
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    Some(format!(
        "{} ({}:{}, driver={driver})",
        path.display(),
        libc::major(device),
        libc::minor(device)
    ))
}

fn process_device_nodes(
    cgroup: &Path,
    include_process: impl Fn(&str) -> bool,
) -> Result<Vec<String>> {
    let mut pids = Vec::new();
    collect_cgroup_pids(cgroup, &mut pids)?;
    let mut devices = BTreeSet::new();
    for pid in pids {
        let process = Path::new("/proc").join(pid.to_string());
        let name = match fs::read_to_string(process.join("comm")) {
            Ok(name) => name,
            Err(_) => continue,
        };
        if !include_process(name.trim()) {
            continue;
        }
        let descriptors = match fs::read_dir(process.join("fd")) {
            Ok(descriptors) => descriptors,
            Err(_) => continue,
        };
        for descriptor in descriptors.flatten() {
            if let Ok(target) = fs::read_link(descriptor.path()) {
                let value = target.to_string_lossy();
                if value.starts_with("/dev/dri/") || value.starts_with("/dev/nvidia") {
                    devices.insert(value.into_owned());
                }
            }
        }
    }
    Ok(devices.into_iter().collect())
}

fn collect_cgroup_pids(cgroup: &Path, pids: &mut Vec<u32>) -> Result<()> {
    let processes = cgroup.join("cgroup.procs");
    if let Ok(contents) = fs::read_to_string(&processes) {
        pids.extend(
            contents
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok()),
        );
    }
    let entries = match fs::read_dir(cgroup) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", cgroup.display()));
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("reading machine cgroup"),
        };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => collect_cgroup_pids(&entry.path(), pids)?,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("inspecting cgroup entry {}", entry.path().display())
                });
            }
        }
    }
    Ok(())
}

fn wait_for_desktop(
    container: &mut Child,
    marker: &Path,
    window_marker: &Path,
    presentation_marker: &Path,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if marker.is_file()
            && read_window_diagnostics(window_marker).is_some_and(|window| window.toplevels == 1)
        {
            let presentation_ready = if presentation_marker.exists() {
                read_presentation_diagnostics(presentation_marker)
                    .is_some_and(|frame| !frame.presentation_feedback || frame.presented)
            } else {
                true
            };
            if presentation_ready {
                return Ok(());
            }
        }
        if let Some(status) = container
            .try_wait()
            .context("checking systemd container readiness")?
        {
            bail!("container exited with {status} before the desktop compositor became ready");
        }
        if Instant::now() >= deadline {
            terminate(container);
            bail!(
                "desktop compositor did not become ready within {} seconds",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn lock_machine(machine_dir: &Path) -> Result<File> {
    let path = machine_dir.join("machine.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !file
        .metadata()
        .context("inspecting machine lock")?
        .is_file()
    {
        bail!("machine lock {} is not a regular file", path.display());
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => break,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                // The previous broker publishes `stopped` immediately before
                // returning and releasing this lock. Tolerate that tiny
                // orderly-shutdown handoff, while keeping a bounded failure
                // for a genuinely concurrent lifecycle operation.
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("machine at {} is already in use", machine_dir.display())
                });
            }
        }
    }
    Ok(file)
}

fn read_container_pid(status_read: RawFd) -> Result<u32> {
    // SAFETY: pipe() returned this new descriptor and ownership is transferred
    // to the File exactly once here.
    let file = unsafe { fs::File::from_raw_fd(status_read) };
    let mut line = String::new();
    BufReader::new(file)
        .read_line(&mut line)
        .context("reading container status")?;
    if line.is_empty() {
        bail!("sandbox helper exited before reporting the systemd process");
    }
    let value: serde_json::Value =
        serde_json::from_str(&line).context("parsing container status")?;
    value
        .get("child-pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .context("sandbox status did not contain a valid child-pid")
}

struct TerminateOnDrop {
    child: Child,
}

impl Drop for TerminateOnDrop {
    fn drop(&mut self) {
        terminate(&mut self.child);
    }
}

fn prepare_host_control_directory(control_socket: &Path) -> Result<()> {
    let directory = control_socket
        .parent()
        .context("host control socket has no parent directory")?;
    match fs::symlink_metadata(directory) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "host control runtime path {} must be a real directory",
                    directory.display()
                );
            }
            if metadata.uid() != Uid::effective().as_raw() {
                bail!(
                    "host control runtime directory {} is not owned by the current user",
                    directory.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(directory)
                .with_context(|| format!("creating {}", directory.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", directory.display()));
        }
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("securing {}", directory.display()))
}

struct DisplayGatewayPaths<'a> {
    host_wayland: &'a Path,
    guest_runtime: &'a Path,
    host_status: &'a Path,
    display_state: &'a Path,
    machine_dir: &'a Path,
}

fn start_display_gateway(
    resources: &ResourceLocator,
    paths: DisplayGatewayPaths<'_>,
    config: &MachineConfig,
    sync_drm_device: Option<&Path>,
) -> Result<TerminateOnDrop> {
    let control_socket = host_control_socket(paths.machine_dir)?;
    prepare_host_control_directory(&control_socket)?;
    let helper = resources.helper_or_path("wildbuzzard-display")?;
    let private_socket = paths.guest_runtime.join("wayland-0");
    let log_path = paths.host_status.join("display-gateway.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let log_error = log.try_clone().context("cloning display gateway log")?;
    let mut command = Command::new(&helper);
    command
        .arg("--host")
        .arg(paths.host_wayland)
        .arg("--listen")
        .arg(&private_socket)
        .arg("--control")
        .arg(&control_socket)
        .arg("--machine-dir")
        .arg(paths.machine_dir)
        .arg("--status-dir")
        .arg(paths.host_status)
        .arg("--output-state-dir")
        .arg(paths.display_state)
        .arg("--initial-width")
        .arg(config.width.to_string())
        .arg("--initial-height")
        .arg(config.height.to_string())
        .arg("--title")
        .arg(&config.name)
        .arg("--app-id")
        .arg(format!("org.openresearchtools.wildbuzzard.{}", config.name));
    if let Some(scale) = config.guest_scale_120 {
        command.arg("--guest-scale-120").arg(scale.to_string());
    }
    if let Some(sync_drm_device) = sync_drm_device {
        command.arg("--sync-drm-device").arg(sync_drm_device);
    }
    if let Some(scale) = std::env::var_os("WILDBUZZARD_TEST_FRACTIONAL_SCALE_120") {
        command.arg("--test-fractional-scale-120").arg(scale);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_error))
        .spawn()
        .with_context(|| format!("starting display gateway {}", helper.display()))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let display_metadata = fs::symlink_metadata(&private_socket);
        let control_metadata = fs::symlink_metadata(&control_socket);
        match (display_metadata, control_metadata) {
            (Ok(display), Ok(control))
                if !display.file_type().is_symlink()
                    && display.file_type().is_socket()
                    && !control.file_type().is_symlink()
                    && control.file_type().is_socket() =>
            {
                return Ok(TerminateOnDrop { child });
            }
            (Ok(_), Ok(_)) => {
                terminate(&mut child);
                bail!("display gateway created an invalid display or control socket");
            }
            (Err(error), _) | (_, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            (Err(error), _) | (_, Err(error)) => {
                terminate(&mut child);
                return Err(error).context("inspecting display gateway sockets");
            }
        }
        if let Some(status) = child
            .try_wait()
            .context("checking display gateway startup")?
        {
            let detail = fs::read_to_string(&log_path)
                .unwrap_or_else(|_| "display gateway produced no log".to_owned());
            bail!("display gateway exited with {status}: {}", detail.trim());
        }
        if Instant::now() >= deadline {
            terminate(&mut child);
            bail!("display gateway did not create its private socket");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn start_slirp(resources: &ResourceLocator, container_pid: u32) -> Result<TerminateOnDrop> {
    let slirp = resources.helper_or_path("slirp4netns")?;
    let (ready_read, ready_write) = pipe().context("creating network readiness pipe")?;
    let ready_fd = ready_write.as_raw_fd().to_string();
    let parent_pid = unsafe { libc::getpid() };
    let mut command = Command::new(&slirp);
    command
        .args([
            "--configure",
            "--disable-host-loopback",
            "--enable-sandbox",
            "--enable-seccomp",
            "--mtu=65520",
            "--ready-fd",
            &ready_fd,
            &container_pid.to_string(),
            "tap0",
        ])
        .stdin(Stdio::null());
    // SAFETY: this hook only calls async-signal-safe libc functions between
    // fork and exec. The parent check closes the race where the broker exits
    // immediately before PR_SET_PDEATHSIG is installed.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent_pid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "Wild Buzzard broker exited during network-helper startup",
                ));
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting bundled network helper {}", slirp.display()))?;
    drop(ready_write);

    let mut ready = unsafe { fs::File::from_raw_fd(ready_read) };
    let mut byte = [0_u8; 1];
    if let Err(error) = ready.read_exact(&mut byte) {
        terminate(&mut child);
        return Err(error).context("network helper exited before configuring the namespace");
    }
    Ok(TerminateOnDrop { child })
}

fn add_gpu_devices(
    command: &mut Command,
    config: &MachineConfig,
    nvidia: Option<&NvidiaInjection>,
) -> Result<()> {
    if config.gpus == ["all"] {
        command.args(["--dev-bind-try", "/dev/dri", "/dev/dri"]);
    } else {
        command.args(["--dir", "/dev/dri"]);
    }
    if let Some(injection) = nvidia {
        for path in &injection.device_nodes {
            if config.gpus == ["all"] && path.starts_with("/dev/dri") {
                continue;
            }
            command.arg("--dev-bind").arg(path).arg(path);
        }
        // Capability nodes are associated with the selected NVIDIA driver and
        // are not always emitted by CDI. Expose the driver-created directory
        // only when CDI selected NVIDIA devices.
        if Path::new("/dev/nvidia-caps").is_dir() {
            command.args(["--dev-bind", "/dev/nvidia-caps", "/dev/nvidia-caps"]);
        }
    } else if config.gpus != ["all"] {
        bail!("explicit GPU selection requires a validated NVIDIA CDI selection");
    }
    Ok(())
}

fn selected_nvidia_indices(gpus: &[String]) -> Result<Option<BTreeSet<u32>>> {
    if gpus == ["all"] {
        return Ok(None);
    }
    let mut selected = BTreeSet::new();
    for gpu in gpus {
        if let Ok(index) = gpu.parse::<u32>() {
            selected.insert(index);
            continue;
        }
        let index = nvidia_uuid_index(gpu)?
            .with_context(|| format!("NVIDIA GPU UUID '{gpu}' is not present on this host"))?;
        selected.insert(index);
    }
    Ok(Some(selected))
}

fn nvidia_uuid_index(requested: &str) -> Result<Option<u32>> {
    let directory = Path::new("/proc/driver/nvidia/gpus");
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("reading NVIDIA GPU information"),
    };
    for entry in entries {
        let information = entry
            .context("reading NVIDIA GPU information entry")?
            .path()
            .join("information");
        let contents = fs::read_to_string(&information)
            .with_context(|| format!("reading {}", information.display()))?;
        let mut uuid = None;
        let mut minor = None;
        for line in contents.lines() {
            if let Some(value) = line.strip_prefix("GPU UUID:") {
                uuid = Some(value.trim());
            } else if let Some(value) = line.strip_prefix("Device Minor:") {
                minor = value.trim().parse::<u32>().ok();
            }
        }
        if uuid == Some(requested) {
            return Ok(minor);
        }
    }
    Ok(None)
}

fn nvidia_drm_nodes(index: u32) -> Result<Vec<PathBuf>> {
    let pci_address = nvidia_pci_address(index)?
        .with_context(|| format!("NVIDIA GPU {index} has no PCI device information"))?;
    let mut nodes = Vec::new();
    for entry in fs::read_dir("/sys/class/drm").context("scanning DRM devices")? {
        let entry = entry.context("reading DRM device")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_node = name
            .strip_prefix("card")
            .or_else(|| name.strip_prefix("renderD"))
            .is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
            });
        if !is_node {
            continue;
        }
        let device = entry
            .path()
            .join("device")
            .canonicalize()
            .with_context(|| format!("resolving DRM device {name}"))?;
        if device
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value == pci_address)
        {
            let node = Path::new("/dev/dri").join(name.as_ref());
            if node.exists() {
                nodes.push(node);
            }
        }
    }
    Ok(nodes)
}

fn nvidia_pci_address(index: u32) -> Result<Option<String>> {
    let directory = Path::new("/proc/driver/nvidia/gpus");
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("reading NVIDIA GPU information"),
    };
    for entry in entries {
        let entry = entry.context("reading NVIDIA GPU information entry")?;
        let information = entry.path().join("information");
        let contents = fs::read_to_string(&information)
            .with_context(|| format!("reading {}", information.display()))?;
        let minor = contents.lines().find_map(|line| {
            line.strip_prefix("Device Minor:")
                .and_then(|value| value.trim().parse::<u32>().ok())
        });
        if minor == Some(index) {
            return Ok(entry.file_name().to_str().map(str::to_owned));
        }
    }
    Ok(None)
}

struct NvidiaInjection {
    library_binds: Vec<(PathBuf, PathBuf)>,
    metadata_binds: Vec<(PathBuf, PathBuf)>,
    cdi_binds: Vec<(PathBuf, PathBuf)>,
    device_nodes: Vec<PathBuf>,
    environment: Vec<(String, String)>,
    toolkit_version: String,
    cdi_devices: Vec<String>,
}

impl NvidiaInjection {
    fn apply(&self, command: &mut Command) {
        for (source, destination) in &self.library_binds {
            command.arg("--ro-bind").arg(source).arg(destination);
        }
        for (source, destination) in &self.metadata_binds {
            command.arg("--ro-bind").arg(source).arg(destination);
        }
        for (source, destination) in &self.cdi_binds {
            command.arg("--ro-bind").arg(source).arg(destination);
        }
        for (name, value) in &self.environment {
            command.arg("--setenv").arg(name).arg(value);
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NvidiaCdiSpec {
    cdi_version: String,
    kind: String,
    #[serde(default)]
    container_edits: NvidiaCdiEdits,
    #[serde(default)]
    devices: Vec<NvidiaCdiDevice>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NvidiaCdiEdits {
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    device_nodes: Vec<NvidiaCdiDeviceNode>,
    #[serde(default)]
    mounts: Vec<NvidiaCdiMount>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NvidiaCdiDevice {
    name: String,
    #[serde(default)]
    container_edits: NvidiaCdiEdits,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NvidiaCdiDeviceNode {
    path: PathBuf,
    major: Option<u64>,
    minor: Option<u64>,
    permissions: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NvidiaCdiMount {
    host_path: PathBuf,
    container_path: PathBuf,
    #[serde(default)]
    options: Vec<String>,
}

struct NvidiaCdiSelection {
    device_nodes: Vec<PathBuf>,
    mounts: Vec<(PathBuf, PathBuf)>,
    environment: Vec<(String, String)>,
    toolkit_version: String,
    device_names: Vec<String>,
}

fn prepare_nvidia_injection(
    resources: &ResourceLocator,
    runtime: &Path,
    config: &MachineConfig,
    host_main_device: Option<u64>,
) -> Result<Option<NvidiaInjection>> {
    if !Path::new("/dev/nvidiactl").exists() {
        if config.gpus != ["all"] {
            bail!("explicit NVIDIA GPU selection was requested, but /dev/nvidiactl is absent");
        }
        return Ok(None);
    }

    let mut cdi = generate_nvidia_cdi(resources, runtime, config)?;
    let driver = runtime.join("driver");
    let libraries = driver.join("lib");
    let gbm_backends = driver.join("gbm");
    fs::create_dir_all(&libraries).context("creating NVIDIA driver injection directory")?;
    fs::create_dir_all(&gbm_backends).context("creating NVIDIA GBM injection directory")?;
    fs::set_permissions(&driver, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(&libraries, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(&gbm_backends, fs::Permissions::from_mode(0o755))?;

    let mut aliases = BTreeMap::<String, PathBuf>::new();
    for directory in [
        "/usr/lib/x86_64-linux-gnu",
        "/lib/x86_64-linux-gnu",
        "/usr/lib64",
        "/lib64",
        "/usr/lib",
        "/lib",
    ] {
        scan_nvidia_libraries(Path::new(directory), &mut aliases)?;
    }
    if aliases.is_empty() {
        bail!(
            "NVIDIA device nodes exist, but matching host driver libraries could not be discovered"
        );
    }

    let mut real_files = BTreeMap::<String, PathBuf>::new();
    for (alias, source) in &aliases {
        let canonical = source
            .canonicalize()
            .with_context(|| format!("resolving NVIDIA library {}", source.display()))?;
        let real_name = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .context("NVIDIA library has a non-UTF-8 filename")?
            .to_owned();
        real_files.entry(real_name.clone()).or_insert(canonical);
        let destination = libraries.join(alias);
        if alias == &real_name {
            File::create(&destination)
                .with_context(|| format!("creating {}", destination.display()))?;
        } else if !destination.exists() {
            symlink(&real_name, &destination).with_context(|| {
                format!("creating NVIDIA library alias {}", destination.display())
            })?;
        }
    }

    let guest_library_dir = Path::new("/run/wildbuzzard-host/driver/lib");
    let mut library_binds = Vec::new();
    for (name, source) in real_files {
        let host_placeholder = libraries.join(&name);
        if !host_placeholder.exists() {
            File::create(&host_placeholder)
                .with_context(|| format!("creating {}", host_placeholder.display()))?;
        }
        library_binds.push((source, guest_library_dir.join(name)));
    }
    let host_gbm_backend = Path::new("/usr/lib/x86_64-linux-gnu/gbm/nvidia-drm_gbm.so");
    if host_gbm_backend.exists() {
        let placeholder = gbm_backends.join("nvidia-drm_gbm.so");
        File::create(&placeholder)
            .with_context(|| format!("creating {}", placeholder.display()))?;
        library_binds.push((
            host_gbm_backend
                .canonicalize()
                .context("resolving NVIDIA GBM backend")?,
            PathBuf::from("/run/wildbuzzard-host/driver/gbm/nvidia-drm_gbm.so"),
        ));
    }

    let mut environment = cdi.environment;
    environment.push((
        "LD_LIBRARY_PATH".into(),
        guest_library_dir.display().to_string(),
    ));
    let mut metadata_binds = Vec::new();
    stage_driver_json(
        Path::new("/usr/share/glvnd/egl_vendor.d/10_nvidia.json"),
        &driver.join("10_nvidia.json"),
        Path::new("/usr/share/glvnd/egl_vendor.d/10_nvidia.json"),
        &mut metadata_binds,
    )?;
    stage_json_directory(
        Path::new("/usr/share/egl/egl_external_platform.d"),
        &driver.join("egl_external_platform.d"),
        Path::new("/usr/share/egl/egl_external_platform.d"),
        &mut metadata_binds,
    )?;
    // CDI normally targets /etc or /usr/share directly. A file bind there
    // makes bubblewrap create a persistent zero-byte mountpoint in the
    // durable rootfs. Keep the NVIDIA ICD entirely in ephemeral runtime state
    // and direct the Vulkan loader to it explicitly instead.
    let nvidia_icd_guest = PathBuf::from("/run/wildbuzzard-host/driver/nvidia_icd.json");
    let nvidia_icd_source = cdi
        .mounts
        .iter()
        .find(|(_, destination)| is_nvidia_icd_destination(destination))
        .map(|(source, _)| source.clone())
        .or_else(|| {
            let source = Path::new("/usr/share/vulkan/icd.d/nvidia_icd.json");
            source.is_file().then(|| source.to_path_buf())
        });
    cdi.mounts
        .retain(|(_, destination)| !is_nvidia_icd_destination(destination));
    if let Some(source) = nvidia_icd_source {
        stage_driver_json(
            &source,
            &driver.join("nvidia_icd.json"),
            &nvidia_icd_guest,
            &mut metadata_binds,
        )?;
    }
    let nvidia_icd_staged = metadata_binds
        .iter()
        .any(|(_, guest)| guest == &nvidia_icd_guest);
    if !nvidia_icd_staged {
        bail!("selected NVIDIA devices lack a valid host Vulkan ICD");
    }

    if config.gpus != ["all"] {
        let selected = selected_nvidia_indices(&config.gpus)?
            .context("explicit GPU selection unexpectedly resolved to all GPUs")?;
        let compositor_gpu = selected
            .iter()
            .next()
            .context("explicit GPU selection is empty")?;
        let render_node = nvidia_drm_nodes(*compositor_gpu)?
            .into_iter()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("renderD"))
            })
            .with_context(|| {
                format!("NVIDIA GPU {compositor_gpu} has no DRM render node on this host")
            })?;
        environment.push((
            "WLR_RENDER_DRM_DEVICE".into(),
            render_node.display().to_string(),
        ));
        environment.push((
            "VK_DRIVER_FILES".into(),
            nvidia_icd_guest.display().to_string(),
        ));
        append_nvidia_compositor_environment(&mut environment);
    } else if let Some(render_node) = render_node_for_device(host_main_device) {
        let nvidia_compositor = render_node_uses_nvidia(&render_node);
        if nvidia_compositor {
            append_nvidia_compositor_environment(&mut environment);
        }
    }
    if config.gpus == ["all"] && nvidia_icd_staged {
        environment.push((
            "VK_ADD_DRIVER_FILES".into(),
            nvidia_icd_guest.display().to_string(),
        ));
    }

    let occupied_destinations = library_binds
        .iter()
        .chain(metadata_binds.iter())
        .map(|(_, destination)| destination.clone())
        .collect::<BTreeSet<_>>();
    let cdi_binds = cdi
        .mounts
        .into_iter()
        .filter(|(_, destination)| !occupied_destinations.contains(destination))
        .collect();

    Ok(Some(NvidiaInjection {
        library_binds,
        metadata_binds,
        cdi_binds,
        device_nodes: cdi.device_nodes,
        environment,
        toolkit_version: cdi.toolkit_version,
        cdi_devices: cdi.device_names,
    }))
}

fn generate_nvidia_cdi(
    resources: &ResourceLocator,
    runtime: &Path,
    config: &MachineConfig,
) -> Result<NvidiaCdiSelection> {
    let toolkit = resources
        .helper_or_path("nvidia-ctk")
        .context("locating bundled NVIDIA Container Toolkit")?;
    let spec_path = runtime.join("nvidia-cdi.json");
    let log_path = runtime.join("nvidia-ctk.log");
    let mut command = Command::new(&toolkit);
    command
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .args(["cdi", "generate", "--format", "json"])
        .arg("--output")
        .arg(&spec_path)
        .args(["--disable-hook", "all"]);
    let output = command
        .output()
        .with_context(|| format!("running bundled NVIDIA toolkit {}", toolkit.display()))?;
    let mut log = Vec::new();
    log.extend_from_slice(&output.stdout);
    log.extend_from_slice(&output.stderr);
    fs::write(&log_path, &log)
        .with_context(|| format!("writing NVIDIA toolkit log {}", log_path.display()))?;
    if !output.status.success() {
        bail!(
            "bundled NVIDIA toolkit failed to generate CDI ({})\n{}",
            output.status,
            String::from_utf8_lossy(&log)
        );
    }

    let version_output = Command::new(&toolkit)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .arg("--version")
        .output()
        .with_context(|| {
            format!(
                "reading bundled NVIDIA toolkit version {}",
                toolkit.display()
            )
        })?;
    if !version_output.status.success() {
        bail!(
            "bundled NVIDIA toolkit version probe failed with {}",
            version_output.status
        );
    }
    let toolkit_version = String::from_utf8(version_output.stdout)
        .context("NVIDIA toolkit version output is not UTF-8")?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if !toolkit_version.contains("1.19.1") {
        bail!("unexpected bundled NVIDIA toolkit version '{toolkit_version}'");
    }

    let bytes = fs::read(&spec_path)
        .with_context(|| format!("reading generated CDI spec {}", spec_path.display()))?;
    if bytes.len() > 16 * 1024 * 1024 {
        bail!("generated NVIDIA CDI spec exceeds the 16 MiB safety limit");
    }
    let spec: NvidiaCdiSpec =
        serde_json::from_slice(&bytes).context("parsing generated NVIDIA CDI JSON")?;
    validate_cdi_header(&spec)?;
    let selected_names = selected_cdi_device_names(config, &spec)?;
    let mut edits = vec![&spec.container_edits];
    for selected in &selected_names {
        let device = spec
            .devices
            .iter()
            .find(|device| &device.name == selected)
            .with_context(|| format!("generated CDI spec omitted selected device '{selected}'"))?;
        edits.push(&device.container_edits);
    }

    let mut devices = BTreeSet::new();
    let mut mounts = BTreeMap::<PathBuf, PathBuf>::new();
    let mut environment = BTreeMap::<String, String>::new();
    for edit in edits {
        for node in &edit.device_nodes {
            validate_cdi_device_node(node)?;
            devices.insert(node.path.clone());
        }
        for mount in &edit.mounts {
            if let Some((source, destination)) = validate_cdi_mount(mount)? {
                match mounts.insert(destination.clone(), source.clone()) {
                    Some(previous) if previous != source => {
                        bail!("NVIDIA CDI maps two sources to {}", destination.display())
                    }
                    _ => {}
                }
            }
        }
        for value in &edit.env {
            let (name, value) = value
                .split_once('=')
                .with_context(|| format!("NVIDIA CDI environment entry lacks '=': {value}"))?;
            if !name.starts_with("NVIDIA_")
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            {
                bail!("NVIDIA CDI supplied disallowed environment variable '{name}'");
            }
            environment.insert(name.to_owned(), value.to_owned());
        }
    }
    if !devices
        .iter()
        .any(|path| path == Path::new("/dev/nvidiactl"))
        || !devices.iter().any(|path| {
            path.to_string_lossy()
                .strip_prefix("/dev/nvidia")
                .is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
    {
        bail!("generated NVIDIA CDI selection lacks required control or GPU device nodes");
    }
    if !mounts.values().any(|path| {
        path.file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("libcuda.so."))
    }) {
        bail!("generated NVIDIA CDI selection lacks the matching host libcuda driver");
    }

    let diagnostics = serde_json::json!({
        "schema": 1,
        "toolkit": toolkit_version,
        "cdi_version": spec.cdi_version,
        "kind": spec.kind,
        "selected_devices": selected_names,
        "device_nodes": devices,
        "mounts": mounts.iter().map(|(destination, source)| {
            serde_json::json!({
                "source": source,
                "destination": destination,
            })
        }).collect::<Vec<_>>(),
    });
    fs::write(
        runtime.join("nvidia-cdi-selection.json"),
        serde_json::to_vec_pretty(&diagnostics)?,
    )
    .context("writing NVIDIA CDI selection diagnostics")?;
    eprintln!(
        "Wild Buzzard NVIDIA CDI: toolkit={toolkit_version}, devices={}",
        selected_names.join(",")
    );

    Ok(NvidiaCdiSelection {
        device_nodes: devices.into_iter().collect(),
        mounts: mounts
            .into_iter()
            .map(|(destination, source)| (source, destination))
            .collect(),
        environment: environment.into_iter().collect(),
        toolkit_version,
        device_names: selected_names,
    })
}

fn validate_cdi_header(spec: &NvidiaCdiSpec) -> Result<()> {
    if spec.kind != "nvidia.com/gpu" {
        bail!("unexpected NVIDIA CDI kind '{}'", spec.kind);
    }
    if !matches!(spec.cdi_version.as_str(), "0.6.0" | "0.7.0") {
        bail!(
            "unsupported NVIDIA CDI specification version '{}'",
            spec.cdi_version
        );
    }
    Ok(())
}

fn selected_cdi_device_names(config: &MachineConfig, spec: &NvidiaCdiSpec) -> Result<Vec<String>> {
    let requested = if config.gpus == ["all"] {
        vec!["all".to_owned()]
    } else {
        config.gpus.clone()
    };
    let available = spec
        .devices
        .iter()
        .map(|device| device.name.as_str())
        .collect::<BTreeSet<_>>();
    for name in &requested {
        if !available.contains(name.as_str()) {
            bail!(
                "selected NVIDIA CDI device '{name}' is unavailable; generated devices: {}",
                available.into_iter().collect::<Vec<_>>().join(", ")
            );
        }
    }
    Ok(requested)
}

fn validate_cdi_device_node(node: &NvidiaCdiDeviceNode) -> Result<()> {
    let value = node.path.to_string_lossy();
    let allowed = value.starts_with("/dev/nvidia")
        || value
            .strip_prefix("/dev/dri/card")
            .is_some_and(|suffix| suffix.bytes().all(|byte| byte.is_ascii_digit()))
        || value
            .strip_prefix("/dev/dri/renderD")
            .is_some_and(|suffix| suffix.bytes().all(|byte| byte.is_ascii_digit()));
    if !allowed || !safe_absolute_path(&node.path) {
        bail!(
            "NVIDIA CDI supplied disallowed device node {}",
            node.path.display()
        );
    }
    let metadata = fs::metadata(&node.path)
        .with_context(|| format!("inspecting NVIDIA CDI device {}", node.path.display()))?;
    if !metadata.file_type().is_char_device() {
        bail!(
            "NVIDIA CDI device {} is not a character device",
            node.path.display()
        );
    }
    if node
        .permissions
        .as_deref()
        .is_some_and(|permissions| !permissions.contains('r') || !permissions.contains('w'))
    {
        bail!(
            "NVIDIA CDI device {} lacks read/write permissions",
            node.path.display()
        );
    }
    let device = metadata.rdev();
    if node
        .major
        .is_some_and(|major| major != u64::from(libc::major(device)))
        || node
            .minor
            .is_some_and(|minor| minor != u64::from(libc::minor(device)))
    {
        bail!(
            "NVIDIA CDI device identity changed for {}",
            node.path.display()
        );
    }
    Ok(())
}

fn validate_cdi_mount(mount: &NvidiaCdiMount) -> Result<Option<(PathBuf, PathBuf)>> {
    if !safe_absolute_path(&mount.host_path) || !safe_absolute_path(&mount.container_path) {
        bail!(
            "NVIDIA CDI supplied unsafe mount {} -> {}",
            mount.host_path.display(),
            mount.container_path.display()
        );
    }
    let metadata = fs::symlink_metadata(&mount.host_path)
        .with_context(|| format!("inspecting NVIDIA CDI mount {}", mount.host_path.display()))?;
    if metadata.file_type().is_socket() {
        // A host persistenced socket is not required for CUDA/graphics and
        // would violate the narrow host-runtime boundary.
        return Ok(None);
    }
    if !mount.options.iter().any(|option| option == "ro") {
        bail!(
            "NVIDIA CDI mount {} is not read-only",
            mount.host_path.display()
        );
    }
    if !metadata.is_file() && !metadata.file_type().is_symlink() {
        bail!(
            "NVIDIA CDI mount source {} is not a file",
            mount.host_path.display()
        );
    }
    let source = mount
        .host_path
        .canonicalize()
        .with_context(|| format!("resolving NVIDIA CDI mount {}", mount.host_path.display()))?;
    if !allowed_nvidia_cdi_source(&source) || !allowed_nvidia_cdi_destination(&mount.container_path)
    {
        bail!(
            "NVIDIA CDI supplied out-of-contract mount {} -> {}",
            source.display(),
            mount.container_path.display()
        );
    }
    Ok(Some((source, mount.container_path.clone())))
}

fn safe_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
}

fn allowed_nvidia_cdi_source(path: &Path) -> bool {
    path.starts_with("/usr/lib")
        || path.starts_with("/lib/x86_64-linux-gnu")
        || path.starts_with("/lib/firmware/nvidia")
        || path.starts_with("/usr/share/nvidia")
        || path.starts_with("/usr/share/vulkan")
        || path.starts_with("/usr/share/glvnd")
        || path.starts_with("/usr/share/egl")
        || path
            .strip_prefix("/usr/bin")
            .ok()
            .and_then(Path::file_name)
            .is_some_and(|name| name.to_string_lossy().starts_with("nvidia-"))
}

fn allowed_nvidia_cdi_destination(path: &Path) -> bool {
    path.starts_with("/usr/lib")
        || path.starts_with("/lib/x86_64-linux-gnu")
        || path.starts_with("/lib/firmware/nvidia")
        || path.starts_with("/usr/share/nvidia")
        || path.starts_with("/usr/share/vulkan")
        || path.starts_with("/usr/share/glvnd")
        || path.starts_with("/usr/share/egl")
        || path.starts_with("/etc/vulkan")
        || path
            .strip_prefix("/usr/bin")
            .ok()
            .and_then(Path::file_name)
            .is_some_and(|name| name.to_string_lossy().starts_with("nvidia-"))
}

fn is_nvidia_icd_destination(path: &Path) -> bool {
    matches!(
        path.to_str(),
        Some("/etc/vulkan/icd.d/nvidia_icd.json") | Some("/usr/share/vulkan/icd.d/nvidia_icd.json")
    )
}

fn append_nvidia_compositor_environment(environment: &mut Vec<(String, String)>) {
    environment.extend([
        ("GBM_BACKEND".into(), "nvidia-drm".into()),
        (
            "GBM_BACKENDS_PATH".into(),
            "/run/wildbuzzard-host/driver/gbm".into(),
        ),
        ("__GLX_VENDOR_LIBRARY_NAME".into(), "nvidia".into()),
        (
            "__EGL_VENDOR_LIBRARY_FILENAMES".into(),
            "/usr/share/glvnd/egl_vendor.d/10_nvidia.json".into(),
        ),
        (
            "__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS".into(),
            "/usr/share/egl/egl_external_platform.d".into(),
        ),
    ]);
}

fn render_node_uses_nvidia(render_node: &Path) -> bool {
    let Some(name) = render_node.file_name() else {
        return false;
    };
    fs::read_link(Path::new("/sys/class/drm").join(name).join("device/driver"))
        .ok()
        .and_then(|driver| driver.file_name().map(|name| name == "nvidia"))
        .unwrap_or(false)
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
                && fs::metadata(path)
                    .map(|metadata| metadata.rdev() == device)
                    .unwrap_or(false)
        })
}

fn scan_nvidia_libraries(directory: &Path, aliases: &mut BTreeMap<String, PathBuf>) -> Result<()> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("scanning {}", directory.display()));
        }
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", directory.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_nvidia_library(name) {
            aliases
                .entry(name.to_owned())
                .or_insert_with(|| entry.path());
        }
    }
    Ok(())
}

fn is_nvidia_library(name: &str) -> bool {
    [
        "libcuda.so",
        "libcudadebugger.so",
        "libEGL_nvidia.so",
        "libGLESv1_CM_nvidia.so",
        "libGLESv2_nvidia.so",
        "libGLX_nvidia.so",
        "libnvcuvid.so",
        "libnvidia-",
        "libnvoptix.so",
        "libvdpau_nvidia.so",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}

fn stage_driver_json(
    source: &Path,
    staged: &Path,
    guest: &Path,
    mounts: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    if source.is_file() {
        fs::copy(source, staged)
            .with_context(|| format!("staging NVIDIA metadata {}", source.display()))?;
        mounts.push((staged.to_owned(), guest.to_owned()));
    }
    Ok(())
}

fn stage_json_directory(
    source: &Path,
    destination: &Path,
    guest_directory: &Path,
    mounts: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", source.display())),
    };
    fs::create_dir_all(destination)?;
    for entry in entries {
        let entry = entry.context("reading NVIDIA metadata directory")?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let staged = destination.join(entry.file_name());
            fs::copy(&path, &staged)
                .with_context(|| format!("staging NVIDIA metadata {}", path.display()))?;
            mounts.push((staged, guest_directory.join(entry.file_name())));
        }
    }
    Ok(())
}

fn write_environment_file(path: &Path, environment: &[(String, String)]) -> Result<()> {
    let mut contents = String::new();
    for (name, value) in environment {
        contents.push_str(name);
        contents.push('=');
        contents.push_str(value);
        contents.push('\n');
    }
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

struct MachineCgroup {
    path: PathBuf,
}

impl MachineCgroup {
    fn create(config: &MachineConfig, unshare: &Path) -> Result<Self> {
        let cgroup_description =
            fs::read_to_string("/proc/self/cgroup").context("reading current cgroup")?;
        let relative = cgroup_description
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .context("host does not use a unified cgroup v2 hierarchy")?;
        let relative = Path::new(relative);
        if !relative.is_absolute()
            || relative.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            })
        {
            bail!("current cgroup path is unsafe");
        }
        let base = Path::new("/sys/fs/cgroup").join(
            relative
                .strip_prefix("/")
                .context("invalid current cgroup path")?,
        );
        let path = base.join(format!("wildbuzzard-{}", config.id.simple()));
        if path.exists() {
            if let Err(initial_error) = cleanup_cgroup_tree(&path) {
                cleanup_cgroup_with_id_map(unshare, &path).with_context(|| {
                    format!(
                        "cleaning stale machine cgroup {} after direct cleanup failed: {initial_error}",
                        path.display()
                    )
                })?;
            }
        }
        fs::create_dir(&path)
            .with_context(|| format!("creating delegated machine cgroup {}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn move_command_on_exec(&self, command: &mut Command) -> Result<()> {
        let procs = self.path.join("cgroup.procs");
        let c_path = CString::new(procs.as_os_str().as_bytes())
            .context("cgroup path contains an embedded NUL")?;
        // SAFETY: the closure uses only async-signal-safe libc calls between
        // fork and exec. Writing "0" moves the calling child into this cgroup.
        unsafe {
            command.pre_exec(move || {
                let fd = libc::open(c_path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let bytes = b"0\n";
                let written = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
                let error = if written == bytes.len() as isize {
                    None
                } else {
                    Some(std::io::Error::last_os_error())
                };
                libc::close(fd);
                match error {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            });
        }
        Ok(())
    }

    fn cleanup(&self) {
        let _ = cleanup_cgroup_tree(&self.path);
    }
}

impl Drop for MachineCgroup {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn cleanup_cgroup_with_id_map(unshare: &Path, path: &Path) -> Result<()> {
    let id_map = IdMap::discover()?;
    let broker = std::env::current_exe().context("locating broker for cgroup cleanup")?;
    let mut command = Command::new(unshare);
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.unshare_args())
        .arg(broker)
        .arg("__cleanup-cgroup")
        .arg("--path")
        .arg(path)
        .stdin(Stdio::null())
        .status()
        .context("starting mapped cgroup cleanup")?;
    if !status.success() {
        bail!("mapped cgroup cleanup exited with {status}");
    }
    fs::remove_dir(path).with_context(|| format!("removing cgroup {}", path.display()))
}

fn cleanup_mapped_cgroup_children(path: &Path) -> Result<()> {
    validate_cgroup_cleanup_path(path)?;
    for entry in fs::read_dir(path).with_context(|| format!("reading cgroup {}", path.display()))? {
        let entry = entry.context("reading cgroup entry")?;
        if entry.file_type()?.is_dir() {
            cleanup_cgroup_tree(&entry.path())?;
        }
    }
    Ok(())
}

fn validate_cgroup_cleanup_path(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("cgroup cleanup path has no UTF-8 name")?;
    let identifier = name
        .strip_prefix("wildbuzzard-")
        .context("cgroup cleanup target is not a Wild Buzzard cgroup")?;
    if identifier.len() != 32 || !identifier.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("cgroup cleanup target has an invalid machine identifier");
    }
    let description = fs::read_to_string("/proc/self/cgroup")?;
    let relative = description
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .context("host does not use unified cgroup v2")?;
    let base = Path::new("/sys/fs/cgroup").join(
        Path::new(relative)
            .strip_prefix("/")
            .context("invalid current cgroup path")?,
    );
    if path.parent() != Some(base.as_path()) {
        bail!("cgroup cleanup target is outside the current delegated cgroup");
    }
    Ok(())
}

fn cleanup_cgroup_tree(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(path).with_context(|| format!("reading cgroup {}", path.display()))? {
        let entry = entry.context("reading cgroup entry")?;
        if entry.file_type()?.is_dir() {
            cleanup_cgroup_tree(&entry.path())?;
        }
    }
    fs::remove_dir(path).with_context(|| format!("removing cgroup {}", path.display()))
}

fn validate_rootfs(rootfs: &Path) -> Result<()> {
    let systemd = rootfs.join("lib/systemd/systemd");
    if !systemd.is_file() {
        bail!(
            "image has no {}; choose a systemd-based desktop image",
            systemd.display()
        );
    }
    Ok(())
}

fn run_private_network_sandbox(
    bwrap: &Path,
    apparmor_access: Option<&Path>,
    cgroup_source: &Path,
    cgroup_staged: &Path,
    arguments: &[OsString],
) -> Result<()> {
    let bwrap_metadata = fs::symlink_metadata(bwrap)
        .with_context(|| format!("inspecting sandbox helper {}", bwrap.display()))?;
    if bwrap_metadata.file_type().is_symlink()
        || !bwrap_metadata.is_file()
        || bwrap_metadata.permissions().mode() & 0o111 == 0
    {
        bail!("private-network sandbox helper must be a real executable file");
    }
    if arguments.is_empty() {
        bail!("private-network sandbox received no sandbox arguments");
    }

    let result = unsafe { libc::unshare(libc::CLONE_NEWNS | libc::CLONE_NEWNET) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("creating private network and mount namespaces");
    }
    mount_raw(
        None,
        Path::new("/"),
        None,
        libc::MS_REC | libc::MS_PRIVATE,
        None,
    )
    .context("making private-network mounts private")?;

    let source_metadata = fs::symlink_metadata(cgroup_source)
        .with_context(|| format!("inspecting delegated cgroup {}", cgroup_source.display()))?;
    let staged_metadata = fs::symlink_metadata(cgroup_staged)
        .with_context(|| format!("inspecting staged cgroup {}", cgroup_staged.display()))?;
    if source_metadata.file_type().is_symlink()
        || !source_metadata.is_dir()
        || staged_metadata.file_type().is_symlink()
        || !staged_metadata.is_dir()
    {
        bail!("delegated and staged cgroup paths must be real directories");
    }
    mount_raw(
        Some(cgroup_source),
        cgroup_staged,
        None,
        libc::MS_BIND | libc::MS_REC,
        None,
    )
    .context("preserving the delegated machine cgroup")?;

    if let Some(staged) = apparmor_access {
        let metadata = fs::symlink_metadata(staged).with_context(|| {
            format!("inspecting AppArmor access mountpoint {}", staged.display())
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("AppArmor access mountpoint must be a real file");
        }
        mount_raw(
            Some(Path::new("/sys/kernel/security/apparmor/.access")),
            staged,
            None,
            libc::MS_BIND,
            None,
        )
        .context("preserving the narrow AppArmor user-namespace gate")?;
    }

    mount_raw(
        Some(Path::new("sysfs")),
        Path::new("/sys"),
        Some("sysfs"),
        libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        None,
    )
    .context("mounting network-namespace-owned sysfs")?;

    if let Some(staged) = apparmor_access {
        let security = Path::new("/sys/kernel/security");
        mount_raw(
            Some(Path::new("tmpfs")),
            security,
            Some("tmpfs"),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            Some("mode=0755,size=4096"),
        )
        .context("creating narrow securityfs compatibility view")?;
        let apparmor = security.join("apparmor");
        fs::create_dir(&apparmor).context("creating narrow AppArmor directory")?;
        let target = apparmor.join(".access");
        File::create(&target).context("creating narrow AppArmor access target")?;
        mount_raw(Some(staged), &target, None, libc::MS_BIND, None)
            .context("mounting the narrow AppArmor access gate")?;
    }

    let error = Command::new(bwrap).args(arguments).exec();
    Err(error).with_context(|| format!("executing sandbox helper {}", bwrap.display()))
}

fn mount_raw(
    source: Option<&Path>,
    target: &Path,
    filesystem: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<()> {
    let source = source
        .map(|path| CString::new(path.as_os_str().as_bytes()))
        .transpose()
        .context("mount source contains an embedded NUL")?;
    let target = CString::new(target.as_os_str().as_bytes())
        .context("mount target contains an embedded NUL")?;
    let filesystem = filesystem
        .map(CString::new)
        .transpose()
        .context("mount filesystem contains an embedded NUL")?;
    let data = data
        .map(CString::new)
        .transpose()
        .context("mount data contains an embedded NUL")?;
    let result = unsafe {
        libc::mount(
            source
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            target.as_ptr(),
            filesystem
                .as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr()),
            flags,
            data.as_ref()
                .map_or(std::ptr::null(), |value| value.as_ptr())
                .cast(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "mounting {} at {}",
                source
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |value| value.to_string_lossy().into()),
                target.to_string_lossy()
            )
        })
    }
}

fn host_wayland_socket() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR is not set; launch Wild Buzzard from a Wayland session")?;
    let display = std::env::var_os("WAYLAND_DISPLAY")
        .context("WAYLAND_DISPLAY is not set; launch Wild Buzzard from a Wayland session")?;
    let display = PathBuf::from(display);
    let socket = if display.is_absolute() {
        display
    } else {
        PathBuf::from(runtime).join(display)
    };
    if !socket.exists() {
        bail!("host Wayland socket {} does not exist", socket.display());
    }
    Ok(socket)
}

fn canonical_real_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "{label} {} must be a real directory, not a symlink",
            path.display()
        );
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolving {label} {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{label} {} is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn validate_portable_layout(
    machine_dir: &Path,
    rootfs: &Path,
    shared: &Path,
    config: &MachineConfig,
) -> Result<()> {
    let machines = machine_dir
        .parent()
        .context("machine directory has no vm parent")?;
    let portable = machines
        .parent()
        .context("vm directory has no portable parent")?;
    if machines.file_name().and_then(|name| name.to_str()) != Some("vm") {
        bail!("machine directory must be directly inside the portable vm directory");
    }
    if shared != portable.join("shared") {
        bail!("shared folder must be the portable folder's direct shared directory");
    }
    if rootfs.parent() != Some(machine_dir)
        || rootfs.file_name().and_then(|name| name.to_str()) != Some("rootfs")
    {
        bail!("machine rootfs must be the machine directory's direct rootfs directory");
    }
    if machine_dir.file_name().and_then(|name| name.to_str()) != Some(config.name.as_str()) {
        bail!("machine metadata name does not match its portable directory");
    }
    for file in [
        machine_dir.join(MachineConfig::FILE),
        machine_dir.join(RuntimeState::FILE),
    ] {
        let metadata = fs::symlink_metadata(&file)
            .with_context(|| format!("inspecting machine metadata {}", file.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("machine metadata {} must be a regular file", file.display());
        }
    }
    Ok(())
}

fn pipe() -> std::io::Result<(RawFd, fs::File)> {
    let mut descriptors = [-1_i32; 2];
    let result = unsafe { libc::pipe(descriptors.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: libc::pipe returned two new, owned file descriptors.
    let writer = unsafe { fs::File::from_raw_fd(descriptors[1]) };
    Ok((descriptors[0], writer))
}

fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

fn terminate(child: &mut Child) {
    let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM);
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn portable_machine() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, MachineConfig) {
        let portable = tempfile::tempdir().unwrap();
        let machine = portable.path().join("vm/demo");
        let rootfs = machine.join("rootfs");
        let data = portable.path().join("shared");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir(&data).unwrap();
        let config = MachineConfig::new(
            "demo".into(),
            "example.invalid/desktop".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        );
        config.save(&machine).unwrap();
        RuntimeState::new(MachineState::Stopped)
            .save(&machine)
            .unwrap();
        (
            portable,
            machine.canonicalize().unwrap(),
            rootfs.canonicalize().unwrap(),
            data.canonicalize().unwrap(),
            config,
        )
    }

    #[test]
    fn validates_direct_portable_machine_layout() {
        let (_portable, machine, rootfs, data, config) = portable_machine();
        validate_portable_layout(&machine, &rootfs, &data, &config).unwrap();
    }

    #[test]
    fn rejects_metadata_name_mismatch_and_symlink_lock() {
        let (portable, machine, rootfs, data, mut config) = portable_machine();
        config.name = "other".into();
        assert!(validate_portable_layout(&machine, &rootfs, &data, &config).is_err());

        let outside = portable.path().join("outside-lock");
        File::create(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, machine.join("machine.lock")).unwrap();
        assert!(lock_machine(&machine).is_err());
    }

    #[test]
    fn disappearing_cgroup_is_normal_during_shutdown_diagnostics() {
        let directory = tempfile::tempdir().unwrap();
        let vanished = directory.path().join("removed-systemd-mount-unit");
        let mut pids = Vec::new();

        collect_cgroup_pids(&vanished, &mut pids).unwrap();

        assert!(pids.is_empty());
    }

    #[test]
    fn diagnostics_refresh_cannot_erase_an_external_stop_request() {
        let (_portable, machine, _rootfs, _data, _config) = portable_machine();
        let mut requested = RuntimeState::new(MachineState::Stopping);
        requested.detail = Some("orderly shutdown requested".into());
        requested.save(&machine).unwrap();

        let mut stale = RuntimeState::new(MachineState::Running);
        stale.detail = Some("periodic diagnostics refresh".into());
        save_diagnostics_preserving_stop(&machine, &mut stale).unwrap();

        let saved = RuntimeState::load(&machine).unwrap().unwrap();
        assert_eq!(saved.state, MachineState::Stopping);
        assert_eq!(saved.detail.as_deref(), Some("orderly shutdown requested"));
    }

    #[test]
    fn parses_unified_native_presentation_schema() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presentation.json");
        fs::write(
            &path,
            serde_json::to_vec(&PresentationDiagnostics {
                width: 2560,
                height: 1318,
                format: u32::from_le_bytes(*b"XR24"),
                modifier: "0x010000000000000f".into(),
                planes: 3,
                scale_120: 160,
                viewport_width: 1920,
                viewport_height: 988,
                native_resolution: true,
                explicit_sync: "linux-drm-syncobj-v1/gateway-wait/gtk-host-sync".into(),
                presentation_feedback: true,
                presented: true,
                vsync: true,
                zero_copy: true,
                gtk_subsurface_offload: true,
                submitted_frames: 5,
                painted_frames: 5,
                presented_frames: 5,
                dropped_frames: 0,
                refresh_ns: 4_166_000,
                ..PresentationDiagnostics::default()
            })
            .unwrap(),
        )
        .unwrap();

        let frame = read_presentation_diagnostics(&path).unwrap();
        assert_eq!(frame.schema, 5);
        assert_eq!((frame.width, frame.height), (2560, 1318));
        assert_eq!((frame.viewport_width, frame.viewport_height), (1920, 988));
        assert!(frame.native_resolution);
        assert!(frame.gtk_subsurface_offload);
        assert!(frame.zero_copy);
        assert_eq!(frame.dropped_frames, 0);
    }

    #[test]
    fn selects_only_requested_devices_from_generated_nvidia_cdi() {
        let spec: NvidiaCdiSpec = serde_json::from_value(serde_json::json!({
            "cdiVersion": "0.7.0",
            "kind": "nvidia.com/gpu",
            "containerEdits": {
                "env": ["NVIDIA_VISIBLE_DEVICES=void"],
                "deviceNodes": [],
                "mounts": []
            },
            "devices": [
                {"name": "0", "containerEdits": {}},
                {"name": "1", "containerEdits": {}},
                {
                    "name": "GPU-f832efd8-97ec-6d10-046f-f7a8e84b1c3b",
                    "containerEdits": {}
                },
                {"name": "all", "containerEdits": {}}
            ]
        }))
        .unwrap();
        validate_cdi_header(&spec).unwrap();

        let mut config = MachineConfig::new(
            "demo".into(),
            "example.invalid/desktop".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["0".into(), "1".into()],
        );
        assert_eq!(
            selected_cdi_device_names(&config, &spec).unwrap(),
            ["0", "1"]
        );
        config.gpus = vec!["all".into()];
        assert_eq!(selected_cdi_device_names(&config, &spec).unwrap(), ["all"]);
        config.gpus = vec!["2".into()];
        assert!(selected_cdi_device_names(&config, &spec).is_err());
    }

    #[test]
    fn rejects_unsafe_or_unrelated_cdi_paths() {
        for unsafe_path in [
            Path::new("relative/device"),
            Path::new("/dev/dri/../mem"),
            Path::new("/usr/lib/../../home/user"),
        ] {
            assert!(!safe_absolute_path(unsafe_path));
        }
        assert!(safe_absolute_path(Path::new("/dev/dri/renderD128")));
        assert!(allowed_nvidia_cdi_source(Path::new(
            "/usr/lib/x86_64-linux-gnu/libcuda.so.610.43.02"
        )));
        assert!(!allowed_nvidia_cdi_source(Path::new(
            "/home/user/.ssh/id_rsa"
        )));
        assert!(allowed_nvidia_cdi_destination(Path::new(
            "/etc/vulkan/icd.d/nvidia_icd.json"
        )));
        assert!(is_nvidia_icd_destination(Path::new(
            "/etc/vulkan/icd.d/nvidia_icd.json"
        )));
        assert!(is_nvidia_icd_destination(Path::new(
            "/usr/share/vulkan/icd.d/nvidia_icd.json"
        )));
        assert!(!is_nvidia_icd_destination(Path::new(
            "/usr/share/vulkan/icd.d/intel_icd.json"
        )));
        assert!(!allowed_nvidia_cdi_destination(Path::new(
            "/run/host-wayland"
        )));
    }
}
