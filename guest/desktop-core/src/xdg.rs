// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::appimage::RegistrationId;
use glib::UserDirectory;
use std::collections::HashSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XdgPaths {
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub data_home: PathBuf,
    pub state_home: PathBuf,
    pub system_data_dirs: Vec<PathBuf>,
    pub desktop_dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum XdgPathError {
    #[error("{name} must be an absolute, lexically normalized path: {path}")]
    InvalidBase { name: &'static str, path: String },
    #[error("XDG system data directory list must not be empty")]
    NoSystemDataDirectories,
    #[error("cannot create XDG directory {path}: {source}")]
    Create {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("XDG directory must not be a symbolic link: {0}")]
    Symlink(String),
}

impl XdgPaths {
    /// Resolve standard XDG paths through GLib, including user-dirs for the
    /// desktop. GLib applies the XDG environment defaults and user-dirs file.
    pub fn discover() -> Result<Self, XdgPathError> {
        let home = glib::home_dir();
        let desktop_dir =
            glib::user_special_dir(UserDirectory::Desktop).unwrap_or_else(|| home.join("Desktop"));
        Self::from_bases(
            home,
            glib::user_config_dir(),
            glib::user_data_dir(),
            glib::user_state_dir(),
            glib::system_data_dirs(),
            desktop_dir,
        )
    }

    pub fn from_bases(
        home: PathBuf,
        config_home: PathBuf,
        data_home: PathBuf,
        state_home: PathBuf,
        system_data_dirs: Vec<PathBuf>,
        desktop_dir: PathBuf,
    ) -> Result<Self, XdgPathError> {
        validate_base("HOME", &home)?;
        validate_base("XDG_CONFIG_HOME", &config_home)?;
        validate_base("XDG_DATA_HOME", &data_home)?;
        validate_base("XDG_STATE_HOME", &state_home)?;
        validate_base("XDG_DESKTOP_DIR", &desktop_dir)?;
        if system_data_dirs.is_empty() {
            return Err(XdgPathError::NoSystemDataDirectories);
        }
        let mut deduplicated = Vec::new();
        let mut seen = HashSet::new();
        for directory in system_data_dirs {
            validate_base("XDG_DATA_DIRS", &directory)?;
            if seen.insert(directory.clone()) {
                deduplicated.push(directory);
            }
        }
        Ok(Self {
            home,
            config_home,
            data_home,
            state_home,
            system_data_dirs: deduplicated,
            desktop_dir,
        })
    }

    pub fn settings_path(&self) -> PathBuf {
        self.config_home.join("wildbuzzard/settings.json")
    }

    pub fn appimage_registration_dir(&self) -> PathBuf {
        self.data_home.join("wildbuzzard/appimages")
    }

    pub fn appimage_registration_path(&self, id: RegistrationId) -> PathBuf {
        self.appimage_registration_dir()
            .join(id.registration_filename())
    }

    pub fn user_applications_dir(&self) -> PathBuf {
        self.data_home.join("applications")
    }

    pub fn application_dirs(&self) -> Vec<PathBuf> {
        let mut directories = vec![self.user_applications_dir()];
        directories.extend(
            self.system_data_dirs
                .iter()
                .map(|directory| directory.join("applications")),
        );
        directories
    }

    pub fn managed_appimage_desktop_path(&self, id: RegistrationId) -> PathBuf {
        self.user_applications_dir().join(id.desktop_file_id())
    }

    pub fn managed_appimage_icon_dir(&self, size: u16) -> PathBuf {
        self.data_home
            .join("icons/hicolor")
            .join(format!("{size}x{size}/apps"))
    }

    pub fn managed_state_dir(&self) -> PathBuf {
        self.state_home.join("wildbuzzard")
    }

    /// Create only Wild Buzzard-owned private state directories. General XDG
    /// application, icon, and Desktop directories are left to their owners.
    pub fn ensure_private_directories(&self) -> Result<(), XdgPathError> {
        for path in [
            self.settings_path()
                .parent()
                .expect("settings has parent")
                .to_path_buf(),
            self.appimage_registration_dir(),
            self.managed_state_dir(),
        ] {
            fs::create_dir_all(&path).map_err(|source| XdgPathError::Create {
                path: path.display().to_string(),
                source,
            })?;
            let metadata = fs::symlink_metadata(&path).map_err(|source| XdgPathError::Create {
                path: path.display().to_string(),
                source,
            })?;
            if metadata.file_type().is_symlink() {
                return Err(XdgPathError::Symlink(path.display().to_string()));
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(|source| {
                XdgPathError::Create {
                    path: path.display().to_string(),
                    source,
                }
            })?;
        }
        Ok(())
    }
}

fn validate_base(name: &'static str, path: &Path) -> Result<(), XdgPathError> {
    let bytes = path.as_os_str().as_bytes();
    let relative = bytes
        .get(1..)
        .and_then(|value| value.strip_suffix(b"/").or(Some(value)))
        .unwrap_or_default();
    if !path.is_absolute()
        || (path != Path::new("/")
            && relative
                .split(|byte| *byte == b'/')
                .any(|component| component.is_empty() || component == b"." || component == b".."))
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(XdgPathError::InvalidBase {
            name,
            path: path.display().to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> XdgPaths {
        XdgPaths::from_bases(
            root.join("home"),
            root.join("config"),
            root.join("data"),
            root.join("state"),
            vec![root.join("local-share"), root.join("share")],
            root.join("Elsewhere/Desktop"),
        )
        .unwrap()
    }

    #[test]
    fn xdg_layout_matches_the_persistent_contract() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        assert!(
            paths
                .settings_path()
                .ends_with("config/wildbuzzard/settings.json")
        );
        assert!(
            paths
                .appimage_registration_dir()
                .ends_with("data/wildbuzzard/appimages")
        );
        assert_eq!(paths.desktop_dir, temp.path().join("Elsewhere/Desktop"));
        assert_eq!(paths.application_dirs().len(), 3);
    }

    #[test]
    fn relative_and_traversing_xdg_bases_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        for hostile in [
            PathBuf::from("relative"),
            temp.path().join("a/../escape"),
            temp.path().join("a/./alias"),
        ] {
            let result = XdgPaths::from_bases(
                temp.path().join("home"),
                hostile,
                temp.path().join("data"),
                temp.path().join("state"),
                vec![temp.path().join("share")],
                temp.path().join("Desktop"),
            );
            assert!(matches!(result, Err(XdgPathError::InvalidBase { .. })));
        }
    }

    #[test]
    fn standard_glib_system_data_directories_may_have_one_trailing_slash() {
        let temp = tempfile::tempdir().unwrap();
        let paths = XdgPaths::from_bases(
            temp.path().join("home"),
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("state"),
            vec![
                PathBuf::from("/usr/local/share/"),
                PathBuf::from("/usr/share/"),
            ],
            temp.path().join("Desktop"),
        )
        .unwrap();

        assert_eq!(
            paths.application_dirs(),
            vec![
                temp.path().join("data/applications"),
                PathBuf::from("/usr/local/share/applications"),
                PathBuf::from("/usr/share/applications"),
            ]
        );
    }

    #[test]
    fn private_directory_creation_rejects_final_symlink() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        fs::create_dir_all(paths.config_home.join("wildbuzzard")).unwrap();
        fs::create_dir_all(paths.data_home.join("wildbuzzard")).unwrap();
        fs::create_dir_all(paths.state_home.clone()).unwrap();
        let victim = temp.path().join("victim");
        fs::create_dir(&victim).unwrap();
        symlink(&victim, paths.managed_state_dir()).unwrap();
        assert!(matches!(
            paths.ensure_private_directories(),
            Err(XdgPathError::Symlink(_))
        ));
    }
}
