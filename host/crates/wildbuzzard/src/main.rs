// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use flate2::read::GzDecoder;
use fs2::FileExt;
use nix::unistd::{Gid, Uid, chown};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wb_core::{
    DESKTOP_READINESS_DEADLINE_DETAIL_PREFIX, IdMap, MachineConfig, MachineState, NetworkMode,
    OciImageMetadata, ResourceLocator, RuntimeState, WaylandCapabilities, WbPaths,
    host_control_socket,
};

const ROOTFS_SEED_ARCHIVE: &str = "WildBuzzard-rootfs-linux-x86_64.tar.zst";
const ROOTFS_SEED_MANIFEST: &str = "WildBuzzard-rootfs-linux-x86_64.json";
const ROOTFS_SEED_KIND: &str = "wildbuzzard-flat-rootfs";
const ROOTFS_SEED_MEDIA_TYPE: &str = "application/vnd.wildbuzzard.rootfs.v1.tar+zstd";
const ROOTFS_SEED_SCHEMA: u32 = 1;
const DEFAULT_ROOTFS_OCI_ARCHIVE: &str = "default-rootfs.oci.tar.zst";
const DEFAULT_ROOTFS_OCI_MANIFEST: &str = "default-rootfs.oci.json";
const MAX_GUEST_ID: u64 = 65_535;
const MAX_OCI_PAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OCI_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ROOTFS_MANIFEST_BYTES: u64 = 1024 * 1024;
const GUEST_ASSETS_REVISION: &str = include_str!("../../../../guest/ASSET_REVISION");
const GUEST_ASSETS_MANIFEST: &str = "usr/lib/wildbuzzard/guest-assets.manifest.json";
const GUEST_RUNTIME_ROOT: &str = "opt/wildbuzzard/runtime";
const BUNDLED_GUEST_RUNTIME: &str = "wildbuzzard-guest-runtime";
const MAX_GUEST_RUNTIME_MANIFEST_BYTES: u64 = 1024 * 1024;
const REQUIRED_GUEST_RUNTIME_FILES: &[&str] = &[
    "bin/sway",
    "bin/swaymsg",
    "bin/cua-driver",
    "libexec/wildbuzzard-shell",
    "libexec/wildbuzzard-settings",
    "libexec/wildbuzzard-shortcut-helper",
    "libexec/wildbuzzard-clipboard-agent",
    "libexec/wildbuzzard-updater",
    "libexec/updater_core.py",
    "libexec/wildbuzzard-init",
    "libexec/wildbuzzard-session",
    "libexec/wildbuzzard-sway-session",
    "libexec/wildbuzzard-output-sync",
    "libexec/wildbuzzard-desktop-stopped",
    "libexec/wildbuzzard-desktop-services",
    "libexec/wildbuzzard-integration-agent",
    "libexec/wildbuzzard-appimage-ready",
    "libexec/wildbuzzard-fusermount",
    "libexec/wildbuzzard-fusermount-exec",
    "libexec/wildbuzzard-runtime-ready",
    "libexec/wildbuzzard-sudo-exec",
];
const LEGACY_REFERENCE_CUA_SHA256: &str =
    "1f7abdd51e6239d3069caec92d73fca4a71c037321518c73036700012b30f029";
const LEGACY_TILED_SWAY_CONFIG_SHA256: &str =
    "eb974c1c489d4ca7f37043be1eca969d38042007eecb1d22e5d418dd7bcf23d3";
const GUEST_ASSETS: &[(&str, &[u8], u32)] = &[
    (
        "usr/lib/systemd/system-generators/wildbuzzard-generator",
        include_bytes!("../../../../guest/assets/wildbuzzard-generator"),
        0o755,
    ),
    (
        "usr/lib/systemd/system/wildbuzzard-desktop.service",
        include_bytes!("../../../../guest/assets/wildbuzzard-desktop.service"),
        0o644,
    ),
    (
        "etc/wildbuzzard/sway-config",
        include_bytes!("../../../../guest/assets/sway-config"),
        0o644,
    ),
    (
        "etc/fonts/conf.d/10-wildbuzzard-rendering.conf",
        include_bytes!("../../../../guest/assets/fontconfig-rendering.conf"),
        0o644,
    ),
    (
        "etc/sudoers.d/90-wildbuzzard",
        include_bytes!("../../../../guest/assets/90-wildbuzzard-sudoers"),
        0o440,
    ),
    (
        "usr/local/bin/sudo",
        include_bytes!("../../../../guest/assets/wildbuzzard-sudo"),
        0o755,
    ),
    (
        "usr/lib/systemd/system/wildbuzzard-runtime-ready.service",
        include_bytes!("../../../../guest/assets/wildbuzzard-runtime-ready.service"),
        0o644,
    ),
    (
        "usr/lib/systemd/system/wildbuzzard-updater.service",
        include_bytes!("../../../../guest/assets/wildbuzzard-updater.service"),
        0o644,
    ),
    (
        "usr/lib/systemd/system/wildbuzzard-updater-check.service",
        include_bytes!("../../../../guest/assets/wildbuzzard-updater-check.service"),
        0o644,
    ),
    (
        "usr/lib/systemd/system/wildbuzzard-updater.timer",
        include_bytes!("../../../../guest/assets/wildbuzzard-updater.timer"),
        0o644,
    ),
    (
        "usr/share/dbus-1/system.d/org.openresearchtools.WildBuzzard.Updater1.conf",
        include_bytes!("../../../../guest/assets/org.openresearchtools.WildBuzzard.Updater1.conf"),
        0o644,
    ),
    (
        "usr/libexec/wildbuzzard-shortcut-helper",
        include_bytes!("../../../../guest/assets/wildbuzzard-shortcut-helper-compat"),
        0o755,
    ),
    (
        "etc/polkit-1/rules.d/49-wildbuzzard-root.rules",
        include_bytes!("../../../../guest/assets/49-wildbuzzard-root.rules"),
        0o644,
    ),
    (
        "etc/xdg/kwalletrc",
        include_bytes!("../../../../guest/assets/kwalletrc"),
        0o644,
    ),
    (
        "etc/gtk-3.0/settings.ini",
        include_bytes!("../../../../guest/assets/gtk-3.0-settings.ini"),
        0o644,
    ),
    (
        "etc/gtk-4.0/settings.ini",
        include_bytes!("../../../../guest/assets/gtk-4.0-settings.ini"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Dark/index.theme",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Dark/index.theme"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Dark/gtk-3.0/gtk.css",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Dark/gtk-3.0/gtk.css"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Dark/gtk-3.0/palette.css",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Dark/gtk-3.0/palette.css"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Dark/gtk-4.0/gtk.css",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Dark/gtk-4.0/gtk.css"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Dark/gtk-4.0/palette.css",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Dark/gtk-4.0/palette.css"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Light/index.theme",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Light/index.theme"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Light/gtk-3.0/gtk.css",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Light/gtk-3.0/gtk.css"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Light/gtk-3.0/palette.css",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Light/gtk-3.0/palette.css"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Light/gtk-4.0/gtk.css",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Light/gtk-4.0/gtk.css"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Light/gtk-4.0/palette.css",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Light/gtk-4.0/palette.css"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Shared/gtk-3.0/geometry.css",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Shared/gtk-3.0/geometry.css"),
        0o644,
    ),
    (
        "usr/share/themes/WildBuzzard-Shared/gtk-4.0/geometry.css",
        include_bytes!("../../../../guest/assets/themes/WildBuzzard-Shared/gtk-4.0/geometry.css"),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/index.theme",
        include_bytes!("../../../../guest/assets/icons/WildBuzzard/index.theme"),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/scalable/apps/wildbuzzard.svg",
        include_bytes!("../../../../guest/assets/icons/WildBuzzard/scalable/apps/wildbuzzard.svg"),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/scalable/apps/wildbuzzard-settings.svg",
        include_bytes!(
            "../../../../guest/assets/icons/WildBuzzard/scalable/apps/wildbuzzard-settings.svg"
        ),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/scalable/places/folder.svg",
        include_bytes!("../../../../guest/assets/icons/WildBuzzard/scalable/places/folder.svg"),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/scalable/places/folder-open.svg",
        include_bytes!(
            "../../../../guest/assets/icons/WildBuzzard/scalable/places/folder-open.svg"
        ),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/scalable/places/folder-publicshare.svg",
        include_bytes!(
            "../../../../guest/assets/icons/WildBuzzard/scalable/places/folder-publicshare.svg"
        ),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/scalable/mimetypes/inode-directory.svg",
        include_bytes!(
            "../../../../guest/assets/icons/WildBuzzard/scalable/mimetypes/inode-directory.svg"
        ),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/symbolic/places/folder-symbolic.svg",
        include_bytes!(
            "../../../../guest/assets/icons/WildBuzzard/symbolic/places/folder-symbolic.svg"
        ),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/symbolic/places/folder-open-symbolic.svg",
        include_bytes!(
            "../../../../guest/assets/icons/WildBuzzard/symbolic/places/folder-open-symbolic.svg"
        ),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/symbolic/places/folder-publicshare-symbolic.svg",
        include_bytes!(
            "../../../../guest/assets/icons/WildBuzzard/symbolic/places/folder-publicshare-symbolic.svg"
        ),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/symbolic/mimetypes/inode-directory-symbolic.svg",
        include_bytes!(
            "../../../../guest/assets/icons/WildBuzzard/symbolic/mimetypes/inode-directory-symbolic.svg"
        ),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/symbolic/apps/wildbuzzard-symbolic.svg",
        include_bytes!(
            "../../../../guest/assets/icons/WildBuzzard/symbolic/apps/wildbuzzard-symbolic.svg"
        ),
        0o644,
    ),
    (
        "usr/share/icons/WildBuzzard/symbolic/apps/wildbuzzard-settings-symbolic.svg",
        include_bytes!(
            "../../../../guest/assets/icons/WildBuzzard/symbolic/apps/wildbuzzard-settings-symbolic.svg"
        ),
        0o644,
    ),
    (
        "usr/share/color-schemes/WildBuzzard-Dark.colors",
        include_bytes!("../../../../guest/assets/WildBuzzard.colors"),
        0o644,
    ),
    (
        "usr/share/color-schemes/WildBuzzard-Light.colors",
        include_bytes!("../../../../guest/assets/WildBuzzard-Light.colors"),
        0o644,
    ),
    (
        "etc/wildbuzzard/xdg/kdeglobals",
        include_bytes!("../../../../guest/assets/kdeglobals"),
        0o644,
    ),
    (
        "etc/wildbuzzard/xdg/foot/foot.ini",
        include_bytes!("../../../../guest/assets/foot.ini"),
        0o644,
    ),
    (
        "etc/wildbuzzard/xdg/mako/config",
        include_bytes!("../../../../guest/assets/mako-config"),
        0o644,
    ),
    (
        "etc/wildbuzzard/xdg/xdg-desktop-portal/portals.conf",
        include_bytes!("../../../../guest/assets/portals.conf"),
        0o644,
    ),
    (
        "etc/wildbuzzard/xdg/Thunar/uca.xml",
        include_bytes!("../../../../guest/assets/thunar-uca.xml"),
        0o644,
    ),
    (
        "usr/local/share/applications/foot-server.desktop",
        include_bytes!("../../../../guest/assets/applications/foot-server.desktop"),
        0o644,
    ),
    (
        "usr/local/share/applications/footclient.desktop",
        include_bytes!("../../../../guest/assets/applications/footclient.desktop"),
        0o644,
    ),
    (
        "usr/local/share/applications/thunar-bulk-rename.desktop",
        include_bytes!("../../../../guest/assets/applications/thunar-bulk-rename.desktop"),
        0o644,
    ),
    (
        "usr/local/share/applications/thunar-settings.desktop",
        include_bytes!("../../../../guest/assets/applications/thunar-settings.desktop"),
        0o644,
    ),
    (
        "usr/share/applications/org.openresearchtools.WildBuzzard.Settings1.desktop",
        include_bytes!(
            "../../../../guest/assets/applications/org.openresearchtools.WildBuzzard.Settings1.desktop"
        ),
        0o644,
    ),
    (
        "usr/share/doc/wildbuzzard-cua/LICENSE.trycua-cua.md",
        include_bytes!("../../../../guest/third_party/trycua-cua/LICENSE.md"),
        0o644,
    ),
    (
        "usr/share/doc/wildbuzzard-cua/UPSTREAM.toml",
        include_bytes!("../../../../guest/third_party/trycua-cua/UPSTREAM.toml"),
        0o644,
    ),
    (
        "usr/share/doc/wildbuzzard-cua/CHANGES.WILDBUZZARD.md",
        include_bytes!("../../../../guest/third_party/trycua-cua/CHANGES.WILDBUZZARD.md"),
        0o644,
    ),
];

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
struct GuestAssetRecord {
    sha256: String,
    mode: u32,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct GuestAssetManifest {
    schema: u32,
    assets: BTreeMap<String, GuestAssetRecord>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct GuestRuntimeFileRecord {
    sha256: String,
    mode: u32,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct GuestRuntimeManifest {
    schema_version: u32,
    revision: String,
    files: BTreeMap<String, GuestRuntimeFileRecord>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct GuestRuntimeReadiness {
    schema_version: u32,
    revision: String,
    manifest_sha256: String,
    ready: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct GuestRuntimeActivationFailure {
    schema_version: u32,
    failed_revision: String,
    fallback_revision: String,
    reason: String,
    observed_at_unix_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GuestRuntimeActivation {
    revision: String,
    previous: String,
}

#[derive(Debug)]
struct DesktopReadinessDeadline {
    seconds: u64,
    diagnostic: Option<String>,
}

impl std::fmt::Display for DesktopReadinessDeadline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "machine did not report desktop readiness within {} seconds",
            self.seconds
        )?;
        if let Some(diagnostic) = &self.diagnostic {
            write!(formatter, ": {diagnostic}")?;
        }
        Ok(())
    }
}

impl std::error::Error for DesktopReadinessDeadline {}

fn state_desktop_readiness_deadline(state: &RuntimeState) -> Option<DesktopReadinessDeadline> {
    let detail = state
        .detail
        .as_deref()?
        .strip_prefix(DESKTOP_READINESS_DEADLINE_DETAIL_PREFIX)?;
    let (seconds, diagnostic) = detail.split_once(':')?;
    let seconds = seconds.parse::<u64>().ok()?;
    if !(1..=600).contains(&seconds) {
        return None;
    }
    Some(DesktopReadinessDeadline {
        seconds,
        diagnostic: Some(diagnostic.trim().to_owned()).filter(|value| !value.is_empty()),
    })
}

#[derive(Debug, Parser)]
#[command(
    name = "BuzzardOS",
    version,
    about = "Persistent, rootless desktop machines in one Wayland window"
)]
struct Cli {
    /// Portable storage folder (default: directory containing the BuzzardOS launcher).
    #[arg(
        long,
        visible_alias = "home",
        global = true,
        env = "WILDBUZZARD_STORAGE_DIR"
    )]
    storage_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create a persistent mutable machine from the bundled seed or an OCI image.
    Create {
        name: String,
        /// Explicit OCI image reference. Without this, use the portable bundled rootfs seed.
        #[arg(long)]
        image: Option<String>,
        #[arg(long, value_enum, default_value_t = NetworkArg::User)]
        network: NetworkArg,
        /// NVIDIA GPU index/UUID to expose; repeat for multiple GPUs.
        #[arg(long = "gpu", value_delimiter = ',', default_value = "all")]
        gpus: Vec<String>,
    },
    /// Boot systemd and the nested desktop compositor.
    Start {
        name: String,
        /// Return after the machine process has started.
        #[arg(long)]
        detach: bool,
    },
    /// Ask the running machine to shut down.
    Stop { name: String },
    /// Import a local OCI layout/archive, a Buzzard OS export, or a remote OCI reference.
    Import {
        source: String,
        #[arg(long)]
        name: String,
        /// Restore the exported identity, or clone it with fresh guest identity material.
        #[arg(long, value_enum, default_value_t = ImportModeArg::Restore)]
        mode: ImportModeArg,
        /// Select a manifest digest or org.opencontainers.image.ref.name from a multi-image index.
        #[arg(long)]
        manifest: Option<String>,
    },
    /// Export one stopped machine as a portable standards-compliant OCI archive.
    Export {
        name: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Flatten one release-builder machine into a generic identity-free OCI seed.
    #[command(name = "__export-generic-seed", hide = true)]
    ExportGenericSeed {
        name: String,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        source_date_epoch: i64,
    },
    /// Clone one stopped machine and regenerate its machine identity.
    Clone { source: String, name: String },
    /// Permanently delete one stopped machine and its persistent rootfs.
    Delete {
        name: String,
        /// Confirm destructive deletion.
        #[arg(long)]
        yes: bool,
    },
    /// Control the native host window from the host launcher.
    Window {
        name: String,
        #[arg(value_enum)]
        action: WindowAction,
    },
    /// Show machine runtime state.
    Status { name: String },
    /// List persistent machines.
    List,
    /// Check host kernel, display, and GPU capabilities.
    Doctor,
    #[command(name = "__apply-image", hide = true)]
    ApplyImage {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        expected_digest: String,
        #[arg(long)]
        rootfs: PathBuf,
        #[arg(long)]
        work_dir: PathBuf,
    },
    #[command(name = "__export-oci", hide = true)]
    ExportOci {
        #[arg(long)]
        rootfs: PathBuf,
        #[arg(long)]
        machine_config: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        work_dir: PathBuf,
        #[arg(long)]
        generic_seed: bool,
        #[arg(long, requires = "generic_seed")]
        source_date_epoch: Option<i64>,
    },
    #[command(name = "__reset-clone-identity", hide = true)]
    ResetCloneIdentity {
        #[arg(long)]
        rootfs: PathBuf,
    },
    #[command(name = "__delete-machine", hide = true)]
    DeleteMachine {
        #[arg(long)]
        machine: PathBuf,
        #[arg(long)]
        machines: PathBuf,
    },
    #[command(name = "__apply-rootfs", hide = true)]
    ApplyRootfs {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        expected_digest: String,
        #[arg(long)]
        rootfs: PathBuf,
    },
    #[command(name = "__cleanup-staging", hide = true)]
    CleanupStaging {
        #[arg(long)]
        staging: PathBuf,
        #[arg(long)]
        machines: PathBuf,
    },
    #[command(name = "__cleanup-export-staging", hide = true)]
    CleanupExportStaging {
        #[arg(long)]
        staging: PathBuf,
        #[arg(long)]
        cache: PathBuf,
    },
    #[command(name = "__install-guest-assets", hide = true)]
    InstallGuestAssets {
        #[arg(long)]
        rootfs: PathBuf,
    },
    #[command(name = "__verify-guest-assets", hide = true)]
    VerifyGuestAssets {
        #[arg(long)]
        rootfs: PathBuf,
    },
    #[command(name = "__rollback-guest-runtime", hide = true)]
    RollbackGuestRuntime {
        #[arg(long)]
        rootfs: PathBuf,
        #[arg(long)]
        expected_current: String,
        #[arg(long)]
        expected_previous: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum NetworkArg {
    User,
    Host,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ImportModeArg {
    Restore,
    Clone,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum WindowAction {
    Minimize,
    Maximize,
    Restore,
    FocusMonitor,
    ToggleMaximize,
    Close,
}

impl WindowAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Maximize => "maximize",
            Self::Restore => "restore",
            Self::FocusMonitor => "focus-monitor",
            Self::ToggleMaximize => "toggle-maximize",
            Self::Close => "close",
        }
    }
}

impl From<NetworkArg> for NetworkMode {
    fn from(value: NetworkArg) -> Self {
        match value {
            NetworkArg::User => Self::User,
            NetworkArg::Host => Self::Host,
            NetworkArg::None => Self::None,
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Buzzard OS: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Commands::ApplyImage {
        archive,
        expected_digest,
        rootfs,
        work_dir,
    }) = &cli.command
    {
        apply_image_archive(archive, expected_digest, rootfs, work_dir)?;
        install_guest_assets(rootfs)?;
        if !guest_assets_are_current(rootfs)? {
            bail!("new machine guest asset revision was not committed");
        }
        validate_extracted_rootfs(rootfs)?;
        return Ok(());
    }
    if let Some(Commands::ExportOci {
        rootfs,
        machine_config,
        output,
        work_dir,
        generic_seed,
        source_date_epoch,
    }) = &cli.command
    {
        if *generic_seed != source_date_epoch.is_some() {
            bail!("generic OCI seed export requires exactly one source timestamp");
        }
        export_oci_archive(rootfs, machine_config, output, work_dir, *source_date_epoch)?;
        return Ok(());
    }
    if let Some(Commands::ResetCloneIdentity { rootfs }) = &cli.command {
        reset_cloned_rootfs_identity(rootfs)?;
        return Ok(());
    }
    if let Some(Commands::DeleteMachine { machine, machines }) = &cli.command {
        remove_persistent_machine_tree(machine, machines)?;
        return Ok(());
    }
    if let Some(Commands::ApplyRootfs {
        archive,
        manifest,
        expected_digest,
        rootfs,
    }) = &cli.command
    {
        apply_rootfs_seed(archive, manifest, expected_digest, rootfs)?;
        if !guest_assets_are_current(rootfs)? {
            install_guest_assets(rootfs)?;
        }
        if !guest_assets_are_current(rootfs)? {
            bail!("new machine guest asset revision was not committed");
        }
        return Ok(());
    }
    if let Some(Commands::CleanupStaging { staging, machines }) = &cli.command {
        remove_machine_staging_tree(staging, machines)?;
        return Ok(());
    }
    if let Some(Commands::CleanupExportStaging { staging, cache }) = &cli.command {
        remove_export_staging_tree(staging, cache)?;
        return Ok(());
    }
    if let Some(Commands::InstallGuestAssets { rootfs }) = &cli.command {
        let rootfs = rootfs
            .canonicalize()
            .with_context(|| format!("resolving guest rootfs {}", rootfs.display()))?;
        migrate_guest_assets(&rootfs)?;
        if !guest_assets_are_current(&rootfs)? {
            bail!("guest asset migration revision was not committed");
        }
        return Ok(());
    }
    if let Some(Commands::VerifyGuestAssets { rootfs }) = &cli.command {
        let rootfs = rootfs
            .canonicalize()
            .with_context(|| format!("resolving guest rootfs {}", rootfs.display()))?;
        if guest_assets_are_current(&rootfs)? {
            return Ok(());
        }
        std::process::exit(3);
    }
    if let Some(Commands::RollbackGuestRuntime {
        rootfs,
        expected_current,
        expected_previous,
    }) = &cli.command
    {
        let rootfs = rootfs
            .canonicalize()
            .with_context(|| format!("resolving guest rootfs {}", rootfs.display()))?;
        rollback_guest_runtime(
            &rootfs,
            expected_current,
            expected_previous,
            "desktop readiness deadline expired",
            0,
        )?;
        return Ok(());
    }
    let paths = WbPaths::discover(cli.storage_dir.as_deref())?;
    paths.ensure()?;

    match cli.command {
        Some(Commands::Create {
            name,
            image,
            network,
            gpus,
        }) => create(&paths, &name, image.as_deref(), network.into(), gpus),
        Some(Commands::Start { name, detach }) => start(&paths, &name, detach),
        Some(Commands::Stop { name }) => stop(&paths, &name),
        Some(Commands::Import {
            source,
            name,
            mode,
            manifest,
        }) => import_machine(&paths, &source, &name, manifest.as_deref(), mode, None),
        Some(Commands::Export { name, output }) => export_machine(&paths, &name, &output, None),
        Some(Commands::ExportGenericSeed {
            name,
            output,
            source_date_epoch,
        }) => export_machine(&paths, &name, &output, Some(source_date_epoch)),
        Some(Commands::Clone { source, name }) => clone_machine(&paths, &source, &name),
        Some(Commands::Delete { name, yes }) => delete_machine(&paths, &name, yes),
        Some(Commands::Window { name, action }) => window(&paths, &name, action),
        Some(Commands::Status { name }) => status(&paths, &name),
        Some(Commands::List) => list(&paths),
        Some(Commands::Doctor) => doctor(),
        Some(Commands::ApplyImage { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::ExportOci { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::ResetCloneIdentity { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::DeleteMachine { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::ApplyRootfs { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::CleanupStaging { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::CleanupExportStaging { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::InstallGuestAssets { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::VerifyGuestAssets { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::RollbackGuestRuntime { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        None => open_portable_desktop(&paths),
    }
}

fn open_portable_desktop(paths: &WbPaths) -> Result<()> {
    let mut machines = Vec::new();
    for entry in fs::read_dir(paths.machines()).context("listing portable machines")? {
        let entry = entry.context("reading portable machine directory")?;
        if entry.file_type()?.is_dir()
            && let Ok(config) = MachineConfig::load(&entry.path())
        {
            machines.push(config.name);
        }
    }
    machines.sort();

    if machines.is_empty() {
        let name = "default";
        println!("Creating persistent desktop machine '{name}' for first launch...");
        create(paths, name, None, NetworkMode::User, vec!["all".into()])?;
    }

    let resources = ResourceLocator::discover()?;
    let display = resources.helper_or_path("wildbuzzard-display")?;
    let launcher = std::env::current_exe().context("locating Buzzard OS launcher")?;
    let status = Command::new(&display)
        .arg("--machine-manager")
        .arg("--portable-dir")
        .arg(paths.base())
        .arg("--launcher")
        .arg(&launcher)
        .status()
        .with_context(|| format!("starting machine manager with {}", display.display()))?;
    if !status.success() {
        bail!("Buzzard OS machine manager exited with {status}");
    }
    Ok(())
}

fn create(
    paths: &WbPaths,
    name: &str,
    image: Option<&str>,
    network: NetworkMode,
    gpus: Vec<String>,
) -> Result<()> {
    MachineConfig::validate_name(name)?;
    MachineConfig::validate_gpus(&gpus)?;
    if image.is_some_and(|image| image.trim().is_empty()) {
        bail!("--image cannot be empty");
    }

    let final_dir = paths.machine(name);
    if final_dir.exists() {
        bail!("machine '{name}' already exists at {}", final_dir.display());
    }

    let resources = ResourceLocator::discover()?;
    let stage = tempfile::Builder::new()
        .prefix(&format!(".{name}-creating-"))
        .tempdir_in(paths.machines())
        .context("creating machine staging directory")?;
    let machine_dir = stage.path();
    let creation_result = (|| -> Result<()> {
        let rootfs = machine_dir.join("rootfs");
        fs::create_dir(&rootfs).context("creating persistent rootfs")?;

        let (source_reference, image_digest, oci_metadata) = if let Some(image) = image {
            // Portable releases always resolve the bundled copy. PATH fallback only makes
            // source-tree development convenient and is never required of end users.
            let crane = resources.helper_or_path("crane")?;
            let platform = oci_platform()?;
            let digest_output = Command::new(&crane)
                .args(["digest", "--platform", platform, image])
                .stdin(Stdio::null())
                .output()
                .with_context(|| format!("resolving image digest with {}", crane.display()))?;
            if !digest_output.status.success() {
                bail!("OCI digest resolution failed with {}", digest_output.status);
            }
            let image_digest =
                String::from_utf8(digest_output.stdout).context("OCI digest is not UTF-8")?;
            let image_digest = image_digest.trim().to_owned();
            validate_sha256_digest(&image_digest)?;
            let immutable_image = format!("{image}@{image_digest}");

            let image_archive = machine_dir.join("image.oci.tar");
            let image_layout_stage = machine_dir.join("image-layout");
            let image_cache = paths.cache().join("oci-blobs");
            fs::create_dir_all(&image_cache).context("creating OCI download cache")?;
            eprintln!("Pulling {image}…");
            let status = Command::new(&crane)
                .args(["pull", "--platform", platform, "--format", "oci"])
                .arg("--cache_path")
                .arg(&image_cache)
                .arg(&immutable_image)
                .arg(&image_archive)
                .stdin(Stdio::null())
                .status()
                .with_context(|| format!("starting {}", crane.display()))?;
            if !status.success() {
                bail!("OCI pull failed with {status}");
            }

            fs::create_dir(&image_layout_stage).context("creating remote OCI layout staging")?;
            extract_oci_archive(&image_archive, &image_layout_stage)
                .context("extracting downloaded OCI layout archive")?;
            let image_layout = canonical_oci_layout(&image_layout_stage)?;
            let index = read_oci_index(&image_layout)?;
            let descriptor = find_oci_manifest_descriptor(&image_layout, &index, &image_digest)
                .context("downloaded OCI layout does not contain the resolved manifest")?;
            let oci_metadata = oci_metadata_from_manifest(&image_layout, &descriptor)?;

            eprintln!("Applying OCI layers to the persistent root filesystem…");
            apply_image_in_user_namespace(
                &resources,
                &image_layout,
                &image_digest,
                &rootfs,
                machine_dir,
            )?;
            fs::remove_file(&image_archive).context("removing downloaded OCI archive")?;
            fs::remove_dir_all(&image_layout_stage).context("removing temporary OCI layout")?;
            (image.to_owned(), image_digest, oci_metadata)
        } else {
            let archive = bundled_rootfs_oci_archive(paths)?;
            let layout_stage = machine_dir.join("bundled-oci-layout");
            fs::create_dir(&layout_stage).context("creating bundled OCI staging directory")?;
            extract_oci_archive(&archive, &layout_stage)?;
            let layout = canonical_oci_layout(&layout_stage)?;
            let index = read_oci_index(&layout)?;
            let descriptor = resolve_oci_manifest_descriptor(&layout, &index, None)?;
            let digest = descriptor.digest.clone();
            let oci_metadata = oci_metadata_from_manifest(&layout, &descriptor)?;
            eprintln!("Applying the bundled OCI rootfs to the persistent root filesystem…");
            apply_image_in_user_namespace(&resources, &layout, &digest, &rootfs, machine_dir)?;
            fs::remove_dir_all(&layout_stage).context("removing bundled OCI staging layout")?;
            (
                format!("bundle:app/runtime/{DEFAULT_ROOTFS_OCI_ARCHIVE}"),
                digest,
                oci_metadata,
            )
        };

        let mut config = MachineConfig::new(
            name.to_owned(),
            source_reference.clone(),
            image_digest,
            network,
            gpus,
        );
        config.oci = oci_metadata;
        config.save(machine_dir)?;
        RuntimeState::new(MachineState::Stopped).save(machine_dir)?;
        File::create(machine_dir.join("machine.lock")).context("creating machine lock")?;

        commit_new_machine(stage.path(), &final_dir)
            .with_context(|| format!("committing machine to {}", final_dir.display()))?;
        println!(
            "Created '{name}' from {source_reference}\nPersistent rootfs: {}\nShared data: {}",
            final_dir.join("rootfs").display(),
            paths.shared().display()
        );
        Ok(())
    })();
    if let Err(error) = creation_result {
        if let Err(cleanup_error) =
            cleanup_failed_machine_stage(&resources, stage.path(), &paths.machines())
        {
            return Err(error).context(format!(
                "machine creation also failed to remove staging tree {}: {cleanup_error:#}",
                stage.path().display()
            ));
        }
        return Err(error);
    }
    Ok(())
}

const BUZZARD_OCI_CONFIG_ANNOTATION: &str = "org.openresearchtools.buzzardos.machine-config.v1";
const OCI_REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";

fn import_machine(
    paths: &WbPaths,
    source: &str,
    name: &str,
    selector: Option<&str>,
    mode: ImportModeArg,
    source_reference_override: Option<&str>,
) -> Result<()> {
    MachineConfig::validate_name(name)?;
    if source.trim().is_empty() {
        bail!("OCI import source cannot be empty");
    }
    let source_path = Path::new(source);
    let final_dir = paths.machine(name);
    if final_dir.exists() {
        bail!("machine '{name}' already exists at {}", final_dir.display());
    }
    let resources = ResourceLocator::discover()?;
    let stage = tempfile::Builder::new()
        .prefix(&format!(".{name}-importing-"))
        .tempdir_in(paths.machines())
        .context("creating machine import staging directory")?;
    let source_stage = tempfile::Builder::new()
        .prefix("oci-import-")
        .tempdir_in(paths.cache())
        .context("creating OCI import staging directory")?;

    let (layout, source_reference, expected_digest) = if source_path.exists() {
        let layout = if source_path.is_dir() {
            canonical_oci_layout(source_path)?
        } else {
            let extracted = source_stage.path().join("layout");
            fs::create_dir(&extracted).context("creating local OCI extraction directory")?;
            extract_oci_archive(source_path, &extracted)?;
            canonical_oci_layout(&extracted)?
        };
        (layout, local_oci_source_reference(source_path), None)
    } else {
        if selector.is_some() {
            bail!("--manifest is supported only for local OCI layouts and archives");
        }
        let crane = resources.helper_or_path("crane")?;
        let platform = oci_platform()?;
        let digest_output = Command::new(&crane)
            .args(["digest", "--platform", platform, source])
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("resolving OCI reference with {}", crane.display()))?;
        if !digest_output.status.success() {
            bail!("OCI digest resolution failed with {}", digest_output.status);
        }
        let digest = String::from_utf8(digest_output.stdout)
            .context("OCI digest is not UTF-8")?
            .trim()
            .to_owned();
        validate_sha256_digest(&digest)?;
        let immutable_source = if source.contains("@sha256:") {
            source.to_owned()
        } else {
            format!("{source}@{digest}")
        };
        let archive = source_stage.path().join("remote.oci.tar");
        let cache = paths.cache().join("oci-blobs");
        fs::create_dir_all(&cache).context("creating OCI download cache")?;
        eprintln!("Pulling {source}…");
        let status = Command::new(&crane)
            .args(["pull", "--platform", platform, "--format", "oci"])
            .arg("--cache_path")
            .arg(&cache)
            .arg(&immutable_source)
            .arg(&archive)
            .stdin(Stdio::null())
            .status()
            .with_context(|| format!("starting {}", crane.display()))?;
        if !status.success() {
            bail!("OCI pull failed with {status}");
        }
        let extracted = source_stage.path().join("layout");
        fs::create_dir(&extracted).context("creating remote OCI extraction directory")?;
        extract_oci_archive(&archive, &extracted)?;
        (
            canonical_oci_layout(&extracted)?,
            format!("oci:{source}"),
            Some(digest),
        )
    };
    let index = read_oci_index(&layout)?;
    let descriptor = if let Some(digest) = expected_digest.as_deref() {
        find_oci_manifest_descriptor(&layout, &index, digest)
            .context("downloaded OCI layout does not contain the resolved manifest")?
    } else {
        resolve_oci_manifest_descriptor(&layout, &index, selector)?
    };
    let digest = descriptor.digest.clone();
    let imported_config = portable_config_from_manifest(&layout, &descriptor)?;
    let imported_oci_metadata = oci_metadata_from_manifest(&layout, &descriptor)?;
    let source_reference = source_reference_override
        .map(ToOwned::to_owned)
        .unwrap_or(source_reference);

    let result = (|| -> Result<()> {
        let rootfs = stage.path().join("rootfs");
        fs::create_dir(&rootfs).context("creating imported machine rootfs")?;
        apply_image_in_user_namespace(&resources, &layout, &digest, &rootfs, stage.path())?;

        let carries_portable_identity = imported_config.is_some();
        let mut config = imported_config.unwrap_or_else(|| {
            MachineConfig::new(
                name.to_owned(),
                source_reference.clone(),
                digest.clone(),
                NetworkMode::User,
                vec!["all".into()],
            )
        });
        config.oci = imported_oci_metadata.clone();
        // Restore identity uniqueness is a portable-root invariant. Serialize
        // only the check/commit window so two completed private stages cannot
        // both observe the same identity as absent and then commit it under
        // different names.
        let _identity_registry_lock = if mode == ImportModeArg::Restore && carries_portable_identity
        {
            Some(lock_machine_identity_registry(paths)?)
        } else {
            None
        };
        if mode == ImportModeArg::Restore && carries_portable_identity {
            reject_duplicate_machine_identity(paths, config.id)?;
        } else {
            config.id = uuid::Uuid::new_v4();
            reset_cloned_machine_identity_in_stage(&resources, &rootfs)?;
        }
        config.name = name.to_owned();
        config.title = name.to_owned();
        config.image = source_reference.clone();
        config.image_digest = Some(digest.clone());
        sanitize_imported_machine_config(&mut config);
        config.save(stage.path())?;
        RuntimeState::new(MachineState::Stopped).save(stage.path())?;
        File::create(stage.path().join("machine.lock")).context("creating machine lock")?;
        commit_new_machine(stage.path(), &final_dir)?;
        println!(
            "Imported '{name}' from {source} in {} mode\nPersistent rootfs: {}\nShared data: {}",
            match mode {
                ImportModeArg::Restore => "restore",
                ImportModeArg::Clone => "clone",
            },
            final_dir.join("rootfs").display(),
            paths.shared().display()
        );
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(cleanup_error) =
            cleanup_failed_machine_stage(&resources, stage.path(), &paths.machines())
        {
            return Err(error).context(format!(
                "machine import also failed to remove staging tree {}: {cleanup_error:#}",
                stage.path().display()
            ));
        }
        return Err(error);
    }
    Ok(())
}

fn local_oci_source_reference(source: &Path) -> String {
    let label = source
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("local-oci");
    format!("oci-import:{label}")
}

fn sanitize_imported_machine_config(config: &mut MachineConfig) {
    config.schema = 3;
    config.gpus = vec!["all".into()];
    config.network = NetworkMode::User;
    for port in &mut config.integrations.ports {
        port.enabled = false;
    }
    config.integrations.media.guest_audio_output = false;
    config.integrations.media.host_microphone = false;
    config.integrations.media.host_camera = false;
    config.integrations.media.audio_target = None;
    config.integrations.media.microphone_target = None;
    config.integrations.media.camera_target = None;
}

fn reject_duplicate_machine_identity(paths: &WbPaths, identity: uuid::Uuid) -> Result<()> {
    for entry in fs::read_dir(paths.machines()).context("listing machine identities")? {
        let entry = entry.context("reading machine identity directory")?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Ok(existing) = MachineConfig::load(&entry.path())
            && existing.id == identity
        {
            bail!(
                "the imported machine identity already exists as '{}'; use `BuzzardOS clone {} NEW_NAME` to create an independent copy",
                existing.name,
                existing.name
            );
        }
    }
    Ok(())
}

fn lock_machine_identity_registry(paths: &WbPaths) -> Result<File> {
    let machines = paths.machines();
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&machines)
        .with_context(|| format!("opening {} for identity locking", machines.display()))?;
    if !file.metadata()?.is_dir() {
        bail!("portable Machines path is not a directory");
    }
    file.lock_exclusive()
        .context("locking the portable machine identity registry")?;
    Ok(file)
}

fn read_oci_index(layout: &Path) -> Result<OciIndex> {
    let path = layout.join("index.json");
    let mut file = open_regular_nofollow(&path, "OCI image index")?;
    let size = file.metadata()?.len();
    if size > MAX_OCI_METADATA_BYTES {
        bail!("OCI image index exceeds {MAX_OCI_METADATA_BYTES} bytes");
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    let index: OciIndex = serde_json::from_slice(&bytes).context("parsing OCI image index")?;
    if index.schema_version != 2 {
        bail!(
            "unsupported OCI index schema version {}",
            index.schema_version
        );
    }
    Ok(index)
}

fn canonical_oci_layout(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting OCI source {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("OCI layout {} must be a real directory", path.display());
    }
    let mut candidates = Vec::new();
    if path.join("oci-layout").is_file() && path.join("index.json").is_file() {
        candidates.push(path.to_path_buf());
    }
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry.context("reading OCI archive root")?;
        if entry.file_type()?.is_dir()
            && entry.path().join("oci-layout").is_file()
            && entry.path().join("index.json").is_file()
        {
            candidates.push(entry.path());
        }
    }
    if candidates.len() != 1 {
        bail!(
            "{} contains {} OCI layouts; exactly one is required",
            path.display(),
            candidates.len()
        );
    }
    let layout = candidates.remove(0).canonicalize()?;
    let mut layout_file = open_regular_nofollow(&layout.join("oci-layout"), "oci-layout")?;
    let mut layout_bytes = Vec::new();
    Read::by_ref(&mut layout_file)
        .take(MAX_ROOTFS_MANIFEST_BYTES + 1)
        .read_to_end(&mut layout_bytes)
        .context("reading oci-layout")?;
    if layout_bytes.len() as u64 > MAX_ROOTFS_MANIFEST_BYTES {
        bail!("oci-layout exceeds {MAX_ROOTFS_MANIFEST_BYTES} bytes");
    }
    let layout_record: serde_json::Value =
        serde_json::from_slice(&layout_bytes).context("parsing oci-layout")?;
    if layout_record
        .get("imageLayoutVersion")
        .and_then(|v| v.as_str())
        != Some("1.0.0")
    {
        bail!("unsupported or missing OCI imageLayoutVersion");
    }
    Ok(layout)
}

fn extract_oci_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    let file = open_regular_nofollow(archive_path, "OCI archive")?;
    let mut input = BufReader::new(file);
    let mut magic = [0_u8; 4];
    let count = input
        .read(&mut magic)
        .context("reading OCI archive header")?;
    input.rewind().context("rewinding OCI archive")?;
    let reader: Box<dyn Read> = if count >= 2 && magic[..2] == [0x1f, 0x8b] {
        Box::new(GzDecoder::new(input))
    } else if count == 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        Box::new(zstd::stream::read::Decoder::new(input).context("opening zstd OCI archive")?)
    } else {
        Box::new(input)
    };
    let mut archive = tar::Archive::new(reader);
    for item in archive.entries().context("reading OCI archive")? {
        let mut entry = item.context("reading OCI archive entry")?;
        let relative = entry
            .path()
            .context("reading OCI archive path")?
            .into_owned();
        let relative = safe_relative_path(&relative)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let kind = entry.header().entry_type();
        if !kind.is_dir() && !kind.is_file() {
            bail!(
                "OCI layout archive contains unsupported non-regular entry {}",
                relative.display()
            );
        }
        entry
            .unpack_in(destination)
            .with_context(|| format!("extracting OCI layout entry {}", relative.display()))?;
    }
    Ok(())
}

fn portable_config_from_manifest(
    layout: &Path,
    descriptor: &OciDescriptor,
) -> Result<Option<MachineConfig>> {
    let bytes = read_verified_blob(layout, descriptor)?;
    let manifest: OciManifest = serde_json::from_slice(&bytes).context("parsing OCI manifest")?;
    manifest
        .annotations
        .get(BUZZARD_OCI_CONFIG_ANNOTATION)
        .map(|value| serde_json::from_str(value).context("parsing Buzzard OS machine annotation"))
        .transpose()
}

fn oci_metadata_from_manifest(
    layout: &Path,
    descriptor: &OciDescriptor,
) -> Result<OciImageMetadata> {
    const MAX_OCI_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
    let manifest_bytes = read_verified_blob(layout, descriptor)?;
    let manifest: OciManifest =
        serde_json::from_slice(&manifest_bytes).context("parsing OCI manifest")?;
    if manifest.config.size > MAX_OCI_CONFIG_BYTES {
        bail!(
            "OCI image config is {} bytes; maximum is {MAX_OCI_CONFIG_BYTES}",
            manifest.config.size
        );
    }
    let config_bytes = read_verified_blob(layout, &manifest.config)?;
    let config: OciImageConfigDocument =
        serde_json::from_slice(&config_bytes).context("parsing OCI image config")?;
    if config.os != "linux" || config.architecture != host_oci_architecture()? {
        bail!(
            "OCI image config targets {}/{}, expected linux/{}",
            config.os,
            config.architecture,
            host_oci_architecture()?
        );
    }
    let metadata = OciImageMetadata {
        environment: config.config.environment,
        labels: config.config.labels,
        working_dir: config.config.working_dir.filter(|value| !value.is_empty()),
        user: config.config.user.filter(|value| !value.is_empty()),
        entrypoint: config.config.entrypoint,
        command: config.config.command,
        stop_signal: config.config.stop_signal.filter(|value| !value.is_empty()),
    };
    metadata.validate()?;
    Ok(metadata)
}

struct DigestingWriter<W> {
    inner: W,
    digest: Sha256,
    bytes: u64,
}

impl<W> DigestingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            digest: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (W, String, u64) {
        (
            self.inner,
            format!("sha256:{:x}", self.digest.finalize()),
            self.bytes,
        )
    }
}

impl<W: Write> Write for DigestingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.digest.update(&buffer[..written]);
        self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn export_machine(
    paths: &WbPaths,
    name: &str,
    output: &Path,
    generic_seed_source_date_epoch: Option<i64>,
) -> Result<()> {
    let machine_dir = require_machine(paths, name)?;
    let _lock = lock_stopped_machine_for_export(&machine_dir)?;
    let output = std::path::absolute(output)
        .with_context(|| format!("resolving export destination {}", output.display()))?;
    if output.exists() {
        bail!("refusing to replace existing export {}", output.display());
    }
    let parent = output
        .parent()
        .context("export destination has no parent")?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("inspecting export directory {}", parent.display()))?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        bail!("export destination parent must be a real directory");
    }

    let temporary = tempfile::Builder::new()
        .prefix(".buzzardos-export-")
        .tempfile_in(parent)
        .context("creating atomic export file")?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    let work = tempfile::Builder::new()
        .prefix("oci-export-")
        .tempdir_in(paths.cache())
        .context("creating OCI export work directory")?;
    let resources = ResourceLocator::discover()?;
    let unshare = resources.helper_or_path("unshare")?;
    let id_map = IdMap::discover()?;
    let namespace_program = id_map.namespace_program(&unshare)?;
    let launcher = std::env::current_exe().context("locating launcher for OCI export")?;
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    command
        .args(id_map.namespace_args())
        .arg(launcher)
        .arg("__export-oci")
        .arg("--rootfs")
        .arg(machine_dir.join("rootfs"))
        .arg("--machine-config")
        .arg(machine_dir.join(MachineConfig::FILE))
        .arg("--output")
        .arg(temporary.path())
        .arg("--work-dir")
        .arg(work.path());
    if let Some(source_date_epoch) = generic_seed_source_date_epoch {
        command
            .arg("--generic-seed")
            .arg("--source-date-epoch")
            .arg(source_date_epoch.to_string());
    }
    let status_result = command.stdin(Stdio::null()).status().with_context(|| {
        format!(
            "starting OCI export namespace with {}",
            namespace_program.display()
        )
    });
    let cleanup_result = cleanup_export_stage(&resources, work.path(), &paths.cache());
    let status = match (status_result, cleanup_result) {
        (Ok(status), Ok(())) => status,
        (Err(start_error), Ok(())) => return Err(start_error),
        (Ok(status), Err(cleanup_error)) => {
            bail!(
                "OCI export namespace exited with {status}; export staging cleanup failed: {cleanup_error:#}"
            )
        }
        (Err(start_error), Err(cleanup_error)) => {
            bail!("{start_error:#}; export staging cleanup also failed: {cleanup_error:#}")
        }
    };
    if !status.success() {
        bail!("OCI export namespace exited with {status}");
    }
    temporary
        .as_file()
        .sync_all()
        .context("syncing completed OCI export")?;

    let verification = tempfile::Builder::new()
        .prefix("oci-export-verify-")
        .tempdir_in(paths.cache())?;
    extract_oci_archive(temporary.path(), verification.path())?;
    let layout = canonical_oci_layout(verification.path())?;
    let index = read_oci_index(&layout)?;
    let descriptor = resolve_oci_manifest_descriptor(&layout, &index, None)?;
    let manifest: OciManifest = serde_json::from_slice(&read_verified_blob(&layout, &descriptor)?)?;
    verified_blob_path(&layout, &manifest.config)?;
    for layer in &manifest.layers {
        verified_blob_path(&layout, layer)?;
    }

    let persisted = temporary
        .persist_noclobber(&output)
        .map_err(|error| error.error)
        .with_context(|| format!("committing export {}", output.display()))?;
    persisted.set_permissions(fs::Permissions::from_mode(0o644))?;
    persisted.sync_all()?;
    sync_parent_directory(&output)?;
    println!("Exported '{name}' to {}", output.display());
    Ok(())
}

fn lock_stopped_machine_for_export(machine_dir: &Path) -> Result<File> {
    let state = RuntimeState::load(machine_dir)?.context("machine has no runtime state")?;
    if !matches!(state.state, MachineState::Stopped | MachineState::Failed)
        || runtime_is_live(&state, machine_dir)
    {
        bail!("machine must be fully stopped before export");
    }
    if supervisor_is_live(&state, machine_dir) {
        send_host_control(machine_dir, "close")
            .context("closing the stopped machine window before export")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while RuntimeState::load(machine_dir)?
            .as_ref()
            .is_some_and(|latest| supervisor_is_live(latest, machine_dir))
        {
            if Instant::now() >= deadline {
                bail!("the stopped machine window did not close before export");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let path = machine_dir.join("machine.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("machine lock is not a regular file");
    }
    file.try_lock_exclusive()
        .context("machine is in use by another process")?;
    Ok(file)
}

fn export_oci_archive(
    rootfs: &Path,
    machine_config_path: &Path,
    output: &Path,
    work_dir: &Path,
    generic_seed_source_date_epoch: Option<i64>,
) -> Result<()> {
    validate_guest_rootfs(rootfs)?;
    reject_rootfs_submounts(rootfs)?;
    let config = MachineConfig::load(
        machine_config_path
            .parent()
            .context("machine config has no machine directory")?,
    )?;
    let resources = ResourceLocator::discover()?;
    let tar = resources.helper_or_path("tar")?;
    let layout = work_dir.join("layout");
    let blob_dir = layout.join("blobs/sha256");
    fs::create_dir_all(&blob_dir).context("creating OCI layout blob directory")?;

    // Generic install media must not retain an imported machine identity, but
    // export must also remain read-only with respect to its source machine.
    // Make the reset in a private, exact tar copy inside this same guest-ID
    // namespace, then snapshot only that private copy.
    let generic_rootfs = generic_seed_source_date_epoch
        .map(|timestamp| copy_rootfs_for_generic_seed(&tar, rootfs, work_dir, timestamp))
        .transpose()?;
    let export_rootfs = generic_rootfs.as_deref().unwrap_or(rootfs);

    let layer_temporary = work_dir.join("rootfs-layer.tar.zst");
    let (diff_digest, layer_digest, layer_size) =
        write_rootfs_layer(&tar, export_rootfs, &layer_temporary)?;
    let layer_hex = validate_sha256_digest(&layer_digest)?;
    fs::rename(&layer_temporary, blob_dir.join(layer_hex))
        .context("committing OCI filesystem layer blob")?;

    let created = generic_seed_source_date_epoch
        .map(|timestamp| {
            DateTime::<Utc>::from_timestamp(timestamp, 0)
                .context("generic seed source timestamp is outside the supported range")
        })
        .transpose()?
        .unwrap_or(config.created_at)
        .to_rfc3339();
    let environment = if config.oci.environment.is_empty() {
        vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned()]
    } else {
        config.oci.environment.clone()
    };
    let entrypoint = if config.oci.entrypoint.is_empty() {
        vec!["/lib/systemd/systemd".to_owned()]
    } else {
        config.oci.entrypoint.clone()
    };
    let mut labels = config.oci.labels.clone();
    if generic_seed_source_date_epoch.is_some() {
        labels
            .entry("org.opencontainers.image.title".into())
            .or_insert_with(|| "Buzzard OS rootfs seed".into());
    } else {
        labels
            .entry("org.opencontainers.image.title".into())
            .or_insert_with(|| "Buzzard OS persistent machine export".into());
    }
    labels
        .entry("org.opencontainers.image.source".into())
        .or_insert_with(|| "https://github.com/openresearchtools/BuzzardOS".into());
    let mut process_config = serde_json::Map::new();
    process_config.insert("Env".into(), serde_json::to_value(environment)?);
    process_config.insert("Entrypoint".into(), serde_json::to_value(entrypoint)?);
    process_config.insert("Labels".into(), serde_json::to_value(labels)?);
    if !config.oci.command.is_empty() {
        process_config.insert("Cmd".into(), serde_json::to_value(&config.oci.command)?);
    }
    if let Some(value) = &config.oci.working_dir {
        process_config.insert("WorkingDir".into(), serde_json::to_value(value)?);
    }
    if let Some(value) = &config.oci.user {
        process_config.insert("User".into(), serde_json::to_value(value)?);
    }
    if let Some(value) = &config.oci.stop_signal {
        process_config.insert("StopSignal".into(), serde_json::to_value(value)?);
    }
    let image_config = serde_json::json!({
        "created": created,
        "architecture": host_oci_architecture()?,
        "os": "linux",
        "config": process_config,
        "rootfs": {"type": "layers", "diff_ids": [diff_digest]},
        "history": [{
            "created": created,
            "created_by": if generic_seed_source_date_epoch.is_some() {
                "BuzzardOS generic seed flattener"
            } else {
                "BuzzardOS export"
            },
            "comment": if generic_seed_source_date_epoch.is_some() {
                "identity-free flattened rootfs install seed"
            } else {
                "full persistent rootfs snapshot"
            }
        }]
    });
    let config_descriptor = write_json_blob(
        &blob_dir,
        "application/vnd.oci.image.config.v1+json",
        &image_config,
    )?;

    let mut annotations = BTreeMap::new();
    if generic_seed_source_date_epoch.is_some() {
        annotations.insert(
            "org.opencontainers.image.title".to_owned(),
            "Buzzard OS rootfs seed".to_owned(),
        );
    } else {
        let mut portable_config = config.clone();
        sanitize_imported_machine_config(&mut portable_config);
        let portable_config = serde_json::to_string(&portable_config)?;
        annotations.insert(
            "org.opencontainers.image.title".to_owned(),
            format!("Buzzard OS machine {}", config.name),
        );
        annotations.insert(BUZZARD_OCI_CONFIG_ANNOTATION.to_owned(), portable_config);
    }
    let manifest = OciManifest {
        schema_version: 2,
        config: config_descriptor,
        layers: vec![OciDescriptor {
            digest: layer_digest,
            size: layer_size,
            media_type: "application/vnd.oci.image.layer.v1.tar+zstd".into(),
            platform: None,
            annotations: BTreeMap::new(),
        }],
        annotations,
    };
    let manifest_descriptor = write_json_blob(
        &blob_dir,
        "application/vnd.oci.image.manifest.v1+json",
        &manifest,
    )?;
    let mut index_descriptor = manifest_descriptor;
    index_descriptor.platform = Some(OciPlatform {
        os: "linux".into(),
        architecture: host_oci_architecture()?.into(),
    });
    index_descriptor.annotations.insert(
        OCI_REF_NAME_ANNOTATION.into(),
        generic_seed_source_date_epoch.map_or_else(
            || format!("{}-snapshot", config.name),
            |_| "buzzardos-rootfs-seed".into(),
        ),
    );
    let index = OciIndex {
        schema_version: 2,
        manifests: vec![index_descriptor],
    };
    fs::write(layout.join("index.json"), serde_json::to_vec(&index)?)?;
    fs::write(
        layout.join("oci-layout"),
        b"{\"imageLayoutVersion\":\"1.0.0\"}\n",
    )?;
    sync_tree_metadata(&layout)?;
    write_compressed_layout_archive(&tar, &layout, output, generic_seed_source_date_epoch)?;
    Ok(())
}

fn write_rootfs_layer(tar: &Path, rootfs: &Path, output: &Path) -> Result<(String, String, u64)> {
    let mut command = Command::new(tar);
    command
        .args(["--create", "--file=-", "--directory"])
        .arg(rootfs);
    let mut child = command
        .args([
            "--format=pax",
            "--numeric-owner",
            "--acls",
            "--selinux",
            "--xattrs",
            "--xattrs-include=*",
            "--sparse",
            "--one-file-system",
            "--atime-preserve=system",
            "--exclude=./proc/*",
            "--exclude=./sys/*",
            "--exclude=./dev/*",
            "--exclude=./run/*",
            "--exclude=./tmp/*",
            "--exclude=./shared/*",
            "--sort=name",
            "--pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime",
            ".",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting bundled tar {}", tar.display()))?;
    let mut stdout = child.stdout.take().context("tar stdout was not piped")?;
    let file = File::create(output).with_context(|| format!("creating {}", output.display()))?;
    let writer = DigestingWriter::new(file);
    let mut encoder = zstd::stream::write::Encoder::new(writer, 15)
        .context("initializing OCI layer zstd encoder")?;
    encoder
        .multithread(zstd_worker_count())
        .context("enabling multithreaded OCI layer compression")?;
    let mut diff = Sha256::new();
    let mut buffer = [0_u8; 256 * 1024];
    loop {
        let count = stdout
            .read(&mut buffer)
            .context("reading rootfs tar stream")?;
        if count == 0 {
            break;
        }
        diff.update(&buffer[..count]);
        encoder
            .write_all(&buffer[..count])
            .context("compressing rootfs tar stream")?;
    }
    let writer = encoder
        .finish()
        .context("finishing OCI layer compression")?;
    let (file, compressed_digest, compressed_size) = writer.finish();
    file.sync_all().context("syncing OCI layer")?;
    let status = child.wait().context("waiting for rootfs tar")?;
    if !status.success() {
        bail!("bundled tar failed while exporting rootfs with {status}");
    }
    Ok((
        format!("sha256:{:x}", diff.finalize()),
        compressed_digest,
        compressed_size,
    ))
}

fn copy_rootfs_for_generic_seed(
    tar: &Path,
    source: &Path,
    work_dir: &Path,
    source_date_epoch: i64,
) -> Result<PathBuf> {
    let destination = work_dir.join("generic-rootfs");
    fs::create_dir(&destination).context("creating private generic-seed rootfs stage")?;

    let mut producer = Command::new(tar)
        .args(["--create", "--file=-", "--directory"])
        .arg(source)
        .args([
            "--format=pax",
            "--numeric-owner",
            "--acls",
            "--selinux",
            "--xattrs",
            "--xattrs-include=*",
            "--sparse",
            "--one-file-system",
            "--atime-preserve=system",
            "--exclude=./proc/*",
            "--exclude=./sys/*",
            "--exclude=./dev/*",
            "--exclude=./run/*",
            "--exclude=./tmp/*",
            "--exclude=./shared/*",
            "--sort=name",
            "--pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime",
            ".",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "starting bundled tar {} for private seed copy",
                tar.display()
            )
        })?;
    let producer_stdout = producer
        .stdout
        .take()
        .context("private seed copy tar stdout was not piped")?;
    let mut consumer = Command::new(tar)
        .args(["--extract", "--file=-", "--directory"])
        .arg(&destination)
        .args([
            "--numeric-owner",
            "--same-owner",
            "--acls",
            "--selinux",
            "--xattrs",
            "--xattrs-include=*",
            "--sparse",
        ])
        .stdin(Stdio::from(producer_stdout))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "starting bundled tar {} to restore private seed copy",
                tar.display()
            )
        })?;
    let producer_status = producer
        .wait()
        .context("waiting for private seed copy writer")?;
    let consumer_status = consumer
        .wait()
        .context("waiting for private seed copy reader")?;
    if !producer_status.success() || !consumer_status.success() {
        bail!(
            "private generic-seed rootfs copy failed: writer={producer_status}, reader={consumer_status}"
        );
    }

    reset_cloned_rootfs_identity(&destination)?;
    for relative in ["", "etc/machine-id", "etc", "etc/ssh", "var/lib/systemd"] {
        let path = destination.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(_) => set_link_mtime(&path, (source_date_epoch, 0))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    validate_identity_free_rootfs(&destination)?;
    Ok(destination)
}

fn validate_identity_free_rootfs(rootfs: &Path) -> Result<()> {
    if !fs::read(rootfs.join("etc/machine-id"))?.is_empty() {
        bail!("generic seed staging rootfs retains a machine ID");
    }
    if rootfs.join("var/lib/systemd/random-seed").exists() {
        bail!("generic seed staging rootfs retains a systemd random seed");
    }
    let ssh = rootfs.join("etc/ssh");
    match fs::symlink_metadata(&ssh) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            for entry in fs::read_dir(&ssh)? {
                if entry?.file_name().as_bytes().starts_with(b"ssh_host_") {
                    bail!("generic seed staging rootfs retains an SSH host identity");
                }
            }
        }
        Ok(_) => bail!("generic seed staging SSH directory has an unsafe type"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn write_json_blob<T: Serialize>(
    blob_dir: &Path,
    media_type: &str,
    value: &T,
) -> Result<OciDescriptor> {
    let bytes = serde_json::to_vec(value)?;
    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    let hexadecimal = validate_sha256_digest(&digest)?;
    let path = blob_dir.join(hexadecimal);
    fs::write(&path, &bytes).with_context(|| format!("writing OCI blob {}", path.display()))?;
    File::open(&path)?.sync_all()?;
    Ok(OciDescriptor {
        digest,
        size: bytes.len() as u64,
        media_type: media_type.into(),
        platform: None,
        annotations: BTreeMap::new(),
    })
}

fn write_compressed_layout_archive(
    tar: &Path,
    layout: &Path,
    output: &Path,
    normalized_mtime: Option<i64>,
) -> Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(output)
        .with_context(|| format!("opening export output {}", output.display()))?;
    if !file.metadata()?.is_file() {
        bail!("export output must be a regular file");
    }
    let mut command = Command::new(tar);
    command
        .args(["--create", "--file=-", "--directory"])
        .arg(layout);
    if let Some(timestamp) = normalized_mtime {
        command.arg(format!("--mtime=@{timestamp}"));
    }
    let mut child = command
        .args([
            "--format=posix",
            "--sort=name",
            "--pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime",
            ".",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("starting OCI layout archiver")?;
    let stdout = child
        .stdout
        .take()
        .context("layout tar stdout was not piped")?;
    let mut encoder = zstd::stream::write::Encoder::new(file, 19)
        .context("initializing OCI archive compressor")?;
    encoder
        .multithread(zstd_worker_count())
        .context("enabling multithreaded OCI archive compression")?;
    std::io::copy(&mut BufReader::new(stdout), &mut encoder)
        .context("compressing OCI layout archive")?;
    let file = encoder
        .finish()
        .context("finishing OCI archive compression")?;
    file.sync_all().context("syncing OCI archive")?;
    let status = child.wait().context("waiting for OCI layout tar")?;
    if !status.success() {
        bail!("bundled tar failed while archiving OCI layout with {status}");
    }
    Ok(())
}

fn zstd_worker_count() -> u32 {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(8) as u32
}

fn reject_rootfs_submounts(rootfs: &Path) -> Result<()> {
    let canonical = rootfs.canonicalize()?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").context("reading mountinfo")?;
    for line in mountinfo.lines() {
        let Some(field) = line.split_whitespace().nth(4) else {
            continue;
        };
        let decoded = field
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        let mount = Path::new(&decoded);
        if mount != canonical && mount.starts_with(&canonical) {
            bail!(
                "rootfs contains active mount {}; stop and fully detach the machine before export",
                mount.display()
            );
        }
    }
    Ok(())
}

fn sync_tree_metadata(root: &Path) -> Result<()> {
    for directory in [
        root.join("blobs/sha256"),
        root.join("blobs"),
        root.to_path_buf(),
    ] {
        File::open(&directory)
            .with_context(|| format!("opening {} for sync", directory.display()))?
            .sync_all()
            .with_context(|| format!("syncing {}", directory.display()))?;
    }
    Ok(())
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    File::open(path.parent().context("path has no parent")?)?
        .sync_all()
        .context("syncing parent directory")
}

fn clone_machine(paths: &WbPaths, source: &str, name: &str) -> Result<()> {
    MachineConfig::validate_name(name)?;
    let temporary = tempfile::Builder::new()
        .prefix("clone-")
        .suffix(".oci.tar.zst")
        .tempfile_in(paths.cache())?;
    let archive_path = temporary.path().to_path_buf();
    // `export_machine` deliberately refuses replacement. Close removes only
    // this private placeholder before handing its randomized name to the
    // exporter; an intervening collision is then rejected rather than
    // replaced.
    temporary
        .close()
        .context("releasing the clone export placeholder")?;
    export_machine(paths, source, &archive_path, None)?;
    let result = import_machine(
        paths,
        archive_path
            .to_str()
            .context("clone archive path is not UTF-8")?,
        name,
        None,
        ImportModeArg::Clone,
        Some(&format!("clone:{source}")),
    );
    let _ = fs::remove_file(&archive_path);
    result
}

fn reset_cloned_machine_identity_in_stage(
    resources: &ResourceLocator,
    rootfs: &Path,
) -> Result<()> {
    let unshare = resources.helper_or_path("unshare")?;
    let id_map = IdMap::discover()?;
    let namespace_program = id_map.namespace_program(&unshare)?;
    let launcher = std::env::current_exe()?;
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.namespace_args())
        .arg(launcher)
        .arg("__reset-clone-identity")
        .arg("--rootfs")
        .arg(rootfs)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        bail!("clone identity reset failed with {status}");
    }
    Ok(())
}

fn reset_cloned_rootfs_identity(rootfs: &Path) -> Result<()> {
    validate_guest_rootfs(rootfs)?;
    let rootfs = rootfs.canonicalize()?;
    let machine_id = rootfs.join("etc/machine-id");
    let mut file = match OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&machine_id)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&machine_id)
            .with_context(|| format!("creating {}", machine_id.display()))?,
        Err(error) => {
            return Err(error).with_context(|| format!("resetting {}", machine_id.display()));
        }
    };
    file.flush()?;
    file.sync_all()?;

    for relative in ["var/lib/systemd/random-seed", "var/lib/dbus/machine-id"] {
        let path = rootfs.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                // Preserve the normal /var/lib/dbus/machine-id -> /etc/machine-id
                // link; the target was reset above. Other identity material is
                // removed only when it is a regular file.
                if relative != "var/lib/dbus/machine-id" {
                    fs::remove_file(&path)?;
                }
            }
            Ok(metadata) if metadata.is_file() => fs::remove_file(&path)?,
            Ok(_) => bail!("clone identity path {} has an unsafe type", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let ssh = rootfs.join("etc/ssh");
    match fs::symlink_metadata(&ssh) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            for entry in fs::read_dir(&ssh)? {
                let entry = entry?;
                let name = entry.file_name();
                if name.as_bytes().starts_with(b"ssh_host_") {
                    let metadata = fs::symlink_metadata(entry.path())?;
                    if metadata.is_file() || metadata.file_type().is_symlink() {
                        fs::remove_file(entry.path())?;
                    } else {
                        bail!("SSH host identity entry has an unsafe type");
                    }
                }
            }
        }
        Ok(_) => bail!("SSH identity directory has an unsafe type"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    sync_parent_directory(&machine_id)?;
    Ok(())
}

fn cleanup_failed_machine_stage(
    resources: &ResourceLocator,
    staging: &Path,
    machines: &Path,
) -> Result<()> {
    match fs::remove_dir_all(staging) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }

    let unshare = resources.helper_or_path("unshare")?;
    let id_map = IdMap::discover()?;
    let namespace_program = id_map.namespace_program(&unshare)?;
    let launcher = std::env::current_exe().context("locating launcher for staging cleanup")?;
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.namespace_args())
        .arg(launcher)
        .arg("__cleanup-staging")
        .arg("--staging")
        .arg(staging)
        .arg("--machines")
        .arg(machines)
        .stdin(Stdio::null())
        .status()
        .with_context(|| {
            format!(
                "starting cleanup namespace with {}",
                namespace_program.display()
            )
        })?;
    if !status.success() {
        bail!("staging cleanup namespace exited with {status}");
    }
    match fs::symlink_metadata(staging) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => bail!("staging cleanup reported success but the tree still exists"),
        Err(error) => Err(error).context("verifying staging cleanup"),
    }
}

fn cleanup_export_stage(resources: &ResourceLocator, staging: &Path, cache: &Path) -> Result<()> {
    match fs::remove_dir_all(staging) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }

    let unshare = resources.helper_or_path("unshare")?;
    let id_map = IdMap::discover()?;
    let namespace_program = id_map.namespace_program(&unshare)?;
    let launcher = std::env::current_exe().context("locating launcher for export cleanup")?;
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.namespace_args())
        .arg(launcher)
        .arg("__cleanup-export-staging")
        .arg("--staging")
        .arg(staging)
        .arg("--cache")
        .arg(cache)
        .stdin(Stdio::null())
        .status()
        .with_context(|| {
            format!(
                "starting export cleanup namespace with {}",
                namespace_program.display()
            )
        })?;
    if !status.success() {
        bail!("export staging cleanup namespace exited with {status}");
    }
    match fs::symlink_metadata(staging) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => bail!("export staging cleanup reported success but the tree still exists"),
        Err(error) => Err(error).context("verifying export staging cleanup"),
    }
}

fn delete_machine(paths: &WbPaths, name: &str, confirmed: bool) -> Result<()> {
    if !confirmed {
        bail!(
            "deleting a machine permanently removes its rootfs; rerun with `./BuzzardOS delete {name} --yes`"
        );
    }
    let machine_dir = require_machine(paths, name)?;
    let lock = lock_stopped_machine_for_export(&machine_dir)?;
    let resources = ResourceLocator::discover()?;
    let unshare = resources.helper_or_path("unshare")?;
    let id_map = IdMap::discover()?;
    let namespace_program = id_map.namespace_program(&unshare)?;
    let launcher = std::env::current_exe()?;
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.namespace_args())
        .arg(launcher)
        .arg("__delete-machine")
        .arg("--machine")
        .arg(&machine_dir)
        .arg("--machines")
        .arg(paths.machines())
        .stdin(Stdio::null())
        .status()?;
    drop(lock);
    if !status.success() {
        bail!("machine deletion namespace exited with {status}");
    }
    println!("Deleted machine '{name}' and its persistent rootfs");
    Ok(())
}

fn remove_persistent_machine_tree(machine: &Path, machines: &Path) -> Result<()> {
    let machines = machines
        .canonicalize()
        .with_context(|| format!("resolving {}", machines.display()))?;
    let parent = machine
        .parent()
        .context("machine has no parent")?
        .canonicalize()?;
    if parent != machines {
        bail!("machine deletion target is outside the portable Machines directory");
    }
    let name = machine
        .file_name()
        .and_then(|value| value.to_str())
        .context("machine directory name is not UTF-8")?;
    MachineConfig::validate_name(name)?;
    let metadata = fs::symlink_metadata(machine)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("machine deletion target must be a real directory");
    }
    let config = MachineConfig::load(machine)?;
    if config.name != name {
        bail!("machine metadata name does not match the deletion target");
    }
    fs::remove_dir_all(machine)
        .with_context(|| format!("removing machine tree {}", machine.display()))?;
    File::open(&machines)?.sync_all()?;
    Ok(())
}

fn remove_machine_staging_tree(staging: &Path, machines: &Path) -> Result<()> {
    let machines_metadata = fs::symlink_metadata(machines)
        .with_context(|| format!("inspecting machine directory {}", machines.display()))?;
    if machines_metadata.file_type().is_symlink() || !machines_metadata.is_dir() {
        bail!("machine directory must be a real directory");
    }
    let staging_metadata = fs::symlink_metadata(staging)
        .with_context(|| format!("inspecting staging directory {}", staging.display()))?;
    if staging_metadata.file_type().is_symlink() || !staging_metadata.is_dir() {
        bail!("machine staging path must be a real directory");
    }
    let name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .context("machine staging directory name is not UTF-8")?;
    if !is_machine_staging_name(name) {
        bail!("refusing to remove a path that is not a machine create/import staging directory");
    }
    let expected_parent = machines
        .canonicalize()
        .with_context(|| format!("resolving machine directory {}", machines.display()))?;
    let actual_parent = staging
        .parent()
        .context("machine staging path has no parent")?
        .canonicalize()
        .context("resolving machine staging parent")?;
    if actual_parent != expected_parent {
        bail!("machine staging directory is outside the expected machine directory");
    }
    fs::remove_dir_all(staging)
        .with_context(|| format!("removing failed machine staging tree {}", staging.display()))
}

fn remove_export_staging_tree(staging: &Path, cache: &Path) -> Result<()> {
    let cache_metadata = fs::symlink_metadata(cache)
        .with_context(|| format!("inspecting export cache directory {}", cache.display()))?;
    if cache_metadata.file_type().is_symlink() || !cache_metadata.is_dir() {
        bail!("export cache directory must be a real directory");
    }
    let staging_metadata = fs::symlink_metadata(staging)
        .with_context(|| format!("inspecting export staging directory {}", staging.display()))?;
    if staging_metadata.file_type().is_symlink() || !staging_metadata.is_dir() {
        bail!("export staging path must be a real directory");
    }
    let name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .context("export staging directory name is not UTF-8")?;
    let nonce = name
        .strip_prefix("oci-export-")
        .filter(|nonce| !nonce.is_empty() && nonce.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        .context("refusing to remove a path that is not an OCI export staging directory")?;
    debug_assert!(!nonce.is_empty());
    let expected_parent = cache
        .canonicalize()
        .with_context(|| format!("resolving export cache directory {}", cache.display()))?;
    let actual_parent = staging
        .parent()
        .context("export staging path has no parent")?
        .canonicalize()
        .context("resolving export staging parent")?;
    if actual_parent != expected_parent {
        bail!("export staging directory is outside the expected cache directory");
    }
    fs::remove_dir_all(staging)
        .with_context(|| format!("removing OCI export staging tree {}", staging.display()))
}

fn is_machine_staging_name(name: &str) -> bool {
    let Some(name) = name.strip_prefix('.') else {
        return false;
    };
    ["-creating-", "-importing-"].iter().any(|marker| {
        let Some((machine, nonce)) = name.rsplit_once(marker) else {
            return false;
        };
        !nonce.is_empty()
            && nonce.bytes().all(|byte| byte.is_ascii_alphanumeric())
            && MachineConfig::validate_name(machine).is_ok()
    })
}

fn commit_new_machine(staging: &Path, destination: &Path) -> Result<()> {
    let destination_display = destination.display().to_string();
    let staging = CString::new(staging.as_os_str().as_bytes())
        .context("machine staging path contains a NUL byte")?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .context("machine destination path contains a NUL byte")?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            staging.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(libc::EEXIST) | Some(libc::ENOTEMPTY)
    ) {
        bail!("machine destination {destination_display} already exists; it was not replaced");
    }
    Err(error).context("atomically renaming the completed machine")
}

#[derive(Debug, Clone, Deserialize)]
struct RootfsSeedManifest {
    schema: u32,
    kind: String,
    platform: RootfsSeedPlatform,
    archive: RootfsSeedArchive,
}

#[derive(Debug, Clone, Deserialize)]
struct RootfsSeedPlatform {
    os: String,
    architecture: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RootfsSeedArchive {
    name: String,
    media_type: String,
    size: u64,
    sha256: String,
    uncompressed_size: u64,
    uncompressed_sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledOciSeedManifest {
    schema: u32,
    kind: String,
    platform: RootfsSeedPlatform,
    archive: BundledOciSeedArchive,
    manifest_digest: String,
    source_manifest_digest: String,
    source_commit: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundledOciSeedArchive {
    name: String,
    size: u64,
    sha256: String,
}

fn bundled_rootfs_oci_archive(paths: &WbPaths) -> Result<PathBuf> {
    let runtime = paths.base().join("app/runtime");
    let runtime_metadata = fs::symlink_metadata(&runtime).with_context(|| {
        format!(
            "portable bundle is incomplete: app/runtime/{DEFAULT_ROOTFS_OCI_ARCHIVE} is missing"
        )
    })?;
    if runtime_metadata.file_type().is_symlink() || !runtime_metadata.is_dir() {
        bail!("portable app/runtime must be a real directory");
    }
    let archive = runtime.join(DEFAULT_ROOTFS_OCI_ARCHIVE);
    let metadata = fs::symlink_metadata(&archive).with_context(|| {
        format!(
            "portable bundle is incomplete: app/runtime/{DEFAULT_ROOTFS_OCI_ARCHIVE} is missing"
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() < 1024 {
        bail!("bundled OCI rootfs must be a non-empty regular file");
    }
    let manifest_path = runtime.join(DEFAULT_ROOTFS_OCI_MANIFEST);
    let mut manifest_file = open_regular_nofollow(&manifest_path, "bundled OCI manifest")?;
    let manifest_size = manifest_file.metadata()?.len();
    if manifest_size == 0 || manifest_size > MAX_ROOTFS_MANIFEST_BYTES {
        bail!("bundled OCI manifest has an invalid size");
    }
    let mut manifest_bytes = Vec::with_capacity(manifest_size as usize);
    manifest_file
        .read_to_end(&mut manifest_bytes)
        .context("reading bundled OCI manifest")?;
    let manifest: BundledOciSeedManifest =
        serde_json::from_slice(&manifest_bytes).context("parsing bundled OCI manifest")?;
    if manifest.schema != 1 || manifest.kind != "buzzardos-oci-seed" {
        bail!("bundled OCI manifest has an unsupported schema or kind");
    }
    if manifest.platform.os != "linux" || manifest.platform.architecture != "amd64" {
        bail!("bundled OCI seed is not for linux/amd64");
    }
    if manifest.archive.name != DEFAULT_ROOTFS_OCI_ARCHIVE {
        bail!("bundled OCI manifest names the wrong archive");
    }
    validate_sha256_hex(&manifest.archive.sha256, "bundled OCI archive")?;
    validate_sha256_digest(&manifest.manifest_digest)?;
    validate_sha256_digest(&manifest.source_manifest_digest)?;
    if !matches!(manifest.source_commit.len(), 40 | 64)
        || !manifest
            .source_commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("bundled OCI source commit is not a lowercase Git object ID");
    }
    if metadata.len() != manifest.archive.size {
        bail!("bundled OCI archive size differs from its manifest");
    }
    if sha256_regular_file(&archive)? != manifest.archive.sha256 {
        bail!("bundled OCI archive digest differs from its manifest");
    }
    Ok(archive)
}

fn open_regular_nofollow(path: &Path, description: &str) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening {description} {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting {description} {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("{description} {} is not a regular file", path.display());
    }
    Ok(file)
}

fn read_rootfs_seed_manifest(path: &Path) -> Result<RootfsSeedManifest> {
    if path.file_name() != Some(std::ffi::OsStr::new(ROOTFS_SEED_MANIFEST)) {
        bail!("bundled rootfs manifest must be named {ROOTFS_SEED_MANIFEST}");
    }
    let mut file = open_regular_nofollow(path, "bundled rootfs manifest")?;
    let size = file.metadata()?.len();
    if size == 0 || size > MAX_ROOTFS_MANIFEST_BYTES {
        bail!(
            "bundled rootfs manifest has invalid size {size}; maximum is {MAX_ROOTFS_MANIFEST_BYTES} bytes"
        );
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.read_to_end(&mut bytes)
        .context("reading bundled rootfs manifest")?;
    let manifest: RootfsSeedManifest =
        serde_json::from_slice(&bytes).context("parsing bundled rootfs manifest")?;
    validate_rootfs_seed_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_rootfs_seed_manifest(manifest: &RootfsSeedManifest) -> Result<()> {
    if manifest.schema != ROOTFS_SEED_SCHEMA {
        bail!(
            "unsupported bundled rootfs manifest schema {}",
            manifest.schema
        );
    }
    if manifest.kind != ROOTFS_SEED_KIND {
        bail!(
            "bundled rootfs manifest has unexpected kind '{}'",
            manifest.kind
        );
    }
    if manifest.platform.os != "linux" || manifest.platform.architecture != "amd64" {
        bail!(
            "bundled rootfs platform must be linux/amd64, not {}/{}",
            manifest.platform.os,
            manifest.platform.architecture
        );
    }
    if std::env::consts::ARCH != "x86_64" {
        bail!(
            "the bundled linux/amd64 rootfs cannot run on host architecture '{}'",
            std::env::consts::ARCH
        );
    }
    if manifest.archive.name != ROOTFS_SEED_ARCHIVE {
        bail!(
            "bundled rootfs archive name must be {ROOTFS_SEED_ARCHIVE}, not {}",
            manifest.archive.name
        );
    }
    if manifest.archive.media_type != ROOTFS_SEED_MEDIA_TYPE {
        bail!(
            "unsupported bundled rootfs media type {}",
            manifest.archive.media_type
        );
    }
    validate_sha256_hex(&manifest.archive.sha256, "rootfs archive")?;
    validate_sha256_hex(
        &manifest.archive.uncompressed_sha256,
        "uncompressed rootfs archive",
    )?;
    if manifest.archive.size < 4 || manifest.archive.uncompressed_size < 1024 {
        bail!("bundled rootfs archive sizes are invalid");
    }
    Ok(())
}

fn validate_sha256_hex(value: &str, description: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{description} sha256 is not 64 lowercase hexadecimal characters");
    }
    Ok(())
}

struct HashingReader<R> {
    inner: R,
    hash: Sha256,
    bytes: u64,
}

impl<R> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> (R, u64, String) {
        (
            self.inner,
            self.bytes,
            format!("{:x}", self.hash.finalize()),
        )
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hash.update(&buffer[..read]);
        self.bytes = self.bytes.saturating_add(read as u64);
        Ok(read)
    }
}

#[derive(Debug)]
struct DeferredRootfsDirectory {
    relative: PathBuf,
    uid: u32,
    gid: u32,
    mode: u32,
    mtime: (i64, i64),
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

fn apply_rootfs_seed(
    archive_path: &Path,
    manifest_path: &Path,
    expected_digest: &str,
    rootfs: &Path,
) -> Result<()> {
    if archive_path.parent() != manifest_path.parent() {
        bail!("bundled rootfs archive and manifest must share one runtime directory");
    }
    let runtime = archive_path
        .parent()
        .context("bundled rootfs archive has no runtime directory")?;
    let runtime_metadata = fs::symlink_metadata(runtime)
        .with_context(|| format!("inspecting bundled runtime directory {}", runtime.display()))?;
    if runtime_metadata.file_type().is_symlink() || !runtime_metadata.is_dir() {
        bail!("bundled rootfs runtime directory must be a real directory");
    }

    let manifest = read_rootfs_seed_manifest(manifest_path)?;
    let expected_hex = expected_digest
        .strip_prefix("sha256:")
        .context("bundled rootfs expected digest does not use sha256")?;
    validate_sha256_hex(expected_hex, "expected rootfs archive")?;
    if expected_hex != manifest.archive.sha256 {
        bail!(
            "bundled rootfs manifest digest changed: expected {expected_hex}, manifest says {}",
            manifest.archive.sha256
        );
    }

    let rootfs_metadata = fs::symlink_metadata(rootfs)
        .with_context(|| format!("inspecting persistent rootfs {}", rootfs.display()))?;
    if rootfs_metadata.file_type().is_symlink() || !rootfs_metadata.is_dir() {
        bail!("persistent rootfs must be a real directory, not a symlink");
    }
    if fs::read_dir(rootfs)
        .context("inspecting new persistent rootfs")?
        .next()
        .is_some()
    {
        bail!("bundled rootfs seed may only be applied to an empty new rootfs");
    }
    #[cfg(not(test))]
    chown(rootfs, Some(Uid::from_raw(0)), Some(Gid::from_raw(0)))
        .with_context(|| format!("setting root ownership on {}", rootfs.display()))?;

    let mut file = open_regular_nofollow(archive_path, "bundled rootfs archive")?;
    let actual_size = file.metadata()?.len();
    if actual_size != manifest.archive.size {
        bail!(
            "bundled rootfs archive size mismatch: manifest says {}, file has {actual_size}",
            manifest.archive.size
        );
    }
    let mut preflight_hash = Sha256::new();
    std::io::copy(&mut file, &mut preflight_hash).context("hashing bundled rootfs archive")?;
    let preflight_hash = format!("{:x}", preflight_hash.finalize());
    if preflight_hash != manifest.archive.sha256 {
        bail!(
            "bundled rootfs archive digest mismatch: expected {}, got {preflight_hash}",
            manifest.archive.sha256
        );
    }
    file.rewind().context("rewinding bundled rootfs archive")?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .context("reading bundled rootfs archive header")?;
    if magic != [0x28, 0xb5, 0x2f, 0xfd] {
        bail!("bundled rootfs archive is not a Zstandard frame");
    }
    file.rewind().context("rewinding bundled rootfs archive")?;

    let compressed = HashingReader::new(file);
    let decoder = zstd::stream::read::Decoder::new(compressed)
        .context("initializing bundled rootfs Zstandard decoder")?;
    let uncompressed = HashingReader::new(decoder);
    let mut tar_archive = tar::Archive::new(uncompressed);
    tar_archive.set_preserve_permissions(true);
    tar_archive.set_preserve_mtime(true);
    tar_archive.set_preserve_ownerships(true);
    tar_archive.set_unpack_xattrs(false);

    let mut directories = Vec::new();
    let mut entry_count = 0_u64;
    {
        let entries = tar_archive
            .entries()
            .context("reading bundled rootfs tar archive")?;
        for item in entries {
            let mut entry = item.context("reading bundled rootfs tar entry")?;
            entry_count = entry_count.saturating_add(1);
            let raw_path = entry
                .path()
                .context("reading bundled rootfs entry path")?
                .into_owned();
            let relative = safe_relative_path(&raw_path)?.to_path_buf();
            if rootfs_path_contains_whiteout(&relative) {
                bail!(
                    "flat rootfs archive contains forbidden OCI whiteout {}",
                    relative.display()
                );
            }

            let entry_type = entry.header().entry_type();
            if !matches!(
                entry_type,
                tar::EntryType::Regular
                    | tar::EntryType::Continuous
                    | tar::EntryType::GNUSparse
                    | tar::EntryType::Directory
                    | tar::EntryType::Symlink
                    | tar::EntryType::Link
            ) {
                bail!(
                    "flat rootfs archive contains unsupported special entry {} ({entry_type:?})",
                    relative.display()
                );
            }
            let uid = entry
                .header()
                .uid()
                .context("reading bundled rootfs entry UID")?;
            let gid = entry
                .header()
                .gid()
                .context("reading bundled rootfs entry GID")?;
            if uid > MAX_GUEST_ID || gid > MAX_GUEST_ID {
                bail!(
                    "flat rootfs entry {} uses unsupported guest ownership {uid}:{gid}; maximum is {MAX_GUEST_ID}",
                    relative.display()
                );
            }
            if entry_type == tar::EntryType::Link {
                let target = entry
                    .link_name()
                    .context("reading bundled rootfs hardlink target")?
                    .context("bundled rootfs hardlink has no target")?
                    .into_owned();
                let target = safe_relative_path(&target)?;
                if target.as_os_str().is_empty() {
                    bail!("bundled rootfs hardlink target cannot be empty");
                }
            }

            let mode = entry
                .header()
                .mode()
                .context("reading bundled rootfs entry mode")?;
            let mut mtime = (
                i64::try_from(
                    entry
                        .header()
                        .mtime()
                        .context("reading bundled rootfs entry mtime")?,
                )
                .context("bundled rootfs entry mtime is outside the supported range")?,
                0_i64,
            );
            let mut xattrs = Vec::new();
            if let Some(extensions) = entry
                .pax_extensions()
                .context("reading bundled rootfs PAX metadata")?
            {
                for extension in extensions {
                    let extension = extension.context("reading bundled rootfs PAX record")?;
                    if extension.key_bytes() == b"mtime" {
                        mtime = parse_pax_timestamp(extension.value_bytes())?;
                    } else if let Some(name) = extension.key_bytes().strip_prefix(b"SCHILY.xattr.")
                    {
                        if name.is_empty() {
                            bail!("bundled rootfs archive contains an empty xattr name");
                        }
                        xattrs.push((name.to_vec(), extension.value_bytes().to_vec()));
                    }
                }
            }

            if entry_type == tar::EntryType::Directory {
                if !relative.as_os_str().is_empty()
                    && !entry
                        .unpack_in(rootfs)
                        .with_context(|| format!("extracting {}", relative.display()))?
                {
                    bail!(
                        "bundled rootfs entry {} escaped the destination",
                        relative.display()
                    );
                }
                directories.push(DeferredRootfsDirectory {
                    relative,
                    uid: uid as u32,
                    gid: gid as u32,
                    mode,
                    mtime,
                    xattrs,
                });
                continue;
            }
            if relative.as_os_str().is_empty() {
                bail!("bundled rootfs archive contains a non-directory root entry");
            }
            if !entry
                .unpack_in(rootfs)
                .with_context(|| format!("extracting {}", relative.display()))?
            {
                bail!(
                    "bundled rootfs entry {} escaped the destination",
                    relative.display()
                );
            }
            let destination = rootfs.join(&relative);
            // A hardlink is another name for metadata already applied to its
            // target. Its tar header's placeholder mode/mtime must not mutate
            // the shared inode after the target was restored.
            if entry_type != tar::EntryType::Link {
                for (name, value) in xattrs {
                    set_link_xattr(&destination, &name, &value)?;
                }
                set_link_mtime(&destination, mtime)?;
            }
        }
    }

    let mut uncompressed = tar_archive.into_inner();
    std::io::copy(&mut uncompressed, &mut std::io::sink())
        .context("finishing bundled rootfs decompression")?;
    let (decoder, uncompressed_size, uncompressed_hash) = uncompressed.finish();
    let compressed_buffer = decoder.finish();
    let (_file, compressed_size, compressed_hash) = compressed_buffer.into_inner().finish();
    if compressed_size != manifest.archive.size || compressed_hash != manifest.archive.sha256 {
        bail!("bundled rootfs archive changed while it was being extracted");
    }
    if uncompressed_size != manifest.archive.uncompressed_size
        || uncompressed_hash != manifest.archive.uncompressed_sha256
    {
        bail!("uncompressed bundled rootfs digest or size does not match its provenance manifest");
    }
    if entry_count == 0 {
        bail!("bundled rootfs archive is empty");
    }

    directories.sort_by_key(|directory| std::cmp::Reverse(directory.relative.components().count()));
    for directory in directories {
        apply_deferred_rootfs_directory(rootfs, directory)?;
    }
    validate_extracted_rootfs(rootfs)
}

fn rootfs_path_contains_whiteout(path: &Path) -> bool {
    path.components().any(|component| match component {
        std::path::Component::Normal(name) => name.as_bytes().starts_with(b".wh."),
        _ => false,
    })
}

fn parse_pax_timestamp(value: &[u8]) -> Result<(i64, i64)> {
    let value = std::str::from_utf8(value).context("PAX mtime is not UTF-8")?;
    let negative = value.starts_with('-');
    let unsigned = if matches!(value.as_bytes().first(), Some(b'-' | b'+')) {
        &value[1..]
    } else {
        value
    };
    let (seconds, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if seconds.is_empty()
        || !seconds.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        bail!("invalid PAX mtime '{value}'");
    }
    let seconds: i64 = seconds.parse().context("PAX mtime seconds overflow")?;
    let mut nanoseconds = fraction
        .bytes()
        .take(9)
        .fold(0_i64, |value, digit| value * 10 + i64::from(digit - b'0'));
    for _ in fraction.len().min(9)..9 {
        nanoseconds *= 10;
    }
    if negative {
        if nanoseconds == 0 {
            Ok((-seconds, 0))
        } else {
            Ok((
                seconds
                    .checked_neg()
                    .and_then(|seconds| seconds.checked_sub(1))
                    .context("PAX mtime seconds overflow")?,
                1_000_000_000 - nanoseconds,
            ))
        }
    } else {
        Ok((seconds, nanoseconds))
    }
}

fn set_link_mtime(path: &Path, mtime: (i64, i64)) -> Result<()> {
    let path =
        CString::new(path.as_os_str().as_bytes()).context("mtime path contains a NUL byte")?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: mtime.0,
            tv_nsec: mtime.1,
        },
    ];
    let result = unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("preserving rootfs entry mtime")
    }
}

fn apply_deferred_rootfs_directory(
    rootfs: &Path,
    directory: DeferredRootfsDirectory,
) -> Result<()> {
    let destination = if directory.relative.as_os_str().is_empty() {
        rootfs.to_path_buf()
    } else {
        rootfs.join(&directory.relative)
    };
    let metadata = fs::symlink_metadata(&destination)
        .with_context(|| format!("inspecting extracted directory {}", destination.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "rootfs archive directory {} was replaced with a non-directory",
            directory.relative.display()
        );
    }
    chown(
        &destination,
        Some(Uid::from_raw(directory.uid)),
        Some(Gid::from_raw(directory.gid)),
    )
    .with_context(|| format!("preserving ownership on {}", destination.display()))?;
    fs::set_permissions(&destination, fs::Permissions::from_mode(directory.mode))
        .with_context(|| format!("preserving permissions on {}", destination.display()))?;
    for (name, value) in directory.xattrs {
        set_link_xattr(&destination, &name, &value)?;
    }
    set_link_mtime(&destination, directory.mtime)
}

fn validate_extracted_rootfs(rootfs: &Path) -> Result<()> {
    let canonical_rootfs = rootfs
        .canonicalize()
        .with_context(|| format!("resolving extracted rootfs {}", rootfs.display()))?;
    for required in [
        "lib/systemd/systemd",
        "opt/wildbuzzard/runtime/current/bin/sway",
        "opt/wildbuzzard/runtime/current/bin/swaymsg",
        "opt/wildbuzzard/runtime/current/bin/cua-driver",
        "opt/wildbuzzard/runtime/current/libexec/wildbuzzard-clipboard-agent",
        "opt/wildbuzzard/runtime/current/libexec/wildbuzzard-settings",
        "opt/wildbuzzard/runtime/current/libexec/wildbuzzard-shell",
        "usr/libexec/wildbuzzard-shortcut-helper",
        "var/lib/dpkg/status",
    ] {
        let path = rootfs.join(required);
        let resolved = path
            .canonicalize()
            .with_context(|| format!("bundled rootfs is missing required file /{required}"))?;
        if !resolved.starts_with(&canonical_rootfs) {
            bail!("bundled rootfs /{required} escapes through a symlink");
        }
        let metadata = fs::metadata(&resolved)
            .with_context(|| format!("inspecting bundled rootfs file /{required}"))?;
        if !metadata.is_file() {
            bail!("bundled rootfs /{required} must resolve to a regular file");
        }
    }
    Ok(())
}

fn apply_image_in_user_namespace(
    resources: &ResourceLocator,
    archive: &Path,
    expected_digest: &str,
    rootfs: &Path,
    work_dir: &Path,
) -> Result<()> {
    let unshare = resources.helper_or_path("unshare")?;
    let id_map = IdMap::discover()?;
    let namespace_program = id_map.namespace_program(&unshare)?;
    let launcher = std::env::current_exe().context("locating launcher for OCI extraction")?;
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.namespace_args())
        .arg(launcher)
        .arg("__apply-image")
        .arg("--archive")
        .arg(archive)
        .arg("--expected-digest")
        .arg(expected_digest)
        .arg("--rootfs")
        .arg(rootfs)
        .arg("--work-dir")
        .arg(work_dir)
        .stdin(Stdio::null())
        .status()
        .with_context(|| {
            format!(
                "starting full-ID namespace with {}",
                namespace_program.display()
            )
        })?;
    if !status.success() {
        bail!(
            "applying the OCI image requires a configured subordinate UID/GID range; namespace helper exited with {status}"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OciIndex {
    schema_version: u32,
    manifests: Vec<OciDescriptor>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OciManifest {
    schema_version: u32,
    config: OciDescriptor,
    layers: Vec<OciDescriptor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct OciImageConfigSection {
    #[serde(default, rename = "Env")]
    environment: Vec<String>,
    #[serde(default, rename = "Labels")]
    labels: BTreeMap<String, String>,
    #[serde(default, rename = "WorkingDir")]
    working_dir: Option<String>,
    #[serde(default, rename = "User")]
    user: Option<String>,
    #[serde(default, rename = "Entrypoint")]
    entrypoint: Vec<String>,
    #[serde(default, rename = "Cmd")]
    command: Vec<String>,
    #[serde(default, rename = "StopSignal")]
    stop_signal: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct OciImageConfigDocument {
    architecture: String,
    os: String,
    #[serde(default)]
    config: OciImageConfigSection,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct OciDescriptor {
    digest: String,
    size: u64,
    media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    platform: Option<OciPlatform>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct OciPlatform {
    os: String,
    architecture: String,
}

fn oci_platform() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("linux/amd64"),
        "aarch64" => Ok("linux/arm64"),
        architecture => bail!("unsupported Buzzard OS architecture '{architecture}'"),
    }
}

fn validate_sha256_digest(digest: &str) -> Result<&str> {
    let hexadecimal = digest
        .strip_prefix("sha256:")
        .context("OCI digest does not use sha256")?;
    if hexadecimal.len() != 64 || !hexadecimal.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid OCI sha256 digest '{digest}'");
    }
    Ok(hexadecimal)
}

fn verified_blob_path(layout: &Path, descriptor: &OciDescriptor) -> Result<PathBuf> {
    let (path, _) = open_verified_blob(layout, descriptor)?;
    Ok(path)
}

fn open_verified_blob(layout: &Path, descriptor: &OciDescriptor) -> Result<(PathBuf, File)> {
    let hexadecimal = validate_sha256_digest(&descriptor.digest)?;
    let path = layout.join("blobs/sha256").join(hexadecimal);
    let mut file = open_regular_nofollow(&path, "OCI blob")?;
    let metadata = file.metadata()?;
    if metadata.len() != descriptor.size {
        bail!(
            "OCI blob {} has size {}, expected {}",
            descriptor.digest,
            metadata.len(),
            descriptor.size
        );
    }

    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)
        .with_context(|| format!("hashing OCI blob {}", descriptor.digest))?;
    let actual = format!("sha256:{:x}", hash.finalize());
    if actual != descriptor.digest {
        bail!(
            "OCI blob digest mismatch: expected {}, got {actual}",
            descriptor.digest
        );
    }
    file.rewind()
        .with_context(|| format!("rewinding OCI blob {}", descriptor.digest))?;
    Ok((path, file))
}

fn read_verified_blob(layout: &Path, descriptor: &OciDescriptor) -> Result<Vec<u8>> {
    if descriptor.size > MAX_OCI_METADATA_BYTES {
        bail!(
            "OCI metadata blob {} is {} bytes; maximum is {MAX_OCI_METADATA_BYTES}",
            descriptor.digest,
            descriptor.size
        );
    }
    let (path, mut file) = open_verified_blob(layout, descriptor)?;
    let mut bytes = Vec::with_capacity(descriptor.size as usize);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    let actual = format!("sha256:{:x}", Sha256::digest(&bytes));
    if actual != descriptor.digest {
        bail!(
            "OCI blob changed after verification: expected {}, got {actual}",
            descriptor.digest
        );
    }
    Ok(bytes)
}

fn host_oci_architecture() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        architecture => bail!("unsupported host architecture '{architecture}'"),
    }
}

fn descriptor_matches_host(descriptor: &OciDescriptor) -> Result<bool> {
    Ok(descriptor.platform.as_ref().is_none_or(|platform| {
        platform.os == "linux" && platform.architecture == host_oci_architecture().unwrap_or("")
    }))
}

fn select_oci_descriptor(
    descriptors: &[OciDescriptor],
    selector: Option<&str>,
) -> Result<OciDescriptor> {
    if descriptors.is_empty() {
        bail!("OCI image index contains no manifests");
    }
    let mut candidates = descriptors
        .iter()
        .filter(|descriptor| {
            selector.is_none_or(|selector| {
                descriptor.digest == selector
                    || descriptor
                        .annotations
                        .get("org.opencontainers.image.ref.name")
                        .is_some_and(|name| name == selector)
            })
        })
        .filter(|descriptor| descriptor_matches_host(descriptor).unwrap_or(false))
        .cloned()
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        let requested = selector
            .map(|value| format!(" matching '{value}'"))
            .unwrap_or_default();
        bail!(
            "OCI index has {} linux/{} manifests{requested}; select exactly one with --manifest DIGEST_OR_REF",
            candidates.len(),
            host_oci_architecture()?
        );
    }
    Ok(candidates.remove(0))
}

fn resolve_oci_manifest_descriptor(
    layout: &Path,
    index: &OciIndex,
    selector: Option<&str>,
) -> Result<OciDescriptor> {
    let descriptor = if let Some(selector) = selector {
        let mut matches = Vec::new();
        find_selected_oci_descriptors(layout, &index.manifests, selector, 0, &mut matches)?;
        if matches.len() != 1 {
            bail!(
                "OCI layout contains {} descriptors matching '{selector}'; select one unique digest or reference",
                matches.len()
            );
        }
        let descriptor = matches.remove(0);
        if !descriptor_matches_host(&descriptor)? {
            bail!(
                "selected OCI descriptor is not for linux/{}",
                host_oci_architecture()?
            );
        }
        descriptor
    } else {
        select_oci_descriptor(&index.manifests, None)?
    };
    if descriptor.media_type == "application/vnd.oci.image.index.v1+json"
        || descriptor.media_type == "application/vnd.docker.distribution.manifest.list.v2+json"
    {
        let bytes = read_verified_blob(layout, &descriptor)?;
        let nested: OciIndex =
            serde_json::from_slice(&bytes).context("parsing nested OCI image index")?;
        if nested.schema_version != 2 {
            bail!(
                "unsupported nested OCI index schema version {}",
                nested.schema_version
            );
        }
        return resolve_oci_manifest_descriptor(layout, &nested, None);
    }
    if descriptor.media_type != "application/vnd.oci.image.manifest.v1+json"
        && descriptor.media_type != "application/vnd.docker.distribution.manifest.v2+json"
    {
        bail!(
            "unsupported OCI descriptor media type {}",
            descriptor.media_type
        );
    }
    Ok(descriptor)
}

fn find_selected_oci_descriptors(
    layout: &Path,
    descriptors: &[OciDescriptor],
    selector: &str,
    depth: usize,
    matches: &mut Vec<OciDescriptor>,
) -> Result<()> {
    if depth > 8 {
        bail!("OCI index nesting exceeds the supported depth");
    }
    for descriptor in descriptors {
        if descriptor.digest == selector
            || descriptor
                .annotations
                .get(OCI_REF_NAME_ANNOTATION)
                .is_some_and(|name| name == selector)
        {
            matches.push(descriptor.clone());
        }
        if matches!(
            descriptor.media_type.as_str(),
            "application/vnd.oci.image.index.v1+json"
                | "application/vnd.docker.distribution.manifest.list.v2+json"
        ) {
            let bytes = read_verified_blob(layout, descriptor)?;
            let nested: OciIndex =
                serde_json::from_slice(&bytes).context("parsing nested OCI image index")?;
            if nested.schema_version != 2 {
                bail!(
                    "unsupported nested OCI index schema version {}",
                    nested.schema_version
                );
            }
            find_selected_oci_descriptors(layout, &nested.manifests, selector, depth + 1, matches)?;
        }
    }
    Ok(())
}

fn find_oci_manifest_descriptor(
    layout: &Path,
    index: &OciIndex,
    expected_digest: &str,
) -> Result<OciDescriptor> {
    fn visit(
        layout: &Path,
        descriptors: &[OciDescriptor],
        expected: &str,
        depth: usize,
    ) -> Result<Option<OciDescriptor>> {
        if depth > 8 {
            bail!("OCI index nesting exceeds the supported depth");
        }
        for descriptor in descriptors {
            if descriptor.digest == expected
                && matches!(
                    descriptor.media_type.as_str(),
                    "application/vnd.oci.image.manifest.v1+json"
                        | "application/vnd.docker.distribution.manifest.v2+json"
                )
            {
                return Ok(Some(descriptor.clone()));
            }
            if matches!(
                descriptor.media_type.as_str(),
                "application/vnd.oci.image.index.v1+json"
                    | "application/vnd.docker.distribution.manifest.list.v2+json"
            ) {
                let bytes = read_verified_blob(layout, descriptor)?;
                let nested: OciIndex =
                    serde_json::from_slice(&bytes).context("parsing nested OCI image index")?;
                if let Some(found) = visit(layout, &nested.manifests, expected, depth + 1)? {
                    return Ok(Some(found));
                }
            }
        }
        Ok(None)
    }

    visit(layout, &index.manifests, expected_digest, 0)?.with_context(|| {
        format!("OCI manifest digest {expected_digest} is not present in the image index")
    })
}

fn apply_image_archive(
    layout: &Path,
    expected_digest: &str,
    rootfs: &Path,
    work_dir: &Path,
) -> Result<()> {
    validate_guest_rootfs(rootfs)?;
    if fs::read_dir(rootfs)
        .context("inspecting new OCI rootfs")?
        .next()
        .is_some()
    {
        bail!("OCI image may only be applied to an empty new rootfs");
    }
    chown(rootfs, Some(Uid::from_raw(0)), Some(Gid::from_raw(0)))
        .with_context(|| format!("setting root ownership on {}", rootfs.display()))?;

    let index = read_oci_index(layout)?;
    let descriptor = find_oci_manifest_descriptor(layout, &index, expected_digest)?;
    if descriptor.media_type != "application/vnd.oci.image.manifest.v1+json"
        && descriptor.media_type != "application/vnd.docker.distribution.manifest.v2+json"
    {
        bail!(
            "unsupported OCI manifest media type {}",
            descriptor.media_type
        );
    }

    let manifest_bytes = read_verified_blob(layout, &descriptor)?;
    let manifest: OciManifest =
        serde_json::from_slice(&manifest_bytes).context("parsing OCI image manifest")?;
    if manifest.schema_version != 2 {
        bail!(
            "unsupported OCI manifest schema version {}",
            manifest.schema_version
        );
    }
    if manifest.layers.is_empty() {
        bail!("OCI image manifest contains no filesystem layers");
    }
    if manifest.config.media_type != "application/vnd.oci.image.config.v1+json"
        && manifest.config.media_type != "application/vnd.docker.container.image.v1+json"
    {
        bail!(
            "unsupported OCI image config media type {}",
            manifest.config.media_type
        );
    }
    // The config is not needed to construct the durable rootfs, but it is
    // content-addressed image material and must be authenticated along with
    // the manifest and every layer.
    verified_blob_path(layout, &manifest.config)?;

    for (index, descriptor) in manifest.layers.iter().enumerate() {
        if !matches!(
            descriptor.media_type.as_str(),
            "application/vnd.oci.image.layer.v1.tar"
                | "application/vnd.oci.image.layer.v1.tar+gzip"
                | "application/vnd.oci.image.layer.v1.tar+zstd"
                | "application/vnd.docker.image.rootfs.diff.tar"
                | "application/vnd.docker.image.rootfs.diff.tar.gzip"
        ) {
            bail!("unsupported OCI layer media type {}", descriptor.media_type);
        }
        let layer_tar = work_dir.join(format!("layer-{index}.tar"));
        decompress_verified_layer(layout, descriptor, &layer_tar)?;
        apply_layer(&layer_tar, rootfs)
            .with_context(|| format!("applying OCI layer {}", index + 1))?;
        fs::remove_file(&layer_tar).context("removing expanded OCI layer")?;
    }

    Ok(())
}

#[cfg(test)]
fn decompress_layer(source: &Path, destination: &Path) -> Result<()> {
    let input = open_regular_nofollow(source, "OCI layer")?;
    decompress_layer_file(input, destination)
}

fn decompress_verified_layer(
    layout: &Path,
    descriptor: &OciDescriptor,
    destination: &Path,
) -> Result<()> {
    let hexadecimal = validate_sha256_digest(&descriptor.digest)?;
    let path = layout.join("blobs/sha256").join(hexadecimal);
    let mut input = open_regular_nofollow(&path, "OCI layer")?;
    let metadata = input.metadata()?;
    if metadata.len() != descriptor.size {
        bail!(
            "OCI blob {} has size {}, expected {}",
            descriptor.digest,
            metadata.len(),
            descriptor.size
        );
    }
    let mut magic = [0_u8; 4];
    let read = input.read(&mut magic).context("reading OCI layer header")?;
    input.rewind().context("rewinding OCI layer")?;
    let input = HashingReader::new(BufReader::new(input));
    let mut output =
        File::create(destination).with_context(|| format!("creating {}", destination.display()))?;

    let (_input, compressed_size, compressed_hash) = if read >= 2 && magic[..2] == [0x1f, 0x8b] {
        let mut decoder = GzDecoder::new(input);
        std::io::copy(&mut decoder, &mut output).context("decompressing gzip OCI layer")?;
        let mut input = decoder.into_inner();
        std::io::copy(&mut input, &mut std::io::sink())
            .context("finishing gzip OCI layer verification")?;
        input.finish()
    } else if read == 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        let mut decoder = zstd::stream::read::Decoder::new(input)
            .context("initializing zstd OCI layer decoder")?;
        std::io::copy(&mut decoder, &mut output).context("decompressing zstd OCI layer")?;
        let mut input = decoder.finish();
        std::io::copy(&mut input, &mut std::io::sink())
            .context("finishing zstd OCI layer verification")?;
        input.into_inner().finish()
    } else {
        let mut input = input;
        std::io::copy(&mut input, &mut output).context("copying uncompressed OCI layer")?;
        input.finish()
    };
    let actual_digest = format!("sha256:{compressed_hash}");
    if compressed_size != descriptor.size || actual_digest != descriptor.digest {
        let _ = fs::remove_file(destination);
        bail!(
            "OCI layer changed while it was being read: expected {} bytes at {}, got {compressed_size} bytes at {actual_digest}",
            descriptor.size,
            descriptor.digest
        );
    }
    output.sync_all().context("syncing expanded OCI layer")?;
    Ok(())
}

#[cfg(test)]
fn decompress_layer_file(input: File, destination: &Path) -> Result<()> {
    let mut input = BufReader::new(input);
    let mut magic = [0_u8; 4];
    let read = input.read(&mut magic).context("reading OCI layer header")?;
    input.rewind().context("rewinding OCI layer")?;
    let mut output =
        File::create(destination).with_context(|| format!("creating {}", destination.display()))?;

    if read >= 2 && magic[..2] == [0x1f, 0x8b] {
        std::io::copy(&mut GzDecoder::new(input), &mut output)
            .context("decompressing gzip OCI layer")?;
    } else if read == 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        let mut decoder = zstd::stream::read::Decoder::new(input)
            .context("initializing zstd OCI layer decoder")?;
        std::io::copy(&mut decoder, &mut output).context("decompressing zstd OCI layer")?;
    } else {
        std::io::copy(&mut input, &mut output).context("copying uncompressed OCI layer")?;
    }
    output.sync_all().context("syncing expanded OCI layer")?;
    Ok(())
}

type PaxRecords = BTreeMap<Vec<u8>, Vec<u8>>;

fn parse_raw_pax_records(contents: &[u8]) -> Result<PaxRecords> {
    let mut records = BTreeMap::new();
    let mut position = 0_usize;
    while position < contents.len() {
        let relative_space = contents[position..]
            .iter()
            .position(|byte| *byte == b' ')
            .context("PAX record has no length separator")?;
        let space = position + relative_space;
        let length_bytes = &contents[position..space];
        if length_bytes.is_empty() || !length_bytes.iter().all(u8::is_ascii_digit) {
            bail!("PAX record has an invalid decimal length");
        }
        let length_text = std::str::from_utf8(length_bytes).context("PAX length is not ASCII")?;
        let length: usize = length_text.parse().context("PAX record length overflow")?;
        let end = position
            .checked_add(length)
            .context("PAX record length overflow")?;
        if end > contents.len() || end <= space + 2 || contents[end - 1] != b'\n' {
            bail!("PAX record length does not match its payload");
        }
        let record = &contents[space + 1..end - 1];
        let equals = record
            .iter()
            .position(|byte| *byte == b'=')
            .context("PAX record has no key/value separator")?;
        let key = &record[..equals];
        if key.is_empty() || key.contains(&0) {
            bail!("PAX record has an invalid key");
        }
        records.insert(key.to_vec(), record[equals + 1..].to_vec());
        position = end;
    }
    Ok(records)
}

fn read_layer_pax_records(layer_tar: &Path) -> Result<BTreeMap<u64, PaxRecords>> {
    let file = File::open(layer_tar).context("opening OCI layer for raw PAX scan")?;
    let mut archive = tar::Archive::new(BufReader::new(file));
    let mut entries = archive
        .entries()
        .context("reading raw OCI layer entries")?
        .raw(true);
    let mut global = PaxRecords::new();
    let mut local: Option<PaxRecords> = None;
    let mut records = BTreeMap::new();
    for item in &mut entries {
        let mut entry = item.context("reading raw OCI layer entry")?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_pax_global_extensions() || entry_type.is_pax_local_extensions() {
            let size = entry.header().size().context("reading PAX entry size")?;
            if size > MAX_OCI_PAX_BYTES {
                bail!("OCI PAX metadata exceeds {MAX_OCI_PAX_BYTES} bytes");
            }
            let mut contents = Vec::with_capacity(size as usize);
            entry
                .read_to_end(&mut contents)
                .context("reading OCI PAX metadata")?;
            if contents.len() as u64 != size {
                bail!("OCI PAX metadata ended before its declared size");
            }
            let parsed = parse_raw_pax_records(&contents)?;
            if entry_type.is_pax_global_extensions() {
                global.extend(parsed);
            } else if local.replace(parsed).is_some() {
                bail!("two local PAX headers describe one OCI layer entry");
            }
            continue;
        }
        if entry_type.is_gnu_longname() || entry_type.is_gnu_longlink() {
            continue;
        }
        if !global.is_empty() || local.is_some() {
            let mut combined = global.clone();
            if let Some(local) = local.take() {
                combined.extend(local);
            }
            records.insert(entry.raw_header_position(), combined);
        }
    }
    if local.is_some() {
        bail!("OCI layer ends with an unconsumed local PAX header");
    }
    Ok(records)
}

fn apply_layer(layer_tar: &Path, rootfs: &Path) -> Result<()> {
    // OCI whiteouts affect content inherited from lower layers. Apply them
    // before unpacking this layer so archive entry ordering cannot change
    // overlay semantics.
    let file = File::open(layer_tar).context("opening OCI layer for whiteout scan")?;
    let mut archive = tar::Archive::new(BufReader::new(file));
    for item in archive.entries().context("reading OCI layer whiteouts")? {
        let entry = item.context("reading OCI whiteout entry")?;
        let entry_path = entry
            .path()
            .context("reading OCI whiteout path")?
            .into_owned();
        let path = safe_relative_path(&entry_path)?;
        if let Some(action) = whiteout_action(path)? {
            match action {
                Whiteout::Remove(target) => remove_whiteout_target(rootfs, &target)?,
                Whiteout::Opaque(parent) => clear_whiteout_directory(rootfs, parent)?,
            }
        }
    }

    let pax_records = read_layer_pax_records(layer_tar)?;
    let file = File::open(layer_tar).context("opening OCI layer for extraction")?;
    let mut archive = tar::Archive::new(BufReader::new(file));
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_preserve_ownerships(true);
    archive.set_unpack_xattrs(false);
    let mut directories = Vec::new();
    for item in archive.entries().context("reading OCI layer")? {
        let mut entry = item.context("reading OCI layer entry")?;
        let pax = pax_records
            .get(&entry.raw_header_position())
            .cloned()
            .unwrap_or_default();
        let entry_path = if let Some(path) = pax.get(b"path".as_slice()) {
            PathBuf::from(OsStr::from_bytes(path))
        } else {
            entry.path().context("reading OCI layer path")?.into_owned()
        };
        let path = safe_relative_path(&entry_path)?;
        if whiteout_action(path)?.is_some() {
            continue;
        }

        let entry_type = entry.header().entry_type();
        if !matches!(
            entry_type,
            tar::EntryType::Regular
                | tar::EntryType::Continuous
                | tar::EntryType::GNUSparse
                | tar::EntryType::Directory
                | tar::EntryType::Symlink
                | tar::EntryType::Link
        ) {
            bail!(
                "OCI layer contains unsupported special entry {} ({entry_type:?})",
                path.display()
            );
        }
        let uid = entry.header().uid().context("reading OCI layer UID")?;
        let gid = entry.header().gid().context("reading OCI layer GID")?;
        if uid > MAX_GUEST_ID || gid > MAX_GUEST_ID {
            bail!(
                "OCI layer entry {} uses unsupported guest ownership {uid}:{gid}; maximum is {MAX_GUEST_ID}",
                path.display()
            );
        }
        if entry_type == tar::EntryType::Link {
            let target = if let Some(target) = pax.get(b"linkpath".as_slice()) {
                PathBuf::from(OsStr::from_bytes(target))
            } else {
                entry
                    .link_name()
                    .context("reading OCI hardlink target")?
                    .context("OCI hardlink has no target")?
                    .into_owned()
            };
            let target = safe_relative_path(&target)?;
            if target.as_os_str().is_empty() {
                bail!("OCI hardlink target cannot be empty");
            }
        }

        let mode = entry.header().mode().context("reading OCI layer mode")?;
        let mut mtime = (
            i64::try_from(entry.header().mtime().context("reading OCI layer mtime")?)
                .context("OCI layer mtime is outside the supported range")?,
            0_i64,
        );
        let mut extended_attributes = Vec::new();
        for (key, value) in &pax {
            if key == b"mtime" {
                mtime = parse_pax_timestamp(value)?;
            } else if let Some(name) = key.strip_prefix(b"SCHILY.xattr.") {
                if name.is_empty() {
                    bail!("OCI layer contains an empty xattr name");
                }
                extended_attributes.push((name.to_vec(), value.clone()));
            }
        }

        if entry_type == tar::EntryType::Directory {
            if !path.as_os_str().is_empty()
                && !entry
                    .unpack_in(rootfs)
                    .with_context(|| format!("extracting {}", path.display()))?
            {
                bail!("OCI layer entry {} escaped the destination", path.display());
            }
            directories.push(DeferredRootfsDirectory {
                relative: path.to_path_buf(),
                uid: uid as u32,
                gid: gid as u32,
                mode,
                mtime,
                xattrs: extended_attributes,
            });
            continue;
        }
        if path.as_os_str().is_empty() {
            bail!("OCI layer contains a non-directory root entry");
        }
        if !entry
            .unpack_in(rootfs)
            .with_context(|| format!("extracting {}", path.display()))?
        {
            bail!("OCI layer entry {} escaped the destination", path.display());
        }
        if entry_type != tar::EntryType::Link {
            let destination = rootfs.join(path);
            for (name, value) in extended_attributes {
                set_link_xattr(&destination, &name, &value)?;
            }
            set_link_mtime(&destination, mtime)?;
        }
    }
    directories.sort_by_key(|directory| std::cmp::Reverse(directory.relative.components().count()));
    for directory in directories {
        apply_deferred_rootfs_directory(rootfs, directory)?;
    }
    Ok(())
}

fn set_link_xattr(path: &Path, name: &[u8], value: &[u8]) -> Result<()> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .context("OCI xattr path contains a NUL byte")?;
    let name = std::ffi::CString::new(name).context("OCI xattr name contains a NUL byte")?;
    let result = unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("applying OCI extended attribute")
    }
}

enum Whiteout<'a> {
    Remove(PathBuf),
    Opaque(&'a Path),
}

fn whiteout_action(path: &Path) -> Result<Option<Whiteout<'_>>> {
    let Some(name) = path.file_name().map(OsStr::as_bytes) else {
        return Ok(None);
    };
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    if name == b".wh..wh..opq" {
        return Ok(Some(Whiteout::Opaque(parent)));
    }
    let Some(target) = name.strip_prefix(b".wh.") else {
        return Ok(None);
    };
    if target.is_empty() {
        bail!("OCI whiteout has no target at {}", path.display());
    }
    Ok(Some(Whiteout::Remove(
        parent.join(OsStr::from_bytes(target)),
    )))
}

fn safe_relative_path(path: &Path) -> Result<&Path> {
    if path == Path::new(".") {
        return Ok(Path::new(""));
    }
    if path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        bail!("OCI archive contains unsafe path {}", path.display());
    }
    Ok(path)
}

fn remove_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path)
                .with_context(|| format!("applying whiteout {}", path.display()))
        }
        Ok(_) => {
            fs::remove_file(path).with_context(|| format!("applying whiteout {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting whiteout {}", path.display())),
    }
}

fn validated_whiteout_path(rootfs: &Path, relative: &Path) -> Result<Option<PathBuf>> {
    let rootfs = rootfs
        .canonicalize()
        .with_context(|| format!("resolving rootfs {}", rootfs.display()))?;
    let path = rootfs.join(relative);
    let parent = path.parent().context("whiteout target has no parent")?;
    let resolved_parent = match parent.canonicalize() {
        Ok(parent) => parent,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("resolving whiteout parent {}", parent.display()));
        }
    };
    if !resolved_parent.starts_with(&rootfs) {
        bail!(
            "OCI whiteout target {} escapes the rootfs through a symlink",
            relative.display()
        );
    }
    Ok(Some(path))
}

fn remove_whiteout_target(rootfs: &Path, relative: &Path) -> Result<()> {
    if let Some(path) = validated_whiteout_path(rootfs, relative)? {
        remove_path(&path)?;
    }
    Ok(())
}

fn clear_whiteout_directory(rootfs: &Path, relative: &Path) -> Result<()> {
    let Some(path) = validated_whiteout_path(rootfs, relative)? else {
        return Ok(());
    };
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            clear_directory(&path)
        }
        Ok(_) => bail!(
            "OCI opaque whiteout target {} is not a directory",
            relative.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspecting opaque whiteout {}", relative.display()))
        }
    }
}

fn clear_directory(path: &Path) -> Result<()> {
    match fs::read_dir(path) {
        Ok(entries) => {
            for entry in entries {
                remove_path(&entry.context("reading opaque directory")?.path())?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("applying opaque whiteout {}", path.display()))
        }
    }
}

fn install_guest_assets(rootfs: &Path) -> Result<()> {
    validate_guest_rootfs(rootfs)?;
    remove_empty_nvidia_mount_placeholders(rootfs)?;
    install_bundled_guest_runtime_for_new_rootfs(rootfs)?;
    for (relative, contents, mode) in GUEST_ASSETS {
        install_guest_asset(rootfs, Path::new(relative), contents, *mode)?;
    }
    remove_kde_wallet_activation(rootfs)?;
    install_guest_asset_manifest(rootfs, &current_guest_asset_manifest())?;
    install_guest_asset(
        rootfs,
        Path::new("usr/lib/wildbuzzard/guest-assets.version"),
        GUEST_ASSETS_REVISION.as_bytes(),
        0o644,
    )
}

#[cfg(test)]
fn install_guest_assets_without_shell(rootfs: &Path) -> Result<()> {
    validate_guest_rootfs(rootfs)?;
    remove_empty_nvidia_mount_placeholders(rootfs)?;
    for (relative, contents, mode) in GUEST_ASSETS {
        install_guest_asset(rootfs, Path::new(relative), contents, *mode)?;
    }
    remove_kde_wallet_activation(rootfs)?;
    install_guest_asset(
        rootfs,
        Path::new("usr/lib/wildbuzzard/guest-assets.version"),
        GUEST_ASSETS_REVISION.as_bytes(),
        0o644,
    )
}

fn validate_guest_rootfs(rootfs: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(rootfs)
        .with_context(|| format!("inspecting guest rootfs {}", rootfs.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("guest rootfs must be a real directory");
    }
    Ok(())
}

fn guest_runtime_revision() -> Result<String> {
    let revision = GUEST_ASSETS_REVISION.trim();
    if !valid_runtime_revision(revision) {
        bail!("invalid protected guest runtime revision {revision:?}");
    }
    Ok(revision.to_owned())
}

fn valid_runtime_revision(revision: &str) -> bool {
    let mut characters = revision.chars();
    !revision.is_empty()
        && revision.len() <= 128
        && characters
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '+' | '~' | '-')
        })
}

fn bundled_guest_runtime_dir(revision: &str) -> Result<PathBuf> {
    let launcher = std::env::current_exe().context("locating bundled guest runtime")?;
    Ok(launcher
        .parent()
        .context("launcher path has no parent")?
        .join(BUNDLED_GUEST_RUNTIME)
        .join(revision))
}

fn protected_runtime_metadata(
    path: &Path,
    expected_uid: Option<u32>,
    kind: &str,
) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {kind} {}", path.display()))?;
    if let Some(expected_uid) = expected_uid {
        let expected_gid = if expected_uid == 0 {
            0
        } else {
            unsafe { libc::getegid() }
        };
        if metadata.uid() != expected_uid || metadata.gid() != expected_gid {
            bail!(
                "{kind} {} is owned by {}:{}, expected {expected_uid}:{expected_gid}",
                path.display(),
                metadata.uid(),
                metadata.gid()
            );
        }
    }
    if !metadata.file_type().is_symlink() && metadata.permissions().mode() & 0o022 != 0 {
        bail!("{kind} {} is group/world writable", path.display());
    }
    Ok(metadata)
}

fn valid_runtime_relative_path(relative: &str) -> bool {
    !relative.is_empty()
        && !relative.starts_with('/')
        && relative.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'+' | b'/' | b'@' | b'~' | b'-')
        })
        && Path::new(relative)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn read_guest_runtime_manifest(
    revision_dir: &Path,
    expected_uid: Option<u32>,
    expected_revision: &str,
) -> Result<(GuestRuntimeManifest, Vec<u8>)> {
    let manifest_path = revision_dir.join("runtime.manifest.json");
    let metadata = protected_runtime_metadata(&manifest_path, expected_uid, "runtime manifest")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "protected runtime manifest {} is not a regular file",
            manifest_path.display()
        );
    }
    if metadata.len() > MAX_GUEST_RUNTIME_MANIFEST_BYTES {
        bail!("protected runtime manifest exceeds its size limit");
    }
    let bytes = fs::read(&manifest_path)
        .with_context(|| format!("reading runtime manifest {}", manifest_path.display()))?;
    let manifest: GuestRuntimeManifest =
        serde_json::from_slice(&bytes).context("parsing protected runtime manifest")?;
    if manifest.schema_version != 1 {
        bail!("unsupported protected runtime manifest schema");
    }
    if manifest.revision != expected_revision {
        bail!("protected runtime manifest revision does not match its directory");
    }
    if manifest.files.is_empty() || manifest.files.len() > 4096 {
        bail!("protected runtime manifest has an invalid file inventory");
    }
    for required in REQUIRED_GUEST_RUNTIME_FILES {
        if !manifest.files.contains_key(*required) {
            bail!("protected runtime is missing required file {required}");
        }
    }
    for (relative, record) in &manifest.files {
        if !valid_runtime_relative_path(relative)
            || record.mode > 0o777
            || record.mode & 0o022 != 0
            || record.sha256.len() != 64
            || !record
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("protected runtime manifest contains an unsafe record");
        }
    }
    Ok((manifest, bytes))
}

fn sha256_regular_file(path: &Path) -> Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("opening protected runtime file {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("reading protected runtime file {}", path.display()))?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn validate_guest_runtime_tree(
    revision_dir: &Path,
    manifest: &GuestRuntimeManifest,
    expected_uid: Option<u32>,
    allow_readiness: bool,
) -> Result<()> {
    let revision_metadata =
        protected_runtime_metadata(revision_dir, expected_uid, "runtime revision")?;
    if revision_metadata.file_type().is_symlink() || !revision_metadata.is_dir() {
        bail!("protected runtime revision must be a real directory");
    }
    let canonical_revision = revision_dir
        .canonicalize()
        .with_context(|| format!("resolving runtime revision {}", revision_dir.display()))?;
    let mut seen = BTreeMap::new();
    let mut directories = vec![(revision_dir.to_path_buf(), PathBuf::new())];
    while let Some((directory, relative_directory)) = directories.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("reading runtime directory {}", directory.display()))?
        {
            let entry = entry.context("reading protected runtime entry")?;
            let file_name = entry.file_name();
            let relative = relative_directory.join(&file_name);
            let path = entry.path();
            let relative_text = relative
                .to_str()
                .context("protected runtime path is not UTF-8")?
                .to_owned();
            let metadata =
                protected_runtime_metadata(&path, expected_uid, "runtime path component")?;
            if metadata.file_type().is_symlink() {
                bail!("protected runtime contains a symbolic link: {relative_text}");
            }
            if metadata.is_dir() {
                directories.push((path, relative));
                continue;
            }
            if !metadata.is_file() {
                bail!("protected runtime contains a special file: {relative_text}");
            }
            if relative_text == "runtime.manifest.json"
                || (allow_readiness && relative_text == "readiness.json")
            {
                continue;
            }
            let record = manifest.files.get(&relative_text).with_context(|| {
                format!("protected runtime contains unmanifested file {relative_text}")
            })?;
            if metadata.permissions().mode() & 0o777 != record.mode {
                bail!("protected runtime mode differs for {relative_text}");
            }
            if sha256_regular_file(&path)? != record.sha256 {
                bail!("protected runtime digest differs for {relative_text}");
            }
            let resolved = path
                .canonicalize()
                .with_context(|| format!("resolving runtime file {}", path.display()))?;
            if !resolved.starts_with(&canonical_revision) {
                bail!("protected runtime file escaped its revision: {relative_text}");
            }
            seen.insert(relative_text, ());
        }
    }
    if seen.len() != manifest.files.len()
        || manifest
            .files
            .keys()
            .any(|relative| !seen.contains_key(relative))
    {
        bail!("protected runtime manifest names missing files");
    }
    Ok(())
}

fn read_revision_link(
    runtime_root: &Path,
    name: &str,
    expected_uid: u32,
) -> Result<Option<String>> {
    let path = runtime_root.join(name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspecting runtime link {}", path.display()));
        }
    };
    let expected_gid = if expected_uid == 0 {
        0
    } else {
        unsafe { libc::getegid() }
    };
    if !metadata.file_type().is_symlink()
        || metadata.uid() != expected_uid
        || metadata.gid() != expected_gid
    {
        bail!("protected runtime {name} is not an owner-controlled symbolic link");
    }
    let target = fs::read_link(&path)
        .with_context(|| format!("reading protected runtime link {}", path.display()))?;
    let target = target
        .to_str()
        .context("protected runtime link target is not UTF-8")?;
    if !valid_runtime_revision(target) {
        bail!("protected runtime {name} has an unsafe target");
    }
    let revision = runtime_root.join(target);
    let revision_metadata =
        protected_runtime_metadata(&revision, Some(expected_uid), "runtime revision")?;
    if revision_metadata.file_type().is_symlink() || !revision_metadata.is_dir() {
        bail!("protected runtime {name} target is not a real revision directory");
    }
    Ok(Some(target.to_owned()))
}

fn create_runtime_stage(runtime_root: &Path, revision: &str) -> Result<PathBuf> {
    for counter in 0..128_u32 {
        let stage = runtime_root.join(format!(
            ".{revision}.staging.{}.{}",
            std::process::id(),
            counter
        ));
        match fs::create_dir(&stage) {
            Ok(()) => {
                fs::set_permissions(&stage, fs::Permissions::from_mode(0o755))
                    .with_context(|| format!("protecting runtime stage {}", stage.display()))?;
                return Ok(stage);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating runtime stage {}", stage.display()));
            }
        }
    }
    bail!("could not allocate a protected runtime staging directory")
}

fn clean_stale_runtime_intermediates(
    runtime_root: &Path,
    revision: &str,
    expected_uid: u32,
) -> Result<()> {
    let staging_prefix = format!(".{revision}.staging.");
    let incomplete_prefix = format!(".{revision}.incomplete.");
    for entry in fs::read_dir(runtime_root)
        .with_context(|| format!("reading protected runtime root {}", runtime_root.display()))?
    {
        let entry = entry.context("reading protected runtime intermediate")?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(&staging_prefix) && !name.starts_with(&incomplete_prefix) {
            continue;
        }
        let path = entry.path();
        let metadata = protected_runtime_metadata(
            &path,
            Some(expected_uid),
            "runtime migration intermediate",
        )?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "runtime migration intermediate {} is not a real directory",
                path.display()
            );
        }
        safely_remove_runtime_directory(&path)?;
    }
    Ok(())
}

fn copy_guest_runtime_revision(
    source: &Path,
    stage: &Path,
    manifest: &GuestRuntimeManifest,
    manifest_bytes: &[u8],
) -> Result<()> {
    for (relative, record) in &manifest.files {
        let source_file = source.join(relative);
        let destination = prepare_guest_asset_destination(stage, Path::new(relative))?;
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&source_file)
            .with_context(|| format!("opening bundled runtime file {}", source_file.display()))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(record.mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&destination)
            .with_context(|| format!("creating runtime file {}", destination.display()))?;
        std::io::copy(&mut input, &mut output)
            .with_context(|| format!("copying protected runtime file {relative}"))?;
        output
            .set_permissions(fs::Permissions::from_mode(record.mode))
            .with_context(|| format!("setting runtime mode for {relative}"))?;
        output
            .sync_all()
            .with_context(|| format!("syncing runtime file {relative}"))?;
    }
    install_guest_asset(
        stage,
        Path::new("runtime.manifest.json"),
        manifest_bytes,
        0o644,
    )
}

fn rename_noreplace(source: &Path, destination: &Path) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .context("runtime source path contains an embedded NUL")?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .context("runtime destination path contains an embedded NUL")?;
    // Linux is the only supported host. RENAME_NOREPLACE prevents a raced or
    // hostile path from being replaced during the revision commit.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "atomically moving {} to {}",
                source.to_string_lossy(),
                destination.to_string_lossy()
            )
        })
    }
}

fn vacant_runtime_path(runtime_root: &Path, stem: &str) -> Result<PathBuf> {
    for counter in 0..128_u32 {
        let candidate = runtime_root.join(format!(".{stem}.{}.{}", std::process::id(), counter));
        match fs::symlink_metadata(&candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting runtime path {}", candidate.display()));
            }
            Ok(_) => continue,
        }
    }
    bail!("could not allocate an atomic protected runtime path")
}

fn atomic_revision_link(
    runtime_root: &Path,
    name: &str,
    revision: &str,
    expected_uid: u32,
) -> Result<()> {
    let expected_gid = if expected_uid == 0 {
        0
    } else {
        unsafe { libc::getegid() }
    };
    if let Ok(metadata) = fs::symlink_metadata(runtime_root.join(name))
        && (!metadata.file_type().is_symlink()
            || metadata.uid() != expected_uid
            || metadata.gid() != expected_gid)
    {
        bail!("protected runtime {name} cannot be replaced safely");
    }
    for counter in 0..128_u32 {
        let temporary =
            runtime_root.join(format!(".{name}.link.{}.{}", std::process::id(), counter));
        match symlink(revision, &temporary) {
            Ok(()) => {
                fs::rename(&temporary, runtime_root.join(name)).with_context(|| {
                    format!("activating protected runtime {name} revision {revision}")
                })?;
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating protected runtime {name} revision link"));
            }
        }
    }
    bail!("could not allocate a protected runtime activation link")
}

fn safely_remove_runtime_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))
        }
        Ok(_) => bail!(
            "refusing to remove non-directory runtime path {}",
            path.display()
        ),
    }
}

fn install_protected_guest_runtime(
    rootfs: &Path,
    source: &Path,
    revision: &str,
    expected_uid: u32,
) -> Result<()> {
    let (manifest, manifest_bytes) = read_guest_runtime_manifest(source, None, revision)?;
    if manifest.revision != revision {
        bail!("bundled runtime revision differs from ASSET_REVISION");
    }
    validate_guest_runtime_tree(source, &manifest, None, false)?;

    let current_path =
        prepare_guest_asset_destination(rootfs, &Path::new(GUEST_RUNTIME_ROOT).join("current"))?;
    let runtime_root = current_path
        .parent()
        .context("protected runtime current path has no parent")?;
    for protected in [
        rootfs.join("opt"),
        rootfs.join("opt/wildbuzzard"),
        runtime_root.to_path_buf(),
    ] {
        let metadata =
            protected_runtime_metadata(&protected, Some(expected_uid), "runtime directory")?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "protected runtime path {} is not a real directory",
                protected.display()
            );
        }
        fs::set_permissions(&protected, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("protecting runtime directory {}", protected.display()))?;
    }
    let current = read_revision_link(runtime_root, "current", expected_uid)?;
    if fs::symlink_metadata(runtime_root.join("previous")).is_ok() {
        let _ = read_revision_link(runtime_root, "previous", expected_uid)?;
    }

    clean_stale_runtime_intermediates(runtime_root, revision, expected_uid)?;
    let stage = create_runtime_stage(runtime_root, revision)?;
    let install_result = (|| -> Result<()> {
        copy_guest_runtime_revision(source, &stage, &manifest, &manifest_bytes)?;
        validate_guest_runtime_tree(&stage, &manifest, Some(expected_uid), false)?;
        let destination = runtime_root.join(revision);
        match fs::symlink_metadata(&destination) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                rename_noreplace(&stage, &destination)?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspecting installed runtime revision {}",
                        destination.display()
                    )
                });
            }
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                let existing =
                    read_guest_runtime_manifest(&destination, Some(expected_uid), revision)
                        .and_then(|(existing, _)| {
                            if existing != manifest {
                                bail!("installed protected runtime manifest differs")
                            }
                            validate_guest_runtime_tree(
                                &destination,
                                &existing,
                                Some(expected_uid),
                                true,
                            )
                        });
                if let Err(existing_error) = existing {
                    if current.as_deref() == Some(revision) {
                        return Err(existing_error).context(
                            "active protected runtime is incomplete; bump ASSET_REVISION",
                        );
                    }
                    let incomplete =
                        vacant_runtime_path(runtime_root, &format!("{revision}.incomplete"))?;
                    rename_noreplace(&destination, &incomplete)?;
                    if let Err(error) = rename_noreplace(&stage, &destination) {
                        let _ = rename_noreplace(&incomplete, &destination);
                        return Err(error);
                    }
                    safely_remove_runtime_directory(&incomplete)?;
                } else {
                    safely_remove_runtime_directory(&stage)?;
                }
            }
            Ok(_) => bail!("installed protected runtime revision is not a real directory"),
        }

        if current.as_deref() != Some(revision) {
            if let Some(previous) = current.as_deref() {
                atomic_revision_link(runtime_root, "previous", previous, expected_uid)?;
            }
            atomic_revision_link(runtime_root, "current", revision, expected_uid)?;
        }
        File::open(runtime_root)
            .and_then(|directory| directory.sync_all())
            .with_context(|| {
                format!("syncing protected runtime root {}", runtime_root.display())
            })?;
        Ok(())
    })();
    if install_result.is_err() {
        let _ = safely_remove_runtime_directory(&stage);
    }
    install_result
}

fn install_bundled_guest_runtime(rootfs: &Path) -> Result<()> {
    let revision = guest_runtime_revision()?;
    let source = bundled_guest_runtime_dir(&revision)?;
    install_protected_guest_runtime(rootfs, &source, &revision, 0)
}

fn install_protected_guest_runtime_for_new_rootfs(
    rootfs: &Path,
    source: &Path,
    revision: &str,
    expected_uid: u32,
) -> Result<()> {
    let (bundled, _) = read_guest_runtime_manifest(source, None, revision)?;
    validate_guest_runtime_tree(source, &bundled, None, false)?;

    let runtime_root = rootfs.join(GUEST_RUNTIME_ROOT);
    let current = read_revision_link(&runtime_root, "current", expected_uid)?;
    let destination = runtime_root.join(revision);
    let replace_staged_revision = current.as_deref() == Some(revision)
        && read_guest_runtime_manifest(&destination, Some(expected_uid), revision)
            .and_then(|(installed, _)| {
                if installed != bundled {
                    bail!("staged protected runtime manifest differs")
                }
                validate_guest_runtime_tree(&destination, &installed, Some(expected_uid), true)
            })
            .is_err();

    if !replace_staged_revision {
        return install_protected_guest_runtime(rootfs, source, revision, expected_uid);
    }

    // This path is used only while creating a new, uncommitted machine rootfs.
    // The OCI and AppImage builders may produce byte-distinct executables from
    // the same source/toolchain contract. Preserve the strict no-replacement
    // rule for an existing machine, but reconcile the disposable staging tree
    // to the exact runtime carried by the AppImage before it becomes visible.
    let original_revision = (0..128_u32)
        .map(|counter| format!("seed~{}~{counter}", std::process::id()))
        .find(|candidate| fs::symlink_metadata(runtime_root.join(candidate)).is_err())
        .context("allocating a staged OCI runtime revision name")?;
    let original = runtime_root.join(&original_revision);
    rename_noreplace(&destination, &original)
        .context("preserving the staged OCI runtime before seed reconciliation")?;
    atomic_revision_link(&runtime_root, "current", &original_revision, expected_uid)?;
    match install_protected_guest_runtime(rootfs, source, revision, expected_uid) {
        Ok(()) => {
            if fs::read_link(runtime_root.join("previous")).ok().as_deref()
                == Some(Path::new(&original_revision))
            {
                fs::remove_file(runtime_root.join("previous"))
                    .context("removing transient seed runtime history")?;
            }
            safely_remove_runtime_directory(&original)?;
            File::open(&runtime_root)
                .and_then(|directory| directory.sync_all())
                .context("syncing reconciled new-machine runtime")?;
            Ok(())
        }
        Err(error) => {
            let _ = safely_remove_runtime_directory(&destination);
            let restore = rename_noreplace(&original, &destination);
            let current_restore =
                atomic_revision_link(&runtime_root, "current", revision, expected_uid);
            if fs::read_link(runtime_root.join("previous")).ok().as_deref()
                == Some(Path::new(&original_revision))
            {
                let _ = fs::remove_file(runtime_root.join("previous"));
            }
            match restore {
                Ok(()) if current_restore.is_ok() => Err(error).context(
                    "installing the AppImage runtime into the new-machine staging rootfs",
                ),
                Ok(()) => Err(error).context(format!(
                    "installing the AppImage runtime into the new-machine staging rootfs; restoring its active revision link also failed: {:#}",
                    current_restore.unwrap_err()
                )),
                Err(restore_error) => Err(error).context(format!(
                    "installing the AppImage runtime into the new-machine staging rootfs; restoring the OCI runtime also failed: {restore_error:#}"
                )),
            }
        }
    }
}

fn install_bundled_guest_runtime_for_new_rootfs(rootfs: &Path) -> Result<()> {
    let revision = guest_runtime_revision()?;
    let source = bundled_guest_runtime_dir(&revision)?;
    install_protected_guest_runtime_for_new_rootfs(rootfs, &source, &revision, 0)
}

fn canonical_runtime_manifest_digest(manifest_bytes: &[u8]) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_slice(manifest_bytes).context("parsing runtime manifest for readiness")?;
    let canonical = serde_json::to_vec(&value).context("canonicalizing runtime manifest")?;
    Ok(format!("{:x}", Sha256::digest(&canonical)))
}

fn validate_runtime_readiness(
    revision_dir: &Path,
    revision: &str,
    manifest_bytes: &[u8],
    expected_uid: u32,
) -> Result<()> {
    let path = revision_dir.join("readiness.json");
    let metadata = protected_runtime_metadata(&path, Some(expected_uid), "runtime readiness")?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_GUEST_RUNTIME_MANIFEST_BYTES
    {
        bail!("protected runtime readiness is not a bounded regular file");
    }
    let bytes =
        fs::read(&path).with_context(|| format!("reading runtime readiness {}", path.display()))?;
    let readiness: GuestRuntimeReadiness =
        serde_json::from_slice(&bytes).context("parsing protected runtime readiness")?;
    if readiness.schema_version != 1
        || readiness.revision != revision
        || !readiness.ready
        || readiness.manifest_sha256 != canonical_runtime_manifest_digest(manifest_bytes)?
    {
        bail!("protected runtime readiness does not bind the complete revision");
    }
    Ok(())
}

fn runtime_failure_marker_relative(revision: &str) -> PathBuf {
    Path::new(GUEST_RUNTIME_ROOT).join(format!("activation-failure.{revision}.json"))
}

fn read_runtime_activation_failure(
    rootfs: &Path,
    failed_revision: &str,
    expected_uid: u32,
) -> Result<Option<GuestRuntimeActivationFailure>> {
    let path = rootfs.join(runtime_failure_marker_relative(failed_revision));
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspecting runtime failure evidence {}", path.display())
            });
        }
    };
    let expected_gid = if expected_uid == 0 {
        0
    } else {
        unsafe { libc::getegid() }
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != expected_uid
        || metadata.gid() != expected_gid
        || metadata.permissions().mode() & 0o022 != 0
        || metadata.len() > MAX_GUEST_RUNTIME_MANIFEST_BYTES
    {
        bail!("protected runtime failure evidence is unsafe");
    }
    let evidence: GuestRuntimeActivationFailure = serde_json::from_slice(
        &fs::read(&path)
            .with_context(|| format!("reading runtime failure evidence {}", path.display()))?,
    )
    .context("parsing protected runtime failure evidence")?;
    if evidence.schema_version != 1
        || evidence.failed_revision != failed_revision
        || !valid_runtime_revision(&evidence.fallback_revision)
        || evidence.fallback_revision == failed_revision
        || evidence.reason != "desktop readiness deadline expired"
        || evidence.observed_at_unix_seconds == 0
    {
        bail!("protected runtime failure evidence is invalid");
    }
    Ok(Some(evidence))
}

fn rollback_guest_runtime(
    rootfs: &Path,
    expected_current: &str,
    expected_previous: &str,
    reason: &str,
    expected_uid: u32,
) -> Result<()> {
    if !valid_runtime_revision(expected_current)
        || !valid_runtime_revision(expected_previous)
        || expected_current == expected_previous
        || reason != "desktop readiness deadline expired"
    {
        bail!("invalid protected runtime rollback request");
    }
    validate_guest_rootfs(rootfs)?;
    let runtime_root = rootfs.join(GUEST_RUNTIME_ROOT);
    let root_metadata =
        protected_runtime_metadata(&runtime_root, Some(expected_uid), "runtime root")?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        bail!("protected runtime root is not a real directory");
    }
    if read_revision_link(&runtime_root, "current", expected_uid)?.as_deref()
        != Some(expected_current)
    {
        bail!("protected runtime current changed before guarded rollback");
    }
    if read_revision_link(&runtime_root, "previous", expected_uid)?.as_deref()
        != Some(expected_previous)
    {
        bail!("protected runtime previous changed before guarded rollback");
    }

    let failed_dir = runtime_root.join(expected_current);
    let (failed_manifest, _) =
        read_guest_runtime_manifest(&failed_dir, Some(expected_uid), expected_current)?;
    validate_guest_runtime_tree(&failed_dir, &failed_manifest, Some(expected_uid), true)?;

    let previous_dir = runtime_root.join(expected_previous);
    let (previous_manifest, previous_manifest_bytes) =
        read_guest_runtime_manifest(&previous_dir, Some(expected_uid), expected_previous)?;
    validate_guest_runtime_tree(&previous_dir, &previous_manifest, Some(expected_uid), true)?;
    validate_runtime_readiness(
        &previous_dir,
        expected_previous,
        &previous_manifest_bytes,
        expected_uid,
    )?;

    let evidence = GuestRuntimeActivationFailure {
        schema_version: 1,
        failed_revision: expected_current.to_owned(),
        fallback_revision: expected_previous.to_owned(),
        reason: reason.to_owned(),
        observed_at_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock precedes the Unix epoch")?
            .as_secs(),
    };
    let mut evidence_bytes =
        serde_json::to_vec_pretty(&evidence).context("serializing runtime failure evidence")?;
    evidence_bytes.push(b'\n');
    install_guest_asset(
        rootfs,
        &runtime_failure_marker_relative(expected_current),
        &evidence_bytes,
        0o644,
    )?;
    atomic_revision_link(&runtime_root, "current", expected_previous, expected_uid)?;
    File::open(&runtime_root)
        .and_then(|directory| directory.sync_all())
        .with_context(|| {
            format!(
                "syncing protected runtime rollback {}",
                runtime_root.display()
            )
        })
}

fn validated_failed_runtime_fallback(
    rootfs: &Path,
    failed_revision: &str,
    current_revision: &str,
    expected_uid: u32,
) -> Result<bool> {
    let Some(evidence) = read_runtime_activation_failure(rootfs, failed_revision, expected_uid)?
    else {
        return Ok(false);
    };
    if evidence.fallback_revision != current_revision {
        return Ok(false);
    }
    let fallback_dir = rootfs.join(GUEST_RUNTIME_ROOT).join(current_revision);
    let (manifest, manifest_bytes) =
        read_guest_runtime_manifest(&fallback_dir, Some(expected_uid), current_revision)?;
    validate_guest_runtime_tree(&fallback_dir, &manifest, Some(expected_uid), true)?;
    validate_runtime_readiness(
        &fallback_dir,
        current_revision,
        &manifest_bytes,
        expected_uid,
    )?;
    Ok(true)
}

fn migrate_guest_assets(rootfs: &Path) -> Result<()> {
    validate_guest_rootfs(rootfs)?;
    remove_empty_nvidia_mount_placeholders(rootfs)?;
    install_bundled_guest_runtime(rootfs)?;
    let previous = read_guest_asset_manifest(rootfs);
    let legacy_tiled_sway_config = GuestAssetRecord {
        sha256: LEGACY_TILED_SWAY_CONFIG_SHA256.into(),
        mode: 0o644,
    };
    for (relative, contents, mode) in GUEST_ASSETS {
        migrate_guest_asset(
            rootfs,
            Path::new(relative),
            contents,
            *mode,
            previous
                .as_ref()
                .and_then(|manifest| manifest.assets.get(*relative)),
            match *relative {
                "etc/wildbuzzard/sway-config" => Some(&legacy_tiled_sway_config),
                _ => None,
            },
        )?;
    }
    for relative in [
        "usr/libexec/wildbuzzard-wayfire-session",
        "etc/xdg/wayfire.ini",
        "etc/chromium.d/wildbuzzard",
        "etc/chromium/master_preferences",
        "usr/share/themes/WildBuzzard/index.theme",
        "usr/share/themes/WildBuzzard/gtk-3.0/gtk.css",
        "usr/share/themes/WildBuzzard/gtk-4.0/gtk.css",
        "usr/share/color-schemes/WildBuzzard.colors",
        "usr/libexec/wildbuzzard-init",
        "usr/libexec/wildbuzzard-session",
        "usr/libexec/wildbuzzard-sway-session",
        "usr/libexec/wildbuzzard-output-sync",
        "usr/libexec/wildbuzzard-desktop-stopped",
        "usr/libexec/wildbuzzard-desktop-services",
        "usr/libexec/wildbuzzard-integration-agent",
        "usr/libexec/wildbuzzard-appimage-ready",
        "usr/libexec/wildbuzzard-fusermount",
        "usr/libexec/wildbuzzard-fusermount-exec",
        "usr/libexec/wildbuzzard-sudo-exec",
        "usr/libexec/wildbuzzard-shell",
        "usr/libexec/wildbuzzard-settings",
    ] {
        remove_retired_guest_asset(
            rootfs,
            Path::new(relative),
            previous
                .as_ref()
                .and_then(|manifest| manifest.assets.get(relative)),
        )?;
    }
    let legacy_cua_record = GuestAssetRecord {
        sha256: LEGACY_REFERENCE_CUA_SHA256.into(),
        mode: 0o755,
    };
    remove_retired_guest_asset(
        rootfs,
        Path::new("usr/local/bin/cua-driver"),
        previous
            .as_ref()
            .and_then(|manifest| manifest.assets.get("usr/local/bin/cua-driver"))
            .or(Some(&legacy_cua_record)),
    )?;
    install_guest_asset_manifest(rootfs, &current_guest_asset_manifest())?;
    // The revision is the commit marker and is deliberately written last. If
    // a migration fails, the next start retries without mistaking a partial
    // update for a completed one.
    install_guest_asset(
        rootfs,
        Path::new("usr/lib/wildbuzzard/guest-assets.version"),
        GUEST_ASSETS_REVISION.as_bytes(),
        0o644,
    )
}

fn guest_asset_record(contents: &[u8], mode: u32) -> GuestAssetRecord {
    GuestAssetRecord {
        sha256: format!("{:x}", Sha256::digest(contents)),
        mode,
    }
}

fn current_guest_asset_manifest() -> GuestAssetManifest {
    let mut assets = BTreeMap::new();
    for (relative, contents, mode) in GUEST_ASSETS {
        assets.insert((*relative).to_owned(), guest_asset_record(contents, *mode));
    }
    GuestAssetManifest { schema: 1, assets }
}

fn read_guest_asset_manifest(rootfs: &Path) -> Option<GuestAssetManifest> {
    let bytes = fs::read(rootfs.join(GUEST_ASSETS_MANIFEST)).ok()?;
    let manifest: GuestAssetManifest = serde_json::from_slice(&bytes).ok()?;
    (manifest.schema == 1).then_some(manifest)
}

fn install_guest_asset_manifest(rootfs: &Path, manifest: &GuestAssetManifest) -> Result<()> {
    let mut contents =
        serde_json::to_vec_pretty(manifest).context("serializing guest asset manifest")?;
    contents.push(b'\n');
    install_guest_asset(rootfs, Path::new(GUEST_ASSETS_MANIFEST), &contents, 0o644)
}

fn guest_asset_matches_record(path: &Path, record: &GuestAssetRecord) -> Result<bool> {
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspecting {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "guest asset destination {} is not a regular file",
            path.display()
        );
    }
    let contents = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(metadata.permissions().mode() & 0o7777 == record.mode
        && format!("{:x}", Sha256::digest(&contents)) == record.sha256)
}

fn remove_retired_guest_asset(
    rootfs: &Path,
    relative: &Path,
    previous: Option<&GuestAssetRecord>,
) -> Result<()> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let destination = prepare_guest_asset_destination(rootfs, relative)?;
    match fs::symlink_metadata(&destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("inspecting retired asset {}", destination.display())),
        Ok(_) if guest_asset_matches_record(&destination, previous)? => {
            remove_guest_file(rootfs, relative)
        }
        Ok(_) => {
            // A guest edit turns a formerly managed file into persistent user
            // state. Preserve it even though new Wild Buzzard releases no
            // longer reference it.
            Ok(())
        }
    }
}

fn migrate_guest_asset(
    rootfs: &Path,
    relative: &Path,
    contents: &[u8],
    mode: u32,
    previous: Option<&GuestAssetRecord>,
    recognized_legacy: Option<&GuestAssetRecord>,
) -> Result<()> {
    let destination = prepare_guest_asset_destination(rootfs, relative)?;
    match fs::symlink_metadata(&destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            install_guest_asset(rootfs, relative, contents, mode)
        }
        Err(error) => {
            Err(error).with_context(|| format!("inspecting guest asset {}", destination.display()))
        }
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "guest asset destination {} is not a regular file",
                    destination.display()
                );
            }
            let matches_previous = match previous {
                Some(record) => guest_asset_matches_record(&destination, record)?,
                None => false,
            };
            let matches_recognized_legacy = match recognized_legacy {
                Some(record) => guest_asset_matches_record(&destination, record)?,
                None => false,
            };
            let replace = matches_previous || matches_recognized_legacy;
            if replace {
                install_guest_asset(rootfs, relative, contents, mode)
            } else {
                // No previous manifest means a legacy machine. Existing files
                // are conservatively user-owned; with a manifest, any
                // content/mode change from the last distributed record is
                // likewise preserved.
                Ok(())
            }
        }
    }
}

fn remove_kde_wallet_activation(rootfs: &Path) -> Result<()> {
    for relative in [
        "usr/share/dbus-1/services/org.kde.kwalletd5.service",
        "usr/share/dbus-1/services/org.kde.kwalletd6.service",
        "usr/share/dbus-1/services/org.kde.secretservicecompat.service",
        "usr/share/dbus-1/services/org.freedesktop.impl.portal.desktop.kwallet.service",
        "usr/share/xdg-desktop-portal/portals/kwallet.portal",
        "usr/share/applications/org.kde.ksecretd.desktop",
    ] {
        remove_guest_file(rootfs, Path::new(relative))?;
    }
    Ok(())
}

fn remove_empty_nvidia_mount_placeholders(rootfs: &Path) -> Result<()> {
    for relative in [
        "etc/vulkan/icd.d/nvidia_icd.json",
        "usr/share/vulkan/icd.d/nvidia_icd.json",
    ] {
        let destination = prepare_guest_asset_destination(rootfs, Path::new(relative))?;
        match fs::symlink_metadata(&destination) {
            Ok(metadata)
                if metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.len() == 0 =>
            {
                fs::remove_file(&destination).with_context(|| {
                    format!(
                        "removing stale NVIDIA metadata mountpoint {}",
                        destination.display()
                    )
                })?;
            }
            Ok(_) => {
                // A nonempty guest-installed ICD is user state. Preserve it.
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "inspecting NVIDIA metadata mountpoint {}",
                        destination.display()
                    )
                });
            }
        }
    }
    Ok(())
}

fn remove_guest_file(rootfs: &Path, relative: &Path) -> Result<()> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("unsafe guest file path {}", relative.display());
    }
    let mut parent = rootfs.to_path_buf();
    for component in relative
        .parent()
        .context("guest file path has no parent")?
        .components()
    {
        let std::path::Component::Normal(component) = component else {
            bail!("unsafe guest file path {}", relative.display());
        };
        parent.push(component);
        match fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => bail!(
                "guest file parent {} is not a real directory",
                parent.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting guest file path {}", parent.display()));
            }
        }
    }
    let destination = rootfs.join(relative);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(&destination)
                .with_context(|| format!("removing guest service {}", destination.display()))
        }
        Ok(_) => bail!(
            "guest service path {} is not a regular file",
            destination.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("inspecting guest service {}", destination.display())),
    }
}

fn prepare_guest_asset_destination(rootfs: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("unsafe guest asset path {}", relative.display());
    }

    let mut parent = rootfs.to_path_buf();
    let relative_parent = relative
        .parent()
        .context("guest asset path has no parent")?;
    for component in relative_parent.components() {
        let std::path::Component::Normal(component) = component else {
            bail!("unsafe guest asset path {}", relative.display());
        };
        parent.push(component);
        match fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => bail!(
                "guest asset parent {} is not a real directory",
                parent.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&parent).with_context(|| {
                    format!("creating guest asset directory {}", parent.display())
                })?;
                fs::set_permissions(&parent, fs::Permissions::from_mode(0o755))
                    .with_context(|| format!("setting permissions on {}", parent.display()))?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting guest asset path {}", parent.display()));
            }
        }
    }
    Ok(rootfs.join(relative))
}

fn install_guest_asset(rootfs: &Path, relative: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let destination = prepare_guest_asset_destination(rootfs, relative)?;
    if let Ok(metadata) = fs::symlink_metadata(&destination)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        bail!(
            "guest asset destination {} is not a regular file",
            destination.display()
        );
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&destination)
        .with_context(|| format!("installing guest asset {}", destination.display()))?;
    file.write_all(contents)
        .with_context(|| format!("writing guest asset {}", destination.display()))?;
    file.set_permissions(fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting permissions on {}", destination.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing guest asset {}", destination.display()))
}

fn start(paths: &WbPaths, name: &str, detach: bool) -> Result<()> {
    let machine_dir = require_machine(paths, name)?;
    let _config = MachineConfig::load(&machine_dir)?;

    if let Some(state) = RuntimeState::load(&machine_dir)? {
        if state.state == MachineState::Running && runtime_is_live(&state, &machine_dir) {
            send_host_control(&machine_dir, "restore")?;
            println!("Machine '{name}' is already running; restored its host window");
            return Ok(());
        }
        if supervisor_is_live(&state, &machine_dir) {
            let rootfs = machine_dir.join("rootfs");
            let activation = refresh_guest_assets(&rootfs)?;
            for diagnostic in guest_settings_runtime_diagnostics(&rootfs)? {
                eprintln!("wildbuzzard: {diagnostic}");
            }
            let supervisor_pid = state.launcher_pid;
            let reused = send_host_control(&machine_dir, "start")
                .and_then(|()| wait_for_supervised_start(&machine_dir, Duration::from_secs(95)));
            match reused {
                Ok(()) => {
                    println!("Started '{name}' in its existing host window");
                    return Ok(());
                }
                Err(error) => {
                    let readiness_deadline =
                        error.downcast_ref::<DesktopReadinessDeadline>().is_some();
                    let error = recover_new_runtime_after_readiness_deadline(
                        &machine_dir,
                        &rootfs,
                        activation.as_ref(),
                        error,
                    );
                    if readiness_deadline && activation.is_some() {
                        return Err(error);
                    }
                    if let Some(pid) = supervisor_pid {
                        let _ = wait_for_process_exit(pid, Duration::from_secs(5));
                    }
                    if RuntimeState::load(&machine_dir)?
                        .as_ref()
                        .is_some_and(|latest| supervisor_is_live(latest, &machine_dir))
                    {
                        return Err(error);
                    }
                    eprintln!(
                        "The previous host window completed closing during start; opening a new native window"
                    );
                }
            }
        }
    }

    let rootfs = machine_dir.join("rootfs");
    let activation = refresh_guest_assets(&rootfs)?;
    for diagnostic in guest_settings_runtime_diagnostics(&rootfs)? {
        eprintln!("wildbuzzard: {diagnostic}");
    }

    let current = std::env::current_exe().context("locating launcher")?;
    let broker = current
        .parent()
        .context("launcher path has no parent")?
        .join("wildbuzzard-broker");
    let broker = if broker.is_file() {
        broker
    } else {
        ResourceLocator::discover()?.helper_or_path("wildbuzzard-broker")?
    };

    let mut command = Command::new(&broker);
    command
        .arg("run")
        .arg("--machine-dir")
        .arg(&machine_dir)
        .arg("--shared")
        .arg(paths.shared());
    if detach {
        command
            .arg("--detach")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", broker.display()))?;
    let broker_pid = child.id();
    if let Err(error) = wait_for_detached_start(
        &machine_dir,
        &mut child,
        broker_pid,
        Duration::from_secs(95),
    ) {
        return Err(recover_new_runtime_after_readiness_deadline(
            &machine_dir,
            &rootfs,
            activation.as_ref(),
            error,
        ));
    }
    if detach {
        println!("Started '{name}' (broker pid {})", child.id());
        Ok(())
    } else {
        let status = child.wait().context("waiting for machine")?;
        if status.success() {
            Ok(())
        } else {
            bail!("machine exited with {status}")
        }
    }
}

fn observed_runtime_link(rootfs: &Path, name: &str) -> Result<Option<String>> {
    let path = rootfs.join(GUEST_RUNTIME_ROOT).join(name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspecting runtime link {}", path.display()));
        }
    };
    if !metadata.file_type().is_symlink() {
        bail!("protected runtime {name} is not a symbolic link");
    }
    let target = fs::read_link(&path)
        .with_context(|| format!("reading protected runtime link {}", path.display()))?;
    let target = target
        .to_str()
        .context("protected runtime link target is not UTF-8")?;
    if !valid_runtime_revision(target) {
        bail!("protected runtime {name} has an unsafe target");
    }
    Ok(Some(target.to_owned()))
}

fn refresh_guest_assets(rootfs: &Path) -> Result<Option<GuestRuntimeActivation>> {
    let before = observed_runtime_link(rootfs, "current")?;
    if guest_assets_are_current_rootless(rootfs)? {
        return Ok(None);
    }

    let status = run_guest_asset_helper(rootfs, "__install-guest-assets")?;
    if !status.success() {
        bail!("rootless guest asset migration exited with {status}");
    }
    if !guest_assets_are_current_rootless(rootfs)? {
        bail!("rootless guest asset migration did not commit its revision");
    }
    let revision = guest_runtime_revision()?;
    let after = observed_runtime_link(rootfs, "current")?;
    if after.as_deref() != Some(revision.as_str()) {
        bail!("rootless guest asset migration activated an unexpected runtime revision");
    }
    let activation = before
        .filter(|previous| previous != &revision)
        .map(|previous| GuestRuntimeActivation { revision, previous });
    if let Some(activation) = &activation
        && observed_runtime_link(rootfs, "previous")?.as_deref()
            != Some(activation.previous.as_str())
    {
        bail!("rootless guest asset migration did not retain its previous revision");
    }
    Ok(activation)
}

fn guest_settings_runtime_diagnostics(rootfs: &Path) -> Result<Vec<String>> {
    validate_guest_rootfs(rootfs)?;
    let canonical_rootfs = rootfs
        .canonicalize()
        .with_context(|| format!("resolving guest rootfs {}", rootfs.display()))?;
    let mut diagnostics = Vec::new();
    for (relative, recovery) in [
        (
            "usr/lib/x86_64-linux-gnu/libgtk-4.so.1",
            "sudo apt install libgtk-4-1",
        ),
        (
            "usr/bin/gsettings",
            "sudo apt install libglib2.0-bin gsettings-desktop-schemas dconf-gsettings-backend",
        ),
        (
            "usr/share/glib-2.0/schemas/org.gnome.desktop.interface.gschema.xml",
            "sudo apt install gsettings-desktop-schemas dconf-gsettings-backend",
        ),
        (
            "usr/share/glib-2.0/schemas/gschemas.compiled",
            "sudo apt install --reinstall gsettings-desktop-schemas",
        ),
        ("usr/bin/unsquashfs", "sudo apt install squashfs-tools"),
    ] {
        let path = rootfs.join(relative);
        let resolved = match path.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                diagnostics.push(format!(
                    "persistent guest compatibility warning: /{relative} is missing. The desktop remains bootable with degraded Settings/theme/AppImage integration; inside the guest run: {recovery}"
                ));
                continue;
            }
            Err(error) => {
                diagnostics.push(format!(
                    "persistent guest compatibility warning: /{relative} could not be inspected ({error}). The desktop remains bootable, but Settings/theme/AppImage integration may be degraded; inside the guest run: {recovery}"
                ));
                continue;
            }
        };
        if !resolved.starts_with(&canonical_rootfs) || !resolved.is_file() {
            bail!(
                "persistent guest Settings runtime /{relative} escapes the rootfs or is not a regular file"
            );
        }
    }
    Ok(diagnostics)
}

fn guest_assets_are_current_rootless(rootfs: &Path) -> Result<bool> {
    let status = run_guest_asset_helper(rootfs, "__verify-guest-assets")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(3) => Ok(false),
        _ => bail!("rootless guest asset verification exited with {status}"),
    }
}

fn run_guest_asset_helper(
    rootfs: &Path,
    internal_command: &str,
) -> Result<std::process::ExitStatus> {
    let id_map = IdMap::discover()?;
    let resources = ResourceLocator::discover()?;
    let unshare = resources.helper_or_path("unshare")?;
    let namespace_program = id_map.namespace_program(&unshare)?;
    let launcher = std::env::current_exe().context("locating guest asset helper")?;
    let mut command = Command::new(namespace_program);
    command.env_clear();
    id_map.configure_command(&mut command);
    command
        .args(id_map.namespace_args())
        .arg(&launcher)
        .arg(internal_command)
        .arg("--rootfs")
        .arg(rootfs)
        .stdin(Stdio::null());
    command.status().with_context(|| {
        format!(
            "starting rootless guest asset helper through {}",
            namespace_program.display()
        )
    })
}

fn run_guest_runtime_rollback_helper(
    rootfs: &Path,
    activation: &GuestRuntimeActivation,
) -> Result<()> {
    let id_map = IdMap::discover()?;
    let resources = ResourceLocator::discover()?;
    let unshare = resources.helper_or_path("unshare")?;
    let namespace_program = id_map.namespace_program(&unshare)?;
    let launcher = std::env::current_exe().context("locating guest runtime rollback helper")?;
    let mut command = Command::new(namespace_program);
    command.env_clear();
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.namespace_args())
        .arg(&launcher)
        .arg("__rollback-guest-runtime")
        .arg("--rootfs")
        .arg(rootfs)
        .arg("--expected-current")
        .arg(&activation.revision)
        .arg("--expected-previous")
        .arg(&activation.previous)
        .stdin(Stdio::null())
        .status()
        .with_context(|| {
            format!(
                "starting rootless guest runtime rollback through {}",
                namespace_program.display()
            )
        })?;
    if !status.success() {
        bail!("guarded guest runtime rollback exited with {status}");
    }
    Ok(())
}

fn recover_new_runtime_after_readiness_deadline(
    machine_dir: &Path,
    rootfs: &Path,
    activation: Option<&GuestRuntimeActivation>,
    error: anyhow::Error,
) -> anyhow::Error {
    let Some(activation) = activation else {
        return error;
    };
    if error.downcast_ref::<DesktopReadinessDeadline>().is_none() {
        return error;
    }
    let recovery = (|| -> Result<()> {
        if RuntimeState::load(machine_dir)?.is_some_and(|state| {
            supervisor_is_live(&state, machine_dir) || state.container_pid.is_some()
        }) {
            send_host_control(machine_dir, "stop")
                .context("stopping the failed newly activated runtime")?;
            wait_for_machine_stopped(machine_dir, Duration::from_secs(30))
                .context("waiting for the failed newly activated runtime to stop")?;
        }
        run_guest_runtime_rollback_helper(rootfs, activation)
    })();
    match recovery {
        Ok(()) => error.context(format!(
            "new protected runtime {} missed desktop readiness; restored complete revision {} and retained failure evidence under /opt/wildbuzzard/runtime (start the machine again to use the fallback)",
            activation.revision, activation.previous
        )),
        Err(recovery_error) => error.context(format!(
            "new protected runtime {} missed desktop readiness, and its guarded fallback to {} failed: {recovery_error:#}",
            activation.revision, activation.previous
        )),
    }
}

fn guest_assets_are_current(rootfs: &Path) -> Result<bool> {
    let installed = fs::read_to_string(rootfs.join("usr/lib/wildbuzzard/guest-assets.version"))
        .unwrap_or_default();
    if installed != GUEST_ASSETS_REVISION {
        return Ok(false);
    }

    let Some(installed_manifest) = read_guest_asset_manifest(rootfs) else {
        return Ok(false);
    };
    if installed_manifest != current_guest_asset_manifest() {
        return Ok(false);
    }

    let revision = guest_runtime_revision()?;
    let source = bundled_guest_runtime_dir(&revision)?;
    let (bundled_manifest, _) = read_guest_runtime_manifest(&source, None, &revision)?;
    validate_guest_runtime_tree(&source, &bundled_manifest, None, false)?;
    let runtime_root = rootfs.join(GUEST_RUNTIME_ROOT);
    let Some(current_revision) = read_revision_link(&runtime_root, "current", 0)? else {
        return Ok(false);
    };
    if current_revision != revision {
        return validated_failed_runtime_fallback(rootfs, &revision, &current_revision, 0);
    }
    let installed_revision = runtime_root.join(&revision);
    let (installed_runtime_manifest, _) =
        read_guest_runtime_manifest(&installed_revision, Some(0), &revision)?;
    if installed_runtime_manifest != bundled_manifest {
        return Ok(false);
    }
    validate_guest_runtime_tree(
        &installed_revision,
        &installed_runtime_manifest,
        Some(0),
        true,
    )?;
    Ok(true)
}

fn wait_for_detached_start(
    machine_dir: &Path,
    broker: &mut std::process::Child,
    expected_broker_pid: u32,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(state) = RuntimeState::load(machine_dir)?
            && state.launcher_pid == Some(expected_broker_pid)
        {
            match state.state {
                MachineState::Running if runtime_is_live(&state, machine_dir) => {
                    return Ok(());
                }
                MachineState::Failed => {
                    if let Some(deadline) = state_desktop_readiness_deadline(&state) {
                        return Err(deadline.into());
                    }
                    bail!(
                        "machine failed to start: {}",
                        state.detail.as_deref().unwrap_or("no diagnostic")
                    );
                }
                _ => {}
            }
        }
        if let Some(status) = broker.try_wait().context("checking detached broker")? {
            if let Some(state) = RuntimeState::load(machine_dir)?
                && state.state == MachineState::Failed
            {
                if let Some(deadline) = state_desktop_readiness_deadline(&state) {
                    return Err(deadline.into());
                }
                bail!(
                    "machine failed to start: {}",
                    state.detail.as_deref().unwrap_or("no diagnostic")
                );
            }
            bail!("machine broker exited with {status} before desktop readiness");
        }
        if Instant::now() >= deadline {
            return Err(DesktopReadinessDeadline {
                seconds: timeout.as_secs(),
                diagnostic: None,
            }
            .into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stop(paths: &WbPaths, name: &str) -> Result<()> {
    let machine_dir = require_machine(paths, name)?;
    let mut state = RuntimeState::load(&machine_dir)?.context("machine has no runtime state")?;
    if matches!(state.state, MachineState::Stopped | MachineState::Failed)
        && supervisor_is_live(&state, &machine_dir)
    {
        println!("'{name}' is already stopped; its host window remains open");
        return Ok(());
    }
    if state.state == MachineState::Starting {
        let Some(broker_pid) = state.launcher_pid else {
            return repair_stale_stop(&machine_dir, &mut state, name);
        };
        if !broker_matches_machine(broker_pid, &machine_dir) {
            return repair_stale_stop(&machine_dir, &mut state, name);
        }
        if let Some(pid) = state.container_pid {
            state.state = MachineState::Stopping;
            state.detail = Some("cancelling machine startup".into());
            state.save(&machine_dir)?;
            let result = unsafe { libc::kill(pid as i32, libc::SIGRTMIN() + 3) };
            if result != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("cancelling machine startup process {pid}"));
            }
            wait_for_machine_stopped(&machine_dir, Duration::from_secs(30))?;
            println!("Cancelled startup of '{name}'; its host window remains open");
            return Ok(());
        }
        send_host_control(&machine_dir, "stop")?;
        wait_for_machine_stopped(&machine_dir, Duration::from_secs(95))?;
        println!("Cancelled startup of '{name}'; its host window remains open");
        return Ok(());
    }
    if !runtime_is_live(&state, &machine_dir) {
        return repair_stale_stop(&machine_dir, &mut state, name);
    }
    let pid = state
        .container_pid
        .context("live machine state has no systemd process id")?;
    state.state = MachineState::Stopping;
    state.detail = Some("orderly shutdown requested".into());
    state.save(&machine_dir)?;
    let signal = libc::SIGRTMIN() + 3;
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("requesting systemd shutdown from process {pid}"));
    }
    if wait_for_process_exit(pid, Duration::from_secs(20)) {
        wait_for_machine_stopped(&machine_dir, Duration::from_secs(10))?;
        println!("Stopped '{name}' cleanly; its host window remains open");
        return Ok(());
    }

    eprintln!("Orderly shutdown timed out; sending SIGTERM to '{name}'");
    signal_process(pid, libc::SIGTERM)?;
    if wait_for_process_exit(pid, Duration::from_secs(5)) {
        wait_for_machine_stopped(&machine_dir, Duration::from_secs(10))?;
        println!("Stopped '{name}' after SIGTERM; its host window remains open");
        return Ok(());
    }

    eprintln!("SIGTERM timed out; sending SIGKILL to '{name}'");
    signal_process(pid, libc::SIGKILL)?;
    if !wait_for_process_exit(pid, Duration::from_secs(5)) {
        bail!("machine process {pid} did not exit after SIGKILL");
    }
    wait_for_machine_stopped(&machine_dir, Duration::from_secs(10))?;
    println!("Stopped '{name}' after forced termination; its host window remains open");
    Ok(())
}

fn wait_for_machine_stopped(machine_dir: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if RuntimeState::load(machine_dir)?.is_some_and(|state| {
            matches!(state.state, MachineState::Stopped | MachineState::Failed)
                && state.container_pid.is_none()
        }) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "machine did not enter stopped state within {} seconds",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_supervised_start(machine_dir: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(state) = RuntimeState::load(machine_dir)? {
            match state.state {
                MachineState::Running if runtime_is_live(&state, machine_dir) => return Ok(()),
                MachineState::Failed => {
                    if let Some(deadline) = state_desktop_readiness_deadline(&state) {
                        return Err(deadline.into());
                    }
                    bail!(
                        "machine failed to start: {}",
                        state.detail.as_deref().unwrap_or("no diagnostic")
                    )
                }
                _ => {}
            }
            if !supervisor_is_live(&state, machine_dir) {
                bail!("machine lifecycle supervisor exited during startup");
            }
        }
        if Instant::now() >= deadline {
            return Err(DesktopReadinessDeadline {
                seconds: timeout.as_secs(),
                diagnostic: None,
            }
            .into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn send_host_control(machine_dir: &Path, command: &str) -> Result<()> {
    use std::os::unix::net::UnixStream;

    let socket = host_control_socket(machine_dir)?;
    let mut connection = UnixStream::connect(&socket)
        .with_context(|| format!("connecting to host window control {}", socket.display()))?;
    connection
        .write_all(format!("{command}\n").as_bytes())
        .context("sending host window control request")?;
    let mut response = String::new();
    connection
        .read_to_string(&mut response)
        .context("reading host window control response")?;
    if response.trim() == "ok" {
        Ok(())
    } else {
        bail!("host window control failed: {}", response.trim())
    }
}

fn window(paths: &WbPaths, name: &str, action: WindowAction) -> Result<()> {
    let machine_dir = require_machine(paths, name)?;
    let state = RuntimeState::load(&machine_dir)?.context("machine has no runtime state")?;
    if !supervisor_is_live(&state, &machine_dir) {
        bail!("machine '{name}' has no live host window");
    }
    send_host_control(&machine_dir, action.as_str())
}

fn repair_stale_stop(machine_dir: &Path, state: &mut RuntimeState, name: &str) -> Result<()> {
    state.state = MachineState::Stopped;
    state.launcher_pid = None;
    state.container_pid = None;
    state.detail = Some("stale process state repaired".into());
    state.save(machine_dir)?;
    println!("'{name}' is already stopped");
    Ok(())
}

fn status(paths: &WbPaths, name: &str) -> Result<()> {
    let machine_dir = require_machine(paths, name)?;
    let config = MachineConfig::load(&machine_dir)?;
    let state = RuntimeState::load(&machine_dir)?;
    println!("name: {}", config.name);
    println!("image: {}", config.image);
    if let Some(digest) = &config.image_digest {
        println!("image digest: {digest}");
    }
    println!("rootfs: {}", machine_dir.join("rootfs").display());
    println!("shared folder: {}", paths.shared().display());
    println!("network: {:?}", config.network);
    println!(
        "configured initial window: {}x{}",
        config.width, config.height
    );
    match config.guest_scale_120 {
        Some(scale_120) => println!(
            "configured guest desktop scale: {:.2}x ({:.0}%)",
            f64::from(scale_120) / 120.0,
            f64::from(scale_120) / 1.2
        ),
        None => println!("configured guest desktop scale: Follow Host"),
    }
    println!("gpus: {}", config.gpus.join(","));
    match state {
        Some(state) => {
            let live = runtime_is_live(&state, &machine_dir);
            println!(
                "state: {:?}{}",
                state.state,
                if live { "" } else { " (not live)" }
            );
            if let Some(pid) = state.container_pid {
                println!("container pid: {pid}");
            }
            if let Some(display) = state.display {
                println!("compositor renderer: {}", display.renderer);
                if let Some(identity) = display.selected_render_device_identity {
                    println!("selected compositor render device: {identity}");
                }
                println!("passed GPU devices: {}", display.exposed_devices.join(", "));
                println!(
                    "compositor-open GPU devices: {}",
                    display.render_nodes.join(", ")
                );
                if !display.render_device_identities.is_empty() {
                    println!(
                        "compositor render identity: {}",
                        display.render_device_identities.join(", ")
                    );
                }
                if let Some(identity) = display.host_device_identity {
                    println!("host dmabuf main identity: {identity}");
                }
                println!(
                    "application-open GPU devices: {}",
                    display.application_devices.join(", ")
                );
                println!(
                    "host dmabuf: {}",
                    if display.host.linux_dmabuf {
                        "yes"
                    } else {
                        "no"
                    }
                );
                println!(
                    "host explicit sync: {}",
                    if display.host.explicit_sync {
                        "yes"
                    } else {
                        "no"
                    }
                );
                if !display.host.explicit_sync_protocols.is_empty() {
                    println!(
                        "host explicit sync protocols: {}",
                        display.host.explicit_sync_protocols.join(", ")
                    );
                }
                println!(
                    "host server-side decorations: {}",
                    if display.host.server_side_decorations {
                        "yes"
                    } else {
                        "no"
                    }
                );
                println!(
                    "host fractional scale + viewporter: {}",
                    if display.host.fractional_scale && display.host.viewporter {
                        "yes"
                    } else {
                        "no"
                    }
                );
                println!(
                    "host color management + representation: {}",
                    if display.host.color_management && display.host.color_representation {
                        "advertised (gateway translation required)"
                    } else {
                        "unavailable"
                    }
                );
                if !display.host.globals.is_empty() {
                    println!(
                        "host Wayland protocol inventory: {}",
                        display
                            .host
                            .globals
                            .iter()
                            .map(|(interface, version)| format!("{interface}@{version}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                if let Some(window) = display.window {
                    println!("host toplevels: {}", window.toplevels);
                    println!("host window: {}x{}", window.width, window.height);
                    println!("host window title: {}", window.title);
                    println!("host window app-id: {}", window.app_id);
                    println!("host window decorations: {}", window.decorations);
                    println!("host window maximized: {}", window.maximized);
                }
                if let Some(frame) = display.presentation {
                    println!("presentation transport: {}", frame.transport);
                    println!(
                        "presentation buffer: {}x{}, format {}, modifier {}, {} plane(s)",
                        frame.width, frame.height, frame.format, frame.modifier, frame.planes
                    );
                    println!(
                        "host surface scale: {:.2}x, viewport={}x{}, native-resolution={}",
                        f64::from(frame.scale_120) / 120.0,
                        frame.viewport_width,
                        frame.viewport_height,
                        frame.native_resolution
                    );
                    println!("presentation explicit sync: {}", frame.explicit_sync);
                    println!(
                        "presentation feedback: presented={}, vblank={}, zero-copy={}, refresh={}ns, sequence={}",
                        frame.presented,
                        frame.vsync,
                        frame.zero_copy,
                        frame.refresh_ns,
                        frame.sequence
                    );
                    println!(
                        "frame lifecycle: submitted={}, painted={}, presented={}, released={}",
                        frame.submitted_frames,
                        frame.painted_frames,
                        frame.presented_frames,
                        frame.released_frames
                    );
                    println!(
                        "guest scanout pacing: source={}, hidden-window frames={}, discarded feedback={}",
                        frame.last_pacing_source,
                        frame.background_paced_frames,
                        frame.background_feedback_discarded
                    );
                    println!(
                        "current host presentation: {}",
                        if frame.last_pacing_source == "internal-hidden-window-clock" {
                            "not presenting (window minimized, covered, unfocused, or on another workspace); guest output remains live"
                        } else {
                            "receiving real host frame/vblank callbacks"
                        }
                    );
                    println!(
                        "superseded before paint: {} (newer dmabuf replaced an unpainted buffer; not stream loss)",
                        frame.superseded_before_paint
                    );
                    println!(
                        "presentation feedback unavailable: {}",
                        frame.presentation_feedback_unavailable
                    );
                    println!(
                        "acquire-fence wait: last={}us, maximum={}us, explicit-sync frames={}",
                        frame.last_acquire_wait_us,
                        frame.maximum_acquire_wait_us,
                        frame.explicit_sync_frames
                    );
                    println!(
                        "submit-to-presentation: last={}us, maximum={}us; last frame interval={}us, refresh interval={}us",
                        frame.last_submission_to_presentation_us,
                        frame.maximum_submission_to_presentation_us,
                        frame.last_presented_frame_interval_us,
                        frame.last_refresh_interval_us
                    );
                    println!(
                        "buffer release residency: last={}us, maximum={}us, last frame={}",
                        frame.last_buffer_residency_us,
                        frame.maximum_buffer_residency_us,
                        frame.last_released_frame_id
                    );
                }
                println!("zero-copy: {}", display.zero_copy);
                println!("display detail: {}", display.detail);
            }
        }
        None => println!("state: unknown"),
    }
    Ok(())
}

fn list(paths: &WbPaths) -> Result<()> {
    let mut found = false;
    for entry in fs::read_dir(paths.machines()).context("listing machines")? {
        let entry = entry.context("reading machine directory")?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if let Ok(config) = MachineConfig::load(&entry.path()) {
            let state = RuntimeState::load(&entry.path())?
                .map(|s| format!("{:?}", s.state))
                .unwrap_or_else(|| "Unknown".into());
            println!("{}\t{}\t{}", config.name, state, config.image);
            found = true;
        }
    }
    if !found {
        println!("No machines");
    }
    Ok(())
}

fn doctor() -> Result<()> {
    let resources = ResourceLocator::discover()?;
    let wayland = wayland_socket();
    let wayland_capabilities = wayland
        .as_deref()
        .and_then(|socket| WaylandCapabilities::probe(socket).ok());
    let checks = [
        ("user namespaces", user_namespaces_available(), true),
        ("unified cgroup v2", unified_cgroup_v2_available(), true),
        ("host Wayland socket", wayland.is_some(), true),
        (
            "host linux-dmabuf",
            wayland_capabilities
                .as_ref()
                .is_some_and(|caps| caps.linux_dmabuf),
            false,
        ),
        (
            "host explicit sync",
            wayland_capabilities
                .as_ref()
                .is_some_and(|caps| caps.explicit_sync),
            false,
        ),
        (
            "host dmabuf feedback",
            wayland_capabilities
                .as_ref()
                .is_some_and(|caps| caps.dmabuf_main_device.is_some()),
            false,
        ),
        (
            "host window decorations",
            wayland_capabilities
                .as_ref()
                .is_some_and(|caps| caps.server_side_decorations),
            false,
        ),
        (
            "host fractional scaling",
            wayland_capabilities
                .as_ref()
                .is_some_and(|caps| caps.fractional_scale && caps.viewporter),
            false,
        ),
        ("host PipeWire session", host_pipewire_available(), false),
        (
            "host color management",
            wayland_capabilities
                .as_ref()
                .is_some_and(|caps| caps.color_management && caps.color_representation),
            false,
        ),
        (
            "DRM render node",
            has_matching_device("/dev/dri", "renderD"),
            true,
        ),
        ("NVIDIA device", Path::new("/dev/nvidiactl").exists(), false),
        (
            "bundled OCI helper",
            resources.helper_or_path("crane").is_ok(),
            true,
        ),
        (
            "bundled network helper",
            resources.helper_or_path("slirp4netns").is_ok(),
            true,
        ),
        (
            "bundled media helper",
            resources.helper_or_path("gst-launch-1.0").is_ok(),
            true,
        ),
        (
            "bundled sandbox helper",
            resources.helper_or_path("bwrap").is_ok(),
            true,
        ),
        (
            "bundled namespace helper",
            resources.helper_or_path("unshare").is_ok(),
            true,
        ),
        (
            "full UID/GID mapping",
            subordinate_mapping_available(&resources),
            true,
        ),
    ];
    let mut missing_required = Vec::new();
    for (name, present, required) in checks {
        println!(
            "{:>24}: {}{}",
            name,
            if present { "yes" } else { "no" },
            if required {
                " (required)"
            } else {
                " (optional)"
            }
        );
        if required && !present {
            missing_required.push(name);
        }
    }
    if let Some(capabilities) = wayland_capabilities
        && !capabilities.explicit_sync_protocols.is_empty()
    {
        println!(
            "{:>24}: {}",
            "explicit sync protocols",
            capabilities.explicit_sync_protocols.join(", ")
        );
    }
    if missing_required.is_empty() {
        Ok(())
    } else {
        bail!(
            "unsupported host: required Buzzard OS facilities are unavailable: {}",
            missing_required.join(", ")
        )
    }
}

fn host_pipewire_available() -> bool {
    use std::os::unix::fs::FileTypeExt;

    let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return false;
    };
    fs::metadata(PathBuf::from(runtime_dir).join("pipewire-0"))
        .is_ok_and(|metadata| metadata.file_type().is_socket())
}

fn unified_cgroup_v2_available() -> bool {
    Path::new("/sys/fs/cgroup/cgroup.controllers").is_file()
        && fs::read_to_string("/proc/self/cgroup")
            .is_ok_and(|description| description.lines().any(|line| line.starts_with("0::/")))
}

fn subordinate_mapping_available(resources: &ResourceLocator) -> bool {
    let Ok(unshare) = resources.helper_or_path("unshare") else {
        return false;
    };
    let Ok(id_map) = IdMap::discover() else {
        return false;
    };
    let Ok(namespace_program) = id_map.namespace_program(&unshare) else {
        return false;
    };
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    command
        .args(id_map.namespace_args())
        .arg("/bin/true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn require_machine(paths: &WbPaths, name: &str) -> Result<PathBuf> {
    MachineConfig::validate_name(name)?;
    let path = paths.machine(name);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("machine '{name}' does not exist");
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting machine '{}'", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("machine '{name}' must be a real directory, not a symlink");
    }
    let rootfs = path.join("rootfs");
    let rootfs_metadata = fs::symlink_metadata(&rootfs)
        .with_context(|| format!("inspecting machine rootfs {}", rootfs.display()))?;
    if rootfs_metadata.file_type().is_symlink() || !rootfs_metadata.is_dir() {
        bail!(
            "machine rootfs {} must be a real directory",
            rootfs.display()
        );
    }
    Ok(path)
}

fn pid_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn runtime_is_live(state: &RuntimeState, machine_dir: &Path) -> bool {
    let Some(container_pid) = state.container_pid else {
        return false;
    };
    let Some(launcher_pid) = state.launcher_pid else {
        return false;
    };
    pid_alive(container_pid)
        && pid_alive(launcher_pid)
        && broker_matches_machine(launcher_pid, machine_dir)
}

fn supervisor_is_live(state: &RuntimeState, machine_dir: &Path) -> bool {
    let Some(launcher_pid) = state.launcher_pid else {
        return false;
    };
    pid_alive(launcher_pid)
        && broker_matches_machine(launcher_pid, machine_dir)
        && host_control_socket(machine_dir).is_ok_and(|socket| {
            fs::symlink_metadata(socket).is_ok_and(|metadata| metadata.file_type().is_socket())
        })
}

fn broker_matches_machine(pid: u32, machine_dir: &Path) -> bool {
    let Ok(command_line) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let fields: Vec<&[u8]> = command_line
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect();
    let expected = machine_dir.as_os_str().as_encoded_bytes();
    fields
        .first()
        .is_some_and(|field| field.ends_with(b"wildbuzzard-broker"))
        && fields.contains(&expected)
}

fn signal_process(pid: u32, signal: i32) -> Result<()> {
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
            .with_context(|| format!("sending signal {signal} to process {pid}"))
    }
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !pid_alive(pid)
}

fn user_namespaces_available() -> bool {
    fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
        .map(|value| value.trim() != "0")
        .unwrap_or(true)
        && fs::read_to_string("/proc/sys/user/max_user_namespaces")
            .map(|value| value.trim() != "0")
            .unwrap_or(true)
}

fn wayland_socket() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let display = std::env::var_os("WAYLAND_DISPLAY")?;
    let display = PathBuf::from(display);
    let path = if display.is_absolute() {
        display
    } else {
        PathBuf::from(runtime).join(display)
    };
    path.exists().then_some(path)
}

fn has_matching_device(directory: &str, prefix: &str) -> bool {
    fs::read_dir(directory)
        .map(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(prefix))
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod layer_tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use tar::{Builder, EntryType, Header};

    struct SeedFixture {
        _temp: tempfile::TempDir,
        archive: PathBuf,
        manifest: PathBuf,
        rootfs: PathBuf,
        digest: String,
    }

    fn runtime_fixture(directory: &Path) -> PathBuf {
        runtime_fixture_for(directory, &guest_runtime_revision().unwrap())
    }

    fn runtime_fixture_for(directory: &Path, revision: &str) -> PathBuf {
        let runtime = directory.join("bundled-runtime").join(revision);
        fs::create_dir_all(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o755)).unwrap();
        let mut files = BTreeMap::new();
        for relative in REQUIRED_GUEST_RUNTIME_FILES
            .iter()
            .copied()
            .chain(["lib/libwlroots-0.20.so"])
        {
            let path = runtime.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let contents = format!("fixture:{relative}\n");
            fs::write(&path, contents.as_bytes()).unwrap();
            let mode = if relative.ends_with(".py") || relative.starts_with("lib/") {
                0o644
            } else {
                0o755
            };
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            files.insert(
                relative.to_owned(),
                GuestRuntimeFileRecord {
                    sha256: format!("{:x}", Sha256::digest(contents.as_bytes())),
                    mode,
                },
            );
        }
        for entry in walk_runtime_directories(&runtime) {
            fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let manifest = GuestRuntimeManifest {
            schema_version: 1,
            revision: revision.to_owned(),
            files,
        };
        fs::write(
            runtime.join("runtime.manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::set_permissions(
            runtime.join("runtime.manifest.json"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        runtime
    }

    fn add_runtime_readiness(runtime: &Path, revision: &str) {
        let manifest_bytes = fs::read(runtime.join("runtime.manifest.json")).unwrap();
        let readiness = GuestRuntimeReadiness {
            schema_version: 1,
            revision: revision.to_owned(),
            manifest_sha256: canonical_runtime_manifest_digest(&manifest_bytes).unwrap(),
            ready: true,
        };
        fs::write(
            runtime.join("readiness.json"),
            serde_json::to_vec(&readiness).unwrap(),
        )
        .unwrap();
        fs::set_permissions(
            runtime.join("readiness.json"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }

    fn walk_runtime_directories(root: &Path) -> Vec<PathBuf> {
        let mut directories = vec![root.to_path_buf()];
        let mut result = Vec::new();
        while let Some(directory) = directories.pop() {
            result.push(directory.clone());
            for entry in fs::read_dir(&directory).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    directories.push(entry.path());
                }
            }
        }
        result
    }

    #[test]
    fn protected_runtime_activation_is_atomic_and_retains_the_previous_revision() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let source = runtime_fixture(temp.path());
        let revision = guest_runtime_revision().unwrap();
        let runtime_root = rootfs.join(GUEST_RUNTIME_ROOT);
        fs::create_dir_all(runtime_root.join("old-revision")).unwrap();
        for directory in [
            rootfs.join("opt"),
            rootfs.join("opt/wildbuzzard"),
            runtime_root.clone(),
            runtime_root.join("old-revision"),
        ] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        symlink("old-revision", runtime_root.join("current")).unwrap();
        let uid = unsafe { libc::geteuid() };

        install_protected_guest_runtime(&rootfs, &source, &revision, uid).unwrap();

        assert_eq!(
            fs::read_link(runtime_root.join("current")).unwrap(),
            Path::new(&revision)
        );
        assert_eq!(
            fs::read_link(runtime_root.join("previous")).unwrap(),
            Path::new("old-revision")
        );
        assert!(runtime_root.join("old-revision").is_dir());
        assert!(runtime_root.join(&revision).join("bin/sway").is_file());
        assert!(
            fs::read_dir(&runtime_root)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| {
                    let name = entry.file_name();
                    name == "current"
                        || name == "previous"
                        || name == revision.as_str()
                        || name == "old-revision"
                })
        );
    }

    #[test]
    fn protected_runtime_retry_preserves_a_readiness_record() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let source = runtime_fixture(temp.path());
        let revision = guest_runtime_revision().unwrap();
        let uid = unsafe { libc::geteuid() };
        install_protected_guest_runtime(&rootfs, &source, &revision, uid).unwrap();
        let readiness = rootfs
            .join(GUEST_RUNTIME_ROOT)
            .join(&revision)
            .join("readiness.json");
        fs::write(&readiness, b"readiness evidence\n").unwrap();
        fs::set_permissions(&readiness, fs::Permissions::from_mode(0o644)).unwrap();

        install_protected_guest_runtime(&rootfs, &source, &revision, uid).unwrap();

        assert_eq!(fs::read(&readiness).unwrap(), b"readiness evidence\n");
    }

    #[test]
    fn protected_runtime_rejects_source_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let source = runtime_fixture(temp.path());
        let outside = temp.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        fs::remove_file(source.join("bin/sway")).unwrap();
        symlink(&outside, source.join("bin/sway")).unwrap();

        let error = install_protected_guest_runtime(
            &rootfs,
            &source,
            &guest_runtime_revision().unwrap(),
            unsafe { libc::geteuid() },
        )
        .unwrap_err();

        assert!(error.to_string().contains("symbolic link"));
        assert!(!rootfs.join(GUEST_RUNTIME_ROOT).exists());
    }

    #[test]
    fn protected_runtime_rejects_a_symlinked_intermediate_parent() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let outside = temp.path().join("outside");
        fs::create_dir(&rootfs).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, rootfs.join("opt")).unwrap();
        let source = runtime_fixture(temp.path());

        let error = install_protected_guest_runtime(
            &rootfs,
            &source,
            &guest_runtime_revision().unwrap(),
            unsafe { libc::geteuid() },
        )
        .unwrap_err();

        assert!(error.to_string().contains("not a real directory"));
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn protected_runtime_never_follows_hostile_staging_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let outside = temp.path().join("outside");
        fs::create_dir(&rootfs).unwrap();
        fs::create_dir(&outside).unwrap();
        let runtime_root = rootfs.join(GUEST_RUNTIME_ROOT);
        fs::create_dir_all(&runtime_root).unwrap();
        for directory in [
            rootfs.join("opt"),
            rootfs.join("opt/wildbuzzard"),
            runtime_root.clone(),
        ] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let revision = guest_runtime_revision().unwrap();
        for counter in 0..128_u32 {
            symlink(
                &outside,
                runtime_root.join(format!(
                    ".{revision}.staging.{}.{}",
                    std::process::id(),
                    counter
                )),
            )
            .unwrap();
        }
        let source = runtime_fixture(temp.path());

        let error = install_protected_guest_runtime(&rootfs, &source, &revision, unsafe {
            libc::geteuid()
        })
        .unwrap_err();

        assert!(error.to_string().contains("runtime migration intermediate"));
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
    }

    #[test]
    fn protected_runtime_discards_a_safe_interrupted_stage_and_retries() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let source = runtime_fixture(temp.path());
        let revision = guest_runtime_revision().unwrap();
        let runtime_root = rootfs.join(GUEST_RUNTIME_ROOT);
        fs::create_dir_all(&runtime_root).unwrap();
        for directory in [
            rootfs.join("opt"),
            rootfs.join("opt/wildbuzzard"),
            runtime_root.clone(),
        ] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let interrupted = runtime_root.join(format!(".{revision}.staging.interrupted"));
        fs::create_dir(&interrupted).unwrap();
        fs::set_permissions(&interrupted, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(interrupted.join("partial"), b"partial").unwrap();
        fs::set_permissions(
            interrupted.join("partial"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        install_protected_guest_runtime(&rootfs, &source, &revision, unsafe { libc::geteuid() })
            .unwrap();

        assert!(!interrupted.exists());
        assert!(runtime_root.join(revision).join("bin/sway").is_file());
    }

    #[test]
    fn protected_runtime_replaces_only_an_inactive_incomplete_revision() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let source = runtime_fixture(temp.path());
        let revision = guest_runtime_revision().unwrap();
        let uid = unsafe { libc::geteuid() };
        install_protected_guest_runtime(&rootfs, &source, &revision, uid).unwrap();
        let runtime_root = rootfs.join(GUEST_RUNTIME_ROOT);
        fs::remove_file(runtime_root.join("current")).unwrap();
        fs::write(
            runtime_root.join(&revision).join("bin/sway"),
            b"interrupted",
        )
        .unwrap();

        install_protected_guest_runtime(&rootfs, &source, &revision, uid).unwrap();

        assert_eq!(
            fs::read(runtime_root.join(&revision).join("bin/sway")).unwrap(),
            b"fixture:bin/sway\n"
        );
        assert_eq!(
            fs::read_link(runtime_root.join("current")).unwrap(),
            Path::new(&revision)
        );
    }

    #[test]
    fn protected_runtime_never_replaces_a_corrupt_active_revision() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let source = runtime_fixture(temp.path());
        let revision = guest_runtime_revision().unwrap();
        let uid = unsafe { libc::geteuid() };
        install_protected_guest_runtime(&rootfs, &source, &revision, uid).unwrap();
        let sway = rootfs
            .join(GUEST_RUNTIME_ROOT)
            .join(&revision)
            .join("bin/sway");
        fs::write(&sway, b"corrupt active payload").unwrap();

        let error = install_protected_guest_runtime(&rootfs, &source, &revision, uid).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("active protected runtime is incomplete")
        );
        assert_eq!(fs::read(&sway).unwrap(), b"corrupt active payload");
    }

    #[test]
    fn new_machine_reconciles_a_same_revision_oci_runtime_before_commit() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let revision = guest_runtime_revision().unwrap();
        let uid = unsafe { libc::geteuid() };
        let oci_source = runtime_fixture(&temp.path().join("oci"));
        install_protected_guest_runtime(&rootfs, &oci_source, &revision, uid).unwrap();

        let appimage_source = runtime_fixture(&temp.path().join("appimage"));
        let sway = appimage_source.join("bin/sway");
        fs::write(&sway, b"AppImage-built Sway runtime fixture\n").unwrap();
        let manifest_path = appimage_source.join("runtime.manifest.json");
        let mut manifest: GuestRuntimeManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.files.get_mut("bin/sway").unwrap().sha256 = format!(
            "{:x}",
            Sha256::digest(b"AppImage-built Sway runtime fixture\n")
        );
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        install_protected_guest_runtime_for_new_rootfs(&rootfs, &appimage_source, &revision, uid)
            .unwrap();

        let installed = rootfs
            .join(GUEST_RUNTIME_ROOT)
            .join(&revision)
            .join("bin/sway");
        assert_eq!(
            fs::read(installed).unwrap(),
            b"AppImage-built Sway runtime fixture\n"
        );
        assert_eq!(
            fs::read_link(rootfs.join(GUEST_RUNTIME_ROOT).join("current")).unwrap(),
            Path::new(&revision)
        );
        assert!(
            fs::read_dir(rootfs.join(GUEST_RUNTIME_ROOT))
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .contains("seed-original"))
        );
    }

    #[test]
    fn guarded_runtime_rollback_restores_only_the_ready_previous_revision() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let uid = unsafe { libc::geteuid() };
        let previous_revision = "previous-ready";
        let previous_source = runtime_fixture_for(&temp.path().join("previous"), previous_revision);
        install_protected_guest_runtime(&rootfs, &previous_source, previous_revision, uid).unwrap();
        let previous_runtime = rootfs.join(GUEST_RUNTIME_ROOT).join(previous_revision);
        add_runtime_readiness(&previous_runtime, previous_revision);

        let current_revision = guest_runtime_revision().unwrap();
        let current_source = runtime_fixture(&temp.path().join("current"));
        install_protected_guest_runtime(&rootfs, &current_source, &current_revision, uid).unwrap();

        rollback_guest_runtime(
            &rootfs,
            &current_revision,
            previous_revision,
            "desktop readiness deadline expired",
            uid,
        )
        .unwrap();

        let runtime_root = rootfs.join(GUEST_RUNTIME_ROOT);
        assert_eq!(
            fs::read_link(runtime_root.join("current")).unwrap(),
            Path::new(previous_revision)
        );
        assert!(runtime_root.join(&current_revision).is_dir());
        assert!(runtime_root.join(previous_revision).is_dir());
        let evidence = read_runtime_activation_failure(&rootfs, &current_revision, uid)
            .unwrap()
            .unwrap();
        assert_eq!(evidence.fallback_revision, previous_revision);
        assert!(
            validated_failed_runtime_fallback(&rootfs, &current_revision, previous_revision, uid,)
                .unwrap()
        );
    }

    #[test]
    fn guarded_runtime_rollback_rejects_stale_current_or_unready_previous() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let uid = unsafe { libc::geteuid() };
        let previous_revision = "previous-unready";
        let previous_source = runtime_fixture_for(&temp.path().join("previous"), previous_revision);
        install_protected_guest_runtime(&rootfs, &previous_source, previous_revision, uid).unwrap();
        let current_revision = guest_runtime_revision().unwrap();
        let current_source = runtime_fixture(&temp.path().join("current"));
        install_protected_guest_runtime(&rootfs, &current_source, &current_revision, uid).unwrap();

        let error = rollback_guest_runtime(
            &rootfs,
            &current_revision,
            previous_revision,
            "desktop readiness deadline expired",
            uid,
        )
        .unwrap_err();
        assert!(error.to_string().contains("runtime readiness"));
        assert_eq!(
            fs::read_link(rootfs.join(GUEST_RUNTIME_ROOT).join("current")).unwrap(),
            Path::new(&current_revision)
        );

        add_runtime_readiness(
            &rootfs.join(GUEST_RUNTIME_ROOT).join(previous_revision),
            previous_revision,
        );
        let stale = rollback_guest_runtime(
            &rootfs,
            "not-the-current-revision",
            previous_revision,
            "desktop readiness deadline expired",
            uid,
        )
        .unwrap_err();
        assert!(stale.to_string().contains("current changed"));
        assert_eq!(
            fs::read_link(rootfs.join(GUEST_RUNTIME_ROOT).join("current")).unwrap(),
            Path::new(&current_revision)
        );
    }

    #[test]
    fn only_the_broker_readiness_deadline_code_enables_guarded_recovery() {
        let mut state = RuntimeState::new(MachineState::Failed);
        state.detail = Some("desktop-readiness-deadline:90: nested compositor log: fixture".into());
        let deadline = state_desktop_readiness_deadline(&state).unwrap();
        assert_eq!(deadline.seconds, 90);
        assert_eq!(
            deadline.diagnostic.as_deref(),
            Some("nested compositor log: fixture")
        );

        state.detail = Some("desktop compositor did not become ready within 90 seconds".into());
        assert!(state_desktop_readiness_deadline(&state).is_none());
        state.detail = Some("desktop-readiness-deadline:9999: forged".into());
        assert!(state_desktop_readiness_deadline(&state).is_none());
    }

    #[test]
    fn compiled_guest_assets_match_the_oci_install_manifest() {
        let manifest = include_str!("../../../../guest/asset-manifest.tsv");
        let declared: BTreeMap<&str, u32> = manifest
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| {
                let mut fields = line.split('\t');
                let mode = u32::from_str_radix(fields.next().unwrap(), 8).unwrap();
                let _source = fields.next().unwrap();
                let destination = fields.next().unwrap();
                assert!(fields.next().is_none());
                (!destination.starts_with("@runtime/")).then_some((destination, mode))
            })
            .collect();
        let compiled: BTreeMap<&str, u32> = GUEST_ASSETS
            .iter()
            .map(|(destination, _contents, mode)| (*destination, *mode))
            .collect();
        assert_eq!(declared, compiled);
    }

    #[test]
    fn guest_installer_output_satisfies_seed_validator() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let binaries = temp.path().join("binaries");
        let runtime = temp.path().join("runtime");
        fs::create_dir(&rootfs).unwrap();
        fs::create_dir(&binaries).unwrap();
        fs::create_dir_all(runtime.join("bin")).unwrap();
        let shell = binaries.join("wildbuzzard-shell");
        let settings = binaries.join("wildbuzzard-settings");
        let shortcut_helper = binaries.join("wildbuzzard-shortcut-helper");
        let clipboard_agent = binaries.join("wildbuzzard-clipboard-agent");
        let cua_driver = binaries.join("cua-driver");
        for executable in [
            &shell,
            &settings,
            &shortcut_helper,
            &clipboard_agent,
            &cua_driver,
        ] {
            fs::write(executable, b"#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for executable in [runtime.join("bin/sway"), runtime.join("bin/swaymsg")] {
            fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .unwrap();
        let status = Command::new("sh")
            .arg(repository.join("guest/install-rootfs-assets.sh"))
            .arg(&rootfs)
            .arg(&shell)
            .arg(&settings)
            .arg(&shortcut_helper)
            .arg(&clipboard_agent)
            .arg(&cua_driver)
            .arg(&runtime)
            .status()
            .unwrap();
        assert!(status.success());
        for required in ["lib/systemd/systemd", "var/lib/dpkg/status"] {
            let destination = rootfs.join(required);
            fs::create_dir_all(destination.parent().unwrap()).unwrap();
            fs::write(destination, b"fixture\n").unwrap();
        }

        validate_extracted_rootfs(&rootfs).unwrap();
    }

    #[test]
    fn old_guest_settings_runtime_is_boot_safe_and_diagnostic_only() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let user_state = rootfs.join("home/wildbuzzard/important.txt");
        fs::create_dir_all(user_state.parent().unwrap()).unwrap();
        fs::write(&user_state, b"preserve me").unwrap();

        let diagnostics = guest_settings_runtime_diagnostics(&rootfs).unwrap();

        assert!(!diagnostics.is_empty());
        assert!(
            diagnostics
                .iter()
                .all(|message| message.contains("bootable"))
        );
        assert!(
            diagnostics
                .iter()
                .any(|message| message.contains("sudo apt install"))
        );
        assert_eq!(fs::read(&user_state).unwrap(), b"preserve me");
    }

    fn header(entry_type: EntryType, mode: u32, size: u64) -> Header {
        let mut header = Header::new_gnu();
        header.set_entry_type(entry_type);
        header.set_mode(mode);
        header.set_uid(u64::from(unsafe { libc::geteuid() }));
        header.set_gid(u64::from(unsafe { libc::getegid() }));
        header.set_mtime(1);
        header.set_size(size);
        header
    }

    fn append_file(builder: &mut Builder<File>, path: &str, contents: &[u8], mode: u32) {
        let mut header = header(EntryType::Regular, mode, contents.len() as u64);
        header.set_path(path).unwrap();
        header.set_cksum();
        builder.append(&header, Cursor::new(contents)).unwrap();
    }

    fn append_directory(builder: &mut Builder<File>, path: &str, mode: u32) {
        let mut header = header(EntryType::Directory, mode, 0);
        header.set_path(path).unwrap();
        header.set_cksum();
        builder.append(&header, std::io::empty()).unwrap();
    }

    fn append_link(builder: &mut Builder<File>, kind: EntryType, path: &str, target: &Path) {
        let mut header = header(kind, 0o777, 0);
        builder.append_link(&mut header, path, target).unwrap();
    }

    fn layer_file(temp: &tempfile::TempDir, build: impl FnOnce(&mut Builder<File>)) -> PathBuf {
        let layer = temp.path().join("layer.tar");
        let mut builder = Builder::new(File::create(&layer).unwrap());
        build(&mut builder);
        builder.finish().unwrap();
        drop(builder);
        layer
    }

    fn seed_fixture(build: impl FnOnce(&mut Builder<File>)) -> SeedFixture {
        let temp = tempfile::tempdir().unwrap();
        let runtime = temp.path().join("runtime");
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&runtime).unwrap();
        fs::create_dir(&rootfs).unwrap();
        let tar_path = temp.path().join("rootfs.tar");
        let mut builder = Builder::new(File::create(&tar_path).unwrap());
        let revision = guest_runtime_revision().unwrap();
        for required in [
            "bin/sway",
            "bin/swaymsg",
            "bin/cua-driver",
            "libexec/wildbuzzard-clipboard-agent",
            "libexec/wildbuzzard-settings",
            "libexec/wildbuzzard-shell",
        ] {
            append_file(
                &mut builder,
                &format!("opt/wildbuzzard/runtime/{revision}/{required}"),
                b"fixture",
                0o755,
            );
        }
        append_file(&mut builder, "lib/systemd/systemd", b"fixture", 0o755);
        append_file(
            &mut builder,
            "usr/libexec/wildbuzzard-shortcut-helper",
            b"fixture",
            0o755,
        );
        append_file(&mut builder, "var/lib/dpkg/status", b"fixture", 0o644);
        append_link(
            &mut builder,
            EntryType::Symlink,
            "opt/wildbuzzard/runtime/current",
            Path::new(&revision),
        );
        build(&mut builder);
        builder.finish().unwrap();
        drop(builder);

        let uncompressed = fs::read(&tar_path).unwrap();
        let compressed = zstd::stream::encode_all(Cursor::new(&uncompressed), 1).unwrap();
        let archive = runtime.join(ROOTFS_SEED_ARCHIVE);
        let manifest = runtime.join(ROOTFS_SEED_MANIFEST);
        fs::write(&archive, &compressed).unwrap();
        let compressed_hash = format!("{:x}", Sha256::digest(&compressed));
        let uncompressed_hash = format!("{:x}", Sha256::digest(&uncompressed));
        let record = serde_json::json!({
            "schema": ROOTFS_SEED_SCHEMA,
            "kind": ROOTFS_SEED_KIND,
            "platform": {"os": "linux", "architecture": "amd64"},
            "archive": {
                "name": ROOTFS_SEED_ARCHIVE,
                "media_type": ROOTFS_SEED_MEDIA_TYPE,
                "size": compressed.len(),
                "sha256": compressed_hash,
                "uncompressed_size": uncompressed.len(),
                "uncompressed_sha256": uncompressed_hash,
            }
        });
        fs::write(&manifest, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        SeedFixture {
            _temp: temp,
            archive,
            manifest,
            rootfs,
            digest: format!("sha256:{compressed_hash}"),
        }
    }

    #[test]
    fn create_without_image_requires_the_full_portable_bundle() {
        let temp = tempfile::tempdir().unwrap();
        let paths = WbPaths::discover(Some(temp.path())).unwrap();
        paths.ensure().unwrap();
        let error = create(
            &paths,
            "seedless",
            None,
            NetworkMode::User,
            vec!["all".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("app/runtime/default-rootfs.oci.tar.zst is missing"));
        assert!(!paths.machine("seedless").exists());
    }

    #[test]
    fn flat_seed_preserves_hardlinks_modes_mtime_and_xattrs() {
        let fixture = seed_fixture(|builder| {
            append_directory(builder, "opt", 0o750);
            builder
                .append_pax_extensions([
                    ("mtime", b"123.456789123".as_slice()),
                    ("SCHILY.xattr.user.wildbuzzard", b"kept".as_slice()),
                ])
                .unwrap();
            append_file(builder, "opt/original", b"persistent", 0o6750);
            append_link(
                builder,
                EntryType::Link,
                "opt/hardlink",
                Path::new("opt/original"),
            );
            append_link(
                builder,
                EntryType::Symlink,
                "opt/symlink",
                Path::new("original"),
            );
        });

        apply_rootfs_seed(
            &fixture.archive,
            &fixture.manifest,
            &fixture.digest,
            &fixture.rootfs,
        )
        .unwrap();

        let original = fs::symlink_metadata(fixture.rootfs.join("opt/original")).unwrap();
        let hardlink = fs::symlink_metadata(fixture.rootfs.join("opt/hardlink")).unwrap();
        assert_eq!(original.ino(), hardlink.ino());
        assert_eq!(original.permissions().mode() & 0o7777, 0o6750);
        assert_eq!(original.mtime(), 123);
        assert_eq!(original.mtime_nsec(), 456_789_123);
        assert_eq!(
            fs::read_link(fixture.rootfs.join("opt/symlink")).unwrap(),
            Path::new("original")
        );
        let path =
            CString::new(fixture.rootfs.join("opt/original").as_os_str().as_bytes()).unwrap();
        let mut value = [0_u8; 16];
        let length = unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                c"user.wildbuzzard".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        assert_eq!(length, 4);
        assert_eq!(&value[..4], b"kept");
    }

    #[test]
    fn flat_seed_rejects_digest_mismatch_before_extraction() {
        let fixture = seed_fixture(|_| {});
        let mut bytes = fs::read(&fixture.archive).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&fixture.archive, bytes).unwrap();

        let error = apply_rootfs_seed(
            &fixture.archive,
            &fixture.manifest,
            &fixture.digest,
            &fixture.rootfs,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("archive digest mismatch"));
        assert!(fs::read_dir(&fixture.rootfs).unwrap().next().is_none());
    }

    #[test]
    fn flat_seed_rejects_parent_hardlinks_whiteouts_and_unmapped_ids() {
        let hardlink = seed_fixture(|builder| {
            append_link(builder, EntryType::Link, "escape", Path::new("../outside"));
        });
        assert!(
            apply_rootfs_seed(
                &hardlink.archive,
                &hardlink.manifest,
                &hardlink.digest,
                &hardlink.rootfs,
            )
            .is_err()
        );

        let whiteout = seed_fixture(|builder| {
            append_file(builder, "etc/.wh.forbidden", b"", 0o000);
        });
        assert!(
            apply_rootfs_seed(
                &whiteout.archive,
                &whiteout.manifest,
                &whiteout.digest,
                &whiteout.rootfs,
            )
            .is_err()
        );

        let unmapped = seed_fixture(|builder| {
            let mut header = header(EntryType::Regular, 0o644, 1);
            header.set_uid(MAX_GUEST_ID + 1);
            header.set_path("unsupported-owner").unwrap();
            header.set_cksum();
            builder.append(&header, Cursor::new(b"x")).unwrap();
        });
        assert!(
            apply_rootfs_seed(
                &unmapped.archive,
                &unmapped.manifest,
                &unmapped.digest,
                &unmapped.rootfs,
            )
            .is_err()
        );
    }

    #[test]
    fn bundled_seed_manifest_rejects_wrong_platform_and_symlink_files() {
        let fixture = seed_fixture(|_| {});
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&fixture.manifest).unwrap()).unwrap();
        manifest["platform"]["architecture"] = serde_json::json!("arm64");
        fs::write(&fixture.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(read_rootfs_seed_manifest(&fixture.manifest).is_err());

        let target = fixture.archive.with_extension("actual");
        fs::rename(&fixture.archive, &target).unwrap();
        std::os::unix::fs::symlink(&target, &fixture.archive).unwrap();
        assert!(open_regular_nofollow(&fixture.archive, "test archive").is_err());
    }

    #[test]
    fn applies_whiteouts_opaque_directories_and_new_content() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("etc/opaque")).unwrap();
        fs::write(rootfs.join("etc/remove-me"), b"lower").unwrap();
        fs::write(rootfs.join("etc/opaque/lower-a"), b"a").unwrap();
        fs::write(rootfs.join("etc/opaque/lower-b"), b"b").unwrap();
        let layer = layer_file(&temp, |builder| {
            append_file(builder, "etc/.wh.remove-me", b"", 0o000);
            append_file(builder, "etc/opaque/.wh..wh..opq", b"", 0o000);
            append_file(builder, "etc/opaque/upper", b"upper", 0o640);
        });

        apply_layer(&layer, &rootfs).unwrap();

        assert!(!rootfs.join("etc/remove-me").exists());
        assert!(!rootfs.join("etc/opaque/lower-a").exists());
        assert!(!rootfs.join("etc/opaque/lower-b").exists());
        assert_eq!(fs::read(rootfs.join("etc/opaque/upper")).unwrap(), b"upper");
        assert_eq!(
            fs::metadata(rootfs.join("etc/opaque/upper"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o640
        );
    }

    #[test]
    fn rejects_a_whiteout_without_a_target() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("etc")).unwrap();
        fs::write(rootfs.join("etc/must-survive"), b"lower").unwrap();
        let layer = layer_file(&temp, |builder| {
            append_file(builder, "etc/.wh.", b"", 0o000);
        });

        let error = apply_layer(&layer, &rootfs).unwrap_err();

        assert!(error.to_string().contains("whiteout has no target"));
        assert_eq!(fs::read(rootfs.join("etc/must-survive")).unwrap(), b"lower");
    }

    #[test]
    fn preserves_hardlinks_symlinks_modes_ownership_and_xattrs() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let layer = layer_file(&temp, |builder| {
            append_directory(builder, "opt", 0o750);
            builder
                .append_pax_extensions([("SCHILY.xattr.user.wildbuzzard", b"kept".as_slice())])
                .unwrap();
            append_file(builder, "opt/original", b"persistent", 0o6750);
            append_link(
                builder,
                EntryType::Link,
                "opt/hardlink",
                Path::new("opt/original"),
            );
            append_link(
                builder,
                EntryType::Symlink,
                "opt/symlink",
                Path::new("original"),
            );
        });

        apply_layer(&layer, &rootfs).unwrap();

        let original = fs::symlink_metadata(rootfs.join("opt/original")).unwrap();
        let hardlink = fs::symlink_metadata(rootfs.join("opt/hardlink")).unwrap();
        assert_eq!(original.ino(), hardlink.ino());
        assert_eq!(original.uid(), unsafe { libc::geteuid() });
        assert_eq!(original.gid(), unsafe { libc::getegid() });
        assert_eq!(original.permissions().mode() & 0o7777, 0o6750);
        assert_eq!(
            fs::read_link(rootfs.join("opt/symlink")).unwrap(),
            Path::new("original")
        );

        let path =
            std::ffi::CString::new(rootfs.join("opt/original").as_os_str().as_bytes().to_vec())
                .unwrap();
        let name = c"user.wildbuzzard";
        let mut value = [0_u8; 16];
        let length = unsafe {
            libc::lgetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        assert_eq!(length, 4);
        assert_eq!(&value[..4], b"kept");
    }

    #[test]
    fn rejects_symlink_ancestor_escape() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let outside = temp.path().join("outside");
        fs::create_dir(&rootfs).unwrap();
        fs::create_dir(&outside).unwrap();
        let layer = layer_file(&temp, |builder| {
            append_link(builder, EntryType::Symlink, "escape", &outside);
            append_file(builder, "escape/pwned", b"no", 0o644);
        });

        assert!(apply_layer(&layer, &rootfs).is_err());
        assert!(!outside.join("pwned").exists());
    }

    #[test]
    fn rejects_hardlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let outside = temp.path().join("outside");
        fs::create_dir(&rootfs).unwrap();
        fs::write(&outside, b"outside").unwrap();
        let layer = layer_file(&temp, |builder| {
            append_link(
                builder,
                EntryType::Link,
                "escaped-hardlink",
                Path::new("../outside"),
            );
        });

        assert!(apply_layer(&layer, &rootfs).is_err());
        assert!(!rootfs.join("escaped-hardlink").exists());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn rejects_whiteout_through_symlink_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let outside = temp.path().join("outside");
        fs::create_dir(&rootfs).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("victim"), b"safe").unwrap();
        std::os::unix::fs::symlink(&outside, rootfs.join("escape")).unwrap();
        let layer = layer_file(&temp, |builder| {
            append_file(builder, "escape/.wh.victim", b"", 0o000);
        });

        assert!(apply_layer(&layer, &rootfs).is_err());
        assert_eq!(fs::read(outside.join("victim")).unwrap(), b"safe");
    }

    #[test]
    fn rejects_unsafe_archive_paths() {
        assert!(safe_relative_path(Path::new("../escape")).is_err());
        assert!(safe_relative_path(Path::new("/absolute")).is_err());
        assert_eq!(safe_relative_path(Path::new(".")).unwrap(), Path::new(""));
        assert_eq!(
            safe_relative_path(Path::new("safe/path")).unwrap(),
            Path::new("safe/path")
        );
    }

    #[test]
    fn installs_versioned_sway_guest_assets() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        fs::create_dir_all(rootfs.join("usr/share/dbus-1/services")).unwrap();
        fs::write(
            rootfs.join("usr/share/dbus-1/services/org.kde.kwalletd6.service"),
            b"[D-BUS Service]\nName=org.kde.kwalletd6\nExec=/usr/bin/kwalletd6\n",
        )
        .unwrap();

        // Static session integration is unit-tested here. The compiled native
        // shell is installed from the sibling AppImage binary during a real
        // machine creation.
        install_guest_assets_without_shell(&rootfs).unwrap();

        let sway_config = fs::read_to_string(rootfs.join("etc/wildbuzzard/sway-config")).unwrap();
        assert!(sway_config.contains("for_window [all] floating enable, border normal 8"));
        assert!(sway_config.contains("titlebar_padding 6 7"));
        assert!(sway_config.contains("client.focused #30343a #30343a #f2f2f2 #ff7139"));
        assert!(sway_config.contains(
            "bindsym button3 focus, exec --no-startup-id \
             /opt/wildbuzzard/runtime/current/libexec/wildbuzzard-shell \
             --request-focused-window-menu"
        ));
        assert!(sway_config.contains("workspace 1"));
        assert!(sway_config.contains("wildbuzzard-desktop-services"));
        assert!(!sway_config.contains("waybar"));
        assert!(!sway_config.contains("fuzzel"));
        assert_eq!(
            fs::read_to_string(rootfs.join("usr/lib/wildbuzzard/guest-assets.version")).unwrap(),
            GUEST_ASSETS_REVISION
        );
        let desktop_services =
            include_str!("../../../../guest/assets/wildbuzzard-desktop-services");
        assert!(desktop_services.contains("wildbuzzard-output-sync"));
        assert!(desktop_services.contains("$runtime/libexec/wildbuzzard-shell"));
        let integration_agent =
            include_str!("../../../../guest/assets/wildbuzzard-integration-agent");
        assert!(integration_agent.contains("media.class=Video/Source,media.role=Camera"));
        assert!(
            integration_agent
                .contains("pipewiresink\", \"mode=provide\",\n                \"async=false")
        );
        assert!(
            !rootfs
                .join("usr/local/bin/wildbuzzard-window-control")
                .exists()
        );
        let session = include_str!("../../../../guest/assets/wildbuzzard-session");
        assert!(session.contains("XDG_CURRENT_DESKTOP=sway"));
        assert!(
            fs::read_to_string(rootfs.join("usr/lib/systemd/system/wildbuzzard-desktop.service"))
                .unwrap()
                .contains("EnvironmentFile=/run/wildbuzzard-host/driver.env")
        );
        assert!(!rootfs.join("etc/chromium.d/wildbuzzard").exists());
        assert!(!rootfs.join("etc/chromium/master_preferences").exists());
        assert!(
            fs::read_to_string(rootfs.join("etc/xdg/kwalletrc"))
                .unwrap()
                .contains("Enabled=false")
        );
        assert!(
            fs::read_to_string(rootfs.join("etc/gtk-3.0/settings.ini"))
                .unwrap()
                .contains("gtk-theme-name=WildBuzzard-Dark")
        );
        let dark_gtk3 =
            fs::read_to_string(rootfs.join("usr/share/themes/WildBuzzard-Dark/gtk-3.0/gtk.css"))
                .unwrap();
        let light_gtk3 =
            fs::read_to_string(rootfs.join("usr/share/themes/WildBuzzard-Light/gtk-3.0/gtk.css"))
                .unwrap();
        assert_eq!(dark_gtk3, light_gtk3);
        assert!(dark_gtk3.contains("WildBuzzard-Shared/gtk-3.0/geometry.css"));
        let gtk3_geometry = fs::read_to_string(
            rootfs.join("usr/share/themes/WildBuzzard-Shared/gtk-3.0/geometry.css"),
        )
        .unwrap();
        assert!(gtk3_geometry.contains(".sidebar .view:selected"));
        assert!(gtk3_geometry.contains("color: @wb_selected_text"));
        assert!(!gtk3_geometry.contains("color: #ffffff"));
        assert!(
            fs::read_to_string(rootfs.join("usr/share/icons/WildBuzzard/index.theme"))
                .unwrap()
                .contains("Inherits=Adwaita,hicolor")
        );
        assert!(
            fs::read_to_string(rootfs.join("etc/wildbuzzard/xdg/kdeglobals"))
                .unwrap()
                .contains("ColorScheme=WildBuzzard-Dark")
        );
        assert!(session.contains("file:///shared Shared"));
        assert!(!session.contains("gsettings set org.gnome.desktop.interface color-scheme"));
        assert!(!session.contains("gsettings set org.gnome.desktop.interface gtk-theme"));
        assert!(
            !rootfs
                .join("usr/share/dbus-1/services/org.kde.kwalletd6.service")
                .exists()
        );
        assert_eq!(
            fs::metadata(rootfs.join("etc/sudoers.d/90-wildbuzzard"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o440
        );
    }

    #[test]
    fn fresh_install_includes_discoverable_application_icons() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();

        install_guest_assets_without_shell(&rootfs).unwrap();

        let application_icons: &[(&str, &[u8])] = &[
            (
                "usr/share/icons/WildBuzzard/scalable/apps/wildbuzzard.svg",
                include_bytes!(
                    "../../../../guest/assets/icons/WildBuzzard/scalable/apps/wildbuzzard.svg"
                ),
            ),
            (
                "usr/share/icons/WildBuzzard/scalable/apps/wildbuzzard-settings.svg",
                include_bytes!(
                    "../../../../guest/assets/icons/WildBuzzard/scalable/apps/wildbuzzard-settings.svg"
                ),
            ),
            (
                "usr/share/icons/WildBuzzard/symbolic/apps/wildbuzzard-symbolic.svg",
                include_bytes!(
                    "../../../../guest/assets/icons/WildBuzzard/symbolic/apps/wildbuzzard-symbolic.svg"
                ),
            ),
            (
                "usr/share/icons/WildBuzzard/symbolic/apps/wildbuzzard-settings-symbolic.svg",
                include_bytes!(
                    "../../../../guest/assets/icons/WildBuzzard/symbolic/apps/wildbuzzard-settings-symbolic.svg"
                ),
            ),
        ];
        for (relative, expected) in application_icons {
            let destination = rootfs.join(relative);
            assert_eq!(fs::read(&destination).unwrap(), *expected, "{relative}");
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o7777,
                0o644,
                "{relative}"
            );
        }

        let icon_theme =
            fs::read_to_string(rootfs.join("usr/share/icons/WildBuzzard/index.theme")).unwrap();
        let directories = icon_theme
            .lines()
            .find_map(|line| line.strip_prefix("Directories="))
            .unwrap()
            .split(',')
            .collect::<std::collections::BTreeSet<_>>();
        for directory in ["scalable/apps", "symbolic/apps"] {
            assert!(directories.contains(directory), "missing {directory}");
            let section = format!("[{directory}]");
            let body = icon_theme
                .split(&section)
                .nth(1)
                .unwrap_or_else(|| panic!("missing {section}"))
                .split("\n[")
                .next()
                .unwrap();
            assert!(body.lines().any(|line| line == "Type=Scalable"));
            assert!(body.lines().any(|line| line == "Context=Applications"));
        }
    }

    #[test]
    fn a_revision_without_the_bundled_manifest_is_not_current() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("usr/lib/wildbuzzard")).unwrap();
        fs::create_dir_all(rootfs.join("etc/wildbuzzard")).unwrap();
        fs::write(
            rootfs.join("usr/lib/wildbuzzard/guest-assets.version"),
            GUEST_ASSETS_REVISION,
        )
        .unwrap();
        fs::write(
            rootfs.join("etc/wildbuzzard/sway-config"),
            b"guest-owned configuration",
        )
        .unwrap();

        assert!(read_guest_asset_manifest(&rootfs).is_none());
        assert_eq!(
            fs::read(rootfs.join("etc/wildbuzzard/sway-config")).unwrap(),
            b"guest-owned configuration"
        );
    }

    #[test]
    fn migration_updates_an_unmodified_distributed_asset() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let relative = Path::new("etc/wildbuzzard/test.conf");
        install_guest_asset(&rootfs, relative, b"old distributed value", 0o644).unwrap();
        let previous = guest_asset_record(b"old distributed value", 0o644);

        migrate_guest_asset(
            &rootfs,
            relative,
            b"new distributed value",
            0o600,
            Some(&previous),
            None,
        )
        .unwrap();

        let destination = rootfs.join(relative);
        assert_eq!(fs::read(&destination).unwrap(), b"new distributed value");
        assert_eq!(
            fs::metadata(destination).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }

    #[test]
    fn migration_preserves_a_guest_modified_asset() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let relative = Path::new("etc/wildbuzzard/test.conf");
        install_guest_asset(&rootfs, relative, b"guest modified value", 0o644).unwrap();
        let previous = guest_asset_record(b"old distributed value", 0o644);

        migrate_guest_asset(
            &rootfs,
            relative,
            b"new distributed value",
            0o600,
            Some(&previous),
            None,
        )
        .unwrap();

        let destination = rootfs.join(relative);
        assert_eq!(fs::read(&destination).unwrap(), b"guest modified value");
        assert_eq!(
            fs::metadata(destination).unwrap().permissions().mode() & 0o7777,
            0o644
        );
    }

    #[test]
    fn migration_removes_only_empty_nvidia_mount_placeholders() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let etc_icd = rootfs.join("etc/vulkan/icd.d/nvidia_icd.json");
        let usr_icd = rootfs.join("usr/share/vulkan/icd.d/nvidia_icd.json");
        fs::create_dir_all(etc_icd.parent().unwrap()).unwrap();
        fs::create_dir_all(usr_icd.parent().unwrap()).unwrap();
        fs::write(&etc_icd, b"").unwrap();
        fs::write(&usr_icd, b"{\"guest\":\"configured\"}\n").unwrap();

        remove_empty_nvidia_mount_placeholders(&rootfs).unwrap();

        assert!(!etc_icd.exists());
        assert_eq!(fs::read(&usr_icd).unwrap(), b"{\"guest\":\"configured\"}\n");
    }

    #[test]
    fn legacy_migration_preserves_existing_assets_and_installs_missing_ones() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let existing = Path::new("etc/wildbuzzard/existing.conf");
        let missing = Path::new("etc/wildbuzzard/missing.conf");
        install_guest_asset(&rootfs, existing, b"legacy guest value", 0o640).unwrap();

        migrate_guest_asset(&rootfs, existing, b"distributed", 0o644, None, None).unwrap();
        migrate_guest_asset(&rootfs, missing, b"new file", 0o600, None, None).unwrap();

        assert_eq!(
            fs::read(rootfs.join(existing)).unwrap(),
            b"legacy guest value"
        );
        assert_eq!(fs::read(rootfs.join(missing)).unwrap(), b"new file");
    }

    #[test]
    fn migration_replaces_a_recognized_unmanifested_legacy_asset() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let relative = Path::new("etc/wildbuzzard/legacy.conf");
        install_guest_asset(&rootfs, relative, b"recognized legacy value", 0o644).unwrap();
        let recognized_legacy = guest_asset_record(b"recognized legacy value", 0o644);

        migrate_guest_asset(
            &rootfs,
            relative,
            b"new distributed value",
            0o644,
            None,
            Some(&recognized_legacy),
        )
        .unwrap();

        assert_eq!(
            fs::read(rootfs.join(relative)).unwrap(),
            b"new distributed value"
        );
    }

    #[test]
    fn migration_removes_unchanged_retired_asset_and_preserves_guest_edit() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let unchanged = Path::new("etc/wildbuzzard/retired.conf");
        let edited = Path::new("etc/wildbuzzard/edited-retired.conf");
        let previous = guest_asset_record(b"managed value", 0o644);
        install_guest_asset(&rootfs, unchanged, b"managed value", 0o644).unwrap();
        install_guest_asset(&rootfs, edited, b"guest value", 0o644).unwrap();

        remove_retired_guest_asset(&rootfs, unchanged, Some(&previous)).unwrap();
        remove_retired_guest_asset(&rootfs, edited, Some(&previous)).unwrap();

        assert!(!rootfs.join(unchanged).exists());
        assert_eq!(fs::read(rootfs.join(edited)).unwrap(), b"guest value");
    }

    #[test]
    fn guest_asset_install_rejects_symlink_parent_escape() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let outside = temp.path().join("outside");
        fs::create_dir(&rootfs).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(rootfs.join("usr")).unwrap();
        std::os::unix::fs::symlink(&outside, rootfs.join("usr/libexec")).unwrap();

        assert!(install_guest_assets(&rootfs).is_err());
        assert!(!outside.join("wildbuzzard-init").exists());
    }

    #[test]
    fn machine_commit_is_atomic_and_never_replaces_an_existing_directory() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join(".machine-creating");
        let destination = temp.path().join("machine");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("complete"), b"new machine").unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("existing"), b"keep").unwrap();

        let error = commit_new_machine(&staging, &destination).unwrap_err();

        assert!(error.to_string().contains("was not replaced"));
        assert_eq!(fs::read(destination.join("existing")).unwrap(), b"keep");
        assert_eq!(fs::read(staging.join("complete")).unwrap(), b"new machine");

        fs::remove_dir_all(&destination).unwrap();
        commit_new_machine(&staging, &destination).unwrap();
        assert!(!staging.exists());
        assert_eq!(
            fs::read(destination.join("complete")).unwrap(),
            b"new machine"
        );
    }

    #[test]
    fn failed_machine_staging_cleanup_is_confined_to_the_machine_directory() {
        let temp = tempfile::tempdir().unwrap();
        let machines = temp.path().join("vm");
        let staging = machines.join(".machine-creating-fixture");
        fs::create_dir(&machines).unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("partial"), b"partial machine").unwrap();

        remove_machine_staging_tree(&staging, &machines).unwrap();

        assert!(!staging.exists());
        assert!(machines.is_dir());
    }

    #[test]
    fn failed_machine_import_staging_cleanup_is_confined_to_the_machine_directory() {
        let temp = tempfile::tempdir().unwrap();
        let machines = temp.path().join("Machines");
        let staging = machines.join(".roundtrip-importing-EMwCQb");
        fs::create_dir(&machines).unwrap();
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("partial"), b"partial import").unwrap();

        remove_machine_staging_tree(&staging, &machines).unwrap();

        assert!(!staging.exists());
        assert!(machines.is_dir());
    }

    #[test]
    fn local_oci_source_reference_never_persists_its_absolute_directory() {
        assert_eq!(
            local_oci_source_reference(Path::new("/host/Downloads/machine.oci.tar.zst")),
            "oci-import:machine.oci.tar.zst"
        );
        assert_eq!(
            local_oci_source_reference(Path::new("machine-layout")),
            "oci-import:machine-layout"
        );
        assert_eq!(
            local_oci_source_reference(Path::new("/")),
            "oci-import:local-oci"
        );
    }

    #[test]
    fn import_mode_defaults_to_restore_and_accepts_explicit_clone() {
        let restore = Cli::try_parse_from([
            "BuzzardOS",
            "import",
            "machine.oci.tar.zst",
            "--name",
            "restored",
        ])
        .unwrap();
        assert!(matches!(
            restore.command,
            Some(Commands::Import {
                mode: ImportModeArg::Restore,
                ..
            })
        ));

        let clone = Cli::try_parse_from([
            "BuzzardOS",
            "import",
            "machine.oci.tar.zst",
            "--name",
            "copy",
            "--mode",
            "clone",
        ])
        .unwrap();
        assert!(matches!(
            clone.command,
            Some(Commands::Import {
                mode: ImportModeArg::Clone,
                ..
            })
        ));
    }

    #[test]
    fn public_export_cannot_request_a_generic_seed() {
        let parsed = Cli::try_parse_from([
            "BuzzardOS",
            "export",
            "fixture",
            "--output",
            "fixture.oci.tar.zst",
            "--generic-seed",
        ]);

        assert!(parsed.is_err());
    }

    #[test]
    fn restore_identity_check_and_commit_use_one_portable_registry_lock() {
        let temp = tempfile::tempdir().unwrap();
        let paths = WbPaths::discover(Some(temp.path())).unwrap();
        paths.ensure().unwrap();
        let first = lock_machine_identity_registry(&paths).unwrap();
        let second = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(paths.machines())
            .unwrap();

        assert_eq!(
            second.try_lock_exclusive().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(first);
        second.try_lock_exclusive().unwrap();
    }

    #[test]
    fn clone_identity_reset_creates_a_missing_machine_id_and_removes_secrets() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("etc/ssh")).unwrap();
        fs::create_dir_all(rootfs.join("var/lib/systemd")).unwrap();
        fs::write(rootfs.join("etc/ssh/ssh_host_ed25519_key"), b"secret").unwrap();
        fs::write(rootfs.join("var/lib/systemd/random-seed"), b"seed").unwrap();

        reset_cloned_rootfs_identity(&rootfs).unwrap();

        let machine_id = rootfs.join("etc/machine-id");
        assert_eq!(fs::read(&machine_id).unwrap(), b"");
        assert_eq!(
            fs::metadata(&machine_id).unwrap().permissions().mode() & 0o777,
            0o444
        );
        assert!(!rootfs.join("etc/ssh/ssh_host_ed25519_key").exists());
        assert!(!rootfs.join("var/lib/systemd/random-seed").exists());
    }

    #[test]
    fn clone_identity_reset_rejects_a_machine_id_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir_all(rootfs.join("etc")).unwrap();
        fs::write(temp.path().join("outside"), b"keep").unwrap();
        symlink(temp.path().join("outside"), rootfs.join("etc/machine-id")).unwrap();

        assert!(reset_cloned_rootfs_identity(&rootfs).is_err());
        assert_eq!(fs::read(temp.path().join("outside")).unwrap(), b"keep");
    }

    #[test]
    fn failed_machine_staging_cleanup_rejects_unrelated_paths() {
        let temp = tempfile::tempdir().unwrap();
        let machines = temp.path().join("vm");
        let unrelated = machines.join("machine");
        fs::create_dir(&machines).unwrap();
        fs::create_dir(&unrelated).unwrap();

        let error = remove_machine_staging_tree(&unrelated, &machines).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("not a machine create/import staging")
        );
        assert!(unrelated.is_dir());
    }

    #[test]
    fn export_staging_cleanup_is_confined_to_the_portable_cache() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let staging = cache.join("oci-export-Ab19zQ");
        fs::create_dir_all(staging.join("layout/blobs/sha256")).unwrap();
        fs::write(staging.join("layout/index.json"), b"partial export").unwrap();

        remove_export_staging_tree(&staging, &cache).unwrap();

        assert!(!staging.exists());
        assert!(cache.is_dir());
    }

    #[test]
    fn export_staging_cleanup_rejects_unrelated_cache_paths() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let unrelated = cache.join("important-data");
        fs::create_dir_all(&unrelated).unwrap();

        let error = remove_export_staging_tree(&unrelated, &cache).unwrap_err();

        assert!(error.to_string().contains("not an OCI export staging"));
        assert!(unrelated.is_dir());
    }

    #[test]
    fn export_staging_cleanup_rejects_valid_name_outside_cache() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let outside = temp.path().join("outside");
        let staging = outside.join("oci-export-Ab19zQ");
        fs::create_dir(&cache).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::create_dir(&staging).unwrap();

        let error = remove_export_staging_tree(&staging, &cache).unwrap_err();

        assert!(error.to_string().contains("outside the expected cache"));
        assert!(staging.is_dir());
    }

    #[test]
    fn oci_export_round_trip_preserves_files_links_xattrs_and_machine_annotation() {
        let temp = tempfile::tempdir().unwrap();
        let machine = temp.path().join("machine");
        let rootfs = machine.join("rootfs");
        let work = temp.path().join("work");
        fs::create_dir_all(rootfs.join("opt/data")).unwrap();
        for ephemeral in ["proc", "sys", "dev", "run", "tmp", "shared"] {
            fs::create_dir_all(rootfs.join(ephemeral)).unwrap();
            fs::write(rootfs.join(ephemeral).join("must-not-export"), ephemeral).unwrap();
        }
        fs::create_dir(&work).unwrap();
        fs::write(rootfs.join("opt/data/original"), b"portable state").unwrap();
        fs::hard_link(
            rootfs.join("opt/data/original"),
            rootfs.join("opt/data/hardlink"),
        )
        .unwrap();
        std::os::unix::fs::symlink("original", rootfs.join("opt/data/symlink")).unwrap();
        fs::set_permissions(rootfs.join("opt/data"), fs::Permissions::from_mode(0o2750)).unwrap();
        set_link_mtime(
            &rootfs.join("opt/data/original"),
            (1_700_000_001, 234_567_890),
        )
        .unwrap();
        set_link_mtime(&rootfs.join("opt/data"), (1_700_000_000, 123_456_789)).unwrap();
        let xattr_name = c"user.buzzardos";
        let xattr_value = b"kept";
        let xattr_path =
            CString::new(rootfs.join("opt/data/original").as_os_str().as_bytes()).unwrap();
        let result = unsafe {
            libc::setxattr(
                xattr_path.as_ptr(),
                xattr_name.as_ptr(),
                xattr_value.as_ptr().cast(),
                xattr_value.len(),
                0,
            )
        };
        if result != 0 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOTSUP)
            );
        }
        let acl_name = c"system.posix_acl_access";
        let mut acl_value = Vec::new();
        acl_value.extend_from_slice(&2_u32.to_ne_bytes());
        for (tag, permissions, id) in [
            (0x01_u16, 0x07_u16, u32::MAX),
            (0x02_u16, 0x05_u16, 1234_u32),
            (0x04_u16, 0x05_u16, u32::MAX),
            (0x10_u16, 0x05_u16, u32::MAX),
            (0x20_u16, 0x00_u16, u32::MAX),
        ] {
            acl_value.extend_from_slice(&tag.to_ne_bytes());
            acl_value.extend_from_slice(&permissions.to_ne_bytes());
            acl_value.extend_from_slice(&id.to_ne_bytes());
        }
        let acl_path = CString::new(rootfs.join("opt/data").as_os_str().as_bytes()).unwrap();
        let acl_result = unsafe {
            libc::setxattr(
                acl_path.as_ptr(),
                acl_name.as_ptr(),
                acl_value.as_ptr().cast(),
                acl_value.len(),
                0,
            )
        };
        if acl_result != 0 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ENOTSUP)
            );
        }
        let mut config = MachineConfig::new(
            "source".into(),
            "fixture".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        );
        config.oci = OciImageMetadata {
            environment: vec!["PATH=/custom/bin:/usr/bin".into(), "EDITOR=mousepad".into()],
            labels: BTreeMap::from([("example.buzzard.test".into(), "preserved".into())]),
            working_dir: Some("/opt/data".into()),
            user: Some("1000:1000".into()),
            entrypoint: vec!["/lib/systemd/systemd".into()],
            command: vec!["--unit=graphical.target".into()],
            stop_signal: Some("SIGRTMIN+3".into()),
        };
        config.save(&machine).unwrap();
        let output = temp.path().join("machine.oci.tar.zst");
        File::create(&output).unwrap();
        export_oci_archive(
            &rootfs,
            &machine.join(MachineConfig::FILE),
            &output,
            &work,
            None,
        )
        .unwrap();

        let extracted = temp.path().join("layout");
        fs::create_dir(&extracted).unwrap();
        extract_oci_archive(&output, &extracted).unwrap();
        let layout = canonical_oci_layout(&extracted).unwrap();
        let index = read_oci_index(&layout).unwrap();
        let descriptor = resolve_oci_manifest_descriptor(&layout, &index, None).unwrap();
        let portable = portable_config_from_manifest(&layout, &descriptor)
            .unwrap()
            .unwrap();
        assert_eq!(portable.id, config.id);
        assert_eq!(portable.name, config.name);
        let restored_oci = oci_metadata_from_manifest(&layout, &descriptor).unwrap();
        assert_eq!(restored_oci.environment, config.oci.environment);
        assert_eq!(restored_oci.working_dir, config.oci.working_dir);
        assert_eq!(restored_oci.user, config.oci.user);
        assert_eq!(restored_oci.entrypoint, config.oci.entrypoint);
        assert_eq!(restored_oci.command, config.oci.command);
        assert_eq!(restored_oci.stop_signal, config.oci.stop_signal);
        assert_eq!(
            restored_oci
                .labels
                .get("example.buzzard.test")
                .map(String::as_str),
            Some("preserved")
        );

        fs::create_dir_all(rootfs.join("etc/ssh")).unwrap();
        fs::create_dir_all(rootfs.join("var/lib/systemd")).unwrap();
        fs::write(rootfs.join("etc/machine-id"), b"builder-machine-id\n").unwrap();
        fs::write(rootfs.join("etc/ssh/ssh_host_ed25519_key"), b"builder-key").unwrap();
        fs::write(rootfs.join("var/lib/systemd/random-seed"), b"builder-seed").unwrap();
        let source_machine_id = fs::read(rootfs.join("etc/machine-id")).unwrap();
        let source_ssh_key = fs::read(rootfs.join("etc/ssh/ssh_host_ed25519_key")).unwrap();
        let source_random_seed = fs::read(rootfs.join("var/lib/systemd/random-seed")).unwrap();
        let generic_work = temp.path().join("generic-work");
        fs::create_dir(&generic_work).unwrap();
        let generic_output = temp.path().join("rootfs-seed.oci.tar.zst");
        File::create(&generic_output).unwrap();
        export_oci_archive(
            &rootfs,
            &machine.join(MachineConfig::FILE),
            &generic_output,
            &generic_work,
            Some(1_700_000_000),
        )
        .unwrap();
        let generic_extracted = temp.path().join("generic-layout");
        fs::create_dir(&generic_extracted).unwrap();
        extract_oci_archive(&generic_output, &generic_extracted).unwrap();
        let generic_layout = canonical_oci_layout(&generic_extracted).unwrap();
        let generic_index = read_oci_index(&generic_layout).unwrap();
        let generic_descriptor =
            resolve_oci_manifest_descriptor(&generic_layout, &generic_index, None).unwrap();
        assert_eq!(
            generic_descriptor
                .annotations
                .get(OCI_REF_NAME_ANNOTATION)
                .map(String::as_str),
            Some("buzzardos-rootfs-seed")
        );
        assert!(
            portable_config_from_manifest(&generic_layout, &generic_descriptor)
                .unwrap()
                .is_none()
        );
        let generic_metadata =
            oci_metadata_from_manifest(&generic_layout, &generic_descriptor).unwrap();
        assert_eq!(generic_metadata.environment, config.oci.environment);
        assert_eq!(generic_metadata.labels["example.buzzard.test"], "preserved");
        assert_eq!(
            generic_metadata.labels["org.opencontainers.image.title"],
            "Buzzard OS rootfs seed"
        );
        assert_eq!(generic_metadata.working_dir, config.oci.working_dir);
        assert_eq!(generic_metadata.user, config.oci.user);
        assert_eq!(generic_metadata.entrypoint, config.oci.entrypoint);
        assert_eq!(generic_metadata.command, config.oci.command);
        assert_eq!(generic_metadata.stop_signal, config.oci.stop_signal);
        let generic_manifest: OciManifest = serde_json::from_slice(
            &read_verified_blob(&generic_layout, &generic_descriptor).unwrap(),
        )
        .unwrap();
        assert_eq!(generic_manifest.layers.len(), 1);
        assert!(
            !generic_manifest
                .annotations
                .contains_key(BUZZARD_OCI_CONFIG_ANNOTATION)
        );
        assert_eq!(
            generic_manifest.annotations["org.opencontainers.image.title"],
            "Buzzard OS rootfs seed"
        );
        let generic_config: serde_json::Value = serde_json::from_slice(
            &read_verified_blob(&generic_layout, &generic_manifest.config).unwrap(),
        )
        .unwrap();
        assert_eq!(generic_config["created"], "2023-11-14T22:13:20+00:00");
        assert_eq!(
            generic_config["rootfs"]["diff_ids"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let generic_layer = temp.path().join("generic-layer.tar");
        decompress_verified_layer(&generic_layout, &generic_manifest.layers[0], &generic_layer)
            .unwrap();
        let generic_rootfs = temp.path().join("generic-rootfs");
        fs::create_dir(&generic_rootfs).unwrap();
        apply_layer(&generic_layer, &generic_rootfs).unwrap();
        assert_eq!(
            fs::read(generic_rootfs.join("etc/machine-id")).unwrap(),
            b""
        );
        assert!(!generic_rootfs.join("etc/ssh/ssh_host_ed25519_key").exists());
        assert!(!generic_rootfs.join("var/lib/systemd/random-seed").exists());
        assert_eq!(
            fs::read(rootfs.join("etc/machine-id")).unwrap(),
            source_machine_id
        );
        assert_eq!(
            fs::read(rootfs.join("etc/ssh/ssh_host_ed25519_key")).unwrap(),
            source_ssh_key
        );
        assert_eq!(
            fs::read(rootfs.join("var/lib/systemd/random-seed")).unwrap(),
            source_random_seed
        );

        let repeated_work = temp.path().join("repeated-generic-work");
        fs::create_dir(&repeated_work).unwrap();
        let repeated_output = temp.path().join("repeated-rootfs-seed.oci.tar.zst");
        File::create(&repeated_output).unwrap();
        export_oci_archive(
            &rootfs,
            &machine.join(MachineConfig::FILE),
            &repeated_output,
            &repeated_work,
            Some(1_700_000_000),
        )
        .unwrap();
        assert_eq!(
            fs::read(&generic_output).unwrap(),
            fs::read(&repeated_output).unwrap()
        );
        assert_eq!(
            fs::read(rootfs.join("etc/machine-id")).unwrap(),
            source_machine_id
        );
        assert_eq!(
            fs::read(rootfs.join("etc/ssh/ssh_host_ed25519_key")).unwrap(),
            source_ssh_key
        );
        assert_eq!(
            fs::read(rootfs.join("var/lib/systemd/random-seed")).unwrap(),
            source_random_seed
        );

        let manifest: OciManifest =
            serde_json::from_slice(&read_verified_blob(&layout, &descriptor).unwrap()).unwrap();
        let expanded_layer = temp.path().join("layer.tar");
        decompress_layer(
            &verified_blob_path(&layout, &manifest.layers[0]).unwrap(),
            &expanded_layer,
        )
        .unwrap();
        let restored = temp.path().join("restored");
        fs::create_dir(&restored).unwrap();
        apply_layer(&expanded_layer, &restored).unwrap();
        assert_eq!(
            fs::read(restored.join("opt/data/original")).unwrap(),
            b"portable state"
        );
        assert_eq!(
            fs::metadata(restored.join("opt/data/original"))
                .unwrap()
                .ino(),
            fs::metadata(restored.join("opt/data/hardlink"))
                .unwrap()
                .ino()
        );
        assert_eq!(
            fs::read_link(restored.join("opt/data/symlink")).unwrap(),
            Path::new("original")
        );
        let restored_file = fs::symlink_metadata(restored.join("opt/data/original")).unwrap();
        assert_eq!(restored_file.mtime(), 1_700_000_001);
        assert_eq!(restored_file.mtime_nsec(), 234_567_890);
        let restored_directory = fs::symlink_metadata(restored.join("opt/data")).unwrap();
        assert_eq!(restored_directory.mtime(), 1_700_000_000);
        assert_eq!(restored_directory.mtime_nsec(), 123_456_789);
        assert_eq!(restored_directory.permissions().mode() & 0o7000, 0o2000);
        for ephemeral in ["proc", "sys", "dev", "run", "tmp", "shared"] {
            assert!(restored.join(ephemeral).is_dir());
            assert!(!restored.join(ephemeral).join("must-not-export").exists());
        }
        if result == 0 {
            let restored_path =
                CString::new(restored.join("opt/data/original").as_os_str().as_bytes()).unwrap();
            let mut buffer = [0_u8; 16];
            let length = unsafe {
                libc::getxattr(
                    restored_path.as_ptr(),
                    xattr_name.as_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            assert_eq!(&buffer[..length as usize], xattr_value);
        }
        if acl_result == 0 {
            let restored_path =
                CString::new(restored.join("opt/data").as_os_str().as_bytes()).unwrap();
            let mut buffer = [0_u8; 128];
            let length = unsafe {
                libc::getxattr(
                    restored_path.as_ptr(),
                    acl_name.as_ptr(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            assert_eq!(&buffer[..length as usize], acl_value.as_slice());
        }
    }

    #[test]
    fn oci_index_requires_explicit_selection_when_multiple_host_images_match() {
        let descriptor = |digest: &str, name: &str| OciDescriptor {
            digest: digest.into(),
            size: 1,
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            platform: Some(OciPlatform {
                os: "linux".into(),
                architecture: host_oci_architecture().unwrap().into(),
            }),
            annotations: BTreeMap::from([(OCI_REF_NAME_ANNOTATION.into(), name.into())]),
        };
        let descriptors = vec![
            descriptor(&format!("sha256:{}", "1".repeat(64)), "one"),
            descriptor(&format!("sha256:{}", "2".repeat(64)), "two"),
        ];
        assert!(select_oci_descriptor(&descriptors, None).is_err());
        assert_eq!(
            select_oci_descriptor(&descriptors, Some("two"))
                .unwrap()
                .annotations[OCI_REF_NAME_ANNOTATION],
            "two"
        );
    }

    #[test]
    fn oversized_oci_metadata_is_rejected_before_allocation() {
        let temp = tempfile::tempdir().unwrap();
        let descriptor = OciDescriptor {
            digest: format!("sha256:{}", "0".repeat(64)),
            size: MAX_OCI_METADATA_BYTES + 1,
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            platform: None,
            annotations: BTreeMap::new(),
        };

        let error = read_verified_blob(temp.path(), &descriptor).unwrap_err();

        assert!(error.to_string().contains("metadata blob"));
        assert!(error.to_string().contains("maximum"));
    }

    #[test]
    fn layer_bytes_are_authenticated_before_the_expanded_layer_is_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let blob_dir = temp.path().join("blobs/sha256");
        fs::create_dir_all(&blob_dir).unwrap();
        let contents = b"authenticated layer bytes";
        let compressed = zstd::stream::encode_all(Cursor::new(contents), 1).unwrap();
        let digest = format!("sha256:{:x}", Sha256::digest(&compressed));
        fs::write(
            blob_dir.join(digest.strip_prefix("sha256:").unwrap()),
            &compressed,
        )
        .unwrap();
        let descriptor = OciDescriptor {
            digest,
            size: compressed.len() as u64,
            media_type: "application/vnd.oci.image.layer.v1.tar+zstd".into(),
            platform: None,
            annotations: BTreeMap::new(),
        };
        let expanded = temp.path().join("expanded-layer.tar");

        decompress_verified_layer(temp.path(), &descriptor, &expanded).unwrap();
        assert_eq!(fs::read(&expanded).unwrap(), contents);

        let wrong_digest = format!("sha256:{}", "0".repeat(64));
        fs::write(
            blob_dir.join(wrong_digest.strip_prefix("sha256:").unwrap()),
            &compressed,
        )
        .unwrap();
        let invalid = OciDescriptor {
            digest: wrong_digest,
            ..descriptor
        };
        let rejected = temp.path().join("rejected-layer.tar");
        assert!(decompress_verified_layer(temp.path(), &invalid, &rejected).is_err());
        assert!(!rejected.exists());
    }

    #[test]
    fn nested_oci_selector_can_name_outer_reference_or_inner_digest() {
        let temp = tempfile::tempdir().unwrap();
        let blob_dir = temp.path().join("blobs/sha256");
        fs::create_dir_all(&blob_dir).unwrap();
        let manifest = OciDescriptor {
            digest: format!("sha256:{}", "3".repeat(64)),
            size: 123,
            media_type: "application/vnd.oci.image.manifest.v1+json".into(),
            platform: Some(OciPlatform {
                os: "linux".into(),
                architecture: host_oci_architecture().unwrap().into(),
            }),
            annotations: BTreeMap::new(),
        };
        let nested = OciIndex {
            schema_version: 2,
            manifests: vec![manifest.clone()],
        };
        let mut nested_descriptor = write_json_blob(
            &blob_dir,
            "application/vnd.oci.image.index.v1+json",
            &nested,
        )
        .unwrap();
        nested_descriptor
            .annotations
            .insert(OCI_REF_NAME_ANNOTATION.into(), "portable-reference".into());
        let outer = OciIndex {
            schema_version: 2,
            manifests: vec![nested_descriptor],
        };

        assert_eq!(
            resolve_oci_manifest_descriptor(temp.path(), &outer, Some("portable-reference"))
                .unwrap()
                .digest,
            manifest.digest
        );
        assert_eq!(
            resolve_oci_manifest_descriptor(temp.path(), &outer, Some(&manifest.digest))
                .unwrap()
                .digest,
            manifest.digest
        );
    }
}
