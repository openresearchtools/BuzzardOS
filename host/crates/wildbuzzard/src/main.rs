// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use flate2::read::GzDecoder;
use nix::unistd::{Gid, Uid, chown};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wb_core::{
    AppImageRuntimeLease, DESKTOP_READINESS_DEADLINE_DETAIL_PREFIX, IdMap, MachineConfig,
    MachineState, NetworkMode, ResourceLocator, RuntimeState, WaylandCapabilities, WbPaths,
    host_control_socket,
};

const ROOTFS_SEED_ARCHIVE: &str = "WildBuzzard-rootfs-linux-x86_64.tar.zst";
const ROOTFS_SEED_MANIFEST: &str = "WildBuzzard-rootfs-linux-x86_64.json";
const ROOTFS_SEED_KIND: &str = "wildbuzzard-flat-rootfs";
const ROOTFS_SEED_MEDIA_TYPE: &str = "application/vnd.wildbuzzard.rootfs.v1.tar+zstd";
const ROOTFS_SEED_SCHEMA: u32 = 1;
const MAX_GUEST_ID: u64 = 65_535;
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
        "usr/share/wildbuzzard/branding/wildbuzzard-mark-dark.svg",
        include_bytes!("../../../../guest/assets/branding/wildbuzzard-mark-dark.svg"),
        0o644,
    ),
    (
        "usr/share/wildbuzzard/branding/wildbuzzard-mark-light.svg",
        include_bytes!("../../../../guest/assets/branding/wildbuzzard-mark-light.svg"),
        0o644,
    ),
    (
        "usr/share/wildbuzzard/branding/wildbuzzard-icon-light.svg",
        include_bytes!("../../../../guest/assets/branding/wildbuzzard-icon-light.svg"),
        0o644,
    ),
    (
        "usr/share/wildbuzzard/branding/wallpaper-presets.json",
        include_bytes!("../../../../guest/assets/branding/wallpaper-presets.json"),
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
    name = "wildbuzzard",
    version,
    about = "Persistent, rootless desktop machines in one Wayland window"
)]
struct Cli {
    /// Portable storage folder (default: directory containing the AppImage).
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
        eprintln!("wildbuzzard: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let appimage_lease = AppImageRuntimeLease::capture()?;
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
        Some(Commands::Start { name, detach }) => {
            start(&paths, &name, detach, appimage_lease.as_ref())
        }
        Some(Commands::Stop { name }) => stop(&paths, &name),
        Some(Commands::Window { name, action }) => window(&paths, &name, action),
        Some(Commands::Status { name }) => status(&paths, &name),
        Some(Commands::List) => list(&paths),
        Some(Commands::Doctor) => doctor(),
        Some(Commands::ApplyImage { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::ApplyRootfs { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::CleanupStaging { .. }) => {
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
        None => open_portable_desktop(&paths, appimage_lease.as_ref()),
    }
}

fn open_portable_desktop(
    paths: &WbPaths,
    appimage_lease: Option<&AppImageRuntimeLease>,
) -> Result<()> {
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

    let name = match machines.as_slice() {
        [] => {
            let name = "default";
            println!("Creating persistent desktop machine '{name}' for first launch...");
            create(paths, name, None, NetworkMode::User, vec!["all".into()])?;
            name.to_owned()
        }
        [name] => name.clone(),
        _ if machines.iter().any(|name| name == "default") => "default".into(),
        _ => bail!(
            "multiple portable machines exist ({}); run `wildbuzzard start NAME` or name one `default`",
            machines.join(", ")
        ),
    };

    start(paths, &name, false, appimage_lease)
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

        let (source_reference, image_digest) = if let Some(image) = image {
            // Release AppImages always resolve the bundled copy. PATH fallback only makes
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

            let image_archive = machine_dir.join("image-layout");
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

            eprintln!("Applying OCI layers to the persistent root filesystem…");
            apply_image_in_user_namespace(
                &resources,
                &image_archive,
                &image_digest,
                &rootfs,
                machine_dir,
            )?;
            fs::remove_dir_all(&image_archive).context("removing temporary OCI layout")?;
            (image.to_owned(), image_digest)
        } else {
            let seed = bundled_rootfs_seed(paths)?.with_context(|| {
                format!(
                    "no bundled rootfs seed was found at runtime/{ROOTFS_SEED_ARCHIVE}; download and extract the full Wild Buzzard portable bundle beside the AppImage, or create from an OCI image explicitly with `wildbuzzard create {name} --image IMAGE_REFERENCE`"
                )
            })?;
            eprintln!("Applying the bundled rootfs seed to the persistent root filesystem…");
            apply_rootfs_in_user_namespace(&resources, &seed, &rootfs)?;
            (
                format!("bundle:runtime/{ROOTFS_SEED_ARCHIVE}"),
                format!("sha256:{}", seed.manifest.archive.sha256),
            )
        };

        let config = MachineConfig::new(
            name.to_owned(),
            source_reference.clone(),
            image_digest,
            network,
            gpus,
        );
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
    let launcher = std::env::current_exe().context("locating launcher for staging cleanup")?;
    let mut command = Command::new(&unshare);
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.unshare_args())
        .arg(launcher)
        .arg("__cleanup-staging")
        .arg("--staging")
        .arg(staging)
        .arg("--machines")
        .arg(machines)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("starting cleanup namespace with {}", unshare.display()))?;
    if !status.success() {
        bail!("staging cleanup namespace exited with {status}");
    }
    match fs::symlink_metadata(staging) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => bail!("staging cleanup reported success but the tree still exists"),
        Err(error) => Err(error).context("verifying staging cleanup"),
    }
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
    if !name.starts_with('.') || !name.contains("-creating-") {
        bail!("refusing to remove a path that is not a machine creation staging directory");
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

#[derive(Debug)]
struct BundledRootfsSeed {
    archive: PathBuf,
    manifest_path: PathBuf,
    manifest: RootfsSeedManifest,
}

fn bundled_rootfs_seed(paths: &WbPaths) -> Result<Option<BundledRootfsSeed>> {
    let runtime = paths.base().join("runtime");
    let archive = runtime.join(ROOTFS_SEED_ARCHIVE);
    let manifest_path = runtime.join(ROOTFS_SEED_MANIFEST);
    let archive_exists = fs::symlink_metadata(&archive).is_ok();
    let manifest_exists = fs::symlink_metadata(&manifest_path).is_ok();
    if !archive_exists && !manifest_exists {
        return Ok(None);
    }

    let runtime_metadata = fs::symlink_metadata(&runtime)
        .with_context(|| format!("inspecting bundled runtime directory {}", runtime.display()))?;
    if runtime_metadata.file_type().is_symlink() || !runtime_metadata.is_dir() {
        bail!(
            "bundled runtime path {} must be a real directory, not a symlink",
            runtime.display()
        );
    }
    if !archive_exists || !manifest_exists {
        bail!(
            "portable bundle is incomplete: runtime/{ROOTFS_SEED_ARCHIVE} and runtime/{ROOTFS_SEED_MANIFEST} must both be present"
        );
    }

    let manifest = read_rootfs_seed_manifest(&manifest_path)?;
    validate_rootfs_seed_archive_header(&archive, &manifest.archive)?;
    Ok(Some(BundledRootfsSeed {
        archive,
        manifest_path,
        manifest,
    }))
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

fn validate_rootfs_seed_archive_header(path: &Path, archive: &RootfsSeedArchive) -> Result<()> {
    if path.file_name() != Some(std::ffi::OsStr::new(ROOTFS_SEED_ARCHIVE)) {
        bail!("bundled rootfs archive must be named {ROOTFS_SEED_ARCHIVE}");
    }
    let mut file = open_regular_nofollow(path, "bundled rootfs archive")?;
    let actual_size = file.metadata()?.len();
    if actual_size != archive.size {
        bail!(
            "bundled rootfs archive size mismatch: manifest says {}, file has {actual_size}",
            archive.size
        );
    }
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .context("reading bundled rootfs archive header")?;
    if magic != [0x28, 0xb5, 0x2f, 0xfd] {
        bail!("bundled rootfs archive is not a Zstandard frame");
    }
    Ok(())
}

fn apply_rootfs_in_user_namespace(
    resources: &ResourceLocator,
    seed: &BundledRootfsSeed,
    rootfs: &Path,
) -> Result<()> {
    let unshare = resources.helper_or_path("unshare")?;
    let id_map = IdMap::discover()?;
    let launcher = std::env::current_exe().context("locating launcher for rootfs extraction")?;
    let expected_digest = format!("sha256:{}", seed.manifest.archive.sha256);
    let mut command = Command::new(&unshare);
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.unshare_args())
        .arg(launcher)
        .arg("__apply-rootfs")
        .arg("--archive")
        .arg(&seed.archive)
        .arg("--manifest")
        .arg(&seed.manifest_path)
        .arg("--expected-digest")
        .arg(expected_digest)
        .arg("--rootfs")
        .arg(rootfs)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("starting full-ID namespace with {}", unshare.display()))?;
    if !status.success() {
        bail!(
            "bundled rootfs namespace/extraction helper exited with {status}; the preceding child diagnostic identifies whether subordinate-ID setup, archive validation, storage, or metadata restoration failed"
        );
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
    let launcher = std::env::current_exe().context("locating launcher for OCI extraction")?;
    let mut command = Command::new(&unshare);
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.unshare_args())
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
        .with_context(|| format!("starting full-ID namespace with {}", unshare.display()))?;
    if !status.success() {
        bail!(
            "applying the OCI image requires a configured subordinate UID/GID range; namespace helper exited with {status}"
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciIndex {
    schema_version: u32,
    manifests: Vec<OciDescriptor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciManifest {
    schema_version: u32,
    config: OciDescriptor,
    layers: Vec<OciDescriptor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciDescriptor {
    digest: String,
    size: u64,
    media_type: String,
}

fn oci_platform() -> Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("linux/amd64"),
        "aarch64" => Ok("linux/arm64"),
        architecture => bail!("unsupported AppImage architecture '{architecture}'"),
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
    let hexadecimal = validate_sha256_digest(&descriptor.digest)?;
    let path = layout.join("blobs/sha256").join(hexadecimal);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("reading OCI blob {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("OCI blob {} is not a regular file", path.display());
    }
    if metadata.len() != descriptor.size {
        bail!(
            "OCI blob {} has size {}, expected {}",
            descriptor.digest,
            metadata.len(),
            descriptor.size
        );
    }

    let mut file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
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
    Ok(path)
}

fn read_verified_blob(layout: &Path, descriptor: &OciDescriptor) -> Result<Vec<u8>> {
    let path = verified_blob_path(layout, descriptor)?;
    fs::read(&path).with_context(|| format!("reading {}", path.display()))
}

fn apply_image_archive(
    layout: &Path,
    expected_digest: &str,
    rootfs: &Path,
    work_dir: &Path,
) -> Result<()> {
    chown(rootfs, Some(Uid::from_raw(0)), Some(Gid::from_raw(0)))
        .with_context(|| format!("setting root ownership on {}", rootfs.display()))?;

    let index_path = layout.join("index.json");
    let index_bytes =
        fs::read(&index_path).with_context(|| format!("reading {}", index_path.display()))?;
    let index: OciIndex =
        serde_json::from_slice(&index_bytes).context("parsing OCI image index")?;
    if index.schema_version != 2 {
        bail!(
            "unsupported OCI index schema version {}",
            index.schema_version
        );
    }
    let descriptor = index
        .manifests
        .first()
        .context("OCI image index contains no manifest")?;
    if index.manifests.len() != 1 {
        bail!("platform-specific OCI pull returned multiple manifests");
    }
    if descriptor.digest != expected_digest {
        bail!(
            "OCI manifest digest mismatch: resolved {expected_digest}, layout contains {}",
            descriptor.digest
        );
    }
    if descriptor.media_type != "application/vnd.oci.image.manifest.v1+json"
        && descriptor.media_type != "application/vnd.docker.distribution.manifest.v2+json"
    {
        bail!(
            "unsupported OCI manifest media type {}",
            descriptor.media_type
        );
    }

    let manifest_bytes = read_verified_blob(layout, descriptor)?;
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
        let compressed = verified_blob_path(layout, descriptor)?;
        let layer_tar = work_dir.join(format!("layer-{index}.tar"));
        decompress_layer(&compressed, &layer_tar)?;
        apply_layer(&layer_tar, rootfs)
            .with_context(|| format!("applying OCI layer {}", index + 1))?;
        fs::remove_file(&layer_tar).context("removing expanded OCI layer")?;
    }

    Ok(())
}

fn decompress_layer(source: &Path, destination: &Path) -> Result<()> {
    let mut input = BufReader::new(
        File::open(source).with_context(|| format!("opening layer {}", source.display()))?,
    );
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
        if let Some(action) = whiteout_action(path) {
            match action {
                Whiteout::Remove(target) => remove_whiteout_target(rootfs, &target)?,
                Whiteout::Opaque(parent) => clear_whiteout_directory(rootfs, parent)?,
            }
        }
    }

    let file = File::open(layer_tar).context("opening OCI layer for extraction")?;
    let mut archive = tar::Archive::new(BufReader::new(file));
    archive.set_preserve_permissions(true);
    archive.set_preserve_mtime(true);
    archive.set_preserve_ownerships(true);
    for item in archive.entries().context("reading OCI layer")? {
        let mut entry = item.context("reading OCI layer entry")?;
        let entry_path = entry.path().context("reading OCI layer path")?.into_owned();
        let path = safe_relative_path(&entry_path)?;
        if path.as_os_str().is_empty() || whiteout_action(path).is_some() {
            continue;
        }
        let extended_attributes = entry
            .pax_extensions()
            .context("reading OCI layer extended attributes")?
            .map(|extensions| {
                extensions
                    .filter_map(|extension| {
                        let extension = extension.ok()?;
                        let name = extension
                            .key_bytes()
                            .strip_prefix(b"SCHILY.xattr.")?
                            .to_vec();
                        Some((name, extension.value_bytes().to_vec()))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        entry
            .unpack_in(rootfs)
            .with_context(|| format!("extracting {}", path.display()))?;
        for (name, value) in extended_attributes {
            set_link_xattr(&rootfs.join(path), &name, &value)?;
        }
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

fn whiteout_action(path: &Path) -> Option<Whiteout<'_>> {
    let name = path.file_name()?.to_str()?;
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    if name == ".wh..wh..opq" {
        return Some(Whiteout::Opaque(parent));
    }
    name.strip_prefix(".wh.")
        .map(|target| Whiteout::Remove(parent.join(target)))
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

fn start(
    paths: &WbPaths,
    name: &str,
    detach: bool,
    appimage_lease: Option<&AppImageRuntimeLease>,
) -> Result<()> {
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
    if let Some(lease) = appimage_lease {
        lease.pass_to(&mut command);
    }
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
    let launcher = std::env::current_exe().context("locating guest asset helper")?;
    let mut command = Command::new(&unshare);
    command.env_clear();
    id_map.configure_command(&mut command);
    command
        .args(id_map.unshare_args())
        .arg(&launcher)
        .arg(internal_command)
        .arg("--rootfs")
        .arg(rootfs)
        .stdin(Stdio::null());
    command.status().with_context(|| {
        format!(
            "starting rootless guest asset helper through {}",
            unshare.display()
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
    let launcher = std::env::current_exe().context("locating guest runtime rollback helper")?;
    let mut command = Command::new(&unshare);
    command.env_clear();
    id_map.configure_command(&mut command);
    let status = command
        .args(id_map.unshare_args())
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
                unshare.display()
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
            "unsupported host: required Wild Buzzard facilities are unavailable: {}",
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
    let mut command = Command::new(unshare);
    id_map.configure_command(&mut command);
    command
        .args(id_map.unshare_args())
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
        assert!(error.contains("download and extract the full Wild Buzzard portable bundle"));
        assert!(error.contains("--image IMAGE_REFERENCE"));
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
    fn fresh_install_includes_branding_assets_and_discoverable_application_icons() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();

        install_guest_assets_without_shell(&rootfs).unwrap();

        let branding_assets: &[(&str, &[u8])] = &[
            (
                "usr/share/wildbuzzard/branding/wildbuzzard-mark-dark.svg",
                include_bytes!("../../../../guest/assets/branding/wildbuzzard-mark-dark.svg"),
            ),
            (
                "usr/share/wildbuzzard/branding/wildbuzzard-mark-light.svg",
                include_bytes!("../../../../guest/assets/branding/wildbuzzard-mark-light.svg"),
            ),
            (
                "usr/share/wildbuzzard/branding/wildbuzzard-icon-light.svg",
                include_bytes!("../../../../guest/assets/branding/wildbuzzard-icon-light.svg"),
            ),
            (
                "usr/share/wildbuzzard/branding/wallpaper-presets.json",
                include_bytes!("../../../../guest/assets/branding/wallpaper-presets.json"),
            ),
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
        for (relative, expected) in branding_assets {
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
    fn branding_migration_updates_managed_content_and_preserves_guest_edits() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let managed = Path::new("usr/share/wildbuzzard/branding/wildbuzzard-mark-dark.svg");
        let modified = Path::new("usr/share/icons/WildBuzzard/scalable/apps/wildbuzzard.svg");
        let old_distributed = b"<svg><!-- old distributed branding --></svg>\n";
        let guest_modified = b"<svg><!-- guest replacement branding --></svg>\n";
        let previous = guest_asset_record(old_distributed, 0o644);
        install_guest_asset(&rootfs, managed, old_distributed, 0o644).unwrap();
        install_guest_asset(&rootfs, modified, guest_modified, 0o644).unwrap();

        migrate_guest_asset(
            &rootfs,
            managed,
            include_bytes!("../../../../guest/assets/branding/wildbuzzard-mark-dark.svg"),
            0o644,
            Some(&previous),
            None,
        )
        .unwrap();
        migrate_guest_asset(
            &rootfs,
            modified,
            include_bytes!(
                "../../../../guest/assets/icons/WildBuzzard/scalable/apps/wildbuzzard.svg"
            ),
            0o644,
            Some(&previous),
            None,
        )
        .unwrap();

        assert_eq!(
            fs::read(rootfs.join(managed)).unwrap(),
            include_bytes!("../../../../guest/assets/branding/wildbuzzard-mark-dark.svg")
        );
        assert_eq!(fs::read(rootfs.join(modified)).unwrap(), guest_modified);
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
    fn failed_machine_staging_cleanup_rejects_unrelated_paths() {
        let temp = tempfile::tempdir().unwrap();
        let machines = temp.path().join("vm");
        let unrelated = machines.join("machine");
        fs::create_dir(&machines).unwrap();
        fs::create_dir(&unrelated).unwrap();

        let error = remove_machine_staging_tree(&unrelated, &machines).unwrap_err();

        assert!(error.to_string().contains("not a machine creation staging"));
        assert!(unrelated.is_dir());
    }
}
