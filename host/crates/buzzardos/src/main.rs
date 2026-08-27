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
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use wb_core::{
    DESKTOP_READINESS_DEADLINE_DETAIL_PREFIX, IdMap, MachineConfig, MachineRegistry, MachineState,
    NetworkMode, OciImageMetadata, ResourceLocator, RetainedOciArchive, RuntimeState, SharedPath,
    WaylandCapabilities, WbPaths, host_control_socket,
};

const MAX_GUEST_ID: u64 = 65_535;
const MAX_OCI_PAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OCI_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OCI_SPARSE_EXTENTS: u64 = 1_000_000;
const MAX_OCI_LAYOUT_BYTES: u64 = 1024 * 1024;
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
    name = "Buzzard OS",
    version,
    about = "Persistent, rootless desktop machines in one Wayland window"
)]
struct Cli {
    /// Exact directory containing this machine's metadata, cache, and rootfs.
    /// Required for create/import/clone; optional as a recovery override for
    /// lifecycle commands that normally resolve the user registry.
    #[arg(long, global = true)]
    machine_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create a persistent mutable machine from an OCI image.
    Create {
        name: String,
        /// OCI image reference to pull and flatten once.
        #[arg(long)]
        image: String,
        /// Host file or directory to expose below /shared; repeat as needed.
        #[arg(long = "share")]
        shares: Vec<PathBuf>,
        #[arg(long, value_enum, default_value_t = NetworkArg::User)]
        network: NetworkArg,
        /// NVIDIA GPU index/UUID to expose; repeat for multiple GPUs.
        #[arg(long = "gpu", value_delimiter = ',', default_value = "all")]
        gpus: Vec<String>,
        /// Retain verified OCI install media as cache/source.oci.tar.
        #[arg(long)]
        keep_oci_archive: bool,
    },
    /// Pull an OCI image with rootless Buildah and create a persistent machine.
    Pull {
        name: String,
        /// Fully qualified OCI image reference.
        image: String,
        #[arg(long = "share")]
        shares: Vec<PathBuf>,
        #[arg(long, value_enum, default_value_t = NetworkArg::User)]
        network: NetworkArg,
        #[arg(long = "gpu", value_delimiter = ',', default_value = "all")]
        gpus: Vec<String>,
        #[arg(long)]
        keep_oci_archive: bool,
    },
    /// Build a Containerfile with rootless Buildah and create a persistent machine.
    Build {
        name: String,
        /// Build context directory.
        #[arg(long)]
        context: PathBuf,
        /// Containerfile path; defaults to CONTEXT/Containerfile.
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long = "share")]
        shares: Vec<PathBuf>,
        #[arg(long, value_enum, default_value_t = NetworkArg::User)]
        network: NetworkArg,
        #[arg(long = "gpu", value_delimiter = ',', default_value = "all")]
        gpus: Vec<String>,
        #[arg(long)]
        keep_oci_archive: bool,
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
        /// Host file or directory to expose below /shared; repeat as needed.
        #[arg(long = "share")]
        shares: Vec<PathBuf>,
        /// Retain verified OCI install media as cache/source.oci.tar.
        #[arg(long)]
        keep_oci_archive: bool,
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
    Clone {
        source: String,
        name: String,
        /// Host file or directory to expose below /shared; repeat as needed.
        #[arg(long = "share")]
        shares: Vec<PathBuf>,
    },
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
    /// Register an existing self-describing machine directory.
    Register,
    /// Remove a machine from the registry without deleting its files.
    Unregister { name: String },
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
        work_dir: _,
    }) = &cli.command
    {
        with_private_bind_mount(rootfs, "OCI rootfs", |mounted_rootfs| {
            apply_image_archive(archive, expected_digest, mounted_rootfs, mounted_rootfs)?;
            validate_extracted_rootfs(mounted_rootfs)
        })?;
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
    if let Some(Commands::CleanupStaging { staging, machines }) = &cli.command {
        remove_machine_staging_tree(staging, machines)?;
        return Ok(());
    }
    if let Some(Commands::CleanupExportStaging { staging, cache }) = &cli.command {
        remove_export_staging_tree(staging, cache)?;
        return Ok(());
    }
    let mut registry = MachineRegistry::discover()?;
    match cli.command {
        Some(Commands::Create {
            name,
            image,
            shares,
            network,
            gpus,
            keep_oci_archive,
        }) => {
            let paths = creation_paths(cli.machine_dir.as_deref(), "create")?;
            ensure_registry_target_available(&registry, &name, &paths.machine(&name))?;
            import_machine(
                &paths,
                &name,
                ImportMachineRequest {
                    source: &image,
                    selector: None,
                    mode: ImportModeArg::Clone,
                    source_reference_override: None,
                    shares: shared_paths(shares)?,
                    keep_oci_archive,
                    network_override: Some(network.into()),
                    gpus_override: Some(gpus),
                },
            )?;
            registry.register(&paths.machine(&name))
        }
        Some(Commands::Pull {
            name,
            image,
            shares,
            network,
            gpus,
            keep_oci_archive,
        }) => {
            let paths = creation_paths(cli.machine_dir.as_deref(), "pull")?;
            ensure_registry_target_available(&registry, &name, &paths.machine(&name))?;
            create(
                &paths,
                &name,
                &image,
                network.into(),
                gpus,
                shared_paths(shares)?,
                keep_oci_archive,
            )?;
            registry.register(&paths.machine(&name))
        }
        Some(Commands::Build {
            name,
            context,
            file,
            shares,
            network,
            gpus,
            keep_oci_archive,
        }) => {
            let paths = creation_paths(cli.machine_dir.as_deref(), "build")?;
            ensure_registry_target_available(&registry, &name, &paths.machine(&name))?;
            build_machine(
                &paths,
                &name,
                BuildMachineRequest {
                    context: &context,
                    containerfile: file.as_deref(),
                    network: network.into(),
                    gpus,
                    shares: shared_paths(shares)?,
                    keep_oci_archive,
                },
            )?;
            registry.register(&paths.machine(&name))
        }
        Some(Commands::Start { name, detach }) => {
            let paths = registered_paths(&registry, &name, cli.machine_dir.as_deref())?;
            start(&paths, &name, detach)
        }
        Some(Commands::Stop { name }) => {
            let paths = registered_paths(&registry, &name, cli.machine_dir.as_deref())?;
            stop(&paths, &name)
        }
        Some(Commands::Import {
            source,
            name,
            mode,
            manifest,
            shares,
            keep_oci_archive,
        }) => {
            let paths = creation_paths(cli.machine_dir.as_deref(), "import")?;
            ensure_registry_target_available(&registry, &name, &paths.machine(&name))?;
            import_machine(
                &paths,
                &name,
                ImportMachineRequest {
                    source: &source,
                    selector: manifest.as_deref(),
                    mode,
                    source_reference_override: None,
                    shares: shared_paths(shares)?,
                    keep_oci_archive,
                    network_override: None,
                    gpus_override: None,
                },
            )?;
            registry.register(&paths.machine(&name))
        }
        Some(Commands::Export { name, output }) => {
            let paths = registered_paths(&registry, &name, cli.machine_dir.as_deref())?;
            export_machine(&paths, &name, &output, None)
        }
        Some(Commands::ExportGenericSeed {
            name,
            output,
            source_date_epoch,
        }) => {
            let paths = registered_paths(&registry, &name, cli.machine_dir.as_deref())?;
            export_machine(&paths, &name, &output, Some(source_date_epoch))
        }
        Some(Commands::Clone {
            source,
            name,
            shares,
        }) => {
            let source_paths = registered_paths(&registry, &source, None)?;
            let destination_paths = creation_paths(cli.machine_dir.as_deref(), "clone")?;
            ensure_registry_target_available(&registry, &name, &destination_paths.machine(&name))?;
            clone_machine(
                &source_paths,
                &destination_paths,
                &source,
                &name,
                shared_paths(shares)?,
            )?;
            registry.register(&destination_paths.machine(&name))
        }
        Some(Commands::Delete { name, yes }) => {
            let paths = registered_paths(&registry, &name, cli.machine_dir.as_deref())?;
            delete_machine(&paths, &name, yes)?;
            registry.unregister(&name)
        }
        Some(Commands::Window { name, action }) => {
            let paths = registered_paths(&registry, &name, cli.machine_dir.as_deref())?;
            window(&paths, &name, action)
        }
        Some(Commands::Status { name }) => {
            let paths = registered_paths(&registry, &name, cli.machine_dir.as_deref())?;
            status(&paths, &name)
        }
        Some(Commands::List) => list(&registry),
        Some(Commands::Register) => {
            let machine_dir = cli
                .machine_dir
                .as_deref()
                .context("register requires --machine-dir /path/to/existing-machine")?;
            registry.register(machine_dir)
        }
        Some(Commands::Unregister { name }) => registry.unregister(&name),
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
        Some(Commands::CleanupStaging { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::CleanupExportStaging { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        None => open_machine_manager(),
    }
}

fn open_machine_manager() -> Result<()> {
    let resources = ResourceLocator::discover()?;
    let display = resources.helper_or_path("buzzardos-display")?;
    let launcher = std::env::current_exe().context("locating Buzzard OS launcher")?;
    let status = Command::new(&display)
        .arg("--machine-manager")
        .arg("--launcher")
        .arg(&launcher)
        .status()
        .with_context(|| format!("starting machine manager with {}", display.display()))?;
    if !status.success() {
        bail!("Buzzard OS machine manager exited with {status}");
    }
    Ok(())
}

fn creation_paths(machine_dir: Option<&Path>, operation: &str) -> Result<WbPaths> {
    let machine_dir = machine_dir
        .with_context(|| format!("{operation} requires --machine-dir /path/to/this-machine"))?;
    let paths = WbPaths::for_machine(machine_dir)?;
    paths.ensure()?;
    validate_machine_storage(&paths)?;
    Ok(paths)
}

fn validate_machine_storage(paths: &WbPaths) -> Result<()> {
    let parent = paths.machines();
    let parent_c = CString::new(parent.as_os_str().as_bytes())
        .context("machine parent path contains a NUL byte")?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::statfs(parent_c.as_ptr(), filesystem.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("inspecting machine storage at {}", parent.display()));
    }
    let filesystem = unsafe { filesystem.assume_init() };
    let filesystem_type = filesystem.f_type as u64;
    let incompatible = match filesystem_type {
        0x0000_4d44 => Some("FAT"),
        0x2011_bab0 => Some("exFAT"),
        0x5346_544e => Some("NTFS"),
        _ => None,
    };
    if let Some(name) = incompatible {
        bail!(
            "the selected machine location uses {name}, which cannot preserve the Linux ownership, permissions, links, and extended attributes required by a persistent machine; choose a Linux filesystem such as ext4, XFS, or Btrfs"
        );
    }

    let probe = tempfile::Builder::new()
        .prefix(".buzzardos-storage-check-")
        .tempdir_in(&parent)
        .with_context(|| {
            format!(
                "creating a machine-storage capability check in {}",
                parent.display()
            )
        })?;
    let original = probe.path().join("original");
    let renamed = probe.path().join("renamed");
    let hardlink = probe.path().join("hardlink");
    let symlink = probe.path().join("symlink");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o640)
        .open(&original)
        .context("machine storage cannot create a regular file with Linux permissions")?;
    file.write_all(b"buzzardos-storage-check")
        .context("machine storage cannot write a regular file")?;
    file.sync_all()
        .context("machine storage cannot durably flush a regular file")?;
    fs::hard_link(&original, &hardlink).context("machine storage does not support hard links")?;
    std::os::unix::fs::symlink("original", &symlink)
        .context("machine storage does not support symbolic links")?;
    fs::rename(&original, &renamed).context("machine storage does not support atomic rename")?;

    let path = CString::new(renamed.as_os_str().as_bytes())
        .context("machine storage check path contains a NUL byte")?;
    let name = c"user.buzzardos-storage-check";
    let value = b"supported";
    if unsafe {
        libc::lsetxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context(
            "machine storage does not support the extended attributes required by OCI images",
        );
    }
    let mut returned = [0_u8; 16];
    let length = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            name.as_ptr(),
            returned.as_mut_ptr().cast(),
            returned.len(),
        )
    };
    if length < 0 {
        return Err(std::io::Error::last_os_error())
            .context("machine storage cannot read back extended attributes");
    }
    if &returned[..length as usize] != value {
        bail!("machine storage changed an extended attribute during the capability check");
    }
    if unsafe { libc::lremovexattr(path.as_ptr(), name.as_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("machine storage cannot remove extended attributes");
    }
    Ok(())
}

fn registered_paths(
    registry: &MachineRegistry,
    name: &str,
    override_dir: Option<&Path>,
) -> Result<WbPaths> {
    let directory = match override_dir {
        Some(path) => path.to_path_buf(),
        None => registry.resolve(name)?,
    };
    let paths = WbPaths::for_machine(&directory)?;
    let config = MachineConfig::load(&paths.machine(name))?;
    if config.name != name {
        bail!(
            "machine path {} contains '{}', not requested machine '{name}'",
            directory.display(),
            config.name
        );
    }
    Ok(paths)
}

fn shared_paths(paths: Vec<PathBuf>) -> Result<Vec<SharedPath>> {
    let shares = paths
        .into_iter()
        .map(|path| {
            let absolute = std::path::absolute(&path)
                .with_context(|| format!("resolving shared path {}", path.display()))?;
            SharedPath::from_host_path(absolute)
        })
        .collect::<Result<Vec<_>>>()?;
    MachineConfig::validate_shares(&shares)?;
    Ok(shares)
}

fn ensure_registry_target_available(
    registry: &MachineRegistry,
    name: &str,
    machine_dir: &Path,
) -> Result<()> {
    for entry in registry.entries() {
        if entry.name == name {
            bail!(
                "machine name '{name}' is already registered at {}",
                entry.machine_dir.display()
            );
        }
        if entry.machine_dir == machine_dir
            || entry.machine_dir.starts_with(machine_dir)
            || machine_dir.starts_with(&entry.machine_dir)
        {
            bail!(
                "selected machine directory {} overlaps registered machine '{}' at {}",
                machine_dir.display(),
                entry.name,
                entry.machine_dir.display()
            );
        }
    }
    Ok(())
}

fn create(
    paths: &WbPaths,
    name: &str,
    image: &str,
    network: NetworkMode,
    gpus: Vec<String>,
    shares: Vec<SharedPath>,
    keep_oci_archive: bool,
) -> Result<()> {
    MachineConfig::validate_name(name)?;
    MachineConfig::validate_gpus(&gpus)?;
    if image.trim().is_empty() {
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

        let (source_reference, image_digest, oci_metadata, retained_oci_archive) = {
            let platform = oci_platform()?;
            let image_archive = machine_dir.join("image.oci.tar");
            let image_layout_stage = machine_dir.join("image-layout");
            eprintln!("Pulling {image}…");
            pull_oci_archive(&resources, image, platform, &image_archive, machine_dir)?;

            fs::create_dir(&image_layout_stage).context("creating remote OCI layout staging")?;
            extract_oci_archive(&image_archive, &image_layout_stage)
                .context("extracting downloaded OCI layout archive")?;
            let image_layout = canonical_oci_layout(&image_layout_stage)?;
            let index = read_oci_index(&image_layout)?;
            let descriptor = resolve_oci_manifest_descriptor(&image_layout, &index, None)?;
            let image_digest = descriptor.digest.clone();
            let oci_metadata = oci_metadata_from_manifest(&image_layout, &descriptor)?;

            eprintln!("Applying OCI layers to the persistent root filesystem…");
            apply_image_in_user_namespace(
                &resources,
                &image_layout,
                &image_digest,
                &rootfs,
                machine_dir,
            )?;
            let retained = keep_oci_archive
                .then(|| retain_oci_layout(&image_layout, machine_dir))
                .transpose()?;
            fs::remove_file(&image_archive).context("removing downloaded OCI archive")?;
            fs::remove_dir_all(&image_layout_stage).context("removing temporary OCI layout")?;
            (image.to_owned(), image_digest, oci_metadata, retained)
        };

        let mut config = MachineConfig::new(
            name.to_owned(),
            source_reference.clone(),
            image_digest,
            network,
            gpus,
        );
        config.oci = oci_metadata;
        config.shares = shares;
        config.retained_oci_archive = retained_oci_archive;
        config.save(machine_dir)?;
        RuntimeState::new(MachineState::Stopped).save(machine_dir)?;
        File::create(machine_dir.join("machine.lock")).context("creating machine lock")?;

        commit_new_machine(stage.path(), &final_dir)
            .with_context(|| format!("committing machine to {}", final_dir.display()))?;
        println!(
            "Created '{name}' from {source_reference}\nMachine directory: {}\nPersistent rootfs: {}",
            final_dir.display(),
            final_dir.join("rootfs").display()
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

fn pull_oci_archive(
    resources: &ResourceLocator,
    image: &str,
    platform: &str,
    archive: &Path,
    work_parent: &Path,
) -> Result<()> {
    let (os, arch) = platform
        .split_once('/')
        .filter(|(os, arch)| !os.is_empty() && !arch.is_empty())
        .context("OCI platform must have the form OS/ARCH")?;
    let buildah = resources.helper_or_path("buildah")?;
    let work = tempfile::Builder::new()
        .prefix(".buildah-pull-")
        .tempdir_in(work_parent)
        .context("creating isolated Buildah pull storage")?;
    let storage = work.path().join("storage");
    let runroot = work.path().join("runroot");
    fs::create_dir(&storage).context("creating isolated Buildah pull storage")?;
    fs::create_dir(&runroot).context("creating isolated Buildah pull runroot")?;

    let pull = Command::new(&buildah)
        .arg("--root")
        .arg(&storage)
        .arg("--runroot")
        .arg(&runroot)
        .args([
            "--storage-driver",
            "vfs",
            "pull",
            "--quiet",
            "--policy=always",
        ])
        .arg("--os")
        .arg(os)
        .arg("--arch")
        .arg(arch)
        .arg(image)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("starting rootless Buildah pull with {}", buildah.display()))?;
    if !pull.status.success() {
        bail!(
            "Buildah OCI pull failed with {}: {}",
            pull.status,
            String::from_utf8_lossy(&pull.stderr).trim()
        );
    }
    let image_id = String::from_utf8(pull.stdout)
        .context("Buildah returned a non-UTF-8 image ID")?
        .trim()
        .to_owned();
    if image_id.is_empty() {
        bail!("Buildah returned an empty image ID");
    }

    let destination = format!("oci-archive:{}", archive.display());
    let push = Command::new(&buildah)
        .arg("--root")
        .arg(&storage)
        .arg("--runroot")
        .arg(&runroot)
        .args(["--storage-driver", "vfs", "push", "--format", "oci"])
        .arg(&image_id)
        .arg(&destination)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("exporting OCI archive with {}", buildah.display()))?;
    if !push.success() {
        bail!("Buildah OCI export failed with {push}");
    }

    cleanup_buildah_store(&buildah, &storage, &runroot);
    Ok(())
}

fn cleanup_buildah_store(buildah: &Path, storage: &Path, runroot: &Path) {
    for arguments in [["rm", "--all", ""], ["rmi", "--all", "--force"]] {
        let mut command = Command::new(buildah);
        command
            .arg("--root")
            .arg(storage)
            .arg("--runroot")
            .arg(runroot)
            .args(["--storage-driver", "vfs"]);
        for argument in arguments
            .into_iter()
            .filter(|argument| !argument.is_empty())
        {
            command.arg(argument);
        }
        let _ = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

struct BuildMachineRequest<'a> {
    context: &'a Path,
    containerfile: Option<&'a Path>,
    network: NetworkMode,
    gpus: Vec<String>,
    shares: Vec<SharedPath>,
    keep_oci_archive: bool,
}

fn build_machine(paths: &WbPaths, name: &str, request: BuildMachineRequest<'_>) -> Result<()> {
    let BuildMachineRequest {
        context,
        containerfile,
        network,
        gpus,
        shares,
        keep_oci_archive,
    } = request;
    MachineConfig::validate_name(name)?;
    MachineConfig::validate_gpus(&gpus)?;
    let context = context
        .canonicalize()
        .with_context(|| format!("resolving Buildah context {}", context.display()))?;
    let metadata = fs::symlink_metadata(&context)
        .with_context(|| format!("inspecting Buildah context {}", context.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("Buildah context must be a real directory");
    }
    let requested_file = containerfile
        .map(PathBuf::from)
        .unwrap_or_else(|| context.join("Containerfile"));
    let requested_file = if requested_file.is_absolute() {
        requested_file
    } else {
        context.join(requested_file)
    };
    let containerfile = requested_file.canonicalize().with_context(|| {
        format!(
            "resolving selected Containerfile {}",
            requested_file.display()
        )
    })?;
    if !containerfile.is_file() {
        bail!("selected Containerfile is not a regular file");
    }

    let resources = ResourceLocator::discover()?;
    let buildah = resources.helper_or_path("buildah")?;
    let work = tempfile::Builder::new()
        .prefix(&format!(".{name}-buildah-"))
        .tempdir_in(paths.machines())
        .context("creating Buildah output staging directory")?;
    let iidfile = work.path().join("image.id");
    let archive = work.path().join("image.oci.tar");
    let storage = work.path().join("buildah-storage");
    let runroot = work.path().join("buildah-runroot");
    fs::create_dir(&storage).context("creating isolated Buildah storage")?;
    fs::create_dir(&runroot).context("creating isolated Buildah runroot")?;
    eprintln!(
        "Building {} with rootless Buildah…",
        containerfile.display()
    );
    let status = Command::new(&buildah)
        .arg("--root")
        .arg(&storage)
        .arg("--runroot")
        .arg(&runroot)
        .args(["--storage-driver", "vfs"])
        .args([
            "build",
            "--format",
            "oci",
            "--no-cache",
            "--pull=always",
            "--force-rm",
        ])
        .arg("--iidfile")
        .arg(&iidfile)
        .arg("--file")
        .arg(&containerfile)
        .arg(&context)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("starting {} build", buildah.display()))?;
    if !status.success() {
        bail!("Buildah build failed with {status}");
    }
    let image_id = fs::read_to_string(&iidfile)
        .context("reading Buildah image ID")?
        .trim()
        .to_owned();
    if !valid_buildah_image_id(&image_id) {
        bail!("Buildah returned an invalid image ID");
    }
    let destination = format!("oci-archive:{}", archive.display());
    let push_status = Command::new(&buildah)
        .arg("--root")
        .arg(&storage)
        .arg("--runroot")
        .arg(&runroot)
        .args(["--storage-driver", "vfs", "push", "--format", "oci"])
        .arg(&image_id)
        .arg(&destination)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("exporting Buildah image with {}", buildah.display()))?;
    if !push_status.success() {
        bail!("Buildah OCI export failed with {push_status}");
    }

    let source = archive
        .to_str()
        .context("Buildah OCI staging path is not UTF-8")?;
    let source_reference = format!("buildah:{}", containerfile.display());
    let result = import_machine(
        paths,
        name,
        ImportMachineRequest {
            source,
            selector: None,
            mode: ImportModeArg::Clone,
            source_reference_override: Some(&source_reference),
            shares,
            keep_oci_archive,
            network_override: Some(network),
            gpus_override: Some(gpus),
        },
    );
    cleanup_buildah_store(&buildah, &storage, &runroot);
    result
}

fn valid_buildah_image_id(image_id: &str) -> bool {
    let digest = image_id.strip_prefix("sha256:").unwrap_or(image_id);
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

struct ImportMachineRequest<'a> {
    source: &'a str,
    selector: Option<&'a str>,
    mode: ImportModeArg,
    source_reference_override: Option<&'a str>,
    shares: Vec<SharedPath>,
    keep_oci_archive: bool,
    network_override: Option<NetworkMode>,
    gpus_override: Option<Vec<String>>,
}

fn import_machine(paths: &WbPaths, name: &str, request: ImportMachineRequest<'_>) -> Result<()> {
    let ImportMachineRequest {
        source,
        selector,
        mode,
        source_reference_override,
        shares,
        keep_oci_archive,
        network_override,
        gpus_override,
    } = request;
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
        .prefix(&format!(".{name}-oci-import-"))
        .tempdir_in(paths.machines())
        .context("creating OCI import staging directory")?;

    let (layout, source_reference) = if source_path.exists() {
        let layout = if source_path.is_dir() {
            canonical_oci_layout(source_path)?
        } else {
            let extracted = source_stage.path().join("layout");
            fs::create_dir(&extracted).context("creating local OCI extraction directory")?;
            extract_oci_archive(source_path, &extracted)?;
            canonical_oci_layout(&extracted)?
        };
        (layout, local_oci_source_reference(source_path))
    } else {
        if selector.is_some() {
            bail!("--manifest is supported only for local OCI layouts and archives");
        }
        let platform = oci_platform()?;
        let archive = source_stage.path().join("remote.oci.tar");
        eprintln!("Pulling {source}…");
        pull_oci_archive(&resources, source, platform, &archive, source_stage.path())?;
        let extracted = source_stage.path().join("layout");
        fs::create_dir(&extracted).context("creating remote OCI extraction directory")?;
        extract_oci_archive(&archive, &extracted)?;
        (canonical_oci_layout(&extracted)?, format!("oci:{source}"))
    };
    let index = read_oci_index(&layout)?;
    let descriptor = resolve_oci_manifest_descriptor(&layout, &index, selector)?;
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
        let retained_oci_archive = keep_oci_archive
            .then(|| retain_oci_layout(&layout, stage.path()))
            .transpose()?;

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
        if mode == ImportModeArg::Restore && carries_portable_identity {
            reject_duplicate_machine_identity(config.id)?;
        } else {
            config.id = uuid::Uuid::new_v4();
            reset_cloned_machine_identity_in_stage(&resources, &rootfs)?;
        }
        config.name = name.to_owned();
        config.title = name.to_owned();
        config.image = source_reference.clone();
        config.image_digest = Some(digest.clone());
        sanitize_imported_machine_config(&mut config);
        if let Some(network) = network_override {
            config.network = network;
        }
        if let Some(gpus) = gpus_override.clone() {
            MachineConfig::validate_gpus(&gpus)?;
            config.gpus = gpus;
        }
        config.shares = shares;
        config.retained_oci_archive = retained_oci_archive;
        config.save(stage.path())?;
        RuntimeState::new(MachineState::Stopped).save(stage.path())?;
        File::create(stage.path().join("machine.lock")).context("creating machine lock")?;
        commit_new_machine(stage.path(), &final_dir)?;
        println!(
            "Imported '{name}' from {source} in {} mode\nMachine directory: {}\nPersistent rootfs: {}",
            match mode {
                ImportModeArg::Restore => "restore",
                ImportModeArg::Clone => "clone",
            },
            final_dir.display(),
            final_dir.join("rootfs").display()
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

fn retain_oci_layout(layout: &Path, machine_stage: &Path) -> Result<RetainedOciArchive> {
    let cache = machine_stage.join("cache");
    fs::create_dir_all(&cache).context("creating machine OCI cache")?;
    let archive_path = cache.join("source.oci.tar");
    let archive_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&archive_path)
        .with_context(|| format!("creating retained OCI archive {}", archive_path.display()))?;
    let mut archive = tar::Builder::new(archive_file);
    archive
        .append_dir_all(".", layout)
        .context("archiving verified OCI layout")?;
    let archive_file = archive
        .into_inner()
        .context("finishing retained OCI archive")?;
    archive_file
        .sync_all()
        .context("syncing retained OCI archive")?;

    let size = archive_file.metadata()?.len();
    drop(archive_file);
    let mut source = open_regular_nofollow(&archive_path, "retained OCI archive")?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let retained = RetainedOciArchive {
        relative_path: "cache/source.oci.tar".into(),
        sha256: format!("{:x}", hash.finalize()),
        size,
    };
    retained.validate()?;
    Ok(retained)
}

fn sanitize_imported_machine_config(config: &mut MachineConfig) {
    config.schema = 5;
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
    config.shares.clear();
    config.retained_oci_archive = None;
}

fn reject_duplicate_machine_identity(identity: uuid::Uuid) -> Result<()> {
    let registry = MachineRegistry::discover()?;
    if let Some(existing_name) = registered_machine_name_for_identity(&registry, identity) {
        bail!(
            "the imported machine identity already exists as '{existing_name}'; use `BuzzardOS clone {existing_name} NEW_NAME` to create an independent copy"
        );
    }
    Ok(())
}

fn registered_machine_name_for_identity(
    registry: &MachineRegistry,
    identity: uuid::Uuid,
) -> Option<&str> {
    // The registry is deliberately only an index, but its recorded UUID is
    // still authoritative for duplicate-restore protection. Do not silently
    // permit a second copy merely because the original machine directory is
    // currently moved, disconnected, or otherwise unreadable.
    registry
        .entries()
        .iter()
        .find(|entry| entry.id == identity)
        .map(|entry| entry.name.as_str())
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
        .take(MAX_OCI_LAYOUT_BYTES + 1)
        .read_to_end(&mut layout_bytes)
        .context("reading oci-layout")?;
    if layout_bytes.len() as u64 > MAX_OCI_LAYOUT_BYTES {
        bail!("oci-layout exceeds {MAX_OCI_LAYOUT_BYTES} bytes");
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
    fs::create_dir_all(paths.cache()).context("creating machine export cache")?;
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
    let mut namespace = PortableNamespaceContext::discover("OCI export")?;
    let rootfs = namespace.relative(&machine_dir.join("rootfs"), "machine rootfs")?;
    let machine_config = namespace.relative(&machine_dir, "machine configuration directory")?;
    // Build inside the portable cache, then copy through the already-open
    // host-user temporary file. This keeps arbitrary export destinations
    // usable even when their parent directories are private to the host UID.
    let staged_output = work.path().join("export.oci.tar.zst");
    let namespace_output = namespace.new_file(&staged_output, "staged OCI export")?;
    let namespace_work = namespace.relative(work.path(), "OCI export work directory")?;
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    namespace.configure(&mut command);
    command
        .args(id_map.namespace_args())
        .arg(&namespace.launcher)
        .arg("__export-oci")
        .arg("--rootfs")
        .arg(rootfs)
        .arg("--machine-config")
        .arg(machine_config)
        .arg("--output")
        .arg(namespace_output)
        .arg("--work-dir")
        .arg(namespace_work);
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
    let status = match status_result {
        Ok(status) => status,
        Err(start_error) => {
            return match cleanup_export_stage(&resources, work.path(), &paths.cache()) {
                Ok(()) => Err(start_error),
                Err(cleanup_error) => Err(anyhow::anyhow!(
                    "{start_error:#}; export staging cleanup also failed: {cleanup_error:#}"
                )),
            };
        }
    };
    if !status.success() {
        return match cleanup_export_stage(&resources, work.path(), &paths.cache()) {
            Ok(()) => Err(anyhow::anyhow!("OCI export namespace exited with {status}")),
            Err(cleanup_error) => Err(anyhow::anyhow!(
                "OCI export namespace exited with {status}; export staging cleanup failed: {cleanup_error:#}"
            )),
        };
    }
    let copy_result = (|| -> Result<()> {
        let mut source = File::open(&staged_output)
            .with_context(|| format!("opening staged export {}", staged_output.display()))?;
        temporary.as_file().set_len(0)?;
        let mut destination = temporary.as_file();
        destination.seek(SeekFrom::Start(0))?;
        std::io::copy(&mut source, &mut destination)
            .context("copying namespace export to destination filesystem")?;
        destination.flush()?;
        Ok(())
    })();
    let cleanup_result = cleanup_export_stage(&resources, work.path(), &paths.cache());
    match (copy_result, cleanup_result) {
        (Ok(()), Ok(())) => {}
        (Err(copy_error), Ok(())) => return Err(copy_error),
        (Ok(()), Err(cleanup_error)) => return Err(cleanup_error),
        (Err(copy_error), Err(cleanup_error)) => {
            bail!("{copy_error:#}; export staging cleanup also failed: {cleanup_error:#}")
        }
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
    let verified_supervisor = state
        .launcher_pid
        .filter(|pid| pid_alive(*pid) && broker_matches_machine(*pid, machine_dir));
    if let Some(pid) = verified_supervisor {
        if supervisor_is_live(&state, machine_dir) {
            send_host_control(machine_dir, "close")
                .context("closing the stopped machine window before export")?;
        } else {
            // Early launch failures can retain the native failure window and
            // machine lock before the host-control socket exists.  The PID is
            // safe to signal only after exact executable and machine-directory
            // verification above.
            signal_process(pid, libc::SIGTERM)
                .context("closing failed machine supervisor before export")?;
        }
        if !wait_for_process_exit(pid, Duration::from_secs(10)) {
            bail!("the stopped machine window did not close before export");
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
    let machine_config_metadata = fs::metadata(machine_config_path).with_context(|| {
        format!(
            "inspecting machine configuration path {}",
            machine_config_path.display()
        )
    })?;
    let machine_config_dir = if machine_config_metadata.is_dir() {
        machine_config_path
    } else {
        machine_config_path
            .parent()
            .context("machine config has no machine directory")?
    };
    let config = MachineConfig::load(machine_config_dir)?;
    let resources = ResourceLocator::discover()?;
    let tar = resources.helper_or_path("tar")?;
    let layout = work_dir.join("layout");
    let blob_dir = layout.join("blobs/sha256");
    fs::create_dir_all(&blob_dir).context("creating OCI layout blob directory")?;

    // A portable archive must never copy the running installation's machine
    // identity.  Export remains read-only with respect to its source: make an
    // exact private copy in this same guest-ID namespace, clear identity only
    // in that copy, validate it, and snapshot the copy.
    let export_rootfs =
        copy_rootfs_without_identity(&tar, rootfs, work_dir, generic_seed_source_date_epoch)?;

    let layer_temporary = work_dir.join("rootfs-layer.tar.zst");
    let (diff_digest, layer_digest, layer_size) =
        write_rootfs_layer(&tar, &export_rootfs, &layer_temporary)?;
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
                "identity-free persistent rootfs snapshot"
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
            "--sparse-version=0.1",
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
    enable_zstd_multithreading(&mut encoder);
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

fn copy_rootfs_without_identity(
    tar: &Path,
    source: &Path,
    work_dir: &Path,
    source_date_epoch: Option<i64>,
) -> Result<PathBuf> {
    let destination = work_dir.join("portable-rootfs");
    fs::create_dir(&destination).context("creating private identity-free rootfs stage")?;

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
            "--sparse-version=0.1",
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
            "--same-permissions",
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
            "private identity-free rootfs copy failed: writer={producer_status}, reader={consumer_status}"
        );
    }

    reset_cloned_rootfs_identity(&destination)
        .context("clearing identity from portable rootfs staging")?;
    if let Some(source_date_epoch) = source_date_epoch {
        for relative in ["", "etc/machine-id", "etc", "etc/ssh", "var/lib/systemd"] {
            let path = destination.join(relative);
            match fs::symlink_metadata(&path) {
                Ok(_) => set_link_mtime(&path, (source_date_epoch, 0))?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    validate_identity_free_rootfs(&destination)?;
    Ok(destination)
}

fn validate_identity_free_rootfs(rootfs: &Path) -> Result<()> {
    let machine_id = rootfs.join("etc/machine-id");
    if !fs::read(&machine_id)
        .with_context(|| format!("reading portable machine ID {}", machine_id.display()))?
        .is_empty()
    {
        bail!("portable staging rootfs retains a machine ID");
    }
    if rootfs.join("var/lib/systemd/random-seed").exists() {
        bail!("portable staging rootfs retains a systemd random seed");
    }
    let ssh = rootfs.join("etc/ssh");
    match fs::symlink_metadata(&ssh) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            for entry in fs::read_dir(&ssh)? {
                if entry?.file_name().as_bytes().starts_with(b"ssh_host_") {
                    bail!("portable staging rootfs retains an SSH host identity");
                }
            }
        }
        Ok(_) => bail!("portable staging SSH directory has an unsafe type"),
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
        .create(true)
        .truncate(true)
        .mode(0o600)
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
    // OCI layer blobs are already compressed.  A high outer level spends
    // minutes recompressing largely incompressible data for negligible gain;
    // use a fast deterministic wrapper while keeping the layer compression
    // level unchanged.
    let mut encoder = zstd::stream::write::Encoder::new(file, 3)
        .context("initializing OCI archive compressor")?;
    enable_zstd_multithreading(&mut encoder);
    std::io::copy(&mut BufReader::new(stdout), &mut encoder)
        .context("compressing OCI layout archive")?;
    let file = encoder
        .finish()
        .context("finishing OCI archive compression")?;
    // The namespace root maps to a subordinate host ID. The outer host-user
    // process copies this file from its mode-0700 portable cache into the
    // user-selected atomic destination, so the completed staging file must be
    // readable without making the cache directory itself public.
    file.set_permissions(fs::Permissions::from_mode(0o644))?;
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

fn enable_zstd_multithreading<W: Write>(encoder: &mut zstd::stream::write::Encoder<'_, W>) {
    // Some supported hosts provide a libzstd build without pthread support.
    // Worker selection is only a performance hint: the encoder remains valid
    // and deterministic with its default single worker after this call fails.
    let _ = encoder.multithread(zstd_worker_count());
}

fn reject_rootfs_submounts(rootfs: &Path) -> Result<()> {
    let canonical = inherited_descriptor_target(rootfs)?.map_or_else(
        || {
            rootfs
                .canonicalize()
                .with_context(|| format!("resolving export rootfs {}", rootfs.display()))
        },
        Ok,
    )?;
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

fn inherited_descriptor_target(path: &Path) -> Result<Option<PathBuf>> {
    let Ok(relative) = path.strip_prefix("/proc/self/fd") else {
        return Ok(None);
    };
    let mut components = relative.components();
    let Some(std::path::Component::Normal(descriptor)) = components.next() else {
        return Ok(None);
    };
    if components.next().is_some()
        || descriptor.is_empty()
        || !descriptor.as_bytes().iter().all(u8::is_ascii_digit)
    {
        return Ok(None);
    }
    let descriptor_path = Path::new("/proc/self/fd").join(descriptor);
    let target = fs::read_link(&descriptor_path).with_context(|| {
        format!(
            "resolving inherited rootfs descriptor {}",
            descriptor_path.display()
        )
    })?;
    if !target.is_absolute() {
        bail!("inherited rootfs descriptor resolved to a relative path");
    }
    Ok(Some(target))
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

fn clone_machine(
    source_paths: &WbPaths,
    destination_paths: &WbPaths,
    source: &str,
    name: &str,
    shares: Vec<SharedPath>,
) -> Result<()> {
    MachineConfig::validate_name(name)?;
    fs::create_dir_all(source_paths.cache()).context("creating source machine cache")?;
    let temporary = tempfile::Builder::new()
        .prefix("clone-")
        .suffix(".oci.tar.zst")
        .tempfile_in(source_paths.cache())?;
    let archive_path = temporary.path().to_path_buf();
    // `export_machine` deliberately refuses replacement. Close removes only
    // this private placeholder before handing its randomized name to the
    // exporter; an intervening collision is then rejected rather than
    // replaced.
    temporary
        .close()
        .context("releasing the clone export placeholder")?;
    export_machine(source_paths, source, &archive_path, None)?;
    let result = import_machine(
        destination_paths,
        name,
        ImportMachineRequest {
            source: archive_path
                .to_str()
                .context("clone archive path is not UTF-8")?,
            selector: None,
            mode: ImportModeArg::Clone,
            source_reference_override: Some(&format!("clone:{source}")),
            shares,
            keep_oci_archive: false,
            network_override: None,
            gpus_override: None,
        },
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
    let namespace = PortableNamespaceContext::discover("clone identity reset")?;
    let rootfs = rootfs
        .canonicalize()
        .with_context(|| format!("resolving cloned rootfs {}", rootfs.display()))?;
    let parent = rootfs.parent().context("cloned rootfs has no parent")?;
    let name = rootfs
        .file_name()
        .context("cloned rootfs has no directory name")?;
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    let status = command
        .current_dir(parent)
        .args(id_map.namespace_args())
        .arg(&namespace.launcher)
        .arg("__reset-clone-identity")
        .arg("--rootfs")
        .arg(name)
        .stdin(Stdio::null())
        .status()?;
    if !status.success() {
        bail!("clone identity reset failed with {status}");
    }
    Ok(())
}

fn reset_cloned_rootfs_identity(rootfs: &Path) -> Result<()> {
    validate_guest_rootfs(rootfs)?;
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
    file.flush()
        .with_context(|| format!("flushing reset machine ID {}", machine_id.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing reset machine ID {}", machine_id.display()))?;

    for relative in ["var/lib/systemd/random-seed", "var/lib/dbus/machine-id"] {
        let path = rootfs.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                // Preserve the normal /var/lib/dbus/machine-id -> /etc/machine-id
                // link; the target was reset above. Other identity material is
                // removed only when it is a regular file.
                if relative != "var/lib/dbus/machine-id" {
                    fs::remove_file(&path)
                        .with_context(|| format!("removing identity link {}", path.display()))?;
                }
            }
            Ok(metadata) if metadata.is_file() => fs::remove_file(&path)
                .with_context(|| format!("removing identity file {}", path.display()))?,
            Ok(_) => bail!("clone identity path {} has an unsafe type", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let ssh = rootfs.join("etc/ssh");
    match fs::symlink_metadata(&ssh) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            for entry in fs::read_dir(&ssh)
                .with_context(|| format!("reading SSH identity directory {}", ssh.display()))?
            {
                let entry = entry.context("reading SSH identity entry")?;
                let name = entry.file_name();
                if name.as_bytes().starts_with(b"ssh_host_") {
                    let metadata = fs::symlink_metadata(entry.path()).with_context(|| {
                        format!("inspecting SSH identity entry {}", entry.path().display())
                    })?;
                    if metadata.is_file() || metadata.file_type().is_symlink() {
                        fs::remove_file(entry.path()).with_context(|| {
                            format!("removing SSH identity entry {}", entry.path().display())
                        })?;
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
    sync_parent_directory(&machine_id).with_context(|| {
        format!(
            "syncing machine identity directory for {}",
            machine_id.display()
        )
    })?;
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
    let mut namespace = PortableNamespaceContext::discover("machine staging cleanup")?;
    let staging_name = staging
        .file_name()
        .context("machine staging directory has no name")?
        .to_owned();
    let namespace_machines = namespace.relative(machines, "Machines directory")?;
    let namespace_staging = namespace_machines.join(staging_name);
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    namespace.configure(&mut command);
    let status = command
        .args(id_map.namespace_args())
        .arg(&namespace.launcher)
        .arg("__cleanup-staging")
        .arg("--staging")
        .arg(namespace_staging)
        .arg("--machines")
        .arg(namespace_machines)
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
    let mut namespace = PortableNamespaceContext::discover("export staging cleanup")?;
    let staging_name = staging
        .file_name()
        .context("export staging directory has no name")?
        .to_owned();
    let namespace_cache = namespace.relative(cache, "portable cache")?;
    let namespace_staging = namespace_cache.join(staging_name);
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    namespace.configure(&mut command);
    let status = command
        .args(id_map.namespace_args())
        .arg(&namespace.launcher)
        .arg("__cleanup-export-staging")
        .arg("--staging")
        .arg(namespace_staging)
        .arg("--cache")
        .arg(namespace_cache)
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
    let mut namespace = PortableNamespaceContext::discover("machine deletion")?;
    let machine_name = machine_dir
        .file_name()
        .context("machine directory has no name")?
        .to_owned();
    let machines = namespace.relative(&paths.machines(), "Machines directory")?;
    let machine_dir = machines.join(machine_name);
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    namespace.configure(&mut command);
    let status = command
        .args(id_map.namespace_args())
        .arg(&namespace.launcher)
        .arg("__delete-machine")
        .arg("--machine")
        .arg(&machine_dir)
        .arg("--machines")
        .arg(machines)
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
    let machines_metadata = fs::symlink_metadata(machines)
        .with_context(|| format!("inspecting machine parent {}", machines.display()))?;
    if machines_metadata.file_type().is_symlink() || !machines_metadata.is_dir() {
        bail!("machine parent must be a real directory");
    }
    let parent = machine.parent().context("machine has no parent")?;
    // `Path::parent` can lexically remove the trailing `/.` from an inherited
    // `/proc/self/fd/N/.` directory, leaving `/proc/self/fd/N` as the parent.
    // Follow that procfs magic link for this identity comparison; the target
    // still has to be the same device/inode as the separately validated
    // selected parent below.
    let parent_metadata = fs::metadata(parent).context("inspecting machine deletion parent")?;
    // Portable deletion addresses the selected parent through an inherited
    // /proc/self/fd descriptor. Canonicalizing that descriptor would resolve
    // it back through host-private ancestors which subordinate guest root is
    // intentionally unable to traverse. Device/inode identity proves it is
    // the same already-open directory without widening namespace access.
    if (parent_metadata.dev(), parent_metadata.ino())
        != (machines_metadata.dev(), machines_metadata.ino())
    {
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
    File::open(machines)?.sync_all()?;
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
    let actual_parent = staging
        .parent()
        .context("machine staging path has no parent")?;
    // `parent()` may strip the trailing `/.` from an inherited procfs
    // descriptor path. Follow that exact descriptor for the identity check;
    // it must still resolve to the separately validated selected directory.
    let actual_parent_metadata =
        fs::metadata(actual_parent).context("inspecting machine staging parent")?;
    if (actual_parent_metadata.dev(), actual_parent_metadata.ino())
        != (machines_metadata.dev(), machines_metadata.ino())
    {
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
    let actual_parent = staging
        .parent()
        .context("export staging path has no parent")?;
    // See the machine-staging check above: an inherited `/proc/self/fd/N/.`
    // parent can become `/proc/self/fd/N` after lexical parent extraction.
    let actual_parent_metadata =
        fs::metadata(actual_parent).context("inspecting export staging parent")?;
    if (actual_parent_metadata.dev(), actual_parent_metadata.ino())
        != (cache_metadata.dev(), cache_metadata.ino())
    {
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

/// Give namespace-only code an absolute path whose ancestors remain
/// searchable by subordinate guest root. The source directory is already
/// pinned as the child process's cwd before the UID map changes; a private
/// bind mount exposes only that exact directory inside this disposable mount
/// namespace. This is necessary because `realpath(3)` retraverses absolute
/// host ancestors even when relative lookup from the inherited cwd succeeds.
fn with_private_bind_mount<T>(
    source: &Path,
    description: &str,
    operation: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
    let temporary = tempfile::Builder::new()
        .prefix("buzzardos-namespace-")
        .tempdir_in("/tmp")
        .with_context(|| format!("creating private {description} mount point"))?;
    let target = temporary.path().join("rootfs");
    fs::create_dir(&target)
        .with_context(|| format!("creating private {description} mount target"))?;
    let source_c = CString::new(source.as_os_str().as_bytes())
        .context("private namespace mount source contains a NUL byte")?;
    let target_c = CString::new(target.as_os_str().as_bytes())
        .context("private namespace mount target contains a NUL byte")?;
    let mounted = unsafe {
        libc::mount(
            source_c.as_ptr(),
            target_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        )
    };
    if mounted != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("binding private {description} mount"));
    }

    let result = operation(&target);
    let unmounted = unsafe { libc::umount2(target_c.as_ptr(), libc::MNT_DETACH) };
    if unmounted != 0 {
        let cleanup = std::io::Error::last_os_error();
        return match result {
            Ok(_) => Err(cleanup).with_context(|| format!("unmounting private {description}")),
            Err(error) => Err(error).context(format!(
                "private {description} operation also failed to unmount: {cleanup}"
            )),
        };
    }
    result
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
        "usr/bin/sway",
        "usr/bin/swaymsg",
        "usr/bin/buzzardoscua",
        "opt/buzzardos/runtime/current/libexec/buzzardos-clipboard-agent",
        "usr/bin/buzzardos-settings",
        "usr/bin/buzzardos-desktop",
        "usr/libexec/buzzardos-shortcut-helper",
        "var/lib/dpkg/status",
    ] {
        let path = rootfs.join(required);
        let resolved = path
            .canonicalize()
            .with_context(|| format!("bundled rootfs is missing required file /{required}"))?;
        if !resolved.starts_with(&canonical_rootfs) {
            bail!("bundled rootfs /{required} escapes through a symlink");
        }
        // Access through the caller-supplied path rather than the absolute
        // canonical result. Namespace helpers deliberately inherit a working
        // directory inside the selected machine so a subordinate guest-root
        // identity never has to retraverse a private host ancestor such as a
        // mode-0700 home directory.
        let metadata = fs::metadata(&path)
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
    let mut namespace = PortableNamespaceContext::discover("OCI extraction")?;
    let archive = namespace.relative(archive, "OCI layout")?;
    let work_dir = work_dir
        .canonicalize()
        .with_context(|| format!("resolving OCI work directory {}", work_dir.display()))?;
    let rootfs = rootfs
        .canonicalize()
        .with_context(|| format!("resolving rootfs {}", rootfs.display()))?;
    let relative_rootfs = rootfs
        .strip_prefix(&work_dir)
        .context("rootfs is outside its OCI work directory")?;
    if relative_rootfs.components().count() != 1 {
        bail!("rootfs must be a direct child of its OCI work directory");
    }
    let mut command = Command::new(namespace_program);
    id_map.configure_command(&mut command);
    let status = command
        // The host user opens the new rootfs before entering the user
        // namespace. Using it as cwd avoids searching the mode-0700 staging
        // directory after guest root has become a subordinate host ID. The
        // extractor first changes `.` to guest-root ownership, then uses it
        // for its transient layer files as well as the final filesystem.
        // Relative paths therefore remain reachable
        // even when a subordinate guest-root ID cannot retraverse a private
        // host ancestor.
        .current_dir(&rootfs)
        .args(id_map.namespace_args())
        .arg(&namespace.launcher)
        .arg("__apply-image")
        .arg("--archive")
        .arg(archive)
        .arg("--expected-digest")
        .arg(expected_digest)
        .arg("--rootfs")
        .arg(".")
        .arg("--work-dir")
        .arg(".")
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

struct PortableNamespaceContext {
    launcher: PathBuf,
    descriptors: Vec<File>,
}

impl PortableNamespaceContext {
    fn discover(kind: &str) -> Result<Self> {
        let launcher =
            std::env::current_exe().with_context(|| format!("locating launcher for {kind}"))?;
        let launcher = launcher
            .canonicalize()
            .with_context(|| format!("resolving launcher for {kind}"))?;
        Ok(Self {
            launcher,
            descriptors: Vec::new(),
        })
    }

    fn relative(&mut self, path: &Path, kind: &str) -> Result<PathBuf> {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolving {kind} {}", path.display()))?;
        self.inherit_path(&canonical, kind)
    }

    fn new_file(&mut self, path: &Path, kind: &str) -> Result<PathBuf> {
        let name = path
            .file_name()
            .filter(|name| !name.is_empty())
            .context("new private namespace file has no name")?;
        if path.exists() {
            bail!("{kind} already exists: {}", path.display());
        }
        let parent = path
            .parent()
            .context("new private namespace file has no parent")?
            .canonicalize()
            .with_context(|| format!("resolving {kind} parent"))?;
        Ok(self.inherit_path(&parent, kind)?.join(name))
    }

    /// Keep an already-resolved path open across `unshare`/`exec` and address
    /// it through procfs. Guest root maps to a subordinate host ID, so it
    /// cannot retraverse a host-private ancestor such as a mode-0700 home or
    /// encrypted data mount. An inherited descriptor preserves access only to
    /// the exact path the host user opened; it does not widen access to any
    /// sibling or parent directory.
    fn inherit_path(&mut self, path: &Path, kind: &str) -> Result<PathBuf> {
        let descriptor = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("opening {kind} {}", path.display()))?;
        let fd = descriptor.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("inspecting inherited {kind} descriptor"));
        }
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("preserving inherited {kind} descriptor"));
        }
        let is_directory = descriptor.metadata()?.is_dir();
        self.descriptors.push(descriptor);
        let inherited = PathBuf::from(format!("/proc/self/fd/{fd}"));
        // Address a directory through a child lookup so `symlink_metadata`
        // observes the opened directory, not procfs's descriptor symlink.
        // The latter is deliberately rejected by rootfs safety validation.
        Ok(if is_directory {
            inherited.join(".")
        } else {
            inherited
        })
    }

    fn configure(&self, command: &mut Command) {
        command.current_dir("/");
    }
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

#[derive(Debug)]
enum GnuSparseMap {
    Pax(Vec<(u64, u64)>),
    EmbeddedV1,
}

#[derive(Debug)]
struct GnuSparseMetadata {
    path: PathBuf,
    real_size: u64,
    map: GnuSparseMap,
}

fn parse_sparse_decimal(value: &[u8], description: &str) -> Result<u64> {
    if value.is_empty() || value.len() > 20 || !value.iter().all(u8::is_ascii_digit) {
        bail!("OCI GNU sparse {description} is not an unsigned decimal integer");
    }
    std::str::from_utf8(value)
        .context("OCI GNU sparse decimal value is not ASCII")?
        .parse()
        .with_context(|| format!("OCI GNU sparse {description} overflows u64"))
}

fn validate_sparse_extents(extents: &[(u64, u64)], real_size: u64) -> Result<()> {
    if extents.len() as u64 > MAX_OCI_SPARSE_EXTENTS {
        bail!("OCI GNU sparse map exceeds {MAX_OCI_SPARSE_EXTENTS} extents");
    }
    let mut previous_end = 0_u64;
    for &(offset, length) in extents {
        let end = offset
            .checked_add(length)
            .context("OCI GNU sparse extent overflows u64")?;
        if offset < previous_end || end > real_size {
            bail!("OCI GNU sparse extents overlap, are out of order, or exceed the real size");
        }
        previous_end = end;
    }
    Ok(())
}

fn parse_pax_sparse_map(value: &[u8], real_size: u64) -> Result<Vec<(u64, u64)>> {
    let fields = value.split(|byte| *byte == b',').collect::<Vec<_>>();
    if fields.is_empty() || fields.len() % 2 != 0 {
        bail!("OCI GNU sparse 0.1 map must contain offset/length pairs");
    }
    if fields.len() as u64 / 2 > MAX_OCI_SPARSE_EXTENTS {
        bail!("OCI GNU sparse map exceeds {MAX_OCI_SPARSE_EXTENTS} extents");
    }
    let mut extents = Vec::with_capacity(fields.len() / 2);
    for pair in fields.chunks_exact(2) {
        extents.push((
            parse_sparse_decimal(pair[0], "extent offset")?,
            parse_sparse_decimal(pair[1], "extent length")?,
        ));
    }
    validate_sparse_extents(&extents, real_size)?;
    Ok(extents)
}

fn parse_gnu_sparse_metadata(
    pax: &PaxRecords,
    archive_path: &Path,
) -> Result<Option<GnuSparseMetadata>> {
    let sparse_keys = pax
        .keys()
        .filter(|key| key.starts_with(b"GNU.sparse."))
        .collect::<Vec<_>>();
    if sparse_keys.is_empty() {
        return Ok(None);
    }
    for key in sparse_keys {
        if !matches!(
            key.as_slice(),
            b"GNU.sparse.major"
                | b"GNU.sparse.minor"
                | b"GNU.sparse.name"
                | b"GNU.sparse.realsize"
                | b"GNU.sparse.size"
                | b"GNU.sparse.numblocks"
                | b"GNU.sparse.map"
        ) {
            bail!(
                "OCI layer contains unsupported GNU sparse metadata {}",
                String::from_utf8_lossy(key)
            );
        }
    }
    let major = pax.get(b"GNU.sparse.major".as_slice());
    let minor = pax.get(b"GNU.sparse.minor".as_slice());
    let path = pax.get(b"GNU.sparse.name".as_slice()).map_or_else(
        || archive_path.to_path_buf(),
        |path| PathBuf::from(OsStr::from_bytes(path)),
    );
    let real_size = pax
        .get(b"GNU.sparse.realsize".as_slice())
        .or_else(|| pax.get(b"GNU.sparse.size".as_slice()))
        .context("OCI GNU sparse metadata has no real size")?;
    let real_size = parse_sparse_decimal(real_size, "real size")?;
    let map = match (major.map(Vec::as_slice), minor.map(Vec::as_slice)) {
        (Some(b"1"), Some(b"0")) => {
            if pax.contains_key(b"GNU.sparse.map".as_slice()) {
                bail!("OCI GNU sparse 1.0 entry unexpectedly carries a PAX map");
            }
            GnuSparseMap::EmbeddedV1
        }
        (Some(b"0"), Some(b"1")) | (None, None)
            if pax.contains_key(b"GNU.sparse.map".as_slice()) =>
        {
            let extents = parse_pax_sparse_map(
                pax.get(b"GNU.sparse.map".as_slice())
                    .context("OCI GNU sparse 0.1 entry has no map")?,
                real_size,
            )?;
            let count = parse_sparse_decimal(
                pax.get(b"GNU.sparse.numblocks".as_slice())
                    .context("OCI GNU sparse 0.1 entry has no extent count")?,
                "extent count",
            )?;
            if count != extents.len() as u64 {
                bail!("OCI GNU sparse extent count does not match its map");
            }
            GnuSparseMap::Pax(extents)
        }
        (Some(major), Some(minor)) => bail!(
            "OCI GNU sparse format {}.{} is unsupported",
            String::from_utf8_lossy(major),
            String::from_utf8_lossy(minor)
        ),
        _ => bail!("OCI GNU sparse metadata has an incomplete or unsupported version"),
    };
    Ok(Some(GnuSparseMetadata {
        path,
        real_size,
        map,
    }))
}

fn read_sparse_decimal_line(reader: &mut dyn Read, description: &str) -> Result<(u64, usize)> {
    let mut value = Vec::with_capacity(20);
    loop {
        let mut byte = [0_u8; 1];
        reader
            .read_exact(&mut byte)
            .with_context(|| format!("reading OCI GNU sparse {description}"))?;
        if byte[0] == b'\n' {
            return Ok((parse_sparse_decimal(&value, description)?, value.len() + 1));
        }
        if value.len() == 20 || !byte[0].is_ascii_digit() {
            bail!("OCI GNU sparse {description} has an invalid decimal line");
        }
        value.push(byte[0]);
    }
}

fn read_embedded_sparse_map(reader: &mut dyn Read, real_size: u64) -> Result<Vec<(u64, u64)>> {
    let (count, mut prefix_bytes) = read_sparse_decimal_line(reader, "extent count")?;
    if count > MAX_OCI_SPARSE_EXTENTS {
        bail!("OCI GNU sparse map exceeds {MAX_OCI_SPARSE_EXTENTS} extents");
    }
    let mut extents = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let (offset, offset_bytes) = read_sparse_decimal_line(reader, "extent offset")?;
        let (length, length_bytes) = read_sparse_decimal_line(reader, "extent length")?;
        prefix_bytes = prefix_bytes
            .checked_add(offset_bytes)
            .and_then(|bytes| bytes.checked_add(length_bytes))
            .context("OCI GNU sparse map prefix overflows usize")?;
        extents.push((offset, length));
    }
    validate_sparse_extents(&extents, real_size)?;
    let padding = (512 - prefix_bytes % 512) % 512;
    let mut remaining = padding;
    let mut buffer = [0_u8; 512];
    while remaining != 0 {
        let count = remaining.min(buffer.len());
        reader
            .read_exact(&mut buffer[..count])
            .context("reading OCI GNU sparse map padding")?;
        if buffer[..count].iter().any(|byte| *byte != 0) {
            bail!("OCI GNU sparse map has non-zero padding");
        }
        remaining -= count;
    }
    Ok(extents)
}

fn ensure_rootfs_file_parent(rootfs: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = rootfs.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let component = match component {
                std::path::Component::CurDir => continue,
                std::path::Component::Normal(component) => component,
                _ => bail!("OCI sparse entry has an unsafe parent path"),
            };
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => bail!(
                    "OCI sparse entry parent {} is not a real directory",
                    current.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&current).with_context(|| {
                        format!("creating OCI sparse entry parent {}", current.display())
                    })?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspecting OCI sparse entry parent {}", current.display())
                    });
                }
            }
        }
    }
    Ok(rootfs.join(relative))
}

fn unpack_gnu_sparse_entry(
    reader: &mut dyn Read,
    rootfs: &Path,
    relative: &Path,
    metadata: GnuSparseMetadata,
    uid: u32,
    gid: u32,
    mode: u32,
) -> Result<()> {
    let extents = match metadata.map {
        GnuSparseMap::Pax(extents) => extents,
        GnuSparseMap::EmbeddedV1 => read_embedded_sparse_map(reader, metadata.real_size)?,
    };
    let destination = ensure_rootfs_file_parent(rootfs, relative)?;
    match fs::symlink_metadata(&destination) {
        Ok(_) => remove_path(&destination)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspecting OCI sparse entry {}", relative.display()));
        }
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&destination)
        .with_context(|| format!("creating OCI sparse entry {}", relative.display()))?;
    output
        .set_len(metadata.real_size)
        .with_context(|| format!("sizing OCI sparse entry {}", relative.display()))?;
    let mut buffer = [0_u8; 256 * 1024];
    for (offset, length) in extents {
        output
            .seek(SeekFrom::Start(offset))
            .with_context(|| format!("seeking OCI sparse entry {}", relative.display()))?;
        let mut remaining = length;
        while remaining != 0 {
            let count = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
            reader
                .read_exact(&mut buffer[..count])
                .with_context(|| format!("reading OCI sparse entry {}", relative.display()))?;
            output
                .write_all(&buffer[..count])
                .with_context(|| format!("writing OCI sparse entry {}", relative.display()))?;
            remaining -= count as u64;
        }
    }
    let mut trailing = [0_u8; 1];
    if reader
        .read(&mut trailing)
        .context("checking OCI sparse entry payload length")?
        != 0
    {
        bail!("OCI GNU sparse entry has trailing physical data");
    }
    chown(
        &destination,
        Some(Uid::from_raw(uid)),
        Some(Gid::from_raw(gid)),
    )
    .with_context(|| format!("preserving ownership on {}", destination.display()))?;
    fs::set_permissions(&destination, fs::Permissions::from_mode(mode))
        .with_context(|| format!("preserving permissions on {}", destination.display()))?;
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
        let header_path = if let Some(path) = pax.get(b"path".as_slice()) {
            PathBuf::from(OsStr::from_bytes(path))
        } else {
            entry.path().context("reading OCI layer path")?.into_owned()
        };
        let sparse = parse_gnu_sparse_metadata(&pax, &header_path)?;
        let entry_path = sparse
            .as_ref()
            .map_or_else(|| header_path.clone(), |sparse| sparse.path.clone());
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
        if let Some(sparse) = sparse {
            if !matches!(
                entry_type,
                tar::EntryType::Regular | tar::EntryType::Continuous
            ) {
                bail!(
                    "OCI GNU sparse metadata describes a non-regular entry {}",
                    path.display()
                );
            }
            unpack_gnu_sparse_entry(
                &mut entry, rootfs, path, sparse, uid as u32, gid as u32, mode,
            )?;
        } else if !entry
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

fn validate_guest_rootfs(rootfs: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(rootfs)
        .with_context(|| format!("inspecting guest rootfs {}", rootfs.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("guest rootfs must be a real directory");
    }
    Ok(())
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
            validate_extracted_rootfs(&rootfs)?;
            for diagnostic in guest_settings_runtime_diagnostics(&rootfs)? {
                eprintln!("buzzardos: {diagnostic}");
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
    validate_extracted_rootfs(&rootfs)?;
    for diagnostic in guest_settings_runtime_diagnostics(&rootfs)? {
        eprintln!("buzzardos: {diagnostic}");
    }

    let current = std::env::current_exe().context("locating launcher")?;
    let broker = current
        .parent()
        .context("launcher path has no parent")?
        .join("buzzardos-broker");
    let broker = if broker.is_file() {
        broker
    } else {
        ResourceLocator::discover()?.helper_or_path("buzzardos-broker")?
    };

    let mut command = Command::new(&broker);
    command.arg("run").arg("--machine-dir").arg(&machine_dir);
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
    wait_for_detached_start(
        &machine_dir,
        &mut child,
        broker_pid,
        Duration::from_secs(95),
    )?;
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
    if config.shares.is_empty() {
        println!("shared paths: none");
    } else {
        println!("shared paths:");
        for share in &config.shares {
            println!(
                "  {} -> {} ({})",
                share.host_path.display(),
                share.guest_path().display(),
                if share.read_only {
                    "read-only"
                } else {
                    "read-write"
                }
            );
        }
    }
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

fn list(registry: &MachineRegistry) -> Result<()> {
    let mut found = false;
    for entry in registry.entries() {
        if let Ok(config) = MachineConfig::load(&entry.machine_dir) {
            let state = RuntimeState::load(&entry.machine_dir)?
                .map(|s| format!("{:?}", s.state))
                .unwrap_or_else(|| "Unknown".into());
            println!(
                "{}\t{}\t{}\t{}",
                config.name,
                state,
                entry.machine_dir.display(),
                config.image
            );
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
            "rootless Buildah OCI tools",
            resources.helper_or_path("buildah").is_ok(),
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
        .is_some_and(|field| field.ends_with(b"buzzardos-broker"))
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use tar::{Builder, EntryType, Header};

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
    fn guest_installer_output_satisfies_seed_validator() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        let binaries = temp.path().join("binaries");
        fs::create_dir(&rootfs).unwrap();
        fs::create_dir(&binaries).unwrap();
        let shell = binaries.join("buzzardos-desktop");
        let settings = binaries.join("buzzardos-settings");
        let shortcut_helper = binaries.join("buzzardos-shortcut-helper");
        let clipboard_agent = binaries.join("buzzardos-clipboard-agent");
        for executable in [&shell, &settings, &shortcut_helper, &clipboard_agent] {
            fs::write(executable, b"#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .canonicalize()
            .unwrap();
        let status = Command::new("sh")
            .arg(repository.join("guest/install-rootfs-assets.sh"))
            .arg(&rootfs)
            .arg(&clipboard_agent)
            .status()
            .unwrap();
        assert!(status.success());
        let status = Command::new("sh")
            .arg(repository.join("guest/install-desktop-assets.sh"))
            .arg(&rootfs)
            .arg(&shell)
            .arg(&settings)
            .arg(&shortcut_helper)
            .status()
            .unwrap();
        assert!(status.success());
        for required in [
            "lib/systemd/systemd",
            "usr/bin/sway",
            "usr/bin/swaymsg",
            "usr/bin/buzzardoscua",
            "var/lib/dpkg/status",
        ] {
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
        let user_state = rootfs.join("home/buzzardos/important.txt");
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

    #[test]
    fn create_requires_an_explicit_oci_image() {
        let parsed =
            Cli::try_parse_from(["BuzzardOS", "--machine-dir", "/data/demo", "create", "demo"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn accepts_the_buildah_iidfile_digest_formats() {
        let digest = "a".repeat(64);
        assert!(valid_buildah_image_id(&digest));
        assert!(valid_buildah_image_id(&format!("sha256:{digest}")));
        assert!(!valid_buildah_image_id("sha256:short"));
        assert!(!valid_buildah_image_id(&format!(
            "sha256:{}z",
            "a".repeat(63)
        )));
    }

    #[test]
    fn deletes_an_exact_machine_below_its_selected_parent() {
        let temp = tempfile::tempdir().unwrap();
        let machine = temp.path().join("delete-me");
        fs::create_dir(&machine).unwrap();
        MachineConfig::new(
            "delete-me".into(),
            "fixture".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        )
        .save(&machine)
        .unwrap();

        remove_persistent_machine_tree(&machine, temp.path()).unwrap();

        assert!(!machine.exists());
    }

    #[test]
    fn deletes_a_machine_below_an_inherited_parent_descriptor() {
        let temp = tempfile::tempdir().unwrap();
        let machine = temp.path().join("delete-through-descriptor");
        fs::create_dir(&machine).unwrap();
        MachineConfig::new(
            "delete-through-descriptor".into(),
            "fixture".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        )
        .save(&machine)
        .unwrap();
        let parent_descriptor = File::open(temp.path()).unwrap();
        let inherited_parent =
            PathBuf::from(format!("/proc/self/fd/{}/.", parent_descriptor.as_raw_fd()));
        let inherited_machine = inherited_parent.join("delete-through-descriptor");

        remove_persistent_machine_tree(&inherited_machine, &inherited_parent).unwrap();

        assert!(!machine.exists());
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
                .append_pax_extensions([("SCHILY.xattr.user.buzzardos", b"kept".as_slice())])
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
        let name = c"user.buzzardos";
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
    fn imports_gnu_sparse_1_0_without_materializing_its_transport_name() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let sparse_path = source.join("var/lib/buzzard/sparse.img");
        fs::create_dir_all(sparse_path.parent().unwrap()).unwrap();
        let mut sparse = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&sparse_path)
            .unwrap();
        sparse.set_len(8 * 1024 * 1024).unwrap();
        sparse.write_all(b"sparse-head").unwrap();
        sparse.seek(SeekFrom::End(-11)).unwrap();
        sparse.write_all(b"sparse-tail").unwrap();
        drop(sparse);

        let layer = temp.path().join("layer.tar");
        let tar = ResourceLocator::discover()
            .unwrap()
            .helper_or_path("tar")
            .unwrap();
        let status = Command::new(tar)
            .args([
                "--create",
                "--file",
                layer.to_str().unwrap(),
                "--directory",
                source.to_str().unwrap(),
                "--format=pax",
                "--numeric-owner",
                "--sparse",
                "--sparse-version=1.0",
                ".",
            ])
            .status()
            .unwrap();
        assert!(status.success());

        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        apply_layer(&layer, &rootfs).unwrap();

        let restored_path = rootfs.join("var/lib/buzzard/sparse.img");
        let restored = fs::metadata(&restored_path).unwrap();
        assert_eq!(restored.len(), 8 * 1024 * 1024);
        assert!(restored.blocks() * 512 < restored.len());
        assert!(
            fs::read_dir(rootfs.join("var/lib/buzzard"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .as_bytes()
                    .starts_with(b"GNUSparseFile"))
        );
        let mut restored = File::open(restored_path).unwrap();
        let mut head = [0_u8; 11];
        restored.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"sparse-head");
        restored.seek(SeekFrom::End(-11)).unwrap();
        let mut tail = [0_u8; 11];
        restored.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"sparse-tail");
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
    fn duplicate_restore_identity_remains_reserved_when_machine_directory_is_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let machine_dir = temp.path().join("machine");
        fs::create_dir(&machine_dir).unwrap();
        let config = MachineConfig::new(
            "original".into(),
            "fixture".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        );
        config.save(&machine_dir).unwrap();
        let registry_path = temp.path().join("config/buzzardos/machines.json");
        let mut registry = MachineRegistry::open(registry_path.clone()).unwrap();
        registry.register(&machine_dir).unwrap();

        fs::remove_dir_all(&machine_dir).unwrap();
        let registry = MachineRegistry::open(registry_path).unwrap();

        assert_eq!(
            registered_machine_name_for_identity(&registry, config.id),
            Some("original")
        );
    }

    #[test]
    fn imported_machine_config_disables_host_bound_runtime_settings() {
        let temp = tempfile::tempdir().unwrap();
        let shared = temp.path().join("host-share");
        fs::write(&shared, b"host-only").unwrap();
        let mut config = MachineConfig::new(
            "portable".into(),
            "fixture".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["GPU-f832efd8-97ec-6d10-046f-f7a8e84b1c3b".into()],
        );
        config.integrations.ports = vec![wb_core::PortForward::new(
            wb_core::PortDirection::HostToGuest,
        )];
        config.integrations.media.guest_audio_output = true;
        config.integrations.media.host_microphone = true;
        config.integrations.media.host_camera = true;
        config.integrations.media.audio_target = Some("host-output".into());
        config.integrations.media.microphone_target = Some("host-input".into());
        config.integrations.media.camera_target = Some("host-camera".into());
        config.shares = vec![SharedPath::from_host_path(shared).unwrap()];
        config.retained_oci_archive = Some(RetainedOciArchive {
            relative_path: "cache/source.oci.tar".into(),
            sha256: "a".repeat(64),
            size: 1,
        });

        sanitize_imported_machine_config(&mut config);

        assert_eq!(config.gpus, ["all"]);
        assert!(matches!(config.network, NetworkMode::User));
        assert_eq!(config.integrations.ports.len(), 1);
        assert!(!config.integrations.ports[0].enabled);
        assert!(!config.integrations.media.guest_audio_output);
        assert!(!config.integrations.media.host_microphone);
        assert!(!config.integrations.media.host_camera);
        assert!(config.integrations.media.audio_target.is_none());
        assert!(config.integrations.media.microphone_target.is_none());
        assert!(config.integrations.media.camera_target.is_none());
        assert!(config.shares.is_empty());
        assert!(config.retained_oci_archive.is_none());
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
    fn export_staging_cleanup_accepts_an_inherited_cache_descriptor() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let staging = cache.join("oci-export-Ab19zQ");
        fs::create_dir_all(&staging).unwrap();
        let cache_descriptor = File::open(&cache).unwrap();
        let inherited_cache =
            PathBuf::from(format!("/proc/self/fd/{}/.", cache_descriptor.as_raw_fd()));
        let inherited_staging = inherited_cache.join("oci-export-Ab19zQ");

        remove_export_staging_tree(&inherited_staging, &inherited_cache).unwrap();

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
    fn export_mount_scan_resolves_an_inherited_directory_without_retraversal() {
        let temp = tempfile::tempdir().unwrap();
        let descriptor = File::open(temp.path()).unwrap();
        let inherited = PathBuf::from(format!("/proc/self/fd/{}/.", descriptor.as_raw_fd()));
        assert_eq!(
            inherited_descriptor_target(&inherited).unwrap(),
            Some(temp.path().canonicalize().unwrap())
        );
        assert_eq!(
            inherited_descriptor_target(Path::new("/ordinary/rootfs")).unwrap(),
            None
        );
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
        fs::create_dir_all(rootfs.join("etc/ssh")).unwrap();
        fs::create_dir_all(rootfs.join("var/lib/systemd")).unwrap();
        fs::write(rootfs.join("etc/machine-id"), b"source-machine-id\n").unwrap();
        fs::write(rootfs.join("etc/ssh/ssh_host_ed25519_key"), b"source-key").unwrap();
        fs::write(rootfs.join("var/lib/systemd/random-seed"), b"source-seed").unwrap();
        let source_machine_id = fs::read(rootfs.join("etc/machine-id")).unwrap();
        let source_ssh_key = fs::read(rootfs.join("etc/ssh/ssh_host_ed25519_key")).unwrap();
        let source_random_seed = fs::read(rootfs.join("var/lib/systemd/random-seed")).unwrap();
        fs::create_dir(&work).unwrap();
        fs::write(rootfs.join("opt/data/original"), b"portable state").unwrap();
        let numeric_owner = rootfs.join("opt/data/numeric-owner");
        fs::write(&numeric_owner, b"guest-owned").unwrap();
        if Uid::effective().is_root() {
            chown(
                &numeric_owner,
                Some(Uid::from_raw(2345)),
                Some(Gid::from_raw(3456)),
            )
            .unwrap();
        }
        let sparse_path = rootfs.join("opt/data/sparse.img");
        let mut sparse = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&sparse_path)
            .unwrap();
        sparse.set_len(8 * 1024 * 1024).unwrap();
        sparse.write_all(b"sparse-head").unwrap();
        sparse.seek(SeekFrom::End(-11)).unwrap();
        sparse.write_all(b"sparse-tail").unwrap();
        drop(sparse);
        fs::hard_link(
            rootfs.join("opt/data/original"),
            rootfs.join("opt/data/hardlink"),
        )
        .unwrap();
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

        // The normal machine export above owns the sparse-file regression.
        // Keep the generic-seed determinism half of this combined test focused
        // on identity normalization rather than filesystem allocation maps.
        fs::remove_file(&sparse_path).unwrap();

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
        if Uid::effective().is_root() {
            let restored_owner =
                fs::symlink_metadata(restored.join("opt/data/numeric-owner")).unwrap();
            assert_eq!(restored_owner.uid(), 2345);
            assert_eq!(restored_owner.gid(), 3456);
        }
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
        let restored_sparse_path = restored.join("opt/data/sparse.img");
        let restored_sparse = fs::metadata(&restored_sparse_path).unwrap();
        assert_eq!(restored_sparse.len(), 8 * 1024 * 1024);
        assert!(restored_sparse.blocks() * 512 < restored_sparse.len());
        let mut restored_sparse = File::open(&restored_sparse_path).unwrap();
        let mut sparse_head = [0_u8; 11];
        restored_sparse.read_exact(&mut sparse_head).unwrap();
        assert_eq!(&sparse_head, b"sparse-head");
        restored_sparse.seek(SeekFrom::End(-11)).unwrap();
        let mut sparse_tail = [0_u8; 11];
        restored_sparse.read_exact(&mut sparse_tail).unwrap();
        assert_eq!(&sparse_tail, b"sparse-tail");
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
        assert_eq!(fs::read(restored.join("etc/machine-id")).unwrap(), b"");
        assert!(!restored.join("etc/ssh/ssh_host_ed25519_key").exists());
        assert!(!restored.join("var/lib/systemd/random-seed").exists());
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
