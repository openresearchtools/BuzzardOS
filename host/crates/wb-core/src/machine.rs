// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;
use uuid::Uuid;

use crate::WaylandCapabilities;

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
}

fn default_width() -> u32 {
    1280
}

fn default_height() -> u32 {
    800
}

fn default_title() -> String {
    "Wild Buzzard".into()
}

fn default_gpus() -> Vec<String> {
    vec!["all".into()]
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
            schema: 3,
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
        if gpus.is_empty() {
            bail!("at least one --gpu value is required");
        }
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
        if !matches!(config.schema, 1..=3) {
            bail!("unsupported machine metadata schema {}", config.schema);
        }
        Self::validate_name(&config.name)?;
        Self::validate_gpus(&config.gpus)?;
        Self::validate_guest_scale(config.guest_scale_120)?;
        config.integrations.validate(config.network)?;
        if !(320..=16384).contains(&config.width) || !(240..=16384).contains(&config.height) {
            bail!(
                "machine display size {}x{} is outside the supported range",
                config.width,
                config.height
            );
        }
        Ok(config)
    }

    pub fn save(&self, machine_dir: &Path) -> Result<()> {
        Self::validate_guest_scale(self.guest_scale_120)?;
        Self::validate_gpus(&self.gpus)?;
        self.integrations.validate(self.network)?;
        atomic_json(&machine_dir.join(Self::FILE), self)
    }

    pub fn validate_guest_scale(scale_120: Option<u32>) -> Result<()> {
        const PRESETS: [u32; 5] = [120, 150, 180, 210, 240];
        if scale_120.is_some_and(|scale| !PRESETS.contains(&scale)) {
            bail!("guest desktop scale must be Follow Host, 100%, 125%, 150%, 175%, or 200%");
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
    pub state: MachineState,
    pub launcher_pid: Option<u32>,
    pub container_pid: Option<u32>,
    pub updated_at: DateTime<Utc>,
    pub detail: Option<String>,
    #[serde(default)]
    pub display: Option<DisplayDiagnostics>,
    #[serde(default)]
    pub integrations: Option<IntegrationDiagnostics>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntegrationDiagnostics {
    pub schema: u32,
    pub generation: u64,
    #[serde(default)]
    pub ports: Vec<PortIntegrationDiagnostics>,
    pub guest_audio_output: MediaIntegrationDiagnostics,
    pub host_microphone: MediaIntegrationDiagnostics,
    pub host_camera: MediaIntegrationDiagnostics,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortIntegrationDiagnostics {
    pub id: Uuid,
    pub direction: PortDirection,
    pub protocol: PortProtocol,
    pub enabled: bool,
    pub active: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaIntegrationDiagnostics {
    pub enabled: bool,
    pub active: bool,
    #[serde(default)]
    pub host_pid: Option<u32>,
    #[serde(default)]
    pub guest_pid: Option<u32>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayDiagnostics {
    pub host: WaylandCapabilities,
    #[serde(default)]
    pub renderer: String,
    #[serde(default)]
    pub selected_render_device_identity: Option<String>,
    #[serde(default)]
    pub exposed_devices: Vec<String>,
    pub render_nodes: Vec<String>,
    #[serde(default)]
    pub render_device_identities: Vec<String>,
    #[serde(default)]
    pub host_device_identity: Option<String>,
    #[serde(default)]
    pub application_devices: Vec<String>,
    #[serde(default)]
    pub window: Option<WindowDiagnostics>,
    #[serde(default)]
    pub presentation: Option<PresentationDiagnostics>,
    pub zero_copy: String,
    pub detail: String,
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
            state,
            launcher_pid: Some(std::process::id()),
            container_pid: None,
            updated_at: Utc::now(),
            detail: None,
            display: None,
            integrations: None,
        }
    }

    pub fn load(machine_dir: &Path) -> Result<Option<Self>> {
        let path = machine_dir.join(Self::FILE);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, machine_dir: &Path) -> Result<()> {
        atomic_json(&machine_dir.join(Self::FILE), self)
    }
}

fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut temp = tempfile_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, value).context("serializing state")?;
    temp.write_all(b"\n").context("finishing state file")?;
    temp.sync_all().context("syncing state file")?;
    fs::rename(temp_path(&temp), path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

fn tempfile_in(parent: &Path) -> Result<fs::File> {
    for attempt in 0..100 {
        let path = parent.join(format!(".wb-state-{}-{attempt}", std::process::id()));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
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
        config.save(temp.path()).unwrap();
        assert!(MachineConfig::load(temp.path()).is_err());

        config.name = "valid".into();
        config.schema = 99;
        config.save(temp.path()).unwrap();
        assert!(MachineConfig::load(temp.path()).is_err());
    }
}
