// SPDX-License-Identifier: AGPL-3.0-or-later

//! Guest-private endpoint for explicit, host-authorized clipboard snapshots.
//!
//! The endpoint can only put one already-authorized value into Sway's regular
//! clipboard, get one snapshot from that clipboard, or probe readiness. It has
//! no route to the host clipboard and accepts no commands, paths, or generic
//! payload types.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, BufReader, Cursor, Read, Write};
use std::mem::{self, MaybeUninit};
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use buzzardos_clipboard_protocol::{
    Frame, IO_TIMEOUT_SECONDS, Kind, MAX_IMAGE_BYTES, MAX_IMAGE_DIMENSION, MAX_IMAGE_PIXELS,
    MAX_TEXT_BYTES, Mime, PNG_MIME, ProtocolError, Status, TEXT_MIME, read_frame, write_frame,
};
use image::codecs::png::{PngDecoder, PngEncoder};
use image::codecs::webp::WebPDecoder;
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Limits};
use rustix::fs::{Mode, OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
use wl_clipboard_rs::{copy, paste, utils};
use zeroize::Zeroize;

const RUNTIME_DIRECTORY: &str = "/run/buzzardos-host";
const SOCKET_PATH: &str = "/run/buzzardos-host/clipboard-agent.sock";
const READY_PATH: &str = "/run/buzzardos-host/clipboard-ready";
const PRE_REQUEST_IDLE_SECONDS: u64 = IO_TIMEOUT_SECONDS + 1;
const MAX_MIME_OFFERS: usize = 128;
const MAX_MIME_METADATA_BYTES: usize = 64 * 1024;
const IMAGE_DECODE_BYTES: u64 = 512 * 1024 * 1024;
const PIPE_CHUNK_BYTES: usize = 64 * 1024;
const WORKER_ADDRESS_SPACE_BYTES: u64 = 768 * 1024 * 1024;
const WORKER_CPU_SECONDS: u64 = IO_TIMEOUT_SECONDS + 1;
const WORKER_OPEN_FILES: u64 = 64;
const WORKER_NONCE: [u8; 16] = *b"WB-CLIP-WORKER1!";
const WORKER_ENVIRONMENT: &str = "BUZZARDOS_CLIPBOARD_INTERNAL_WORKER";
const WORKER_ENVIRONMENT_VALUE: &str = "fixed-v1";
const SOCKET_MODE: u32 = 0o666;
const READY_MODE: u32 = 0o644;

/// Exact private mode used only by the parent clipboard agent. It accepts one
/// fixed clipboard-protocol frame over stdin; it is not a command, path, or
/// generic RPC interface.
pub const INTERNAL_WORKER_ARGUMENT: &str = "--buzzardos-internal-clipboard-worker-v1";

const SUPPORTED_IMAGE_FORMATS: [ImageFormat; 5] = [
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::WebP,
    ImageFormat::Bmp,
    ImageFormat::Tiff,
];

#[derive(Clone, Copy, Debug)]
struct Failure {
    status: Status,
    category: &'static str,
}

impl Failure {
    const fn new(status: Status, category: &'static str) -> Self {
        Self { status, category }
    }

    const fn timeout() -> Self {
        Self::new(Status::Timeout, "deadline_exceeded")
    }
}

/// Startup failures deliberately expose only a fixed, content-free category.
pub struct RunError {
    category: &'static str,
}

impl RunError {
    const fn new(category: &'static str) -> Self {
        Self { category }
    }

    pub const fn category(&self) -> &'static str {
        self.category
    }
}

struct SecretVec(Vec<u8>);

impl SecretVec {
    fn with_capacity(capacity: usize) -> Result<Self, Failure> {
        let mut value = Vec::new();
        value
            .try_reserve(capacity)
            .map_err(|_| Failure::new(Status::Internal, "memory_allocation"))?;
        Ok(Self(value))
    }

    fn take(&mut self) -> Vec<u8> {
        mem::take(&mut self.0)
    }
}

impl From<Vec<u8>> for SecretVec {
    fn from(value: Vec<u8>) -> Self {
        Self(value)
    }
}

impl Drop for SecretVec {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

struct SecretFrame(Frame);

impl SecretFrame {
    fn take_payload(&mut self) -> Vec<u8> {
        mem::take(&mut self.0.payload)
    }
}

impl Drop for SecretFrame {
    fn drop(&mut self) {
        self.0.payload.zeroize();
        self.0.nonce.zeroize();
    }
}

struct SecretImage(DynamicImage);

impl Drop for SecretImage {
    fn drop(&mut self) {
        zero_dynamic_image(&mut self.0);
    }
}

struct Deadline {
    expires: Instant,
}

impl Deadline {
    fn new() -> Self {
        Self::after(Duration::from_secs(IO_TIMEOUT_SECONDS))
    }

    fn after(duration: Duration) -> Self {
        Self {
            expires: Instant::now() + duration,
        }
    }

    fn check(&self) -> Result<(), Failure> {
        if Instant::now() >= self.expires {
            Err(Failure::timeout())
        } else {
            Ok(())
        }
    }

    fn remaining_io(&self) -> io::Result<Duration> {
        self.expires
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "clipboard deadline expired"))
    }
}

struct DeadlineStream<'a> {
    stream: &'a UnixStream,
    deadline: &'a Deadline,
}

impl Read for DeadlineStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream
            .set_read_timeout(Some(self.deadline.remaining_io()?))?;
        self.stream.read(buffer)
    }
}

impl Write for DeadlineStream<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream
            .set_write_timeout(Some(self.deadline.remaining_io()?))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream
            .set_write_timeout(Some(self.deadline.remaining_io()?))?;
        self.stream.flush()
    }
}

#[derive(Clone, Copy)]
enum PathKind {
    Socket,
    File,
}

struct OwnedRuntimePath {
    path: PathBuf,
    device: u64,
    inode: u64,
    kind: PathKind,
}

impl OwnedRuntimePath {
    fn capture(path: &Path, kind: PathKind) -> Result<Self, RunError> {
        let metadata =
            fs::symlink_metadata(path).map_err(|_| RunError::new("runtime_path_inspection"))?;
        let type_matches = match kind {
            PathKind::Socket => metadata.file_type().is_socket(),
            PathKind::File => metadata.file_type().is_file(),
        };
        if metadata.file_type().is_symlink() || !type_matches {
            return Err(RunError::new("runtime_path_type"));
        }
        Ok(Self {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
            kind,
        })
    }

    fn still_owned(&self) -> bool {
        fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            let type_matches = match self.kind {
                PathKind::Socket => metadata.file_type().is_socket(),
                PathKind::File => metadata.file_type().is_file(),
            };
            !metadata.file_type().is_symlink()
                && type_matches
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        })
    }
}

impl Drop for OwnedRuntimePath {
    fn drop(&mut self) {
        if self.still_owned() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct Endpoint {
    listener: UnixListener,
    _socket_path: OwnedRuntimePath,
    ready_path: Option<OwnedRuntimePath>,
}

impl Endpoint {
    fn bind() -> Result<Self, RunError> {
        let runtime = Path::new(RUNTIME_DIRECTORY);
        let metadata = fs::symlink_metadata(runtime)
            .map_err(|_| RunError::new("runtime_directory_missing"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(RunError::new("runtime_directory_type"));
        }
        require_absent(Path::new(SOCKET_PATH))?;
        require_absent(Path::new(READY_PATH))?;

        // The enclosing host runtime directory is 0700.  Use a connectable
        // socket inside it so the host desktop user can reach this guest-owned
        // endpoint with every stock Podman user-namespace mapping, including
        // the default subordinate-ID mapping and keep-id.  The private parent
        // remains the authorization boundary and avoids a chmod-by-path race.
        rustix::process::umask(Mode::from_raw_mode((!SOCKET_MODE) & 0o777));
        let listener = UnixListener::bind(SOCKET_PATH).map_err(|_| RunError::new("socket_bind"))?;
        ensure_cloexec(&listener).map_err(|_| RunError::new("socket_cloexec"))?;
        let socket_path = OwnedRuntimePath::capture(Path::new(SOCKET_PATH), PathKind::Socket)?;
        let socket_metadata =
            fs::symlink_metadata(SOCKET_PATH).map_err(|_| RunError::new("socket_inspection"))?;
        if socket_metadata.mode() & 0o777 != SOCKET_MODE {
            return Err(RunError::new("socket_permissions"));
        }
        Ok(Self {
            listener,
            _socket_path: socket_path,
            ready_path: None,
        })
    }

    fn publish_ready(&mut self) -> Result<(), RunError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(READY_MODE)
            .open(READY_PATH)
            .map_err(|_| RunError::new("readiness_create"))?;
        ensure_cloexec(&file).map_err(|_| RunError::new("readiness_cloexec"))?;
        file.sync_all()
            .map_err(|_| RunError::new("readiness_sync"))?;
        drop(file);
        self.ready_path = Some(OwnedRuntimePath::capture(
            Path::new(READY_PATH),
            PathKind::File,
        )?);
        Ok(())
    }
}

fn require_absent(path: &Path) -> Result<(), RunError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(RunError::new("runtime_path_inspection")),
        Ok(_) => Err(RunError::new("runtime_path_exists")),
    }
}

fn ensure_cloexec(descriptor: &impl AsFd) -> io::Result<()> {
    let flags = fcntl_getfd(descriptor).map_err(io::Error::from)?;
    fcntl_setfd(descriptor, flags | FdFlags::CLOEXEC).map_err(io::Error::from)
}

trait ClipboardBackend {
    fn probe(&mut self, deadline: &Deadline) -> Result<(), Failure>;
    fn put(&mut self, mime: Mime, payload: Vec<u8>, deadline: &Deadline) -> Result<usize, Failure>;
    fn get(&mut self, deadline: &Deadline) -> Result<(Mime, Vec<u8>), Failure>;
}

struct WaylandClipboard {
    owner: Option<PersistentOwner>,
}

impl WaylandClipboard {
    fn new() -> Self {
        Self { owner: None }
    }

    fn reap_finished_owner(&mut self) {
        let finished = self.owner.as_mut().is_some_and(|owner| !owner.is_running());
        if finished {
            self.owner = None;
        }
    }
}

impl ClipboardBackend for WaylandClipboard {
    fn probe(&mut self, deadline: &Deadline) -> Result<(), Failure> {
        self.reap_finished_owner();
        let request = SecretFrame(Frame::probe(WORKER_NONCE));
        let (response, _worker) = WorkerProcess::spawn()?.exchange(request, deadline)?;
        validate_worker_response(&response, Kind::ProbeResult, Mime::None).map(drop)
    }

    fn put(&mut self, mime: Mime, payload: Vec<u8>, deadline: &Deadline) -> Result<usize, Failure> {
        let mut source = SecretVec::from(payload);
        if !matches!(mime, Mime::Text | Mime::Png) {
            return Err(Failure::new(
                Status::UnsupportedMime,
                "unsupported_wire_mime",
            ));
        }
        if source.0.len() > mime.payload_limit() {
            return Err(Failure::new(Status::TooLarge, "worker_request_too_large"));
        }
        let byte_count = source.0.len();
        let request = SecretFrame(Frame {
            kind: Kind::Put,
            mime,
            status: Status::Ok,
            nonce: WORKER_NONCE,
            payload: source.take(),
        });
        let (response, worker) = WorkerProcess::spawn()?.exchange(request, deadline)?;
        validate_worker_response(&response, Kind::PutResult, Mime::None)?;
        self.owner = Some(PersistentOwner { worker });
        Ok(byte_count)
    }

    fn get(&mut self, deadline: &Deadline) -> Result<(Mime, Vec<u8>), Failure> {
        self.reap_finished_owner();
        let request = SecretFrame(Frame::get(WORKER_NONCE));
        let (mut response, _worker) = WorkerProcess::spawn()?.exchange(request, deadline)?;
        validate_worker_response(&response, Kind::GetResult, response.0.mime)?;
        if !matches!(response.0.mime, Mime::Text | Mime::Png) {
            return Err(Failure::new(
                Status::UnsupportedMime,
                "worker_response_mime",
            ));
        }
        let mime = response.0.mime;
        Ok((mime, response.take_payload()))
    }
}

struct DeadlineFd<'a, T> {
    io: &'a mut T,
    deadline: &'a Deadline,
}

impl<T: Read> Read for DeadlineFd<'_, T> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            self.deadline.remaining_io()?;
            match self.io.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = self.deadline.remaining_io()?;
                    thread::sleep(remaining.min(Duration::from_millis(5)));
                }
                result => return result,
            }
        }
    }
}

impl<T: Write> Write for DeadlineFd<'_, T> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        loop {
            self.deadline.remaining_io()?;
            match self.io.write(buffer) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = self.deadline.remaining_io()?;
                    thread::sleep(remaining.min(Duration::from_millis(5)));
                }
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        loop {
            self.deadline.remaining_io()?;
            match self.io.flush() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = self.deadline.remaining_io()?;
                    thread::sleep(remaining.min(Duration::from_millis(5)));
                }
                result => return result,
            }
        }
    }
}

fn set_nonblocking(descriptor: &impl AsFd) -> Result<(), Failure> {
    let flags =
        fcntl_getfl(descriptor).map_err(|_| Failure::new(Status::Internal, "pipe_flags"))?;
    fcntl_setfl(descriptor, flags | OFlags::NONBLOCK)
        .map_err(|_| Failure::new(Status::Internal, "pipe_nonblocking"))
}

struct WorkerProcess {
    child: Option<Child>,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
}

fn kill_and_reap(mut child: Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let _ = child.kill();
    // Reaping is deliberately detached: even an uninterruptible guest
    // compositor syscall cannot block the sole clipboard service loop.
    let _ = thread::Builder::new()
        .name("wb-clipboard-worker-reaper".to_owned())
        .spawn(move || {
            let _ = child.wait();
        });
}

impl WorkerProcess {
    fn spawn() -> Result<Self, Failure> {
        let executable = std::env::current_exe()
            .map_err(|_| Failure::new(Status::Internal, "worker_executable"))?;
        let mut child = Command::new(executable)
            .arg(INTERNAL_WORKER_ARGUMENT)
            .env(WORKER_ENVIRONMENT, WORKER_ENVIRONMENT_VALUE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|_| Failure::new(Status::Internal, "worker_spawn"))?;
        let Some(input) = child.stdin.take() else {
            kill_and_reap(child);
            return Err(Failure::new(Status::Internal, "worker_input"));
        };
        let Some(output) = child.stdout.take() else {
            kill_and_reap(child);
            return Err(Failure::new(Status::Internal, "worker_output"));
        };
        let worker = Self {
            child: Some(child),
            input: Some(input),
            output: Some(output),
        };
        ensure_cloexec(worker.input.as_ref().expect("worker input exists"))
            .map_err(|_| Failure::new(Status::Internal, "worker_cloexec"))?;
        ensure_cloexec(worker.output.as_ref().expect("worker output exists"))
            .map_err(|_| Failure::new(Status::Internal, "worker_cloexec"))?;
        Ok(worker)
    }

    fn exchange(
        mut self,
        request: SecretFrame,
        deadline: &Deadline,
    ) -> Result<(SecretFrame, Self), Failure> {
        let mut request = request;
        let input = self
            .input
            .as_mut()
            .ok_or_else(|| Failure::new(Status::Internal, "worker_input"))?;
        set_nonblocking(input)?;
        write_frame(
            DeadlineFd {
                io: input,
                deadline,
            },
            &request.0,
        )
        .map_err(|error| protocol_failure(&error))?;
        request.0.payload.zeroize();
        request.0.nonce.zeroize();
        self.input = None;

        let output = self
            .output
            .as_mut()
            .ok_or_else(|| Failure::new(Status::Internal, "worker_output"))?;
        set_nonblocking(output)?;
        let response = SecretFrame(
            read_frame(DeadlineFd {
                io: output,
                deadline,
            })
            .map_err(|error| protocol_failure(&error))?,
        );
        self.output = None;
        if response.0.nonce != WORKER_NONCE {
            return Err(Failure::new(Status::InvalidRequest, "worker_nonce"));
        }
        Ok((response, self))
    }

    fn is_running(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        let status = child.try_wait();
        match status {
            Ok(None) => true,
            Ok(Some(_)) => {
                self.child = None;
                false
            }
            Err(_) => {
                self.terminate();
                false
            }
        }
    }

    fn terminate(&mut self) {
        self.input = None;
        self.output = None;
        let Some(child) = self.child.take() else {
            return;
        };
        kill_and_reap(child);
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct PersistentOwner {
    worker: WorkerProcess,
}

impl PersistentOwner {
    fn is_running(&mut self) -> bool {
        self.worker.is_running()
    }
}

fn validate_worker_response(
    response: &SecretFrame,
    expected_kind: Kind,
    expected_mime: Mime,
) -> Result<(), Failure> {
    if response.0.kind != expected_kind {
        return Err(Failure::new(Status::InvalidRequest, "worker_response_kind"));
    }
    if response.0.status != Status::Ok {
        return Err(worker_status_failure(response.0.status));
    }
    if response.0.mime != expected_mime {
        return Err(Failure::new(Status::InvalidContent, "worker_response_mime"));
    }
    Ok(())
}

fn worker_status_failure(status: Status) -> Failure {
    match status {
        Status::Ok => Failure::new(Status::Internal, "worker_status_shape"),
        Status::InvalidRequest => Failure::new(status, "worker_invalid_request"),
        Status::UnsupportedMime => Failure::new(status, "worker_unsupported_mime"),
        Status::TooLarge => Failure::new(status, "worker_too_large"),
        Status::InvalidContent => Failure::new(status, "worker_invalid_content"),
        Status::ClipboardUnavailable => Failure::new(status, "worker_clipboard_unavailable"),
        Status::Timeout => Failure::timeout(),
        Status::Busy => Failure::new(status, "worker_busy"),
        Status::Internal => Failure::new(status, "worker_internal"),
    }
}

fn direct_probe(deadline: &Deadline) -> Result<(), Failure> {
    deadline.check()?;
    utils::is_primary_selection_supported().map_err(|_| {
        Failure::new(
            Status::ClipboardUnavailable,
            "wayland_clipboard_unavailable",
        )
    })?;
    deadline.check()
}

fn direct_prepare_put(
    mime: Mime,
    payload: Vec<u8>,
    deadline: &Deadline,
) -> Result<copy::PreparedCopy, Failure> {
    deadline.check()?;
    let mut source = SecretVec::from(payload);
    let (canonical_mime, canonical_bytes) = match mime {
        Mime::Text => {
            validate_text(&source.0)?;
            (TEXT_MIME, source.take())
        }
        Mime::Png => {
            let encoded = canonical_image(&source.0, Some(ImageFormat::Png), deadline)?;
            (PNG_MIME, encoded)
        }
        Mime::None => {
            return Err(Failure::new(
                Status::UnsupportedMime,
                "unsupported_wire_mime",
            ));
        }
    };
    let mut canonical_bytes = SecretVec::from(canonical_bytes);
    deadline.check()?;

    let mut options = copy::Options::new();
    options
        .clipboard(copy::ClipboardType::Regular)
        .foreground(true);
    let prepared = options
        .prepare_copy(
            copy::Source::Bytes(canonical_bytes.take().into_boxed_slice()),
            copy::MimeType::Specific(canonical_mime.to_owned()),
        )
        .map_err(|_| Failure::new(Status::ClipboardUnavailable, "clipboard_ownership_failed"))?;
    deadline.check()?;
    Ok(prepared)
}

fn direct_get(deadline: &Deadline) -> Result<(Mime, Vec<u8>), Failure> {
    deadline.check()?;
    // wl-clipboard-rs materializes the compositor's complete MIME offer before
    // returning. This happens only in the disposable address-space-limited
    // worker, so a hostile offer cannot exhaust or wedge the service process.
    let offers =
        paste::get_mime_types_ordered(paste::ClipboardType::Regular, paste::Seat::Unspecified)
            .map_err(|_| {
                Failure::new(Status::ClipboardUnavailable, "clipboard_offer_unavailable")
            })?;
    deadline.check()?;
    validate_offer_metadata(&offers)?;
    let offer = choose_offer(&offers)
        .ok_or_else(|| Failure::new(Status::UnsupportedMime, "clipboard_offer_unsupported"))?;

    let (mut pipe, actual_mime) = paste::get_contents(
        paste::ClipboardType::Regular,
        paste::Seat::Unspecified,
        paste::MimeType::Specific(&offer.mime),
    )
    .map_err(|_| Failure::new(Status::ClipboardUnavailable, "clipboard_read_unavailable"))?;
    if actual_mime != offer.mime {
        return Err(Failure::new(
            Status::InvalidContent,
            "clipboard_mime_changed",
        ));
    }
    ensure_cloexec(&pipe).map_err(|_| Failure::new(Status::Internal, "pipe_cloexec"))?;
    let mut raw = read_pipe_bounded(&mut pipe, offer.source_limit(), deadline)?;
    deadline.check()?;

    match offer.kind {
        OfferKind::Text => {
            validate_text(&raw.0)?;
            Ok((Mime::Text, raw.take()))
        }
        OfferKind::Image(format) => {
            let encoded = canonical_image(&raw.0, Some(format), deadline)?;
            Ok((Mime::Png, encoded))
        }
    }
}

struct WorkerLimits {
    original_cpu: Rlimit,
}

fn cap_worker_resource(
    resource: Resource,
    cap: u64,
    category: &'static str,
) -> Result<(), Failure> {
    let inherited = getrlimit(resource);
    let limit = inherited.maximum.map_or(cap, |maximum| maximum.min(cap));
    setrlimit(
        resource,
        Rlimit {
            current: Some(limit),
            maximum: Some(limit),
        },
    )
    .map_err(|_| Failure::new(Status::Internal, category))
}

fn apply_worker_limits() -> Result<WorkerLimits, Failure> {
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(|_| Failure::new(Status::Internal, "worker_dumpable"))?;
    setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )
    .map_err(|_| Failure::new(Status::Internal, "worker_core_limit"))?;
    setrlimit(
        Resource::Fsize,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )
    .map_err(|_| Failure::new(Status::Internal, "worker_file_limit"))?;
    cap_worker_resource(Resource::Nofile, WORKER_OPEN_FILES, "worker_fd_limit")?;
    cap_worker_resource(
        Resource::As,
        WORKER_ADDRESS_SPACE_BYTES,
        "worker_memory_limit",
    )?;

    let original_cpu = getrlimit(Resource::Cpu);
    let bounded_cpu = original_cpu.maximum.map_or(WORKER_CPU_SECONDS, |maximum| {
        maximum.min(WORKER_CPU_SECONDS)
    });
    setrlimit(
        Resource::Cpu,
        Rlimit {
            current: Some(bounded_cpu),
            maximum: original_cpu.maximum,
        },
    )
    .map_err(|_| Failure::new(Status::Internal, "worker_cpu_limit"))?;
    Ok(WorkerLimits { original_cpu })
}

impl WorkerLimits {
    fn prepare_for_persistent_owner(&self) -> Result<(), Failure> {
        setrlimit(Resource::Cpu, self.original_cpu)
            .map_err(|_| Failure::new(Status::Internal, "worker_cpu_restore"))
    }
}

enum WorkerSuccess {
    Put(Box<copy::PreparedCopy>),
    Get(Mime, Vec<u8>),
    Probe,
}

/// Return true only for the exact fixed child mode spawned by this agent.
pub fn is_internal_worker_invocation(arguments: &[OsString]) -> bool {
    arguments.len() == 2
        && arguments[1] == INTERNAL_WORKER_ARGUMENT
        && std::env::var_os(WORKER_ENVIRONMENT).as_deref()
            == Some(std::ffi::OsStr::new(WORKER_ENVIRONMENT_VALUE))
}

/// Execute one isolated fixed clipboard operation. A successful Put signals
/// readiness and then remains alive as the ordinary Sway selection owner.
pub fn internal_worker_entrypoint() -> i32 {
    let limits = match apply_worker_limits() {
        Ok(limits) => limits,
        Err(_) => return 1,
    };
    let mut request = match read_frame(io::stdin().lock()) {
        Ok(frame) => SecretFrame(frame),
        Err(_) => return 2,
    };
    if !matches!(request.0.kind, Kind::Put | Kind::Get | Kind::Probe) {
        return 2;
    }
    let kind = request.0.kind;
    let mime = request.0.mime;
    let mut nonce = request.0.nonce;
    let deadline = Deadline::new();
    let operation = match kind {
        Kind::Put => direct_prepare_put(mime, request.take_payload(), &deadline)
            .map(Box::new)
            .map(WorkerSuccess::Put),
        Kind::Get => direct_get(&deadline).map(|(mime, payload)| WorkerSuccess::Get(mime, payload)),
        Kind::Probe => direct_probe(&deadline).map(|()| WorkerSuccess::Probe),
        Kind::PutResult | Kind::GetResult | Kind::ProbeResult => unreachable!(),
    };
    request.0.payload.zeroize();
    request.0.nonce.zeroize();

    let (mut response, mut owner) = match operation {
        Ok(WorkerSuccess::Put(owner)) => (
            SecretFrame(
                Frame::result(Kind::PutResult, nonce, Status::Ok, Mime::None, Vec::new())
                    .expect("fixed worker Put response is valid"),
            ),
            Some(owner),
        ),
        Ok(WorkerSuccess::Get(mime, payload)) => (
            SecretFrame(
                Frame::result(Kind::GetResult, nonce, Status::Ok, mime, payload)
                    .expect("validated worker Get response is valid"),
            ),
            None,
        ),
        Ok(WorkerSuccess::Probe) => (
            SecretFrame(
                Frame::result(Kind::ProbeResult, nonce, Status::Ok, Mime::None, Vec::new())
                    .expect("fixed worker Probe response is valid"),
            ),
            None,
        ),
        Err(failure) => {
            let response_kind = match kind {
                Kind::Put => Kind::PutResult,
                Kind::Get => Kind::GetResult,
                Kind::Probe => Kind::ProbeResult,
                Kind::PutResult | Kind::GetResult | Kind::ProbeResult => unreachable!(),
            };
            (
                SecretFrame(Frame::error(response_kind, nonce, failure.status)),
                None,
            )
        }
    };
    if owner.is_some() && limits.prepare_for_persistent_owner().is_err() {
        let response_nonce = response.0.nonce;
        response = SecretFrame(Frame::error(
            Kind::PutResult,
            response_nonce,
            Status::Internal,
        ));
        owner = None;
    }
    nonce.zeroize();
    let write_result = write_frame(io::stdout().lock(), &response.0);
    response.0.payload.zeroize();
    response.0.nonce.zeroize();
    if write_result.is_err() {
        return 1;
    }

    if let Some(owner) = owner {
        if owner.serve().is_err() {
            return 1;
        }
    }
    0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OfferKind {
    Text,
    Image(ImageFormat),
}

struct SelectedOffer {
    mime: String,
    kind: OfferKind,
}

impl SelectedOffer {
    fn source_limit(&self) -> usize {
        match self.kind {
            OfferKind::Text => MAX_TEXT_BYTES,
            OfferKind::Image(_) => MAX_IMAGE_BYTES,
        }
    }
}

fn validate_offer_metadata(offers: &[String]) -> Result<(), Failure> {
    if offers.len() > MAX_MIME_OFFERS {
        return Err(Failure::new(Status::TooLarge, "mime_offer_count"));
    }
    let bytes = offers
        .iter()
        .try_fold(0_usize, |total, value| total.checked_add(value.len()));
    if bytes.is_none_or(|bytes| bytes > MAX_MIME_METADATA_BYTES) {
        return Err(Failure::new(Status::TooLarge, "mime_offer_metadata"));
    }
    Ok(())
}

fn choose_offer(offers: &[String]) -> Option<SelectedOffer> {
    const TEXT_PREFERENCE: [&str; 5] = [TEXT_MIME, "UTF8_STRING", "text/plain", "TEXT", "STRING"];
    for wanted in TEXT_PREFERENCE {
        if let Some(offered) = offers.iter().find(|offered| mime_eq(offered, wanted)) {
            return Some(SelectedOffer {
                mime: offered.clone(),
                kind: OfferKind::Text,
            });
        }
    }
    offers.iter().find_map(|offered| {
        image_format_for_mime(offered).map(|format| SelectedOffer {
            mime: offered.clone(),
            kind: OfferKind::Image(format),
        })
    })
}

fn mime_eq(left: &str, right: &str) -> bool {
    if right.contains('/') {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn image_format_for_mime(mime: &str) -> Option<ImageFormat> {
    if mime.eq_ignore_ascii_case("image/png") {
        Some(ImageFormat::Png)
    } else if mime.eq_ignore_ascii_case("image/jpeg") || mime.eq_ignore_ascii_case("image/jpg") {
        Some(ImageFormat::Jpeg)
    } else if mime.eq_ignore_ascii_case("image/webp") {
        Some(ImageFormat::WebP)
    } else if mime.eq_ignore_ascii_case("image/bmp") || mime.eq_ignore_ascii_case("image/x-bmp") {
        Some(ImageFormat::Bmp)
    } else if mime.eq_ignore_ascii_case("image/tiff")
        || mime.eq_ignore_ascii_case("image/tif")
        || mime.eq_ignore_ascii_case("image/x-tiff")
    {
        Some(ImageFormat::Tiff)
    } else {
        None
    }
}

fn read_pipe_bounded(
    pipe: &mut impl ReadAndFd,
    limit: usize,
    deadline: &Deadline,
) -> Result<SecretVec, Failure> {
    let flags = fcntl_getfl(&*pipe).map_err(|_| Failure::new(Status::Internal, "pipe_flags"))?;
    fcntl_setfl(&*pipe, flags | OFlags::NONBLOCK)
        .map_err(|_| Failure::new(Status::Internal, "pipe_nonblocking"))?;
    let mut result = SecretVec::with_capacity(limit.min(PIPE_CHUNK_BYTES))?;
    let mut chunk = [0_u8; PIPE_CHUNK_BYTES];
    loop {
        deadline.check()?;
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                let new_length =
                    result.0.len().checked_add(count).ok_or_else(|| {
                        Failure::new(Status::TooLarge, "clipboard_source_too_large")
                    })?;
                if new_length > limit {
                    chunk.zeroize();
                    return Err(Failure::new(Status::TooLarge, "clipboard_source_too_large"));
                }
                result
                    .0
                    .try_reserve(count)
                    .map_err(|_| Failure::new(Status::Internal, "memory_allocation"))?;
                result.0.extend_from_slice(&chunk[..count]);
                chunk[..count].zeroize();
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                let remaining = deadline
                    .expires
                    .checked_duration_since(Instant::now())
                    .ok_or_else(Failure::timeout)?;
                thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            Err(_) => {
                chunk.zeroize();
                return Err(Failure::new(
                    Status::ClipboardUnavailable,
                    "clipboard_source_read",
                ));
            }
        }
    }
    chunk.zeroize();
    Ok(result)
}

trait ReadAndFd: Read + AsFd {}
impl<T: Read + AsFd> ReadAndFd for T {}

fn validate_text(bytes: &[u8]) -> Result<(), Failure> {
    if bytes.len() > MAX_TEXT_BYTES {
        return Err(Failure::new(Status::TooLarge, "text_too_large"));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Failure::new(Status::InvalidContent, "text_invalid_utf8"))?;
    if text.contains('\0') {
        return Err(Failure::new(Status::InvalidContent, "text_embedded_nul"));
    }
    Ok(())
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), Failure> {
    if width == 0 || height == 0 {
        return Err(Failure::new(
            Status::InvalidContent,
            "image_empty_dimension",
        ));
    }
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(Failure::new(Status::TooLarge, "image_edge_limit"));
    }
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS {
        return Err(Failure::new(Status::TooLarge, "image_area_limit"));
    }
    Ok(())
}

fn canonical_image(
    bytes: &[u8],
    expected_format: Option<ImageFormat>,
    deadline: &Deadline,
) -> Result<Vec<u8>, Failure> {
    deadline.check()?;
    if bytes.is_empty() {
        return Err(Failure::new(Status::InvalidContent, "image_empty"));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(Failure::new(Status::TooLarge, "image_source_too_large"));
    }
    let format = image::guess_format(bytes)
        .map_err(|_| Failure::new(Status::InvalidContent, "image_format_invalid"))?;
    if !SUPPORTED_IMAGE_FORMATS.contains(&format) {
        return Err(Failure::new(
            Status::UnsupportedMime,
            "image_format_unsupported",
        ));
    }
    if expected_format.is_some_and(|expected| expected != format) {
        return Err(Failure::new(Status::InvalidContent, "image_mime_mismatch"));
    }
    reject_non_still_content(format, bytes)?;
    deadline.check()?;

    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|_| Failure::new(Status::InvalidContent, "image_dimensions_invalid"))?;
    validate_dimensions(width, height)?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(image_limits());
    let decoded = SecretImage(
        reader
            .decode()
            .map_err(|_| Failure::new(Status::InvalidContent, "image_decode_failed"))?,
    );
    if decoded.0.dimensions() != (width, height) {
        return Err(Failure::new(
            Status::InvalidContent,
            "image_dimensions_changed",
        ));
    }
    deadline.check()?;

    let mut output = BoundedWriter::new(MAX_IMAGE_BYTES)?;
    let encoded = decoded.0.write_with_encoder(PngEncoder::new(&mut output));
    if output.exceeded {
        return Err(Failure::new(Status::TooLarge, "canonical_png_too_large"));
    }
    encoded.map_err(|_| Failure::new(Status::InvalidContent, "png_encode_failed"))?;
    deadline.check()?;
    Ok(output.bytes.take())
}

fn reject_non_still_content(format: ImageFormat, bytes: &[u8]) -> Result<(), Failure> {
    match format {
        ImageFormat::Png => {
            let decoder =
                PngDecoder::with_limits(BufReader::new(Cursor::new(bytes)), image_limits())
                    .map_err(|_| Failure::new(Status::InvalidContent, "png_structure_invalid"))?;
            if decoder
                .is_apng()
                .map_err(|_| Failure::new(Status::InvalidContent, "png_structure_invalid"))?
            {
                return Err(Failure::new(
                    Status::InvalidContent,
                    "animated_png_rejected",
                ));
            }
            let (width, height) = decoder.dimensions();
            validate_dimensions(width, height)
        }
        ImageFormat::WebP => {
            let decoder = WebPDecoder::new(BufReader::new(Cursor::new(bytes)))
                .map_err(|_| Failure::new(Status::InvalidContent, "webp_structure_invalid"))?;
            if decoder.has_animation() {
                return Err(Failure::new(
                    Status::InvalidContent,
                    "animated_webp_rejected",
                ));
            }
            let (width, height) = decoder.dimensions();
            validate_dimensions(width, height)
        }
        ImageFormat::Tiff => {
            let mut decoder = tiff::decoder::Decoder::new(Cursor::new(bytes))
                .map_err(|_| Failure::new(Status::InvalidContent, "tiff_structure_invalid"))?;
            if decoder.more_images() {
                return Err(Failure::new(
                    Status::InvalidContent,
                    "multi_page_tiff_rejected",
                ));
            }
            let (width, height) = decoder
                .dimensions()
                .map_err(|_| Failure::new(Status::InvalidContent, "tiff_dimensions_invalid"))?;
            validate_dimensions(width, height)
        }
        _ => Ok(()),
    }
}

fn image_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(IMAGE_DECODE_BYTES);
    limits
}

fn zero_dynamic_image(image: &mut DynamicImage) {
    match image {
        DynamicImage::ImageLuma8(buffer) => buffer.as_mut().zeroize(),
        DynamicImage::ImageLumaA8(buffer) => buffer.as_mut().zeroize(),
        DynamicImage::ImageRgb8(buffer) => buffer.as_mut().zeroize(),
        DynamicImage::ImageRgba8(buffer) => buffer.as_mut().zeroize(),
        DynamicImage::ImageLuma16(buffer) => buffer.as_mut().zeroize(),
        DynamicImage::ImageLumaA16(buffer) => buffer.as_mut().zeroize(),
        DynamicImage::ImageRgb16(buffer) => buffer.as_mut().zeroize(),
        DynamicImage::ImageRgba16(buffer) => buffer.as_mut().zeroize(),
        DynamicImage::ImageRgb32F(buffer) => buffer.as_mut().zeroize(),
        DynamicImage::ImageRgba32F(buffer) => buffer.as_mut().zeroize(),
        _ => {}
    }
}

struct BoundedWriter {
    bytes: SecretVec,
    limit: usize,
    exceeded: bool,
}

impl BoundedWriter {
    fn new(limit: usize) -> Result<Self, Failure> {
        Ok(Self {
            bytes: SecretVec::with_capacity(limit.min(PIPE_CHUNK_BYTES))?,
            limit,
            exceeded: false,
        })
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let Some(new_length) = self.bytes.0.len().checked_add(buffer.len()) else {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bounded clipboard buffer exceeded",
            ));
        };
        if new_length > self.limit {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bounded clipboard buffer exceeded",
            ));
        }
        self.bytes
            .0
            .try_reserve(buffer.len())
            .map_err(|_| io::Error::other("clipboard buffer allocation failed"))?;
        self.bytes.0.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Audit {
    direction: &'static str,
    mime: &'static str,
    bytes: usize,
    status: Status,
    category: &'static str,
}

impl Audit {
    fn emit(&self) {
        eprintln!(
            "clipboard-agent direction={} mime={} bytes={} result={} category={}",
            self.direction,
            self.mime,
            self.bytes,
            self.status.code(),
            self.category
        );
    }
}

fn dispatch(
    backend: &mut impl ClipboardBackend,
    mut request: Frame,
    deadline: &Deadline,
) -> Option<(Frame, Audit)> {
    let mut nonce = request.nonce;
    let direction = match request.kind {
        Kind::Put => "host_to_guest",
        Kind::Get => "guest_to_host",
        Kind::Probe => "probe",
        Kind::PutResult | Kind::GetResult | Kind::ProbeResult => {
            request.payload.zeroize();
            request.nonce.zeroize();
            return None;
        }
    };
    let mime_name = request.mime.canonical().unwrap_or("none");
    let request_bytes = request.payload.len();
    let operation = match request.kind {
        Kind::Put => backend
            .put(request.mime, mem::take(&mut request.payload), deadline)
            .map(|bytes| (Kind::PutResult, Mime::None, Vec::new(), bytes)),
        Kind::Get => backend
            .get(deadline)
            .map(|(mime, payload)| (Kind::GetResult, mime, payload, 0)),
        Kind::Probe => backend
            .probe(deadline)
            .map(|()| (Kind::ProbeResult, Mime::None, Vec::new(), 0)),
        Kind::PutResult | Kind::GetResult | Kind::ProbeResult => unreachable!(),
    };
    request.payload.zeroize();
    request.nonce.zeroize();

    let result = match operation {
        Ok((response_kind, mime, payload, put_bytes)) => {
            let bytes = if response_kind == Kind::GetResult {
                payload.len()
            } else if response_kind == Kind::PutResult {
                put_bytes
            } else {
                request_bytes
            };
            let audit_mime = mime.canonical().unwrap_or(mime_name);
            let response = Frame::result(response_kind, nonce, Status::Ok, mime, payload)
                .expect("agent constructs a valid clipboard response");
            Some((
                response,
                Audit {
                    direction,
                    mime: audit_mime,
                    bytes,
                    status: Status::Ok,
                    category: "ok",
                },
            ))
        }
        Err(failure) => {
            let response_kind = match request.kind {
                Kind::Put => Kind::PutResult,
                Kind::Get => Kind::GetResult,
                Kind::Probe => Kind::ProbeResult,
                Kind::PutResult | Kind::GetResult | Kind::ProbeResult => unreachable!(),
            };
            Some((
                Frame::error(response_kind, nonce, failure.status),
                Audit {
                    direction,
                    mime: mime_name,
                    bytes: request_bytes,
                    status: failure.status,
                    category: failure.category,
                },
            ))
        }
    };
    nonce.zeroize();
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PeerIdentity {
    uid: u32,
    gid: u32,
}

fn mounted_host_identity(runtime: &Path) -> Result<PeerIdentity, RunError> {
    let metadata =
        fs::symlink_metadata(runtime).map_err(|_| RunError::new("runtime_directory_missing"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(RunError::new("runtime_directory_type"));
    }
    Ok(PeerIdentity {
        uid: metadata.uid(),
        gid: metadata.gid(),
    })
}

fn peer_credentials(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut credentials = MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` points to writable storage of exactly `length`
    // bytes and `stream` remains alive for the complete getsockopt call.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERCRED returned the wrong credential size",
        ));
    }
    // SAFETY: a successful SO_PEERCRED call initialized the complete ucred.
    Ok(unsafe { credentials.assume_init() })
}

fn protocol_failure(error: &ProtocolError) -> Failure {
    match error {
        ProtocolError::PayloadTooLarge { .. } => Failure::new(Status::TooLarge, "frame_too_large"),
        ProtocolError::Io(error)
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) =>
        {
            Failure::timeout()
        }
        ProtocolError::Io(_) => Failure::new(Status::Internal, "frame_io"),
        _ => Failure::new(Status::InvalidRequest, "frame_invalid"),
    }
}

fn serve_connection(
    stream: UnixStream,
    backend: &mut impl ClipboardBackend,
    host: PeerIdentity,
) -> Result<(), Failure> {
    ensure_cloexec(&stream).map_err(|_| Failure::new(Status::Internal, "socket_cloexec"))?;
    let credentials = peer_credentials(&stream)
        .map_err(|_| Failure::new(Status::InvalidRequest, "peer_credentials"))?;
    if credentials.uid != host.uid || credentials.gid != host.gid {
        return Err(Failure::new(Status::InvalidRequest, "peer_uid"));
    }

    // The host may pin this authenticated endpoint before it asynchronously
    // snapshots its native clipboard. Bound that pre-request idle separately
    // so it cannot consume the operation's five-second budget.
    let request_deadline = Deadline::after(Duration::from_secs(PRE_REQUEST_IDLE_SECONDS));
    let mut request_transport = DeadlineStream {
        stream: &stream,
        deadline: &request_deadline,
    };
    let request = read_frame(&mut request_transport).map_err(|error| protocol_failure(&error))?;
    let deadline = Deadline::new();
    let Some((mut response, audit)) = dispatch(backend, request, &deadline) else {
        return Err(Failure::new(Status::InvalidRequest, "wrong_direction"));
    };
    let mut response_transport = DeadlineStream {
        stream: &stream,
        deadline: &deadline,
    };
    let write_result =
        write_frame(&mut response_transport, &response).map_err(|error| protocol_failure(&error));
    response.payload.zeroize();
    response.nonce.zeroize();
    match write_result {
        Ok(()) => audit.emit(),
        Err(failure) => Audit {
            status: failure.status,
            category: failure.category,
            ..audit
        }
        .emit(),
    }
    write_result
}

/// Bind the fixed private endpoint and serve one framed transaction per
/// connection. The outer desktop UID maps directly to guest UID 1000 in this
/// machine's active Podman user namespace. The bind-mounted directory owner
/// is the kernel-translated identity of the host process in that namespace,
/// so this remains exact for `host`, `keep-id`, `auto`, `nomap`, and explicit
/// UID/GID maps without Buzzard interpreting the selected mapping.
pub fn run() -> Result<(), RunError> {
    rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
        .map_err(|_| RunError::new("core_dump_policy"))?;
    let host = mounted_host_identity(Path::new(RUNTIME_DIRECTORY))?;
    let mut endpoint = Endpoint::bind()?;
    let mut backend = WaylandClipboard::new();
    backend
        .probe(&Deadline::new())
        .map_err(|_| RunError::new("clipboard_probe"))?;
    endpoint.publish_ready()?;

    loop {
        match endpoint.listener.accept() {
            Ok((stream, _)) => {
                if let Err(failure) = serve_connection(stream, &mut backend, host) {
                    Audit {
                        direction: "transport",
                        mime: "none",
                        bytes: 0,
                        status: failure.status,
                        category: failure.category,
                    }
                    .emit();
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(RunError::new("socket_accept")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crc32fast::Hasher;
    use image::{Rgba, RgbaImage};
    use std::net::Shutdown;
    use tiff::encoder::{TiffEncoder, colortype};

    struct MockClipboard {
        put: Option<(Mime, Vec<u8>)>,
        get: Result<(Mime, Vec<u8>), Failure>,
        probes: usize,
    }

    impl ClipboardBackend for MockClipboard {
        fn probe(&mut self, _deadline: &Deadline) -> Result<(), Failure> {
            self.probes += 1;
            Ok(())
        }

        fn put(
            &mut self,
            mime: Mime,
            payload: Vec<u8>,
            _deadline: &Deadline,
        ) -> Result<usize, Failure> {
            let length = payload.len();
            self.put = Some((mime, payload));
            Ok(length)
        }

        fn get(&mut self, _deadline: &Deadline) -> Result<(Mime, Vec<u8>), Failure> {
            mem::replace(
                &mut self.get,
                Err(Failure::new(Status::Internal, "test_get_consumed")),
            )
        }
    }

    fn mock(get: Result<(Mime, Vec<u8>), Failure>) -> MockClipboard {
        MockClipboard {
            put: None,
            get,
            probes: 0,
        }
    }

    fn test_image(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            width,
            height,
            Rgba([0x22, 0x66, 0xaa, 0x80]),
        ));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, format).unwrap();
        bytes.into_inner()
    }

    fn animated_png_marker(mut png: Vec<u8>) -> Vec<u8> {
        let data = [0_u8, 0, 0, 1, 0, 0, 0, 0];
        let mut hasher = Hasher::new();
        hasher.update(b"acTL");
        hasher.update(&data);
        let mut chunk = Vec::new();
        chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
        chunk.extend_from_slice(b"acTL");
        chunk.extend_from_slice(&data);
        chunk.extend_from_slice(&hasher.finalize().to_be_bytes());
        png.splice(33..33, chunk);
        png
    }

    #[test]
    fn accepts_unicode_text_and_rejects_invalid_nul_and_oversize_text() {
        assert!(validate_text("Buzzard — 日本語 🦅".as_bytes()).is_ok());
        assert_eq!(
            validate_text(&[0xff]).unwrap_err().status,
            Status::InvalidContent
        );
        assert_eq!(
            validate_text(b"contains\0nul").unwrap_err().status,
            Status::InvalidContent
        );
        assert_eq!(
            validate_text(&vec![b'a'; MAX_TEXT_BYTES + 1])
                .unwrap_err()
                .status,
            Status::TooLarge
        );
    }

    #[test]
    fn canonicalizes_every_supported_native_still_format_to_bounded_png_in_memory() {
        for format in SUPPORTED_IMAGE_FORMATS {
            let source = test_image(format, 7, 5);
            let png = canonical_image(&source, Some(format), &Deadline::new()).unwrap();
            assert!(png.len() <= MAX_IMAGE_BYTES);
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            assert_eq!(image::load_from_memory(&png).unwrap().dimensions(), (7, 5));
        }

        let jpeg = test_image(ImageFormat::Jpeg, 1, 1);
        assert_eq!(
            canonical_image(&jpeg, Some(ImageFormat::Png), &Deadline::new())
                .unwrap_err()
                .category,
            "image_mime_mismatch"
        );
    }

    #[test]
    fn rejects_animated_png_and_multi_page_tiff() {
        let png = animated_png_marker(test_image(ImageFormat::Png, 2, 2));
        assert_eq!(
            canonical_image(&png, Some(ImageFormat::Png), &Deadline::new())
                .unwrap_err()
                .category,
            "animated_png_rejected"
        );

        let mut cursor = Cursor::new(Vec::new());
        {
            let mut encoder = TiffEncoder::new(&mut cursor).unwrap();
            encoder
                .write_image::<colortype::Gray8>(1, 1, &[0x11])
                .unwrap();
            encoder
                .write_image::<colortype::Gray8>(1, 1, &[0x22])
                .unwrap();
        }
        assert_eq!(
            canonical_image(cursor.get_ref(), Some(ImageFormat::Tiff), &Deadline::new())
                .unwrap_err()
                .category,
            "multi_page_tiff_rejected"
        );
    }

    #[test]
    fn enforces_edge_area_and_encoded_output_bounds() {
        assert_eq!(
            validate_dimensions(0, 1).unwrap_err().status,
            Status::InvalidContent
        );
        assert_eq!(
            validate_dimensions(MAX_IMAGE_DIMENSION + 1, 1)
                .unwrap_err()
                .status,
            Status::TooLarge
        );
        assert_eq!(
            validate_dimensions(8192, 8193).unwrap_err().status,
            Status::TooLarge
        );
        assert!(validate_dimensions(8192, 8192).is_ok());

        let mut writer = BoundedWriter::new(3).unwrap();
        assert!(writer.write_all(b"four").is_err());
        assert!(writer.exceeded);
        assert!(writer.bytes.0.is_empty());
    }

    #[test]
    fn text_is_preferred_then_the_first_supported_native_image() {
        let offers = vec![
            "text/html".to_owned(),
            "image/webp".to_owned(),
            "image/png".to_owned(),
            "text/plain".to_owned(),
        ];
        let selected = choose_offer(&offers).unwrap();
        assert_eq!(selected.kind, OfferKind::Text);
        assert_eq!(selected.mime, "text/plain");

        let images = vec!["image/webp".to_owned(), "image/png".to_owned()];
        let selected = choose_offer(&images).unwrap();
        assert_eq!(selected.kind, OfferKind::Image(ImageFormat::WebP));

        for alias in [
            "image/jpg",
            "image/x-bmp",
            "image/tiff",
            "image/tif",
            "image/x-tiff",
        ] {
            assert!(image_format_for_mime(alias).is_some(), "alias={alias}");
        }
    }

    #[test]
    fn peer_credentials_come_from_the_connected_unix_process() {
        let (server, _client) = UnixStream::pair().unwrap();
        let credentials = peer_credentials(&server).unwrap();
        assert_eq!(credentials.uid, rustix::process::getuid().as_raw());
        assert_eq!(credentials.gid, rustix::process::getgid().as_raw());
    }

    #[test]
    fn framed_connection_accepts_the_kernel_translated_mount_owner() {
        let nonce = [0x71; 16];
        let (server, mut client) = UnixStream::pair().unwrap();
        write_frame(&mut client, &Frame::probe(nonce)).unwrap();
        let mut backend = mock(Err(Failure::new(Status::UnsupportedMime, "empty")));
        let host = PeerIdentity {
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
        };
        serve_connection(server, &mut backend, host).unwrap();
        let response = read_frame(&mut client).unwrap();
        assert_eq!(response.kind, Kind::ProbeResult);
        assert_eq!(response.nonce, nonce);
        assert_eq!(response.status, Status::Ok);
    }

    #[test]
    fn framed_connection_rejects_an_identity_other_than_the_mapped_mount_owner() {
        let nonce = [0x72; 16];
        let (server, mut client) = UnixStream::pair().unwrap();
        write_frame(&mut client, &Frame::probe(nonce)).unwrap();
        let mut backend = mock(Err(Failure::new(Status::UnsupportedMime, "empty")));
        let host = PeerIdentity {
            uid: rustix::process::getuid().as_raw().wrapping_add(1),
            gid: rustix::process::getgid().as_raw(),
        };
        assert_eq!(
            serve_connection(server, &mut backend, host)
                .unwrap_err()
                .category,
            "peer_uid"
        );
        assert_eq!(backend.probes, 0);
    }

    #[test]
    fn expired_deadline_returns_the_typed_timeout_status() {
        let deadline = Deadline {
            expires: Instant::now(),
        };
        assert_eq!(deadline.check().unwrap_err().status, Status::Timeout);

        let png = test_image(ImageFormat::Png, 2, 2);
        assert_eq!(
            canonical_image(&png, Some(ImageFormat::Png), &deadline)
                .unwrap_err()
                .status,
            Status::Timeout
        );
    }

    #[test]
    fn stalled_clipboard_source_and_oversize_source_are_bounded() {
        let (mut reader, writer) = UnixStream::pair().unwrap();
        let started = Instant::now();
        let deadline = Deadline::after(Duration::from_millis(35));
        let failure = match read_pipe_bounded(&mut reader, 32, &deadline) {
            Ok(_) => panic!("stalled source unexpectedly completed"),
            Err(failure) => failure,
        };
        assert_eq!(failure.status, Status::Timeout);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(writer);

        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"four").unwrap();
        writer.shutdown(Shutdown::Write).unwrap();
        let failure = match read_pipe_bounded(&mut reader, 3, &Deadline::new()) {
            Ok(_) => panic!("oversized source unexpectedly succeeded"),
            Err(failure) => failure,
        };
        assert_eq!(failure.status, Status::TooLarge);
        assert_eq!(failure.category, "clipboard_source_too_large");
    }

    #[test]
    fn stalled_isolated_worker_is_killed_without_holding_service_loop() {
        let mut child = Command::new("/usr/bin/sleep")
            .arg("30")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let worker = WorkerProcess {
            child: Some(child),
            input: Some(input),
            output: Some(output),
        };
        let started = Instant::now();
        let failure = match worker.exchange(
            SecretFrame(Frame::probe(WORKER_NONCE)),
            &Deadline::after(Duration::from_millis(40)),
        ) {
            Ok(_) => panic!("stalled worker unexpectedly completed"),
            Err(failure) => failure,
        };
        assert_eq!(failure.status, Status::Timeout);
        assert!(started.elapsed() < Duration::from_secs(1));

        let proc_path = PathBuf::from(format!("/proc/{pid}"));
        let reap_deadline = Instant::now() + Duration::from_secs(1);
        while proc_path.exists() && Instant::now() < reap_deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !proc_path.exists(),
            "timed-out worker was not killed and reaped"
        );
    }

    #[test]
    fn replacing_and_dropping_selection_owner_kills_and_reaps_workers() {
        fn sleeping_owner() -> (PersistentOwner, u32) {
            let child = Command::new("/usr/bin/sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let pid = child.id();
            (
                PersistentOwner {
                    worker: WorkerProcess {
                        child: Some(child),
                        input: None,
                        output: None,
                    },
                },
                pid,
            )
        }

        fn wait_until_reaped(pid: u32) {
            let proc_path = PathBuf::from(format!("/proc/{pid}"));
            let deadline = Instant::now() + Duration::from_secs(1);
            while proc_path.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(!proc_path.exists(), "owner worker {pid} was not reaped");
        }

        let (first, first_pid) = sleeping_owner();
        let (second, second_pid) = sleeping_owner();
        let mut backend = WaylandClipboard { owner: Some(first) };
        assert!(backend.owner.as_mut().unwrap().is_running());

        backend.owner = Some(second);
        wait_until_reaped(first_pid);
        assert!(backend.owner.as_mut().unwrap().is_running());

        drop(backend);
        wait_until_reaped(second_pid);
    }

    #[test]
    fn fixed_dispatch_mirrors_nonce_and_rejects_response_directions() {
        let nonce = [0x42; 16];
        let mut backend = mock(Ok((Mime::Text, b"guest text".to_vec())));
        let (mut response, _) =
            dispatch(&mut backend, Frame::get(nonce), &Deadline::new()).unwrap();
        assert_eq!(response.kind, Kind::GetResult);
        assert_eq!(response.mime, Mime::Text);
        assert_eq!(response.nonce, nonce);
        assert_eq!(response.payload, b"guest text");
        response.payload.zeroize();
        response.nonce.zeroize();

        let invalid = Frame::error(Kind::GetResult, nonce, Status::InvalidRequest);
        assert!(dispatch(&mut backend, invalid, &Deadline::new()).is_none());
    }

    #[test]
    fn dispatch_supports_only_put_get_and_probe_shapes() {
        let nonce = [0x33; 16];
        let mut backend = mock(Err(Failure::new(Status::UnsupportedMime, "empty")));
        let request = Frame::put(nonce, Mime::Text, b"host text".to_vec()).unwrap();
        let (response, audit) = dispatch(&mut backend, request, &Deadline::new()).unwrap();
        assert_eq!(response.kind, Kind::PutResult);
        assert_eq!(response.status, Status::Ok);
        assert_eq!(audit.bytes, b"host text".len());
        assert_eq!(backend.put.as_ref().unwrap().1, b"host text");

        let (response, _) = dispatch(&mut backend, Frame::probe(nonce), &Deadline::new()).unwrap();
        assert_eq!(response.kind, Kind::ProbeResult);
        assert_eq!(response.status, Status::Ok);
        assert_eq!(backend.probes, 1);
    }

    #[test]
    fn offer_metadata_is_bounded_before_selection() {
        let too_many = vec!["text/plain".to_owned(); MAX_MIME_OFFERS + 1];
        assert_eq!(
            validate_offer_metadata(&too_many).unwrap_err().status,
            Status::TooLarge
        );
        let too_long = vec!["x".repeat(MAX_MIME_METADATA_BYTES + 1)];
        assert_eq!(
            validate_offer_metadata(&too_long).unwrap_err().status,
            Status::TooLarge
        );
    }
}
