// SPDX-License-Identifier: AGPL-3.0-or-later

mod integrations;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, Uid, setsid};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use wb_core::{
    DESKTOP_READINESS_DEADLINE_DETAIL_PREFIX, DisplayDiagnostics, IdMap, MachineConfig,
    MachineState, NetworkMode, PresentationDiagnostics, ResourceLocator, RuntimeState,
    WaylandCapabilities, WindowDiagnostics, host_control_socket,
};

use integrations::{IntegrationRuntime, SlirpRuntime};
use uuid::Uuid;

const GUEST_POWEROFF_MARKER: &str = "guest-poweroff-requested";
const GUEST_RUNTIME_MODE: u32 = 0o700;
const DESKTOP_READY_MODE: u32 = 0o600;
const SESSION_TOKEN_BYTES: usize = 32;
const MAX_DESKTOP_READY_BYTES: u64 = 256;
const MAX_BUBBLEWRAP_INFO_BYTES: u64 = 64 * 1024;
const BUZZARDOS_HOST_APP_ID: &str = "org.openresearchtools.buzzardos";

#[derive(Debug)]
struct DesktopReadinessDeadline {
    seconds: u64,
}

impl std::fmt::Display for DesktopReadinessDeadline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "desktop compositor did not become ready within {} seconds",
            self.seconds
        )
    }
}

impl std::error::Error for DesktopReadinessDeadline {}

#[derive(Debug)]
struct DesktopFrameReadinessDeadline {
    seconds: u64,
}

impl std::fmt::Display for DesktopFrameReadinessDeadline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "desktop compositor became ready but did not submit and paint a dmabuf frame within {} seconds; verify that the host display provides working DRM render-node and 3D acceleration support",
            self.seconds
        )
    }
}

impl std::error::Error for DesktopFrameReadinessDeadline {}

fn machine_session_failure_detail(error: &anyhow::Error) -> String {
    if let Some(deadline) = error.downcast_ref::<DesktopReadinessDeadline>() {
        format!(
            "{DESKTOP_READINESS_DEADLINE_DETAIL_PREFIX}{}: {error:#}",
            deadline.seconds
        )
    } else if let Some(deadline) = error.downcast_ref::<DesktopFrameReadinessDeadline>() {
        format!(
            "{DESKTOP_READINESS_DEADLINE_DETAIL_PREFIX}{}: {error:#}",
            deadline.seconds
        )
    } else {
        format!("{error:#}")
    }
}

#[derive(Debug, Parser)]
#[command(name = "buzzardos-broker", version)]
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
        detach: bool,
    },
    #[command(name = "__cleanup-cgroup", hide = true)]
    CleanupCgroup {
        #[arg(long)]
        path: PathBuf,
    },
    #[command(name = "__hold-user-namespace", hide = true)]
    HoldUserNamespace {
        #[arg(long)]
        ready_fd: RawFd,
        #[arg(long)]
        release_fd: RawFd,
    },
    #[command(name = "__hold-nested-user-namespace", hide = true)]
    HoldNestedUserNamespace {
        #[arg(long)]
        ready_fd: RawFd,
        #[arg(long)]
        release_fd: RawFd,
    },
    #[command(name = "__map-nested-user-namespace", hide = true)]
    MapNestedUserNamespace {
        #[arg(long)]
        holder_pid: u32,
        #[arg(long)]
        start_fd: RawFd,
        #[arg(long)]
        done_fd: RawFd,
    },
    #[command(name = "__private-network-sandbox", hide = true)]
    PrivateNetworkSandbox {
        #[arg(long)]
        bwrap: PathBuf,
        #[arg(long)]
        host_network: bool,
        #[arg(long)]
        private_bind_root: PathBuf,
        #[arg(long = "private-bind")]
        private_binds: Vec<String>,
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
        eprintln!("buzzardos-broker: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    match Cli::parse().command {
        Commands::Run {
            machine_dir,
            detach,
        } => run_machine(&machine_dir, detach),
        Commands::CleanupCgroup { path } => cleanup_mapped_cgroup_children(&path),
        Commands::HoldUserNamespace {
            ready_fd,
            release_fd,
        } => hold_user_namespace(ready_fd, release_fd),
        Commands::HoldNestedUserNamespace {
            ready_fd,
            release_fd,
        } => hold_nested_user_namespace(ready_fd, release_fd),
        Commands::MapNestedUserNamespace {
            holder_pid,
            start_fd,
            done_fd,
        } => map_nested_user_namespace(holder_pid, start_fd, done_fd),
        Commands::PrivateNetworkSandbox {
            bwrap,
            host_network,
            private_bind_root,
            private_binds,
            apparmor_access,
            cgroup_source,
            cgroup_staged,
            arguments,
        } => run_private_network_sandbox(PrivateNetworkSandbox {
            bwrap: &bwrap,
            host_network,
            private_bind_root: &private_bind_root,
            private_binds: &private_binds,
            apparmor_access: apparmor_access.as_deref(),
            cgroup_source: &cgroup_source,
            cgroup_staged: &cgroup_staged,
            arguments: &arguments,
        }),
    }
}

fn run_machine(machine_dir: &Path, detach: bool) -> Result<()> {
    if detach {
        setsid().context("creating detached broker session")?;
    }

    let machine_dir = canonical_real_directory(machine_dir, "machine directory")?;
    let rootfs = canonical_real_directory(&machine_dir.join("rootfs"), "machine rootfs")?;
    let initial_config = MachineConfig::load(&machine_dir)?;
    validate_machine_layout(&machine_dir, &rootfs)?;
    resolve_shared_paths(&initial_config)?;
    validate_rootfs(&rootfs)?;
    let _machine_lock = lock_machine(&machine_dir)?;

    let wayland = host_wayland_socket()?;
    let resources = ResourceLocator::discover()?;
    let bwrap = resources.helper_or_path("bwrap")?;
    let unshare = resources.helper_or_path("unshare")?;

    let host_wayland =
        WaylandCapabilities::probe(&wayland).context("probing host Wayland capabilities")?;
    let runtime = LifecycleRuntime::create()?;
    let render_device = preferred_render_node(host_wayland.dmabuf_main_device);
    let private_dmabuf_version = private_dmabuf_version(&host_wayland)?;
    let mut display = start_display_gateway(
        &resources,
        DisplayGatewayPaths {
            host_wayland: &wayland,
            guest_runtime: &runtime.guest,
            host_status: &runtime.host_status,
            display_state: &runtime.display_state,
            machine_dir: &machine_dir,
        },
        &initial_config,
        private_dmabuf_version,
        render_device.as_deref(),
    )?;
    let machine_name = initial_config.name.clone();

    let mut start_requested = true;
    loop {
        if start_requested {
            clear_session_runtime(&runtime)?;
            let config = MachineConfig::load(&machine_dir)?;
            validate_machine_layout(&machine_dir, &rootfs)?;
            let shares = resolve_shared_paths(&config)?;
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
                &shares,
                &host_wayland,
                &runtime,
                &mut display,
                &mut state,
            );
            // The display application survives an in-place restart, but a
            // complete stop closes it. The clipboard endpoint never survives
            // either transition. Revoke the old socket and readiness evidence
            // immediately after PID 1 and all descendants are gone, including
            // failed-start paths. A later start must publish a fresh socket;
            // it can never inherit a pathname from the previous session.
            let clipboard_cleanup = clear_clipboard_session_runtime(&runtime);
            let result = combine_session_and_clipboard_cleanup(result, clipboard_cleanup);

            let latest = RuntimeState::load(&machine_dir)?;
            match result {
                Ok(session)
                    if session.status.success()
                        || latest
                            .as_ref()
                            .is_some_and(|state| state.state == MachineState::Stopping) =>
                {
                    let restart = session.restart;
                    let shutdown_detail = latest
                        .filter(|state| state.state == MachineState::Stopping)
                        .and_then(|state| state.detail)
                        .unwrap_or_else(|| "clean shutdown".into());
                    let mut stopped = RuntimeState::new(MachineState::Stopped);
                    if !restart {
                        stopped.launcher_pid = None;
                    }
                    stopped.container_pid = None;
                    stopped.detail = Some(shutdown_detail);
                    stopped.save(&machine_dir)?;
                    if !restart {
                        return Ok(());
                    }
                    start_requested = true;
                }
                Ok(session) => {
                    let mut failed = RuntimeState::new(MachineState::Failed);
                    failed.container_pid = None;
                    failed.detail = Some(format!("container exited with {}", session.status));
                    failed.save(&machine_dir)?;
                    start_requested = session.restart;
                }
                Err(error) => {
                    let mut failed = RuntimeState::new(MachineState::Failed);
                    failed.container_pid = None;
                    failed.detail = Some(machine_session_failure_detail(&error));
                    failed.save(&machine_dir)?;
                    eprintln!("Buzzard OS machine session failed: {error:#}");
                    start_requested = false;
                }
            }
        }

        if start_requested {
            continue;
        }

        if let Some(status) = display
            .child
            .try_wait()
            .context("checking persistent display gateway status")?
        {
            let mut stopped = RuntimeState::new(MachineState::Stopped);
            stopped.launcher_pid = None;
            stopped.container_pid = None;
            stopped.detail = Some("host application closed".into());
            stopped.save(&machine_dir)?;
            if status.success() {
                return Ok(());
            }
            bail!("persistent display gateway exited with {status}");
        }

        // Once the native window has accepted Close it will quit as soon as
        // this stopped state is visible.  Do not consume a concurrent `start`
        // request during that short interval: the display child and its
        // control socket are already committed to exiting, so launching PID 1
        // against them would leave a headless session stuck in Starting.
        // The launcher observes this supervisor exit and starts a fresh native
        // application instead.
        if read_window_diagnostics(&runtime.host_status.join("window.json"))
            .is_some_and(|window| window.close_requested)
        {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }

        match take_host_request(&runtime.host_status, &machine_name) {
            Ok(Some(request)) => match request.as_str() {
                "start" | "restart" => start_requested = true,
                "stop" => {}
                _ => unreachable!("validated host request"),
            },
            Ok(None) => {}
            Err(error) => {
                let mut failed = RuntimeState::new(MachineState::Failed);
                failed.container_pid = None;
                failed.detail = Some(format!("invalid host lifecycle request: {error:#}"));
                failed.save(&machine_dir)?;
                eprintln!("Buzzard OS rejected host lifecycle request: {error:#}");
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn add_guest_pseudo_filesystems(command: &mut Command) {
    // Mount mqueue before PID 1 starts, just as procfs and the private /dev
    // view are mounted. Letting systemd create this mount later requires a
    // mount-helper exec path that is not reliable in an unprivileged user
    // namespace with no usable session keyring. Bubblewrap performs the mount
    // directly while it constructs the already-authorized guest namespace.
    command.args([
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--mqueue",
        "/dev/mqueue",
    ]);
}

fn guest_hosts_contents(hostname: &str) -> String {
    format!(
        "127.0.0.1\tlocalhost\n127.0.1.1\t{hostname}\n::1\tlocalhost ip6-localhost ip6-loopback\nff02::1\tip6-allnodes\nff02::2\tip6-allrouters\n"
    )
}

#[allow(clippy::too_many_arguments)]
fn launch_container(
    bwrap: &Path,
    unshare: &Path,
    resources: &ResourceLocator,
    config: &MachineConfig,
    machine_dir: &Path,
    rootfs: &Path,
    shares: &[ResolvedShare],
    host_wayland: &WaylandCapabilities,
    runtime: &LifecycleRuntime,
    display: &mut TerminateOnDrop,
    state: &mut RuntimeState,
) -> Result<SessionResult> {
    let (block_read, mut block_write) = pipe().context("creating container start barrier")?;
    let (info_read, info_write) = pipe().context("creating container information pipe")?;
    let guest_runtime = &runtime.guest;
    let host_status = &runtime.host_status;
    let display_state = &runtime.display_state;
    let resolv_conf = guest_runtime.join("resolv.conf");
    let resolv_contents = match config.network {
        NetworkMode::User => {
            "# Buzzard OS slirp4netns DNS\nnameserver 10.0.2.3\noptions edns0\n".to_owned()
        }
        NetworkMode::Host => {
            fs::read_to_string("/etc/resolv.conf").context("reading host resolver configuration")?
        }
        NetworkMode::None => "# Networking disabled by Buzzard OS\n".to_owned(),
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
    // Keep libc, sudo, and package maintainer scripts able to resolve the
    // ephemeral UTS hostname without modifying the persistent rootfs. A
    // matching /etc/hosts entry is part of the same per-launch hostname
    // state as /etc/hostname.
    let hosts = guest_runtime.join("hosts");
    fs::write(&hosts, guest_hosts_contents(&config.name))
        .with_context(|| format!("writing {}", hosts.display()))?;
    fs::set_permissions(&hosts, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting permissions on {}", hosts.display()))?;
    let initial_output = guest_runtime.join("initial-output.conf");
    fs::write(
        &initial_output,
        format!("output * mode {}x{}\n", config.width, config.height),
    )
    .with_context(|| format!("writing {}", initial_output.display()))?;
    fs::set_permissions(&initial_output, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting permissions on {}", initial_output.display()))?;
    let poweroff_drop_in = guest_runtime.join("buzzardos-desktop-poweroff-marker.conf");
    fs::write(
        &poweroff_drop_in,
        format!(
            "[Service]\nExecStop=+/usr/bin/touch /run/buzzardos-host/{GUEST_POWEROFF_MARKER}\n"
        ),
    )
    .with_context(|| format!("writing {}", poweroff_drop_in.display()))?;
    fs::set_permissions(&poweroff_drop_in, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting permissions on {}", poweroff_drop_in.display()))?;
    let nvidia = prepare_nvidia_injection(
        resources,
        guest_runtime,
        config,
        host_wayland.dmabuf_main_device,
    )?;
    let session_token = Uuid::new_v4().simple().to_string();
    let mut service_environment = vec![
        ("BUZZARDOS_SESSION_TOKEN".into(), session_token.clone()),
        ("BUZZARDOS_MACHINE_ID".into(), config.id.to_string()),
        ("BUZZARDOS_MACHINE_NAME".into(), config.name.clone()),
        (
            "BUZZARDOS_WINDOW_TITLE".into(),
            format!("Buzzard OS — {}", config.name),
        ),
        (
            "BUZZARDOS_WINDOW_APP_ID".into(),
            BUZZARDOS_HOST_APP_ID.into(),
        ),
        // Select a known stock wlroots renderer so diagnostics describe the
        // renderer actually requested by this launch rather than inferring it
        // from library availability.
        ("WLR_RENDERER".into(), "gles2".into()),
    ];
    if config.gpus == ["all"]
        && let Some(render_node) = preferred_render_node(host_wayland.dmabuf_main_device)
    {
        service_environment.push((
            "WLR_RENDER_DRM_DEVICE".into(),
            render_node.display().to_string(),
        ));
    }
    if let Some(injection) = &nvidia {
        service_environment.push((
            "BUZZARDOS_NVIDIA_TOOLKIT_VERSION".into(),
            injection.toolkit_version.clone(),
        ));
        service_environment.push((
            "BUZZARDOS_NVIDIA_CDI_DEVICES".into(),
            injection.cdi_devices.join(","),
        ));
        service_environment.extend(injection.environment.iter().cloned());
    }
    write_environment_file(&guest_runtime.join("driver.env"), &service_environment)?;
    let cgroup = MachineCgroup::create(config, unshare)?;
    let id_map = IdMap::discover()?;
    let bwrap = bwrap
        .canonicalize()
        .with_context(|| format!("resolving sandbox helper {}", bwrap.display()))?;
    let rootfs = rootfs
        .canonicalize()
        .with_context(|| format!("resolving machine rootfs {}", rootfs.display()))?;
    let rootfs_descriptor = inherited_bind_descriptor(&rootfs, "machine rootfs")?;
    let broker = std::env::current_exe()
        .context("locating Buzzard OS broker")?
        .canonicalize()
        .context("resolving Buzzard OS broker")?;
    // Bubblewrap opens every host-side bind source before it changes identity,
    // then enters this authorized full subordinate-ID namespace as guest root.
    // Guest UID/GID 0 map to subordinate host IDs (never host root); selecting
    // them here is nevertheless required so Bubblewrap can retain the
    // namespace-local capabilities needed by systemd. Starting as the keep-id
    // user would make Bubblewrap discard those capabilities before the final
    // setpriv transition, which cannot be recovered under no_new_privs.
    let mut user_namespaces = create_mapped_user_namespaces(unshare, &broker, &id_map)?;
    let host_apparmor_access = Path::new("/sys/kernel/security/apparmor/.access");
    let mut command = Command::new(&bwrap);
    command.env_clear();
    command.current_dir("/");
    add_guest_user_namespaces(
        &mut command,
        &user_namespaces.mount_setup.descriptor,
        &user_namespaces.guest.descriptor,
    );
    if !matches!(config.network, NetworkMode::Host) {
        command.arg("--unshare-net");
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
        .arg("--bind-fd")
        .arg(rootfs_descriptor.as_raw_fd().to_string())
        .arg("/");
    add_guest_pseudo_filesystems(&mut command);
    command
        .args([
            "--tmpfs", "/run", "--tmpfs", "/tmp", "--chmod", "1777", "/tmp",
        ])
        .args(["--dir", "/run/systemd/system"])
        .args(["--dir", "/run/systemd/system/buzzardos-desktop.service.d"])
        .arg("--ro-bind")
        .arg(&poweroff_drop_in)
        .arg(
            "/run/systemd/system/buzzardos-desktop.service.d/\
             10-buzzardos-poweroff-marker.conf",
        )
        .args(["--dir", "/shared"])
        .args(["--dir", "/run/buzzardos-host"])
        .args(["--dir", "/run/buzzardos-display-state"])
        .arg("--bind")
        .arg(guest_runtime)
        .arg("/run/buzzardos-host")
        .arg("--ro-bind")
        .arg(display_state)
        .arg("/run/buzzardos-display-state")
        .args(["--ro-bind-try", "/sys", "/sys"])
        .arg("--bind")
        .arg(cgroup.path())
        .arg("/sys/fs/cgroup")
        .arg("--ro-bind")
        .arg(&resolv_conf)
        .arg("/etc/resolv.conf")
        .arg("--ro-bind")
        .arg(&hostname)
        .arg("/etc/hostname")
        .arg("--ro-bind")
        .arg(&hosts)
        .arg("/etc/hosts")
        .arg("--block-fd")
        .arg(block_read.to_string());
    for share in shares {
        command
            .arg(if share.read_only {
                "--ro-bind"
            } else {
                "--bind"
            })
            .arg(&share.source)
            .arg(&share.destination);
    }
    add_bubblewrap_pid_report(&mut command, info_write.as_raw_fd());
    for (name, value) in config.oci.environment_pairs()? {
        command.args(["--setenv", name, value]);
    }
    command
        .args(["--setenv", "container", "buzzardos"])
        .args(["--setenv", "BUZZARDOS_STATUS_DIR", "/run/buzzardos-host"])
        .args(["--setenv", "BUZZARDOS_MACHINE_ID", &config.id.to_string()])
        .args([
            "--setenv",
            "BUZZARDOS_WINDOW_TITLE",
            &format!("Buzzard OS — {}", config.name),
        ])
        .args(["--setenv", "BUZZARDOS_WINDOW_APP_ID", BUZZARDOS_HOST_APP_ID]);

    add_fuse_device(&mut command)?;
    add_gpu_devices(&mut command, config, nvidia.as_ref())?;
    if let Some(injection) = &nvidia {
        injection.apply(&mut command);
    }
    if host_apparmor_access.exists() {
        command
            .arg("--bind-try")
            .arg(host_apparmor_access)
            .arg("/sys/kernel/security/apparmor/.access");
    }
    cgroup.move_command_on_exec(&mut command)?;

    add_guest_init_command(&mut command);
    command.stdin(Stdio::null());

    let mut container = TerminateOnDrop {
        child: command
            .spawn()
            .with_context(|| format!("starting bundled sandbox helper {}", bwrap.display()))?,
    };
    close_fd(block_read);
    drop(info_write);

    let container_pid = read_container_pid(info_read).inspect_err(|_| {
        terminate(&mut container.child);
    })?;
    user_namespaces.release_holders()?;
    state.container_pid = Some(container_pid);
    state.detail = Some("waiting for desktop readiness".into());
    state.save(machine_dir)?;

    let mut network = match config.network {
        NetworkMode::User => match SlirpRuntime::start(
            resources,
            container_pid,
            &host_status.join("slirp-api.sock"),
            user_namespaces.mount_setup.descriptor.as_raw_fd(),
        ) {
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

    match wait_for_desktop(
        &mut container.child,
        &guest_runtime.join("desktop-ready"),
        &session_token,
        &host_status.join("window.json"),
        &host_status.join("presentation.json"),
        host_status,
        machine_dir,
        &config.name,
        container_pid,
        Duration::from_secs(90),
    ) {
        Ok(DesktopWait::Ready) => {}
        Ok(DesktopWait::Exited { status, restart }) => {
            if let Some(mut child) = network.take() {
                terminate(&mut child.process.child);
            }
            cgroup.cleanup();
            return Ok(SessionResult { status, restart });
        }
        Err(error) => {
            if let Some(mut child) = network.take() {
                terminate(&mut child.process.child);
            }
            cgroup.kill_all();
            return Err(error.context("nested compositor did not become ready"));
        }
    }

    state.state = MachineState::Running;
    state.container_pid = Some(container_pid);
    state.detail = Some("systemd and nested compositor ready".into());
    state.display = Some(display_diagnostics(
        host_wayland,
        cgroup.path(),
        container_pid,
        &host_status.join("window.json"),
        &host_status.join("presentation.json"),
    )?);
    state.save(machine_dir)?;
    eprintln!("Buzzard OS desktop '{}' is ready", config.name);

    let mut integrations = IntegrationRuntime::new(guest_runtime, display_state)?;
    let mut integration_snapshot = config.integrations.clone();
    match integrations.reconcile(&config.integrations, network.as_ref(), resources) {
        Ok(diagnostics) => {
            state.integrations = Some(diagnostics);
        }
        Err(error) => {
            state.integrations = Some(integrations.diagnostics(&config.integrations));
            state.detail = Some(format!(
                "desktop ready; live integration will retry after: {error:#}"
            ));
        }
    }
    state.save(machine_dir)?;

    let mut window_snapshot = fs::read(host_status.join("window.json")).ok();
    let mut presentation_snapshot = fs::read(host_status.join("presentation.json")).ok();
    let mut last_diagnostics_refresh = Instant::now();
    let mut last_integration_attempt = Instant::now() - Duration::from_secs(1);
    let mut close_shutdown_requested = false;
    let mut host_action_shutdown_requested = false;
    let mut restart_requested = false;
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
                terminate(&mut child.process.child);
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
                .process
                .child
                .try_wait()
                .context("checking network helper status")?
        {
            terminate(&mut container.child);
            bail!("user-mode network helper exited unexpectedly with {network_status}");
        }
        match MachineConfig::load(machine_dir) {
            Ok(latest) if last_integration_attempt.elapsed() >= Duration::from_secs(1) => {
                last_integration_attempt = Instant::now();
                let config_changed = latest.integrations != integration_snapshot;
                match integrations.reconcile(&latest.integrations, network.as_ref(), resources) {
                    Ok(diagnostics) => {
                        integration_snapshot = latest.integrations;
                        if config_changed || state.integrations.as_ref() != Some(&diagnostics) {
                            state.integrations = Some(diagnostics);
                            state.detail =
                                Some("systemd, desktop, and live integrations ready".into());
                            save_diagnostics_preserving_stop(machine_dir, state)?;
                        }
                    }
                    Err(error) => {
                        // Keep the machine running, but retain the latest
                        // host-requested settings for truthful diagnostics.
                        // `IntegrationRuntime` separately tracks fully applied
                        // state and retries until host and guest converge.
                        integration_snapshot = latest.integrations;
                        eprintln!("Buzzard OS live integration retry pending: {error:#}");
                        state.integrations = Some(integrations.diagnostics(&integration_snapshot));
                        state.detail = Some(format!("live integration rejected: {error:#}"));
                        save_diagnostics_preserving_stop(machine_dir, state)?;
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("Buzzard OS could not reload live integration settings: {error:#}");
            }
        }
        if !host_action_shutdown_requested
            && let Some(request) = take_host_request(host_status, &config.name)?
        {
            match request.as_str() {
                "start" => {}
                "stop" | "restart" => {
                    restart_requested = request == "restart";
                    state.state = MachineState::Stopping;
                    state.detail = Some(if restart_requested {
                        "host application requested an orderly restart".into()
                    } else {
                        "host application requested an orderly shutdown".into()
                    });
                    state.save(machine_dir)?;
                    let result = unsafe { libc::kill(container_pid as i32, libc::SIGRTMIN() + 3) };
                    if result != 0 {
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::ESRCH) {
                            return Err(error)
                                .context("requesting shutdown from native host application");
                        }
                    }
                    host_action_shutdown_requested = true;
                }
                _ => unreachable!("validated host request"),
            }
        }
        let current_window = fs::read(host_status.join("window.json")).ok();
        let current_presentation = fs::read(host_status.join("presentation.json")).ok();
        if current_window != window_snapshot
            || current_presentation != presentation_snapshot
            || last_diagnostics_refresh.elapsed() >= Duration::from_secs(2)
        {
            let diagnostics = display_diagnostics(
                host_wayland,
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
            let integration_diagnostics = integrations.diagnostics(&integration_snapshot);
            let integration_changed = state
                .integrations
                .as_ref()
                .and_then(|previous| serde_json::to_vec(previous).ok())
                != serde_json::to_vec(&integration_diagnostics).ok();
            if changed || integration_changed {
                state.display = Some(diagnostics);
                state.integrations = Some(integration_diagnostics);
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
        terminate(&mut child.process.child);
    }
    cgroup.cleanup();
    Ok(SessionResult {
        status,
        restart: restart_requested,
    })
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
                    .map(|metadata| {
                        metadata.rdev() == main_device
                            || drm_devices_share_backing_device(
                                main_device,
                                metadata.rdev(),
                                Path::new("/sys/dev/char"),
                            )
                    })
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

#[allow(clippy::too_many_arguments)]
fn wait_for_desktop(
    container: &mut Child,
    marker: &Path,
    expected_session_token: &str,
    _window_marker: &Path,
    presentation_marker: &Path,
    host_status: &Path,
    machine_dir: &Path,
    machine_name: &str,
    container_pid: u32,
    timeout: Duration,
) -> Result<DesktopWait> {
    let deadline = Instant::now() + timeout;
    let mut requested_restart = None;
    loop {
        let desktop_ready = desktop_ready_for_session(marker, expected_session_token)?;
        if desktop_ready && presentation_has_first_frame(presentation_marker) {
            return Ok(DesktopWait::Ready);
        }
        if requested_restart.is_none()
            && let Some(request) = take_host_request(host_status, machine_name)?
        {
            match request.as_str() {
                "start" => {}
                "stop" | "restart" => {
                    let restart = request == "restart";
                    let mut state = RuntimeState::load(machine_dir)?
                        .unwrap_or_else(|| RuntimeState::new(MachineState::Stopping));
                    state.state = MachineState::Stopping;
                    state.detail = Some(if restart {
                        "host application cancelled startup for an orderly restart".into()
                    } else {
                        "host application cancelled startup".into()
                    });
                    state.save(machine_dir)?;
                    let result = unsafe { libc::kill(container_pid as i32, libc::SIGRTMIN() + 3) };
                    if result != 0 {
                        let error = std::io::Error::last_os_error();
                        if error.raw_os_error() != Some(libc::ESRCH) {
                            return Err(error).context("cancelling desktop startup");
                        }
                    }
                    requested_restart = Some(restart);
                }
                _ => unreachable!("validated host request"),
            }
        }
        if let Some(status) = container
            .try_wait()
            .context("checking systemd container readiness")?
        {
            if requested_restart.is_some()
                || RuntimeState::load(machine_dir)?
                    .is_some_and(|state| state.state == MachineState::Stopping)
            {
                return Ok(DesktopWait::Exited {
                    status,
                    restart: requested_restart.unwrap_or(false),
                });
            }
            bail!("container exited with {status} before the desktop compositor became ready");
        }
        if Instant::now() >= deadline {
            terminate(container);
            if desktop_ready {
                return Err(DesktopFrameReadinessDeadline {
                    seconds: timeout.as_secs(),
                }
                .into());
            }
            return Err(DesktopReadinessDeadline {
                seconds: timeout.as_secs(),
            }
            .into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn presentation_has_first_frame(path: &Path) -> bool {
    read_presentation_diagnostics(path).is_some_and(|frame| {
        frame.transport == "dmabuf" && frame.submitted_frames > 0 && frame.painted_frames > 0
    })
}

fn desktop_ready_for_session(path: &Path, expected_session_token: &str) -> Result<bool> {
    if expected_session_token.len() != SESSION_TOKEN_BYTES
        || !expected_session_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("broker generated an invalid desktop session token");
    }
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("opening desktop readiness marker {}", path.display()));
        }
    };
    let before = file
        .metadata()
        .with_context(|| format!("inspecting desktop readiness marker {}", path.display()))?;
    if !before.is_file()
        || before.nlink() != 1
        || before.uid() != Uid::effective().as_raw()
        || before.gid() != nix::unistd::Gid::effective().as_raw()
        || before.permissions().mode() & 0o777 != DESKTOP_READY_MODE
        || before.len() > MAX_DESKTOP_READY_BYTES
    {
        bail!(
            "desktop readiness marker {} has unsafe ownership, type, links, size, or mode",
            path.display()
        );
    }
    let mut contents = Vec::with_capacity(before.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_DESKTOP_READY_BYTES + 1)
        .read_to_end(&mut contents)
        .with_context(|| format!("reading desktop readiness marker {}", path.display()))?;
    let after = file
        .metadata()
        .with_context(|| format!("rechecking desktop readiness marker {}", path.display()))?;
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        bail!(
            "desktop readiness marker {} changed while being read",
            path.display()
        );
    }
    let mut expected = expected_session_token.as_bytes().to_vec();
    expected.push(b'\n');
    Ok(contents == expected)
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

fn add_bubblewrap_pid_report(command: &mut Command, info_fd: RawFd) {
    // `--info-fd` is deliberately one-shot: Bubblewrap writes one JSON object
    // containing the child PID after setup. Some supported Bubblewrap builds
    // retain their copy of this descriptor for the sandbox lifetime, so the
    // reader must stop at the end of that first object rather than waiting for
    // EOF. Do not replace this with `--json-status-fd`: that interface is a
    // lifecycle stream rather than the single startup record needed here.
    command.arg("--info-fd").arg(info_fd.to_string());
}

fn read_container_pid(info_read: RawFd) -> Result<u32> {
    // SAFETY: pipe() returned this new descriptor and ownership is transferred
    // to the File exactly once here.
    let file = unsafe { fs::File::from_raw_fd(info_read) };
    // Deserialize exactly one bounded JSON value. `serde_json::from_reader`
    // additionally waits for EOF to reject trailing data, which deadlocks
    // against Bubblewrap versions that keep `--info-fd` open while their child
    // is paused at `--block-fd`.
    let mut deserializer =
        serde_json::Deserializer::from_reader(file.take(MAX_BUBBLEWRAP_INFO_BYTES));
    let value = serde_json::Value::deserialize(&mut deserializer)
        .context("parsing container information")?;
    value
        .get("child-pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .context("sandbox information did not contain a valid child-pid")
}

struct TerminateOnDrop {
    child: Child,
}

impl Drop for TerminateOnDrop {
    fn drop(&mut self) {
        // `terminate` reaps the child. Several revocation paths deliberately
        // terminate before removing an owned runtime record; do not signal the
        // stale numeric PID again if the child was already reaped.
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => terminate(&mut self.child),
        }
    }
}

struct LifecycleRuntime {
    _guard: Option<tempfile::TempDir>,
    guest: PathBuf,
    host_status: PathBuf,
    display_state: PathBuf,
}

impl LifecycleRuntime {
    fn create() -> Result<Self> {
        let temporary = tempfile::Builder::new()
            .prefix("buzzardos-runtime-")
            .tempdir()
            .context("creating lifecycle runtime directory")?;
        let root = temporary.path().to_path_buf();
        let guard = Some(temporary);
        let guest = root.join("guest");
        let host_status = root.join("host-status");
        let display_state = root.join("display-state");
        fs::create_dir(&guest).context("creating guest runtime directory")?;
        fs::create_dir(&host_status).context("creating host display status directory")?;
        fs::create_dir(&display_state).context("creating display state directory")?;
        // The source is owned by the host desktop UID/GID, which the IdMap
        // keeps as guest UID/GID 1000. Namespace root retains DAC override for
        // this mapped inode, so Bubblewrap and systemd can traverse 0700,
        // while unrelated guest service UIDs cannot enumerate or open known
        // runtime files. Sway, output-sync, and the clipboard agent all run as
        // the mapped interactive owner.
        fs::set_permissions(&guest, fs::Permissions::from_mode(GUEST_RUNTIME_MODE))
            .context("setting guest runtime permissions")?;
        fs::set_permissions(&host_status, fs::Permissions::from_mode(0o700))
            .context("setting host display status permissions")?;
        fs::set_permissions(&display_state, fs::Permissions::from_mode(0o755))
            .context("setting display state permissions")?;
        Ok(Self {
            _guard: guard,
            guest,
            host_status,
            display_state,
        })
    }
}

struct SessionResult {
    status: ExitStatus,
    restart: bool,
}

enum DesktopWait {
    Ready,
    Exited { status: ExitStatus, restart: bool },
}

#[derive(Deserialize)]
struct HostRequest {
    schema: u32,
    action: String,
    machine: String,
}

fn clear_session_runtime(runtime: &LifecycleRuntime) -> Result<()> {
    // The native host window and its lifecycle runtime intentionally outlive
    // one guest systemd session. Integration relays do not: their Unix socket
    // directory is recreated for every boot. Remove the previous confined
    // directory before the new namespace can start, while no guest process can
    // race this cleanup. A replaced symlink is unlinked rather than followed.
    let reverse = runtime.guest.join("reverse");
    match fs::symlink_metadata(&reverse) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            fs::remove_file(&reverse)
                .with_context(|| format!("clearing session relay path {}", reverse.display()))?;
        }
        Ok(_) => {
            fs::remove_dir_all(&reverse).with_context(|| {
                format!("clearing session relay directory {}", reverse.display())
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspecting session relay path {}", reverse.display()));
        }
    }
    clear_clipboard_session_runtime(runtime)?;
    for relative in [
        "desktop-ready",
        "guest-poweroff-requested",
        "resolv.conf",
        "hostname",
        "hosts",
        "initial-output.conf",
        "buzzardos-desktop-poweroff-marker.conf",
        "driver.env",
    ] {
        let path = runtime.guest.join(relative);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("clearing session file {}", path.display()));
            }
        }
    }
    let request = runtime.host_status.join("host-request.json");
    match fs::remove_file(&request) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("clearing lifecycle request {}", request.display()));
        }
    }
    let presentation = runtime.host_status.join("presentation.json");
    match fs::remove_file(&presentation) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "clearing previous presentation state {}",
                    presentation.display()
                )
            });
        }
    }
    let staged_cgroup = runtime.host_status.join("cgroup");
    match fs::remove_dir(&staged_cgroup) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "clearing previous staged cgroup directory {}",
                    staged_cgroup.display()
                )
            });
        }
    }
    Ok(())
}

fn clear_clipboard_session_runtime(runtime: &LifecycleRuntime) -> Result<()> {
    for relative in ["clipboard-ready", "clipboard-agent.sock"] {
        let path = runtime.guest.join(relative);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("revoking session clipboard path {}", path.display())
                });
            }
        }
    }
    Ok(())
}

fn combine_session_and_clipboard_cleanup(
    session: Result<SessionResult>,
    cleanup: Result<()>,
) -> Result<SessionResult> {
    match (session, cleanup) {
        (Ok(session), Ok(())) => Ok(session),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup.context("revoking the ended clipboard session")),
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "clipboard endpoint revocation also failed after the session error: {cleanup:#}"
        ))),
    }
}

fn take_host_request(status_dir: &Path, machine: &str) -> Result<Option<String>> {
    let path = status_dir.join("host-request.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("reading host request {}", path.display()));
        }
    };
    fs::remove_file(&path).with_context(|| format!("consuming host request {}", path.display()))?;
    let request: HostRequest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing host request {}", path.display()))?;
    if request.schema != 1 {
        bail!("unsupported host request schema {}", request.schema);
    }
    if request.machine != machine {
        bail!(
            "host request targets machine '{}' instead of '{}'",
            request.machine,
            machine
        );
    }
    if !matches!(request.action.as_str(), "start" | "stop" | "restart") {
        bail!("unsupported host lifecycle request '{}'", request.action);
    }
    Ok(Some(request.action))
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
    dmabuf_version: u32,
    sync_drm_device: Option<&Path>,
) -> Result<TerminateOnDrop> {
    let control_socket = host_control_socket(paths.machine_dir)?;
    prepare_host_control_directory(&control_socket)?;
    let helper = resources.helper_or_path("buzzardos-display")?;
    let xkb_config_root = Path::new("/usr/share/X11/xkb")
        .canonicalize()
        .context("resolving distro xkb-data directory /usr/share/X11/xkb")?;
    let private_socket = paths.guest_runtime.join("wayland-0");
    let guest_scale_control = paths.guest_runtime.join("display-scale-host.sock");
    let guest_clipboard_control = paths.guest_runtime.join("clipboard-agent.sock");
    let mut command = Command::new(&helper);
    command
        .arg("--host")
        .arg(paths.host_wayland)
        .arg("--listen")
        .arg(&private_socket)
        .arg("--control")
        .arg(&control_socket)
        .arg("--guest-scale-control")
        .arg(&guest_scale_control)
        .arg("--guest-clipboard-control")
        .arg(&guest_clipboard_control)
        .arg("--dmabuf-version")
        .arg(dmabuf_version.to_string())
        .arg("--xkb-config-root")
        .arg(&xkb_config_root)
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
        .arg(machine_window_app_id(&config.name));
    if let Some(scale) = config.guest_scale_120 {
        command.arg("--guest-scale-120").arg(scale.to_string());
    }
    if let Some(sync_drm_device) = sync_drm_device {
        command.arg("--sync-drm-device").arg(sync_drm_device);
    }
    if let Some(scale) = std::env::var_os("BUZZARDOS_TEST_FRACTIONAL_SCALE_120") {
        command.arg("--test-fractional-scale-120").arg(scale);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting display gateway {}", helper.display()))?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let display_metadata = fs::symlink_metadata(&private_socket);
        let control_metadata = fs::symlink_metadata(&control_socket);
        let scale_metadata = fs::symlink_metadata(&guest_scale_control);
        match (display_metadata, control_metadata, scale_metadata) {
            (Ok(display), Ok(control), Ok(scale))
                if !display.file_type().is_symlink()
                    && display.file_type().is_socket()
                    && !control.file_type().is_symlink()
                    && control.file_type().is_socket()
                    && !scale.file_type().is_symlink()
                    && scale.file_type().is_socket() =>
            {
                return Ok(TerminateOnDrop { child });
            }
            (Ok(_), Ok(_), Ok(_)) => {
                terminate(&mut child);
                bail!(
                    "display gateway created an invalid display, host-control, or guest-scale socket"
                );
            }
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error))
                if error.kind() == std::io::ErrorKind::NotFound => {}
            (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
                terminate(&mut child);
                return Err(error).context("inspecting display gateway sockets");
            }
        }
        if let Some(status) = child
            .try_wait()
            .context("checking display gateway startup")?
        {
            bail!("display gateway exited with {status}");
        }
        if Instant::now() >= deadline {
            terminate(&mut child);
            bail!("display gateway did not create its private socket");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn machine_window_app_id(_machine_name: &str) -> String {
    BUZZARDOS_HOST_APP_ID.to_owned()
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

/// Expose only the kernel FUSE character device required by native Type-2
/// AppImages. The guest still receives a freshly constructed `/dev`; no other
/// host device becomes visible through this integration.
fn add_fuse_device(command: &mut Command) -> Result<()> {
    add_fuse_device_at(command, Path::new("/dev/fuse"))
}

fn validate_fuse_device_identity(
    source: &Path,
    is_symlink: bool,
    is_character_device: bool,
    device: u64,
) -> Result<()> {
    if is_symlink || !is_character_device {
        bail!("{} must be a real character device", source.display());
    }
    let major = libc::major(device);
    let minor = libc::minor(device);
    if major != 10 || minor != 229 {
        bail!(
            "{} has device identity {major}:{minor}; expected the Linux FUSE device 10:229",
            source.display()
        );
    }
    Ok(())
}

fn add_fuse_device_at(command: &mut Command, source: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!(
                "required FUSE device {} is missing; native Type-2 AppImage support requires /dev/fuse (character device 10:229)",
                source.display()
            );
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", source.display()));
        }
    };
    validate_fuse_device_identity(
        source,
        metadata.file_type().is_symlink(),
        metadata.file_type().is_char_device(),
        metadata.rdev(),
    )?;
    command.arg("--dev-bind").arg(source).arg("/dev/fuse");
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

    let guest_library_dir = Path::new("/run/buzzardos-host/driver/lib");
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
            PathBuf::from("/run/buzzardos-host/driver/gbm/nvidia-drm_gbm.so"),
        ));
    }

    let image_library_path = config
        .oci
        .environment_pairs()?
        .into_iter()
        .find_map(|(name, value)| (name == "LD_LIBRARY_PATH").then_some(value));
    let injected_library_path = match image_library_path {
        Some(value) if !value.is_empty() => {
            format!("{}:{value}", guest_library_dir.display())
        }
        _ => guest_library_dir.display().to_string(),
    };
    let mut environment = cdi.environment;
    environment.push(("LD_LIBRARY_PATH".into(), injected_library_path));
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
    let nvidia_icd_guest = PathBuf::from("/run/buzzardos-host/driver/nvidia_icd.json");
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
        .helper("nvidia-ctk")
        .context("locating bundled NVIDIA Container Toolkit")?;
    let toolkit_directory = toolkit
        .parent()
        .context("bundled NVIDIA Container Toolkit has no parent directory")?;
    let toolkit_path = std::env::join_paths([
        toolkit_directory,
        Path::new("/usr/sbin"),
        Path::new("/usr/bin"),
        Path::new("/sbin"),
        Path::new("/bin"),
    ])
    .context("constructing the bounded NVIDIA toolkit helper path")?;
    let spec_path = runtime.join("nvidia-cdi.json");
    let mut command = Command::new(&toolkit);
    command
        .env_clear()
        .env("PATH", &toolkit_path)
        .args(["cdi", "generate", "--format", "json"])
        .arg("--output")
        .arg(&spec_path)
        .args(["--disable-hook", "all"]);
    let output = command
        .output()
        .with_context(|| format!("running bundled NVIDIA toolkit {}", toolkit.display()))?;
    if !output.status.success() {
        let mut detail = Vec::new();
        detail.extend_from_slice(&output.stdout);
        detail.extend_from_slice(&output.stderr);
        bail!(
            "bundled NVIDIA toolkit failed to generate CDI ({})\n{}",
            output.status,
            String::from_utf8_lossy(&detail)
        );
    }

    let version_output = Command::new(&toolkit)
        .env_clear()
        .env("PATH", &toolkit_path)
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
    validate_nvidia_toolkit_version(&toolkit_version)?;

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
        "Buzzard OS NVIDIA CDI: toolkit={toolkit_version}, devices={}",
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

fn validate_nvidia_toolkit_version(version_line: &str) -> Result<()> {
    let version = version_line
        .strip_prefix("NVIDIA Container Toolkit CLI version ")
        .with_context(|| format!("unexpected NVIDIA toolkit version output '{version_line}'"))?;
    let mut components = version.split('.');
    let major = components
        .next()
        .context("NVIDIA toolkit version has no major component")?
        .parse::<u32>()
        .context("NVIDIA toolkit major version is invalid")?;
    let minor = components
        .next()
        .context("NVIDIA toolkit version has no minor component")?
        .parse::<u32>()
        .context("NVIDIA toolkit minor version is invalid")?;
    let patch = components
        .next()
        .context("NVIDIA toolkit version has no patch component")?
        .parse::<u32>()
        .context("NVIDIA toolkit patch version is invalid")?;
    if components.next().is_some() {
        bail!("NVIDIA toolkit version '{version}' is not semantic version x.y.z");
    }
    if major != 1 || minor < 19 || (minor == 19 && patch < 1) {
        bail!(
            "unsupported NVIDIA Container Toolkit {version}; Buzzard OS requires version 1.19.1 or newer within the compatible 1.x series"
        );
    }
    Ok(())
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
        || path == Path::new("/etc/OpenCL/vendors/nvidia.icd")
        || path
            .strip_prefix("/usr/bin")
            .ok()
            .and_then(Path::file_name)
            .is_some_and(|name| name.to_string_lossy().starts_with("nvidia-"))
        || path == Path::new("/usr/sbin/nvidia-cuda-mps-server")
        || path == Path::new("/usr/share/X11/xorg.conf.d/10-nvidia.conf")
        || path == Path::new("/usr/share/X11/xorg.conf.d/nvidia-drm-outputclass.conf")
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
        || path == Path::new("/etc/OpenCL/vendors/nvidia.icd")
        || path
            .strip_prefix("/usr/bin")
            .ok()
            .and_then(Path::file_name)
            .is_some_and(|name| name.to_string_lossy().starts_with("nvidia-"))
        || path == Path::new("/usr/sbin/nvidia-cuda-mps-server")
        || path == Path::new("/usr/share/X11/xorg.conf.d/10-nvidia.conf")
        || path == Path::new("/usr/share/X11/xorg.conf.d/nvidia-drm-outputclass.conf")
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
            "/run/buzzardos-host/driver/gbm".into(),
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
                    .map(|metadata| {
                        metadata.rdev() == device
                            || drm_devices_share_backing_device(
                                device,
                                metadata.rdev(),
                                Path::new("/sys/dev/char"),
                            )
                    })
                    .unwrap_or(false)
        })
}

/// Select the renderer identified by linux-dmabuf feedback when available.
/// The deterministic DRM scan remains useful for diagnostics, but launch
/// compatibility is validated separately and never treats that scan as a
/// replacement for the host compositor's v4 main-device feedback.
fn preferred_render_node(main_device: Option<u64>) -> Option<PathBuf> {
    render_node_for_device(main_device).or_else(|| {
        let mut nodes = fs::read_dir("/dev/dri")
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.strip_prefix("renderD")
                            .is_some_and(|minor| minor.parse::<u32>().is_ok())
                    })
                    && fs::metadata(path).is_ok_and(|metadata| {
                        metadata.file_type().is_char_device() && libc::major(metadata.rdev()) == 226
                    })
            })
            .collect::<Vec<_>>();
        nodes.sort_by_key(|path| {
            fs::metadata(path)
                .map(|metadata| libc::minor(metadata.rdev()))
                .unwrap_or(u32::MAX)
        });
        nodes
            .iter()
            .find(|path| render_node_is_boot_vga(path))
            .cloned()
            .or_else(|| nodes.into_iter().next())
    })
}

fn render_node_is_boot_vga(render_node: &Path) -> bool {
    render_node
        .file_name()
        .and_then(|name| {
            fs::read_to_string(
                Path::new("/sys/class/drm")
                    .join(name)
                    .join("device/boot_vga"),
            )
            .ok()
        })
        .is_some_and(|value| value.trim() == "1")
}

fn private_dmabuf_version(host: &WaylandCapabilities) -> Result<u32> {
    if !host.linux_dmabuf || host.linux_dmabuf_version < 4 {
        bail!(
            "host display does not provide the required accelerated graphics contract: zwp_linux_dmabuf_v1 version 4 or newer is required; enable DRM render-node and 3D acceleration support"
        );
    }
    if host.dmabuf_main_device.is_none() {
        bail!(
            "host display does not provide the required accelerated graphics contract: linux-dmabuf feedback supplied no DRM main device; enable DRM render-node and 3D acceleration support"
        );
    }
    Ok(4)
}

/// Linux-dmabuf feedback is allowed to identify a DRM primary node even when
/// clients must open that GPU's render node.  The two nodes have different
/// `dev_t` values but their sysfs `device` links resolve to the same backing
/// GPU.  Comparing only `st_rdev` therefore drops the render node on Mutter
/// configurations that report `cardN` as the feedback main device.
fn drm_devices_share_backing_device(first: u64, second: u64, sysfs_char: &Path) -> bool {
    let backing = |device| {
        sysfs_char
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
        let path = base.join(format!("buzzardos-{}", config.id.simple()));
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

    fn kill_all(&self) {
        let _ = fs::write(self.path.join("cgroup.kill"), b"1\n");
    }
}

impl Drop for MachineCgroup {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn cleanup_cgroup_with_id_map(unshare: &Path, path: &Path) -> Result<()> {
    let id_map = IdMap::discover()?;
    let namespace_program = id_map.namespace_program(unshare)?;
    let broker = std::env::current_exe().context("locating broker for cgroup cleanup")?;
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.namespace_args())
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
        .strip_prefix("buzzardos-")
        .context("cgroup cleanup target is not a Buzzard OS cgroup")?;
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

struct PrivateNetworkSandbox<'a> {
    bwrap: &'a Path,
    host_network: bool,
    private_bind_root: &'a Path,
    private_binds: &'a [String],
    apparmor_access: Option<&'a Path>,
    cgroup_source: &'a Path,
    cgroup_staged: &'a Path,
    arguments: &'a [OsString],
}

fn run_private_network_sandbox(config: PrivateNetworkSandbox<'_>) -> Result<()> {
    let PrivateNetworkSandbox {
        bwrap,
        host_network,
        private_bind_root,
        private_binds,
        apparmor_access,
        cgroup_source,
        cgroup_staged,
        arguments,
    } = config;
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

    let namespace_flags = if host_network {
        libc::CLONE_NEWNS
    } else {
        libc::CLONE_NEWNS | libc::CLONE_NEWNET
    };
    let result = unsafe { libc::unshare(namespace_flags) };
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

    install_private_bind_aliases(private_bind_root, private_binds)?;

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

    if !host_network {
        mount_raw(
            Some(Path::new("sysfs")),
            Path::new("/sys"),
            Some("sysfs"),
            libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            None,
        )
        .context("mounting network-namespace-owned sysfs")?;
    }

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

fn install_private_bind_aliases(root: &Path, specifications: &[String]) -> Result<()> {
    let root_value = root.to_string_lossy();
    if !root_value.starts_with("/tmp/buzzardos-private-binds-") || !safe_absolute_path(root) {
        bail!("private bind root is outside the fixed /tmp namespace");
    }
    let root_metadata = fs::symlink_metadata(root)
        .with_context(|| format!("inspecting private bind root {}", root.display()))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        bail!("private bind root must be a real directory");
    }

    for specification in specifications {
        let mut fields = specification.split(':');
        let descriptor = fields
            .next()
            .context("private bind has no descriptor")?
            .parse::<RawFd>()
            .context("private bind descriptor is invalid")?;
        let kind = fields.next().context("private bind has no kind")?;
        let name = fields.next().context("private bind has no name")?;
        if fields.next().is_some()
            || descriptor < 3
            || !name.starts_with("source-")
            || !name["source-".len()..]
                .bytes()
                .all(|byte| byte.is_ascii_digit())
            || !matches!(kind, "d" | "f")
        {
            bail!("invalid private bind specification '{specification}'");
        }
        if unsafe { libc::fcntl(descriptor, libc::F_GETFD) } < 0 {
            return Err(std::io::Error::last_os_error())
                .context("private bind descriptor is not inherited");
        }
        let target = root.join(name);
        let target_metadata = fs::symlink_metadata(&target)?;
        if target_metadata.file_type().is_symlink()
            || (kind == "d" && !target_metadata.is_dir())
            || (kind == "f" && !target_metadata.is_file())
        {
            bail!("private bind target {name} has the wrong type");
        }
        let source = PathBuf::from(format!("/proc/self/fd/{descriptor}"));
        mount_raw(
            Some(&source),
            &target,
            None,
            libc::MS_BIND | if kind == "d" { libc::MS_REC } else { 0 },
            None,
        )
        .with_context(|| format!("installing private bind alias {name}"))?;
    }
    Ok(())
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
        .context("XDG_RUNTIME_DIR is not set; launch Buzzard OS from a Wayland session")?;
    let display = std::env::var_os("WAYLAND_DISPLAY")
        .context("WAYLAND_DISPLAY is not set; launch Buzzard OS from a Wayland session")?;
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

fn inherited_bind_descriptor(path: &Path, label: &str) -> Result<File> {
    let descriptor = File::open(path)
        .with_context(|| format!("opening {label} {} for a pinned bind", path.display()))?;
    let fd = descriptor.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "making the pinned {label} descriptor inheritable for {}",
                path.display()
            )
        });
    }
    Ok(descriptor)
}

fn add_guest_user_namespaces(command: &mut Command, mount_setup: &File, guest: &File) {
    command
        .arg("--userns")
        .arg(mount_setup.as_raw_fd().to_string())
        .arg("--userns2")
        .arg(guest.as_raw_fd().to_string())
        .args(["--uid", "0", "--gid", "0"]);
}

fn add_guest_init_command(command: &mut Command) {
    // Bubblewrap constructs the host-backed mount tree while it is in the
    // keep-id setup namespace, then `--userns2` enters the nested subordinate
    // guest namespace.  A mount namespace remains owned by the user namespace
    // in which it was created, so guest root cannot perform even private
    // systemd/FUSE mounts in Bubblewrap's setup mount namespace.  Create the
    // final mount namespace only after entering the guest user namespace.  It
    // inherits the already prepared flat-rootfs tree, is recursively private,
    // and grants no mount authority over either the setup namespace or host.
    command.arg("--").args([
        "/usr/bin/unshare",
        "--mount",
        "--propagation",
        "private",
        "/usr/bin/setpriv",
        "--reuid=0",
        "--regid=0",
        "--clear-groups",
        "/usr/lib/buzzardos/runtime/current/libexec/buzzardos-init",
    ]);
}

fn validate_machine_layout(machine_dir: &Path, rootfs: &Path) -> Result<()> {
    if rootfs.parent() != Some(machine_dir)
        || rootfs.file_name().and_then(|name| name.to_str()) != Some("rootfs")
    {
        bail!("machine rootfs must be the machine directory's direct rootfs directory");
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

#[derive(Debug)]
struct ResolvedShare {
    source: PathBuf,
    destination: PathBuf,
    read_only: bool,
}

fn resolve_shared_paths(config: &MachineConfig) -> Result<Vec<ResolvedShare>> {
    MachineConfig::validate_shares(&config.shares)?;
    config
        .shares
        .iter()
        .map(|share| {
            let metadata = fs::symlink_metadata(&share.host_path)
                .with_context(|| format!("inspecting shared path {}", share.host_path.display()))?;
            if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
                bail!(
                    "shared path must be a regular file or real directory: {}",
                    share.host_path.display()
                );
            }
            let source = share
                .host_path
                .canonicalize()
                .with_context(|| format!("resolving shared path {}", share.host_path.display()))?;
            Ok(ResolvedShare {
                source,
                destination: share.guest_path(),
                read_only: share.read_only,
            })
        })
        .collect()
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

fn hold_user_namespace(ready_fd: RawFd, release_fd: RawFd) -> Result<()> {
    if ready_fd < 3 || release_fd < 3 || ready_fd == release_fd {
        bail!("user namespace holder received invalid descriptors");
    }
    // SAFETY: these descriptors are transferred exclusively to this hidden
    // helper by the broker parent.
    let mut ready = unsafe { File::from_raw_fd(ready_fd) };
    // SAFETY: see above.
    let mut release = unsafe { File::from_raw_fd(release_fd) };
    ready.write_all(&[1])?;
    drop(ready);
    let mut signal = [0_u8; 1];
    release.read_exact(&mut signal)?;
    Ok(())
}

fn hold_nested_user_namespace(ready_fd: RawFd, release_fd: RawFd) -> Result<()> {
    if ready_fd < 3 || release_fd < 3 || ready_fd == release_fd {
        bail!("nested user namespace holder received invalid descriptors");
    }
    let holder_pid = std::process::id();
    let (start_read, mut start_write) = pipe().context("creating nested-map start pipe")?;
    let (done_read, done_write) = pipe().context("creating nested-map completion pipe")?;
    let broker = std::env::current_exe().context("locating nested namespace mapper")?;
    let mut mapper_command = Command::new(&broker);
    mapper_command
        .arg("__map-nested-user-namespace")
        .arg("--holder-pid")
        .arg(holder_pid.to_string())
        .arg("--start-fd")
        .arg(start_read.to_string())
        .arg("--done-fd")
        .arg(done_write.as_raw_fd().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    let unused_start_write = start_write.as_raw_fd();
    // SAFETY: only async-signal-safe close calls run between fork and exec.
    unsafe {
        use std::os::unix::process::CommandExt;
        mapper_command.pre_exec(move || {
            libc::close(unused_start_write);
            libc::close(done_read);
            Ok(())
        });
    }
    let mut mapper = mapper_command
        .spawn()
        .context("starting nested user namespace mapper")?;
    close_fd(start_read);
    drop(done_write);

    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        terminate(&mut mapper);
        return Err(std::io::Error::last_os_error())
            .context("creating nested guest user namespace");
    }
    start_write
        .write_all(&[1])
        .context("releasing nested user namespace mapper")?;
    drop(start_write);
    // SAFETY: the parent owns the read end returned by `pipe`.
    let mut done_read = unsafe { File::from_raw_fd(done_read) };
    let mut signal = [0_u8; 1];
    if let Err(error) = done_read.read_exact(&mut signal) {
        terminate(&mut mapper);
        return Err(error).context("waiting for nested guest identity mapping");
    }
    let status = mapper
        .wait()
        .context("reaping nested user namespace mapper")?;
    if !status.success() {
        bail!("nested user namespace mapper exited with {status}");
    }
    if unsafe { libc::setresgid(0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("becoming guest root group in nested user namespace");
    }
    if unsafe { libc::setresuid(0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("becoming guest root in nested user namespace");
    }
    hold_user_namespace(ready_fd, release_fd)
}

fn map_nested_user_namespace(holder_pid: u32, start_fd: RawFd, done_fd: RawFd) -> Result<()> {
    if holder_pid != unsafe { libc::getppid() as u32 }
        || start_fd < 3
        || done_fd < 3
        || start_fd == done_fd
    {
        bail!("nested user namespace mapper received invalid authority");
    }
    // SAFETY: these descriptors are transferred exclusively to this hidden
    // helper by its direct parent.
    let mut start = unsafe { File::from_raw_fd(start_fd) };
    // SAFETY: see above.
    let mut done = unsafe { File::from_raw_fd(done_fd) };
    let mut signal = [0_u8; 1];
    start
        .read_exact(&mut signal)
        .context("waiting for nested namespace creation")?;
    let process = PathBuf::from(format!("/proc/{holder_pid}"));
    fs::write(
        process.join("uid_map"),
        b"0 1 1000\n1000 0 1\n1001 1001 64535\n",
    )
    .context("installing nested guest UID map")?;
    fs::write(
        process.join("gid_map"),
        b"0 1 1000\n1000 0 1\n1001 1001 64535\n",
    )
    .context("installing nested guest GID map")?;
    done.write_all(&[1])
        .context("reporting nested guest identity mapping")?;
    Ok(())
}

struct MappedUserNamespace {
    descriptor: File,
    holder: Option<Child>,
    release: Option<File>,
}

struct MappedUserNamespaces {
    mount_setup: MappedUserNamespace,
    guest: MappedUserNamespace,
}

impl MappedUserNamespaces {
    fn release_holders(&mut self) -> Result<()> {
        self.guest.release_holder()?;
        self.mount_setup.release_holder()
    }
}

impl MappedUserNamespace {
    fn release_holder(&mut self) -> Result<()> {
        if let Some(mut release) = self.release.take() {
            release.write_all(&[1])?;
        }
        if let Some(mut holder) = self.holder.take() {
            let status = holder
                .wait()
                .context("reaping mapped user namespace holder")?;
            if !status.success() {
                bail!("mapped user namespace holder exited with {status}");
            }
        }
        Ok(())
    }
}

impl Drop for MappedUserNamespace {
    fn drop(&mut self) {
        if self.release_holder().is_err()
            && let Some(holder) = &mut self.holder
        {
            terminate(holder);
        }
    }
}

fn create_mapped_user_namespaces(
    unshare: &Path,
    broker: &Path,
    id_map: &IdMap,
) -> Result<MappedUserNamespaces> {
    let mount_setup = create_held_user_namespace(
        unshare,
        broker,
        id_map,
        id_map.mount_setup_namespace_args(),
        None,
    )?;
    let guest = create_held_nested_user_namespace(broker, mount_setup.descriptor.as_raw_fd())?;
    Ok(MappedUserNamespaces { mount_setup, guest })
}

fn create_held_nested_user_namespace(
    broker: &Path,
    parent_user_namespace: RawFd,
) -> Result<MappedUserNamespace> {
    let (ready_read, ready_write) = pipe()?;
    let (release_read, release_write) = pipe()?;
    let mut command = Command::new(broker);
    command
        .arg("__hold-nested-user-namespace")
        .arg("--ready-fd")
        .arg(ready_write.as_raw_fd().to_string())
        .arg("--release-fd")
        .arg(release_read.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: setns and close are async-signal-safe. The descriptor is a
    // validated user-namespace fd owned by the immediately preceding held
    // setup namespace and is inherited into this child only.
    unsafe {
        command.pre_exec(move || {
            if libc::setns(parent_user_namespace, libc::CLONE_NEWUSER) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(parent_user_namespace);
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .context("creating nested mapped user namespace")?;
    drop(ready_write);
    close_fd(release_read);
    // SAFETY: the parent owns the read end returned by `pipe`.
    let mut ready_read = unsafe { File::from_raw_fd(ready_read) };
    let mut signal = [0_u8; 1];
    if let Err(error) = ready_read.read_exact(&mut signal) {
        terminate(&mut child);
        let mut diagnostic = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut diagnostic);
        }
        if diagnostic.trim().is_empty() {
            return Err(error).context("waiting for nested mapped user namespace");
        }
        bail!(
            "waiting for nested mapped user namespace: {error}: {}",
            diagnostic.trim()
        );
    }
    let namespace_path = PathBuf::from(format!("/proc/{}/ns/user", child.id()));
    let namespace = File::open(&namespace_path).with_context(|| {
        format!(
            "opening nested mapped user namespace {}",
            namespace_path.display()
        )
    })?;
    let fd = namespace.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        terminate(&mut child);
        return Err(std::io::Error::last_os_error())
            .context("making nested mapped user namespace descriptor inheritable");
    }
    Ok(MappedUserNamespace {
        descriptor: namespace,
        holder: Some(child),
        release: Some(release_write),
    })
}

fn create_held_user_namespace(
    unshare: &Path,
    broker: &Path,
    id_map: &IdMap,
    namespace_args: Vec<OsString>,
    parent_user_namespace: Option<RawFd>,
) -> Result<MappedUserNamespace> {
    let (ready_read, ready_write) = pipe()?;
    let (release_read, release_write) = pipe()?;
    let mut command = Command::new(unshare);
    id_map.configure_command(&mut command);
    command
        .args(namespace_args)
        .arg(broker)
        .arg("__hold-user-namespace")
        .arg("--ready-fd")
        .arg(ready_write.as_raw_fd().to_string())
        .arg("--release-fd")
        .arg(release_read.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(parent_user_namespace) = parent_user_namespace {
        // SAFETY: setns and close are async-signal-safe. The descriptor is a
        // validated user-namespace fd owned by the immediately preceding
        // held setup namespace and is inherited into this child only.
        unsafe {
            command.pre_exec(move || {
                if libc::setns(parent_user_namespace, libc::CLONE_NEWUSER) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(parent_user_namespace);
                Ok(())
            });
        }
    }
    let mut child = command.spawn().context("creating mapped user namespace")?;
    drop(ready_write);
    close_fd(release_read);
    // SAFETY: the parent owns the read end returned by `pipe`.
    let mut ready_read = unsafe { File::from_raw_fd(ready_read) };
    let mut signal = [0_u8; 1];
    if let Err(error) = ready_read.read_exact(&mut signal) {
        terminate(&mut child);
        return Err(error).context("waiting for mapped user namespace");
    }
    let namespace_path = PathBuf::from(format!("/proc/{}/ns/user", child.id()));
    let namespace = File::open(&namespace_path)
        .with_context(|| format!("opening mapped user namespace {}", namespace_path.display()))?;
    let fd = namespace.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        terminate(&mut child);
        return Err(std::io::Error::last_os_error())
            .context("making mapped user namespace descriptor inheritable");
    }
    Ok(MappedUserNamespace {
        descriptor: namespace,
        holder: Some(child),
        release: Some(release_write),
    })
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

    #[test]
    fn readiness_deadline_has_a_stable_state_code_and_retains_diagnostics() {
        let error = anyhow::Error::new(DesktopReadinessDeadline { seconds: 90 })
            .context("nested compositor log: fixture");

        let detail = machine_session_failure_detail(&error);

        assert!(detail.starts_with("desktop-readiness-deadline:90: "));
        assert!(detail.contains("nested compositor log: fixture"));
        assert_eq!(
            machine_session_failure_detail(&anyhow::anyhow!("ordinary startup failure")),
            "ordinary startup failure"
        );

        let frame_error = anyhow::Error::new(DesktopFrameReadinessDeadline { seconds: 90 })
            .context("nested compositor log: no frame fixture");
        let frame_detail = machine_session_failure_detail(&frame_error);
        assert!(frame_detail.starts_with("desktop-readiness-deadline:90: "));
        assert!(frame_detail.contains("did not submit and paint a dmabuf frame"));
        assert!(frame_detail.contains("no frame fixture"));
    }

    #[test]
    fn desktop_readiness_requires_a_real_painted_dmabuf_frame() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("presentation.json");

        fs::write(
            &path,
            serde_json::to_vec(&PresentationDiagnostics::default()).unwrap(),
        )
        .unwrap();
        assert!(!presentation_has_first_frame(&path));

        let mut submitted = PresentationDiagnostics {
            submitted_frames: 1,
            ..PresentationDiagnostics::default()
        };
        fs::write(&path, serde_json::to_vec(&submitted).unwrap()).unwrap();
        assert!(!presentation_has_first_frame(&path));

        submitted.painted_frames = 1;
        fs::write(&path, serde_json::to_vec(&submitted).unwrap()).unwrap();
        assert!(presentation_has_first_frame(&path));

        submitted.transport = "shm".into();
        fs::write(&path, serde_json::to_vec(&submitted).unwrap()).unwrap();
        assert!(!presentation_has_first_frame(&path));
    }

    #[test]
    fn desktop_readiness_is_bound_to_the_current_session_token() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("desktop-ready");
        let current = "0123456789abcdef0123456789abcdef";
        fs::write(&marker, format!("{current}\n")).unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(DESKTOP_READY_MODE)).unwrap();

        assert!(desktop_ready_for_session(&marker, current).unwrap());
        assert!(!desktop_ready_for_session(&marker, &"f".repeat(SESSION_TOKEN_BYTES)).unwrap());

        let target = directory.path().join("outside-ready");
        fs::rename(&marker, &target).unwrap();
        symlink(&target, &marker).unwrap();
        assert!(desktop_ready_for_session(&marker, current).is_err());
    }

    #[test]
    fn guest_runtime_is_private_to_the_keep_id_desktop_owner() {
        let runtime = LifecycleRuntime::create().unwrap();
        let metadata = fs::symlink_metadata(&runtime.guest).unwrap();

        assert!(metadata.is_dir());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(metadata.uid(), Uid::effective().as_raw());
        assert_eq!(metadata.gid(), nix::unistd::Gid::effective().as_raw());
        assert_eq!(metadata.permissions().mode() & 0o777, GUEST_RUNTIME_MODE);

        // IdMap's keep-id segment maps this host owner to guest UID/GID 1000.
        // Consequently the interactive Sway session owns all three private
        // endpoints, while 0700 denies unrelated guest service identities.
        for name in [
            "wayland-0",
            "display-scale-host.sock",
            "clipboard-agent.sock",
        ] {
            let path = runtime.guest.join(name);
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let connection = std::os::unix::net::UnixStream::connect(&path).unwrap();
            let (_accepted, _) = listener.accept().unwrap();
            drop(connection);
        }
    }

    #[test]
    fn bubblewrap_pid_reporting_is_one_shot_not_a_lifecycle_status_stream() {
        let mut command = Command::new("bwrap");

        add_bubblewrap_pid_report(&mut command, 42);

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(arguments, ["--info-fd", "42"]);
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--json-status-fd")
        );
    }

    #[test]
    fn bubblewrap_mounts_as_the_host_user_then_enters_subordinate_guest_root() {
        let mount_setup = File::open("/proc/self/ns/user").unwrap();
        let guest = File::open("/proc/self/ns/user").unwrap();
        let mut command = Command::new("bwrap");

        add_guest_user_namespaces(&mut command, &mount_setup, &guest);

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let mount_setup_fd = mount_setup.as_raw_fd().to_string();
        let guest_fd = guest.as_raw_fd().to_string();
        assert_eq!(
            arguments,
            [
                "--userns".to_owned(),
                mount_setup_fd,
                "--userns2".to_owned(),
                guest_fd,
                "--uid".to_owned(),
                "0".to_owned(),
                "--gid".to_owned(),
                "0".to_owned(),
            ]
        );
    }

    #[test]
    fn guest_pid_one_owns_a_private_mount_namespace_after_the_userns_transition() {
        let mut command = Command::new("bwrap");

        add_guest_init_command(&mut command);

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "--",
                "/usr/bin/unshare",
                "--mount",
                "--propagation",
                "private",
                "/usr/bin/setpriv",
                "--reuid=0",
                "--regid=0",
                "--clear-groups",
                "/usr/lib/buzzardos/runtime/current/libexec/buzzardos-init",
            ]
        );
    }

    #[test]
    fn machine_rootfs_bind_descriptor_survives_exec() {
        let directory = tempfile::tempdir().unwrap();
        let descriptor = inherited_bind_descriptor(directory.path(), "test rootfs").unwrap();
        let flags = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_GETFD) };

        assert!(flags >= 0);
        assert_eq!(flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn reads_complete_multiline_one_shot_bubblewrap_information() {
        let (info_read, mut info_write) = pipe().unwrap();
        info_write
            .write_all(
                br#"{
    "child-pid": 4242,
    "mnt-namespace": 123456
}
"#,
            )
            .unwrap();
        drop(info_write);

        assert_eq!(read_container_pid(info_read).unwrap(), 4242);
    }

    #[test]
    fn reads_bubblewrap_information_without_waiting_for_descriptor_eof() {
        let (info_read, mut info_write) = pipe().unwrap();
        info_write
            .write_all(
                br#"{
    "child-pid": 4242,
    "mnt-namespace": 123456
}
"#,
            )
            .unwrap();

        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            sender.send(read_container_pid(info_read)).unwrap();
        });
        let child_pid = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("PID parser waited for Bubblewrap to close --info-fd")
            .unwrap();
        assert_eq!(child_pid, 4242);

        drop(info_write);
        reader.join().unwrap();
    }

    #[test]
    fn guest_pseudo_filesystems_include_posix_message_queues_before_pid_one() {
        let mut command = Command::new("bwrap");

        add_guest_pseudo_filesystems(&mut command);

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--mqueue",
                "/dev/mqueue",
            ]
        );
    }

    #[test]
    fn fuse_integration_requires_exact_kernel_device_identity() {
        validate_fuse_device_identity(Path::new("/dev/fuse"), false, true, libc::makedev(10, 229))
            .unwrap();
        assert!(
            validate_fuse_device_identity(
                Path::new("/dev/null"),
                false,
                true,
                libc::makedev(1, 3),
            )
            .unwrap_err()
            .to_string()
            .contains("expected the Linux FUSE device 10:229")
        );
        assert!(
            validate_fuse_device_identity(
                Path::new("/dev/fuse"),
                true,
                true,
                libc::makedev(10, 229),
            )
            .is_err()
        );

        let mut command = Command::new("true");
        match fs::symlink_metadata("/dev/fuse") {
            Ok(_) => add_fuse_device_at(&mut command, Path::new("/dev/fuse")).unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                assert!(
                    add_fuse_device_at(&mut command, Path::new("/dev/fuse"))
                        .unwrap_err()
                        .to_string()
                        .contains("required FUSE device")
                );
            }
            Err(error) => panic!("could not inspect /dev/fuse: {error}"),
        }

        let directory = tempfile::tempdir().unwrap();
        let regular = directory.path().join("fuse");
        fs::write(&regular, b"not a device").unwrap();
        assert!(add_fuse_device_at(&mut command, &regular).is_err());
        let missing = add_fuse_device_at(&mut command, &directory.path().join("missing"))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("required FUSE device"));
        assert!(missing.contains("10:229"));
    }

    fn test_machine() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, MachineConfig) {
        let workspace = tempfile::tempdir().unwrap();
        let machine = workspace.path().join("demo");
        let rootfs = machine.join("rootfs");
        let data = workspace.path().join("shared");
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
            workspace,
            machine.canonicalize().unwrap(),
            rootfs.canonicalize().unwrap(),
            data.canonicalize().unwrap(),
            config,
        )
    }

    #[test]
    fn validates_user_selected_machine_layout() {
        let (_workspace, machine, rootfs, _data, _config) = test_machine();
        validate_machine_layout(&machine, &rootfs).unwrap();
    }

    #[test]
    fn arbitrary_folder_name_is_valid_but_symlink_lock_is_rejected() {
        let (workspace, machine, rootfs, _data, mut config) = test_machine();
        config.name = "logical-name-can-differ".into();
        config.save(&machine).unwrap();
        validate_machine_layout(&machine, &rootfs).unwrap();
        let outside = workspace.path().join("outside-lock");
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
    fn primary_and_render_nodes_for_one_gpu_share_the_dmabuf_device() {
        let sysfs = tempfile::tempdir().unwrap();
        let gpu0 = sysfs.path().join("devices/gpu0");
        let gpu1 = sysfs.path().join("devices/gpu1");
        fs::create_dir_all(&gpu0).unwrap();
        fs::create_dir_all(&gpu1).unwrap();
        let chars = sysfs.path().join("char");
        for (node, target) in [("226:0", &gpu0), ("226:128", &gpu0), ("226:1", &gpu1)] {
            let directory = chars.join(node);
            fs::create_dir_all(&directory).unwrap();
            std::os::unix::fs::symlink(target, directory.join("device")).unwrap();
        }

        assert!(drm_devices_share_backing_device(
            libc::makedev(226, 0),
            libc::makedev(226, 128),
            &chars,
        ));
        assert!(!drm_devices_share_backing_device(
            libc::makedev(226, 0),
            libc::makedev(226, 1),
            &chars,
        ));
    }

    #[test]
    fn private_dmabuf_protocol_requires_v4_main_device_feedback() {
        let mut host = WaylandCapabilities {
            linux_dmabuf: true,
            linux_dmabuf_version: 3,
            ..WaylandCapabilities::default()
        };
        assert!(private_dmabuf_version(&host).is_err());

        host.linux_dmabuf_version = 4;
        assert!(private_dmabuf_version(&host).is_err());

        host.dmabuf_main_device = Some(libc::makedev(226, 0));
        assert_eq!(private_dmabuf_version(&host).unwrap(), 4);

        host.linux_dmabuf = false;
        assert!(private_dmabuf_version(&host).is_err());
    }

    #[test]
    fn host_visible_machine_id_uses_buzzard_os_branding() {
        assert_eq!(
            machine_window_app_id("default"),
            "org.openresearchtools.buzzardos"
        );
    }

    #[test]
    fn ephemeral_hosts_resolves_the_machine_hostname() {
        assert_eq!(
            guest_hosts_contents("development-machine"),
            "127.0.0.1\tlocalhost\n\
             127.0.1.1\tdevelopment-machine\n\
             ::1\tlocalhost ip6-localhost ip6-loopback\n\
             ff02::1\tip6-allnodes\n\
             ff02::2\tip6-allrouters\n"
        );
    }

    #[test]
    fn diagnostics_refresh_cannot_erase_an_external_stop_request() {
        let (_workspace, machine, _rootfs, _data, _config) = test_machine();
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
    fn session_restart_clears_relays_without_following_a_replaced_directory() {
        let runtime = LifecycleRuntime::create().unwrap();
        let reverse = runtime.guest.join("reverse");
        fs::create_dir(&reverse).unwrap();
        fs::write(reverse.join("reverse-stale.sock"), b"stale").unwrap();

        clear_session_runtime(&runtime).unwrap();
        assert!(!reverse.exists());

        let outside = tempfile::tempdir().unwrap();
        let marker = outside.path().join("must-survive");
        fs::write(&marker, b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), &reverse).unwrap();

        clear_session_runtime(&runtime).unwrap();
        assert!(marker.is_file());
        assert!(fs::symlink_metadata(&reverse).is_err());
    }

    #[test]
    fn session_restart_clears_clipboard_endpoint_without_following_symlinks() {
        let runtime = LifecycleRuntime::create().unwrap();
        let socket = runtime.guest.join("clipboard-agent.sock");
        let ready = runtime.guest.join("clipboard-ready");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        fs::write(&ready, b"ready\n").unwrap();
        drop(listener);

        clear_session_runtime(&runtime).unwrap();
        assert!(fs::symlink_metadata(&socket).is_err());
        assert!(fs::symlink_metadata(&ready).is_err());

        let outside = tempfile::tempdir().unwrap();
        let outside_socket = outside.path().join("must-survive-socket-target");
        let outside_ready = outside.path().join("must-survive-ready-target");
        fs::write(&outside_socket, b"outside socket target").unwrap();
        fs::write(&outside_ready, b"outside ready target").unwrap();
        symlink(&outside_socket, &socket).unwrap();
        symlink(&outside_ready, &ready).unwrap();

        clear_session_runtime(&runtime).unwrap();
        assert_eq!(fs::read(&outside_socket).unwrap(), b"outside socket target");
        assert_eq!(fs::read(&outside_ready).unwrap(), b"outside ready target");
        assert!(fs::symlink_metadata(&socket).is_err());
        assert!(fs::symlink_metadata(&ready).is_err());
    }

    #[test]
    fn clipboard_revocation_preserves_persistent_display_endpoints() {
        let runtime = LifecycleRuntime::create().unwrap();
        let wayland = runtime.guest.join("wayland-0");
        let scale = runtime.guest.join("display-scale-host.sock");
        let clipboard = runtime.guest.join("clipboard-agent.sock");
        let ready = runtime.guest.join("clipboard-ready");
        let wayland_listener = std::os::unix::net::UnixListener::bind(&wayland).unwrap();
        let scale_listener = std::os::unix::net::UnixListener::bind(&scale).unwrap();
        let clipboard_listener = std::os::unix::net::UnixListener::bind(&clipboard).unwrap();
        fs::write(&ready, b"ready\n").unwrap();

        clear_clipboard_session_runtime(&runtime).unwrap();

        assert!(
            fs::symlink_metadata(&wayland)
                .unwrap()
                .file_type()
                .is_socket()
        );
        assert!(
            fs::symlink_metadata(&scale)
                .unwrap()
                .file_type()
                .is_socket()
        );
        assert!(fs::symlink_metadata(&clipboard).is_err());
        assert!(fs::symlink_metadata(&ready).is_err());
        let wayland_client = std::os::unix::net::UnixStream::connect(&wayland).unwrap();
        let scale_client = std::os::unix::net::UnixStream::connect(&scale).unwrap();
        let (_wayland_server, _) = wayland_listener.accept().unwrap();
        let (_scale_server, _) = scale_listener.accept().unwrap();
        drop((wayland_client, scale_client, clipboard_listener));
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
        for generated_driver_file in [
            Path::new("/usr/sbin/nvidia-cuda-mps-server"),
            Path::new("/usr/share/X11/xorg.conf.d/10-nvidia.conf"),
            Path::new("/usr/share/X11/xorg.conf.d/nvidia-drm-outputclass.conf"),
        ] {
            assert!(allowed_nvidia_cdi_source(generated_driver_file));
            assert!(allowed_nvidia_cdi_destination(generated_driver_file));
        }
        assert!(!allowed_nvidia_cdi_source(Path::new(
            "/usr/sbin/nvidia-unrelated"
        )));
        assert!(!allowed_nvidia_cdi_destination(Path::new(
            "/usr/share/X11/xorg.conf.d/unrelated.conf"
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
