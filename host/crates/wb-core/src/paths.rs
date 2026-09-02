// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::env;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Paths for one exact machine directory selected by the user.
#[derive(Debug, Clone)]
pub struct WbPaths {
    machine_dir: PathBuf,
}

impl WbPaths {
    pub fn for_machine(machine_dir: &Path) -> Result<Self> {
        let machine_dir = physical_absolute(machine_dir)?;
        if machine_dir.parent().is_none() {
            bail!("machine directory cannot be the filesystem root");
        }
        Ok(Self { machine_dir })
    }

    /// Parent used only for atomic staging next to the selected final path.
    pub fn machines(&self) -> PathBuf {
        self.machine_dir
            .parent()
            .expect("validated machine directory has a parent")
            .to_path_buf()
    }

    /// Cache belongs to this machine, not to an application-global store.
    pub fn cache(&self) -> PathBuf {
        self.machine_dir.join("cache")
    }

    pub fn machine(&self, _name: &str) -> PathBuf {
        self.machine_dir.clone()
    }

    /// Ensure only the selected path's parent. The final machine path stays
    /// absent until create/import commits its fully prepared staging tree.
    pub fn ensure(&self) -> Result<()> {
        let parent = self.machines();
        std::fs::create_dir_all(&parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        let metadata = std::fs::symlink_metadata(&parent)
            .with_context(|| format!("inspecting {}", parent.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "machine parent {} must be a real directory, not a symlink",
                parent.display()
            );
        }
        Ok(())
    }
}

fn physical_absolute(path: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(path)
        .with_context(|| format!("resolving machine directory {}", path.display()))?;
    let mut cursor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match cursor.canonicalize() {
            Ok(mut physical) => {
                for component in missing.into_iter().rev() {
                    if component == ".." {
                        physical.pop();
                    } else if component != "." {
                        physical.push(component);
                    }
                }
                return Ok(physical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = cursor
                    .components()
                    .next_back()
                    .context("machine directory has no existing ancestor")?
                    .as_os_str()
                    .to_owned();
                missing.push(component);
                cursor = cursor
                    .parent()
                    .context("machine directory has no existing ancestor")?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("resolving machine directory {}", path.display()));
            }
        }
    }
}

pub fn host_control_socket(machine_dir: &Path) -> Result<PathBuf> {
    if !machine_dir.is_absolute() {
        bail!(
            "machine directory must be absolute when deriving its host runtime socket: {}",
            machine_dir.display()
        );
    }
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR is required for the host window control socket")?;
    if !runtime.is_absolute() {
        bail!("XDG_RUNTIME_DIR must be absolute: {}", runtime.display());
    }
    host_control_socket_in(&runtime, machine_dir)
}

fn host_control_socket_in(runtime: &Path, machine_dir: &Path) -> Result<PathBuf> {
    if !runtime.is_absolute() {
        bail!("runtime directory must be absolute: {}", runtime.display());
    }
    let digest = Sha256::digest(machine_dir.as_os_str().as_bytes());
    let key = format!("{digest:x}");
    Ok(runtime
        .join("buzzardos")
        .join(format!("window-{}.sock", &key[..24])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn machine_parent_cannot_be_symlinked_elsewhere() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), temp.path().join("machines")).unwrap();
        let paths = WbPaths {
            machine_dir: temp.path().join("machines/demo"),
        };
        assert!(paths.ensure().is_err());
    }

    #[test]
    fn selected_machine_path_is_exact() {
        let temp = tempfile::tempdir().unwrap();
        let selected = temp.path().join("disk-a/custom-folder");
        let paths = WbPaths::for_machine(&selected).unwrap();
        paths.ensure().unwrap();
        assert_eq!(paths.machine("different-logical-name"), selected);
        assert_eq!(paths.cache(), selected.join("cache"));
        assert_eq!(paths.machines(), selected.parent().unwrap());
    }

    #[test]
    fn host_control_socket_stays_short_for_long_machine_paths() {
        let runtime = Path::new("/tmp/buzzardos-test-runtime");
        let machine = PathBuf::from("/tmp").join("very-long-name".repeat(24));
        let socket = host_control_socket_in(runtime, &machine).unwrap();
        assert!(socket.as_os_str().as_bytes().len() < 108);
        assert!(socket.starts_with(runtime));
    }

    #[test]
    fn relative_and_absolute_paths_have_one_runtime_identity() {
        let relative = WbPaths::for_machine(Path::new("./machine")).unwrap();
        let absolute = WbPaths::for_machine(&env::current_dir().unwrap().join("machine")).unwrap();
        assert_eq!(relative.machine("demo"), absolute.machine("demo"));
    }

    #[test]
    fn symlinked_parent_uses_physical_machine_identity() {
        let temp = tempfile::tempdir().unwrap();
        let physical = temp.path().join("physical");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&physical).unwrap();
        symlink(&physical, &alias).unwrap();
        let direct = WbPaths::for_machine(&physical.join("demo")).unwrap();
        let linked = WbPaths::for_machine(&alias.join("demo")).unwrap();
        assert_eq!(linked.machine("demo"), direct.machine("demo"));
    }
}
