// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
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
            schema: 2,
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
        if !matches!(config.schema, 1 | 2) {
            bail!("unsupported machine metadata schema {}", config.schema);
        }
        Self::validate_name(&config.name)?;
        Self::validate_gpus(&config.gpus)?;
        Self::validate_guest_scale(config.guest_scale_120)?;
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
