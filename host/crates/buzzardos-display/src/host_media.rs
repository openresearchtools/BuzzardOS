// SPDX-License-Identifier: AGPL-3.0-or-later

//! Host halves of the three explicitly enabled media bridges.
//!
//! Podman owns the network boundary and publishes one loopback-only ephemeral
//! host port per enabled endpoint. This worker owns only fixed GStreamer
//! pipelines; it owns no container lifecycle, networking, or mounts.

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wb_core::{
    HostMediaBackend, HostMediaDevice, HostMediaKind, MachineConfig, ResourceLocator,
    discover_host_media,
};

const MAX_PIPEWIRE_DUMP_BYTES: usize = 32 * 1024 * 1024;
const MICROPHONE_APPLICATION_ID: &str = "org.openresearchtools.BuzzardOS";
const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_PIX_FMT_MJPEG: u32 = u32::from_le_bytes(*b"MJPG");
const VIDIOC_ENUM_FMT: libc::c_ulong = 0xc040_5602;
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Parser)]
#[command(name = "buzzardos-display --media-worker")]
struct WorkerArgs {
    #[arg(long)]
    machine_dir: PathBuf,
    #[arg(long)]
    endpoints: PathBuf,
    #[arg(long)]
    status: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Endpoints {
    schema: u32,
    guest_audio_output: Option<u16>,
    host_microphone: Option<u16>,
    host_camera: Option<u16>,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MediaWorkerStatus {
    pub schema: u32,
    pub guest_audio_output: bool,
    pub host_microphone: bool,
    pub host_camera: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaKind {
    GuestAudio,
    HostMicrophone,
    HostCamera,
}

impl MediaKind {
    fn name(self) -> &'static str {
        match self {
            Self::GuestAudio => "guest audio",
            Self::HostMicrophone => "host microphone",
            Self::HostCamera => "host camera",
        }
    }

    fn host_kind(self) -> HostMediaKind {
        match self {
            Self::GuestAudio => HostMediaKind::AudioSink,
            Self::HostMicrophone => HostMediaKind::Microphone,
            Self::HostCamera => HostMediaKind::Camera,
        }
    }
}

pub(crate) struct MediaWorker {
    child: Child,
}

struct MediaChannel {
    kind: MediaKind,
    port: u16,
    target: Option<String>,
    process: Option<MediaProcess>,
    error: Option<String>,
    retry_at: Instant,
}

impl MediaChannel {
    fn new(kind: MediaKind, port: u16, target: Option<&str>) -> Self {
        Self {
            kind,
            port,
            target: target.map(str::to_owned),
            process: None,
            error: None,
            retry_at: Instant::now(),
        }
    }

    fn reconcile(&mut self, resources: &ResourceLocator) {
        let kind = self.kind;
        let port = self.port;
        let target = self.target.clone();
        self.reconcile_with(|| start_pipeline(kind, port, target.as_deref(), resources));
    }

    fn reconcile_with(&mut self, start: impl FnOnce() -> Result<MediaProcess>) {
        if let Some(process) = &mut self.process {
            match process.0.try_wait() {
                Ok(None) => return,
                Ok(Some(status)) => {
                    self.error = Some(format!("{} bridge exited with {status}", self.kind.name()));
                }
                Err(error) => {
                    self.error = Some(format!("checking {} bridge: {error}", self.kind.name()));
                }
            }
            self.process.take();
            self.retry_at = Instant::now() + Duration::from_secs(1);
        }
        if Instant::now() < self.retry_at || STOP_REQUESTED.load(Ordering::Acquire) {
            return;
        }
        match start() {
            Ok(process) => {
                self.process = Some(process);
                self.error = None;
            }
            Err(error) => {
                self.error = Some(format!("{}: {error:#}", self.kind.name()));
                self.retry_at = Instant::now() + Duration::from_secs(1);
            }
        }
    }
}

// Startup can fail after the process begins (for example while confirming a
// microphone stream). Every exit path must revoke that exact child process.
struct MediaProcess(Child);

impl Drop for MediaProcess {
    fn drop(&mut self) {
        terminate(&mut self.0);
    }
}

fn channel_status(channels: &[MediaChannel]) -> MediaWorkerStatus {
    let mut status = MediaWorkerStatus {
        schema: 1,
        ..MediaWorkerStatus::default()
    };
    let mut errors = Vec::new();
    for channel in channels {
        let running = channel.process.is_some();
        match channel.kind {
            MediaKind::GuestAudio => status.guest_audio_output = running,
            MediaKind::HostMicrophone => status.host_microphone = running,
            MediaKind::HostCamera => status.host_camera = running,
        }
        if let Some(error) = &channel.error {
            errors.push(error.as_str());
        }
    }
    if !errors.is_empty() {
        status.error = Some(errors.join("; "));
    }
    status
}

impl MediaWorker {
    pub(crate) fn start(machine_dir: &Path, status_dir: &Path) -> Result<Option<Self>> {
        let config = MachineConfig::load(machine_dir)?;
        let media = &config.integrations.media;
        if !(media.guest_audio_output || media.host_microphone || media.host_camera) {
            return Ok(None);
        }
        let endpoints = status_dir.join("media-endpoints.json");
        if !endpoints.is_file() {
            bail!("Podman media endpoints are not ready");
        }
        let status = status_dir.join("media-worker.json");
        let executable = std::env::current_exe().context("locating native display executable")?;
        let parent_pid = unsafe { libc::getpid() };
        let mut command = Command::new(executable);
        command
            .arg("--media-worker")
            .arg("--machine-dir")
            .arg(machine_dir)
            .arg("--endpoints")
            .arg(endpoints)
            .arg("--status")
            .arg(status)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: only async-signal-safe libc calls run between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "native machine window exited during media startup",
                    ));
                }
                Ok(())
            });
        }
        let child = command.spawn().context("starting private media worker")?;
        Ok(Some(Self { child }))
    }
}

impl Drop for MediaWorker {
    fn drop(&mut self) {
        if self.child.try_wait().is_ok_and(|status| status.is_none()) {
            let _ = unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if self.child.try_wait().is_ok_and(|status| status.is_some()) {
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

pub(crate) fn read_status(status_dir: &Path) -> Option<MediaWorkerStatus> {
    let bytes = fs::read(status_dir.join("media-worker.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn maybe_run() -> Option<Result<()>> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new("--media-worker")) {
        return None;
    }
    let args = std::env::args_os()
        .enumerate()
        .filter_map(|(index, value)| (index != 1).then_some(value));
    Some(run_worker(WorkerArgs::parse_from(args)))
}

fn run_worker(args: WorkerArgs) -> Result<()> {
    STOP_REQUESTED.store(false, Ordering::Release);
    install_signal_handlers();
    let config = MachineConfig::load(&args.machine_dir)?;
    let endpoints: Endpoints = serde_json::from_slice(
        &fs::read(&args.endpoints)
            .with_context(|| format!("reading {}", args.endpoints.display()))?,
    )
    .context("parsing Podman media endpoints")?;
    if endpoints.schema != 1 {
        bail!("unsupported media endpoint schema {}", endpoints.schema);
    }
    let resources = ResourceLocator::discover()?;
    require_host_pipewire()?;
    let mut channels = Vec::new();
    let result = (|| -> Result<()> {
        let requested = [
            (
                MediaKind::GuestAudio,
                endpoints.guest_audio_output,
                config.integrations.media.audio_target.as_deref(),
            ),
            (
                MediaKind::HostMicrophone,
                endpoints.host_microphone,
                config.integrations.media.microphone_target.as_deref(),
            ),
            (
                MediaKind::HostCamera,
                endpoints.host_camera,
                config.integrations.media.camera_target.as_deref(),
            ),
        ];
        for (kind, port, target) in requested {
            if let Some(port) = port {
                validate_port(port)?;
                channels.push(MediaChannel::new(kind, port, target));
            }
        }
        let mut previous_status = None;
        while !STOP_REQUESTED.load(Ordering::Acquire) {
            for channel in &mut channels {
                channel.reconcile(&resources);
            }
            let status = channel_status(&channels);
            if previous_status.as_ref() != Some(&status) {
                write_status(&args.status, &status)?;
                previous_status = Some(status);
            }
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    })();

    channels.clear();
    let error = result.as_ref().err().map(|error| format!("{error:#}"));
    let _ = write_status(
        &args.status,
        &MediaWorkerStatus {
            schema: 1,
            error,
            ..MediaWorkerStatus::default()
        },
    );
    result
}

fn start_pipeline(
    kind: MediaKind,
    port: u16,
    target: Option<&str>,
    resources: &ResourceLocator,
) -> Result<MediaProcess> {
    let gst = resources.helper_or_path("gst-launch-1.0")?;
    let device = resolve_device(kind, target, resources)?;
    let mut command = Command::new(&gst);
    command.arg("-q");
    match kind {
        MediaKind::GuestAudio => {
            command.args([
                "tcpclientsrc",
                "host=127.0.0.1",
                &format!("port={port}"),
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
                .context("the host advertises no usable microphone")?;
            command.args([
                "pulsesrc",
                "client-name=Buzzard OS Microphone",
                &format!("server={}", recording_service()?),
                &format!("device={}", device.node_name),
                "do-timestamp=true",
                "buffer-time=40000",
                "latency-time=10000",
                &format!(
                    "stream-properties=props,application.id={MICROPHONE_APPLICATION_ID},application.name=BuzzardOS,media.role=communication"
                ),
                "!",
                "audioconvert",
                "!",
                "audioresample",
                "!",
                "audio/x-raw,format=S16LE,rate=48000,channels=2",
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
                &format!("port={port}"),
                "sync=false",
                "async=false",
            ]);
        }
        MediaKind::HostCamera => {
            let device = device
                .as_ref()
                .context("the host advertises no usable camera")?;
            append_camera_source(&mut command, device)?;
            if matches!(&device.backend, HostMediaBackend::V4l2 { device } if v4l2_supports_mjpeg(device).unwrap_or(false))
            {
                command.args(["!", "image/jpeg", "!", "jpegdec"]);
            }
            command.args([
                "!",
                "videoconvert",
                "!",
                "videoscale",
                "!",
                "video/x-raw,format=BGRA,width=640,height=480",
                "!",
                "gdppay",
                "!",
                "tcpclientsink",
                "host=127.0.0.1",
                &format!("port={port}"),
                "sync=false",
                "async=false",
            ]);
        }
    }
    let worker_pid = unsafe { libc::getpid() };
    // SAFETY: only async-signal-safe libc calls run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != worker_pid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "media worker exited during pipeline startup",
                ));
            }
            Ok(())
        });
    }
    let mut process = MediaProcess(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {} bridge with {}", kind.name(), gst.display()))?,
    );
    thread::sleep(Duration::from_millis(150));
    if let Some(status) = process.0.try_wait().context("checking media startup")? {
        bail!("{} bridge exited during startup with {status}", kind.name());
    }
    if kind == MediaKind::HostMicrophone {
        let target = device
            .as_ref()
            .map(|device| device.node_name.as_str())
            .context("microphone selection disappeared")?;
        wait_for_tracked_microphone(resources, process.0.id(), target)?;
    }
    Ok(process)
}

fn resolve_device(
    kind: MediaKind,
    requested: Option<&str>,
    resources: &ResourceLocator,
) -> Result<Option<HostMediaDevice>> {
    if kind == MediaKind::GuestAudio && requested.is_none() {
        return Ok(None);
    }
    let devices: Vec<_> = discover_host_media(resources)?
        .into_iter()
        .filter(|device| device.kind == kind.host_kind())
        .collect();
    let device = if let Some(requested) = requested {
        devices
            .iter()
            .find(|device| device.node_name == requested)
            .cloned()
            .with_context(|| format!("selected {} '{requested}' is unavailable", kind.name()))?
    } else {
        devices
            .iter()
            .find(|device| device.is_default)
            .or_else(|| devices.first())
            .cloned()
            .with_context(|| format!("the host advertises no usable {}", kind.name()))?
    };
    Ok(Some(device))
}

fn append_camera_source(command: &mut Command, device: &HostMediaDevice) -> Result<()> {
    match &device.backend {
        HostMediaBackend::V4l2 { device } => {
            command.args([
                "v4l2src",
                &format!("device={}", device.display()),
                "do-timestamp=true",
            ]);
        }
        HostMediaBackend::PipeWire => {
            command.args([
                "pipewiresrc",
                "client-name=Buzzard OS Host Camera",
                "do-timestamp=true",
                &format!("target-object={}", device.serial),
            ]);
        }
        HostMediaBackend::Alsa { .. } => bail!("camera resolved to an audio backend"),
    }
    Ok(())
}

fn validate_port(port: u16) -> Result<()> {
    if port == 0 {
        bail!("Podman returned media port zero");
    }
    Ok(())
}

fn require_host_pipewire() -> Result<()> {
    require_socket(&runtime_dir().join("pipewire-0"), "PipeWire")
}

fn recording_service() -> Result<String> {
    let socket = runtime_dir().join("pulse/native");
    require_socket(&socket, "PipeWire-Pulse")?;
    Ok(format!("unix:{}", socket.display()))
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })))
}

fn require_socket(path: &Path, service: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path).with_context(|| {
        format!(
            "host {service} service is unavailable at {}",
            path.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_socket() {
        bail!(
            "host {service} endpoint {} is not a real socket",
            path.display()
        );
    }
    Ok(())
}

fn wait_for_tracked_microphone(
    resources: &ResourceLocator,
    process_id: u32,
    target: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(graph) = read_pipewire_graph(resources)
            && pipewire_has_microphone_stream(&graph, process_id, target)
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    bail!("host microphone did not become a desktop-visible recording stream")
}

fn read_pipewire_graph(resources: &ResourceLocator) -> Result<Value> {
    let pw_dump = resources.helper_or_path("pw-dump")?;
    let output = Command::new(&pw_dump)
        .output()
        .with_context(|| format!("running {}", pw_dump.display()))?;
    if !output.status.success() {
        bail!("pw-dump exited with {}", output.status);
    }
    if output.stdout.len() > MAX_PIPEWIRE_DUMP_BYTES {
        bail!("host PipeWire graph exceeded 32 MiB");
    }
    serde_json::from_slice(&output.stdout).context("parsing host PipeWire graph")
}

fn pipewire_has_microphone_stream(graph: &Value, process_id: u32, target: &str) -> bool {
    let Some(objects) = graph.as_array() else {
        return false;
    };
    let Some(target_id) = objects.iter().find_map(|object| {
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
        let matches = stream.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Node")
            && stream.pointer("/info/state").and_then(Value::as_str) == Some("running")
            && stream
                .pointer("/info/props/pulse.corked")
                .and_then(value_bool)
                != Some(true)
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
        let Some(stream_id) = matches
            .then(|| stream.get("id").and_then(value_u64))
            .flatten()
        else {
            return false;
        };
        objects.iter().any(|link| {
            link.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Link")
                && link.pointer("/info/state").and_then(Value::as_str) == Some("active")
                && link.pointer("/info/output-node-id").and_then(value_u64) == Some(target_id)
                && link.pointer("/info/input-node-id").and_then(value_u64) == Some(stream_id)
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

fn v4l2_supports_mjpeg(device: &Path) -> Result<bool> {
    let file = fs::File::open(device)
        .with_context(|| format!("opening selected camera {}", device.display()))?;
    for index in 0..256_u32 {
        let mut format = V4l2FormatDescription {
            index,
            buffer_type: V4L2_BUF_TYPE_VIDEO_CAPTURE,
            ..V4l2FormatDescription::default()
        };
        let result = unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_ENUM_FMT, &mut format) };
        if result == 0 {
            if format.pixel_format == V4L2_PIX_FMT_MJPEG {
                return Ok(true);
            }
            continue;
        }
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
            return Ok(false);
        }
        bail!("could not enumerate selected camera formats");
    }
    bail!("selected camera advertised more than 256 formats")
}

extern "C" fn request_stop(_: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Release);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            request_stop as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            request_stop as *const () as libc::sighandler_t,
        );
    }
}

fn terminate(child: &mut Child) {
    if child.try_wait().is_ok_and(|status| status.is_some()) {
        return;
    }
    let _ = unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait().is_ok_and(|status| status.is_some()) {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn write_status(path: &Path, status: &MediaWorkerStatus) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(status).context("serializing media status")?,
    )
    .with_context(|| format!("writing {}", temporary.display()))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protecting {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("saving {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_process() -> Result<MediaProcess> {
        Ok(MediaProcess(Command::new("sleep").arg("30").spawn()?))
    }

    #[test]
    fn failed_audio_does_not_disable_microphone_or_camera() {
        let mut channels = [
            MediaChannel::new(MediaKind::GuestAudio, 1234, None),
            MediaChannel::new(MediaKind::HostMicrophone, 1235, None),
            MediaChannel::new(MediaKind::HostCamera, 1236, None),
        ];
        channels[0].reconcile_with(|| bail!("audio unavailable"));
        channels[1].reconcile_with(idle_process);
        channels[2].reconcile_with(idle_process);
        let status = channel_status(&channels);
        assert!(!status.guest_audio_output);
        assert!(status.host_microphone);
        assert!(status.host_camera);
        assert!(status.error.unwrap().contains("audio unavailable"));
        let microphone = channels[1].process.as_ref().unwrap().0.id();
        channels[0].retry_at = Instant::now();
        channels[0].reconcile_with(idle_process);
        assert!(channel_status(&channels).error.is_none());
        assert_eq!(channels[1].process.as_ref().unwrap().0.id(), microphone);
    }

    #[test]
    fn reconnect_replaces_only_the_exited_channel_and_reaps_children() {
        let mut channel = MediaChannel::new(MediaKind::HostCamera, 1236, None);
        channel.reconcile_with(idle_process);
        let old_pid = channel.process.as_ref().unwrap().0.id();
        channel.process.as_mut().unwrap().0.kill().unwrap();
        channel.process.as_mut().unwrap().0.wait().unwrap();
        channel.reconcile_with(|| panic!("retry must be delayed"));
        assert!(channel.process.is_none());
        assert!(channel.error.is_some());
        channel.retry_at = Instant::now();
        channel.reconcile_with(idle_process);
        let new_pid = channel.process.as_ref().unwrap().0.id();
        assert_ne!(old_pid, new_pid);
        drop(channel);
        assert_eq!(
            unsafe { libc::waitpid(new_pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn failed_start_is_rate_limited_and_healthy_channels_are_not_restarted() {
        let mut channel = MediaChannel::new(MediaKind::GuestAudio, 1234, None);
        channel.reconcile_with(|| bail!("not ready"));
        channel.reconcile_with(|| panic!("no retry before backoff"));
        channel.retry_at = Instant::now();
        channel.reconcile_with(idle_process);
        channel.reconcile_with(|| panic!("a healthy bridge stays running"));
    }
}
