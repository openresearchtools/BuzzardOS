// SPDX-License-Identifier: AGPL-3.0-or-later

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

mod transport;

const REAL_SUDO: &str = "/usr/bin/sudo";
const SUDOERS_DIRECTORY: &str = "/etc/sudoers.d";
const PASSWORDLESS_POLICY: &str = "/etc/sudoers.d/91-buzzardos-passwordless";
const PASSWORDLESS_POLICY_CONTENT: &[u8] = b"user ALL=(ALL:ALL) NOPASSWD: ALL\n";

fn main() -> ExitCode {
    let arguments = env::args_os().collect::<Vec<_>>();
    let invoked_as = arguments
        .first()
        .and_then(|argument| Path::new(argument).file_name())
        .unwrap_or_else(|| OsStr::new("sudo"));
    // The bridge is installed as three distinct executable files.  Resolve the
    // actual executable for the two privileged entry points instead of
    // trusting argv[0], which process launchers are allowed to replace.
    let installed_as = env::current_exe()
        .ok()
        .and_then(|path| path.file_name().map(OsStr::to_os_string));

    if installed_as.as_deref() == Some(OsStr::new("buzzardos-sudo-exec")) {
        if arguments
            .get(1)
            .is_some_and(|argument| argument == "--serve")
            && arguments.len() == 2
        {
            return match transport::serve() {
                Ok(()) => ExitCode::SUCCESS,
                Err(_) => ExitCode::from(126),
            };
        }
        return ExitCode::from(126);
    }
    if installed_as.as_deref() == Some(OsStr::new("sudo-policy")) {
        return match run_sudo_policy(&arguments[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("buzzardos sudo policy failed: {error}");
                ExitCode::from(1)
            }
        };
    }

    let mode = match invocation_mode(invoked_as) {
        Ok(mode) => mode,
        Err(error) => {
            eprintln!("buzzardos sudo handoff failed: {error}");
            return ExitCode::from(126);
        }
    };
    if unsafe { libc::getuid() } == 0 {
        let error = exec_real_sudo(mode, &arguments[1..]);
        eprintln!("buzzardos sudo handoff failed: {error}");
        return ExitCode::from(126);
    }
    match transport::run_client(mode, &arguments[1..]) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("buzzardos sudo handoff failed: {error}");
            ExitCode::from(126)
        }
    }
}

fn invocation_mode(executable: &OsStr) -> io::Result<&'static str> {
    match executable.as_bytes() {
        b"sudo" | b"buzzardos-sudo" => Ok("sudo"),
        b"sudoedit" => Ok("sudoedit"),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the sudo bridge must be invoked as sudo or sudoedit",
        )),
    }
}

fn exec_real_sudo(mode: &str, arguments: &[OsString]) -> io::Error {
    Command::new(REAL_SUDO).arg0(mode).args(arguments).exec()
}

fn run_sudo_policy(arguments: &[OsString]) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the policy helper must run as guest root",
        ));
    }
    if arguments.len() != 1 {
        return Err(invalid_policy_request());
    }
    match arguments[0].as_bytes() {
        b"enable-passwordless" => enable_passwordless_sudo(),
        b"disable-passwordless" => disable_passwordless_sudo(),
        b"status-passwordless" => {
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
        _ => Err(invalid_policy_request()),
    }
}

fn invalid_policy_request() -> io::Error {
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

fn enable_passwordless_sudo() -> io::Result<()> {
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
            .args(["-c", "-f"])
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

fn disable_passwordless_sudo() -> io::Result<()> {
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
    fn recognizes_only_sudo_and_sudoedit_names() {
        assert_eq!(invocation_mode(OsStr::new("sudo")).unwrap(), "sudo");
        assert_eq!(
            invocation_mode(OsStr::new("buzzardos-sudo")).unwrap(),
            "sudo"
        );
        assert_eq!(invocation_mode(OsStr::new("sudoedit")).unwrap(), "sudoedit");
        assert!(invocation_mode(OsStr::new("su")).is_err());
    }

    #[test]
    fn passwordless_policy_is_exactly_scoped_to_the_interactive_user() {
        assert_eq!(
            PASSWORDLESS_POLICY_CONTENT,
            b"user ALL=(ALL:ALL) NOPASSWD: ALL\n"
        );
    }
}
