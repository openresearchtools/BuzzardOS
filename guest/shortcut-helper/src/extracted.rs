// SPDX-License-Identifier: AGPL-3.0-or-later

//! Persistent, source-adjacent AppImage extraction and launch.

use crate::{ValidatedAppImage, validate_appimage};
use anyhow::{Context, Result, ensure};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const EXTRACTED_SUFFIX: &str = ".extracted";
const NO_SANDBOX_MARKER: &str = ".no-sandbox";
const MAX_DESKTOP_BYTES: u64 = 1024 * 1024;

pub fn extracted_path(source: &Path) -> PathBuf {
    let mut name = source.as_os_str().to_os_string();
    name.push(EXTRACTED_SUFFIX);
    PathBuf::from(name)
}

pub fn launch_path(source: &Path) -> Result<Child> {
    let validated = validate_appimage(source).context("validate AppImage")?;
    launch_validated(&validated)
}

pub fn launch_validated(validated: &ValidatedAppImage) -> Result<Child> {
    let destination = extracted_path(validated.path());
    match fs::symlink_metadata(&destination) {
        Ok(_) => launch_extracted(validated, &destination),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            validated
                .authorize_owner_execute()
                .context("authorize AppImage execution")?;
            validated.spawn_exact().context("launch AppImage")
        }
        Err(error) => Err(error).with_context(|| format!("inspect {}", destination.display())),
    }
}

pub fn extract_and_launch(source: &Path, no_sandbox: bool) -> Result<Child> {
    let validated = validate_appimage(source).context("validate AppImage")?;
    let destination = extracted_path(source);
    match fs::symlink_metadata(&destination) {
        Ok(_) => validate_extracted_directory(&destination)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            extract_atomically(&validated, &destination)?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", destination.display()));
        }
    }
    if no_sandbox {
        ensure_no_sandbox_marker(&destination)?;
    }
    launch_extracted(&validated, &destination)
}

fn extract_atomically(validated: &ValidatedAppImage, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .context("AppImage extraction has no parent directory")?;
    validate_owned_directory(parent)?;
    let stem = destination
        .file_name()
        .context("AppImage extraction has no file name")?;
    let mut temporary = None;
    for attempt in 0_u32..128 {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut name = OsString::from(".");
        name.push(stem);
        name.push(format!(".tmp-{}-{nonce}-{attempt}", std::process::id()));
        let candidate = parent.join(name);
        match fs::create_dir(&candidate) {
            Ok(()) => {
                fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("secure {}", candidate.display()))?;
                temporary = Some(candidate);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("create {}", candidate.display()));
            }
        }
    }
    let temporary = temporary.context("could not allocate a private extraction directory")?;
    let result = (|| -> Result<()> {
        validated
            .extract_to(&temporary)
            .context("extract AppImage payload")?;
        validate_apprun(&temporary)?;
        fs::rename(&temporary, destination)
            .with_context(|| format!("commit {}", destination.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result
}

fn validate_owned_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect directory {}", path.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "directory is not a real directory: {}",
        path.display()
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "directory is not owned by the current guest user: {}",
        path.display()
    );
    Ok(())
}

fn validate_extracted_directory(path: &Path) -> Result<()> {
    validate_owned_directory(path)?;
    validate_apprun(path).map(|_| ())
}

fn validate_apprun(directory: &Path) -> Result<PathBuf> {
    let canonical_directory = directory
        .canonicalize()
        .with_context(|| format!("resolve {}", directory.display()))?;
    let candidate = directory.join("AppRun");
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("resolve {}", candidate.display()))?;
    ensure!(
        resolved.starts_with(&canonical_directory),
        "AppRun escapes the extracted directory"
    );
    let metadata =
        fs::metadata(&resolved).with_context(|| format!("inspect {}", resolved.display()))?;
    ensure!(metadata.is_file(), "AppRun is not a regular file");
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "AppRun is not owned by the current guest user"
    );
    if metadata.permissions().mode() & libc::S_IXUSR == 0 {
        fs::set_permissions(
            &resolved,
            fs::Permissions::from_mode(metadata.permissions().mode() | libc::S_IXUSR),
        )
        .with_context(|| format!("authorize {}", resolved.display()))?;
    }
    Ok(resolved)
}

fn ensure_no_sandbox_marker(directory: &Path) -> Result<()> {
    let path = directory.join(NO_SANDBOX_MARKER);
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(file) => file
            .sync_all()
            .with_context(|| format!("sync {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("inspect {}", path.display()))?;
            ensure!(
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.permissions().mode() & 0o777 == 0o600
                    && metadata.len() == 0,
                "existing no-sandbox marker is unsafe: {}",
                path.display()
            );
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("create {}", path.display())),
    }
}

fn no_sandbox_approved(directory: &Path) -> bool {
    fs::symlink_metadata(directory.join(NO_SANDBOX_MARKER)).is_ok_and(|metadata| {
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o777 == 0o600
            && metadata.len() == 0
    })
}

fn has_desktop_field_code(argument: &str) -> bool {
    let without_literal_percent = argument.replace("%%", "");
    without_literal_percent
        .as_bytes()
        .windows(2)
        .any(|pair| pair[0] == b'%' && pair[1].is_ascii_alphabetic())
}

fn extracted_desktop_arguments(directory: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut desktop_files = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "desktop")
        })
        .collect::<Vec<_>>();
    desktop_files.sort();

    for desktop_file in desktop_files {
        let Ok(metadata) = fs::symlink_metadata(&desktop_file) else {
            continue;
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.len() == 0
            || metadata.len() > MAX_DESKTOP_BYTES
        {
            continue;
        }
        let Ok(content) = fs::read_to_string(&desktop_file) else {
            continue;
        };
        let Some(exec_value) = content.lines().find_map(|line| line.strip_prefix("Exec=")) else {
            continue;
        };
        let Some(tokens) = shlex::split(exec_value) else {
            continue;
        };
        if tokens.is_empty() {
            continue;
        }
        return tokens
            .into_iter()
            .skip(1)
            .filter(|argument| !has_desktop_field_code(argument))
            .map(|argument| argument.replace("%%", "%"))
            .collect();
    }
    Vec::new()
}

fn launch_extracted(validated: &ValidatedAppImage, directory: &Path) -> Result<Child> {
    validate_extracted_directory(directory)?;
    let apprun = validate_apprun(directory)?;
    let desktop_arguments = extracted_desktop_arguments(directory);
    let no_sandbox_position = desktop_arguments
        .iter()
        .position(|argument| argument == "--no-sandbox");
    let mut launch_arguments = desktop_arguments
        .into_iter()
        .filter(|argument| argument != "--no-sandbox")
        .collect::<Vec<_>>();
    if no_sandbox_approved(directory) {
        let position = no_sandbox_position.unwrap_or(launch_arguments.len());
        launch_arguments.insert(
            position.min(launch_arguments.len()),
            "--no-sandbox".to_owned(),
        );
    }
    let mut command = Command::new(&apprun);
    command.args(launch_arguments);
    let parent = validated.path().parent().unwrap_or(directory);
    command
        .current_dir(directory)
        .env("APPIMAGE", validated.path())
        .env("APPDIR", directory)
        .env("OWD", parent)
        .env("ELECTRON_OZONE_PLATFORM_HINT", "wayland")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launch {}", apprun.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::symlink;

    fn mksquashfs_available() -> bool {
        Command::new("/usr/bin/mksquashfs")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn appimage_fixture(directory: &Path, name: &str) -> PathBuf {
        let root = directory.join(format!("{name}-root"));
        fs::create_dir(&root).unwrap();
        let apprun = root.join("AppRun");
        fs::write(
            &apprun,
            b"#!/bin/sh\nprintf '%s' \"$*\" > \"$OWD/launch-arguments\"\n",
        )
        .unwrap();
        fs::set_permissions(&apprun, fs::Permissions::from_mode(0o700)).unwrap();
        let squashfs = directory.join(format!("{name}.squashfs"));
        let status = Command::new("/usr/bin/mksquashfs")
            .arg(&root)
            .arg(&squashfs)
            .args([
                "-noappend",
                "-quiet",
                "-no-progress",
                "-no-xattrs",
                "-processors",
                "1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        let source = directory.join(format!("{name}.AppImage"));
        let mut file = fs::File::create(&source).unwrap();
        let mut prefix = vec![0_u8; 4096];
        prefix[..4].copy_from_slice(b"\x7fELF");
        prefix[4] = 2;
        prefix[5] = 1;
        prefix[6] = 1;
        prefix[8..11].copy_from_slice(b"AI\x02");
        prefix[16..18].copy_from_slice(&3_u16.to_le_bytes());
        prefix[18..20].copy_from_slice(&62_u16.to_le_bytes());
        prefix[20..24].copy_from_slice(&1_u32.to_le_bytes());
        prefix[52..54].copy_from_slice(&64_u16.to_le_bytes());
        file.write_all(&prefix).unwrap();
        file.write_all(&fs::read(squashfs).unwrap()).unwrap();
        source
    }

    #[test]
    fn persistent_extraction_is_source_adjacent_and_remembers_no_sandbox() {
        if !mksquashfs_available() {
            eprintln!("skipping persistent extraction fixture: mksquashfs is unavailable");
            return;
        }
        let temporary = tempfile::tempdir().unwrap();
        let source = appimage_fixture(temporary.path(), "Fixture");
        let mut child = extract_and_launch(&source, true).unwrap();
        assert!(child.wait().unwrap().success());

        let extracted = extracted_path(&source);
        assert!(extracted.is_dir());
        let marker = extracted.join(NO_SANDBOX_MARKER);
        let metadata = fs::symlink_metadata(&marker).unwrap();
        assert_eq!(metadata.len(), 0);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::read_to_string(temporary.path().join("launch-arguments")).unwrap(),
            "--no-sandbox"
        );

        fs::remove_file(temporary.path().join("launch-arguments")).unwrap();
        let mut child = launch_path(&source).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            fs::read_to_string(temporary.path().join("launch-arguments")).unwrap(),
            "--no-sandbox"
        );
    }

    #[test]
    fn normal_launch_reuses_extraction_and_applies_only_approved_fixed_arguments() {
        if !mksquashfs_available() {
            eprintln!("skipping persistent extraction fixture: mksquashfs is unavailable");
            return;
        }
        let temporary = tempfile::tempdir().unwrap();
        let source = appimage_fixture(temporary.path(), "Arguments");
        let mut child = extract_and_launch(&source, false).unwrap();
        assert!(child.wait().unwrap().success());
        let extracted = extracted_path(&source);
        fs::write(
            extracted.join("Arguments.desktop"),
            "[Desktop Entry]\nType=Application\nName=Arguments\nExec=fixture --profile \"Two Words\" %U --no-sandbox --literal=%%\n",
        )
        .unwrap();

        let mut child = launch_path(&source).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            fs::read_to_string(temporary.path().join("launch-arguments")).unwrap(),
            "--profile Two Words --literal=%"
        );

        let mut child = extract_and_launch(&source, true).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            fs::read_to_string(temporary.path().join("launch-arguments")).unwrap(),
            "--profile Two Words --no-sandbox --literal=%"
        );

        fs::set_permissions(
            extracted.join(NO_SANDBOX_MARKER),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let mut child = launch_path(&source).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            fs::read_to_string(temporary.path().join("launch-arguments")).unwrap(),
            "--profile Two Words --literal=%"
        );
    }

    #[test]
    fn normal_launch_rejects_a_symlink_at_the_persistent_extraction_path() {
        if !mksquashfs_available() {
            eprintln!("skipping persistent extraction fixture: mksquashfs is unavailable");
            return;
        }
        let temporary = tempfile::tempdir().unwrap();
        let source = appimage_fixture(temporary.path(), "Symlink");
        symlink(temporary.path().join("missing"), extracted_path(&source)).unwrap();
        assert!(launch_path(&source).is_err());
    }
}
