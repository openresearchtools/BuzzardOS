// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Descriptor installed by Wild Buzzard's AppImage runtime.
///
/// The descriptor is one end of a runtime-owned pipe. Keeping it open leases
/// the mounted or temporarily extracted AppDir; closing the final copy lets the
/// runtime unmount or remove that AppDir. It is intentionally an implementation
/// detail rather than part of machine metadata.
pub const APPIMAGE_LEASE_FD_ENV: &str = "WILDBUZZARD_APPIMAGE_LEASE_FD";

#[derive(Debug)]
pub struct AppImageRuntimeLease {
    descriptor: OwnedFd,
}

impl AppImageRuntimeLease {
    /// Take ownership of a runtime lease inherited by this process.
    ///
    /// This must run before application threads are created. The environment
    /// entry is consumed so an unrelated child cannot observe a stale fd
    /// number, and close-on-exec prevents accidental inheritance. A deliberate
    /// broker handoff clears close-on-exec only in that broker's child process.
    pub fn capture() -> Result<Option<Self>> {
        let Some(value) = std::env::var_os(APPIMAGE_LEASE_FD_ENV) else {
            return Ok(None);
        };
        // SAFETY: launcher and broker call this at single-threaded process
        // startup, before any worker threads can concurrently access the
        // environment.
        unsafe { std::env::remove_var(APPIMAGE_LEASE_FD_ENV) };

        let value = value
            .into_string()
            .map_err(|_| anyhow::anyhow!("AppImage runtime lease fd is not UTF-8"))?;
        let descriptor: RawFd = value
            .parse()
            .context("AppImage runtime lease fd is not an integer")?;
        if descriptor < 3 {
            bail!("AppImage runtime lease fd must not alias standard input/output");
        }

        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `metadata` points to writable storage for one libc::stat.
        if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("inspecting inherited AppImage runtime lease fd");
        }
        // SAFETY: fstat succeeded and initialized the value.
        let metadata = unsafe { metadata.assume_init() };
        if metadata.st_mode & libc::S_IFMT != libc::S_IFIFO {
            bail!("AppImage runtime lease fd is not a pipe");
        }

        set_close_on_exec(descriptor, true)
            .context("protecting inherited AppImage runtime lease fd")?;
        // SAFETY: the runtime transferred this valid descriptor to the process,
        // and this function is its single ownership boundary.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        Ok(Some(Self { descriptor }))
    }

    /// Transfer the lease to one explicitly selected broker exec.
    pub fn pass_to(&self, command: &mut Command) {
        let descriptor = self.descriptor.as_raw_fd();
        command.env(APPIMAGE_LEASE_FD_ENV, descriptor.to_string());
        // SAFETY: this closure calls only async-signal-safe fcntl operations in
        // the post-fork child. The parent descriptor remains close-on-exec.
        unsafe {
            command.pre_exec(move || set_close_on_exec(descriptor, false));
        }
    }
}

fn set_close_on_exec(descriptor: RawFd, enabled: bool) -> std::io::Result<()> {
    // SAFETY: fcntl does not outlive or alias Rust memory.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    // SAFETY: same valid descriptor as above and an integer flag value.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::process::Stdio;

    #[test]
    fn lease_is_inherited_only_by_an_explicit_command() {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors points to storage for two new fds.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: pipe returned two new owned descriptors.
        let read_end = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
        // SAFETY: pipe returned two new owned descriptors.
        let _write_end = unsafe { OwnedFd::from_raw_fd(descriptors[1]) };
        set_close_on_exec(read_end.as_raw_fd(), true).unwrap();
        let lease = AppImageRuntimeLease {
            descriptor: read_end,
        };

        let descriptor = lease.descriptor.as_raw_fd();
        let mut ordinary = Command::new("/bin/sh");
        let ordinary_status = ordinary
            .arg("-c")
            .arg(format!("test -e /proc/self/fd/{descriptor}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!ordinary_status.success());

        let mut selected = Command::new("/bin/sh");
        selected
            .arg("-c")
            .arg(format!("test -p /proc/self/fd/${APPIMAGE_LEASE_FD_ENV}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        lease.pass_to(&mut selected);
        assert!(selected.status().unwrap().success());
    }
}
