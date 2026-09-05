// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{MachineConfig, NetworkMode, PortDirection, PortProtocol, ResourceLocator};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use uuid::Uuid;

const MACHINE_LABEL: &str = "org.openresearchtools.buzzardos.machine-id";
const MANAGED_LABEL: &str = "org.openresearchtools.buzzardos.managed";
const GUEST_HOST_RUNTIME: &str = "/run/buzzardos-host";
const GUEST_DISPLAY_STATE: &str = "/run/buzzardos-display-state";
pub const GUEST_AUDIO_PORT: u16 = 47_130;
pub const HOST_MICROPHONE_PORT: u16 = 47_131;
pub const HOST_CAMERA_PORT: u16 = 47_132;

/// Stable host runtime paths mounted into one Podman machine.
///
/// Podman stores these paths in the persistent container definition. Their
/// contents are ephemeral and are recreated before every start, but the path
/// itself is stable for the machine UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanRuntimePaths {
    pub root: PathBuf,
    pub host_exchange: PathBuf,
    pub host_status: PathBuf,
    pub display_state: PathBuf,
}

impl PodmanRuntimePaths {
    pub fn discover(machine_id: Uuid) -> Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .context("XDG_RUNTIME_DIR is required to run a Buzzard OS machine")?;
        if !runtime.is_absolute() {
            bail!("XDG_RUNTIME_DIR must be absolute: {}", runtime.display());
        }
        Ok(Self::under(&runtime, machine_id))
    }

    pub fn under(runtime: &Path, machine_id: Uuid) -> Self {
        let root = runtime
            .join(crate::host_identity().package)
            .join("machines")
            .join(machine_id.simple().to_string());
        Self {
            host_exchange: root.join("host"),
            host_status: root.join("host-status"),
            display_state: root.join("display-state"),
            root,
        }
    }

    pub fn prepare(&self) -> Result<()> {
        for path in [
            &self.root,
            &self.host_exchange,
            &self.host_status,
            &self.display_state,
        ] {
            fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("protecting {}", path.display()))?;
        }
        // This directory is visible only through the private bind mount. Its
        // contents must be reachable by the interactive guest user under any
        // native Podman user-namespace mapping.
        fs::set_permissions(&self.host_exchange, fs::Permissions::from_mode(0o777))
            .with_context(|| format!("preparing {}", self.host_exchange.display()))?;
        fs::set_permissions(&self.display_state, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("preparing {}", self.display_state.display()))?;
        Ok(())
    }
}

/// The exact persistent `podman create` definition for one machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanDefinition {
    pub container_name: String,
    pub arguments: Vec<OsString>,
    pub digest: String,
}

impl PodmanDefinition {
    pub fn for_machine(
        config: &MachineConfig,
        machine_dir: &Path,
        runtime: &PodmanRuntimePaths,
    ) -> Result<Self> {
        if !machine_dir.is_absolute() {
            bail!(
                "machine directory must be absolute: {}",
                machine_dir.display()
            );
        }
        let rootfs = machine_dir.join("rootfs");
        if !rootfs.is_dir() {
            bail!("machine rootfs is missing: {}", rootfs.display());
        }
        config.integrations.validate(config.network)?;
        MachineConfig::validate_shares(&config.shares)?;

        let container_name = container_name(config.id);
        let mut arguments = vec![
            OsString::from("create"),
            OsString::from("--name"),
            OsString::from(&container_name),
            OsString::from("--label"),
            OsString::from(format!("{MANAGED_LABEL}=true")),
            OsString::from("--label"),
            OsString::from(format!("{MACHINE_LABEL}={}", config.id)),
            OsString::from("--hostname"),
            OsString::from(&config.name),
            OsString::from("--systemd=always"),
            // A Buzzard machine always boots systemd as PID 1. Native modes
            // such as keep-id otherwise select the caller's UID as the
            // process user. Keep this before the unrestricted arguments so a
            // user can still override it with stock Podman semantics.
            OsString::from("--user=0"),
            // systemd uses this standard container marker to select its
            // container boot path. Podman's --systemd flag prepares mounts
            // and stop behavior but does not add the variable for an
            // external --rootfs container.
            OsString::from("--env"),
            OsString::from("container=podman"),
        ];

        match config.network {
            // No explicit argument: native Podman configuration selects the
            // rootless default network and all its normal behavior.
            NetworkMode::User => append_user_network(&mut arguments, config),
            NetworkMode::Host => arguments.push(OsString::from("--network=host")),
            NetworkMode::None => arguments.push(OsString::from("--network=none")),
        }

        for variable in &config.oci.environment {
            arguments.push(OsString::from("--env"));
            arguments.push(OsString::from(variable));
        }
        // Preserve the original hardware renderer. Device access must be
        // configured through native rootless Podman, never substituted with
        // a software-rendered desktop.
        arguments.push(OsString::from("--env"));
        arguments.push(OsString::from("WLR_RENDERER=gles2"));
        if let Some(signal) = config.oci.stop_signal.as_deref() {
            arguments.push(OsString::from("--stop-signal"));
            arguments.push(OsString::from(signal));
        }

        for gpu in &config.gpus {
            arguments.push(OsString::from("--gpus"));
            arguments.push(OsString::from(gpu));
        }

        for (enabled, guest_port) in [
            (
                config.integrations.media.guest_audio_output,
                GUEST_AUDIO_PORT,
            ),
            (
                config.integrations.media.host_microphone,
                HOST_MICROPHONE_PORT,
            ),
            (config.integrations.media.host_camera, HOST_CAMERA_PORT),
        ] {
            if enabled {
                arguments.push(OsString::from("--publish"));
                arguments.push(OsString::from(format!("127.0.0.1::{guest_port}/tcp")));
            }
        }

        append_bind_mount(
            &mut arguments,
            &runtime.host_exchange,
            Path::new(GUEST_HOST_RUNTIME),
            false,
        );
        append_bind_mount(
            &mut arguments,
            &runtime.display_state,
            Path::new(GUEST_DISPLAY_STATE),
            true,
        );
        for share in &config.shares {
            append_bind_mount(
                &mut arguments,
                &share.host_path,
                &share.guest_path(),
                share.read_only,
            );
        }

        // This is the sole free-form runtime extension point. It deliberately
        // receives no filtering or policy classification; Podman parses and
        // owns every supplied option.
        arguments.extend(
            MachineConfig::parse_custom_podman_arguments(&config.custom_podman_arguments)?
                .into_iter()
                .map(OsString::from),
        );

        // Podman opens the external rootfs directly. No user namespace or
        // idmapped-mount mode is selected or implied by Buzzard.
        arguments.push(OsString::from("--rootfs"));
        arguments.push(rootfs.into_os_string());
        // The fixed guest init creates only the private runtime directories
        // required by the desktop and then execs systemd as PID 1. Podman
        // still owns all container, namespace, mount, device, and lifecycle
        // behavior.
        arguments.push(OsString::from(
            "/usr/lib/buzzardos/runtime/current/libexec/buzzardos-init",
        ));

        let digest = digest_arguments(&arguments);
        Ok(Self {
            container_name,
            arguments,
            digest,
        })
    }
}

fn append_user_network(arguments: &mut Vec<OsString>, config: &MachineConfig) {
    let mut pasta = Vec::new();
    for port in config.integrations.ports.iter().filter(|port| port.enabled) {
        match port.direction {
            PortDirection::HostToGuest => {
                arguments.push(OsString::from("--publish"));
                arguments.push(OsString::from(format!(
                    "{}:{}:{}/{}",
                    port.host_address,
                    port.host_port,
                    port.guest_port,
                    protocol_name(port.protocol)
                )));
            }
            PortDirection::GuestToHost => {
                pasta.push(match port.protocol {
                    PortProtocol::Tcp => "-T".to_owned(),
                    PortProtocol::Udp => "-U".to_owned(),
                });
                pasta.push(format!(
                    "{}/{}:{}",
                    port.guest_address, port.guest_port, port.host_port
                ));
            }
        }
    }
    if !pasta.is_empty() {
        arguments.push(OsString::from(format!(
            "--network=pasta:{}",
            pasta.join(",")
        )));
    }
}

fn protocol_name(protocol: PortProtocol) -> &'static str {
    match protocol {
        PortProtocol::Tcp => "tcp",
        PortProtocol::Udp => "udp",
    }
}

fn append_bind_mount(arguments: &mut Vec<OsString>, source: &Path, target: &Path, read_only: bool) {
    arguments.push(OsString::from("--mount"));
    let mut value = format!(
        "type=bind,src={},dst={}",
        quote_mount_field(source.as_os_str()),
        quote_mount_field(target.as_os_str())
    );
    if read_only {
        value.push_str(",ro=true");
    }
    arguments.push(OsString::from(value));
}

fn quote_mount_field(value: &OsStr) -> String {
    let value = value.to_string_lossy();
    if value.contains([',', '"']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.into_owned()
    }
}

fn digest_arguments(arguments: &[OsString]) -> String {
    use std::os::unix::ffi::OsStrExt;

    let mut hash = Sha256::new();
    for argument in arguments {
        let bytes = argument.as_os_str().as_bytes();
        hash.update(bytes.len().to_le_bytes());
        hash.update(bytes);
    }
    format!("sha256:{:x}", hash.finalize())
}

fn container_name(id: Uuid) -> String {
    format!("{}-{}", crate::host_identity().package, id.simple())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodmanContainerState {
    Configured,
    Created,
    Running,
    Paused,
    Stopping,
    Stopped,
    Exited,
    Unknown,
}

impl PodmanContainerState {
    fn parse(value: &str) -> Self {
        match value {
            "configured" => Self::Configured,
            "created" | "initialized" => Self::Created,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "stopping" => Self::Stopping,
            "stopped" => Self::Stopped,
            "exited" => Self::Exited,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanInspection {
    pub id: String,
    pub name: String,
    pub state: PodmanContainerState,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub definition_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodmanImageInspection {
    pub id: String,
    pub digest: Option<String>,
    pub names: Vec<String>,
    pub environment: Vec<String>,
    pub labels: std::collections::BTreeMap<String, String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub entrypoint: Vec<String>,
    pub command: Vec<String>,
    pub stop_signal: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Podman {
    executable: PathBuf,
}

impl Podman {
    pub fn discover(resources: &ResourceLocator) -> Result<Self> {
        Ok(Self {
            executable: resources.helper_or_path("podman")?,
        })
    }

    pub fn create(&self, definition: &PodmanDefinition) -> Result<PodmanInspection> {
        let mut arguments = definition.arguments.clone();
        let label_index = arguments
            .iter()
            .position(|argument| argument == "--hostname")
            .context("internal Podman definition has no hostname boundary")?;
        arguments.splice(
            label_index..label_index,
            [
                OsString::from("--label"),
                OsString::from(format!(
                    "org.openresearchtools.buzzardos.definition={}",
                    definition.digest
                )),
            ],
        );
        self.run_checked(&arguments, "creating Podman machine")?;
        self.inspect(&definition.container_name)?.with_context(|| {
            format!(
                "Podman created '{}' but it cannot be inspected",
                definition.container_name
            )
        })
    }

    pub fn pull(&self, reference: &str) -> Result<String> {
        self.run_streamed(
            &[OsString::from("pull"), OsString::from(reference)],
            "pulling image with Podman",
        )?;
        Ok(reference.to_owned())
    }

    pub fn build(&self, tag: &str, containerfile: &Path, context: &Path) -> Result<String> {
        self.run_streamed(
            &[
                OsString::from("build"),
                OsString::from("--tag"),
                OsString::from(tag),
                OsString::from("--file"),
                containerfile.as_os_str().to_owned(),
                context.as_os_str().to_owned(),
            ],
            "building image with Podman",
        )?;
        Ok(tag.to_owned())
    }

    pub fn load(&self, archive: &Path) -> Result<String> {
        let output = self.run_checked(
            &[
                OsString::from("load"),
                OsString::from("--input"),
                archive.as_os_str().to_owned(),
            ],
            "loading image archive with Podman",
        )?;
        output_text(output, "Podman load")
    }

    pub fn inspect_image(&self, image: &str) -> Result<PodmanImageInspection> {
        let output = self.run_checked(
            &[
                OsString::from("image"),
                OsString::from("inspect"),
                OsString::from(image),
            ],
            "inspecting Podman image",
        )?;
        let documents: Vec<ImageInspectDocument> = serde_json::from_slice(&output.stdout)
            .context("parsing podman image inspect output")?;
        let document = documents
            .into_iter()
            .next()
            .context("podman image inspect returned no image")?;
        Ok(PodmanImageInspection {
            id: document.id,
            digest: document.digest.filter(|value| !value.is_empty()),
            names: document.repo_tags,
            environment: document.config.env,
            labels: document.config.labels,
            working_dir: nonempty(document.config.working_dir),
            user: nonempty(document.config.user),
            entrypoint: document.config.entrypoint,
            command: document.config.cmd,
            stop_signal: nonempty(document.config.stop_signal),
        })
    }

    pub fn create_from_image(&self, name: &str, image: &str) -> Result<()> {
        self.run_checked(
            &[
                OsString::from("create"),
                OsString::from("--name"),
                OsString::from(name),
                OsString::from(image),
                OsString::from("/usr/bin/true"),
            ],
            "creating temporary Podman image container",
        )?;
        Ok(())
    }

    /// Run one argv-only command against an external rootfs under the same
    /// native Podman creation arguments selected for the machine.
    pub fn run_in_rootfs(
        &self,
        rootfs: &Path,
        custom_arguments: &str,
        command: &[OsString],
    ) -> Result<()> {
        if command.is_empty() {
            bail!("rootfs command cannot be empty");
        }
        let mut arguments = vec![OsString::from("run"), OsString::from("--rm")];
        arguments.extend(
            MachineConfig::parse_custom_podman_arguments(custom_arguments)?
                .into_iter()
                .map(OsString::from),
        );
        arguments.push(OsString::from("--rootfs"));
        arguments.push(rootfs.as_os_str().to_owned());
        arguments.extend_from_slice(command);
        self.run_checked(&arguments, "running a command in an external rootfs")?;
        Ok(())
    }

    pub fn version(&self) -> Result<String> {
        let output = self.run_checked(
            &[
                OsString::from("version"),
                OsString::from("--format={{.Client.Version}}"),
            ],
            "querying Podman version",
        )?;
        output_text(output, "Podman version")
    }

    pub fn export_rootfs(&self, container: &str, output: &Path) -> Result<()> {
        self.run_checked(
            &[
                OsString::from("export"),
                OsString::from("--output"),
                output.as_os_str().to_owned(),
                OsString::from(container),
            ],
            "exporting a flat rootfs with Podman",
        )?;
        Ok(())
    }

    /// Archive a stopped exploded rootfs from inside the machine's selected
    /// native Podman user namespace.
    ///
    /// Podman's `export` command currently exports the empty storage layer of
    /// a container created with `--rootfs`, rather than that external rootfs.
    /// Run the rootfs's own GNU tar in a disposable stock-Podman container and
    /// expose the unmounted source tree at a private read-only bind instead.
    /// This retains guest numeric ownership for default, keep-id, host, auto,
    /// nomap, and explicit native mapping arguments without Buzzard translating
    /// IDs. Runtime-only directory contents are intentionally omitted.
    pub fn archive_external_rootfs(
        &self,
        rootfs: &Path,
        custom_arguments: &str,
        output: &Path,
    ) -> Result<()> {
        let source = format!("/run/buzzardos-export-{}", Uuid::new_v4().simple());
        let mut arguments = vec![OsString::from("run"), OsString::from("--rm")];
        arguments.extend(
            MachineConfig::parse_custom_podman_arguments(custom_arguments)?
                .into_iter()
                .map(OsString::from),
        );
        arguments.extend([
            OsString::from("--network=none"),
            OsString::from("--read-only"),
            OsString::from("--user=0"),
            OsString::from("--mount"),
            OsString::from(format!(
                "type=bind,src={},dst={source},ro=true",
                quote_mount_field(rootfs.as_os_str())
            )),
            OsString::from("--rootfs"),
            rootfs.as_os_str().to_owned(),
            OsString::from("/usr/bin/tar"),
            OsString::from("--numeric-owner"),
            OsString::from("--xattrs"),
            OsString::from("--xattrs-include=*"),
            OsString::from("--acls"),
            OsString::from("--sparse"),
            OsString::from("--exclude=./dev/*"),
            OsString::from("--exclude=./proc/*"),
            OsString::from("--exclude=./run/*"),
            OsString::from("--exclude=./shared/*"),
            OsString::from("--exclude=./sys/*"),
            OsString::from("--exclude=./tmp/*"),
            OsString::from("-cpf"),
            OsString::from("-"),
            OsString::from("-C"),
            OsString::from(source),
            OsString::from("."),
        ]);

        let archive = File::options()
            .write(true)
            .truncate(true)
            .open(output)
            .with_context(|| format!("opening rootfs archive {}", output.display()))?;
        let result = Command::new(&self.executable)
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::from(archive))
            .output()
            .with_context(|| {
                format!(
                    "running {} to archive the external rootfs",
                    self.executable.display()
                )
            })?;
        if !result.status.success() {
            return command_error(
                "archiving the external rootfs with Podman",
                &self.executable,
                &result,
            );
        }
        Ok(())
    }

    /// Extract a Podman-exported rootfs using the selected image's own tar
    /// and the same unrestricted native creation arguments as the machine.
    /// File ownership is therefore created by the selected Podman user
    /// namespace rather than translated by Buzzard.
    pub fn materialize_rootfs(
        &self,
        image: &str,
        archive: &Path,
        destination: &Path,
        custom_arguments: &str,
    ) -> Result<()> {
        let mut arguments = vec![
            OsString::from("run"),
            OsString::from("--rm"),
            OsString::from("--interactive"),
        ];
        arguments.extend(
            MachineConfig::parse_custom_podman_arguments(custom_arguments)?
                .into_iter()
                .map(OsString::from),
        );
        arguments.extend([
            OsString::from("--user=0"),
            OsString::from("--mount"),
            OsString::from(format!(
                "type=bind,src={},dst=/run/buzzardos-materialize,rw=true",
                quote_mount_field(destination.as_os_str())
            )),
            OsString::from(image),
            OsString::from("/bin/sh"),
            OsString::from("-ec"),
            OsString::from(
                "chown 0:0 /run/buzzardos-materialize; \
                 chmod 0755 /run/buzzardos-materialize; \
                 exec /usr/bin/tar --numeric-owner --xattrs --acls -xpf - \
                 -C /run/buzzardos-materialize",
            ),
        ]);
        let input = File::open(archive)
            .with_context(|| format!("opening exported rootfs {}", archive.display()))?;
        let output = Command::new(&self.executable)
            .args(&arguments)
            .stdin(input)
            .output()
            .with_context(|| {
                format!(
                    "running {} to materialize the external rootfs",
                    self.executable.display()
                )
            })?;
        if !output.status.success() {
            return command_error(
                "materializing the external rootfs with Podman",
                &self.executable,
                &output,
            );
        }
        Ok(())
    }

    /// Import a flat rootfs archive as an image using only stock Podman
    /// `--change` directives supplied by the caller.
    pub fn import_rootfs_archive(
        &self,
        archive: &Path,
        image: &str,
        changes: &[OsString],
    ) -> Result<String> {
        let mut arguments = vec![OsString::from("import")];
        for change in changes {
            arguments.push(OsString::from("--change"));
            arguments.push(change.clone());
        }
        arguments.push(archive.as_os_str().to_owned());
        arguments.push(OsString::from(image));
        let output = self.run_checked(&arguments, "importing a flat rootfs with Podman")?;
        output_text(output, "Podman import")
    }

    pub fn save_oci_archive(&self, image: &str, output: &Path) -> Result<()> {
        self.run_checked(
            &[
                OsString::from("save"),
                OsString::from("--format=oci-archive"),
                OsString::from("--output"),
                output.as_os_str().to_owned(),
                OsString::from(image),
            ],
            "saving OCI archive with Podman",
        )?;
        Ok(())
    }

    pub fn remove_image(&self, image: &str) -> Result<()> {
        self.run_checked(
            &[
                OsString::from("image"),
                OsString::from("rm"),
                OsString::from("--ignore"),
                OsString::from(image),
            ],
            "removing temporary Podman image",
        )?;
        Ok(())
    }

    pub fn start(&self, container: &str) -> Result<()> {
        self.run_checked(
            &[OsString::from("start"), OsString::from(container)],
            "starting Podman machine",
        )?;
        Ok(())
    }

    pub fn stop(&self, container: &str) -> Result<()> {
        self.run_checked(
            &[OsString::from("stop"), OsString::from(container)],
            "stopping Podman machine",
        )?;
        Ok(())
    }

    pub fn restart(&self, container: &str) -> Result<()> {
        self.run_checked(
            &[OsString::from("restart"), OsString::from(container)],
            "restarting Podman machine",
        )?;
        Ok(())
    }

    /// Execute an argv vector through stock `podman exec` without parsing,
    /// filtering, or rewriting any of the caller-supplied arguments.
    pub fn exec(&self, arguments: &[OsString]) -> Result<()> {
        if arguments.is_empty() {
            bail!("Podman exec arguments cannot be empty");
        }
        let mut command = Vec::with_capacity(arguments.len() + 1);
        command.push(OsString::from("exec"));
        command.extend_from_slice(arguments);
        self.run_checked(&command, "executing a command in a Podman machine")?;
        Ok(())
    }

    pub fn wait(&self, container: &str) -> Result<i32> {
        let output = self.run_checked(
            &[OsString::from("wait"), OsString::from(container)],
            "waiting for Podman machine",
        )?;
        output_text(output, "Podman wait")?
            .lines()
            .next()
            .context("Podman wait returned no exit status")?
            .trim()
            .parse::<i32>()
            .context("Podman wait returned an invalid exit status")
    }

    pub fn remove_definition(&self, container: &str) -> Result<()> {
        self.run_checked(
            &[
                OsString::from("rm"),
                OsString::from("--ignore"),
                OsString::from(container),
            ],
            "removing stopped Podman machine definition",
        )?;
        Ok(())
    }

    /// Remove one exact external machine/staging tree through Podman's native
    /// rootless user namespace when host ownership prevents ordinary removal.
    pub fn remove_external_tree(&self, path: &Path) -> Result<()> {
        if !path.is_absolute() || path.parent().is_none() {
            bail!("external tree removal requires a specific absolute path");
        }
        self.run_checked(
            &[
                OsString::from("unshare"),
                OsString::from("rm"),
                OsString::from("--recursive"),
                OsString::from("--force"),
                OsString::from("--"),
                path.as_os_str().to_owned(),
            ],
            "removing an external machine tree in Podman's user namespace",
        )?;
        Ok(())
    }

    pub fn inspect(&self, container: &str) -> Result<Option<PodmanInspection>> {
        let output = Command::new(&self.executable)
            .args(["container", "inspect", container])
            .output()
            .with_context(|| format!("running {} container inspect", self.executable.display()))?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            if error.contains("no such container") || error.contains("does not exist") {
                return Ok(None);
            }
            return command_error("inspecting Podman machine", &self.executable, &output);
        }
        let documents: Vec<InspectDocument> = serde_json::from_slice(&output.stdout)
            .context("parsing podman container inspect output")?;
        let document = documents
            .into_iter()
            .next()
            .context("podman container inspect succeeded without returning a container document")?;
        let state = document.state.unwrap_or_default();
        Ok(Some(PodmanInspection {
            id: document.id,
            name: document.name,
            state: PodmanContainerState::parse(&state.status),
            pid: (state.pid > 0).then_some(state.pid),
            exit_code: state.exit_code,
            definition_digest: document.config.and_then(|config| config.labels).and_then(
                |labels| {
                    labels
                        .get("org.openresearchtools.buzzardos.definition")
                        .cloned()
                },
            ),
        }))
    }

    pub fn port(&self, container: &str, guest_port: u16, protocol: PortProtocol) -> Result<String> {
        let output = self.run_checked(
            &[
                OsString::from("port"),
                OsString::from(container),
                OsString::from(format!("{guest_port}/{}", protocol_name(protocol))),
            ],
            "querying Podman port mapping",
        )?;
        String::from_utf8(output.stdout)
            .context("Podman port output is not UTF-8")
            .map(|value| value.trim().to_owned())
    }

    fn run_checked(&self, arguments: &[OsString], action: &str) -> Result<Output> {
        let output = Command::new(&self.executable)
            .args(arguments)
            .output()
            .with_context(|| format!("running {} for {action}", self.executable.display()))?;
        if !output.status.success() {
            return command_error(action, &self.executable, &output);
        }
        Ok(output)
    }

    fn run_streamed(&self, arguments: &[OsString], action: &str) -> Result<()> {
        let status = Command::new(&self.executable)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("running {} for {action}", self.executable.display()))?;
        if !status.success() {
            bail!(
                "{action} failed: {} exited with {status}",
                self.executable.display()
            );
        }
        Ok(())
    }
}

fn output_text(output: Output, action: &str) -> Result<String> {
    String::from_utf8(output.stdout)
        .with_context(|| format!("{action} output is not UTF-8"))
        .map(|value| value.trim().to_owned())
}

fn command_error<T>(action: &str, executable: &Path, output: &Output) -> Result<T> {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let detail = if !stderr.is_empty() { stderr } else { stdout };
    bail!(
        "{action} failed: {} exited with {}{}",
        executable.display(),
        output.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectDocument {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    state: Option<InspectState>,
    config: Option<InspectConfig>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectState {
    #[serde(default)]
    status: String,
    #[serde(default)]
    pid: u32,
    exit_code: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InspectConfig {
    labels: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageInspectDocument {
    #[serde(default)]
    id: String,
    #[serde(default)]
    digest: Option<String>,
    #[serde(default)]
    repo_tags: Vec<String>,
    #[serde(default)]
    config: ImageInspectConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImageInspectConfig {
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    working_dir: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    entrypoint: Vec<String>,
    #[serde(default)]
    cmd: Vec<String>,
    #[serde(default)]
    stop_signal: String,
}

fn nonempty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IntegrationSettings, MachineConfig, NetworkMode, PortForward, SharedPath};

    fn config() -> MachineConfig {
        MachineConfig::new(
            "machine-one".into(),
            "fixture".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            Vec::new(),
        )
    }

    fn definition(config: &MachineConfig) -> (tempfile::TempDir, PodmanDefinition) {
        let temp = tempfile::tempdir().unwrap();
        let machine = temp.path().join("machine");
        fs::create_dir_all(machine.join("rootfs")).unwrap();
        let runtime = PodmanRuntimePaths::under(temp.path(), config.id);
        runtime.prepare().unwrap();
        let definition = PodmanDefinition::for_machine(config, &machine, &runtime).unwrap();
        (temp, definition)
    }

    fn strings(definition: &PodmanDefinition) -> Vec<String> {
        definition
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn desktop_renderer_is_gles2_and_never_forced_to_software() {
        let (_temp, definition) = definition(&config());
        let arguments = strings(&definition);
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--env", "WLR_RENDERER=gles2"])
        );
        assert!(!arguments.iter().any(|argument| argument.contains("pixman")));
    }

    #[test]
    fn explicitly_blank_definition_uses_unmodified_podman_userns_and_security_defaults() {
        let mut config = config();
        config.custom_podman_arguments.clear();
        let (_temp, definition) = definition(&config);
        let arguments = strings(&definition);
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.starts_with("--userns"))
        );
        for forbidden in [
            "--privileged",
            "--security-opt",
            "--cap-add",
            "--cap-drop",
            "--ipc=host",
            "--pid=host",
        ] {
            assert!(!arguments.iter().any(|argument| argument == forbidden));
        }
        assert!(
            arguments
                .iter()
                .any(|argument| argument.ends_with("/rootfs"))
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.ends_with(":idmap"))
        );
    }

    #[test]
    fn new_desktop_mapping_is_podmans_native_rootless_default() {
        let config = config();
        assert!(config.custom_podman_arguments.is_empty());
        let (_temp, definition) = definition(&config);
        let arguments = strings(&definition);
        assert!(!arguments.iter().any(|argument| {
            argument == "--userns"
                || argument.starts_with("--userns=")
                || argument == "--uidmap"
                || argument.starts_with("--uidmap=")
                || argument == "--gidmap"
                || argument.starts_with("--gidmap=")
        }));
        assert!(
            arguments
                .windows(2)
                .any(|window| { window == ["--env", "container=podman"] })
        );
    }

    #[test]
    fn every_native_user_namespace_form_passes_through_unchanged() {
        for value in [
            "--userns=host",
            "--userns=keep-id",
            "--userns=auto",
            "--userns=nomap",
            "--uidmap=0:100000:65536 --gidmap=0:100000:65536",
        ] {
            let mut config = config();
            config.custom_podman_arguments = value.into();
            let (_temp, definition) = definition(&config);
            let arguments = strings(&definition);
            let expected = shell_words::split(value).unwrap();
            assert!(
                arguments
                    .windows(expected.len())
                    .any(|window| window == expected)
            );
        }
    }

    #[test]
    fn systemd_starts_as_root_and_native_user_override_remains_last() {
        let mut config = config();
        config.custom_podman_arguments = "--userns=keep-id --user=1234".into();
        let (_temp, definition) = definition(&config);
        let arguments = strings(&definition);
        let system_root = arguments
            .iter()
            .position(|argument| argument == "--user=0")
            .unwrap();
        let native_override = arguments
            .iter()
            .position(|argument| argument == "--user=1234")
            .unwrap();
        assert!(system_root < native_override);
        assert_eq!(
            arguments.last().map(String::as_str),
            Some("/usr/lib/buzzardos/runtime/current/libexec/buzzardos-init")
        );
    }

    #[test]
    fn custom_arguments_are_argv_not_a_shell_command() {
        let mut config = config();
        config.custom_podman_arguments =
            "--annotation 'example=value with spaces' --userns=keep-id".into();
        let (_temp, definition) = definition(&config);
        let arguments = strings(&definition);
        assert!(
            arguments
                .windows(2)
                .any(|window| { window == ["--annotation", "example=value with spaces"] })
        );
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--userns=keep-id")
        );
    }

    #[test]
    fn native_network_ports_gpus_and_shares_are_direct_podman_arguments() {
        let temp = tempfile::tempdir().unwrap();
        let share_dir = temp.path().join("shared,source");
        fs::create_dir(&share_dir).unwrap();
        let mut config = config();
        config.gpus = vec!["all".into()];
        config.integrations = IntegrationSettings {
            ports: vec![PortForward::new(PortDirection::HostToGuest)],
            media: crate::MediaSharing {
                host_microphone: true,
                ..crate::MediaSharing::default()
            },
        };
        config.shares = vec![SharedPath::from_host_path(share_dir).unwrap()];
        let (_machine, definition) = definition(&config);
        let arguments = strings(&definition);
        assert!(
            arguments
                .windows(2)
                .any(|window| window == ["--gpus", "all"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|window| { window == ["--publish", "127.0.0.1::47131/tcp"] })
        );
        assert!(
            arguments
                .windows(2)
                .any(|window| { window == ["--publish", "127.0.0.1:8080:8080/tcp"] })
        );
        assert!(arguments.iter().any(|argument| {
            argument.contains("type=bind") && argument.contains("shared,source")
        }));
    }

    #[test]
    fn definition_digest_changes_only_when_effective_argv_changes() {
        let config = config();
        let (_first_temp, first) = definition(&config);
        let (_second_temp, second) = definition(&config);
        // Runtime roots differ between these fixtures, so normalize through a
        // shared fixture for the actual stability assertion.
        assert_ne!(first.digest, second.digest);

        let temp = tempfile::tempdir().unwrap();
        let machine = temp.path().join("machine");
        fs::create_dir_all(machine.join("rootfs")).unwrap();
        let runtime = PodmanRuntimePaths::under(temp.path(), config.id);
        runtime.prepare().unwrap();
        let one = PodmanDefinition::for_machine(&config, &machine, &runtime).unwrap();
        let two = PodmanDefinition::for_machine(&config, &machine, &runtime).unwrap();
        assert_eq!(one.digest, two.digest);

        let mut changed = config;
        changed.custom_podman_arguments = "--userns=nomap".into();
        let changed = PodmanDefinition::for_machine(&changed, &machine, &runtime).unwrap();
        assert_ne!(one.digest, changed.digest);
    }
}
