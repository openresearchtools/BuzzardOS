// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use std::env;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ResourceLocator {
    roots: Vec<PathBuf>,
    asset_roots: Vec<PathBuf>,
    packaged: bool,
}

impl ResourceLocator {
    pub fn discover() -> Result<Self> {
        let exe = env::current_exe().context("locating current executable")?;
        let mut roots = Vec::new();
        let mut asset_roots = Vec::new();
        let packaged = env::var_os("APPIMAGE").is_some() || env::var_os("APPDIR").is_some();

        if let Some(appdir) = env::var_os("APPDIR") {
            let appdir = PathBuf::from(appdir);
            roots.push(appdir.join("usr/libexec/wildbuzzard"));
            asset_roots.push(appdir.join("usr/share/wildbuzzard"));
        }
        if let Some(parent) = exe.parent() {
            roots.push(parent.join("../libexec/wildbuzzard"));
            roots.push(parent.to_path_buf());
            asset_roots.push(parent.join("../share/wildbuzzard"));
        }
        if !packaged && let Some(root) = env::var_os("WILDBUZZARD_RESOURCE_DIR") {
            let root = PathBuf::from(root);
            roots.insert(0, root.clone());
            asset_roots.insert(0, root);
        }

        Ok(Self {
            roots,
            asset_roots,
            packaged,
        })
    }

    pub fn helper(&self, name: &str) -> Result<PathBuf> {
        self.find_executable(name).ok_or_else(|| {
            anyhow::anyhow!(
                "the bundled helper '{name}' is missing; this is a broken Wild Buzzard build"
            )
        })
    }

    pub fn helper_or_path(&self, name: &str) -> Result<PathBuf> {
        if let Some(path) = self.find_executable(name) {
            return Ok(path);
        }
        if self.packaged {
            return self.helper(name);
        }
        if let Some(path) = find_on_path(name) {
            return Ok(path);
        }
        bail!(
            "cannot find '{name}'; release AppImages bundle it, or set WILDBUZZARD_RESOURCE_DIR for a development build"
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
    /// `usr/share/wildbuzzard`; development builds may place the same layout
    /// under `WILDBUZZARD_RESOURCE_DIR`. The canonical result prevents a
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
