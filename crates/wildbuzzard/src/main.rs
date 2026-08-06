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
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use wb_core::{
    IdMap, MachineConfig, MachineState, NetworkMode, ResourceLocator, RuntimeState,
    WaylandCapabilities, WbPaths, host_control_socket,
};

const DEFAULT_IMAGE: &str = "ghcr.io/openresearchtools/buzzardos-desktop:latest";
const GUEST_ASSETS_REVISION: &str = concat!(env!("CARGO_PKG_VERSION"), "+assets.27\n");
const GUEST_ASSETS_MANIFEST: &str = "usr/lib/wildbuzzard/guest-assets.manifest.json";
const LEGACY_REFERENCE_CUA_SHA256: &str =
    "1f7abdd51e6239d3069caec92d73fca4a71c037321518c73036700012b30f029";
const REFERENCE_CHROMIUM_MASTER_PREFERENCES_SHA256: &str =
    "9cbc1b64e3b027cb424ccea3c99ef0da3c82371ca4c5b3af9b18df21ad85dfdb";
const GUEST_ASSETS: &[(&str, &[u8], u32)] = &[
    (
        "usr/libexec/wildbuzzard-init",
        include_bytes!("../../../assets/guest/wildbuzzard-init"),
        0o755,
    ),
    (
        "usr/lib/systemd/system-generators/wildbuzzard-generator",
        include_bytes!("../../../assets/guest/wildbuzzard-generator"),
        0o755,
    ),
    (
        "usr/lib/systemd/system/wildbuzzard-desktop.service",
        include_bytes!("../../../assets/guest/wildbuzzard-desktop.service"),
        0o644,
    ),
    (
        "usr/libexec/wildbuzzard-session",
        include_bytes!("../../../assets/guest/wildbuzzard-session"),
        0o755,
    ),
    (
        "usr/libexec/wildbuzzard-sway-session",
        include_bytes!("../../../assets/guest/wildbuzzard-sway-session"),
        0o755,
    ),
    (
        "usr/libexec/wildbuzzard-output-sync",
        include_bytes!("../../../assets/guest/wildbuzzard-output-sync"),
        0o755,
    ),
    (
        "usr/libexec/wildbuzzard-desktop-stopped",
        include_bytes!("../../../assets/guest/wildbuzzard-desktop-stopped"),
        0o755,
    ),
    (
        "usr/libexec/wildbuzzard-desktop-services",
        include_bytes!("../../../assets/guest/wildbuzzard-desktop-services"),
        0o755,
    ),
    (
        "etc/wildbuzzard/sway-config",
        include_bytes!("../../../assets/guest/sway-config"),
        0o644,
    ),
    (
        "etc/fonts/conf.d/10-wildbuzzard-rendering.conf",
        include_bytes!("../../../assets/guest/fontconfig-rendering.conf"),
        0o644,
    ),
    (
        "etc/sudoers.d/90-wildbuzzard",
        include_bytes!("../../../assets/guest/90-wildbuzzard-sudoers"),
        0o440,
    ),
    (
        "usr/local/bin/sudo",
        include_bytes!("../../../assets/guest/wildbuzzard-sudo"),
        0o755,
    ),
    (
        "usr/libexec/wildbuzzard-sudo-exec",
        include_bytes!("../../../assets/guest/wildbuzzard-sudo-exec"),
        0o755,
    ),
    (
        "etc/polkit-1/rules.d/49-wildbuzzard-root.rules",
        include_bytes!("../../../assets/guest/49-wildbuzzard-root.rules"),
        0o644,
    ),
    (
        "etc/chromium.d/wildbuzzard",
        include_bytes!("../../../assets/guest/chromium-flags"),
        0o644,
    ),
    (
        "etc/chromium/master_preferences",
        include_bytes!("../../../assets/guest/chromium-master-preferences"),
        0o644,
    ),
    (
        "etc/xdg/kwalletrc",
        include_bytes!("../../../assets/guest/kwalletrc"),
        0o644,
    ),
    (
        "usr/local/share/applications/foot-server.desktop",
        include_bytes!("../../../assets/guest/applications/foot-server.desktop"),
        0o644,
    ),
    (
        "usr/local/share/applications/footclient.desktop",
        include_bytes!("../../../assets/guest/applications/footclient.desktop"),
        0o644,
    ),
    (
        "usr/local/share/applications/thunar-bulk-rename.desktop",
        include_bytes!("../../../assets/guest/applications/thunar-bulk-rename.desktop"),
        0o644,
    ),
    (
        "usr/local/share/applications/thunar-settings.desktop",
        include_bytes!("../../../assets/guest/applications/thunar-settings.desktop"),
        0o644,
    ),
    (
        "usr/share/doc/wildbuzzard-cua/LICENSE.trycua-cua.md",
        include_bytes!("../../../third_party/trycua-cua/LICENSE.md"),
        0o644,
    ),
    (
        "usr/share/doc/wildbuzzard-cua/UPSTREAM.toml",
        include_bytes!("../../../third_party/trycua-cua/UPSTREAM.toml"),
        0o644,
    ),
    (
        "usr/share/doc/wildbuzzard-cua/CHANGES.WILDBUZZARD.md",
        include_bytes!("../../../third_party/trycua-cua/CHANGES.WILDBUZZARD.md"),
        0o644,
    ),
];

#[derive(Debug, Clone, Deserialize, Serialize)]
struct GuestAssetRecord {
    sha256: String,
    mode: u32,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct GuestAssetManifest {
    schema: u32,
    assets: BTreeMap<String, GuestAssetRecord>,
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
    /// Pull an OCI image once and create a persistent mutable machine.
    Create {
        name: String,
        #[arg(long, default_value = DEFAULT_IMAGE)]
        image: String,
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
    ToggleMaximize,
    Close,
}

impl WindowAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Maximize => "maximize",
            Self::Restore => "restore",
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
    let paths = WbPaths::discover(cli.storage_dir.as_deref())?;
    paths.ensure()?;

    match cli.command {
        Some(Commands::Create {
            name,
            image,
            network,
            gpus,
        }) => create(&paths, &name, &image, network.into(), gpus),
        Some(Commands::Start { name, detach }) => start(&paths, &name, detach),
        Some(Commands::Stop { name }) => stop(&paths, &name),
        Some(Commands::Window { name, action }) => window(&paths, &name, action),
        Some(Commands::Status { name }) => status(&paths, &name),
        Some(Commands::List) => list(&paths),
        Some(Commands::Doctor) => doctor(),
        Some(Commands::ApplyImage { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::InstallGuestAssets { .. }) => {
            unreachable!("handled before portable path discovery")
        }
        Some(Commands::VerifyGuestAssets { .. }) => {
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

    let name = match machines.as_slice() {
        [] => {
            let name = "default";
            println!("Creating persistent desktop machine '{name}' for first launch...");
            create(
                paths,
                name,
                DEFAULT_IMAGE,
                NetworkMode::User,
                vec!["all".into()],
            )?;
            name.to_owned()
        }
        [name] => name.clone(),
        _ if machines.iter().any(|name| name == "default") => "default".into(),
        _ => bail!(
            "multiple portable machines exist ({}); run `wildbuzzard start NAME` or name one `default`",
            machines.join(", ")
        ),
    };

    start(paths, &name, false)
}

fn create(
    paths: &WbPaths,
    name: &str,
    image: &str,
    network: NetworkMode,
    gpus: Vec<String>,
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
    // Release AppImages always resolve the bundled copy. PATH fallback only makes
    // source-tree development convenient and is never required of end users.
    let crane = resources.helper_or_path("crane")?;
    let stage = tempfile::Builder::new()
        .prefix(&format!(".{name}-creating-"))
        .tempdir_in(paths.machines())
        .context("creating machine staging directory")?;
    let machine_dir = stage.path();
    let rootfs = machine_dir.join("rootfs");
    fs::create_dir(&rootfs).context("creating persistent rootfs")?;

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

    let config = MachineConfig::new(
        name.to_owned(),
        image.to_owned(),
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
        "Created '{name}' from {image}\nPersistent rootfs: {}\nShared data: {}",
        final_dir.join("rootfs").display(),
        paths.shared().display()
    );
    Ok(())
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
    for (relative, contents, mode) in GUEST_ASSETS {
        install_guest_asset(rootfs, Path::new(relative), contents, *mode)?;
    }
    remove_kde_wallet_activation(rootfs)?;
    let shell = bundled_guest_shell_contents()?;
    let cua_driver = bundled_guest_cua_driver_contents()?;
    install_guest_asset(
        rootfs,
        Path::new("usr/libexec/wildbuzzard-shell"),
        &shell,
        0o755,
    )?;
    install_guest_asset(
        rootfs,
        Path::new("usr/local/bin/cua-driver"),
        &cua_driver,
        0o755,
    )?;
    install_guest_asset_manifest(rootfs, &current_guest_asset_manifest(&shell, &cua_driver)?)?;
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

fn migrate_guest_assets(rootfs: &Path) -> Result<()> {
    validate_guest_rootfs(rootfs)?;
    remove_empty_nvidia_mount_placeholders(rootfs)?;
    let previous = read_guest_asset_manifest(rootfs);
    let reference_chromium_preferences = GuestAssetRecord {
        sha256: REFERENCE_CHROMIUM_MASTER_PREFERENCES_SHA256.into(),
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
            (*relative == "etc/chromium/master_preferences")
                .then_some(&reference_chromium_preferences),
        )?;
    }
    for relative in [
        "usr/libexec/wildbuzzard-wayfire-session",
        "etc/xdg/wayfire.ini",
    ] {
        remove_retired_guest_asset(
            rootfs,
            Path::new(relative),
            previous
                .as_ref()
                .and_then(|manifest| manifest.assets.get(relative)),
        )?;
    }
    let shell = bundled_guest_shell_contents()?;
    let cua_driver = bundled_guest_cua_driver_contents()?;
    migrate_guest_asset(
        rootfs,
        Path::new("usr/libexec/wildbuzzard-shell"),
        &shell,
        0o755,
        previous
            .as_ref()
            .and_then(|manifest| manifest.assets.get("usr/libexec/wildbuzzard-shell")),
        None,
    )?;
    let legacy_cua_record = GuestAssetRecord {
        sha256: LEGACY_REFERENCE_CUA_SHA256.into(),
        mode: 0o755,
    };
    migrate_guest_asset(
        rootfs,
        Path::new("usr/local/bin/cua-driver"),
        &cua_driver,
        0o755,
        previous
            .as_ref()
            .and_then(|manifest| manifest.assets.get("usr/local/bin/cua-driver"))
            .or(Some(&legacy_cua_record)),
        Some(&legacy_cua_record),
    )?;
    install_guest_asset_manifest(rootfs, &current_guest_asset_manifest(&shell, &cua_driver)?)?;
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

fn bundled_guest_shell_contents() -> Result<Vec<u8>> {
    bundled_guest_executable_contents("wildbuzzard-shell")
}

fn bundled_guest_cua_driver_contents() -> Result<Vec<u8>> {
    bundled_guest_executable_contents("wildbuzzard-cua-driver")
}

fn bundled_guest_executable_contents(name: &str) -> Result<Vec<u8>> {
    let sibling = bundled_guest_executable(name)?;
    let metadata = fs::symlink_metadata(&sibling)
        .with_context(|| format!("finding bundled guest executable {}", sibling.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "bundled guest executable {} must be a regular file",
            sibling.display()
        );
    }
    fs::read(&sibling).with_context(|| format!("reading guest executable {}", sibling.display()))
}

fn guest_asset_record(contents: &[u8], mode: u32) -> GuestAssetRecord {
    GuestAssetRecord {
        sha256: format!("{:x}", Sha256::digest(contents)),
        mode,
    }
}

fn current_guest_asset_manifest(shell: &[u8], cua_driver: &[u8]) -> Result<GuestAssetManifest> {
    let mut assets = BTreeMap::new();
    for (relative, contents, mode) in GUEST_ASSETS {
        assets.insert((*relative).to_owned(), guest_asset_record(contents, *mode));
    }
    assets.insert(
        "usr/libexec/wildbuzzard-shell".to_owned(),
        guest_asset_record(shell, 0o755),
    );
    assets.insert(
        "usr/local/bin/cua-driver".to_owned(),
        guest_asset_record(cua_driver, 0o755),
    );
    Ok(GuestAssetManifest { schema: 1, assets })
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

fn bundled_guest_executable(name: &str) -> Result<PathBuf> {
    let launcher = std::env::current_exe().context("locating bundled guest executable")?;
    Ok(launcher
        .parent()
        .context("launcher path has no parent")?
        .join(name))
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

    if let Some(state) = RuntimeState::load(&machine_dir)?
        && state.state == MachineState::Running
        && runtime_is_live(&state, &machine_dir)
    {
        window(paths, name, WindowAction::Restore)?;
        println!("Machine '{name}' is already running; restored its host window");
        return Ok(());
    }

    refresh_guest_assets(&machine_dir.join("rootfs"))?;

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
    if detach {
        let broker_pid = child.id();
        wait_for_detached_start(
            &machine_dir,
            &mut child,
            broker_pid,
            Duration::from_secs(95),
        )?;
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

fn refresh_guest_assets(rootfs: &Path) -> Result<()> {
    if guest_assets_are_current_rootless(rootfs)? {
        return Ok(());
    }

    let status = run_guest_asset_helper(rootfs, "__install-guest-assets")?;
    if !status.success() {
        bail!("rootless guest asset migration exited with {status}");
    }
    if !guest_assets_are_current_rootless(rootfs)? {
        bail!("rootless guest asset migration did not commit its revision");
    }
    Ok(())
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

fn guest_assets_are_current(rootfs: &Path) -> Result<bool> {
    let installed = fs::read_to_string(rootfs.join("usr/lib/wildbuzzard/guest-assets.version"))
        .unwrap_or_default();
    Ok(installed == GUEST_ASSETS_REVISION)
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
                bail!(
                    "machine failed to start: {}",
                    state.detail.as_deref().unwrap_or("no diagnostic")
                );
            }
            bail!("machine broker exited with {status} before desktop readiness");
        }
        if Instant::now() >= deadline {
            bail!(
                "machine did not report desktop readiness within {} seconds",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn stop(paths: &WbPaths, name: &str) -> Result<()> {
    let machine_dir = require_machine(paths, name)?;
    let mut state = RuntimeState::load(&machine_dir)?.context("machine has no runtime state")?;
    if state.state == MachineState::Starting {
        let Some(broker_pid) = state.launcher_pid else {
            return repair_stale_stop(&machine_dir, &mut state, name);
        };
        if !broker_matches_machine(broker_pid, &machine_dir) {
            return repair_stale_stop(&machine_dir, &mut state, name);
        }
        state.state = MachineState::Stopping;
        state.detail = Some("cancelling machine startup".into());
        state.save(&machine_dir)?;
        signal_process(broker_pid, libc::SIGTERM)?;
        if !wait_for_process_exit(broker_pid, Duration::from_secs(5)) {
            signal_process(broker_pid, libc::SIGKILL)?;
            if !wait_for_process_exit(broker_pid, Duration::from_secs(5)) {
                bail!("machine broker {broker_pid} did not exit after SIGKILL");
            }
        }
        state.state = MachineState::Stopped;
        state.launcher_pid = None;
        state.container_pid = None;
        state.detail = Some("startup cancelled".into());
        state.save(&machine_dir)?;
        println!("Cancelled startup of '{name}'");
        return Ok(());
    }
    if !runtime_is_live(&state, &machine_dir) {
        return repair_stale_stop(&machine_dir, &mut state, name);
    }
    let broker_pid = state
        .launcher_pid
        .context("live machine state has no supervising broker process id")?;
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
        wait_for_broker_shutdown(&machine_dir, broker_pid)?;
        println!("Stopped '{name}' cleanly");
        return Ok(());
    }

    eprintln!("Orderly shutdown timed out; sending SIGTERM to '{name}'");
    signal_process(pid, libc::SIGTERM)?;
    if wait_for_process_exit(pid, Duration::from_secs(5)) {
        wait_for_broker_shutdown(&machine_dir, broker_pid)?;
        println!("Stopped '{name}' after SIGTERM");
        return Ok(());
    }

    eprintln!("SIGTERM timed out; sending SIGKILL to '{name}'");
    signal_process(pid, libc::SIGKILL)?;
    if !wait_for_process_exit(pid, Duration::from_secs(5)) {
        bail!("machine process {pid} did not exit after SIGKILL");
    }
    wait_for_broker_shutdown(&machine_dir, broker_pid)?;
    println!("Stopped '{name}' after forced termination");
    Ok(())
}

fn window(paths: &WbPaths, name: &str, action: WindowAction) -> Result<()> {
    use std::os::unix::net::UnixStream;

    let machine_dir = require_machine(paths, name)?;
    let state = RuntimeState::load(&machine_dir)?.context("machine has no runtime state")?;
    if !runtime_is_live(&state, &machine_dir) {
        bail!("machine '{name}' is not running");
    }
    let socket = host_control_socket(&machine_dir)?;
    let mut connection = UnixStream::connect(&socket)
        .with_context(|| format!("connecting to host window control {}", socket.display()))?;
    connection
        .write_all(format!("{}\n", action.as_str()).as_bytes())
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

fn wait_for_broker_shutdown(machine_dir: &Path, broker_pid: u32) -> Result<()> {
    if wait_for_process_exit(broker_pid, Duration::from_secs(10)) {
        return Ok(());
    }
    if !broker_matches_machine(broker_pid, machine_dir) {
        return Ok(());
    }

    eprintln!(
        "Machine systemd exited but broker cleanup timed out; sending SIGTERM to broker {broker_pid}"
    );
    signal_process(broker_pid, libc::SIGTERM)?;
    if wait_for_process_exit(broker_pid, Duration::from_secs(5)) {
        return Ok(());
    }
    if !broker_matches_machine(broker_pid, machine_dir) {
        return Ok(());
    }

    eprintln!("Broker cleanup ignored SIGTERM; sending SIGKILL to broker {broker_pid}");
    signal_process(broker_pid, libc::SIGKILL)?;
    if !wait_for_process_exit(broker_pid, Duration::from_secs(5)) {
        bail!("machine broker {broker_pid} did not exit after SIGKILL");
    }
    Ok(())
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
        assert!(sway_config.contains("for_window [shell=\"xdg_shell\"] floating enable"));
        assert!(sway_config.contains("for_window [shell=\"xwayland\"] floating enable"));
        assert!(sway_config.contains("workspace 1"));
        assert!(sway_config.contains("wildbuzzard-desktop-services"));
        assert!(!sway_config.contains("waybar"));
        assert!(!sway_config.contains("fuzzel"));
        assert_eq!(
            fs::read_to_string(rootfs.join("usr/lib/wildbuzzard/guest-assets.version")).unwrap(),
            GUEST_ASSETS_REVISION
        );
        assert!(
            fs::read_to_string(rootfs.join("usr/libexec/wildbuzzard-desktop-services"))
                .unwrap()
                .contains("wildbuzzard-output-sync")
        );
        assert!(
            fs::read_to_string(rootfs.join("usr/libexec/wildbuzzard-desktop-services"))
                .unwrap()
                .contains("/usr/libexec/wildbuzzard-shell")
        );
        assert!(
            !rootfs
                .join("usr/local/bin/wildbuzzard-window-control")
                .exists()
        );
        assert!(
            fs::read_to_string(rootfs.join("usr/libexec/wildbuzzard-session"))
                .unwrap()
                .contains("XDG_CURRENT_DESKTOP=sway")
        );
        assert!(
            fs::read_to_string(rootfs.join("usr/lib/systemd/system/wildbuzzard-desktop.service"))
                .unwrap()
                .contains("EnvironmentFile=/run/wildbuzzard-host/driver.env")
        );
        assert!(
            fs::read_to_string(rootfs.join("etc/chromium.d/wildbuzzard"))
                .unwrap()
                .contains("--password-store=basic")
        );
        assert!(
            fs::read_to_string(rootfs.join("etc/chromium.d/wildbuzzard"))
                .unwrap()
                .contains("--force-renderer-accessibility=complete")
        );
        let chromium_preferences: serde_json::Value = serde_json::from_slice(
            &fs::read(rootfs.join("etc/chromium/master_preferences")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            chromium_preferences["browser"]["custom_chrome_frame"],
            false
        );
        assert!(
            fs::read_to_string(rootfs.join("etc/xdg/kwalletrc"))
                .unwrap()
                .contains("Enabled=false")
        );
        assert!(
            !rootfs
                .join("usr/share/dbus-1/services/org.kde.kwalletd6.service")
                .exists()
        );
        assert_eq!(
            fs::metadata(rootfs.join("usr/libexec/wildbuzzard-init"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
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
    fn committed_guest_asset_revision_does_not_overwrite_guest_edits() {
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

        assert!(guest_assets_are_current(&rootfs).unwrap());
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
    fn migration_replaces_a_recognized_reference_image_file() {
        let temp = tempfile::tempdir().unwrap();
        let rootfs = temp.path().join("rootfs");
        fs::create_dir(&rootfs).unwrap();
        let relative = Path::new("etc/chromium/master_preferences");
        install_guest_asset(&rootfs, relative, b"reference package value", 0o644).unwrap();
        let stale_manifest_record = guest_asset_record(b"new distributed value", 0o644);
        let recognized_reference = guest_asset_record(b"reference package value", 0o644);

        migrate_guest_asset(
            &rootfs,
            relative,
            b"new distributed value",
            0o644,
            Some(&stale_manifest_record),
            Some(&recognized_reference),
        )
        .unwrap();

        assert_eq!(
            fs::read(rootfs.join(relative)).unwrap(),
            b"new distributed value"
        );
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
}
