// SPDX-License-Identifier: AGPL-3.0-or-later

//! Host-authorized live integration boundary.
//!
//! Nothing in this module accepts a command line or path supplied by the
//! guest. The broker consumes validated `machine.json` state from outside the
//! rootfs and exposes only the exact port and media endpoints selected there.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, UdpSocket};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;
use wb_core::{
    HostMediaBackend, HostMediaDevice, HostMediaKind, IntegrationDiagnostics, IntegrationSettings,
    MediaIntegrationDiagnostics, PortDirection, PortForward, PortIntegrationDiagnostics,
    PortProtocol, ResourceLocator, discover_host_media,
};

use crate::{TerminateOnDrop, pipe, terminate};

const SLIRP_GUEST_ADDRESS: &str = "10.0.2.100";
const GUEST_AUDIO_PORT: u16 = 47_130;
const HOST_MICROPHONE_PORT: u16 = 47_131;
const HOST_CAMERA_PORT: u16 = 47_132;
const MAX_PIPEWIRE_DUMP_BYTES: usize = 32 * 1024 * 1024;
const MICROPHONE_APPLICATION_ID: &str = "org.openresearchtools.WildBuzzard";
const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_PIX_FMT_MJPEG: u32 = u32::from_le_bytes(*b"MJPG");
const VIDIOC_ENUM_FMT: libc::c_ulong = 0xc040_5602;

#[repr(C)]
#[derive(Default)]
struct V4l2FormatDescription {
    index: u32,
    buffer_type: u32,
    flags: u32,
    description: [u8; 32],
    pixel_format: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

pub(crate) struct SlirpRuntime {
    pub(crate) process: TerminateOnDrop,
    api_socket: PathBuf,
}

/// Removes a newly-created private mapping on every unsuccessful media start.
/// The ID is transferred to `ActiveMedia` only after the fixed pipeline is
/// alive and (for microphones) its host-accounted recording stream is proven.
struct PendingSlirpForward<'a> {
    slirp: &'a SlirpRuntime,
    id: Option<i64>,
}

impl<'a> PendingSlirpForward<'a> {
    fn new(slirp: &'a SlirpRuntime, id: i64) -> Self {
        Self {
            slirp,
            id: Some(id),
        }
    }

    fn commit(mut self) -> i64 {
        self.id.take().expect("pending slirp mapping is present")
    }
}

impl Drop for PendingSlirpForward<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let _ = self.slirp.remove_media_forward_fail_closed(id);
        }
    }
}

impl SlirpRuntime {
    pub(crate) fn start(
        resources: &ResourceLocator,
        container_pid: u32,
        api_socket: &Path,
    ) -> Result<Self> {
        let slirp = resources.helper_or_path("slirp4netns")?;
        if api_socket.as_os_str().as_encoded_bytes().len() >= 100 {
            bail!("private network API socket path is too long");
        }
        match fs::remove_file(api_socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("clearing {}", api_socket.display()));
            }
        }
        let (ready_read, ready_write) = pipe().context("creating network readiness pipe")?;
        let ready_fd = std::os::fd::AsRawFd::as_raw_fd(&ready_write).to_string();
        let parent_pid = unsafe { libc::getpid() };
        let mut command = Command::new(&slirp);
        command
            .args([
                "--configure",
                "--disable-host-loopback",
                "--enable-sandbox",
                "--enable-seccomp",
                "--mtu=65520",
                "--ready-fd",
                &ready_fd,
                "--api-socket",
            ])
            .arg(api_socket)
            .args([&container_pid.to_string(), "tap0"])
            .stdin(Stdio::null());
        // SAFETY: only async-signal-safe libc calls run between fork and exec.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(move || {
                // slirp creates the API socket after joining the subordinate
                // user namespace, so its apparent owner on the host is the
                // mapped container root UID. The containing directory is
                // host-owned 0700 and is never guest-mounted; a 0777 socket
                // within it lets only this broker reach the endpoint while
                // avoiding an impossible host-side chmod of subordinate-owned
                // metadata.
                libc::umask(0);
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "Buzzard OS broker exited during network-helper startup",
                    ));
                }
                Ok(())
            });
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("starting bundled network helper {}", slirp.display()))?;
        drop(ready_write);

        // SAFETY: pipe() returned this descriptor and this File assumes its
        // ownership exactly once.
        let mut ready = unsafe { fs::File::from_raw_fd(ready_read) };
        let mut byte = [0_u8; 1];
        if let Err(error) = ready.read_exact(&mut byte) {
            terminate(&mut child);
            return Err(error).context("network helper exited before configuring the namespace");
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !api_socket.exists() {
            if let Some(status) = child.try_wait().context("checking network helper")? {
                bail!("network helper exited with {status} before creating its API socket");
            }
            if Instant::now() >= deadline {
                terminate(&mut child);
                bail!("network helper did not create its private API socket");
            }
            thread::sleep(Duration::from_millis(10));
        }
        Ok(Self {
            process: TerminateOnDrop { child },
            api_socket: api_socket.to_path_buf(),
        })
    }

    fn request(&self, request: Value) -> Result<Value> {
        let mut stream = UnixStream::connect(&self.api_socket)
            .with_context(|| format!("connecting to {}", self.api_socket.display()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .context("setting network API timeout")?;
        // slirp4netns's API consumes one bounded request read per accepted
        // connection. Serialize first and submit the complete small JSON
        // object in one write instead of exposing serde's field-sized writes
        // as partial requests.
        let request = serde_json::to_vec(&request).context("serializing network API request")?;
        stream
            .write_all(&request)
            .context("writing network API request")?;
        stream
            .shutdown(Shutdown::Write)
            .context("finishing network API request")?;
        let mut response = Vec::new();
        stream
            .take(1024 * 1024)
            .read_to_end(&mut response)
            .context("reading network API response")?;
        let response: Value =
            serde_json::from_slice(&response).context("parsing network API response")?;
        if let Some(error) = response.get("error").filter(|value| !value.is_null()) {
            bail!("slirp4netns rejected live mapping: {error}");
        }
        Ok(response.get("return").cloned().unwrap_or(Value::Null))
    }

    fn add_host_forward(&self, mapping: &PortForward) -> Result<i64> {
        let protocol = match mapping.protocol {
            PortProtocol::Tcp => "tcp",
            PortProtocol::Udp => "udp",
        };
        let result = self.request(json!({
            "execute": "add_hostfwd",
            "arguments": {
                "proto": protocol,
                "host_addr": mapping.host_address,
                "host_port": mapping.host_port,
                "guest_addr": mapping.guest_address,
                "guest_port": mapping.guest_port,
            }
        }))?;
        result
            .get("id")
            .and_then(Value::as_i64)
            .context("network helper did not return a mapping id")
    }

    fn remove_host_forward(&self, id: i64) -> Result<()> {
        self.request(json!({
            "execute": "remove_hostfwd",
            "arguments": { "id": id }
        }))?;
        Ok(())
    }

    fn remove_media_forward_fail_closed(&self, id: i64) -> Result<()> {
        if let Err(error) = self.remove_host_forward(id) {
            // Once a media mapping exists, an unconfirmed removal must not
            // leave a stale route into the guest. Stop private networking;
            // the broker supervisor treats that as fatal and terminates the
            // machine, which removes every guest listener/source as well.
            unsafe {
                libc::kill(self.process.child.id() as i32, libc::SIGKILL);
            }
            return Err(error.context(format!(
                "revoking private media mapping {id} failed; private networking was terminated fail-closed"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
struct GuestIntegrationControl<'a> {
    schema: u32,
    generation: u64,
    reverse_ports: Vec<&'a PortForward>,
    forward_udp_ports: Vec<&'a PortForward>,
    media: &'a wb_core::MediaSharing,
}

struct ActiveHostForward {
    mapping: PortForward,
    backend: HostForwardBackend,
}

enum HostForwardBackend {
    Slirp(i64),
    Udp(HostUdpRelay),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MediaKind {
    GuestAudio,
    HostMicrophone,
    HostCamera,
}

impl MediaKind {
    fn name(self) -> &'static str {
        match self {
            Self::GuestAudio => "guest-audio-output",
            Self::HostMicrophone => "host-microphone",
            Self::HostCamera => "host-camera",
        }
    }

    fn guest_port(self) -> u16 {
        match self {
            Self::GuestAudio => GUEST_AUDIO_PORT,
            Self::HostMicrophone => HOST_MICROPHONE_PORT,
            Self::HostCamera => HOST_CAMERA_PORT,
        }
    }
}

struct ActiveMedia {
    process: TerminateOnDrop,
    slirp_id: i64,
    target: Option<String>,
    resolved_device: Option<String>,
    tracking_target: Option<String>,
}

struct ReverseTcpRelay {
    stop: Arc<AtomicBool>,
    socket_path: PathBuf,
    thread: Option<JoinHandle<()>>,
}

struct ReverseUdpRelay {
    stop: Arc<AtomicBool>,
    socket_path: PathBuf,
    thread: Option<JoinHandle<()>>,
}

struct HostUdpRelay {
    stop: Arc<AtomicBool>,
    socket_path: PathBuf,
    thread: Option<JoinHandle<()>>,
}

impl HostUdpRelay {
    fn start(mapping: &PortForward, socket_dir: &Path) -> Result<Self> {
        let socket_path = socket_dir.join(format!("forward-{}.sock", mapping.id));
        let client_path = socket_dir.join(format!("forward-client-{}.sock", mapping.id));
        match fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("clearing {}", socket_path.display()));
            }
        }
        let relay = UnixDatagram::bind(&socket_path)
            .with_context(|| format!("binding {}", socket_path.display()))?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o666))
            .with_context(|| format!("setting permissions on {}", socket_path.display()))?;
        relay
            .set_nonblocking(true)
            .context("making host-to-guest UDP relay nonblocking")?;
        let listener = UdpSocket::bind((mapping.host_address.as_str(), mapping.host_port))
            .with_context(|| {
                format!(
                    "binding host UDP listener {}:{}",
                    mapping.host_address, mapping.host_port
                )
            })?;
        listener
            .set_nonblocking(true)
            .context("making host UDP listener nonblocking")?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let expected_client_name = client_path.file_name().map(|name| name.to_owned());
        let thread = thread::Builder::new()
            .name(format!("wb-forward-udp-{}", mapping.id))
            .spawn(move || {
                let mut peer_tokens: BTreeMap<std::net::SocketAddr, u64> = BTreeMap::new();
                let mut token_peers: BTreeMap<u64, std::net::SocketAddr> = BTreeMap::new();
                let mut next_token = 1_u64;
                let mut packet = vec![0_u8; 65_535];
                while !worker_stop.load(Ordering::Acquire) {
                    loop {
                        match listener.recv_from(&mut packet[8..]) {
                            Ok((length, peer)) => {
                                let token = *peer_tokens.entry(peer).or_insert_with(|| {
                                    let token = next_token;
                                    next_token = next_token.saturating_add(1);
                                    token_peers.insert(token, peer);
                                    token
                                });
                                packet[..8].copy_from_slice(&token.to_be_bytes());
                                let _ = relay.send_to(&packet[..length + 8], &client_path);
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(_) => return,
                        }
                    }
                    loop {
                        match relay.recv_from(&mut packet) {
                            Ok((length, address)) if length >= 8 => {
                                // The same mounted Unix socket has a host path
                                // and a guest path. Its sockaddr contains the
                                // path used by the guest bind(), so compare the
                                // stable, unguessable per-mapping basename.
                                if address.as_pathname().and_then(Path::file_name)
                                    != expected_client_name.as_deref()
                                {
                                    continue;
                                }
                                let token = u64::from_be_bytes(
                                    packet[..8].try_into().expect("fixed token header"),
                                );
                                if let Some(peer) = token_peers.get(&token) {
                                    let _ = listener.send_to(&packet[8..length], peer);
                                }
                            }
                            Ok(_) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(_) => return,
                        }
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            })
            .context("starting host-to-guest UDP relay")?;
        Ok(Self {
            stop,
            socket_path,
            thread: Some(thread),
        })
    }

    fn is_active(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
    }
}

impl Drop for HostUdpRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

impl ReverseUdpRelay {
    fn start(mapping: &PortForward, socket_dir: &Path) -> Result<Self> {
        let socket_path = socket_dir.join(format!("reverse-{}.sock", mapping.id));
        let client_path = socket_dir.join(format!("reverse-client-{}.sock", mapping.id));
        match fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("clearing {}", socket_path.display()));
            }
        }
        let relay = UnixDatagram::bind(&socket_path)
            .with_context(|| format!("binding {}", socket_path.display()))?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o666))
            .with_context(|| format!("setting permissions on {}", socket_path.display()))?;
        relay
            .set_nonblocking(true)
            .context("making reverse UDP relay nonblocking")?;
        let destination = format!("{}:{}", mapping.host_address, mapping.host_port)
            .parse::<std::net::SocketAddr>()
            .context("parsing reverse UDP destination")?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name(format!("wb-reverse-udp-{}", mapping.id))
            .spawn(move || {
                let mut clients: BTreeMap<u64, UdpSocket> = BTreeMap::new();
                let mut packet = vec![0_u8; 65_535];
                while !worker_stop.load(Ordering::Acquire) {
                    loop {
                        match relay.recv_from(&mut packet) {
                            Ok((length, address)) if length >= 8 => {
                                if address.as_pathname().and_then(Path::file_name)
                                    != client_path.file_name()
                                {
                                    continue;
                                }
                                let token = u64::from_be_bytes(
                                    packet[..8].try_into().expect("fixed token header"),
                                );
                                if let std::collections::btree_map::Entry::Vacant(entry) =
                                    clients.entry(token)
                                {
                                    let Ok(socket) = UdpSocket::bind("127.0.0.1:0") else {
                                        continue;
                                    };
                                    if socket.connect(destination).is_err()
                                        || socket.set_nonblocking(true).is_err()
                                    {
                                        continue;
                                    }
                                    entry.insert(socket);
                                }
                                if let Some(socket) = clients.get(&token) {
                                    let _ = socket.send(&packet[8..length]);
                                }
                            }
                            Ok(_) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(_) => return,
                        }
                    }
                    for (token, socket) in &clients {
                        loop {
                            match socket.recv(&mut packet[8..]) {
                                Ok(length) => {
                                    packet[..8].copy_from_slice(&token.to_be_bytes());
                                    let _ = relay.send_to(&packet[..length + 8], &client_path);
                                }
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            })
            .context("starting reverse UDP relay")?;
        Ok(Self {
            stop,
            socket_path,
            thread: Some(thread),
        })
    }
}

impl Drop for ReverseUdpRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

enum ReverseRelay {
    Tcp(ReverseTcpRelay),
    Udp(ReverseUdpRelay),
}

impl ReverseRelay {
    fn start(mapping: &PortForward, socket_dir: &Path) -> Result<Self> {
        match mapping.protocol {
            PortProtocol::Tcp => ReverseTcpRelay::start(mapping, socket_dir).map(Self::Tcp),
            PortProtocol::Udp => ReverseUdpRelay::start(mapping, socket_dir).map(Self::Udp),
        }
    }

    fn is_active(&self) -> bool {
        match self {
            Self::Tcp(relay) => !relay.stop.load(Ordering::Acquire),
            Self::Udp(relay) => !relay.stop.load(Ordering::Acquire),
        }
    }
}

impl ReverseTcpRelay {
    fn start(mapping: &PortForward, socket_dir: &Path) -> Result<Self> {
        let socket_path = socket_dir.join(format!("reverse-{}.sock", mapping.id));
        match fs::remove_file(&socket_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("clearing {}", socket_path.display()));
            }
        }
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("binding {}", socket_path.display()))?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o666))
            .with_context(|| format!("setting permissions on {}", socket_path.display()))?;
        listener
            .set_nonblocking(true)
            .context("making reverse relay nonblocking")?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let destination = format!("{}:{}", mapping.host_address, mapping.host_port);
        let thread = thread::Builder::new()
            .name(format!("wb-reverse-{}", mapping.id))
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((guest, _)) => {
                            let destination = destination.clone();
                            let connection_stop = Arc::clone(&worker_stop);
                            let _ = thread::Builder::new()
                                .name("wb-reverse-tcp-connection".into())
                                .spawn(move || {
                                    let Ok(host) = TcpStream::connect(&destination) else {
                                        let _ = guest.shutdown(Shutdown::Both);
                                        return;
                                    };
                                    relay_tcp(guest, host, &connection_stop);
                                });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(20));
                        }
                        Err(_) => break,
                    }
                }
            })
            .context("starting reverse TCP relay")?;
        Ok(Self {
            stop,
            socket_path,
            thread: Some(thread),
        })
    }
}

impl Drop for ReverseTcpRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn relay_tcp(guest: UnixStream, host: TcpStream, stop: &AtomicBool) {
    let _ = guest.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = guest.set_write_timeout(Some(Duration::from_millis(200)));
    let _ = host.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = host.set_write_timeout(Some(Duration::from_millis(200)));
    let Ok(mut guest_read) = guest.try_clone() else {
        return;
    };
    let Ok(mut host_read) = host.try_clone() else {
        return;
    };
    let mut guest_write = guest;
    let mut host_write = host;
    let finished = Arc::new(AtomicBool::new(false));
    let opposite = Arc::clone(&finished);
    let upstream = thread::spawn(move || copy_until(&mut guest_read, &mut host_write, &opposite));
    copy_until(&mut host_read, &mut guest_write, stop);
    finished.store(true, Ordering::Release);
    let _ = upstream.join();
    let _ = guest_write.shutdown(Shutdown::Both);
    let _ = host_read.shutdown(Shutdown::Both);
}

fn copy_until<R: Read, W: Write>(reader: &mut R, writer: &mut W, stop: &AtomicBool) {
    let mut buffer = [0_u8; 64 * 1024];
    while !stop.load(Ordering::Acquire) {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(length) => {
                if writer.write_all(&buffer[..length]).is_err() {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }
}

pub(crate) struct IntegrationRuntime {
    generation: u64,
    applied: IntegrationSettings,
    host_forwards: BTreeMap<Uuid, ActiveHostForward>,
    reverse: BTreeMap<Uuid, (PortForward, ReverseRelay)>,
    socket_dir: PathBuf,
    control_path: PathBuf,
    guest_status_path: PathBuf,
    host_status_dir: PathBuf,
    media: BTreeMap<MediaKind, ActiveMedia>,
}

impl IntegrationRuntime {
    pub(crate) fn new(
        socket_dir: &Path,
        display_state: &Path,
        host_status_dir: &Path,
    ) -> Result<Self> {
        let reverse_dir = socket_dir.join("reverse");
        fs::create_dir(&reverse_dir)
            .with_context(|| format!("creating {}", reverse_dir.display()))?;
        fs::set_permissions(&reverse_dir, fs::Permissions::from_mode(0o777))
            .with_context(|| format!("setting permissions on {}", reverse_dir.display()))?;
        Ok(Self {
            generation: 0,
            applied: IntegrationSettings::default(),
            host_forwards: BTreeMap::new(),
            reverse: BTreeMap::new(),
            socket_dir: reverse_dir,
            control_path: display_state.join("integration.json"),
            guest_status_path: socket_dir.join("integration-status.json"),
            host_status_dir: host_status_dir.to_path_buf(),
            media: BTreeMap::new(),
        })
    }

    pub(crate) fn reconcile(
        &mut self,
        requested: &IntegrationSettings,
        slirp: Option<&SlirpRuntime>,
        resources: &ResourceLocator,
    ) -> Result<IntegrationDiagnostics> {
        // Revoke host-side access before doing any health check that may fail.
        // In particular, disabling a microphone must not leave the guest
        // control enabled merely because its already-revoked PipeWire stream
        // disappeared while the transaction was being inspected.
        self.stop_removed_media(&requested.media, slirp)?;
        let removed_unhealthy_media = self.remove_unhealthy_media(slirp, resources)?;
        let host_resources_match = self.host_resources_match(requested);
        let guest_state_matches = self.guest_state_matches(requested);
        if &self.applied == requested
            && !removed_unhealthy_media
            && host_resources_match
            && guest_state_matches
        {
            return Ok(self.diagnostics(requested));
        }

        let wanted_host: BTreeMap<_, _> = requested
            .ports
            .iter()
            .filter(|mapping| mapping.enabled && mapping.direction == PortDirection::HostToGuest)
            .map(|mapping| (mapping.id, mapping))
            .collect();
        let stale_host: Vec<_> = self
            .host_forwards
            .iter()
            .filter(|(id, active)| wanted_host.get(id).copied() != Some(&active.mapping))
            .map(|(id, _)| *id)
            .collect();
        for id in stale_host {
            if let Some(active) = self.host_forwards.remove(&id) {
                if let HostForwardBackend::Slirp(slirp_id) = active.backend {
                    slirp
                        .context("host-to-guest mappings require private user-mode networking")?
                        .remove_host_forward(slirp_id)?;
                }
            }
        }
        for (id, mapping) in wanted_host {
            if !self.host_forwards.contains_key(&id) {
                let backend = match mapping.protocol {
                    PortProtocol::Tcp => HostForwardBackend::Slirp(
                        slirp
                            .context("host-to-guest mappings require private user-mode networking")?
                            .add_host_forward(mapping)?,
                    ),
                    PortProtocol::Udp => {
                        HostForwardBackend::Udp(HostUdpRelay::start(mapping, &self.socket_dir)?)
                    }
                };
                self.host_forwards.insert(
                    id,
                    ActiveHostForward {
                        mapping: mapping.clone(),
                        backend,
                    },
                );
            }
        }

        let wanted_reverse: BTreeMap<_, _> = requested
            .ports
            .iter()
            .filter(|mapping| mapping.enabled && mapping.direction == PortDirection::GuestToHost)
            .map(|mapping| (mapping.id, mapping))
            .collect();
        self.reverse
            .retain(|id, (active, _)| wanted_reverse.get(id).copied() == Some(active));
        for (id, mapping) in &wanted_reverse {
            if !self.reverse.contains_key(id) {
                let relay = ReverseRelay::start(mapping, &self.socket_dir)?;
                self.reverse.insert(*id, ((*mapping).clone(), relay));
            }
        }

        self.generation = self.generation.saturating_add(1);
        self.write_guest_control(requested)?;
        self.wait_for_guest_state(requested)?;
        self.start_added_media(&requested.media, slirp, resources)?;
        if !self.host_resources_match(requested) || !self.guest_state_matches(requested) {
            bail!(
                "integration generation {} did not converge after reconciliation",
                self.generation
            );
        }
        self.applied = requested.clone();
        Ok(self.diagnostics(requested))
    }

    fn host_resources_match(&self, settings: &IntegrationSettings) -> bool {
        let wanted_host: BTreeMap<_, _> = settings
            .ports
            .iter()
            .filter(|mapping| mapping.enabled && mapping.direction == PortDirection::HostToGuest)
            .map(|mapping| (mapping.id, mapping))
            .collect();
        let host_matches = self.host_forwards.len() == wanted_host.len()
            && self.host_forwards.iter().all(|(id, active)| {
                wanted_host.get(id).copied() == Some(&active.mapping)
                    && match &active.backend {
                        HostForwardBackend::Slirp(_) => true,
                        HostForwardBackend::Udp(relay) => relay.is_active(),
                    }
            });

        let wanted_reverse: BTreeMap<_, _> = settings
            .ports
            .iter()
            .filter(|mapping| mapping.enabled && mapping.direction == PortDirection::GuestToHost)
            .map(|mapping| (mapping.id, mapping))
            .collect();
        let reverse_matches = self.reverse.len() == wanted_reverse.len()
            && self.reverse.iter().all(|(id, (active, relay))| {
                wanted_reverse.get(id).copied() == Some(active) && relay.is_active()
            });

        host_matches && reverse_matches && self.media_resources_match(&settings.media)
    }

    fn media_resources_match(&self, requested: &wb_core::MediaSharing) -> bool {
        [
            (
                MediaKind::GuestAudio,
                requested.guest_audio_output,
                requested.audio_target.as_ref(),
            ),
            (
                MediaKind::HostMicrophone,
                requested.host_microphone,
                requested.microphone_target.as_ref(),
            ),
            (
                MediaKind::HostCamera,
                requested.host_camera,
                requested.camera_target.as_ref(),
            ),
        ]
        .into_iter()
        .all(|(kind, enabled, target)| match self.media.get(&kind) {
            Some(active) => {
                enabled
                    && active.target.as_ref() == target
                    && process_is_running(active.process.child.id())
            }
            None => !enabled,
        })
    }

    fn remove_unhealthy_media(
        &mut self,
        slirp: Option<&SlirpRuntime>,
        resources: &ResourceLocator,
    ) -> Result<bool> {
        let kinds: Vec<_> = self.media.keys().copied().collect();
        let mut removed = false;
        for kind in kinds {
            let (exited, process_id, tracking_target, mut health_error) = {
                let active = self.media.get_mut(&kind).expect("key came from media map");
                let (exited, health_error) = match active.process.child.try_wait() {
                    Ok(status) => (status.is_some(), None),
                    Err(error) => (
                        false,
                        Some(
                            anyhow::Error::new(error)
                                .context(format!("checking {} bridge process health", kind.name())),
                        ),
                    ),
                };
                (
                    exited,
                    active.process.child.id(),
                    active.tracking_target.clone(),
                    health_error,
                )
            };

            let unhealthy = if health_error.is_some() || exited {
                true
            } else if kind == MediaKind::HostMicrophone {
                match read_host_pipewire_graph(resources) {
                    Ok(graph) => microphone_bridge_needs_restart(
                        false,
                        &graph,
                        process_id,
                        tracking_target.as_deref(),
                    ),
                    Err(error) => {
                        health_error = Some(error);
                        true
                    }
                }
            } else {
                false
            };

            if !unhealthy {
                continue;
            }

            let slirp_id = {
                let active = self.media.get_mut(&kind).expect("checked above");
                if !exited {
                    // A live process without its exact running PipeWire-Pulse
                    // source-output is not microphone capture that the host can
                    // account for. Revoke it before attempting a replacement so
                    // diagnostics and the native header cannot claim recording
                    // from a stale PID alone.
                    terminate(&mut active.process.child);
                }
                active.slirp_id
            };
            // Keep the terminated bridge record until the helper confirms
            // removal. If that request fails, fail-closed removal terminates
            // private networking and the record remains owned for teardown.
            slirp
                .context("active media bridge requires private user-mode networking")?
                .remove_media_forward_fail_closed(slirp_id)?;
            self.media.remove(&kind);
            removed = true;

            if let Some(error) = health_error {
                return Err(error.context(format!(
                    "{} health could not be continuously verified; the bridge was revoked",
                    kind.name()
                )));
            }
        }
        Ok(removed)
    }

    fn stop_removed_media(
        &mut self,
        requested: &wb_core::MediaSharing,
        slirp: Option<&SlirpRuntime>,
    ) -> Result<()> {
        let wanted = [
            (
                MediaKind::GuestAudio,
                requested.guest_audio_output,
                requested.audio_target.as_ref(),
            ),
            (
                MediaKind::HostMicrophone,
                requested.host_microphone,
                requested.microphone_target.as_ref(),
            ),
            (
                MediaKind::HostCamera,
                requested.host_camera,
                requested.camera_target.as_ref(),
            ),
        ];
        for (kind, enabled, target) in wanted {
            let remove = self
                .media
                .get(&kind)
                .is_some_and(|active| !enabled || active.target.as_ref() != target);
            if remove {
                let slirp_id = {
                    let active = self.media.get_mut(&kind).expect("checked above");
                    // For host capture, termination is the first and strongest
                    // revocation step. Removing the private mapping then makes a
                    // stale or compromised guest listener unreachable.
                    terminate(&mut active.process.child);
                    active.slirp_id
                };
                slirp
                    .context("active media bridge requires private user-mode networking")?
                    .remove_media_forward_fail_closed(slirp_id)?;
                // Do not discard the mapping ID until removal succeeds. A
                // failed helper request terminates private networking.
                self.media.remove(&kind);
            }
        }
        Ok(())
    }

    fn start_added_media(
        &mut self,
        requested: &wb_core::MediaSharing,
        slirp: Option<&SlirpRuntime>,
        resources: &ResourceLocator,
    ) -> Result<()> {
        let wanted = [
            (
                MediaKind::GuestAudio,
                requested.guest_audio_output,
                requested.audio_target.as_ref(),
            ),
            (
                MediaKind::HostMicrophone,
                requested.host_microphone,
                requested.microphone_target.as_ref(),
            ),
            (
                MediaKind::HostCamera,
                requested.host_camera,
                requested.camera_target.as_ref(),
            ),
        ];
        for (kind, enabled, target) in wanted {
            if enabled && !self.media.contains_key(&kind) {
                let active = self.start_media(kind, target.cloned(), slirp, resources)?;
                self.media.insert(kind, active);
            }
        }
        Ok(())
    }

    fn wait_for_guest_state(&self, settings: &IntegrationSettings) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(bytes) = fs::read(&self.guest_status_path)
                && let Ok(status) = serde_json::from_slice::<GuestIntegrationStatus>(&bytes)
                && status.schema == 1
                && status.generation == self.generation
            {
                if let Some(error) = status.error.as_deref().filter(|error| !error.is_empty()) {
                    bail!(
                        "guest integration agent rejected generation {}: {error}",
                        self.generation
                    );
                }
                if self.guest_status_matches(settings, &status) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                bail!(
                    "guest did not converge to integration generation {}",
                    self.generation
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn start_media(
        &self,
        kind: MediaKind,
        target: Option<String>,
        slirp: Option<&SlirpRuntime>,
        resources: &ResourceLocator,
    ) -> Result<ActiveMedia> {
        require_host_pipewire()?;
        let slirp = slirp.context("media sharing requires private user-mode networking")?;
        let gst = resources.helper_or_path("gst-launch-1.0")?;
        let device = resolve_media_device(kind, target.as_deref(), resources)?;
        let microphone_pulse_server = (kind == MediaKind::HostMicrophone)
            .then(require_host_recording_service)
            .transpose()?;
        let host_port = unused_loopback_port()?;
        let mapping = PortForward {
            id: Uuid::new_v4(),
            enabled: true,
            direction: PortDirection::HostToGuest,
            protocol: PortProtocol::Tcp,
            host_address: "127.0.0.1".into(),
            host_port,
            guest_address: SLIRP_GUEST_ADDRESS.into(),
            guest_port: kind.guest_port(),
        };
        let slirp_id = slirp.add_host_forward(&mapping)?;
        let pending_mapping = PendingSlirpForward::new(slirp, slirp_id);
        let log_path = self.host_status_dir.join(format!("{}.log", kind.name()));
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("opening {}", log_path.display()))?;
        let error_log = log.try_clone().context("cloning media log")?;
        let mut command = Command::new(&gst);
        command.arg("-q");
        match kind {
            MediaKind::GuestAudio => {
                command.args([
                    "tcpclientsrc",
                    "host=127.0.0.1",
                    &format!("port={host_port}"),
                    "!",
                    "gdpdepay",
                    "!",
                    "queue",
                    "max-size-buffers=8",
                    "leaky=downstream",
                    "!",
                    "pipewiresink",
                    "client-name=Buzzard OS Guest Audio",
                ]);
                if let Some(device) = &device {
                    command.arg(format!("target-object={}", device.serial));
                }
                command.arg("sync=true");
            }
            MediaKind::HostMicrophone => {
                let device = device
                    .as_ref()
                    .context("host microphone selection resolved to no device")?;
                let pulse_server = microphone_pulse_server
                    .as_deref()
                    .context("host microphone recording service was not resolved")?;
                append_microphone_capture_source(&mut command, device, pulse_server);
                command.args([
                    "!",
                    "audioconvert",
                    "!",
                    "audioresample",
                    "!",
                    "audio/x-raw,format=S16LE,rate=48000,channels=2",
                ]);
                append_microphone_forwarding(&mut command, host_port);
            }
            MediaKind::HostCamera => {
                let device = device
                    .as_ref()
                    .context("host camera selection resolved to no device")?;
                append_camera_capture_source(&mut command, device, "Buzzard OS Host Camera")?;
                let prefer_mjpeg = match &device.backend {
                    HostMediaBackend::V4l2 { device } => v4l2_supports_mjpeg(device)?,
                    _ => false,
                };
                append_camera_normalization(&mut command, prefer_mjpeg);
                command.args([
                    "!",
                    "gdppay",
                    "!",
                    "tcpclientsink",
                    "host=127.0.0.1",
                    &format!("port={host_port}"),
                    "sync=false",
                    "async=false",
                ]);
            }
        }
        let broker_pid = unsafe { libc::getpid() };
        // SAFETY: only async-signal-safe libc calls run between fork and exec.
        unsafe {
            use std::os::unix::process::CommandExt;
            command.pre_exec(move || {
                // An abrupt broker death must revoke capture even if the
                // media framework is wedged and would ignore graceful exit.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != broker_pid {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "Buzzard OS broker exited during media-helper startup",
                    ));
                }
                Ok(())
            });
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log))
            .spawn()
            .with_context(|| format!("starting bundled {} bridge", kind.name()))?;
        thread::sleep(Duration::from_millis(150));
        match child.try_wait() {
            Ok(Some(status)) => {
                let detail = fs::read_to_string(&log_path).unwrap_or_default();
                bail!(
                    "{} bridge exited with {status}: {}",
                    kind.name(),
                    detail.trim()
                );
            }
            Ok(None) => {}
            Err(error) => {
                terminate(&mut child);
                return Err(error).context("checking media bridge startup");
            }
        }
        if kind == MediaKind::HostMicrophone {
            let selected = device
                .as_ref()
                .context("host microphone selection resolved to no device")?;
            if let Err(error) =
                wait_for_tracked_microphone(resources, child.id(), selected.node_name.as_str())
            {
                terminate(&mut child);
                return Err(error);
            }
        }
        let slirp_id = pending_mapping.commit();
        let tracking_target = (kind == MediaKind::HostMicrophone)
            .then(|| device.as_ref().map(|device| device.node_name.clone()))
            .flatten();
        Ok(ActiveMedia {
            process: TerminateOnDrop { child },
            slirp_id,
            target,
            resolved_device: device.map(|device| device.description),
            tracking_target,
        })
    }

    fn guest_control<'a>(&self, settings: &'a IntegrationSettings) -> GuestIntegrationControl<'a> {
        let reverse_ports = settings
            .ports
            .iter()
            .filter(|mapping| mapping.enabled && mapping.direction == PortDirection::GuestToHost)
            .collect();
        let forward_udp_ports = settings
            .ports
            .iter()
            .filter(|mapping| {
                mapping.enabled
                    && mapping.direction == PortDirection::HostToGuest
                    && mapping.protocol == PortProtocol::Udp
            })
            .collect();
        GuestIntegrationControl {
            schema: 1,
            generation: self.generation,
            reverse_ports,
            forward_udp_ports,
            media: &settings.media,
        }
    }

    fn write_guest_control(&self, settings: &IntegrationSettings) -> Result<()> {
        atomic_json(&self.control_path, &self.guest_control(settings))
    }

    fn guest_control_matches(&self, settings: &IntegrationSettings) -> bool {
        let Ok(expected) = serde_json::to_value(self.guest_control(settings)) else {
            return false;
        };
        fs::read(&self.control_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|actual| actual == expected)
    }

    fn guest_state_matches(&self, settings: &IntegrationSettings) -> bool {
        if !self.guest_control_matches(settings) {
            return false;
        }
        fs::read(&self.guest_status_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<GuestIntegrationStatus>(&bytes).ok())
            .is_some_and(|status| self.guest_status_matches(settings, &status))
    }

    fn guest_status_matches(
        &self,
        settings: &IntegrationSettings,
        status: &GuestIntegrationStatus,
    ) -> bool {
        if status.schema != 1
            || status.generation != self.generation
            || status
                .error
                .as_deref()
                .is_some_and(|error| !error.is_empty())
        {
            return false;
        }

        let mut wanted_reverse: Vec<_> = settings
            .ports
            .iter()
            .filter(|mapping| mapping.enabled && mapping.direction == PortDirection::GuestToHost)
            .map(|mapping| mapping.id.to_string())
            .collect();
        wanted_reverse.sort();
        let mut wanted_forward_udp: Vec<_> = settings
            .ports
            .iter()
            .filter(|mapping| {
                mapping.enabled
                    && mapping.direction == PortDirection::HostToGuest
                    && mapping.protocol == PortProtocol::Udp
            })
            .map(|mapping| mapping.id.to_string())
            .collect();
        wanted_forward_udp.sort();
        if status.reverse_ports != wanted_reverse || status.forward_udp_ports != wanted_forward_udp
        {
            return false;
        }

        let wanted_media = [
            (MediaKind::GuestAudio, settings.media.guest_audio_output),
            (MediaKind::HostMicrophone, settings.media.host_microphone),
            (MediaKind::HostCamera, settings.media.host_camera),
        ];
        let wanted_count = wanted_media.iter().filter(|(_, enabled)| *enabled).count();
        status.media.len() == wanted_count
            && wanted_media.into_iter().all(|(kind, enabled)| {
                let process = status.media.get(kind.control_name());
                if enabled {
                    process.is_some_and(|process| process.running && process.pid > 0)
                } else {
                    process.is_none()
                }
            })
    }

    pub(crate) fn diagnostics(&self, settings: &IntegrationSettings) -> IntegrationDiagnostics {
        let ports = settings
            .ports
            .iter()
            .map(|mapping| {
                let active = mapping.enabled
                    && match mapping.direction {
                        PortDirection::HostToGuest => self
                            .host_forwards
                            .get(&mapping.id)
                            .is_some_and(|active| match &active.backend {
                                HostForwardBackend::Slirp(_) => true,
                                HostForwardBackend::Udp(relay) => relay.is_active(),
                            }),
                        PortDirection::GuestToHost => self
                            .reverse
                            .get(&mapping.id)
                            .is_some_and(|(_, relay)| relay.is_active()),
                    };
                PortIntegrationDiagnostics {
                    id: mapping.id,
                    direction: mapping.direction,
                    protocol: mapping.protocol,
                    enabled: mapping.enabled,
                    active,
                    detail: if !mapping.enabled {
                        "disabled".into()
                    } else if active {
                        match mapping.direction {
                            PortDirection::HostToGuest => format!(
                                "{}:{} -> {}:{} ({:?})",
                                mapping.host_address,
                                mapping.host_port,
                                mapping.guest_address,
                                mapping.guest_port,
                                mapping.protocol
                            ),
                            PortDirection::GuestToHost => format!(
                                "{}:{} -> {}:{} ({:?}, private relay)",
                                mapping.guest_address,
                                mapping.guest_port,
                                mapping.host_address,
                                mapping.host_port,
                                mapping.protocol
                            ),
                        }
                    } else {
                        "not active".into()
                    },
                }
            })
            .collect();
        let guest = fs::read(&self.guest_status_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<GuestIntegrationStatus>(&bytes).ok())
            .filter(|status| status.schema == 1);
        let guest_generation_matches = guest
            .as_ref()
            .is_some_and(|status| status.generation == self.generation);
        IntegrationDiagnostics {
            schema: 1,
            generation: self.generation,
            ports,
            guest_audio_output: self.media_diagnostic(
                MediaKind::GuestAudio,
                settings.media.guest_audio_output,
                guest.as_ref(),
                guest_generation_matches,
            ),
            host_microphone: self.media_diagnostic(
                MediaKind::HostMicrophone,
                settings.media.host_microphone,
                guest.as_ref(),
                guest_generation_matches,
            ),
            host_camera: self.media_diagnostic(
                MediaKind::HostCamera,
                settings.media.host_camera,
                guest.as_ref(),
                guest_generation_matches,
            ),
        }
    }

    fn media_diagnostic(
        &self,
        kind: MediaKind,
        enabled: bool,
        guest: Option<&GuestIntegrationStatus>,
        guest_generation_matches: bool,
    ) -> MediaIntegrationDiagnostics {
        let host_pid = self
            .media
            .get(&kind)
            .map(|active| active.process.child.id());
        let guest_process = guest.and_then(|status| status.media.get(kind.control_name()));
        let guest_pid = guest_process.map(|process| process.pid);
        let host_running = host_pid.is_some_and(process_is_running);
        let guest_running = guest_process.is_some_and(|process| process.running);
        let active = enabled && guest_generation_matches && host_running && guest_running;
        MediaIntegrationDiagnostics {
            enabled,
            active,
            host_pid,
            guest_pid,
            detail: if !enabled && host_pid.is_none() && guest_pid.is_none() {
                "disabled; no host capture process, mapping, or guest source".into()
            } else if !enabled {
                format!(
                    "disabled, but revocation is incomplete (host bridge PID: {}; guest source PID: {}; guest generation current: {guest_generation_matches})",
                    host_pid
                        .map(|pid| pid.to_string())
                        .unwrap_or_else(|| "none".into()),
                    guest_pid
                        .map(|pid| pid.to_string())
                        .unwrap_or_else(|| "none".into())
                )
            } else if active {
                let device = self
                    .media
                    .get(&kind)
                    .and_then(|media| media.resolved_device.as_deref())
                    .unwrap_or("current host default");
                if kind == MediaKind::HostMicrophone {
                    format!(
                        "{} bridge running for {device}; the host PipeWire-Pulse recording stream is running, uncorked, actively linked to the selected source, and the guest bridge is active",
                        kind.name()
                    )
                } else {
                    format!(
                        "{} bridge processes running for {device}; data flow requires a consumer",
                        kind.name()
                    )
                }
            } else {
                format!("{} requested but not fully active", kind.name())
            },
        }
    }
}

impl Drop for IntegrationRuntime {
    fn drop(&mut self) {
        // Slirp exits with the session and removes all host-forward state. The
        // relay maps drop here, close their listeners, and sever active I/O.
        self.reverse.clear();
        for (_, mut media) in std::mem::take(&mut self.media) {
            terminate(&mut media.process.child);
        }
    }
}

impl MediaKind {
    fn control_name(self) -> &'static str {
        match self {
            Self::GuestAudio => "guest_audio_output",
            Self::HostMicrophone => "host_microphone",
            Self::HostCamera => "host_camera",
        }
    }
}

#[derive(Debug, Deserialize)]
struct GuestIntegrationStatus {
    schema: u32,
    generation: u64,
    #[serde(default)]
    reverse_ports: Vec<String>,
    #[serde(default)]
    forward_udp_ports: Vec<String>,
    #[serde(default)]
    media: BTreeMap<String, GuestMediaProcess>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GuestMediaProcess {
    pid: u32,
    running: bool,
}

fn resolve_media_device(
    kind: MediaKind,
    requested: Option<&str>,
    resources: &ResourceLocator,
) -> Result<Option<HostMediaDevice>> {
    // A null audio target intentionally follows the host's current default;
    // PipeWire can move the playback stream when that default changes. Input
    // devices are resolved so the host broker can open the exact physical
    // backend advertised for the user-selected PipeWire device.
    if kind == MediaKind::GuestAudio && requested.is_none() {
        return Ok(None);
    }
    let host_kind = match kind {
        MediaKind::GuestAudio => HostMediaKind::AudioSink,
        MediaKind::HostMicrophone => HostMediaKind::Microphone,
        MediaKind::HostCamera => HostMediaKind::Camera,
    };
    let devices: Vec<_> = discover_host_media(resources)?
        .into_iter()
        .filter(|device| device.kind == host_kind)
        .collect();
    let selected = if let Some(requested) = requested {
        devices
            .iter()
            .find(|device| device.node_name == requested)
            .cloned()
            .with_context(|| {
                format!(
                    "selected {} device '{requested}' is no longer available",
                    kind.name()
                )
            })?
    } else {
        devices
            .iter()
            .find(|device| device.is_default)
            .or_else(|| devices.first())
            .cloned()
            .with_context(|| format!("the host advertises no usable {} device", kind.name()))?
    };
    Ok(Some(selected))
}

fn append_microphone_capture_source(
    command: &mut Command,
    device: &HostMediaDevice,
    pulse_server: &str,
) {
    // GNOME and other PulseAudio-compatible desktop privacy controls discover
    // microphone use through source-output streams. `pulsesrc` talks to the
    // host's PipeWire-Pulse compatibility service, so capture remains in the
    // host PipeWire graph while acquiring an OS-visible recording identity.
    // Never bypass that accounting path with direct ALSA capture.
    command.args([
        "pulsesrc",
        "client-name=Buzzard OS Microphone",
        &format!("server={pulse_server}"),
        &format!("device={}", device.node_name),
        "do-timestamp=true",
        "buffer-time=40000",
        "latency-time=10000",
        &format!(
            "stream-properties=props,application.id={MICROPHONE_APPLICATION_ID},application.name=WildBuzzard,media.role=communication"
        ),
    ]);
}

fn append_microphone_forwarding(command: &mut Command, host_port: u16) {
    // Enabling microphone sharing means capture is active for the entire
    // enabled interval, even when no guest application currently has the
    // virtual microphone open. The drain branch continuously consumes samples
    // whenever the host source supplies them, while the Pulse source-output
    // remains registered for the host desktop's microphone/privacy UI even if
    // the hardware transport is idle or suspended. The bounded leaky
    // forwarding branch cannot stall capture when the guest virtual source has
    // no consumer; once consumed, it receives the newest live samples instead
    // of buffered historical audio.
    command.args([
        "!",
        "tee",
        "name=wb_microphone",
        "wb_microphone.",
        "!",
        "queue",
        "max-size-buffers=8",
        "max-size-bytes=0",
        "max-size-time=0",
        "leaky=downstream",
        "!",
        "fakesink",
        "sync=false",
        "async=false",
        "wb_microphone.",
        "!",
        "queue",
        "max-size-buffers=32",
        "max-size-bytes=0",
        "max-size-time=0",
        "leaky=downstream",
        "!",
        "gdppay",
        "!",
        "tcpclientsink",
        "host=127.0.0.1",
        &format!("port={host_port}"),
        "sync=false",
        "async=false",
    ]);
}

fn append_camera_capture_source(
    command: &mut Command,
    device: &HostMediaDevice,
    client_name: &str,
) -> Result<()> {
    match &device.backend {
        HostMediaBackend::V4l2 { device } => {
            let device = device
                .to_str()
                .context("host camera device path is not UTF-8")?;
            command.args(["v4l2src", &format!("device={device}"), "do-timestamp=true"]);
        }
        HostMediaBackend::PipeWire => {
            command.args([
                "pipewiresrc",
                &format!("client-name={client_name}"),
                "do-timestamp=true",
                &format!("target-object={}", device.serial),
            ]);
        }
        HostMediaBackend::Alsa { .. } => {
            bail!("selected camera unexpectedly resolved to an audio backend");
        }
    }
    Ok(())
}

fn wait_for_tracked_microphone(
    resources: &ResourceLocator,
    process_id: u32,
    target: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_error = None;
    loop {
        match read_host_pipewire_graph(resources) {
            Ok(graph) if pipewire_graph_has_tracked_microphone(&graph, process_id, target) => {
                return Ok(());
            }
            Ok(_) => {}
            Err(error) => last_error = Some(format!("{error:#}")),
        }
        if Instant::now() >= deadline {
            let suffix = last_error
                .map(|error| format!(" ({error})"))
                .unwrap_or_default();
            bail!(
                "host microphone capture did not become a running, desktop-visible recording stream{suffix}"
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn read_host_pipewire_graph(resources: &ResourceLocator) -> Result<Value> {
    let pw_dump = resources.helper_or_path("pw-dump")?;
    let output = Command::new(&pw_dump)
        .output()
        .with_context(|| format!("running {}", pw_dump.display()))?;
    if !output.status.success() {
        bail!("pw-dump exited with {}", output.status);
    }
    if output.stdout.len() > MAX_PIPEWIRE_DUMP_BYTES {
        bail!("host PipeWire graph exceeded 32 MiB while verifying microphone use");
    }
    serde_json::from_slice(&output.stdout).context("parsing host PipeWire graph")
}

fn microphone_bridge_needs_restart(
    process_exited: bool,
    graph: &Value,
    process_id: u32,
    target: Option<&str>,
) -> bool {
    process_exited
        || !target
            .is_some_and(|target| pipewire_graph_has_tracked_microphone(graph, process_id, target))
}

fn pipewire_graph_has_tracked_microphone(graph: &Value, process_id: u32, target: &str) -> bool {
    let Some(objects) = graph.as_array() else {
        return false;
    };
    let Some(target_node_id) = objects.iter().find_map(|object| {
        (object.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Node")
            && object
                .pointer("/info/props/node.name")
                .and_then(Value::as_str)
                == Some(target)
            && object
                .pointer("/info/props/media.class")
                .and_then(Value::as_str)
                == Some("Audio/Source"))
        .then(|| object.get("id").and_then(value_u64))
        .flatten()
    }) else {
        return false;
    };

    objects.iter().any(|stream| {
        let tracked_stream = stream.get("type").and_then(Value::as_str)
            == Some("PipeWire:Interface:Node")
            // A merely registered or suspended Pulse source-output is not
            // proof of capture. In particular, pulsesrc creates the object
            // before format negotiation; the old check could report success
            // while it remained corked and later timed out without producing
            // a sample. Require the transport state that represents actual
            // capture, while allowing Pulse servers that omit pulse.corked.
            && stream.pointer("/info/state").and_then(Value::as_str) == Some("running")
            && stream
                .pointer("/info/props/pulse.corked")
                .and_then(value_bool)
                != Some(true)
            && stream
                .pointer("/info/props/media.class")
                .and_then(Value::as_str)
                == Some("Stream/Input/Audio")
            && stream
                .pointer("/info/props/client.api")
                .and_then(Value::as_str)
                == Some("pipewire-pulse")
            && stream
                .pointer("/info/props/application.id")
                .and_then(Value::as_str)
                == Some(MICROPHONE_APPLICATION_ID)
            && stream
                .pointer("/info/props/application.process.id")
                .and_then(value_u64)
                == Some(u64::from(process_id))
            && stream
                .pointer("/info/props/target.object")
                .and_then(Value::as_str)
                == Some(target);
        if !tracked_stream {
            return false;
        }
        let Some(stream_node_id) = stream.get("id").and_then(value_u64) else {
            return false;
        };

        // Prove that the selected physical source is actively linked to this
        // exact Pulse recording stream. A same-named orphan node elsewhere in
        // the graph must not satisfy host privacy/accounting diagnostics.
        objects.iter().any(|link| {
            link.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Link")
                && link.pointer("/info/state").and_then(Value::as_str) == Some("active")
                && link.pointer("/info/output-node-id").and_then(value_u64) == Some(target_node_id)
                && link.pointer("/info/input-node-id").and_then(value_u64) == Some(stream_node_id)
        })
    })
}

fn value_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn value_bool(value: &Value) -> Option<bool> {
    value.as_bool().or_else(|| {
        value.as_str().and_then(|value| match value {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        })
    })
}

fn append_camera_normalization(command: &mut Command, prefer_mjpeg: bool) {
    if prefer_mjpeg {
        command.args(["!", "image/jpeg", "!", "jpegdec"]);
    }
    command.args([
        "!",
        "videoconvert",
        "!",
        "videoscale",
        "!",
        "video/x-raw,format=BGRA,width=640,height=480",
    ]);
}

fn v4l2_supports_mjpeg(device: &Path) -> Result<bool> {
    let file = fs::File::open(device)
        .with_context(|| format!("opening selected camera {}", device.display()))?;
    for index in 0..256_u32 {
        let mut format = V4l2FormatDescription {
            index,
            buffer_type: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            ..V4l2FormatDescription::default()
        };
        // SAFETY: `format` has the exact Linux `v4l2_fmtdesc` C layout and
        // remains valid and exclusively borrowed for the duration of ioctl.
        let result = unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_ENUM_FMT, &mut format) };
        if result == 0 {
            if format.pixel_format == V4L2_PIX_FMT_MJPEG {
                return Ok(true);
            }
            continue;
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINVAL) {
            return Ok(false);
        }
        return Err(error)
            .with_context(|| format!("enumerating formats for camera {}", device.display()));
    }
    bail!(
        "camera {} advertised more than 256 V4L2 formats",
        device.display()
    )
}

fn require_host_pipewire() -> Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })));
    let socket = runtime.join("pipewire-0");
    let metadata = fs::symlink_metadata(&socket).with_context(|| {
        format!(
            "host PipeWire service is not available at {}",
            socket.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
        bail!(
            "host PipeWire endpoint {} is not a real Unix socket",
            socket.display()
        );
    }
    Ok(())
}

fn require_host_recording_service() -> Result<String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })));
    let socket = runtime.join("pulse/native");
    let metadata = fs::symlink_metadata(&socket).with_context(|| {
        format!(
            "host PipeWire-Pulse recording service is not available at {}",
            socket.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
        bail!(
            "host PipeWire-Pulse endpoint {} is not a real Unix socket",
            socket.display()
        );
    }
    Ok(format!("unix:{}", socket.display()))
}

fn unused_loopback_port() -> Result<u16> {
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).context("allocating a private media bridge port")?;
    Ok(listener
        .local_addr()
        .context("reading private media bridge port")?
        .port())
}

fn process_is_running(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("integration state has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating integration state in {}", parent.display()))?;
    serde_json::to_writer_pretty(&mut temporary, value).context("serializing integration state")?;
    temporary
        .write_all(b"\n")
        .context("finishing integration state")?;
    temporary
        .as_file()
        .sync_all()
        .context("syncing integration state")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("saving {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("setting permissions on {}", path.display()))
}

// `File::from_raw_fd` is intentionally kept close to the one ownership
// transfer in `SlirpRuntime::start`.
use std::os::fd::FromRawFd;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn tracked_microphone_graph() -> Value {
        json!([
            {
                "id": 42,
                "type": "PipeWire:Interface:Node",
                "info": {
                    "state": "running",
                    "props": {
                        "media.class": "Audio/Source",
                        "node.name": "mic.one"
                    }
                }
            },
            {
                "id": "84",
                "type": "PipeWire:Interface:Node",
                "info": {
                    "state": "running",
                    "props": {
                        "media.class": "Stream/Input/Audio",
                        "client.api": "pipewire-pulse",
                        "pulse.corked": false,
                        "application.id": MICROPHONE_APPLICATION_ID,
                        "application.process.id": "90210",
                        "target.object": "mic.one"
                    }
                }
            },
            {
                "type": "PipeWire:Interface:Link",
                "info": {
                    "state": "active",
                    "output-node-id": 42,
                    "input-node-id": "84"
                }
            }
        ])
    }

    #[test]
    fn microphone_capture_is_host_session_tracked_and_never_direct_alsa() {
        let device = HostMediaDevice {
            node_name: "stable-host-node".into(),
            description: "Host microphone".into(),
            serial: "2047".into(),
            kind: HostMediaKind::Microphone,
            backend: HostMediaBackend::Alsa {
                device: "hw:camera-test,6".into(),
            },
            is_default: true,
        };
        let mut command = Command::new("gst-launch-1.0");
        append_microphone_capture_source(&mut command, &device, "unix:/run/user/1000/pulse/native");
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(arguments[0], "pulsesrc");
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "device=stable-host-node")
        );
        assert!(arguments.iter().any(|argument| {
            argument.contains("application.id=org.openresearchtools.WildBuzzard")
        }));
        assert!(!arguments.iter().any(|argument| argument == "alsasrc"));
        assert!(!arguments.iter().any(|argument| argument.contains("hw:")));
    }

    #[test]
    fn microphone_capture_remains_active_without_a_guest_consumer() {
        let mut command = Command::new("gst-launch-1.0");
        append_microphone_forwarding(&mut command, 47_131);
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert!(arguments.iter().any(|argument| argument == "tee"));
        assert!(arguments.iter().any(|argument| argument == "fakesink"));
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| argument.as_str() == "wb_microphone.")
                .count(),
            2
        );
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| argument.as_str() == "leaky=downstream")
                .count(),
            2
        );
        assert!(arguments.iter().any(|argument| argument == "gdppay"));
        assert!(arguments.iter().any(|argument| argument == "port=47131"));
    }

    #[test]
    fn camera_capture_uses_selected_physical_backend() {
        let device = HostMediaDevice {
            node_name: "stable-host-node".into(),
            description: "Host camera".into(),
            serial: "2047".into(),
            kind: HostMediaKind::Camera,
            backend: HostMediaBackend::V4l2 {
                device: PathBuf::from("/dev/video-test"),
            },
            is_default: true,
        };
        let mut command = Command::new("gst-launch-1.0");
        append_camera_capture_source(&mut command, &device, "Buzzard OS Capture").unwrap();
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(arguments[0], "v4l2src");
        assert!(!arguments.iter().any(|argument| argument == "pipewiresrc"));
    }

    #[test]
    fn tracked_microphone_matches_desktop_visible_pipewire_pulse_source_output() {
        let graph = tracked_microphone_graph();
        assert!(pipewire_graph_has_tracked_microphone(
            &graph, 90210, "mic.one"
        ));
        assert!(!microphone_bridge_needs_restart(
            false,
            &graph,
            90210,
            Some("mic.one")
        ));
        assert!(microphone_bridge_needs_restart(
            true,
            &graph,
            90210,
            Some("mic.one")
        ));
        assert!(microphone_bridge_needs_restart(false, &graph, 90210, None));
        assert!(!pipewire_graph_has_tracked_microphone(
            &graph, 90211, "mic.one"
        ));
        assert!(!pipewire_graph_has_tracked_microphone(
            &graph, 90210, "mic.two"
        ));

        let mut untracked = graph.clone();
        untracked[1]["info"]["props"]["client.api"] = json!("pipewire");
        assert!(!pipewire_graph_has_tracked_microphone(
            &untracked, 90210, "mic.one"
        ));

        let mut wrong_source_class = graph.clone();
        wrong_source_class[0]["info"]["props"]["media.class"] = json!("Stream/Output/Audio");
        assert!(microphone_bridge_needs_restart(
            false,
            &wrong_source_class,
            90210,
            Some("mic.one")
        ));
    }

    #[test]
    fn living_suspended_microphone_bridge_requires_restart() {
        let mut graph = tracked_microphone_graph();
        graph[1]["info"]["state"] = json!("suspended");
        assert!(microphone_bridge_needs_restart(
            false,
            &graph,
            90210,
            Some("mic.one")
        ));
    }

    #[test]
    fn living_corked_microphone_bridge_requires_restart() {
        let mut graph = tracked_microphone_graph();
        graph[1]["info"]["props"]["pulse.corked"] = json!(true);
        assert!(microphone_bridge_needs_restart(
            false,
            &graph,
            90210,
            Some("mic.one")
        ));

        graph[1]["info"]["props"]["pulse.corked"] = json!("true");
        assert!(microphone_bridge_needs_restart(
            false,
            &graph,
            90210,
            Some("mic.one")
        ));
    }

    #[test]
    fn living_unlinked_microphone_bridge_requires_restart() {
        let mut graph = tracked_microphone_graph();
        graph[2]["info"]["state"] = json!("init");
        assert!(microphone_bridge_needs_restart(
            false,
            &graph,
            90210,
            Some("mic.one")
        ));

        graph[2]["info"]["state"] = json!("active");
        graph[2]["info"]["output-node-id"] = json!(7);
        assert!(microphone_bridge_needs_restart(
            false,
            &graph,
            90210,
            Some("mic.one")
        ));
    }

    #[test]
    fn failed_mapping_removal_stops_capture_and_private_network() {
        let runtime_dir = tempfile::tempdir().unwrap();
        let display_state = tempfile::tempdir().unwrap();
        let host_status = tempfile::tempdir().unwrap();
        let mut runtime =
            IntegrationRuntime::new(runtime_dir.path(), display_state.path(), host_status.path())
                .unwrap();
        let media_child = Command::new("sleep").arg("30").spawn().unwrap();
        runtime.media.insert(
            MediaKind::HostMicrophone,
            ActiveMedia {
                process: TerminateOnDrop { child: media_child },
                slirp_id: 73,
                target: Some("mic.one".into()),
                resolved_device: Some("Test microphone".into()),
                tracking_target: Some("mic.one".into()),
            },
        );
        let slirp_child = Command::new("sleep").arg("30").spawn().unwrap();
        let mut slirp = SlirpRuntime {
            process: TerminateOnDrop { child: slirp_child },
            api_socket: runtime_dir.path().join("missing-slirp-api.sock"),
        };

        let error = runtime
            .stop_removed_media(&wb_core::MediaSharing::default(), Some(&slirp))
            .unwrap_err();
        assert!(error.to_string().contains("terminated fail-closed"));
        let active = runtime
            .media
            .get_mut(&MediaKind::HostMicrophone)
            .expect("mapping ID must remain available for the next revocation retry");
        assert!(active.process.child.try_wait().unwrap().is_some());
        assert_eq!(active.slirp_id, 73);
        assert!(!slirp.process.child.wait().unwrap().success());
    }

    #[test]
    fn camera_normalization_prefers_advertised_mjpeg_without_vendor_rules() {
        let mut command = Command::new("gst-launch-1.0");
        append_camera_normalization(&mut command, true);
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(arguments[0..4], ["!", "image/jpeg", "!", "jpegdec"]);
        assert!(arguments.iter().any(|argument| argument == "videoscale"));
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "video/x-raw,format=BGRA,width=640,height=480")
        );

        let mut raw_command = Command::new("gst-launch-1.0");
        append_camera_normalization(&mut raw_command, false);
        assert!(!raw_command.get_args().any(|argument| argument == "jpegdec"));
    }

    #[test]
    fn reverse_tcp_relay_reaches_only_configured_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let target_port = target.local_addr().unwrap().port();
        let mapping = PortForward {
            id: Uuid::new_v4(),
            enabled: true,
            direction: PortDirection::GuestToHost,
            protocol: PortProtocol::Tcp,
            host_address: "127.0.0.1".into(),
            host_port: target_port,
            guest_address: "127.0.0.1".into(),
            guest_port: 9000,
        };
        let relay = ReverseTcpRelay::start(&mapping, temporary.path()).unwrap();
        let mut guest = UnixStream::connect(&relay.socket_path).unwrap();
        guest.write_all(b"machine-only").unwrap();
        let (mut accepted, _) = target.accept().unwrap();
        let mut payload = [0_u8; 12];
        accepted.read_exact(&mut payload).unwrap();
        assert_eq!(&payload, b"machine-only");
        accepted.write_all(b"ok").unwrap();
        let mut reply = [0_u8; 2];
        guest.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"ok");
        drop(relay);
        assert!(
            !temporary
                .path()
                .join(format!("reverse-{}.sock", mapping.id))
                .exists()
        );
    }

    #[test]
    fn reverse_udp_relay_round_trips_with_per_guest_token() {
        let temporary = tempfile::tempdir().unwrap();
        let target = UdpSocket::bind("127.0.0.1:0").unwrap();
        target
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mapping = PortForward {
            id: Uuid::new_v4(),
            enabled: true,
            direction: PortDirection::GuestToHost,
            protocol: PortProtocol::Udp,
            host_address: "127.0.0.1".into(),
            host_port: target.local_addr().unwrap().port(),
            guest_address: "127.0.0.1".into(),
            guest_port: 9001,
        };
        let relay = ReverseUdpRelay::start(&mapping, temporary.path()).unwrap();
        let client_path = temporary
            .path()
            .join(format!("reverse-client-{}.sock", mapping.id));
        let client = UnixDatagram::bind(&client_path).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let token = 42_u64;
        let mut request = token.to_be_bytes().to_vec();
        request.extend_from_slice(b"datagram");
        client.send_to(&request, &relay.socket_path).unwrap();
        let mut payload = [0_u8; 64];
        let (length, peer) = target.recv_from(&mut payload).unwrap();
        assert_eq!(&payload[..length], b"datagram");
        target.send_to(b"reply", peer).unwrap();
        let length = client.recv(&mut payload).unwrap();
        assert_eq!(u64::from_be_bytes(payload[..8].try_into().unwrap()), token);
        assert_eq!(&payload[8..length], b"reply");
        drop(relay);
        assert!(
            !temporary
                .path()
                .join(format!("reverse-{}.sock", mapping.id))
                .exists()
        );
    }

    #[test]
    fn host_udp_relay_round_trips_without_slirp_udp_nat() {
        let temporary = tempfile::tempdir().unwrap();
        let reservation = UdpSocket::bind("127.0.0.1:0").unwrap();
        let host_port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let mapping = PortForward {
            id: Uuid::new_v4(),
            enabled: true,
            direction: PortDirection::HostToGuest,
            protocol: PortProtocol::Udp,
            host_address: "127.0.0.1".into(),
            host_port,
            guest_address: "10.0.2.100".into(),
            guest_port: 9002,
        };
        let relay = HostUdpRelay::start(&mapping, temporary.path()).unwrap();
        let guest_path = temporary
            .path()
            .join(format!("forward-client-{}.sock", mapping.id));
        let guest = UnixDatagram::bind(&guest_path).unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        host.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        host.send_to(b"request", ("127.0.0.1", host_port)).unwrap();
        let mut packet = [0_u8; 64];
        let length = guest.recv(&mut packet).unwrap();
        assert_eq!(&packet[8..length], b"request");
        let mut response = packet[..8].to_vec();
        response.extend_from_slice(b"response");
        guest.send_to(&response, &relay.socket_path).unwrap();
        let length = host.recv(&mut packet).unwrap();
        assert_eq!(&packet[..length], b"response");
        drop(relay);
        assert!(
            !temporary
                .path()
                .join(format!("forward-{}.sock", mapping.id))
                .exists()
        );
    }

    #[test]
    fn guest_control_contains_no_host_pipewire_socket() {
        let temporary = tempfile::tempdir().unwrap();
        let display = tempfile::tempdir().unwrap();
        let runtime =
            IntegrationRuntime::new(temporary.path(), display.path(), display.path()).unwrap();
        runtime
            .write_guest_control(&IntegrationSettings::default())
            .unwrap();
        let value = fs::read_to_string(display.path().join("integration.json")).unwrap();
        assert!(!value.contains("pipewire-0"));
        assert!(!value.contains("XDG_RUNTIME_DIR"));
    }

    #[test]
    fn disabled_media_repairs_stale_guest_control_and_waits_for_guest_revocation() {
        let runtime_dir = tempfile::tempdir().unwrap();
        let display_state = tempfile::tempdir().unwrap();
        let host_status = tempfile::tempdir().unwrap();
        let mut runtime =
            IntegrationRuntime::new(runtime_dir.path(), display_state.path(), host_status.path())
                .unwrap();
        runtime.generation = 12;

        let requested = IntegrationSettings::default();
        runtime.applied = requested.clone();
        let mut stale = requested.clone();
        stale.media.host_microphone = true;
        runtime.write_guest_control(&stale).unwrap();
        atomic_json(
            &runtime.guest_status_path,
            &json!({
                "schema": 1,
                "generation": 12,
                "reverse_ports": [],
                "forward_udp_ports": [],
                "media": {
                    "host_microphone": { "pid": 394, "running": true }
                },
                "error": null
            }),
        )
        .unwrap();

        assert!(!runtime.guest_state_matches(&requested));
        let stale_diagnostics = runtime.diagnostics(&requested);
        assert_eq!(stale_diagnostics.host_microphone.guest_pid, Some(394));
        assert!(!stale_diagnostics.host_microphone.active);
        assert!(
            stale_diagnostics
                .host_microphone
                .detail
                .contains("revocation is incomplete")
        );
        assert!(
            !stale_diagnostics
                .host_microphone
                .detail
                .contains("no host capture process, mapping, or guest source")
        );

        let control_path = runtime.control_path.clone();
        let status_path = runtime.guest_status_path.clone();
        let simulated_guest = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let control = fs::read(&control_path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
                if let Some(control) = control
                    && control["generation"]
                        .as_u64()
                        .is_some_and(|value| value > 12)
                    && control["media"]["host_microphone"] == json!(false)
                {
                    let generation = control["generation"].as_u64().unwrap();
                    atomic_json(
                        &status_path,
                        &json!({
                            "schema": 1,
                            "generation": generation,
                            "reverse_ports": [],
                            "forward_udp_ports": [],
                            "media": {},
                            "error": null
                        }),
                    )
                    .unwrap();
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "broker did not publish a repaired disabled control generation"
                );
                thread::sleep(Duration::from_millis(10));
            }
        });

        let resources = ResourceLocator::discover().unwrap();
        let diagnostics = runtime
            .reconcile(&requested, None, &resources)
            .expect("stale guest control should be repaired");
        simulated_guest.join().unwrap();

        assert_eq!(runtime.generation, 13);
        assert!(runtime.guest_state_matches(&requested));
        assert_eq!(diagnostics.host_microphone.guest_pid, None);
        assert_eq!(
            diagnostics.host_microphone.detail,
            "disabled; no host capture process, mapping, or guest source"
        );
    }
}
