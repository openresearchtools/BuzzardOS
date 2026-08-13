// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::env;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct WbPaths {
    base: PathBuf,
}

impl WbPaths {
    pub fn discover(override_base: Option<&Path>) -> Result<Self> {
        let base = match override_base {
            Some(path) => path.to_path_buf(),
            None => portable_base()?,
        };
        // Runtime-only paths such as the host control socket must have one
        // stable identity regardless of whether the CLI was given `./dist`,
        // `dist`, or an absolute spelling. Portable machine metadata remains
        // relative; this normalization is never persisted into machine.json.
        let base = std::path::absolute(&base)
            .with_context(|| format!("resolving portable folder {}", base.display()))?;
        Ok(Self { base })
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn machines(&self) -> PathBuf {
        self.base.join("Machines")
    }

    pub fn cache(&self) -> PathBuf {
        self.machines().join(".cache")
    }

    pub fn shared(&self) -> PathBuf {
        self.base.join("shared")
    }

    pub fn machine(&self, name: &str) -> PathBuf {
        self.machines().join(name)
    }

    pub fn ensure(&self) -> Result<()> {
        for directory in [self.machines(), self.cache(), self.shared()] {
            std::fs::create_dir_all(&directory)
                .with_context(|| format!("creating {}", directory.display()))?;
            let metadata = std::fs::symlink_metadata(&directory)
                .with_context(|| format!("inspecting {}", directory.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "portable directory {} must be a real directory, not a symlink",
                    directory.display()
                );
            }
        }
        Ok(())
    }
}

fn portable_base() -> Result<PathBuf> {
    // The top-level BuzzardOS executable pins the portable root explicitly.
    // This avoids deriving durable state from the caller's current directory
    // or from the dependency payload under app/.
    if let Some(portable) = env::var_os("BUZZARDOS_PORTABLE_DIR") {
        let path = PathBuf::from(portable);
        if !path.is_absolute() {
            bail!(
                "BUZZARDOS_PORTABLE_DIR must be absolute: {}",
                path.display()
            );
        }
        return Ok(path);
    }

    // Development builds behave predictably from the directory in which they
    // are launched. Packaged builds always take the explicit branch above.
    env::current_dir().context("determining portable storage directory")
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
        .join("wildbuzzard")
        .join(format!("window-{}.sock", &key[..24])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn portable_directories_cannot_be_symlinked_elsewhere() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), temp.path().join("Machines")).unwrap();
        let paths = WbPaths {
            base: temp.path().to_path_buf(),
        };
        assert!(paths.ensure().is_err());
    }

    #[test]
    fn portable_layout_relocates_without_rewriting_paths() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original");
        let relocated = temp.path().join("relocated");
        let paths = WbPaths::discover(Some(&original)).unwrap();
        paths.ensure().unwrap();
        std::fs::create_dir_all(paths.machine("demo").join("rootfs")).unwrap();
        std::fs::write(paths.machine("demo").join("rootfs/marker"), b"persistent").unwrap();
        std::fs::write(paths.shared().join("marker"), b"portable").unwrap();

        std::fs::rename(&original, &relocated).unwrap();
        let moved = WbPaths::discover(Some(&relocated)).unwrap();

        assert_eq!(moved.base(), relocated);
        assert_eq!(
            std::fs::read(moved.machine("demo").join("rootfs/marker")).unwrap(),
            b"persistent"
        );
        assert_eq!(
            std::fs::read(moved.shared().join("marker")).unwrap(),
            b"portable"
        );
        assert!(!original.exists());
    }

    #[test]
    fn host_control_socket_stays_short_for_long_portable_paths() {
        let runtime = Path::new("/tmp/buzzardos-test-runtime");
        let machine = PathBuf::from("/tmp")
            .join("very-long-portable-folder-name".repeat(12))
            .join("Machines/demo");
        let socket = host_control_socket_in(runtime, &machine).unwrap();

        assert!(socket.as_os_str().as_bytes().len() < 108);
        assert!(socket.starts_with(runtime));
        assert_ne!(
            socket,
            host_control_socket_in(runtime, &PathBuf::from("/tmp/elsewhere/Machines/demo"))
                .unwrap()
        );
    }

    #[test]
    fn relative_storage_override_has_one_absolute_runtime_identity() {
        let relative = WbPaths::discover(Some(Path::new("./portable"))).unwrap();
        let absolute =
            WbPaths::discover(Some(&std::env::current_dir().unwrap().join("portable"))).unwrap();

        assert!(relative.base().is_absolute());
        assert_eq!(relative.base(), absolute.base());
        let runtime = Path::new("/tmp/buzzardos-test-runtime");
        assert_eq!(
            host_control_socket_in(runtime, &relative.machine("demo")).unwrap(),
            host_control_socket_in(runtime, &absolute.machine("demo")).unwrap()
        );
    }
}
