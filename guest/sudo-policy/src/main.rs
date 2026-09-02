// SPDX-License-Identifier: AGPL-3.0-or-later

use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

const SUDOERS_DIRECTORY: &str = "/etc/sudoers.d";
const PASSWORDLESS_POLICY: &str = "/etc/sudoers.d/91-buzzardos-passwordless";
const PASSWORDLESS_POLICY_CONTENT: &[u8] = b"user ALL=(ALL:ALL) NOPASSWD: ALL\n";

fn main() -> ExitCode {
    match run(env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Buzzard OS sudo policy: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(mut arguments: impl Iterator<Item = std::ffi::OsString>) -> io::Result<()> {
    let action = arguments.next().ok_or_else(invalid_request)?;
    if arguments.next().is_some() {
        return Err(invalid_request());
    }
    let action = match action.as_os_str().as_bytes() {
        b"enable-passwordless" => Action::Enable,
        b"disable-passwordless" => Action::Disable,
        b"status-passwordless" => Action::Status,
        _ => return Err(invalid_request()),
    };
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "this utility must be invoked through the guest's native sudo",
        ));
    }
    match action {
        Action::Enable => enable_passwordless(),
        Action::Disable => disable_passwordless(),
        Action::Status => {
            validate_sudoers_directory()?;
            println!(
                "{}",
                if validate_existing_policy(Path::new(PASSWORDLESS_POLICY))? {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            Ok(())
        }
    }
}

enum Action {
    Enable,
    Disable,
    Status,
}

fn invalid_request() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "expected enable-passwordless, disable-passwordless, or status-passwordless",
    )
}

fn validate_sudoers_directory() -> io::Result<()> {
    for path in [Path::new("/etc"), Path::new(SUDOERS_DIRECTORY)] {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != 0
            || metadata.gid() != 0
            || metadata.mode() & 0o022 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is not a trusted root-owned directory", path.display()),
            ));
        }
    }
    Ok(())
}

fn validate_existing_policy(path: &Path) -> io::Result<bool> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.gid() != 0
        || metadata.mode() & 0o777 != 0o440
        || metadata.len() != PASSWORDLESS_POLICY_CONTENT.len() as u64
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the existing passwordless-sudo policy is not the exact Buzzard OS policy",
        ));
    }
    let mut contents = Vec::with_capacity(PASSWORDLESS_POLICY_CONTENT.len());
    file.read_to_end(&mut contents)?;
    if contents != PASSWORDLESS_POLICY_CONTENT {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the existing passwordless-sudo policy has unexpected contents",
        ));
    }
    Ok(true)
}

fn enable_passwordless() -> io::Result<()> {
    validate_sudoers_directory()?;
    let policy = Path::new(PASSWORDLESS_POLICY);
    if validate_existing_policy(policy)? {
        return Ok(());
    }
    let temporary = format!("{PASSWORDLESS_POLICY}.tmp.{}", std::process::id());
    let temporary = Path::new(&temporary);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o440)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(temporary)?;
        file.write_all(PASSWORDLESS_POLICY_CONTENT)?;
        file.set_permissions(fs::Permissions::from_mode(0o440))?;
        file.sync_all()?;
        let status = Command::new("/usr/sbin/visudo")
            .args([OsStr::new("-c"), OsStr::new("-f")])
            .arg(temporary)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()?;
        if !status.success() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "visudo rejected the generated policy",
            ));
        }
        fs::rename(temporary, policy)?;
        File::open(SUDOERS_DIRECTORY)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn disable_passwordless() -> io::Result<()> {
    validate_sudoers_directory()?;
    let policy = Path::new(PASSWORDLESS_POLICY);
    if !validate_existing_policy(policy)? {
        return Ok(());
    }
    fs::remove_file(policy)?;
    File::open(SUDOERS_DIRECTORY)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_is_exactly_scoped_to_the_interactive_user() {
        assert_eq!(
            PASSWORDLESS_POLICY_CONTENT,
            b"user ALL=(ALL:ALL) NOPASSWD: ALL\n"
        );
    }

    #[test]
    fn rejects_unknown_actions_before_touching_policy() {
        let error = run([std::ffi::OsString::from("unknown")].into_iter()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
