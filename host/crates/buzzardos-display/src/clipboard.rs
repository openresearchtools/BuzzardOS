// SPDX-License-Identifier: AGPL-3.0-or-later

//! Host-side client for the guest-owned, one-shot clipboard endpoint.
//!
//! No code in this module can read the host clipboard. GTK performs that read
//! only in direct response to a native header action and passes an owned byte
//! snapshot here.

use std::fs;
use std::io::{BufReader, Cursor, Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use buzzardos_clipboard_protocol::{
    Frame, IO_TIMEOUT_SECONDS, Kind, MAX_IMAGE_BYTES, MAX_IMAGE_DIMENSION, MAX_IMAGE_PIXELS,
    MAX_TEXT_BYTES, Mime, Status, read_frame, write_frame,
};
use image::codecs::png::{PngDecoder, PngEncoder};
use image::codecs::webp::WebPDecoder;
use image::{
    ColorType, DynamicImage, GenericImageView, ImageDecoder, ImageEncoder, ImageFormat,
    ImageReader, Limits,
};
use nix::errno::Errno;
use nix::sys::socket::{
    AddressFamily, SockFlag, SockType, UnixAddr, connect, getsockopt, socket, sockopt,
};
use nix::unistd::{Gid, Uid};

const SUPPORTED_IMAGE_FORMATS: [ImageFormat; 5] = [
    ImageFormat::Png,
    ImageFormat::Jpeg,
    ImageFormat::WebP,
    ImageFormat::Bmp,
    ImageFormat::Tiff,
];
const IMAGE_DECODE_BYTES: u64 = 512 * 1024 * 1024;
pub(crate) const IMAGE_WORKER_PNG_ARG: &str = "--buzzardos-clipboard-image-png";
pub(crate) const IMAGE_WORKER_RAW_ARG: &str = "--buzzardos-clipboard-image-raw";
const RAW_IMAGE_MAGIC: [u8; 8] = *b"WBRAW001";
const RAW_IMAGE_HEADER_BYTES: usize = 32;

struct WipingArray<const N: usize>([u8; N]);

impl<const N: usize> Drop for WipingArray<N> {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct WipingVec(Vec<u8>);

impl Drop for WipingVec {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct DeadlineStream<'a> {
    stream: &'a UnixStream,
    deadline: Instant,
}

impl DeadlineStream<'_> {
    fn remaining(&self) -> std::io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "clipboard transaction deadline expired",
                )
            })
    }
}

impl Read for DeadlineStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(buffer)
    }
}

impl Write for DeadlineStream<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.flush()
    }
}

#[derive(Debug)]
pub(crate) enum ClipboardValue {
    Text(Vec<u8>),
    Png {
        encoded: Vec<u8>,
        decoded: Option<DecodedImage>,
    },
}

#[derive(Debug)]
pub(crate) struct DecodedImage {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) stride: usize,
    pub(crate) rgba: Vec<u8>,
}

impl DecodedImage {
    pub(crate) fn take_rgba(&mut self) -> Vec<u8> {
        mem::take(&mut self.rgba)
    }
}

impl Drop for DecodedImage {
    fn drop(&mut self) {
        self.rgba.fill(0);
    }
}

impl ClipboardValue {
    pub(crate) fn mime(&self) -> Mime {
        match self {
            Self::Text(_) => Mime::Text,
            Self::Png { .. } => Mime::Png,
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Self::Text(bytes) => bytes,
            Self::Png { encoded, .. } => encoded,
        }
    }

    pub(crate) fn take_bytes(&mut self) -> Vec<u8> {
        match self {
            Self::Text(bytes) => mem::take(bytes),
            Self::Png { encoded, .. } => mem::take(encoded),
        }
    }

    pub(crate) fn take_decoded_image(&mut self) -> Option<DecodedImage> {
        match self {
            Self::Text(_) => None,
            Self::Png { decoded, .. } => decoded.take(),
        }
    }

    pub(crate) fn install_decoded_image(&mut self, image: DecodedImage) -> Result<()> {
        match self {
            Self::Png { decoded, .. } if decoded.is_none() => {
                *decoded = Some(image);
                Ok(())
            }
            Self::Png { .. } => bail!("clipboard image pixels were already installed"),
            Self::Text(_) => bail!("plain text cannot carry decoded image pixels"),
        }
    }
}

impl Drop for ClipboardValue {
    fn drop(&mut self) {
        match self {
            Self::Text(bytes) => bytes.fill(0),
            Self::Png { encoded, .. } => encoded.fill(0),
        }
    }
}

pub(crate) fn validated_text(mut bytes: Vec<u8>) -> Result<ClipboardValue> {
    if bytes.len() > MAX_TEXT_BYTES {
        bytes.fill(0);
        bail!(
            "plain text exceeds the {} MiB clipboard limit",
            MAX_TEXT_BYTES / 1024 / 1024
        );
    }
    if std::str::from_utf8(&bytes).is_err() {
        bytes.fill(0);
        bail!("plain text is not valid UTF-8");
    }
    if bytes.contains(&0) {
        bytes.fill(0);
        bail!("plain text contains an embedded NUL character");
    }
    Ok(ClipboardValue::Text(bytes))
}

/// Decode a common native still-image offering and return the one canonical
/// PNG representation used on the private wire. Nothing is written to disk.
pub(crate) fn canonical_image(bytes: &[u8]) -> Result<ClipboardValue> {
    canonical_image_inner(bytes, false)
}

fn canonical_image_for_host(bytes: &[u8]) -> Result<ClipboardValue> {
    canonical_image_inner(bytes, true)
}

pub(crate) fn png_from_image_worker(mut encoded: Vec<u8>) -> Result<ClipboardValue> {
    if encoded.is_empty() || encoded.len() > MAX_IMAGE_BYTES {
        encoded.fill(0);
        bail!("clipboard image worker returned an invalid PNG length");
    }
    if !encoded.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        encoded.fill(0);
        bail!("clipboard image worker returned an invalid PNG signature");
    }
    Ok(ClipboardValue::Png {
        encoded,
        decoded: None,
    })
}

pub(crate) fn decoded_from_image_worker(mut output: Vec<u8>) -> Result<DecodedImage> {
    let result = (|| {
        if output.len() < RAW_IMAGE_HEADER_BYTES || output[..8] != RAW_IMAGE_MAGIC {
            bail!("clipboard image worker returned an invalid raw-image header");
        }
        let width = u32::from_be_bytes(output[8..12].try_into().expect("fixed width field"));
        let height = u32::from_be_bytes(output[12..16].try_into().expect("fixed height field"));
        validate_image_dimensions(width, height)?;
        let stride_u64 = u64::from_be_bytes(output[16..24].try_into().expect("fixed stride field"));
        let length_u64 = u64::from_be_bytes(output[24..32].try_into().expect("fixed length field"));
        let stride = usize::try_from(stride_u64).context("raw clipboard stride is too large")?;
        let length = usize::try_from(length_u64).context("raw clipboard image is too large")?;
        let expected_stride = width as usize * 4;
        let expected_length = expected_stride
            .checked_mul(height as usize)
            .context("raw clipboard image length overflow")?;
        if stride != expected_stride
            || length != expected_length
            || output.len() != RAW_IMAGE_HEADER_BYTES + expected_length
        {
            bail!("clipboard image worker returned inconsistent raw-image geometry");
        }
        let rgba = output.split_off(RAW_IMAGE_HEADER_BYTES);
        Ok(DecodedImage {
            width,
            height,
            stride,
            rgba,
        })
    })();
    output.fill(0);
    result
}

/// Runs before GTK initialization when this fixed binary is re-executed in
/// its private image-worker mode. It has no filesystem or desktop protocol
/// input: one bounded stdin image becomes either canonical PNG or raw RGBA on
/// stdout, and every owned intermediate is wiped.
pub(crate) fn maybe_run_image_worker() -> Option<Result<()>> {
    let mut arguments = std::env::args_os().skip(1);
    let mode = match arguments.next().as_deref() {
        Some(value) if value == std::ffi::OsStr::new(IMAGE_WORKER_PNG_ARG) => false,
        Some(value) if value == std::ffi::OsStr::new(IMAGE_WORKER_RAW_ARG) => true,
        _ => return None,
    };
    if arguments.next().is_some() {
        return Some(Err(anyhow::anyhow!(
            "clipboard image worker accepts no additional arguments"
        )));
    }
    Some(run_image_worker(mode))
}

fn run_image_worker(raw_output: bool) -> Result<()> {
    apply_image_worker_limits()?;
    let mut source = WipingVec(Vec::new());
    std::io::stdin()
        .lock()
        .take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut source.0)
        .context("reading bounded clipboard worker input")?;
    if source.0.len() > MAX_IMAGE_BYTES {
        bail!("clipboard worker input exceeds the image limit");
    }
    let mut value = if raw_output {
        canonical_image_for_host(&source.0)?
    } else {
        canonical_image(&source.0)?
    };
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    if raw_output {
        let decoded = value
            .take_decoded_image()
            .context("clipboard worker did not produce decoded pixels")?;
        let mut header = WipingArray([0_u8; RAW_IMAGE_HEADER_BYTES]);
        header.0[..8].copy_from_slice(&RAW_IMAGE_MAGIC);
        header.0[8..12].copy_from_slice(&decoded.width.to_be_bytes());
        header.0[12..16].copy_from_slice(&decoded.height.to_be_bytes());
        header.0[16..24].copy_from_slice(&(decoded.stride as u64).to_be_bytes());
        header.0[24..32].copy_from_slice(&(decoded.rgba.len() as u64).to_be_bytes());
        stdout
            .write_all(&header.0)
            .context("writing clipboard raw-image header")?;
        stdout
            .write_all(&decoded.rgba)
            .context("writing clipboard raw-image pixels")?;
    } else {
        stdout
            .write_all(value.bytes())
            .context("writing canonical clipboard PNG")?;
    }
    stdout.flush().context("flushing clipboard image output")
}

fn apply_image_worker_limits() -> Result<()> {
    // The worker is the same pinned executable. These limits bound hostile
    // decoder behavior independently of the GTK process; the parent also
    // enforces wall time and force-kills on cancellation.
    for (resource, soft_limit) in [
        (libc::RLIMIT_AS, 1024_u64 * 1024 * 1024),
        (libc::RLIMIT_CPU, 4),
        (libc::RLIMIT_FSIZE, 384_u64 * 1024 * 1024),
        (libc::RLIMIT_NOFILE, 16),
        (libc::RLIMIT_CORE, 0),
    ] {
        let mut current = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `current` is valid writable storage and `resource` is one
        // of the fixed RLIMIT constants above.
        if unsafe { libc::getrlimit(resource, &mut current) } != 0 {
            return Err(std::io::Error::last_os_error()).context("reading image worker limit");
        }
        current.rlim_cur = current.rlim_max.min(soft_limit as libc::rlim_t);
        // SAFETY: the limit structure is initialized and only lowers the soft
        // limit for this disposable worker process.
        if unsafe { libc::setrlimit(resource, &current) } != 0 {
            return Err(std::io::Error::last_os_error()).context("setting image worker limit");
        }
    }
    // SAFETY: PR_SET_DUMPABLE with zero takes no pointer arguments.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).context("disabling image worker core access");
    }
    Ok(())
}

fn canonical_image_inner(bytes: &[u8], retain_decoded: bool) -> Result<ClipboardValue> {
    if bytes.is_empty() {
        bail!("clipboard image is empty");
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!(
            "source image exceeds the {} MiB clipboard limit",
            MAX_IMAGE_BYTES / 1024 / 1024
        );
    }
    let format = image::guess_format(bytes).context("clipboard image format is invalid")?;
    if !SUPPORTED_IMAGE_FORMATS.contains(&format) {
        bail!("clipboard image format {format:?} is not supported");
    }
    reject_non_still_content(format, bytes)?;

    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .context("reading clipboard image dimensions")?;
    validate_image_dimensions(width, height)?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(image_limits());
    let mut image = reader.decode().context("decoding clipboard still image")?;
    let decoded = image.dimensions();
    if decoded != (width, height) {
        zero_dynamic_image(&mut image);
        bail!("clipboard image dimensions changed during decode");
    }

    let mut rgba = image.to_rgba8();
    zero_dynamic_image(&mut image);
    let stride = width as usize * 4;
    let mut encoded = BoundedWriter::new(MAX_IMAGE_BYTES);
    let encode_result = PngEncoder::new(&mut encoded).write_image(
        rgba.as_raw(),
        width,
        height,
        ColorType::Rgba8.into(),
    );
    if encoded.exceeded {
        rgba.as_mut().fill(0);
        bail!(
            "canonical PNG exceeds the {} MiB clipboard limit",
            MAX_IMAGE_BYTES / 1024 / 1024
        );
    }
    if let Err(error) = encode_result {
        rgba.as_mut().fill(0);
        return Err(error).context("encoding canonical clipboard PNG");
    }
    let decoded = if retain_decoded {
        Some(DecodedImage {
            width,
            height,
            stride,
            rgba: rgba.into_raw(),
        })
    } else {
        rgba.as_mut().fill(0);
        None
    };
    Ok(ClipboardValue::Png {
        encoded: encoded.take(),
        decoded,
    })
}

fn image_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(IMAGE_DECODE_BYTES);
    limits
}

pub(crate) fn validate_canonical_value(mime: Mime, mut payload: Vec<u8>) -> Result<ClipboardValue> {
    match mime {
        Mime::Text => validated_text(payload),
        Mime::Png => {
            if payload.is_empty()
                || !payload.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a])
            {
                payload.fill(0);
                bail!("guest clipboard returned an invalid canonical PNG signature");
            }
            Ok(ClipboardValue::Png {
                encoded: payload,
                decoded: None,
            })
        }
        Mime::None => {
            payload.fill(0);
            bail!("clipboard response does not contain a supported value")
        }
    }
}

pub(crate) fn validate_image_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("clipboard image has an empty dimension");
    }
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        bail!(
            "clipboard image {}x{} exceeds the {}-pixel edge limit",
            width,
            height,
            MAX_IMAGE_DIMENSION
        );
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_IMAGE_PIXELS {
        bail!("clipboard image contains {pixels} pixels, above the {MAX_IMAGE_PIXELS}-pixel limit");
    }
    Ok(())
}

fn reject_non_still_content(format: ImageFormat, bytes: &[u8]) -> Result<()> {
    match format {
        ImageFormat::Png => {
            let decoder = PngDecoder::new(BufReader::new(Cursor::new(bytes)))
                .context("parsing clipboard PNG")?;
            if decoder
                .is_apng()
                .context("checking PNG animation metadata")?
            {
                bail!("animated PNG clipboard content is not supported");
            }
            validate_image_dimensions(decoder.dimensions().0, decoder.dimensions().1)
        }
        ImageFormat::WebP => {
            let decoder = WebPDecoder::new(BufReader::new(Cursor::new(bytes)))
                .context("parsing clipboard WebP")?;
            if decoder.has_animation() {
                bail!("animated WebP clipboard content is not supported");
            }
            validate_image_dimensions(decoder.dimensions().0, decoder.dimensions().1)
        }
        ImageFormat::Tiff => {
            let mut decoder = tiff::decoder::Decoder::new(Cursor::new(bytes))
                .context("parsing clipboard TIFF")?;
            if decoder.more_images() {
                bail!("multi-page TIFF clipboard content is not supported");
            }
            let (width, height) = decoder
                .dimensions()
                .context("reading clipboard TIFF dimensions")?;
            validate_image_dimensions(width, height)
        }
        _ => Ok(()),
    }
}

fn zero_dynamic_image(image: &mut DynamicImage) {
    match image {
        DynamicImage::ImageLuma8(buffer) => buffer.as_mut().fill(0),
        DynamicImage::ImageLumaA8(buffer) => buffer.as_mut().fill(0),
        DynamicImage::ImageRgb8(buffer) => buffer.as_mut().fill(0),
        DynamicImage::ImageRgba8(buffer) => buffer.as_mut().fill(0),
        DynamicImage::ImageLuma16(buffer) => buffer.as_mut().fill(0),
        DynamicImage::ImageLumaA16(buffer) => buffer.as_mut().fill(0),
        DynamicImage::ImageRgb16(buffer) => buffer.as_mut().fill(0),
        DynamicImage::ImageRgba16(buffer) => buffer.as_mut().fill(0),
        DynamicImage::ImageRgb32F(buffer) => buffer.as_mut().fill(0.0),
        DynamicImage::ImageRgba32F(buffer) => buffer.as_mut().fill(0.0),
        _ => {}
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }

    fn take(&mut self) -> Vec<u8> {
        mem::take(&mut self.bytes)
    }
}

impl Drop for BoundedWriter {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(new_length) = self.bytes.len().checked_add(buffer.len()) else {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bounded clipboard buffer exceeded",
            ));
        };
        if new_length > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bounded clipboard buffer exceeded",
            ));
        }
        self.bytes
            .try_reserve(buffer.len())
            .map_err(|_| std::io::Error::other("clipboard buffer memory allocation failed"))?;
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

const PRIVATE_SOCKET_MODE: u32 = 0o666;
const PRIVATE_READY_MODE: u32 = 0o644;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndpointKind {
    Directory,
    Socket,
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    links: u64,
    uid: u32,
    gid: u32,
    mode: u32,
}

impl FileIdentity {
    fn capture(path: &Path, kind: EndpointKind) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspecting clipboard runtime path {}", path.display()))?;
        let type_matches = match kind {
            EndpointKind::Directory => metadata.is_dir(),
            EndpointKind::Socket => metadata.file_type().is_socket(),
            EndpointKind::File => metadata.file_type().is_file(),
        };
        if metadata.file_type().is_symlink() || !type_matches {
            bail!("clipboard runtime endpoint has an invalid filesystem type");
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            links: metadata.nlink(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.permissions().mode() & 0o777,
        })
    }
}

/// Filesystem identity captured synchronously for the exact ready endpoint
/// that enabled one native clipboard action. A later worker may connect only
/// while every inode and ownership field still matches this snapshot.
#[derive(Clone, Debug)]
pub(crate) struct EndpointSnapshot {
    socket_path: PathBuf,
    ready_path: PathBuf,
    runtime_root: FileIdentity,
    parent: FileIdentity,
    socket: FileIdentity,
    ready: FileIdentity,
}

impl EndpointSnapshot {
    pub(crate) fn capture(socket_path: &Path, ready_path: &Path) -> Result<Self> {
        let socket_parent = socket_path
            .parent()
            .context("guest clipboard endpoint has no runtime directory")?;
        if ready_path.parent() != Some(socket_parent) {
            bail!("guest clipboard endpoint and readiness marker are not colocated");
        }
        let runtime_root_path = socket_parent
            .parent()
            .context("guest clipboard endpoint has no private runtime root")?;
        let runtime_root = FileIdentity::capture(runtime_root_path, EndpointKind::Directory)?;
        let parent = FileIdentity::capture(socket_parent, EndpointKind::Directory)?;
        let socket = FileIdentity::capture(socket_path, EndpointKind::Socket)?;
        let ready = FileIdentity::capture(ready_path, EndpointKind::File)?;
        let expected_uid = Uid::effective().as_raw();
        let expected_gid = Gid::effective().as_raw();
        if runtime_root.uid != expected_uid
            || runtime_root.gid != expected_gid
            || runtime_root.mode != 0o700
            || parent.uid != expected_uid
            || parent.gid != expected_gid
            || parent.mode != 0o777
        {
            bail!("guest clipboard runtime directory is not private to the host user");
        }
        if socket.uid != ready.uid
            || socket.gid != ready.gid
            || socket.mode != PRIVATE_SOCKET_MODE
            || ready.mode != PRIVATE_READY_MODE
            || socket.links != 1
            || ready.links != 1
        {
            bail!("guest clipboard endpoint ownership or permissions are invalid");
        }
        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            ready_path: ready_path.to_path_buf(),
            runtime_root,
            parent,
            socket,
            ready,
        })
    }

    pub(crate) fn is_current(&self) -> bool {
        self.matches_current().is_ok()
    }

    fn matches_current(&self) -> Result<()> {
        let parent_path = self
            .socket_path
            .parent()
            .context("guest clipboard endpoint has no runtime directory")?;
        let runtime_root_path = parent_path
            .parent()
            .context("guest clipboard endpoint has no private runtime root")?;
        if FileIdentity::capture(runtime_root_path, EndpointKind::Directory)? != self.runtime_root
            || FileIdentity::capture(parent_path, EndpointKind::Directory)? != self.parent
            || FileIdentity::capture(&self.socket_path, EndpointKind::Socket)? != self.socket
            || FileIdentity::capture(&self.ready_path, EndpointKind::File)? != self.ready
        {
            bail!("guest clipboard endpoint changed during the transaction");
        }
        Ok(())
    }

    pub(crate) fn begin_connect(self) -> Result<PendingEndpointConnection> {
        self.matches_current()?;
        let descriptor = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            None,
        )
        .context("creating one-shot clipboard socket")?;
        let address =
            UnixAddr::new(&self.socket_path).context("encoding guest clipboard endpoint path")?;
        let connected = match connect(descriptor.as_raw_fd(), &address) {
            Ok(()) => true,
            Err(Errno::EINPROGRESS) => false,
            // AF_UNIX uses EAGAIN when a nonblocking listener backlog is
            // full; unlike EINPROGRESS it does not give this transaction a
            // path-bound connection that can safely be completed later.
            Err(Errno::EAGAIN) => bail!("guest clipboard agent connection queue is full"),
            Err(error) => return Err(error).context("connecting to the guest clipboard agent"),
        };
        // The nonblocking connect syscall above resolves the pathname now, in
        // the native click callback. A delayed worker receives only this fd;
        // it has no pathname with which to reach a replacement lifecycle.
        self.matches_current()?;
        Ok(PendingEndpointConnection {
            descriptor,
            connected,
            snapshot: self,
        })
    }

    #[cfg(test)]
    fn connect(self) -> Result<ConnectedEndpoint> {
        self.begin_connect()?.finish()
    }
}

#[derive(Debug)]
pub(crate) struct PendingEndpointConnection {
    descriptor: OwnedFd,
    connected: bool,
    snapshot: EndpointSnapshot,
}

impl PendingEndpointConnection {
    pub(crate) fn finish(self) -> Result<ConnectedEndpoint> {
        if !self.connected {
            wait_for_nonblocking_connect(&self.descriptor)?;
        }
        let connection = UnixStream::from(self.descriptor);
        connection
            .set_nonblocking(false)
            .context("making the connected clipboard socket blocking")?;
        validate_connected_peer(&connection, &self.snapshot.socket)?;
        self.snapshot.matches_current()?;
        Ok(ConnectedEndpoint {
            connection,
            snapshot: self.snapshot,
        })
    }
}

#[derive(Debug)]
pub(crate) struct ConnectedEndpoint {
    connection: UnixStream,
    snapshot: EndpointSnapshot,
}

impl ConnectedEndpoint {
    pub(crate) fn cancel_handle(&self) -> Result<UnixStream> {
        self.connection
            .try_clone()
            .context("duplicating clipboard cancellation descriptor")
    }

    pub(crate) fn is_current(&self) -> bool {
        self.snapshot.is_current()
    }
}

pub(crate) fn agent_ready(socket: &Path, ready: &Path) -> bool {
    EndpointSnapshot::capture(socket, ready).is_ok()
}

pub(crate) fn put(
    mut endpoint: ConnectedEndpoint,
    nonce: [u8; 16],
    mut value: ClipboardValue,
) -> Result<()> {
    if !endpoint.is_current() {
        bail!("guest clipboard endpoint changed before the snapshot was sent");
    }
    let mut request = Frame::put(nonce, value.mime(), value.take_bytes())?;
    let response = exchange(&mut endpoint.connection, &request);
    request.payload.fill(0);
    let response = response?;
    require_response(&response, Kind::PutResult, nonce)?;
    if response.status != Status::Ok {
        bail!(
            "guest clipboard rejected the snapshot ({})",
            response.status.code()
        );
    }
    Ok(())
}

pub(crate) fn get(mut endpoint: ConnectedEndpoint, nonce: [u8; 16]) -> Result<ClipboardValue> {
    if !endpoint.is_current() {
        bail!("guest clipboard endpoint changed before the snapshot was requested");
    }
    let mut response = exchange(&mut endpoint.connection, &Frame::get(nonce))?;
    require_response(&response, Kind::GetResult, nonce)?;
    if response.status != Status::Ok {
        bail!(
            "guest clipboard snapshot failed ({})",
            response.status.code()
        );
    }
    validate_canonical_value(response.mime, response.take_payload())
}

#[allow(dead_code)]
pub(crate) fn probe(mut endpoint: ConnectedEndpoint, nonce: [u8; 16]) -> Result<()> {
    let response = exchange(&mut endpoint.connection, &Frame::probe(nonce))?;
    require_response(&response, Kind::ProbeResult, nonce)?;
    if response.status != Status::Ok {
        bail!(
            "guest clipboard agent is not ready ({})",
            response.status.code()
        );
    }
    Ok(())
}

fn exchange(connection: &mut UnixStream, request: &Frame) -> Result<Frame> {
    exchange_with_timeout(connection, request, Duration::from_secs(IO_TIMEOUT_SECONDS))
}

fn exchange_with_timeout(
    connection: &mut UnixStream,
    request: &Frame,
    timeout: Duration,
) -> Result<Frame> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("clipboard transaction deadline overflow")?;
    let mut transport = DeadlineStream {
        stream: connection,
        deadline,
    };
    write_frame(&mut transport, request).context("sending one-shot clipboard request")?;
    read_frame(&mut transport).context("reading one-shot clipboard response")
}

fn validate_connected_peer(connection: &UnixStream, socket: &FileIdentity) -> Result<()> {
    let credentials = getsockopt(connection, sockopt::PeerCredentials)
        .context("authenticating guest clipboard peer credentials")?;
    if credentials.pid() <= 0 || socket.uid != credentials.uid() || socket.gid != credentials.gid()
    {
        bail!("guest clipboard peer credentials do not match the confined endpoint");
    }
    Ok(())
}

fn wait_for_nonblocking_connect(descriptor: &OwnedFd) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(IO_TIMEOUT_SECONDS);
    loop {
        let now = Instant::now();
        if now >= deadline {
            bail!("connecting to the guest clipboard agent timed out");
        }
        let remaining = deadline.duration_since(now);
        let timeout_ms = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
        let mut event = libc::pollfd {
            fd: descriptor.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: `event` is one initialized pollfd and `descriptor`
        // remains open for the duration of this call.
        let result = unsafe { libc::poll(&mut event, 1, timeout_ms) };
        if result == 0 {
            bail!("connecting to the guest clipboard agent timed out");
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("waiting for the guest clipboard connection");
        }
        break;
    }
    let mut socket_error: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the output pointers reference initialized writable
    // storage and the descriptor is an open AF_UNIX stream socket.
    let result = unsafe {
        libc::getsockopt(
            descriptor.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&mut socket_error as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("checking guest clipboard connection result");
    }
    if socket_error != 0 {
        return Err(std::io::Error::from_raw_os_error(socket_error))
            .context("connecting to the guest clipboard agent");
    }
    Ok(())
}

fn require_response(response: &Frame, expected: Kind, nonce: [u8; 16]) -> Result<()> {
    if response.kind != expected {
        bail!("guest clipboard returned the wrong response kind");
    }
    if response.nonce != nonce {
        bail!("guest clipboard returned a response for a different request");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzzardos_clipboard_protocol::{read_frame, write_frame};
    use image::{DynamicImage, Rgba, RgbaImage};
    use std::io::ErrorKind;
    use std::net::Shutdown;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;
    use std::thread;
    use tiff::encoder::{TiffEncoder, colortype};

    fn test_png(width: u32, height: u32) -> Vec<u8> {
        test_image(ImageFormat::Png, width, height)
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

    fn secure_endpoint(
        temporary: &tempfile::TempDir,
    ) -> (PathBuf, PathBuf, UnixListener, EndpointSnapshot) {
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let exchange = temporary.path().join("host");
        fs::create_dir(&exchange).unwrap();
        fs::set_permissions(&exchange, fs::Permissions::from_mode(0o777)).unwrap();
        let socket = exchange.join("clipboard.sock");
        let ready = exchange.join("clipboard-ready");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(PRIVATE_SOCKET_MODE)).unwrap();
        fs::write(&ready, b"").unwrap();
        fs::set_permissions(&ready, fs::Permissions::from_mode(PRIVATE_READY_MODE)).unwrap();
        let snapshot = EndpointSnapshot::capture(&socket, &ready).unwrap();
        (socket, ready, listener, snapshot)
    }

    #[test]
    fn canonicalizes_still_image_without_a_file() {
        let value = canonical_image(&test_png(7, 5)).unwrap();
        assert_eq!(value.mime(), Mime::Png);
        let decoded = image::load_from_memory(value.bytes()).unwrap();
        assert_eq!(decoded.dimensions(), (7, 5));
        assert_eq!(
            decoded.to_rgba8().get_pixel(0, 0).0,
            [0x22, 0x66, 0xaa, 0x80]
        );
    }

    #[test]
    fn canonicalizes_every_supported_native_still_image_offering() {
        for format in SUPPORTED_IMAGE_FORMATS {
            let source = test_image(format, 9, 6);
            let value = canonical_image(&source).unwrap();
            assert_eq!(value.mime(), Mime::Png, "source format {format:?}");
            let decoded = image::load_from_memory(value.bytes()).unwrap();
            assert_eq!(decoded.dimensions(), (9, 6), "source format {format:?}");
        }
    }

    #[test]
    fn rejects_invalid_text_and_image_dimensions() {
        assert!(validated_text(vec![0xff]).is_err());
        assert!(validated_text(b"contains\0nul".to_vec()).is_err());
        assert!(validate_image_dimensions(0, 1).is_err());
        assert!(validate_image_dimensions(MAX_IMAGE_DIMENSION + 1, 1).is_err());

        let mut writer = BoundedWriter::new(3);
        assert!(writer.write_all(b"four").is_err());
        assert!(writer.exceeded);
        assert!(writer.bytes.is_empty());
    }

    #[test]
    fn rejects_multi_page_tiff_as_non_still_content() {
        let mut source = Cursor::new(Vec::new());
        {
            let mut encoder = TiffEncoder::new(&mut source).unwrap();
            encoder
                .write_image::<colortype::Gray8>(1, 1, &[0x11])
                .unwrap();
            encoder
                .write_image::<colortype::Gray8>(1, 1, &[0x22])
                .unwrap();
        }
        assert!(canonical_image(source.get_ref()).is_err());
    }

    #[test]
    fn readiness_rejects_regular_files() {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("clipboard.sock");
        let ready = temporary.path().join("clipboard-ready");
        fs::write(&socket, b"not a socket").unwrap();
        fs::write(&ready, b"ready").unwrap();
        assert!(!agent_ready(&socket, &ready));
    }

    #[test]
    fn guest_snapshot_requires_the_exact_host_created_nonce() {
        let temporary = tempfile::tempdir().unwrap();
        let (_, _, listener, snapshot) = secure_endpoint(&temporary);
        let worker = thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let request = read_frame(&mut connection).unwrap();
            assert_eq!(request.kind, Kind::Get);
            let response = Frame::result(
                Kind::GetResult,
                [0x99; 16],
                Status::Ok,
                Mime::Text,
                b"guest text".to_vec(),
            )
            .unwrap();
            write_frame(&mut connection, &response).unwrap();
        });
        let error = get(snapshot.connect().unwrap(), [0x42; 16]).unwrap_err();
        worker.join().unwrap();
        assert!(error.to_string().contains("different request"));
    }

    #[test]
    fn guest_snapshot_round_trips_only_the_supported_value() {
        let temporary = tempfile::tempdir().unwrap();
        let (_, _, listener, snapshot) = secure_endpoint(&temporary);
        let nonce = [0x33; 16];
        let worker = thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let request = read_frame(&mut connection).unwrap();
            assert_eq!(request, Frame::get(nonce));
            let response = Frame::result(
                Kind::GetResult,
                nonce,
                Status::Ok,
                Mime::Text,
                "guest — 日本語 🦅".as_bytes().to_vec(),
            )
            .unwrap();
            write_frame(&mut connection, &response).unwrap();
        });
        let value = get(snapshot.connect().unwrap(), nonce).unwrap();
        worker.join().unwrap();
        assert_eq!(value.mime(), Mime::Text);
        assert_eq!(value.bytes(), "guest — 日本語 🦅".as_bytes());
    }

    #[test]
    fn endpoint_replacement_is_rejected_before_connect() {
        let temporary = tempfile::tempdir().unwrap();
        let (socket, _, original, snapshot) = secure_endpoint(&temporary);
        let old_socket = temporary.path().join("old.sock");
        fs::rename(&socket, &old_socket).unwrap();
        let replacement = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(PRIVATE_SOCKET_MODE)).unwrap();

        let error = snapshot.connect().unwrap_err();
        assert!(error.to_string().contains("changed"));
        drop((original, replacement));
    }

    #[test]
    fn delayed_finisher_cannot_connect_to_a_replacement_lifecycle() {
        let temporary = tempfile::tempdir().unwrap();
        let (socket, _, original, snapshot) = secure_endpoint(&temporary);
        // Path resolution happens here, synchronously with the click.
        let pending = snapshot.begin_connect().unwrap();
        fs::rename(&socket, temporary.path().join("old.sock")).unwrap();
        let replacement = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(PRIVATE_SOCKET_MODE)).unwrap();
        replacement.set_nonblocking(true).unwrap();

        // The delayed worker can only finish the already-connected fd. It
        // detects the lifecycle identity change and never opens `socket`.
        let error = pending.finish().unwrap_err();
        assert!(error.to_string().contains("changed"));
        assert_eq!(
            replacement.accept().unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        drop(original);
    }

    #[test]
    fn connected_transaction_never_reopens_a_replacement_path() {
        let temporary = tempfile::tempdir().unwrap();
        let (socket, _, original, snapshot) = secure_endpoint(&temporary);
        let mut endpoint = snapshot.connect().unwrap();
        let nonce = [0x57; 16];
        let original_worker = thread::spawn(move || {
            let (mut connection, _) = original.accept().unwrap();
            let request = read_frame(&mut connection).unwrap();
            assert_eq!(request, Frame::get(nonce));
            write_frame(
                &mut connection,
                &Frame::result(
                    Kind::GetResult,
                    nonce,
                    Status::Ok,
                    Mime::Text,
                    b"original endpoint".to_vec(),
                )
                .unwrap(),
            )
            .unwrap();
        });

        fs::rename(&socket, temporary.path().join("old.sock")).unwrap();
        let replacement = UnixListener::bind(&socket).unwrap();
        replacement.set_nonblocking(true).unwrap();
        let response = exchange(&mut endpoint.connection, &Frame::get(nonce)).unwrap();
        original_worker.join().unwrap();
        assert_eq!(response.payload, b"original endpoint");
        assert_eq!(
            replacement.accept().unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
    }

    #[test]
    fn connected_exchange_has_a_deterministic_deadline() {
        let temporary = tempfile::tempdir().unwrap();
        let (_, _, listener, snapshot) = secure_endpoint(&temporary);
        let mut endpoint = snapshot.connect().unwrap();
        let worker = thread::spawn(move || {
            let (_connection, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(100));
        });
        let started = Instant::now();
        let error = exchange_with_timeout(
            &mut endpoint.connection,
            &Frame::get([0x71; 16]),
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(error.to_string().contains("response"));
        worker.join().unwrap();
    }

    #[test]
    fn retained_descriptor_cancels_an_in_flight_transaction() {
        let temporary = tempfile::tempdir().unwrap();
        let (_, _, listener, snapshot) = secure_endpoint(&temporary);
        let endpoint = snapshot.connect().unwrap();
        let cancel = endpoint.cancel_handle().unwrap();
        let (ready_send, ready_receive) = mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            ready_send.send(()).unwrap();
            let mut byte = [0_u8; 1];
            connection.read(&mut byte).unwrap()
        });
        ready_receive.recv().unwrap();
        cancel.shutdown(Shutdown::Both).unwrap();
        assert_eq!(worker.join().unwrap(), 0);
        drop(endpoint);
    }

    #[test]
    fn raw_worker_output_requires_exact_geometry_and_length() {
        let mut output = Vec::from(RAW_IMAGE_MAGIC);
        output.extend_from_slice(&2_u32.to_be_bytes());
        output.extend_from_slice(&1_u32.to_be_bytes());
        output.extend_from_slice(&8_u64.to_be_bytes());
        output.extend_from_slice(&8_u64.to_be_bytes());
        output.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let decoded = decoded_from_image_worker(output).unwrap();
        assert_eq!((decoded.width, decoded.height, decoded.stride), (2, 1, 8));
        assert_eq!(decoded.rgba, [1, 2, 3, 4, 5, 6, 7, 8]);

        let mut malformed = Vec::from(RAW_IMAGE_MAGIC);
        malformed.extend_from_slice(&2_u32.to_be_bytes());
        malformed.extend_from_slice(&1_u32.to_be_bytes());
        malformed.extend_from_slice(&8_u64.to_be_bytes());
        malformed.extend_from_slice(&9_u64.to_be_bytes());
        malformed.extend_from_slice(&[0; 8]);
        assert!(decoded_from_image_worker(malformed).is_err());
    }
}
