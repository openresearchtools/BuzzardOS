// SPDX-License-Identifier: AGPL-3.0-or-later

use std::env;
use std::ffi::{CString, OsStr, OsString};
use std::io::{self, Read, Write};
use std::mem::{self, MaybeUninit};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};

const SOCKET_PATH: &str = "/run/buzzardos/sudo.sock";
const MAGIC: &[u8; 8] = b"BZSDO001";
const INTERACTIVE_UID: u32 = 1000;
const INTERACTIVE_GID: u32 = 1000;
const INTERACTIVE_USER: &[u8] = b"user\0";
const REAL_SUDO: &[u8] = b"/usr/bin/sudo\0";
const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_ITEMS: usize = 16_384;
const MAX_ITEM_BYTES: usize = 1024 * 1024;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const REQUIRED_FDS: usize = 4;
const REQUIRED_FDS_WITH_TTY: usize = 5;

const FRAME_READY: u8 = 1;
const FRAME_INPUT: u8 = 2;
const FRAME_OUTPUT: u8 = 3;
const FRAME_SIGNAL: u8 = 4;
const FRAME_RESIZE: u8 = 5;
const FRAME_STOPPED: u8 = 6;
const FRAME_STATUS: u8 = 7;
const FRAME_ERROR: u8 = 8;

static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

#[derive(Debug)]
struct Request {
    mode: &'static str,
    umask: u32,
    tty_mask: u8,
    has_tty: bool,
    arguments: Vec<OsString>,
    environment: Vec<OsString>,
    descriptors: Vec<OwnedFd>,
}

struct TerminalGuard {
    descriptor: RawFd,
    original: libc::termios,
    raw: bool,
}

impl TerminalGuard {
    fn new(descriptor: RawFd) -> io::Result<Self> {
        let mut original = MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(descriptor, original.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            descriptor,
            original: unsafe { original.assume_init() },
            raw: false,
        })
    }

    fn enter_raw(&mut self) -> io::Result<()> {
        if self.raw {
            return Ok(());
        }
        let mut raw = self.original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(self.descriptor, libc::TCSADRAIN, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.raw = true;
        Ok(())
    }

    fn restore(&mut self) {
        if self.raw {
            unsafe {
                libc::tcsetattr(self.descriptor, libc::TCSADRAIN, &self.original);
            }
            self.raw = false;
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

pub(crate) fn run_client(mode: &'static str, arguments: &[OsString]) -> io::Result<u8> {
    let mut connection = UnixStream::connect(SOCKET_PATH)?;
    let descriptors = client_descriptors()?;
    let has_tty = descriptors.len() == REQUIRED_FDS_WITH_TTY;
    let tty_mask = if has_tty {
        matching_terminal_mask(&descriptors[..3], descriptors[4].as_raw_fd())?
    } else {
        0
    };
    let request = encode_request(
        mode,
        current_umask(),
        tty_mask,
        has_tty,
        arguments,
        env::vars_os().map(|(name, value)| {
            let mut entry = name;
            entry.push("=");
            entry.push(value);
            entry
        }),
    )?;
    let raw_descriptors = descriptors
        .iter()
        .map(AsRawFd::as_raw_fd)
        .collect::<Vec<_>>();
    send_with_fds(&mut connection, &request, &raw_descriptors)?;

    let signal_pipe = signal_pipe()?;
    install_signal_handlers(signal_pipe.1.as_raw_fd())?;
    let tty_fd = has_tty.then(|| descriptors[4].as_raw_fd());
    let mut terminal = tty_fd.map(TerminalGuard::new).transpose()?;
    let result = client_relay(&mut connection, tty_fd, terminal.as_mut(), &signal_pipe.0);
    SIGNAL_WRITE_FD.store(-1, Ordering::SeqCst);
    result
}

pub(crate) fn serve() -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sudo service did not start as guest root",
        ));
    }
    let mut connection = unsafe { UnixStream::from_raw_fd(duplicate_fd(0)?) };
    let result = (|| {
        validate_peer(connection.as_raw_fd())?;
        let request = receive_request(&mut connection)?;
        run_server_request(connection.try_clone()?, request)
    })();
    if let Err(error) = &result {
        let _ = send_frame(&mut connection, FRAME_ERROR, error.to_string().as_bytes());
    }
    result
}

fn encode_request<I>(
    mode: &'static str,
    umask: u32,
    tty_mask: u8,
    has_tty: bool,
    arguments: &[OsString],
    environment: I,
) -> io::Result<Vec<u8>>
where
    I: IntoIterator<Item = OsString>,
{
    if arguments.len() > MAX_ITEMS {
        return Err(invalid("too many sudo arguments"));
    }
    let environment = environment.into_iter().collect::<Vec<_>>();
    if environment.len() > MAX_ITEMS {
        return Err(invalid("too many environment entries"));
    }
    let mut body = Vec::new();
    body.extend_from_slice(MAGIC);
    body.push(match mode {
        "sudo" => 0,
        "sudoedit" => 1,
        _ => return Err(invalid("invalid sudo invocation mode")),
    });
    body.push(tty_mask & 0b111);
    body.push(u8::from(has_tty));
    body.push(0);
    body.extend_from_slice(&umask.to_be_bytes());
    body.extend_from_slice(&(arguments.len() as u32).to_be_bytes());
    body.extend_from_slice(&(environment.len() as u32).to_be_bytes());
    for value in arguments.iter().chain(environment.iter()) {
        let bytes = value.as_os_str().as_bytes();
        if bytes.len() > MAX_ITEM_BYTES || bytes.contains(&0) {
            return Err(invalid("sudo request item is invalid or too long"));
        }
        body.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        body.extend_from_slice(bytes);
        if body.len() > MAX_REQUEST_BYTES {
            return Err(invalid("sudo request exceeds the protocol limit"));
        }
    }
    let mut request = Vec::with_capacity(body.len() + 4);
    request.extend_from_slice(&(body.len() as u32).to_be_bytes());
    request.extend_from_slice(&body);
    Ok(request)
}

fn decode_request(payload: &[u8], descriptors: Vec<OwnedFd>) -> io::Result<Request> {
    if payload.len() < 24 || &payload[..8] != MAGIC {
        return Err(invalid("invalid sudo protocol header"));
    }
    let mode = match payload[8] {
        0 => "sudo",
        1 => "sudoedit",
        _ => return Err(invalid("invalid sudo protocol mode")),
    };
    let tty_mask = payload[9];
    let has_tty = match payload[10] {
        0 => false,
        1 => true,
        _ => return Err(invalid("invalid sudo terminal-presence metadata")),
    };
    if tty_mask & !0b111 != 0 || payload[11] != 0 || (!has_tty && tty_mask != 0) {
        return Err(invalid("invalid sudo terminal metadata"));
    }
    let umask = read_u32(&payload[12..16]);
    if umask & !0o777 != 0 {
        return Err(invalid("invalid sudo umask"));
    }
    let argument_count = read_u32(&payload[16..20]) as usize;
    let environment_count = read_u32(&payload[20..24]) as usize;
    if argument_count > MAX_ITEMS || environment_count > MAX_ITEMS {
        return Err(invalid("sudo request has too many items"));
    }
    let expected_fds = if has_tty {
        REQUIRED_FDS_WITH_TTY
    } else {
        REQUIRED_FDS
    };
    if descriptors.len() != expected_fds {
        return Err(invalid("sudo request has the wrong descriptor count"));
    }
    validate_descriptors(&descriptors, tty_mask, has_tty)?;
    let mut offset = 24;
    let mut values = Vec::with_capacity(argument_count + environment_count);
    for _index in 0..argument_count + environment_count {
        if offset + 4 > payload.len() {
            return Err(invalid("truncated sudo request item"));
        }
        let length = read_u32(&payload[offset..offset + 4]) as usize;
        offset += 4;
        if length > MAX_ITEM_BYTES || offset + length > payload.len() {
            return Err(invalid("invalid sudo request item length"));
        }
        let value = &payload[offset..offset + length];
        if value.contains(&0) {
            return Err(invalid("sudo request item contains NUL"));
        }
        values.push(OsStr::from_bytes(value).to_owned());
        offset += length;
    }
    if offset != payload.len() {
        return Err(invalid("sudo request has trailing data"));
    }
    let environment = values.split_off(argument_count);
    validate_environment(&environment)?;
    Ok(Request {
        mode,
        umask,
        tty_mask,
        has_tty,
        arguments: values,
        environment,
        descriptors,
    })
}

fn receive_request(connection: &mut UnixStream) -> io::Result<Request> {
    let (mut bytes, descriptors) = receive_with_fds(connection)?;
    if bytes.len() < 4 {
        return Err(invalid("truncated sudo request length"));
    }
    let length = read_u32(&bytes[..4]) as usize;
    if length > MAX_REQUEST_BYTES {
        return Err(invalid("sudo request exceeds the protocol limit"));
    }
    while bytes.len() < length + 4 {
        let mut buffer = [0u8; 65_536];
        let wanted = (length + 4 - bytes.len()).min(buffer.len());
        let received = connection.read(&mut buffer[..wanted])?;
        if received == 0 {
            return Err(invalid("truncated sudo request"));
        }
        bytes.extend_from_slice(&buffer[..received]);
    }
    if bytes.len() != length + 4 {
        return Err(invalid("sudo request has trailing data"));
    }
    decode_request(&bytes[4..], descriptors)
}

fn client_descriptors() -> io::Result<Vec<OwnedFd>> {
    let mut result = vec![
        duplicate_or_null(0, libc::O_RDONLY)?,
        duplicate_or_null(1, libc::O_WRONLY)?,
        duplicate_or_null(2, libc::O_WRONLY)?,
        open_path(OsStr::new("."), libc::O_PATH | libc::O_DIRECTORY)?,
    ];
    let tty = open_path(OsStr::new("/dev/tty"), libc::O_RDWR | libc::O_NOCTTY);
    match tty {
        Ok(tty) => result.push(tty),
        Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {}
        Err(error) if error.raw_os_error() == Some(libc::ENODEV) => {}
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
        Err(error) => return Err(error),
    }
    Ok(result)
}

fn open_path(path: &OsStr, flags: i32) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_bytes()).map_err(|_| invalid("path contains NUL"))?;
    let descriptor = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn duplicate_or_null(descriptor: RawFd, access: i32) -> io::Result<OwnedFd> {
    match duplicate_fd(descriptor) {
        Ok(duplicated) => Ok(unsafe { OwnedFd::from_raw_fd(duplicated) }),
        Err(error) if error.raw_os_error() == Some(libc::EBADF) => {
            open_path(OsStr::new("/dev/null"), access)
        }
        Err(error) => Err(error),
    }
}

fn duplicate_fd(descriptor: RawFd) -> io::Result<RawFd> {
    let duplicated = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 3) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(duplicated)
}

fn matching_terminal_mask(descriptors: &[OwnedFd], tty: RawFd) -> io::Result<u8> {
    let tty_metadata = descriptor_metadata(tty)?;
    let mut mask = 0;
    for (index, descriptor) in descriptors.iter().enumerate() {
        let fd = descriptor.as_raw_fd();
        let metadata = descriptor_metadata(fd)?;
        if unsafe { libc::isatty(fd) } == 1
            && metadata.st_dev == tty_metadata.st_dev
            && metadata.st_rdev == tty_metadata.st_rdev
        {
            mask |= 1 << index;
        }
    }
    Ok(mask)
}

fn validate_descriptors(descriptors: &[OwnedFd], tty_mask: u8, has_tty: bool) -> io::Result<()> {
    let cwd = descriptor_metadata(descriptors[3].as_raw_fd())?;
    if cwd.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(invalid(
            "sudo working-directory descriptor is not a directory",
        ));
    }
    if has_tty {
        let tty = descriptors[4].as_raw_fd();
        if unsafe { libc::isatty(tty) } != 1 {
            return Err(invalid("sudo terminal descriptor is not a terminal"));
        }
        if matching_terminal_mask(&descriptors[..3], tty)? != tty_mask {
            return Err(invalid("sudo terminal descriptor metadata does not match"));
        }
    }
    Ok(())
}

fn descriptor_metadata(descriptor: RawFd) -> io::Result<libc::stat> {
    let mut metadata = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { metadata.assume_init() })
}

fn validate_environment(environment: &[OsString]) -> io::Result<()> {
    for entry in environment {
        let bytes = entry.as_os_str().as_bytes();
        let Some(separator) = bytes.iter().position(|byte| *byte == b'=') else {
            return Err(invalid("sudo environment entry has no name separator"));
        };
        if separator == 0 || bytes[..separator].contains(&b'=') {
            return Err(invalid("sudo environment entry has an invalid name"));
        }
    }
    Ok(())
}

fn validate_peer(socket: RawFd) -> io::Result<()> {
    let mut credentials = MaybeUninit::<libc::ucred>::zeroed();
    let mut length = mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            socket,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != mem::size_of::<libc::ucred>() {
        return Err(invalid("invalid sudo peer credentials"));
    }
    let credentials = unsafe { credentials.assume_init() };
    if credentials.pid < 2
        || credentials.uid != INTERACTIVE_UID
        || credentials.gid != INTERACTIVE_GID
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "sudo socket peer is not the interactive guest user",
        ));
    }
    Ok(())
}

fn send_with_fds(
    connection: &mut UnixStream,
    payload: &[u8],
    descriptors: &[RawFd],
) -> io::Result<()> {
    let mut control = vec![0u8; cmsg_space(mem::size_of_val(descriptors))];
    let mut iovec = libc::iovec {
        iov_base: payload.as_ptr().cast_mut().cast(),
        iov_len: payload.len(),
    };
    let mut message = unsafe { mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    let header = message.msg_control.cast::<libc::cmsghdr>();
    unsafe {
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = cmsg_len(mem::size_of_val(descriptors));
        std::ptr::copy_nonoverlapping(
            descriptors.as_ptr().cast::<u8>(),
            cmsg_data(header),
            mem::size_of_val(descriptors),
        );
    }
    let sent = unsafe { libc::sendmsg(connection.as_raw_fd(), &message, libc::MSG_NOSIGNAL) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    let sent = sent as usize;
    if sent == 0 {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "sudo request was not sent",
        ));
    }
    connection.write_all(&payload[sent..])
}

fn receive_with_fds(connection: &mut UnixStream) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut bytes = vec![0u8; 65_536];
    let mut control = vec![0u8; cmsg_space(REQUIRED_FDS_WITH_TTY * mem::size_of::<RawFd>())];
    let mut iovec = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut message = unsafe { mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iovec;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    let received =
        unsafe { libc::recvmsg(connection.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    if received == 0 {
        return Err(invalid("empty sudo request"));
    }
    if message.msg_flags & (libc::MSG_CTRUNC | libc::MSG_TRUNC) != 0 {
        return Err(invalid("truncated sudo request descriptors"));
    }
    bytes.truncate(received as usize);
    let header = message.msg_control.cast::<libc::cmsghdr>();
    if message.msg_controllen < mem::size_of::<libc::cmsghdr>()
        || unsafe { (*header).cmsg_level } != libc::SOL_SOCKET
        || unsafe { (*header).cmsg_type } != libc::SCM_RIGHTS
    {
        return Err(invalid("sudo request did not include descriptors"));
    }
    let length = unsafe { (*header).cmsg_len };
    let data_length = length
        .checked_sub(cmsg_align(mem::size_of::<libc::cmsghdr>()))
        .ok_or_else(|| invalid("invalid sudo descriptor message"))?;
    if data_length % mem::size_of::<RawFd>() != 0 {
        return Err(invalid("misaligned sudo descriptor message"));
    }
    let count = data_length / mem::size_of::<RawFd>();
    if !matches!(count, REQUIRED_FDS | REQUIRED_FDS_WITH_TTY) {
        return Err(invalid("sudo request has an invalid descriptor count"));
    }
    let raw = unsafe { std::slice::from_raw_parts(cmsg_data(header).cast::<RawFd>(), count) };
    let descriptors = raw
        .iter()
        .map(|descriptor| unsafe { OwnedFd::from_raw_fd(*descriptor) })
        .collect();
    Ok((bytes, descriptors))
}

fn cmsg_align(length: usize) -> usize {
    let alignment = mem::size_of::<usize>();
    (length + alignment - 1) & !(alignment - 1)
}

fn cmsg_len(data_length: usize) -> usize {
    cmsg_align(mem::size_of::<libc::cmsghdr>()) + data_length
}

fn cmsg_space(data_length: usize) -> usize {
    cmsg_align(mem::size_of::<libc::cmsghdr>()) + cmsg_align(data_length)
}

unsafe fn cmsg_data(header: *mut libc::cmsghdr) -> *mut u8 {
    unsafe {
        header
            .cast::<u8>()
            .add(cmsg_align(mem::size_of::<libc::cmsghdr>()))
    }
}

fn run_server_request(mut connection: UnixStream, request: Request) -> io::Result<()> {
    let tty = request.has_tty.then(|| request.descriptors[4].as_raw_fd());
    let mut master = None;
    let mut slave = None;
    if let Some(tty) = tty {
        let (new_master, new_slave) = open_pty(tty)?;
        master = Some(new_master);
        slave = Some(new_slave);
    }
    send_frame(&mut connection, FRAME_READY, &[])?;

    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(io::Error::last_os_error());
    }
    if child == 0 {
        let result = child_exec(&request, master.as_ref(), slave.as_ref());
        let message = format!("buzzardos sudo handoff failed: {result}\n");
        let output = if request.tty_mask != 0 {
            slave.as_ref().map(AsRawFd::as_raw_fd).unwrap_or(2)
        } else {
            request.descriptors[2].as_raw_fd()
        };
        unsafe {
            libc::write(output, message.as_ptr().cast(), message.len());
            libc::_exit(126);
        }
    }
    drop(slave);
    relay_server(
        &mut connection,
        child,
        master.as_ref().map(AsRawFd::as_raw_fd),
    )
}

fn child_exec(request: &Request, master: Option<&OwnedFd>, slave: Option<&OwnedFd>) -> io::Error {
    if let Some(master) = master {
        unsafe { libc::close(master.as_raw_fd()) };
    }
    if let Some(slave) = slave {
        if unsafe { libc::setsid() } < 0 {
            return io::Error::last_os_error();
        }
        if unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY as _, 0) } != 0 {
            return io::Error::last_os_error();
        }
    } else if unsafe { libc::setpgid(0, 0) } != 0 {
        return io::Error::last_os_error();
    }
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGHUP) } != 0 {
        return io::Error::last_os_error();
    }
    for index in 0..3 {
        let source = if request.tty_mask & (1 << index) != 0 {
            slave
                .as_ref()
                .expect("terminal mask requires a PTY")
                .as_raw_fd()
        } else {
            request.descriptors[index].as_raw_fd()
        };
        if unsafe { libc::dup2(source, index as RawFd) } < 0 {
            return io::Error::last_os_error();
        }
    }
    if unsafe { libc::fchdir(request.descriptors[3].as_raw_fd()) } != 0 {
        return io::Error::last_os_error();
    }
    unsafe { libc::umask(request.umask as libc::mode_t) };
    if unsafe {
        libc::initgroups(
            INTERACTIVE_USER.as_ptr().cast(),
            INTERACTIVE_GID as libc::gid_t,
        )
    } != 0
    {
        return io::Error::last_os_error();
    }
    if unsafe {
        libc::setresgid(
            INTERACTIVE_GID as libc::gid_t,
            INTERACTIVE_GID as libc::gid_t,
            INTERACTIVE_GID as libc::gid_t,
        )
    } != 0
    {
        return io::Error::last_os_error();
    }
    if unsafe { libc::setresuid(INTERACTIVE_UID as libc::uid_t, 0, 0) } != 0 {
        return io::Error::last_os_error();
    }
    exec_real_sudo(request)
}

fn exec_real_sudo(request: &Request) -> io::Error {
    let descriptor = unsafe {
        libc::open(
            REAL_SUDO.as_ptr().cast(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if descriptor < 0 {
        return io::Error::last_os_error();
    }
    let metadata = match descriptor_metadata(descriptor) {
        Ok(metadata) => metadata,
        Err(error) => return error,
    };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_uid != 0
        || metadata.st_gid != 0
        || metadata.st_mode & 0o022 != 0
        || metadata.st_mode & libc::S_ISUID == 0
    {
        return io::Error::new(
            io::ErrorKind::PermissionDenied,
            "real sudo is not a trusted root-owned setuid executable",
        );
    }
    let mut arguments = Vec::with_capacity(request.arguments.len() + 1);
    arguments.push(CString::new(request.mode).expect("static sudo mode has no NUL"));
    for argument in &request.arguments {
        match CString::new(argument.as_os_str().as_bytes()) {
            Ok(argument) => arguments.push(argument),
            Err(_) => return invalid("sudo argument contains NUL"),
        }
    }
    let environment = match request
        .environment
        .iter()
        .map(|entry| CString::new(entry.as_os_str().as_bytes()))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(environment) => environment,
        Err(_) => return invalid("sudo environment contains NUL"),
    };
    let mut argument_pointers = arguments
        .iter()
        .map(|argument| argument.as_ptr())
        .collect::<Vec<_>>();
    argument_pointers.push(std::ptr::null());
    let mut environment_pointers = environment
        .iter()
        .map(|entry| entry.as_ptr())
        .collect::<Vec<_>>();
    environment_pointers.push(std::ptr::null());
    unsafe {
        libc::fexecve(
            descriptor,
            argument_pointers.as_ptr(),
            environment_pointers.as_ptr(),
        );
    }
    io::Error::last_os_error()
}

fn open_pty(terminal: RawFd) -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master = -1;
    let mut slave = -1;
    let mut attributes = MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(terminal, attributes.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut size = MaybeUninit::<libc::winsize>::zeroed();
    if unsafe { libc::ioctl(terminal, libc::TIOCGWINSZ as _, size.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            attributes.as_ptr(),
            size.as_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    set_nonblocking(master)?;
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

fn set_nonblocking(descriptor: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn relay_server(
    connection: &mut UnixStream,
    child: libc::pid_t,
    master: Option<RawFd>,
) -> io::Result<()> {
    connection.set_nonblocking(false)?;
    let mut final_status = None;
    let mut master_open = master.is_some();
    loop {
        if final_status.is_none() {
            let mut status = 0;
            let waited = unsafe {
                libc::waitpid(
                    child,
                    &mut status,
                    libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
                )
            };
            if waited < 0 {
                return Err(io::Error::last_os_error());
            }
            if waited == child {
                if libc::WIFSTOPPED(status) {
                    send_frame(
                        connection,
                        FRAME_STOPPED,
                        &(libc::WSTOPSIG(status) as i32).to_be_bytes(),
                    )?;
                } else if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                    final_status = Some(status);
                }
            }
        }
        if let (Some(status), false) = (final_status, master_open) {
            let (code, signal) = if libc::WIFEXITED(status) {
                (libc::WEXITSTATUS(status), 0)
            } else {
                (128 + libc::WTERMSIG(status), libc::WTERMSIG(status))
            };
            let mut payload = Vec::with_capacity(8);
            payload.extend_from_slice(&code.to_be_bytes());
            payload.extend_from_slice(&signal.to_be_bytes());
            send_frame(connection, FRAME_STATUS, &payload)?;
            return Ok(());
        }

        let mut polls = [
            libc::pollfd {
                fd: connection.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            },
            libc::pollfd {
                fd: master.unwrap_or(-1),
                events: if master_open {
                    libc::POLLIN | libc::POLLHUP
                } else {
                    0
                },
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, 50) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if polls[0].revents & libc::POLLIN != 0 {
            match read_frame(connection) {
                Ok((FRAME_INPUT, payload)) if master_open => {
                    write_all_fd(master.unwrap(), &payload)?
                }
                Ok((FRAME_SIGNAL, payload)) if payload.len() == 4 => {
                    let signal = i32::from_be_bytes(payload.try_into().expect("length checked"));
                    if allowed_signal(signal) {
                        unsafe { libc::kill(-child, signal) };
                    }
                }
                Ok((FRAME_RESIZE, payload)) if payload.len() == 8 && master_open => {
                    let size = libc::winsize {
                        ws_row: u16::from_be_bytes([payload[0], payload[1]]),
                        ws_col: u16::from_be_bytes([payload[2], payload[3]]),
                        ws_xpixel: u16::from_be_bytes([payload[4], payload[5]]),
                        ws_ypixel: u16::from_be_bytes([payload[6], payload[7]]),
                    };
                    unsafe { libc::ioctl(master.unwrap(), libc::TIOCSWINSZ as _, &size) };
                }
                Ok(_) => return Err(invalid("invalid sudo client frame")),
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    unsafe { libc::kill(-child, libc::SIGHUP) };
                }
                Err(error) => return Err(error),
            }
        }
        if polls[0].revents & libc::POLLHUP != 0 && polls[0].revents & libc::POLLIN == 0 {
            unsafe { libc::kill(-child, libc::SIGHUP) };
        }
        if master_open && polls[1].revents & libc::POLLIN != 0 {
            let mut output = [0u8; 16_384];
            let count =
                unsafe { libc::read(master.unwrap(), output.as_mut_ptr().cast(), output.len()) };
            if count > 0 {
                send_frame(connection, FRAME_OUTPUT, &output[..count as usize])?;
            } else if count == 0 {
                master_open = false;
            } else {
                let error = io::Error::last_os_error();
                if !matches!(error.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EIO)) {
                    return Err(error);
                }
                if error.raw_os_error() == Some(libc::EIO) {
                    master_open = false;
                }
            }
        }
        if master_open && polls[1].revents & libc::POLLHUP != 0 {
            let mut output = [0u8; 16_384];
            loop {
                let count = unsafe {
                    libc::read(master.unwrap(), output.as_mut_ptr().cast(), output.len())
                };
                if count > 0 {
                    send_frame(connection, FRAME_OUTPUT, &output[..count as usize])?;
                } else {
                    break;
                }
            }
            master_open = false;
        }
        if final_status.is_some() && master.is_none() {
            master_open = false;
        }
    }
}

fn client_relay(
    connection: &mut UnixStream,
    terminal_fd: Option<RawFd>,
    mut terminal: Option<&mut TerminalGuard>,
    signal_reader: &OwnedFd,
) -> io::Result<u8> {
    let mut ready = false;
    loop {
        let mut polls = [
            libc::pollfd {
                fd: connection.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            },
            libc::pollfd {
                fd: terminal_fd.unwrap_or(-1),
                events: if ready && terminal_fd.is_some() {
                    libc::POLLIN
                } else {
                    0
                },
                revents: 0,
            },
            libc::pollfd {
                fd: signal_reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as _, -1) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if polls[0].revents & libc::POLLIN != 0 {
            match read_frame(connection)? {
                (FRAME_READY, payload) if payload.is_empty() => {
                    ready = true;
                    if let Some(terminal) = terminal.as_deref_mut() {
                        terminal.enter_raw()?;
                    }
                    if let Some(descriptor) = terminal_fd {
                        send_resize(connection, descriptor)?;
                    }
                }
                (FRAME_OUTPUT, payload) => {
                    if let Some(descriptor) = terminal_fd {
                        write_all_fd(descriptor, &payload)?;
                    } else {
                        io::stderr().write_all(&payload)?;
                    }
                }
                (FRAME_STOPPED, payload) if payload.len() == 4 => {
                    if let Some(terminal) = terminal.as_deref_mut() {
                        terminal.restore();
                    }
                    suspend_self()?;
                    if let Some(terminal) = terminal.as_deref_mut() {
                        terminal.enter_raw()?;
                    }
                    send_frame(connection, FRAME_SIGNAL, &libc::SIGCONT.to_be_bytes())?;
                }
                (FRAME_STATUS, payload) if payload.len() == 8 => {
                    if let Some(terminal) = terminal.as_deref_mut() {
                        terminal.restore();
                    }
                    let code = i32::from_be_bytes(payload[..4].try_into().expect("length checked"));
                    return Ok(u8::try_from(code).unwrap_or(255));
                }
                (FRAME_ERROR, payload) => {
                    if let Some(terminal) = terminal.as_deref_mut() {
                        terminal.restore();
                    }
                    return Err(io::Error::other(
                        String::from_utf8_lossy(&payload).into_owned(),
                    ));
                }
                _ => return Err(invalid("invalid sudo service frame")),
            }
        }
        if polls[0].revents & libc::POLLHUP != 0 && polls[0].revents & libc::POLLIN == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sudo service closed without status",
            ));
        }
        if ready && polls[1].revents & libc::POLLIN != 0 {
            let mut input = [0u8; 16_384];
            let count =
                unsafe { libc::read(terminal_fd.unwrap(), input.as_mut_ptr().cast(), input.len()) };
            if count > 0 {
                send_frame(connection, FRAME_INPUT, &input[..count as usize])?;
            } else if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
        if polls[2].revents & libc::POLLIN != 0 {
            let mut signals = [0u8; 64];
            let count = unsafe {
                libc::read(
                    signal_reader.as_raw_fd(),
                    signals.as_mut_ptr().cast(),
                    signals.len(),
                )
            };
            if count > 0 {
                for signal in &signals[..count as usize] {
                    let signal = i32::from(*signal);
                    if signal == libc::SIGWINCH {
                        if let Some(descriptor) = terminal_fd {
                            send_resize(connection, descriptor)?;
                        }
                    } else if allowed_signal(signal) {
                        send_frame(connection, FRAME_SIGNAL, &signal.to_be_bytes())?;
                    }
                }
            }
        }
    }
}

fn send_resize(connection: &mut UnixStream, terminal: RawFd) -> io::Result<()> {
    let mut size = MaybeUninit::<libc::winsize>::zeroed();
    if unsafe { libc::ioctl(terminal, libc::TIOCGWINSZ as _, size.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let size = unsafe { size.assume_init() };
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&size.ws_row.to_be_bytes());
    payload.extend_from_slice(&size.ws_col.to_be_bytes());
    payload.extend_from_slice(&size.ws_xpixel.to_be_bytes());
    payload.extend_from_slice(&size.ws_ypixel.to_be_bytes());
    send_frame(connection, FRAME_RESIZE, &payload)
}

fn send_frame(connection: &mut UnixStream, kind: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(invalid("sudo protocol frame is too large"));
    }
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    connection.write_all(&header)?;
    connection.write_all(payload)
}

fn read_frame(connection: &mut UnixStream) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    connection.read_exact(&mut header)?;
    let length = read_u32(&header[1..]) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(invalid("sudo protocol frame exceeds the limit"));
    }
    let mut payload = vec![0u8; length];
    connection.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

fn signal_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

extern "C" fn signal_handler(signal: libc::c_int) {
    let descriptor = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if descriptor >= 0 {
        let byte = signal as u8;
        unsafe {
            libc::write(descriptor, (&byte as *const u8).cast(), 1);
        }
    }
}

fn install_signal_handlers(write_fd: RawFd) -> io::Result<()> {
    SIGNAL_WRITE_FD.store(write_fd, Ordering::SeqCst);
    for signal in [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTERM,
        libc::SIGTSTP,
        libc::SIGCONT,
        libc::SIGWINCH,
    ] {
        let mut action = unsafe { mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = signal_handler as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn suspend_self() -> io::Result<()> {
    let mut action = unsafe { mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = libc::SIG_DFL;
    unsafe { libc::sigemptyset(&mut action.sa_mask) };
    let mut previous = MaybeUninit::<libc::sigaction>::uninit();
    if unsafe { libc::sigaction(libc::SIGTSTP, &action, previous.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::raise(libc::SIGTSTP) };
    let previous = unsafe { previous.assume_init() };
    if unsafe { libc::sigaction(libc::SIGTSTP, &previous, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn allowed_signal(signal: i32) -> bool {
    matches!(
        signal,
        libc::SIGHUP
            | libc::SIGINT
            | libc::SIGQUIT
            | libc::SIGTERM
            | libc::SIGTSTP
            | libc::SIGCONT
            | libc::SIGWINCH
    )
}

fn write_all_fd(descriptor: RawFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        let written = unsafe { libc::write(descriptor, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "terminal write returned zero",
            ));
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn current_umask() -> u32 {
    let mask = unsafe { libc::umask(0) };
    unsafe { libc::umask(mask) };
    mask as u32
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("four-byte protocol field"))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip_preserves_opaque_arguments_and_environment() {
        let arguments = vec![
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from("printf '%s\\n' \"$HOME with spaces\""),
        ];
        let environment = vec![
            OsString::from("TERM=xterm-256color"),
            OsString::from("VALUE=line one\nline two=three"),
        ];
        let encoded =
            encode_request("sudo", 0o027, 0, false, &arguments, environment.clone()).unwrap();
        let null = || open_path(OsStr::new("/dev/null"), libc::O_RDWR).unwrap();
        let cwd = open_path(OsStr::new("."), libc::O_PATH | libc::O_DIRECTORY).unwrap();
        let decoded = decode_request(&encoded[4..], vec![null(), null(), null(), cwd]).unwrap();
        assert_eq!(decoded.mode, "sudo");
        assert_eq!(decoded.umask, 0o027);
        assert_eq!(decoded.arguments, arguments);
        assert_eq!(decoded.environment, environment);
    }

    #[test]
    fn request_rejects_extra_descriptors_and_unknown_modes() {
        let encoded = encode_request("sudoedit", 0o022, 0, false, &[], []).unwrap();
        let null = || open_path(OsStr::new("/dev/null"), libc::O_RDWR).unwrap();
        let cwd = open_path(OsStr::new("."), libc::O_PATH | libc::O_DIRECTORY).unwrap();
        assert!(decode_request(&encoded[4..], vec![null(), null(), null(), cwd, null()]).is_err());
        let mut corrupt = encoded[4..].to_vec();
        corrupt[8] = 9;
        let cwd = open_path(OsStr::new("."), libc::O_PATH | libc::O_DIRECTORY).unwrap();
        assert!(decode_request(&corrupt, vec![null(), null(), null(), cwd]).is_err());
    }

    #[test]
    fn only_expected_signals_are_forwarded() {
        assert!(allowed_signal(libc::SIGINT));
        assert!(allowed_signal(libc::SIGWINCH));
        assert!(!allowed_signal(libc::SIGKILL));
        assert!(!allowed_signal(libc::SIGSTOP));
    }
}
