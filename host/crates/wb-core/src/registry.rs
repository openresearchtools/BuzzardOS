// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::{MachineConfig, MachineState, RuntimeState};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisteredMachine {
    pub id: Uuid,
    pub name: String,
    pub machine_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryDocument {
    schema: u32,
    machines: Vec<RegisteredMachine>,
}

#[derive(Debug, Clone)]
pub struct MachineRegistry {
    path: PathBuf,
    machines: Vec<RegisteredMachine>,
}

impl MachineRegistry {
    pub const SCHEMA: u32 = 1;

    pub fn discover() -> Result<Self> {
        let config_home = match env::var_os("XDG_CONFIG_HOME") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(env::var_os("HOME").context(
                "HOME or XDG_CONFIG_HOME is required to locate the Buzzard OS registry",
            )?)
            .join(".config"),
        };
        if !config_home.is_absolute() {
            bail!(
                "XDG_CONFIG_HOME must be absolute: {}",
                config_home.display()
            );
        }
        Self::open(config_home.join(crate::host_identity().package).join("machines.json"))
    }

    pub fn open(path: PathBuf) -> Result<Self> {
        if !path.is_absolute() {
            bail!("machine registry path must be absolute: {}", path.display());
        }
        let machines = match fs::read(&path) {
            Ok(bytes) => {
                let document: RegistryDocument = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if document.schema != Self::SCHEMA {
                    bail!("unsupported machine registry schema {}", document.schema);
                }
                validate_entries(&document.machines)?;
                document.machines
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", path.display()));
            }
        };
        Ok(Self { path, machines })
    }

    pub fn entries(&self) -> &[RegisteredMachine] {
        &self.machines
    }

    pub fn resolve(&self, name: &str) -> Result<PathBuf> {
        MachineConfig::validate_name(name)?;
        let entry = self
            .machines
            .iter()
            .find(|entry| entry.name == name)
            .with_context(|| format!("machine '{name}' is not registered"))?;
        verify_entry(entry)?;
        Ok(entry.machine_dir.clone())
    }

    pub fn register(&mut self, machine_dir: &Path) -> Result<()> {
        let machine_dir = machine_dir
            .canonicalize()
            .with_context(|| format!("resolving machine directory {}", machine_dir.display()))?;
        let config = MachineConfig::load(&machine_dir)?;
        if let Some(index) = self
            .machines
            .iter()
            .position(|entry| entry.name == config.name || entry.id == config.id)
        {
            let entry = &self.machines[index];
            if entry.name == config.name
                && entry.id == config.id
                && entry.machine_dir == machine_dir
            {
                return Ok(());
            }
            let same_identity = entry.name == config.name && entry.id == config.id;
            let old_directory_is_missing = matches!(
                fs::symlink_metadata(&entry.machine_dir),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            );
            if same_identity && old_directory_is_missing {
                if RuntimeState::load(&machine_dir)?.is_some_and(|state| {
                    matches!(
                        state.state,
                        MachineState::Starting | MachineState::Running | MachineState::Stopping
                    )
                }) {
                    bail!(
                        "machine '{}' is still running and cannot be re-registered at its moved directory",
                        config.name
                    );
                }
                if let Some(other) = self
                    .machines
                    .iter()
                    .enumerate()
                    .find(|(other_index, other)| {
                        *other_index != index && other.machine_dir == machine_dir
                    })
                    .map(|(_, other)| other)
                {
                    bail!(
                        "machine directory {} is already registered as '{}'",
                        machine_dir.display(),
                        other.name
                    );
                }
                self.machines[index].machine_dir = machine_dir;
                return self.save();
            }
            bail!(
                "machine '{}' conflicts with registered machine '{}' at {}",
                config.name,
                entry.name,
                entry.machine_dir.display()
            );
        }
        if let Some(entry) = self
            .machines
            .iter()
            .find(|entry| entry.machine_dir == machine_dir)
        {
            bail!(
                "machine directory {} is already registered as '{}'",
                machine_dir.display(),
                entry.name
            );
        }
        self.machines.push(RegisteredMachine {
            id: config.id,
            name: config.name,
            machine_dir,
        });
        self.machines
            .sort_by(|left, right| left.name.cmp(&right.name));
        self.save()
    }

    pub fn unregister(&mut self, name: &str) -> Result<()> {
        let before = self.machines.len();
        self.machines.retain(|entry| entry.name != name);
        if self.machines.len() == before {
            bail!("machine '{name}' is not registered");
        }
        self.save()
    }

    fn save(&self) -> Result<()> {
        validate_entries(&self.machines)?;
        let parent = self.path.parent().context("registry path has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("protecting {}", parent.display()))?;
        let temporary = self
            .path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        let document = RegistryDocument {
            schema: Self::SCHEMA,
            machines: self.machines.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&document)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        let write_result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)
                .with_context(|| format!("committing {}", self.path.display()))?;
            fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }
}

fn validate_entries(entries: &[RegisteredMachine]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for entry in entries {
        MachineConfig::validate_name(&entry.name)?;
        if !entry.machine_dir.is_absolute() {
            bail!(
                "registered machine directory must be absolute: {}",
                entry.machine_dir.display()
            );
        }
        if !ids.insert(entry.id)
            || !names.insert(entry.name.as_str())
            || !paths.insert(entry.machine_dir.as_path())
        {
            bail!("machine registry contains a duplicate id, name, or directory");
        }
    }
    Ok(())
}

fn verify_entry(entry: &RegisteredMachine) -> Result<()> {
    let metadata = fs::symlink_metadata(&entry.machine_dir)
        .with_context(|| format!("inspecting {}", entry.machine_dir.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!(
            "registered machine directory must be a real directory: {}",
            entry.machine_dir.display()
        );
    }
    let config = MachineConfig::load(&entry.machine_dir)?;
    if config.id != entry.id || config.name != entry.name {
        bail!(
            "registered metadata for '{}' does not match {}",
            entry.name,
            entry.machine_dir.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MachineState, NetworkMode, RuntimeState};

    #[test]
    fn registry_is_only_an_index_to_self_describing_machine_directories() {
        let temp = tempfile::tempdir().unwrap();
        let machine_dir = temp.path().join("data-disk/demo");
        fs::create_dir_all(machine_dir.join("rootfs")).unwrap();
        let config = MachineConfig::new(
            "demo".into(),
            "oci:example".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        );
        config.save(&machine_dir).unwrap();
        RuntimeState::new(MachineState::Stopped)
            .save(&machine_dir)
            .unwrap();

        let registry_path = temp.path().join("config/buzzardos/machines.json");
        let mut registry = MachineRegistry::open(registry_path.clone()).unwrap();
        registry.register(&machine_dir).unwrap();
        assert_eq!(
            registry.resolve("demo").unwrap(),
            machine_dir.canonicalize().unwrap()
        );

        let reopened = MachineRegistry::open(registry_path).unwrap();
        assert_eq!(reopened.entries().len(), 1);
        assert_eq!(reopened.entries()[0].name, "demo");
    }

    #[test]
    fn registering_the_same_identity_relocates_a_missing_old_directory() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("data-disk/demo");
        let moved = temp.path().join("other-folder/demo");
        fs::create_dir_all(original.join("rootfs")).unwrap();
        fs::create_dir_all(moved.parent().unwrap()).unwrap();
        let config = MachineConfig::new(
            "demo".into(),
            "oci:example".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        );
        config.save(&original).unwrap();

        let registry_path = temp.path().join("config/buzzardos/machines.json");
        let mut registry = MachineRegistry::open(registry_path.clone()).unwrap();
        registry.register(&original).unwrap();
        fs::rename(&original, &moved).unwrap();

        registry.register(&moved).unwrap();
        assert_eq!(registry.entries().len(), 1);
        assert_eq!(
            registry.resolve("demo").unwrap(),
            moved.canonicalize().unwrap()
        );
        let reopened = MachineRegistry::open(registry_path).unwrap();
        assert_eq!(
            reopened.entries()[0].machine_dir,
            moved.canonicalize().unwrap()
        );
    }

    #[test]
    fn registering_a_duplicate_identity_does_not_replace_a_live_directory() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original/demo");
        let duplicate = temp.path().join("duplicate/demo");
        fs::create_dir_all(original.join("rootfs")).unwrap();
        fs::create_dir_all(duplicate.join("rootfs")).unwrap();
        let config = MachineConfig::new(
            "demo".into(),
            "oci:example".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        );
        config.save(&original).unwrap();
        config.save(&duplicate).unwrap();

        let registry_path = temp.path().join("config/buzzardos/machines.json");
        let mut registry = MachineRegistry::open(registry_path).unwrap();
        registry.register(&original).unwrap();
        let error = registry.register(&duplicate).unwrap_err().to_string();

        assert!(error.contains("conflicts with registered machine"));
        assert_eq!(
            registry.resolve("demo").unwrap(),
            original.canonicalize().unwrap()
        );
    }

    #[test]
    fn registering_a_moved_running_machine_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("data-disk/demo");
        let moved = temp.path().join("other-folder/demo");
        fs::create_dir_all(original.join("rootfs")).unwrap();
        fs::create_dir_all(moved.parent().unwrap()).unwrap();
        let config = MachineConfig::new(
            "demo".into(),
            "oci:example".into(),
            format!("sha256:{}", "0".repeat(64)),
            NetworkMode::User,
            vec!["all".into()],
        );
        config.save(&original).unwrap();
        RuntimeState::new(MachineState::Running)
            .save(&original)
            .unwrap();

        let registry_path = temp.path().join("config/buzzardos/machines.json");
        let mut registry = MachineRegistry::open(registry_path).unwrap();
        registry.register(&original).unwrap();
        fs::rename(&original, &moved).unwrap();

        let error = registry.register(&moved).unwrap_err().to_string();
        assert!(error.contains("is still running"));
        assert_eq!(registry.entries()[0].machine_dir, original);
    }
}
