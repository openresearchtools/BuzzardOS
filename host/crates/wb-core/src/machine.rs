// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// New machines use Podman's native rootless user-namespace default. Users may
/// select any other stock Podman mapping, including keep-id, host, auto,
/// nomap, or explicit uidmap/gidmap arguments, without a Buzzard translation
/// layer.
pub const DEFAULT_PODMAN_ARGUMENTS: &str = "";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    #[default]
    User,
    Host,
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum PortDirection {
    /// A host listener forwards into the machine.
    HostToGuest,
    /// A guest listener forwards to one explicitly authorized host target.
    GuestToHost,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PortProtocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortForward {
    pub id: Uuid,
    #[serde(default)]
    pub enabled: bool,
    pub direction: PortDirection,
    pub protocol: PortProtocol,
    /// Host bind address for host-to-guest mappings and host destination
    /// address for guest-to-host mappings.
    pub host_address: String,
    pub host_port: u16,
    /// Guest destination address for host-to-guest mappings and guest bind
    /// address for guest-to-host mappings.
    pub guest_address: String,
    pub guest_port: u16,
}

impl PortForward {
    pub fn new(direction: PortDirection) -> Self {
        let guest_address = match direction {
            PortDirection::HostToGuest => "10.0.2.100",
            PortDirection::GuestToHost => "127.0.0.1",
        };
        Self {
            id: Uuid::new_v4(),
            enabled: true,
            direction,
            protocol: PortProtocol::Tcp,
            host_address: "127.0.0.1".into(),
            host_port: 8080,
            guest_address: guest_address.into(),
            guest_port: 8080,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let host: IpAddr = self
            .host_address
            .parse()
            .with_context(|| format!("invalid host address '{}'", self.host_address))?;
        let guest: IpAddr = self
            .guest_address
            .parse()
            .with_context(|| format!("invalid guest address '{}'", self.guest_address))?;
        if !host.is_ipv4() || !guest.is_ipv4() {
            bail!("port forwarding currently requires IPv4 addresses");
        }
        if self.host_port == 0 || self.guest_port == 0 {
            bail!("port forwarding ports must be between 1 and 65535");
        }
        match self.direction {
            PortDirection::HostToGuest => {
                if guest.is_unspecified() || guest.is_multicast() {
                    bail!("host-to-guest destination must be a unicast guest address");
                }
            }
            PortDirection::GuestToHost => {
                if host.is_unspecified() || host.is_multicast() {
                    bail!("guest-to-host destination must be a unicast host address");
                }
                if !(guest.is_loopback() || guest.is_unspecified()) {
                    bail!("guest-to-host listener must bind 127.0.0.1 or 0.0.0.0 inside the guest");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaSharing {
    /// Play the guest's default audio sink on the host's default output.
    #[serde(default)]
    pub guest_audio_output: bool,
    /// Create a private guest source fed from the host's selected microphone.
    #[serde(default)]
    pub host_microphone: bool,
    /// Create a private guest camera source fed from the host's selected camera.
    #[serde(default)]
    pub host_camera: bool,
    /// Stable host PipeWire sink name. `None` follows the current host default.
    #[serde(default)]
    pub audio_target: Option<String>,
    /// Stable host PipeWire source name. `None` follows the current host default.
    #[serde(default)]
    pub microphone_target: Option<String>,
    /// Stable host PipeWire camera name. `None` follows the current host default.
    #[serde(default)]
    pub camera_target: Option<String>,
}

impl MediaSharing {
    pub fn validate(&self) -> Result<()> {
        for (label, target) in [
            ("audio output", self.audio_target.as_deref()),
            ("microphone", self.microphone_target.as_deref()),
            ("camera", self.camera_target.as_deref()),
        ] {
            if let Some(target) = target {
                if target.is_empty() || target.len() > 256 || target.contains(['\0', '\n', '\r']) {
                    bail!("{label} PipeWire target is invalid");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntegrationSettings {
    #[serde(default)]
    pub ports: Vec<PortForward>,
    #[serde(default)]
    pub media: MediaSharing,
}

/// One host-authorized file or directory exposed below `/shared`.
///
/// The host path is intentionally absolute: shares describe resources on the
/// current host and are disabled by a missing source after moving/importing a
/// machine. The guest name is one safe path component, never a guest-chosen
/// mount destination.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SharedPath {
    pub id: Uuid,
    pub host_path: PathBuf,
    pub guest_name: String,
    #[serde(default)]
    pub read_only: bool,
}

impl SharedPath {
    pub fn from_host_path(host_path: PathBuf) -> Result<Self> {
        if !host_path.is_absolute() {
            bail!("shared host path must be absolute: {}", host_path.display());
        }
        let metadata = fs::symlink_metadata(&host_path)
            .with_context(|| format!("inspecting shared path {}", host_path.display()))?;
        if metadata.file_type().is_symlink() || !(metadata.is_file() || metadata.is_dir()) {
            bail!("shared path must be a regular file or real directory");
        }
        let host_path = host_path
            .canonicalize()
            .with_context(|| format!("resolving shared path {}", host_path.display()))?;
        let guest_name = host_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("shared host path must have a UTF-8 file name")?
            .to_owned();
        let share = Self {
            id: Uuid::new_v4(),
            host_path,
            guest_name,
            read_only: false,
        };
        share.validate_metadata()?;
        Ok(share)
    }

    pub fn guest_path(&self) -> PathBuf {
        Path::new("/shared").join(&self.guest_name)
    }

    pub fn validate_metadata(&self) -> Result<()> {
        if !self.host_path.is_absolute() {
            bail!(
                "shared host path must be absolute: {}",
                self.host_path.display()
            );
        }
        if self.guest_name.is_empty()
            || self.guest_name.len() > 255
            || matches!(self.guest_name.as_str(), "." | "..")
            || self.guest_name.contains(['/', '\0', '\n', '\r'])
        {
            bail!("shared guest name must be one safe non-empty path component");
        }
        Ok(())
    }
}

impl IntegrationSettings {
    pub fn validate(&self, network: NetworkMode) -> Result<()> {
        if !matches!(network, NetworkMode::User)
            && (self.ports.iter().any(|port| port.enabled)
                || self.media.guest_audio_output
                || self.media.host_microphone
                || self.media.host_camera)
        {
            bail!("live ports and media require private user-mode networking");
        }
        if self.ports.len() > 128 {
            bail!("a machine may configure at most 128 port mappings");
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut listeners = std::collections::BTreeSet::new();
        for port in &self.ports {
            port.validate()?;
            if !ids.insert(port.id) {
                bail!("duplicate port mapping id {}", port.id);
            }
            if port.enabled {
                let listener = match port.direction {
                    PortDirection::HostToGuest => (
                        0_u8,
                        port.protocol,
                        port.host_address.as_str(),
                        port.host_port,
                    ),
                    PortDirection::GuestToHost => (
                        1_u8,
                        port.protocol,
                        port.guest_address.as_str(),
                        port.guest_port,
                    ),
                };
                if !listeners.insert(listener) {
                    bail!("two enabled mappings use the same listening address and port");
                }
            }
        }
        self.media.validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineConfig {
    pub schema: u32,
    pub id: Uuid,
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub image_digest: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub network: NetworkMode,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default = "default_gpus")]
    pub gpus: Vec<String>,
    /// Guest desktop UI scale in 1/120 units. `None` follows the host surface
    /// scale while preserving a native-resolution final dmabuf.
    #[serde(default)]
    pub guest_scale_120: Option<u32>,
    /// Host-authorized, live-reconciled integration settings. This metadata
    /// is outside the guest rootfs and cannot be changed by guest processes.
    #[serde(default)]
    pub integrations: IntegrationSettings,
    /// Optional host-authorized file and directory shares. An empty list
    /// exposes no host filesystem path to the guest.
    #[serde(default)]
    pub shares: Vec<SharedPath>,
    /// Unrestricted Podman creation arguments entered by the user. The value
    /// is parsed into argv without invoking a shell and is otherwise passed
    /// to Podman unchanged.
    #[serde(default)]
    pub custom_podman_arguments: String,
    /// Authenticated OCI process metadata retained for portability. Buzzard
    /// OS always boots systemd as PID 1, but applies the image environment to
    /// that guest process and preserves the remaining metadata on export.
    #[serde(default)]
    pub oci: OciImageMetadata,
    /// Optional verified install media retained below this machine's cache.
    /// The running rootfs never depends on this archive.
    #[serde(default)]
    pub retained_oci_archive: Option<RetainedOciArchive>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetainedOciArchive {
    pub relative_path: String,
    pub sha256: String,
    pub size: u64,
}

impl RetainedOciArchive {
    pub fn validate(&self) -> Result<()> {
        if self.relative_path != "cache/source.oci.tar" {
            bail!("retained OCI archive must use cache/source.oci.tar");
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("retained OCI archive has an invalid SHA-256 digest");
        }
        if self.size == 0 {
            bail!("retained OCI archive cannot be empty");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OciImageMetadata {
    #[serde(default)]
    pub environment: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub stop_signal: Option<String>,
}

impl OciImageMetadata {
    const MAX_ITEMS: usize = 4096;
    const MAX_TEXT_BYTES: usize = 1024 * 1024;

    pub fn validate(&self) -> Result<()> {
        if self.environment.len() > Self::MAX_ITEMS
            || self.labels.len() > Self::MAX_ITEMS
            || self.entrypoint.len() > Self::MAX_ITEMS
            || self.command.len() > Self::MAX_ITEMS
        {
            bail!("OCI process metadata contains too many entries");
        }
        let mut total = 0_usize;
        for variable in &self.environment {
            let (name, _) = variable
                .split_once('=')
                .context("OCI environment entry has no '=' separator")?;
            if name.is_empty()
                || !name.bytes().enumerate().all(|(index, byte)| {
                    byte == b'_'
                        || byte.is_ascii_alphanumeric() && (index != 0 || !byte.is_ascii_digit())
                })
            {
                bail!("OCI environment entry has an invalid variable name");
            }
            total = total.saturating_add(variable.len());
        }
        for (key, value) in &self.labels {
            if key.is_empty() {
                bail!("OCI label name cannot be empty");
            }
            total = total.saturating_add(key.len()).saturating_add(value.len());
        }
        for value in self.entrypoint.iter().chain(&self.command) {
            total = total.saturating_add(value.len());
        }
        for value in [
            self.working_dir.as_deref(),
            self.user.as_deref(),
            self.stop_signal.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            total = total.saturating_add(value.len());
        }
        if total > Self::MAX_TEXT_BYTES {
            bail!("OCI process metadata exceeds the 1 MiB limit");
        }
        Ok(())
    }

    pub fn environment_pairs(&self) -> Result<Vec<(&str, &str)>> {
        self.validate()?;
        self.environment
            .iter()
            .map(|variable| {
                variable
                    .split_once('=')
                    .context("OCI environment entry has no '=' separator")
            })
            .collect()
    }
}

fn default_width() -> u32 {
    1280
}

fn default_height() -> u32 {
    800
}

fn default_title() -> String {
    "Buzzard OS".into()
}

fn default_gpus() -> Vec<String> {
    Vec::new()
}

impl MachineConfig {
    pub const FILE: &'static str = "machine.json";

    pub fn new(
        name: String,
        image: String,
        image_digest: String,
        network: NetworkMode,
        gpus: Vec<String>,
    ) -> Self {
        Self {
            schema: 1,
            id: Uuid::new_v4(),
            title: name.clone(),
            name,
            image,
            image_digest: Some(image_digest),
            created_at: Utc::now(),
            network,
            width: default_width(),
            height: default_height(),
            gpus,
            guest_scale_120: None,
            integrations: IntegrationSettings::default(),
            shares: Vec::new(),
            custom_podman_arguments: DEFAULT_PODMAN_ARGUMENTS.into(),
            oci: OciImageMetadata::default(),
            retained_oci_archive: None,
        }
    }

    pub fn validate_name(name: &str) -> Result<()> {
        if name.is_empty() || name.len() > 64 {
            bail!("machine name must contain between 1 and 64 characters");
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            bail!("machine name may contain only letters, digits, '-' and '_'");
        }
        Ok(())
    }

    pub fn validate_gpus(gpus: &[String]) -> Result<()> {
        if gpus.iter().any(|gpu| gpu == "all") && gpus.len() != 1 {
            bail!("GPU selection 'all' cannot be combined with another GPU");
        }
        for gpu in gpus {
            let valid_index = !gpu.is_empty() && gpu.bytes().all(|byte| byte.is_ascii_digit());
            let valid_uuid = gpu.strip_prefix("GPU-").is_some_and(|uuid| {
                !uuid.is_empty()
                    && uuid
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
            });
            if gpu != "all" && !valid_index && !valid_uuid {
                bail!("GPU '{gpu}' must be 'all', a numeric NVIDIA index, or a GPU UUID");
            }
        }
        Ok(())
    }

    pub fn load(machine_dir: &Path) -> Result<Self> {
        let path = machine_dir.join(Self::FILE);
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        if config.schema != 1 {
            bail!("unsupported machine metadata schema {}", config.schema);
        }
        Self::validate_name(&config.name)?;
        Self::validate_gpus(&config.gpus)?;
        Self::validate_guest_scale(config.guest_scale_120)?;
        config.oci.validate()?;
        if let Some(archive) = &config.retained_oci_archive {
            archive.validate()?;
        }
        config.integrations.validate(config.network)?;
        Self::validate_shares(&config.shares)?;
        Self::parse_custom_podman_arguments(&config.custom_podman_arguments)?;
        Self::validate_display_size(config.width, config.height)?;
        Ok(config)
    }

    pub fn save(&self, machine_dir: &Path) -> Result<()> {
        if self.schema != 1 {
            bail!("unsupported machine metadata schema {}", self.schema);
        }
        Self::validate_name(&self.name)?;
        Self::validate_guest_scale(self.guest_scale_120)?;
        Self::validate_gpus(&self.gpus)?;
        Self::validate_display_size(self.width, self.height)?;
        self.oci.validate()?;
        if let Some(archive) = &self.retained_oci_archive {
            archive.validate()?;
        }
        self.integrations.validate(self.network)?;
        Self::validate_shares(&self.shares)?;
        Self::parse_custom_podman_arguments(&self.custom_podman_arguments)?;
        atomic_json(&machine_dir.join(Self::FILE), self)
    }

    pub fn validate_shares(shares: &[SharedPath]) -> Result<()> {
        if shares.len() > 128 {
            bail!("a machine may configure at most 128 shared paths");
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut names = std::collections::BTreeSet::new();
        let mut paths = std::collections::BTreeSet::new();
        for share in shares {
            share.validate_metadata()?;
            if !ids.insert(share.id) {
                bail!("duplicate shared-path id {}", share.id);
            }
            if !names.insert(share.guest_name.as_str()) {
                bail!("two shared paths use guest name '{}'", share.guest_name);
            }
            if !paths.insert(&share.host_path) {
                bail!(
                    "host path is shared more than once: {}",
                    share.host_path.display()
                );
            }
        }
        Ok(())
    }

    pub fn validate_guest_scale(scale_120: Option<u32>) -> Result<()> {
        const PRESETS: [u32; 5] = [120, 150, 180, 210, 240];
        if scale_120.is_some_and(|scale| !PRESETS.contains(&scale)) {
            bail!("guest desktop scale must be Follow Host, 100%, 125%, 150%, 175%, or 200%");
        }
        Ok(())
    }

    pub fn parse_custom_podman_arguments(value: &str) -> Result<Vec<String>> {
        shell_words::split(value).context("parsing custom Podman arguments")
    }

    fn validate_display_size(width: u32, height: u32) -> Result<()> {
        if !(320..=16384).contains(&width) || !(240..=16384).contains(&height) {
            bail!("machine display size {width}x{height} is outside the supported range");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MachineState {
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    pub schema: u32,
    pub state: MachineState,
    #[serde(default)]
    pub container_id: Option<String>,
    #[serde(default)]
    pub definition_digest: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowDiagnostics {
    pub schema: u32,
    pub boundary: String,
    pub toplevels: usize,
    pub width: u32,
    pub height: u32,
    pub title: String,
    pub app_id: String,
    pub decorations: String,
    pub close_requested: bool,
    #[serde(default)]
    pub maximized: bool,
    #[serde(default)]
    pub minimized: bool,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresentationDiagnostics {
    pub schema: u32,
    pub transport: String,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub modifier: String,
    pub planes: u32,
    #[serde(default = "default_scale_120")]
    pub scale_120: u32,
    #[serde(default)]
    pub viewport_width: u32,
    #[serde(default)]
    pub viewport_height: u32,
    #[serde(default)]
    pub native_resolution: bool,
    pub explicit_sync: String,
    pub presentation_feedback: bool,
    pub presented: bool,
    pub discarded: bool,
    pub vsync: bool,
    #[serde(default)]
    pub zero_copy: bool,
    pub sequence: u64,
    pub refresh_ns: u32,
    pub timestamp_ns: u64,
    #[serde(default)]
    pub gtk_subsurface_offload: bool,
    #[serde(default)]
    pub submitted_frames: u64,
    #[serde(default)]
    pub superseded_before_paint: u64,
    #[serde(default)]
    pub painted_frames: u64,
    /// Frames consumed from the one guest scanout while the host compositor
    /// was not issuing frame callbacks (for example while minimized).
    #[serde(default)]
    pub background_paced_frames: u64,
    /// Presentation-feedback objects deliberately discarded while no real
    /// host-vblank result was available.
    #[serde(default)]
    pub background_feedback_discarded: u64,
    /// `host-vblank`, `internal-hidden-window-clock`, or `not-started`.
    #[serde(default)]
    pub last_pacing_source: String,
    #[serde(default)]
    pub presented_frames: u64,
    #[serde(default)]
    pub dropped_frames: u64,
    #[serde(default)]
    pub presentation_feedback_unavailable: u64,
    #[serde(default)]
    pub released_frames: u64,
    #[serde(default)]
    pub last_released_frame_id: u64,
    #[serde(default)]
    pub last_buffer_residency_us: u64,
    #[serde(default)]
    pub maximum_buffer_residency_us: u64,
    #[serde(default)]
    pub explicit_sync_frames: u64,
    #[serde(default)]
    pub last_acquire_wait_us: u64,
    #[serde(default)]
    pub maximum_acquire_wait_us: u64,
    #[serde(default)]
    pub last_presentation_time_us: i64,
    #[serde(default)]
    pub last_presented_frame_interval_us: i64,
    #[serde(default)]
    pub last_refresh_interval_us: i64,
    #[serde(default)]
    pub last_submission_time_us: u64,
    #[serde(default)]
    pub last_submission_to_presentation_us: u64,
    #[serde(default)]
    pub maximum_submission_to_presentation_us: u64,
}

fn default_scale_120() -> u32 {
    120
}

impl Default for PresentationDiagnostics {
    fn default() -> Self {
        Self {
            schema: 5,
            transport: "dmabuf".into(),
            width: 0,
            height: 0,
            format: 0,
            modifier: "unknown".into(),
            planes: 0,
            scale_120: default_scale_120(),
            viewport_width: 0,
            viewport_height: 0,
            native_resolution: false,
            explicit_sync: "not-negotiated".into(),
            presentation_feedback: false,
            presented: false,
            discarded: false,
            vsync: false,
            zero_copy: false,
            sequence: 0,
            refresh_ns: 0,
            timestamp_ns: 0,
            gtk_subsurface_offload: false,
            submitted_frames: 0,
            superseded_before_paint: 0,
            painted_frames: 0,
            background_paced_frames: 0,
            background_feedback_discarded: 0,
            last_pacing_source: "not-started".into(),
            presented_frames: 0,
            dropped_frames: 0,
            presentation_feedback_unavailable: 0,
            released_frames: 0,
            last_released_frame_id: 0,
            last_buffer_residency_us: 0,
            maximum_buffer_residency_us: 0,
            explicit_sync_frames: 0,
            last_acquire_wait_us: 0,
            maximum_acquire_wait_us: 0,
            last_presentation_time_us: 0,
            last_presented_frame_interval_us: 0,
            last_refresh_interval_us: 0,
            last_submission_time_us: 0,
            last_submission_to_presentation_us: 0,
            maximum_submission_to_presentation_us: 0,
        }
    }
}

impl RuntimeState {
    pub const FILE: &'static str = "runtime.json";

    pub fn new(state: MachineState) -> Self {
        Self {
            schema: 1,
            state,
            container_id: None,
            definition_digest: None,
            updated_at: Utc::now(),
            detail: None,
        }
    }

    pub fn load(machine_dir: &Path) -> Result<Option<Self>> {
        let path = machine_dir.join(Self::FILE);
        match fs::read(&path) {
            Ok(bytes) => {
                let state: Self = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if state.schema != 1 {
                    bail!("unsupported runtime metadata schema {}", state.schema);
                }
                Ok(Some(state))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, machine_dir: &Path) -> Result<()> {
        if self.schema != 1 {
            bail!("unsupported runtime metadata schema {}", self.schema);
        }
        atomic_json(&machine_dir.join(Self::FILE), self)
    }
}

fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut temp = tempfile_in(parent)?;
    temp.set_permissions(fs::Permissions::from_mode(0o600))
        .context("securing temporary state file")?;
    serde_json::to_writer_pretty(&mut temp, value).context("serializing state")?;
    temp.write_all(b"\n").context("finishing state file")?;
    temp.sync_all().context("syncing state file")?;
    fs::rename(temp_path(&temp), path).with_context(|| format!("saving {}", path.display()))?;
    fs::File::open(parent)
        .with_context(|| format!("opening {} for sync", parent.display()))?
        .sync_all()
        .with_context(|| format!("syncing {}", parent.display()))?;
    Ok(())
}

fn tempfile_in(parent: &Path) -> Result<fs::File> {
    for attempt in 0..100 {
        let path = parent.join(format!(".wb-state-{}-{attempt}", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("creating temporary state file"),
        }
    }
    bail!("could not create a unique temporary state file")
}

fn temp_path(file: &fs::File) -> std::path::PathBuf {
    Path::new("/proc/self/fd")
        .join(file.as_raw_fd().to_string())
        .read_link()
        .expect("Linux /proc must expose the temporary file path")
}

use std::os::fd::AsRawFd;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn validates_machine_names() {
        assert!(MachineConfig::validate_name("machine-01_dev").is_ok());
        assert!(MachineConfig::validate_name("../escape").is_err());
        assert!(MachineConfig::validate_name("").is_err());
    }

    #[test]
    fn validates_gpu_selections() {
        assert!(MachineConfig::validate_gpus(&["all".into()]).is_ok());
        assert!(MachineConfig::validate_gpus(&["0".into(), "2".into()]).is_ok());
        assert!(
            MachineConfig::validate_gpus(&["GPU-f832efd8-97ec-6d10-046f-f7a8e84b1c3b".into()])
                .is_ok()
        );
        assert!(MachineConfig::validate_gpus(&["all".into(), "1".into()]).is_err());
        assert!(MachineConfig::validate_gpus(&["../device".into()]).is_err());
    }

    #[test]
    fn validates_guest_desktop_scale_presets() {
        for scale in [None, Some(120), Some(150), Some(180), Some(210), Some(240)] {
            assert!(MachineConfig::validate_guest_scale(scale).is_ok());
        }
        assert!(MachineConfig::validate_guest_scale(Some(160)).is_err());
        assert!(MachineConfig::validate_guest_scale(Some(0)).is_err());
    }

    #[test]
    fn validates_and_splits_oci_environment_without_shell_parsing() {
        let metadata = OciImageMetadata {
            environment: vec!["PATH=/custom/bin:/usr/bin".into(), "EMPTY=".into()],
            ..OciImageMetadata::default()
        };
        assert_eq!(
            metadata.environment_pairs().unwrap(),
            vec![("PATH", "/custom/bin:/usr/bin"), ("EMPTY", "")]
        );
        for invalid in ["NO_SEPARATOR", "1BAD=value", "BAD-NAME=value", "=value"] {
            let metadata = OciImageMetadata {
                environment: vec![invalid.into()],
                ..OciImageMetadata::default()
            };
            assert!(metadata.validate().is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn validates_bidirectional_port_mappings_and_conflicts() {
        let mut integrations = IntegrationSettings::default();
        integrations
            .ports
            .push(PortForward::new(PortDirection::HostToGuest));
        assert!(integrations.validate(NetworkMode::User).is_ok());
        assert!(integrations.validate(NetworkMode::None).is_err());

        let mut reverse = PortForward::new(PortDirection::GuestToHost);
        reverse.guest_port = 9000;
        reverse.host_port = 3000;
        integrations.ports.push(reverse.clone());
        assert!(integrations.validate(NetworkMode::User).is_ok());

        reverse.id = Uuid::new_v4();
        integrations.ports.push(reverse);
        assert!(integrations.validate(NetworkMode::User).is_err());
    }

    #[test]
    fn rejects_unsafe_or_invalid_media_and_port_values() {
        let mut forward = PortForward::new(PortDirection::HostToGuest);
        forward.host_port = 0;
        assert!(forward.validate().is_err());
        forward.host_port = 8080;
        forward.guest_address = "0.0.0.0".into();
        assert!(forward.validate().is_err());

        let mut reverse = PortForward::new(PortDirection::GuestToHost);
        reverse.guest_address = "192.0.2.2".into();
        assert!(reverse.validate().is_err());

        let media = MediaSharing {
            microphone_target: Some("bad\nnode".into()),
            ..MediaSharing::default()
        };
        assert!(media.validate().is_err());
    }

    #[test]
    fn load_revalidates_mutable_machine_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = MachineConfig::new(
            "valid".into(),
            "example.invalid/image".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        );
        config.name = "../escape".into();
        assert!(config.save(temp.path()).is_err());
        fs::write(
            temp.path().join(MachineConfig::FILE),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        assert!(MachineConfig::load(temp.path()).is_err());

        config.name = "valid".into();
        config.schema = 99;
        assert!(config.save(temp.path()).is_err());
        fs::write(
            temp.path().join(MachineConfig::FILE),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        assert!(MachineConfig::load(temp.path()).is_err());
    }

    #[test]
    fn save_rejects_a_machine_that_load_would_reject() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = MachineConfig::new(
            "invalid-display".into(),
            "fixture".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        );
        config.width = 0;

        assert!(config.save(temp.path()).is_err());
        assert!(!temp.path().join(MachineConfig::FILE).exists());
    }

    #[test]
    fn machine_and_runtime_metadata_are_private() {
        let temp = tempfile::tempdir().unwrap();
        let config = MachineConfig::new(
            "private".into(),
            "fixture".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        );
        config.save(temp.path()).unwrap();
        RuntimeState::new(MachineState::Stopped)
            .save(temp.path())
            .unwrap();

        for name in [MachineConfig::FILE, RuntimeState::FILE] {
            assert_eq!(
                fs::metadata(temp.path().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{name} exposed private machine metadata"
            );
        }
    }
}
