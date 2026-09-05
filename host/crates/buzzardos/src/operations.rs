// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::cli::{
    Cli, Command as CliCommand, CreateArguments, ImportMode, NewMachineArguments, PullArguments,
};
use crate::display;
use anyhow::{Context, Result, bail};
use chrono::Utc;
use fs2::FileExt;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;
use wb_core::{
    IntegrationSettings, MachineConfig, MachineRegistry, MachineState, OciImageMetadata, Podman,
    PodmanContainerState, PodmanDefinition, PodmanImageInspection, PodmanInspection,
    PodmanRuntimePaths, ResourceLocator, RuntimeState, SharedPath, WbPaths,
};

const CONFIG_LABEL: &str = "org.openresearchtools.buzzardos.machine-config.v1";

pub(crate) fn run(cli: Cli) -> Result<()> {
    let resources = ResourceLocator::discover()?;
    let podman = Podman::discover(&resources)?;
    let mut registry = MachineRegistry::discover()?;

    match cli.command {
        Some(CliCommand::Create(arguments)) => {
            create_from_pull(&podman, &mut registry, cli.machine_dir, arguments)
        }
        Some(CliCommand::Pull(arguments)) => {
            create_from_pull_positional(&podman, &mut registry, cli.machine_dir, arguments)
        }
        Some(CliCommand::Build {
            name,
            context,
            file,
            machine,
        }) => build_machine(
            &podman,
            &mut registry,
            cli.machine_dir,
            &name,
            &context,
            file.as_deref(),
            &machine,
        ),
        Some(CliCommand::Start { name, detach }) => {
            let machine_dir = resolve_machine(&registry, cli.machine_dir.as_deref(), &name)?;
            start_machine(&resources, &podman, &machine_dir, detach)
        }
        Some(CliCommand::Stop { name }) => {
            let machine_dir = resolve_machine(&registry, cli.machine_dir.as_deref(), &name)?;
            stop_machine(&podman, &machine_dir, true)
        }
        Some(CliCommand::Restart { name }) => {
            let machine_dir = resolve_machine(&registry, cli.machine_dir.as_deref(), &name)?;
            restart_machine(&resources, &podman, &machine_dir)
        }
        Some(CliCommand::Import {
            source,
            name,
            mode,
            machine,
        }) => import_machine(
            &podman,
            &mut registry,
            cli.machine_dir,
            &source,
            &name,
            mode,
            &machine,
        ),
        Some(CliCommand::Export { name, output }) => {
            let machine_dir = resolve_machine(&registry, cli.machine_dir.as_deref(), &name)?;
            export_machine(&podman, &machine_dir, &output)
        }
        Some(CliCommand::Clone {
            source,
            name,
            machine,
        }) => clone_machine(
            &podman,
            &mut registry,
            cli.machine_dir,
            &source,
            &name,
            &machine,
        ),
        Some(CliCommand::Delete { name, yes }) => {
            let machine_dir = resolve_machine(&registry, cli.machine_dir.as_deref(), &name)?;
            delete_machine(&podman, &mut registry, &machine_dir, yes)
        }
        Some(CliCommand::Window { name, action }) => {
            let machine_dir = resolve_machine(&registry, cli.machine_dir.as_deref(), &name)?;
            display::send_control(&machine_dir, action.as_str())
        }
        Some(CliCommand::Status { name }) => {
            let machine_dir = resolve_machine(&registry, cli.machine_dir.as_deref(), &name)?;
            print_status(&podman, &machine_dir)
        }
        Some(CliCommand::List) => list_machines(&podman, &registry),
        Some(CliCommand::Register) => {
            let machine_dir = cli
                .machine_dir
                .as_deref()
                .context("register requires --machine-dir /path/to/machine")?;
            registry.register(machine_dir)
        }
        Some(CliCommand::Unregister { name }) => registry.unregister(&name),
        Some(CliCommand::Doctor) => doctor(&resources, &podman),
        None => open_manager(&resources),
    }
}

fn create_from_pull(
    podman: &Podman,
    registry: &mut MachineRegistry,
    machine_dir: Option<PathBuf>,
    arguments: CreateArguments,
) -> Result<()> {
    let image = podman.pull(&arguments.image)?;
    let inspection = podman.inspect_image(&image)?;
    let config = new_config(
        &arguments.name,
        &arguments.image,
        &inspection,
        &arguments.machine,
    )?;
    materialize_machine(podman, registry, machine_dir, config, &image, false)
}

fn create_from_pull_positional(
    podman: &Podman,
    registry: &mut MachineRegistry,
    machine_dir: Option<PathBuf>,
    arguments: PullArguments,
) -> Result<()> {
    let image = podman.pull(&arguments.image)?;
    let inspection = podman.inspect_image(&image)?;
    let config = new_config(
        &arguments.name,
        &arguments.image,
        &inspection,
        &arguments.machine,
    )?;
    materialize_machine(podman, registry, machine_dir, config, &image, false)
}

fn build_machine(
    podman: &Podman,
    registry: &mut MachineRegistry,
    machine_dir: Option<PathBuf>,
    name: &str,
    context: &Path,
    file: Option<&Path>,
    arguments: &NewMachineArguments,
) -> Result<()> {
    MachineConfig::validate_name(name)?;
    let context = context
        .canonicalize()
        .with_context(|| format!("resolving build context {}", context.display()))?;
    if !context.is_dir() {
        bail!("build context is not a directory: {}", context.display());
    }
    let containerfile = file
        .map(PathBuf::from)
        .unwrap_or_else(|| context.join("Containerfile"));
    let containerfile = if containerfile.is_absolute() {
        containerfile
    } else {
        context.join(containerfile)
    };
    let containerfile = containerfile
        .canonicalize()
        .with_context(|| format!("resolving Containerfile {}", containerfile.display()))?;
    if !containerfile.is_file() {
        bail!(
            "Containerfile is not a regular file: {}",
            containerfile.display()
        );
    }

    let tag = format!(
        "localhost/buzzardos-build-{}:latest",
        Uuid::new_v4().simple()
    );
    podman.build(&tag, &containerfile, &context)?;
    let result = (|| {
        let inspection = podman.inspect_image(&tag)?;
        let config = new_config(
            name,
            &format!("containerfile:{}", containerfile.display()),
            &inspection,
            arguments,
        )?;
        materialize_machine(podman, registry, machine_dir, config, &tag, false)
    })();
    let cleanup = podman.remove_image(&tag);
    combine_result(result, cleanup, "removing the temporary build image")
}

fn import_machine(
    podman: &Podman,
    registry: &mut MachineRegistry,
    machine_dir: Option<PathBuf>,
    source: &str,
    name: &str,
    mode: ImportMode,
    arguments: &NewMachineArguments,
) -> Result<()> {
    MachineConfig::validate_name(name)?;
    let (image, source_description) = import_image(podman, source)?;
    let inspection = podman.inspect_image(&image)?;
    let imported = inspection
        .labels
        .get(CONFIG_LABEL)
        .map(|value| decode_config_label(value))
        .transpose()?;
    let is_buzzard_export = imported.is_some();

    let mut config = imported.unwrap_or_else(|| {
        MachineConfig::new(
            name.to_owned(),
            source_description.clone(),
            image_digest(&inspection),
            arguments.network.into(),
            arguments.gpus.clone(),
        )
    });
    if mode == ImportMode::Clone || !inspection.labels.contains_key(CONFIG_LABEL) {
        config.id = Uuid::new_v4();
    } else if registry.entries().iter().any(|entry| entry.id == config.id) {
        bail!(
            "a machine with exported identity {} is already registered",
            config.id
        );
    }
    config.schema = 1;
    config.name = name.to_owned();
    config.title = name.to_owned();
    config.image = source_description;
    config.image_digest = Some(image_digest(&inspection));
    config.network = arguments.network.into();
    config.gpus = arguments.gpus.clone();
    config.shares = shared_paths(&arguments.shares)?;
    if let Some(podman_arguments) = &arguments.podman_arguments {
        config.custom_podman_arguments = podman_arguments.clone();
    }
    config.integrations = IntegrationSettings::default();
    config.retained_oci_archive = None;
    if !is_buzzard_export {
        config.oci = metadata_from_inspection(&inspection);
    }
    config.save_to_validation_only()?;

    materialize_machine(
        podman,
        registry,
        machine_dir,
        config,
        &image,
        mode == ImportMode::Clone,
    )
}

fn clone_machine(
    podman: &Podman,
    registry: &mut MachineRegistry,
    machine_dir: Option<PathBuf>,
    source: &str,
    name: &str,
    arguments: &NewMachineArguments,
) -> Result<()> {
    let source_dir = registry.resolve(source)?;
    let source_config = MachineConfig::load(&source_dir)?;
    let _lock = lock_stopped_machine(podman, &source_dir, "clone")?;
    let definition = podman.definition_for_machine(
        &source_config,
        &source_dir,
        &PodmanRuntimePaths::discover(source_config.id)?,
    )?;
    let inspection = podman
        .inspect(&definition.container_name)?
        .context("source Podman container definition is missing")?;
    require_stopped(&inspection, "clone")?;

    let image = format!(
        "localhost/buzzardos-clone-{}:latest",
        Uuid::new_v4().simple()
    );
    snapshot_machine_image(
        podman,
        &source_config,
        &source_dir.join("rootfs"),
        &source_dir,
        &image,
    )?;
    let result = (|| {
        let image_inspection = podman.inspect_image(&image)?;
        let mut config = source_config;
        config.id = Uuid::new_v4();
        config.name = name.to_owned();
        config.title = name.to_owned();
        config.image = format!("clone:{source}");
        config.image_digest = Some(image_digest(&image_inspection));
        config.created_at = Utc::now();
        config.integrations = IntegrationSettings::default();
        config.shares = shared_paths(&arguments.shares)?;
        config.gpus = arguments.gpus.clone();
        if let Some(podman_arguments) = &arguments.podman_arguments {
            config.custom_podman_arguments = podman_arguments.clone();
        }
        materialize_machine(podman, registry, machine_dir, config, &image, true)
    })();
    let cleanup = podman.remove_image(&image);
    combine_result(result, cleanup, "removing the temporary clone image")
}

fn materialize_machine(
    podman: &Podman,
    registry: &mut MachineRegistry,
    selected_dir: Option<PathBuf>,
    config: MachineConfig,
    image: &str,
    reset_identity: bool,
) -> Result<()> {
    let final_dir = creation_target(registry, selected_dir.as_deref(), &config)?;
    let parent = final_dir
        .parent()
        .context("machine directory has no parent")?;
    let stage = parent.join(format!(
        ".{}.creating-{}",
        config.name,
        Uuid::new_v4().simple()
    ));
    fs::create_dir(&stage)
        .with_context(|| format!("creating machine staging directory {}", stage.display()))?;

    let result = (|| -> Result<()> {
        let rootfs = stage.join("rootfs");
        fs::create_dir(&rootfs).context("creating external machine rootfs")?;
        let archive = stage.join("rootfs.tar");
        let source_container = format!("buzzardos-source-{}", Uuid::new_v4().simple());
        podman.create_from_image(&source_container, image)?;
        let export_result = podman.export_rootfs(&source_container, &archive);
        let remove_result = podman.remove_definition(&source_container);
        combine_result(
            export_result,
            remove_result,
            "removing the temporary source container",
        )?;
        podman.materialize_rootfs(image, &archive, &rootfs, &config.custom_podman_arguments)?;
        fs::remove_file(&archive).context("removing temporary rootfs archive")?;
        validate_desktop_rootfs(&rootfs)?;
        if reset_identity {
            reset_clone_identity(podman, &rootfs, &config.custom_podman_arguments)?;
        }

        config.save(&stage)?;
        RuntimeState::new(MachineState::Stopped).save(&stage)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(stage.join("machine.lock"))
            .context("creating machine lock")?;
        File::open(&stage)?.sync_all()?;
        fs::rename(&stage, &final_dir)
            .with_context(|| format!("committing machine directory {}", final_dir.display()))?;

        let runtime = PodmanRuntimePaths::discover(config.id)?;
        runtime.prepare()?;
        let definition = podman.definition_for_machine(&config, &final_dir, &runtime)?;
        let inspection = podman.create(&definition)?;
        save_inspection(&final_dir, &inspection, Some(&definition.digest), None)?;
        registry.register(&final_dir)?;
        println!(
            "Created '{}'\nMachine directory: {}\nPersistent rootfs: {}\nPodman container: {}",
            config.name,
            final_dir.display(),
            final_dir.join("rootfs").display(),
            inspection.name
        );
        Ok(())
    })();

    if result.is_err() {
        let _ = cleanup_tree(podman, &stage);
        if final_dir.exists() {
            if let Ok(runtime) = PodmanRuntimePaths::discover(config.id)
                && let Ok(definition) = podman.definition_for_machine(&config, &final_dir, &runtime)
            {
                let _ = podman.remove_definition(&definition.container_name);
            }
            let _ = cleanup_tree(podman, &final_dir);
        }
    }
    result
}

fn start_machine(
    resources: &ResourceLocator,
    podman: &Podman,
    machine_dir: &Path,
    detach: bool,
) -> Result<()> {
    let _lock = lock_machine(machine_dir)?;
    let config = MachineConfig::load(machine_dir)?;
    let runtime = PodmanRuntimePaths::discover(config.id)?;
    let definition = podman.definition_for_machine(&config, machine_dir, &runtime)?;
    if let Some(inspection) = podman.inspect(&definition.container_name)?
        && inspection.state == PodmanContainerState::Running
    {
        if display::send_control(machine_dir, "focus-monitor").is_ok() {
            save_inspection(machine_dir, &inspection, Some(&definition.digest), None)?;
            println!("'{}' is already running", config.name);
            return Ok(());
        }

        // Podman owns the persistent container independently of the native
        // window. If the window process was lost while the container kept
        // running, rebuild only Buzzard's display bridge and restart only the
        // fixed guest desktop unit against its new private Wayland endpoint.
        // The Podman container and external rootfs remain untouched.
        let mut state = runtime_from_inspection(&inspection);
        state.state = MachineState::Starting;
        state.definition_digest = Some(definition.digest.clone());
        state.detail = Some("reattaching the native machine window".into());
        state.save(machine_dir)?;

        let prepared = match display::prepare_and_launch(resources, machine_dir, &config, &runtime)
        {
            Ok(prepared) => prepared,
            Err(error) => {
                save_running_attachment_failure(machine_dir, &inspection, &definition, &error)?;
                return Err(error);
            }
        };
        let exec_arguments = [
            OsString::from("--user=0"),
            OsString::from(&definition.container_name),
            OsString::from("/bin/systemctl"),
            OsString::from("restart"),
            OsString::from("buzzardos-desktop.service"),
        ];
        if let Err(error) = podman.exec(&exec_arguments) {
            save_running_attachment_failure(machine_dir, &inspection, &definition, &error)?;
            return Err(error);
        }
        display::publish_media_endpoints(podman, &definition, &config, &runtime)?;
        wait_for_desktop(
            podman,
            machine_dir,
            &definition,
            &runtime,
            &prepared.session_token,
        )?;
        println!("Reattached Buzzard OS desktop '{}'", config.name);
        return Ok(());
    }
    runtime.prepare()?;
    ensure_definition(podman, machine_dir, &definition)?;

    let prepared = display::prepare_and_launch(resources, machine_dir, &config, &runtime)?;
    let mut state = RuntimeState::new(MachineState::Starting);
    state.definition_digest = Some(definition.digest.clone());
    state.detail = Some("starting persistent Podman container".into());
    state.save(machine_dir)?;
    if let Err(error) = podman.start(&definition.container_name) {
        state.state = MachineState::Failed;
        state.detail = Some(format!("Podman start failed: {error:#}"));
        state.updated_at = Utc::now();
        state.save(machine_dir)?;
        return Err(error);
    }
    display::publish_media_endpoints(podman, &definition, &config, &runtime)?;
    wait_for_desktop(
        podman,
        machine_dir,
        &definition,
        &runtime,
        &prepared.session_token,
    )?;
    println!("Buzzard OS desktop '{}' is ready", config.name);
    if !detach {
        let exit_code = podman.wait(&definition.container_name)?;
        let inspection = podman.inspect(&definition.container_name)?;
        if let Some(inspection) = inspection {
            save_inspection(
                machine_dir,
                &inspection,
                Some(&definition.digest),
                Some(format!("container exited with status {exit_code}")),
            )?;
        }
        let _ = display::send_control(machine_dir, "close");
    }
    Ok(())
}

fn stop_machine(podman: &Podman, machine_dir: &Path, close_window: bool) -> Result<()> {
    let _lock = lock_machine(machine_dir)?;
    let config = MachineConfig::load(machine_dir)?;
    let runtime = PodmanRuntimePaths::discover(config.id)?;
    let definition = podman.definition_for_machine(&config, machine_dir, &runtime)?;
    let Some(inspection) = podman.inspect(&definition.container_name)? else {
        save_stopped(
            machine_dir,
            None,
            Some("Podman definition is absent".into()),
        )?;
        if close_window {
            let _ = display::send_control(machine_dir, "close");
        }
        return Ok(());
    };
    if matches!(
        inspection.state,
        PodmanContainerState::Running
            | PodmanContainerState::Paused
            | PodmanContainerState::Stopping
    ) {
        let mut state = RuntimeState::new(MachineState::Stopping);
        state.container_id = Some(inspection.id.clone());
        state.definition_digest = inspection.definition_digest.clone();
        state.detail = Some("stopping persistent Podman container".into());
        state.save(machine_dir)?;
        podman.stop(&definition.container_name)?;
    }
    let stopped = podman.inspect(&definition.container_name)?;
    save_stopped(
        machine_dir,
        stopped.as_ref(),
        Some("persistent rootfs preserved".into()),
    )?;
    if close_window {
        let _ = display::send_control(machine_dir, "close");
    }
    println!("Stopped '{}'", config.name);
    Ok(())
}

fn restart_machine(resources: &ResourceLocator, podman: &Podman, machine_dir: &Path) -> Result<()> {
    let _lock = lock_machine(machine_dir)?;
    let config = MachineConfig::load(machine_dir)?;
    let runtime = PodmanRuntimePaths::discover(config.id)?;
    let definition = podman.definition_for_machine(&config, machine_dir, &runtime)?;
    let current = podman.inspect(&definition.container_name)?;
    let definition_changed = current
        .as_ref()
        .and_then(|inspection| inspection.definition_digest.as_deref())
        != Some(definition.digest.as_str());
    // Publish the planned lifecycle transition before Podman stops the old
    // container process. The native window's blocking `podman wait` observer
    // uses this state to distinguish an intentional restart from a complete
    // stop, so it can keep the same host toplevel and accept the replacement
    // Sway connection.
    let mut state = RuntimeState::new(MachineState::Starting);
    state.container_id = current.as_ref().map(|inspection| inspection.id.clone());
    state.definition_digest = Some(definition.digest.clone());
    state.detail = Some(if definition_changed {
        "starting the updated persistent Podman definition".into()
    } else {
        "restarting the persistent Podman container".into()
    });
    state.save(machine_dir)?;
    if definition_changed {
        if current.as_ref().is_some_and(|inspection| {
            matches!(
                inspection.state,
                PodmanContainerState::Running
                    | PodmanContainerState::Paused
                    | PodmanContainerState::Stopping
            )
        }) {
            podman.stop(&definition.container_name)?;
        }
        podman.remove_definition(&definition.container_name)?;
        runtime.prepare()?;
        podman.create(&definition)?;
    }

    let prepared = display::prepare_and_launch(resources, machine_dir, &config, &runtime)?;
    if definition_changed {
        podman.start(&definition.container_name)?;
    } else {
        podman.restart(&definition.container_name)?;
    }
    display::publish_media_endpoints(podman, &definition, &config, &runtime)?;
    wait_for_desktop(
        podman,
        machine_dir,
        &definition,
        &runtime,
        &prepared.session_token,
    )?;
    println!("Restarted '{}'", config.name);
    Ok(())
}

fn save_running_attachment_failure(
    machine_dir: &Path,
    inspection: &PodmanInspection,
    definition: &PodmanDefinition,
    error: &anyhow::Error,
) -> Result<()> {
    let mut state = runtime_from_inspection(inspection);
    state.state = MachineState::Failed;
    state.definition_digest = Some(definition.digest.clone());
    state.detail = Some(format!(
        "the Podman container is running but its native window could not be attached: {error:#}"
    ));
    state.updated_at = Utc::now();
    state.save(machine_dir)
}

fn wait_for_desktop(
    podman: &Podman,
    machine_dir: &Path,
    definition: &PodmanDefinition,
    runtime: &PodmanRuntimePaths,
    session_token: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        let desktop_ready = fs::read_to_string(runtime.host_exchange.join("desktop-ready"))
            .is_ok_and(|value| value.trim() == session_token);
        let first_frame = fs::read(runtime.host_status.join("presentation.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|value| {
                value
                    .get("submitted_frames")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    > 0
                    && value
                        .get("painted_frames")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0)
                        > 0
            });
        if desktop_ready && first_frame {
            let inspection = podman
                .inspect(&definition.container_name)?
                .context("Podman container disappeared during startup")?;
            save_inspection(
                machine_dir,
                &inspection,
                Some(&definition.digest),
                Some("systemd and nested desktop are ready".into()),
            )?;
            return Ok(());
        }
        let inspection = podman
            .inspect(&definition.container_name)?
            .context("Podman container disappeared during startup")?;
        if inspection.state != PodmanContainerState::Running {
            save_inspection(
                machine_dir,
                &inspection,
                Some(&definition.digest),
                Some("container exited before desktop readiness".into()),
            )?;
            bail!("Podman container exited before the desktop became ready");
        }
        if Instant::now() >= deadline {
            let mut state = runtime_from_inspection(&inspection);
            state.state = MachineState::Failed;
            state.definition_digest = Some(definition.digest.clone());
            state.detail = Some("desktop readiness timed out after 90 seconds".into());
            state.save(machine_dir)?;
            bail!("desktop readiness timed out after 90 seconds");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn ensure_definition(
    podman: &Podman,
    machine_dir: &Path,
    definition: &PodmanDefinition,
) -> Result<PodmanInspection> {
    match podman.inspect(&definition.container_name)? {
        Some(inspection)
            if inspection.definition_digest.as_deref() == Some(definition.digest.as_str()) =>
        {
            Ok(inspection)
        }
        Some(inspection) => {
            require_stopped(&inspection, "apply changed machine settings")?;
            podman.remove_definition(&definition.container_name)?;
            let created = podman.create(definition)?;
            save_inspection(
                machine_dir,
                &created,
                Some(&definition.digest),
                Some("updated persistent Podman definition".into()),
            )?;
            Ok(created)
        }
        None => {
            let created = podman.create(definition)?;
            save_inspection(
                machine_dir,
                &created,
                Some(&definition.digest),
                Some("created persistent Podman definition".into()),
            )?;
            Ok(created)
        }
    }
}

fn export_machine(podman: &Podman, machine_dir: &Path, output: &Path) -> Result<()> {
    let config = MachineConfig::load(machine_dir)?;
    let _lock = lock_stopped_machine(podman, machine_dir, "export")?;
    let runtime = PodmanRuntimePaths::discover(config.id)?;
    let definition = podman.definition_for_machine(&config, machine_dir, &runtime)?;
    let inspection = podman
        .inspect(&definition.container_name)?
        .context("Podman container definition is missing")?;
    require_stopped(&inspection, "export")?;

    let output = std::path::absolute(output)
        .with_context(|| format!("resolving export path {}", output.display()))?;
    if output.exists() {
        bail!("refusing to replace existing export {}", output.display());
    }
    let parent = output.parent().context("export path has no parent")?;
    if !parent.is_dir() {
        bail!("export parent is not a directory: {}", parent.display());
    }
    let temporary = tempfile::Builder::new()
        .prefix(".buzzardos-export-")
        .tempfile_in(parent)
        .context("creating atomic export file")?;
    temporary.as_file().set_len(0)?;
    let image = format!(
        "localhost/buzzardos-export-{}:latest",
        Uuid::new_v4().simple()
    );
    snapshot_machine_image(podman, &config, &machine_dir.join("rootfs"), parent, &image)?;
    let save_result = podman.save_oci_archive(&image, temporary.path());
    let remove_result = podman.remove_image(&image);
    combine_result(
        save_result,
        remove_result,
        "removing the temporary export image",
    )?;
    temporary.as_file().sync_all()?;
    let persisted = temporary
        .persist_noclobber(&output)
        .map_err(|error| error.error)
        .with_context(|| format!("committing export {}", output.display()))?;
    persisted.set_permissions(fs::Permissions::from_mode(0o644))?;
    persisted.sync_all()?;
    File::open(parent)?.sync_all()?;
    println!("Exported '{}' to {}", config.name, output.display());
    Ok(())
}

/// Convert one stopped external-rootfs machine into a temporary OCI image
/// using operations supported by stock Podman for `--rootfs` containers.
/// `podman commit` explicitly rejects exploded rootfs containers, so the
/// supported path is export -> import. The archive stays on the caller's
/// selected data filesystem and disappears when this function returns.
fn snapshot_machine_image(
    podman: &Podman,
    config: &MachineConfig,
    rootfs: &Path,
    temporary_parent: &Path,
    image: &str,
) -> Result<()> {
    let rootfs_archive = tempfile::Builder::new()
        .prefix(".buzzardos-rootfs-")
        .suffix(".tar")
        .tempfile_in(temporary_parent)
        .context("creating temporary flat-rootfs archive")?;
    podman.archive_external_rootfs(
        rootfs,
        &config.custom_podman_arguments,
        rootfs_archive.path(),
    )?;
    let changes = oci_import_changes(config)?;
    podman.import_rootfs_archive(rootfs_archive.path(), image, &changes)?;
    Ok(())
}

fn oci_import_changes(config: &MachineConfig) -> Result<Vec<OsString>> {
    let mut changes = Vec::new();
    for environment in &config.oci.environment {
        changes.push(OsString::from(format!("ENV {environment}")));
    }
    for (name, value) in &config.oci.labels {
        if name != CONFIG_LABEL {
            changes.push(OsString::from(format!("LABEL {name}={value}")));
        }
    }
    if let Some(working_dir) = config.oci.working_dir.as_deref() {
        changes.push(OsString::from(format!("WORKDIR {working_dir}")));
    }
    if let Some(user) = config.oci.user.as_deref() {
        changes.push(OsString::from(format!("USER {user}")));
    }
    if !config.oci.entrypoint.is_empty() {
        changes.push(OsString::from(format!(
            "ENTRYPOINT {}",
            serde_json::to_string(&config.oci.entrypoint)
                .context("serializing exported OCI entrypoint")?
        )));
    }
    if !config.oci.command.is_empty() {
        changes.push(OsString::from(format!(
            "CMD {}",
            serde_json::to_string(&config.oci.command)
                .context("serializing exported OCI command")?
        )));
    }
    if let Some(stop_signal) = config.oci.stop_signal.as_deref() {
        changes.push(OsString::from(format!("STOPSIGNAL {stop_signal}")));
    }
    changes.push(OsString::from(format!(
        "LABEL {CONFIG_LABEL}={}",
        encode_config_label(config)?
    )));
    Ok(changes)
}

fn delete_machine(
    podman: &Podman,
    registry: &mut MachineRegistry,
    machine_dir: &Path,
    confirmed: bool,
) -> Result<()> {
    if !confirmed {
        bail!("delete requires --yes after user confirmation");
    }
    let config = MachineConfig::load(machine_dir)?;
    let _lock = lock_stopped_machine(podman, machine_dir, "delete")?;
    let runtime = PodmanRuntimePaths::discover(config.id)?;
    let definition = podman.definition_for_machine(&config, machine_dir, &runtime)?;
    if let Some(inspection) = podman.inspect(&definition.container_name)? {
        require_stopped(&inspection, "delete")?;
        podman.remove_definition(&definition.container_name)?;
    }
    registry.unregister(&config.name)?;
    cleanup_tree(podman, machine_dir)?;
    let _ = fs::remove_dir_all(runtime.root);
    println!("Deleted '{}'", config.name);
    Ok(())
}

fn print_status(podman: &Podman, machine_dir: &Path) -> Result<()> {
    let config = MachineConfig::load(machine_dir)?;
    let runtime = PodmanRuntimePaths::discover(config.id)?;
    let definition = podman.definition_for_machine(&config, machine_dir, &runtime)?;
    let inspection = podman.inspect(&definition.container_name)?;
    if let Some(inspection) = &inspection {
        save_inspection(machine_dir, inspection, Some(&definition.digest), None)?;
    } else {
        save_stopped(
            machine_dir,
            None,
            Some("Podman definition is absent".into()),
        )?;
    }
    println!("name: {}", config.name);
    println!("machine directory: {}", machine_dir.display());
    println!("rootfs: {}", machine_dir.join("rootfs").display());
    println!("container: {}", definition.container_name);
    println!(
        "state: {}",
        inspection
            .as_ref()
            .map(|value| format!("{:?}", value.state).to_ascii_lowercase())
            .unwrap_or_else(|| "not-created".into())
    );
    println!(
        "custom Podman arguments: {}",
        config.custom_podman_arguments
    );
    Ok(())
}

fn list_machines(podman: &Podman, registry: &MachineRegistry) -> Result<()> {
    for entry in registry.entries() {
        let config = MachineConfig::load(&entry.machine_dir)?;
        let runtime = PodmanRuntimePaths::discover(config.id)?;
        let definition = podman.definition_for_machine(&config, &entry.machine_dir, &runtime)?;
        let inspection = podman.inspect(&definition.container_name)?;
        if let Some(inspection) = &inspection {
            save_inspection(
                &entry.machine_dir,
                inspection,
                Some(&definition.digest),
                None,
            )?;
        }
        println!(
            "{}\t{}\t{}",
            entry.name,
            inspection
                .map(|value| format!("{:?}", value.state).to_ascii_lowercase())
                .unwrap_or_else(|| "not-created".into()),
            entry.machine_dir.display()
        );
    }
    Ok(())
}

fn doctor(resources: &ResourceLocator, podman: &Podman) -> Result<()> {
    println!("Podman: {}", podman.version()?);
    let display = resources.helper_or_path("buzzardos-display")?;
    println!("Buzzard display: {}", display.display());
    let host = std::env::var_os("WAYLAND_DISPLAY").context("WAYLAND_DISPLAY is not set")?;
    println!("Host Wayland display: {}", PathBuf::from(host).display());
    println!("Buzzard OS Podman runtime prerequisites are available");
    Ok(())
}

fn open_manager(resources: &ResourceLocator) -> Result<()> {
    let display = resources.helper_or_path("buzzardos-display")?;
    let launcher = std::env::current_exe().context("locating Buzzard OS launcher")?;
    let status = Command::new(&display)
        .arg("--machine-manager")
        .arg("--launcher")
        .arg(launcher)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("starting machine manager with {}", display.display()))?;
    if !status.success() {
        bail!("Buzzard OS machine manager exited with {status}");
    }
    Ok(())
}

fn new_config(
    name: &str,
    source: &str,
    inspection: &PodmanImageInspection,
    arguments: &NewMachineArguments,
) -> Result<MachineConfig> {
    let mut config = MachineConfig::new(
        name.to_owned(),
        source.to_owned(),
        image_digest(inspection),
        arguments.network.into(),
        arguments.gpus.clone(),
    );
    config.shares = shared_paths(&arguments.shares)?;
    if let Some(podman_arguments) = &arguments.podman_arguments {
        config.custom_podman_arguments = podman_arguments.clone();
    }
    config.oci = metadata_from_inspection(inspection);
    config.save_to_validation_only()?;
    Ok(config)
}

fn metadata_from_inspection(inspection: &PodmanImageInspection) -> OciImageMetadata {
    OciImageMetadata {
        environment: inspection.environment.clone(),
        labels: inspection.labels.clone(),
        working_dir: inspection.working_dir.clone(),
        user: inspection.user.clone(),
        entrypoint: inspection.entrypoint.clone(),
        command: inspection.command.clone(),
        stop_signal: inspection.stop_signal.clone(),
    }
}

fn image_digest(inspection: &PodmanImageInspection) -> String {
    inspection.digest.clone().unwrap_or_else(|| {
        if inspection.id.starts_with("sha256:") {
            inspection.id.clone()
        } else {
            format!("sha256:{}", inspection.id)
        }
    })
}

fn import_image(podman: &Podman, source: &str) -> Result<(String, String)> {
    if source.trim().is_empty() {
        bail!("image source cannot be empty");
    }
    let path = Path::new(source);
    if path.is_dir() {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolving OCI layout {}", path.display()))?;
        let transport = format!("oci:{}", canonical.display());
        let image = podman.pull(&transport)?;
        return Ok((image, format!("oci-layout:{}", canonical.display())));
    }
    if path.is_file() {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolving image archive {}", path.display()))?;
        let output = podman.load(&canonical)?;
        let image = loaded_image_reference(&output)?;
        return Ok((image, format!("archive:{}", canonical.display())));
    }
    let image = podman.pull(source)?;
    Ok((image, source.to_owned()))
}

fn loaded_image_reference(output: &str) -> Result<String> {
    for line in output
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        for prefix in ["Loaded image:", "Loaded image ID:"] {
            if let Some(reference) = line.strip_prefix(prefix).map(str::trim)
                && !reference.is_empty()
            {
                return Ok(reference.to_owned());
            }
        }
        if !line.contains(char::is_whitespace) {
            return Ok(line.to_owned());
        }
    }
    bail!("Podman loaded the archive but returned no image reference")
}

fn creation_target(
    registry: &MachineRegistry,
    selected: Option<&Path>,
    config: &MachineConfig,
) -> Result<PathBuf> {
    let selected = selected.context("creation requires --machine-dir /path/to/new-machine")?;
    let paths = WbPaths::for_machine(selected)?;
    paths.ensure()?;
    let final_dir = paths.machine(&config.name);
    if final_dir.exists() {
        bail!("machine directory already exists: {}", final_dir.display());
    }
    for entry in registry.entries() {
        if entry.name == config.name || entry.id == config.id {
            bail!(
                "machine '{}' or identity {} is already registered",
                config.name,
                config.id
            );
        }
        if entry.machine_dir == final_dir
            || entry.machine_dir.starts_with(&final_dir)
            || final_dir.starts_with(&entry.machine_dir)
        {
            bail!(
                "selected directory {} overlaps registered machine '{}'",
                final_dir.display(),
                entry.name
            );
        }
    }
    Ok(final_dir)
}

fn resolve_machine(
    registry: &MachineRegistry,
    override_dir: Option<&Path>,
    name: &str,
) -> Result<PathBuf> {
    MachineConfig::validate_name(name)?;
    let directory = match override_dir {
        Some(directory) => WbPaths::for_machine(directory)?.machine(name),
        None => registry.resolve(name)?,
    };
    let metadata = fs::symlink_metadata(&directory)
        .with_context(|| format!("inspecting machine directory {}", directory.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "machine directory must be a real directory: {}",
            directory.display()
        );
    }
    let config = MachineConfig::load(&directory)?;
    if config.name != name {
        bail!("machine directory contains '{}', not '{name}'", config.name);
    }
    validate_desktop_rootfs(&directory.join("rootfs"))?;
    Ok(directory)
}

fn validate_desktop_rootfs(rootfs: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(rootfs)
        .with_context(|| format!("inspecting external rootfs {}", rootfs.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "external rootfs must be a real directory: {}",
            rootfs.display()
        );
    }
    for required in ["etc", "usr", "sbin/init"] {
        let path = rootfs.join(required);
        if !path.exists() {
            bail!("image is not a systemd desktop rootfs: missing /{required}");
        }
    }
    Ok(())
}

fn shared_paths(paths: &[PathBuf]) -> Result<Vec<SharedPath>> {
    let shares = paths
        .iter()
        .map(|path| {
            let path = std::path::absolute(path)
                .with_context(|| format!("resolving shared path {}", path.display()))?;
            SharedPath::from_host_path(path)
        })
        .collect::<Result<Vec<_>>>()?;
    MachineConfig::validate_shares(&shares)?;
    Ok(shares)
}

fn reset_clone_identity(podman: &Podman, rootfs: &Path, custom: &str) -> Result<()> {
    for command in [
        vec![
            OsString::from("/usr/bin/truncate"),
            OsString::from("--size=0"),
            OsString::from("/etc/machine-id"),
        ],
        vec![
            OsString::from("/usr/bin/rm"),
            OsString::from("--force"),
            OsString::from("/var/lib/dbus/machine-id"),
            OsString::from("/var/lib/systemd/random-seed"),
        ],
        vec![
            OsString::from("/usr/bin/find"),
            OsString::from("/etc/ssh"),
            OsString::from("-maxdepth"),
            OsString::from("1"),
            OsString::from("-type"),
            OsString::from("f"),
            OsString::from("-name"),
            OsString::from("ssh_host_*_key*"),
            OsString::from("-delete"),
        ],
    ] {
        podman.run_in_rootfs(rootfs, custom, &command)?;
    }
    Ok(())
}

fn lock_machine(machine_dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(machine_dir.join("machine.lock"))
        .context("opening machine lock")?;
    file.try_lock_exclusive()
        .context("machine is busy with another lifecycle operation")?;
    Ok(file)
}

fn lock_stopped_machine(podman: &Podman, machine_dir: &Path, operation: &str) -> Result<File> {
    let lock = lock_machine(machine_dir)?;
    let config = MachineConfig::load(machine_dir)?;
    let runtime = PodmanRuntimePaths::discover(config.id)?;
    let definition = podman.definition_for_machine(&config, machine_dir, &runtime)?;
    if let Some(inspection) = podman.inspect(&definition.container_name)? {
        require_stopped(&inspection, operation)?;
    }
    Ok(lock)
}

fn require_stopped(inspection: &PodmanInspection, operation: &str) -> Result<()> {
    if matches!(
        inspection.state,
        PodmanContainerState::Running
            | PodmanContainerState::Paused
            | PodmanContainerState::Stopping
    ) {
        bail!("machine must be stopped before {operation}");
    }
    Ok(())
}

fn runtime_from_inspection(inspection: &PodmanInspection) -> RuntimeState {
    let mut state = RuntimeState::new(match inspection.state {
        PodmanContainerState::Running | PodmanContainerState::Paused => MachineState::Running,
        PodmanContainerState::Stopping => MachineState::Stopping,
        PodmanContainerState::Unknown => MachineState::Failed,
        PodmanContainerState::Configured
        | PodmanContainerState::Created
        | PodmanContainerState::Stopped
        | PodmanContainerState::Exited => MachineState::Stopped,
    });
    state.container_id = Some(inspection.id.clone());
    state.definition_digest = inspection.definition_digest.clone();
    state
}

fn save_inspection(
    machine_dir: &Path,
    inspection: &PodmanInspection,
    digest: Option<&str>,
    detail: Option<String>,
) -> Result<()> {
    let mut state = runtime_from_inspection(inspection);
    if let Some(digest) = digest {
        state.definition_digest = Some(digest.to_owned());
    }
    state.detail = detail;
    state.updated_at = Utc::now();
    state.save(machine_dir)
}

fn save_stopped(
    machine_dir: &Path,
    inspection: Option<&PodmanInspection>,
    detail: Option<String>,
) -> Result<()> {
    let mut state = inspection
        .map(runtime_from_inspection)
        .unwrap_or_else(|| RuntimeState::new(MachineState::Stopped));
    state.state = MachineState::Stopped;
    state.detail = detail;
    state.updated_at = Utc::now();
    state.save(machine_dir)
}

fn cleanup_tree(podman: &Podman, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(first_error) => podman
            .remove_external_tree(path)
            .with_context(|| format!("removing {} after {first_error}", path.display())),
    }
}

fn encode_config_label(config: &MachineConfig) -> Result<String> {
    let bytes = serde_json::to_vec(config)?;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(encoded)
}

fn decode_config_label(value: &str) -> Result<MachineConfig> {
    if value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Buzzard machine-config label is not valid hexadecimal JSON");
    }
    let bytes = value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).expect("validated ASCII");
            u8::from_str_radix(pair, 16).context("decoding machine-config label")
        })
        .collect::<Result<Vec<_>>>()?;
    let config: MachineConfig =
        serde_json::from_slice(&bytes).context("parsing exported Buzzard machine config")?;
    Ok(config)
}

fn combine_result<T>(result: Result<T>, cleanup: Result<()>, action: &str) -> Result<T> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup).context(action.to_owned()),
        (Err(error), Err(cleanup)) => {
            Err(error).context(format!("{action} also failed: {cleanup:#}"))
        }
    }
}

trait ValidateMachineConfig {
    fn save_to_validation_only(&self) -> Result<()>;
}

impl ValidateMachineConfig for MachineConfig {
    fn save_to_validation_only(&self) -> Result<()> {
        let temp = tempfile::tempdir().context("creating metadata validation directory")?;
        self.save(temp.path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exported_machine_config_label_round_trips_without_shell_syntax() {
        let mut config = MachineConfig::new(
            "demo".into(),
            "image".into(),
            format!("sha256:{}", "0".repeat(64)),
            wb_core::NetworkMode::User,
            Vec::new(),
        );
        config.custom_podman_arguments = "--userns=keep-id --annotation 'a=b c'".into();
        let encoded = encode_config_label(&config).unwrap();
        assert!(encoded.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let decoded = decode_config_label(&encoded).unwrap();
        assert_eq!(decoded.id, config.id);
        assert_eq!(
            decoded.custom_podman_arguments,
            config.custom_podman_arguments
        );
    }

    #[test]
    fn parses_native_podman_load_result() {
        assert_eq!(
            loaded_image_reference("Loaded image: localhost/demo:latest\n").unwrap(),
            "localhost/demo:latest"
        );
        assert_eq!(
            loaded_image_reference("Loaded image ID: sha256:abc\n").unwrap(),
            "sha256:abc"
        );
    }

    #[test]
    fn external_rootfs_snapshot_retains_complete_oci_intent_as_direct_changes() {
        let mut config = MachineConfig::new(
            "demo".into(),
            "image".into(),
            format!("sha256:{}", "0".repeat(64)),
            wb_core::NetworkMode::User,
            Vec::new(),
        );
        config.oci.environment = vec!["EXAMPLE=value with spaces".into()];
        config
            .oci
            .labels
            .insert("example.label".into(), "value".into());
        config.oci.working_dir = Some("/workspace".into());
        config.oci.user = Some("1000:1000".into());
        config.oci.entrypoint = vec!["/usr/bin/env".into()];
        config.oci.command = vec!["bash".into(), "-l".into()];
        config.oci.stop_signal = Some("SIGRTMIN+3".into());

        let changes = oci_import_changes(&config)
            .unwrap()
            .into_iter()
            .map(|change| change.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(changes.contains(&"ENV EXAMPLE=value with spaces".into()));
        assert!(changes.contains(&"LABEL example.label=value".into()));
        assert!(changes.contains(&"WORKDIR /workspace".into()));
        assert!(changes.contains(&"USER 1000:1000".into()));
        assert!(changes.contains(&"ENTRYPOINT [\"/usr/bin/env\"]".into()));
        assert!(changes.contains(&"CMD [\"bash\",\"-l\"]".into()));
        assert!(changes.contains(&"STOPSIGNAL SIGRTMIN+3".into()));
        assert!(changes.iter().any(|change| {
            change.starts_with(&format!("LABEL {CONFIG_LABEL}="))
                && change
                    .trim_start_matches(&format!("LABEL {CONFIG_LABEL}="))
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
        }));
    }
}
