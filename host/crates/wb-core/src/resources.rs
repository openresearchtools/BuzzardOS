// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ResourceLocator {
    roots: Vec<PathBuf>,
    asset_roots: Vec<PathBuf>,
}

impl ResourceLocator {
    pub fn discover() -> Result<Self> {
        let exe = env::current_exe().context("locating current executable")?;
        let mut roots = Vec::new();
        let mut asset_roots = Vec::new();
        if let Some(parent) = exe.parent() {
            roots.push(parent.join("../libexec/buzzardos"));
            roots.push(parent.to_path_buf());
            asset_roots.push(parent.join("../share/buzzardos"));
        }
        if let Some(root) = env::var_os("BUZZARDOS_RESOURCE_DIR") {
            let root = PathBuf::from(root);
            roots.insert(0, root.clone());
            asset_roots.insert(0, root);
        }

        Ok(Self { roots, asset_roots })
    }

    pub fn helper(&self, name: &str) -> Result<PathBuf> {
        self.find_executable(name).ok_or_else(|| {
            anyhow::anyhow!(
                "the bundled helper '{name}' is missing; this is a broken Buzzard OS build"
            )
        })
    }

    pub fn helper_or_path(&self, name: &str) -> Result<PathBuf> {
        if let Some(path) = self.find_executable(name) {
            return Ok(path);
        }
        if let Some(path) = find_on_path(name) {
            return Ok(path);
        }
        bail!(
            "cannot find required helper '{name}'; install the matching Buzzard OS package dependency, or set BUZZARDOS_RESOURCE_DIR for a development build"
        )
    }

    pub fn asset(&self, relative: impl AsRef<Path>) -> Result<PathBuf> {
        let relative = relative.as_ref();
        for root in self.asset_roots.iter().chain(&self.roots) {
            let candidate = root.join(relative);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        bail!("bundled asset '{}' is missing", relative.display())
    }

    /// Resolve a bundled, read-only directory without consulting host PATH.
    ///
    /// Release payloads keep non-executable data under
    /// `usr/share/buzzardos`; development builds may place the same layout
    /// under `BUZZARDOS_RESOURCE_DIR`. The canonical result prevents a
    /// packaged symlink from redirecting a security-sensitive parser to
    /// mutable host data.
    pub fn asset_directory(&self, relative: impl AsRef<Path>) -> Result<PathBuf> {
        let relative = relative.as_ref();
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            bail!("bundled asset directory must be a normalized relative path");
        }
        for root in &self.asset_roots {
            let canonical_root = match root.canonicalize() {
                Ok(root) if root.is_dir() => root,
                _ => continue,
            };
            let candidate = root.join(relative);
            let canonical = match candidate.canonicalize() {
                Ok(candidate) if candidate.is_dir() => candidate,
                _ => continue,
            };
            if canonical.starts_with(&canonical_root) {
                return Ok(canonical);
            }
        }
        bail!(
            "bundled asset directory '{}' is missing",
            relative.display()
        )
    }

    fn find_executable(&self, name: &str) -> Option<PathBuf> {
        self.roots
            .iter()
            .map(|root| root.join(name))
            .find(|path| is_executable(path))
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|path| path.join(name))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
