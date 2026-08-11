// SPDX-License-Identifier: AGPL-3.0-or-later

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use wb_core::MachineConfig;

#[derive(Debug, Clone, Parser)]
#[command(name = "wildbuzzard-display", version)]
pub(crate) struct Launch {
    /// Real host Wayland socket. This path is never passed into the guest.
    #[arg(long)]
    pub(crate) host: PathBuf,

    /// Private Wayland socket exposed only to the nested guest compositor.
    #[arg(long)]
    pub(crate) listen: PathBuf,

    /// Host-only fixed-command socket. It is never mounted into the guest.
    #[arg(long)]
    pub(crate) control: PathBuf,

    /// Guest-visible, narrowly typed display-control socket. It accepts only
    /// the enumerated scale and transactional keyboard-map requests and is
    /// separate from host controls.
    #[arg(long)]
    pub(crate) guest_scale_control: PathBuf,

    /// Guest-owned fixed clipboard-agent endpoint. The native application is
    /// only a client of this socket after an explicit header action; the guest
    /// cannot use it to call into the host or read the host clipboard.
    #[arg(long)]
    pub(crate) guest_clipboard_control: PathBuf,

    /// Immutable XKB definitions bundled with the pinned guest compositor
    /// runtime. Physical host input and Sway compile from these exact bytes.
    #[arg(long)]
    pub(crate) xkb_config_root: PathBuf,

    /// Validated persistent machine directory used by native lifecycle UI.
    #[arg(long)]
    pub(crate) machine_dir: PathBuf,

    /// Host-private directory receiving window and presentation diagnostics.
    #[arg(long)]
    pub(crate) status_dir: PathBuf,

    /// Read-only-in-guest directory receiving the native monitor mode.
    #[arg(long)]
    pub(crate) output_state_dir: PathBuf,

    /// Initial guest monitor width in logical pixels.
    #[arg(long)]
    pub(crate) initial_width: u32,

    /// Initial guest monitor height in logical pixels.
    #[arg(long)]
    pub(crate) initial_height: u32,

    /// Guest desktop UI scale in 1/120 units. Omit to follow the host scale.
    #[arg(long)]
    pub(crate) guest_scale_120: Option<u32>,

    /// DRM render node used to import guest syncobj timelines.
    ///
    /// When omitted, the private display does not advertise explicit sync.
    #[arg(long)]
    pub(crate) sync_drm_device: Option<PathBuf>,

    /// Native host window title.
    #[arg(long, default_value = "Wild Buzzard")]
    pub(crate) title: String,

    /// Native host application identifier.
    #[arg(long, default_value = "org.openresearchtools.wildbuzzard")]
    pub(crate) app_id: String,

    /// Test-only fractional scale override, in units of 1/120.
    #[arg(long, hide = true)]
    pub(crate) test_fractional_scale_120: Option<u32>,
}

impl Launch {
    pub(crate) fn validate(mut self) -> Result<Self> {
        validate_host_socket(&self.host)?;
        self.host = self
            .host
            .canonicalize()
            .with_context(|| format!("resolving host Wayland socket {}", self.host.display()))?;
        self.machine_dir = canonical_directory(&self.machine_dir, "machine directory")?;
        self.status_dir = canonical_directory(&self.status_dir, "status directory")?;
        self.output_state_dir =
            canonical_directory(&self.output_state_dir, "output state directory")?;
        self.xkb_config_root = canonical_directory(&self.xkb_config_root, "XKB config root")?;
        for required in ["rules/evdev", "symbols", "keycodes", "types", "compat"] {
            let path = self.xkb_config_root.join(required);
            let resolved = path
                .canonicalize()
                .with_context(|| format!("resolving bundled XKB definition {}", path.display()))?;
            if !resolved.starts_with(&self.xkb_config_root) {
                bail!(
                    "bundled XKB definition {} resolves outside its immutable root",
                    path.display()
                );
            }
        }
        if let Some(device) = self.sync_drm_device.as_ref() {
            let metadata = fs::symlink_metadata(device)
                .with_context(|| format!("inspecting sync DRM device {}", device.display()))?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_char_device() {
                bail!(
                    "sync DRM device {} must be a real character device",
                    device.display()
                );
            }
            if libc::major(metadata.rdev()) != 226 {
                bail!("sync DRM device {} is not a DRM device", device.display());
            }
            self.sync_drm_device = Some(
                device
                    .canonicalize()
                    .with_context(|| format!("resolving sync DRM device {}", device.display()))?,
            );
        }

        let listen_parent = canonical_parent(&self.listen, "guest display socket")?;
        let control_parent = canonical_parent(&self.control, "host control socket")?;
        let guest_scale_control_parent = canonical_parent(
            &self.guest_scale_control,
            "guest display-scale control socket",
        )?;
        let guest_clipboard_control_parent = canonical_parent(
            &self.guest_clipboard_control,
            "guest clipboard-agent socket",
        )?;
        if self.listen.parent() != Some(listen_parent.as_path()) {
            bail!("guest display socket parent must not contain symlink aliases");
        }
        if self.control.parent() != Some(control_parent.as_path()) {
            bail!("host control socket parent must not contain symlink aliases");
        }
        if self.guest_scale_control.parent() != Some(guest_scale_control_parent.as_path()) {
            bail!("guest display-scale control socket parent must not contain symlink aliases");
        }
        if self.guest_clipboard_control.parent() != Some(guest_clipboard_control_parent.as_path()) {
            bail!("guest clipboard-agent socket parent must not contain symlink aliases");
        }
        if self.listen == self.control
            || self.listen == self.guest_scale_control
            || self.listen == self.guest_clipboard_control
            || self.control == self.guest_scale_control
            || self.control == self.guest_clipboard_control
            || self.guest_scale_control == self.guest_clipboard_control
        {
            bail!(
                "display, host-control, guest display-scale, and guest clipboard sockets must be distinct"
            );
        }
        if listen_parent == control_parent {
            bail!("guest display and host control sockets require separate directories");
        }
        if guest_scale_control_parent != listen_parent {
            bail!(
                "guest display-scale control socket must share the private guest display directory"
            );
        }
        if guest_clipboard_control_parent != listen_parent {
            bail!("guest clipboard-agent socket must share the private guest display directory");
        }
        if [&self.status_dir, &self.output_state_dir]
            .iter()
            .any(|directory| {
                **directory == listen_parent
                    || **directory == control_parent
                    || **directory == guest_scale_control_parent
                    || **directory == guest_clipboard_control_parent
            })
        {
            bail!("display sockets and diagnostic state require separate directories");
        }
        if self.status_dir == self.output_state_dir {
            bail!("host diagnostics and guest-visible output state require separate directories");
        }
        require_private_directory(&self.status_dir, "status directory")?;
        require_private_directory(&self.output_state_dir, "output state directory")?;

        if !(320..=16_384).contains(&self.initial_width)
            || !(240..=16_384).contains(&self.initial_height)
        {
            bail!(
                "initial monitor size {}x{} is outside 320x240..16384x16384",
                self.initial_width,
                self.initial_height
            );
        }
        if self
            .test_fractional_scale_120
            .is_some_and(|scale| !(120..=960).contains(&scale))
        {
            bail!("test fractional scale must be between 120 and 960");
        }
        MachineConfig::validate_guest_scale(self.guest_scale_120)?;
        if self.title.is_empty() || self.title.contains(['\n', '\0']) {
            bail!("host title must be a non-empty single line");
        }
        if self.app_id.is_empty()
            || self
                .app_id
                .bytes()
                .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')))
        {
            bail!("host application ID contains unsupported characters");
        }

        Ok(self)
    }

    /// Selects the exact validated host Wayland socket before GTK or any
    /// worker thread is initialized. Wayland accepts an absolute socket path
    /// as `WAYLAND_DISPLAY`, so the frontend never falls back to a different
    /// display inherited from mutable guest state.
    pub(crate) fn configure_native_backend(&self) {
        // SAFETY: main calls this before GatewaySockets starts either worker
        // thread and before GTK initializes GLib/GDK.
        unsafe {
            std::env::set_var("GDK_BACKEND", "wayland");
            std::env::set_var("WAYLAND_DISPLAY", &self.host);
        }
    }
}

fn validate_host_socket(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting host Wayland socket {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
        bail!(
            "host Wayland endpoint {} must be a real Unix socket",
            path.display()
        );
    }
    Ok(())
}

fn canonical_parent(path: &Path, description: &str) -> Result<PathBuf> {
    let parent = path
        .parent()
        .with_context(|| format!("{description} has no parent directory"))?;
    canonical_directory(parent, &format!("{description} directory"))
}

fn canonical_directory(path: &Path, description: &str) -> Result<PathBuf> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolving {description} {}", path.display()))?;
    if !canonical.is_dir() {
        bail!("{description} {} is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn require_private_directory(path: &Path, description: &str) -> Result<()> {
    let mode = fs::metadata(path)
        .with_context(|| format!("inspecting {description} {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o022 != 0 {
        bail!(
            "{description} {} must not be group- or world-writable",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_id_is_restricted_to_desktop_safe_characters() {
        assert!(
            Launch {
                host: "/missing".into(),
                listen: "/missing".into(),
                control: "/missing".into(),
                guest_scale_control: "/missing".into(),
                guest_clipboard_control: "/missing".into(),
                xkb_config_root: "/missing".into(),
                machine_dir: "/missing".into(),
                status_dir: "/missing".into(),
                output_state_dir: "/missing".into(),
                initial_width: 1920,
                initial_height: 1080,
                guest_scale_120: None,
                sync_drm_device: None,
                title: "machine".into(),
                app_id: "org.example.machine/escape".into(),
                test_fractional_scale_120: None,
            }
            .app_id
            .contains('/')
        );
    }
}
