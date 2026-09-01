// SPDX-License-Identifier: AGPL-3.0-or-later

use std::env;
use std::ffi::{CString, OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{self, Command, ExitCode};

const INTERACTIVE_UID: u32 = 1000;
const INTERACTIVE_GID: u32 = 1000;
const INTERACTIVE_USER: &[u8] = b"buzzard\0";
const REAL_SUDO: &str = "/usr/bin/sudo";
const SYSTEMD_RUN: &str = "/usr/bin/systemd-run";
const EXECUTOR: &str = "/usr/lib/buzzardos/runtime/current/libexec/buzzardos-sudo-exec";

fn main() -> ExitCode {
    let arguments = env::args_os().collect::<Vec<_>>();
    let executable = arguments
        .first()
        .and_then(|argument| Path::new(argument).file_name())
        .unwrap_or_else(|| OsStr::new("sudo"));

    if executable == OsStr::new("buzzardos-sudo-exec") {
        if let Err(error) = run_executor(&arguments[1..]) {
            eprintln!("buzzardos sudo handoff failed: {error}");
            return ExitCode::from(126);
        }
        unreachable!("successful execve does not return");
    }

    match run_client(executable, &arguments[1..]) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("buzzardos sudo handoff failed: {error}");
            ExitCode::from(126)
        }
    }
}

fn run_client(executable: &OsStr, arguments: &[OsString]) -> io::Result<u8> {
    let mode = invocation_mode(executable)?;
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        return exec_real_sudo(mode, arguments);
    }
    if uid != INTERACTIVE_UID {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "only the Buzzard OS interactive user may request guest root",
        ));
    }

    let pid = process::id();
    let start_time = process_start_time(pid)?;
    let mask = current_umask();
    let mut command = build_systemd_command(pid, start_time, mask, mode, arguments);
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() == 1 {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "sudo caller exited before the handoff started",
                ));
            }
            Ok(())
        });
    }
    let status = command.status()?;
    Ok(exit_status_code(status))
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

fn build_systemd_command(
    pid: u32,
    start_time: u64,
    mask: u32,
    mode: &str,
    arguments: &[OsString],
) -> Command {
    let mut command = Command::new(SYSTEMD_RUN);
    command.args([
        "--system",
        "--quiet",
        "--wait",
        "--collect",
        "--service-type=exec",
        "--uid=0",
        "--gid=0",
        "--same-dir",
        "--pipe",
        "--pty",
        "--send-sighup",
        "--expand-environment=no",
    ]);
    command.arg(format!("--property=UMask={mask:04o}"));
    command.args([
        "--",
        EXECUTOR,
        &pid.to_string(),
        &start_time.to_string(),
        mode,
        "--",
    ]);
    command.args(arguments);
    command
}

fn run_executor(arguments: &[OsString]) -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "executor did not start as guest root",
        ));
    }
    if arguments.len() < 4 || arguments[3] != OsStr::new("--") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid executor invocation",
        ));
    }

    let pid = parse_decimal_u32(&arguments[0], "caller PID")?;
    let expected_start_time = parse_decimal_u64(&arguments[1], "caller start time")?;
    let mode = arguments[2]
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid sudo mode"))?;
    if mode != "sudo" && mode != "sudoedit" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid sudo mode",
        ));
    }

    validate_caller(pid, expected_start_time)?;
    let environment = read_environment(pid)?;
    let sudo_arguments = &arguments[4..];

    if unsafe {
        libc::initgroups(
            INTERACTIVE_USER.as_ptr().cast(),
            INTERACTIVE_GID as libc::gid_t,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe {
        libc::setresgid(
            INTERACTIVE_GID as libc::gid_t,
            INTERACTIVE_GID as libc::gid_t,
            INTERACTIVE_GID as libc::gid_t,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::setresuid(INTERACTIVE_UID as libc::uid_t, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }

    exec_sudo(mode, sudo_arguments, &environment)
}

fn validate_caller(pid: u32, expected_start_time: u64) -> io::Result<()> {
    if process_start_time(pid)? != expected_start_time {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sudo caller identity changed before the handoff completed",
        ));
    }
    let status = fs::read(format!("/proc/{pid}/status"))?;
    let uid_line = status
        .split(|byte| *byte == b'\n')
        .find(|line| line.starts_with(b"Uid:"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "caller has no UID record"))?;
    let uids = std::str::from_utf8(uid_line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid caller UID record"))?
        .split_ascii_whitespace()
        .skip(1)
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid caller UID value"))?;
    if uids != [INTERACTIVE_UID; 4] {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sudo caller is not the interactive guest user",
        ));
    }
    Ok(())
}

fn process_start_time(pid: u32) -> io::Result<u64> {
    let stat = fs::read(format!("/proc/{pid}/stat"))?;
    parse_process_start_time(&stat)
}

fn parse_process_start_time(stat: &[u8]) -> io::Result<u64> {
    let close = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process stat record"))?;
    let tail = std::str::from_utf8(stat.get(close + 1..).unwrap_or_default())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid process stat text"))?;
    // The tail begins with field 3 (state); starttime is field 22.
    tail.split_ascii_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process start time"))?
        .parse::<u64>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid process start time"))
}

fn read_environment(pid: u32) -> io::Result<Vec<CString>> {
    let bytes = fs::read(format!("/proc/{pid}/environ"))?;
    bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            CString::new(entry).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "caller environment contains NUL",
                )
            })
        })
        .collect()
}

fn exec_real_sudo(mode: &str, arguments: &[OsString]) -> io::Result<u8> {
    let mut command = Command::new(REAL_SUDO);
    command.arg0(mode).args(arguments);
    let error = command.exec();
    Err(error)
}

fn exec_sudo(mode: &str, arguments: &[OsString], environment: &[CString]) -> io::Result<()> {
    let path = CString::new(REAL_SUDO).expect("static sudo path has no NUL");
    let mut argv = Vec::with_capacity(arguments.len() + 1);
    argv.push(CString::new(mode).expect("static invocation name has no NUL"));
    for argument in arguments {
        argv.push(CString::new(argument.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "sudo argument contains NUL")
        })?);
    }
    let mut argv_pointers = argv
        .iter()
        .map(|argument| argument.as_ptr())
        .collect::<Vec<_>>();
    argv_pointers.push(std::ptr::null());
    let mut environment_pointers = environment
        .iter()
        .map(|entry| entry.as_ptr())
        .collect::<Vec<_>>();
    environment_pointers.push(std::ptr::null());

    unsafe {
        libc::execve(
            path.as_ptr(),
            argv_pointers.as_ptr(),
            environment_pointers.as_ptr(),
        );
    }
    Err(io::Error::last_os_error())
}

fn parse_decimal_u32(value: &OsStr, label: &str) -> io::Result<u32> {
    value
        .to_str()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid {label}")))
}

fn parse_decimal_u64(value: &OsStr, label: &str) -> io::Result<u64> {
    value
        .to_str()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid {label}")))
}

fn current_umask() -> u32 {
    let mask = unsafe { libc::umask(0) };
    unsafe {
        libc::umask(mask);
    }
    mask as u32
}

fn exit_status_code(status: std::process::ExitStatus) -> u8 {
    if let Some(code) = status.code() {
        return u8::try_from(code).unwrap_or(255);
    }
    status
        .signal()
        .and_then(|signal| u8::try_from(128 + signal).ok())
        .unwrap_or(255)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn parses_start_time_when_process_name_contains_spaces_and_parentheses() {
        let mut fields = vec!["S".to_owned()];
        fields.extend((4..=21).map(|field| field.to_string()));
        fields.push("987654".to_owned());
        fields.push("23".to_owned());
        let stat = format!("42 (sudo worker ) test) {}\n", fields.join(" "));
        assert_eq!(parse_process_start_time(stat.as_bytes()).unwrap(), 987654);
    }

    #[test]
    fn preserves_sudo_arguments_as_distinct_opaque_arguments() {
        let arguments = vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("printf '%s\\n' \"$HOME with spaces\""),
        ];
        let command = build_systemd_command(42, 987654, 0o027, "sudo", &arguments);
        let actual = command.get_args().map(OsStr::to_owned).collect::<Vec<_>>();
        assert!(actual.contains(&OsString::from("--expand-environment=no")));
        assert!(actual.contains(&OsString::from("--pipe")));
        assert!(actual.contains(&OsString::from("--pty")));
        assert!(actual.contains(&OsString::from("--property=UMask=0027")));
        assert_eq!(&actual[actual.len() - arguments.len()..], arguments);
    }

    #[test]
    fn recognizes_sudo_and_sudoedit_without_parsing_their_options() {
        assert_eq!(invocation_mode(OsStr::new("sudo")).unwrap(), "sudo");
        assert_eq!(invocation_mode(OsStr::new("sudoedit")).unwrap(), "sudoedit");
        assert!(invocation_mode(OsStr::new("su")).is_err());
    }

    #[test]
    fn splits_proc_environment_without_interpreting_values() {
        let environment = b"TERM=xterm-256color\0VALUE=line one\nline two=three\0";
        let parsed = environment
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
            .map(|entry| CString::new(entry).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(parsed[0].as_bytes(), b"TERM=xterm-256color");
        assert_eq!(parsed[1].as_bytes(), b"VALUE=line one\nline two=three");
    }

    #[test]
    fn command_argv_zero_selects_the_real_sudo_mode() {
        let mut command = Command::new(REAL_SUDO);
        command.arg0("sudoedit").arg("/etc/hosts");
        assert_eq!(command.get_program(), OsStr::new(REAL_SUDO));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("/etc/hosts")]
        );
    }
}
