// SPDX-License-Identifier: AGPL-3.0-or-later

//! Numbered, daemonless Buzzard CUA invocation identity.
//!
//! `cua` and `cua1` share seat/workspace 1. `cuaN` uses seat/workspace N.
//! The private lock serializes physical actions only within that numbered
//! seat; different numbered CUA invocations remain independent.

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const CUA_INDEX_ENV: &str = "BUZZARDOS_CUA_INDEX";
pub const CUA_SEAT_ENV: &str = "BUZZARDOS_CUA_SEAT";
pub const CUA_WORKSPACE_ENV: &str = "BUZZARDOS_CUA_WORKSPACE";
pub const CUA_OUTPUT_ENV: &str = "BUZZARDOS_CUA_OUTPUT";

pub struct SeatContext {
    _lock: File,
}

pub fn invocation_index(argv0: &std::ffi::OsStr) -> Result<u32> {
    let name = Path::new(argv0)
        .file_name()
        .and_then(|name| name.to_str())
        .context("CUA executable name is not valid UTF-8")?;
    match name {
        "cua" | "cua1" | "buzzardoscua" => Ok(1),
        _ => {
            let suffix = name
                .strip_prefix("cua")
                .filter(|suffix| !suffix.is_empty())
                .with_context(|| format!("unsupported CUA executable identity {name}"))?;
            let index = suffix
                .parse::<u32>()
                .with_context(|| format!("invalid CUA seat number in {name}"))?;
            anyhow::ensure!(index >= 2, "cua0 is reserved for the human Desktop");
            Ok(index)
        }
    }
}

fn runtime_root() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is unavailable")?;
    let root = PathBuf::from(runtime).join("buzzardoscua");
    match fs::create_dir(&root) {
        Ok(()) => fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("creating private Buzzard CUA runtime directory"),
    }
    let metadata = fs::symlink_metadata(&root)?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "Buzzard CUA runtime directory is not private to the guest user"
    );
    Ok(root)
}

#[cfg_attr(test, allow(dead_code))]
fn state_name(kind: &str) -> Result<String> {
    anyhow::ensure!(
        !kind.is_empty()
            && kind.chars().all(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
            }),
        "invalid Buzzard CUA runtime-state name"
    );
    Ok(format!("seat{}-{kind}.json", current_index()))
}

#[cfg_attr(test, allow(dead_code))]
pub fn read_state(kind: &str, maximum_bytes: usize) -> Result<Option<Vec<u8>>> {
    let path = runtime_root()?.join(state_name(kind)?);
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    };
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.len() <= maximum_bytes as u64,
        "Buzzard CUA runtime state is unsafe or exceeds its byte limit"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::take(&mut file, maximum_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= maximum_bytes,
        "Buzzard CUA runtime state exceeds its byte limit"
    );
    Ok(Some(bytes))
}

#[cfg_attr(test, allow(dead_code))]
pub fn write_state(kind: &str, bytes: &[u8], maximum_bytes: usize) -> Result<()> {
    anyhow::ensure!(
        bytes.len() <= maximum_bytes,
        "Buzzard CUA runtime state exceeds its byte limit"
    );
    let root = runtime_root()?;
    let name = state_name(kind)?;
    let destination = root.join(&name);
    let temporary = root.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &destination)?;
        File::open(&root)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn open_lock(index: u32, nonblocking: bool) -> Result<File> {
    let path = runtime_root()?.join(format!("seat{index}.lock"));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "Buzzard CUA seat lock is not private to the guest user"
    );
    // SAFETY: `file` owns a valid descriptor for the duration of the call.
    let operation = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
    if unsafe { libc::flock(file.as_raw_fd(), operation) } != 0 {
        return Err(std::io::Error::last_os_error()).context("locking numbered CUA seat");
    }
    Ok(file)
}

pub fn prepare(index: u32) -> Result<SeatContext> {
    anyhow::ensure!(index > 0, "seat0 is reserved for the human Desktop");
    let lock = open_lock(index, false)?;
    let workspace = crate::platform::wayland::sway_ipc::cua_workspace_name(index)?;
    let output = crate::platform::wayland::sway_ipc::ensure_cua_workspace(index)?;
    let seat = format!("seat{index}");
    std::env::set_var(CUA_INDEX_ENV, index.to_string());
    std::env::set_var(CUA_SEAT_ENV, &seat);
    std::env::set_var(CUA_WORKSPACE_ENV, &workspace);
    std::env::set_var(CUA_OUTPUT_ENV, &output);
    Ok(SeatContext { _lock: lock })
}

pub fn try_lock_other(index: u32) -> Result<Option<File>> {
    if index == 0 || index == current_index() {
        return Ok(None);
    }
    open_lock(index, true)
        .map(Some)
        .with_context(|| format!("CUA{index} is busy with another operation"))
}

pub fn current_index() -> u32 {
    std::env::var(CUA_INDEX_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multicall_names_map_to_numbered_seats() {
        assert_eq!(invocation_index("cua".as_ref()).unwrap(), 1);
        assert_eq!(invocation_index("cua1".as_ref()).unwrap(), 1);
        assert_eq!(invocation_index("cua2".as_ref()).unwrap(), 2);
        assert!(invocation_index("cua0".as_ref()).is_err());
    }
}
