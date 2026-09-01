// SPDX-License-Identifier: AGPL-3.0-or-later

use std::env;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};

mod transport;

const REAL_SUDO: &str = "/usr/bin/sudo";

fn main() -> ExitCode {
    let arguments = env::args_os().collect::<Vec<_>>();
    let executable = arguments
        .first()
        .and_then(|argument| Path::new(argument).file_name())
        .unwrap_or_else(|| OsStr::new("sudo"));

    if executable == OsStr::new("buzzardos-sudo-exec") {
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

    let mode = match invocation_mode(executable) {
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
}
