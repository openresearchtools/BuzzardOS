//! Native-Wayland backend.
//!
//! Used when the experimental backend is enabled under a Wayland compositor.
//! Enumerates toplevels via `zwlr_foreign_toplevel_manager_v1` or the generic
//! staging `ext_foreign_toplevel_list_v1`, captures per-output screenshots via
//! `zwlr_screencopy_manager_v1` + `wl_shm` (native — `grim` remains a
//! fallback), and synthesises pointer / scroll / drag input via
//! `zwlr_virtual_pointer_v1`. Per-window image capture is deferred until
//! `ext-foreign-toplevel-image-capture-source-v1` lands in
//! `wayland-protocols-wlr`; until then `screenshot_window_dispatch` returns a
//! typed error on pure Wayland.

pub(crate) mod compositor_ipc;
pub mod ext_screencopy;
pub mod ext_toplevel;
pub mod overlay;
pub mod persistent_vptr;
pub(crate) mod portal;
pub mod portal_screenshot;
pub mod shell_helper;
pub mod sway_ipc;
mod virtual_keyboard;
pub(crate) use virtual_keyboard::Admission as KeyboardAdmission;
// RemoteDesktop/libei input is portable and ships in release binaries.
// PipeWire ScreenCast capture remains separately gated for modern/Nix builds.
#[cfg(feature = "portal-input")]
pub mod libei;
#[cfg(feature = "portal-capture")]
pub mod portal_screencast;

/// Whether the GNOME/KDE RemoteDesktop + libei input backend is compiled in.
pub const PORTAL_INPUT_ENABLED: bool = cfg!(feature = "portal-input");

/// Gracefully neutralize the daemon-owned compositor keyboard before the CUA
/// runtime exits. No Wayland connection is created when keyboard input was
/// never used.
pub fn shutdown_keyboard() -> anyhow::Result<()> {
    virtual_keyboard::shutdown()
}

/// Install the session-revocation hook before any platform Tool can be
/// admitted. This is intentionally separate from starting the Wayland owner.
pub fn initialize_keyboard_cancellation() {
    virtual_keyboard::initialize();
}

pub fn keyboard_admission(
    trusted_leases: Vec<cua_driver_core::session::SessionLease>,
) -> anyhow::Result<KeyboardAdmission> {
    virtual_keyboard::admit(trusted_leases)
}

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

use wayland_client::{
    event_created_child,
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::{self, WlOutput},
        wl_pointer::{Axis, AxisSource, ButtonState},
        wl_registry,
        wl_seat::WlSeat,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self as ftl_handle, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{
        self as ftl_manager, ZwlrForeignToplevelManagerV1, EVT_TOPLEVEL_OPCODE,
    },
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self as scrcopy_frame, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

/// Linux evdev BTN_LEFT — the button code the virtual-pointer protocol expects.
const BTN_LEFT: u32 = 0x110;

use crate::x11::WindowInfo;

/// Name of the opt-in env var that unlocks the experimental native-Wayland
/// backend.
pub const ENABLE_WAYLAND_ENV: &str = "CUA_DRIVER_RS_ENABLE_WAYLAND";
const BUZZARDOS_OUTPUT_STATE: &str = "/run/buzzardos-display-state/output-state.json";

fn buzzardos_output_state_required() -> bool {
    std::env::var_os("BUZZARDOS_MACHINE_ID").is_some()
        || std::path::Path::new("/run/buzzardos-host").is_dir()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct BuzzardOSOutputState {
    schema: u32,
    physical_width: u32,
    physical_height: u32,
    host_surface_scale_120: u32,
    guest_ui_scale_120: u32,
    logical_width: u32,
    logical_height: u32,
    geometry_generation: u64,
}

impl BuzzardOSOutputState {
    fn validate(self) -> anyhow::Result<Self> {
        if self.schema != 7 {
            anyhow::bail!(
                "unsupported Buzzard OS output-state schema {}; expected 7",
                self.schema
            );
        }
        if self.physical_width == 0
            || self.physical_height == 0
            || self.logical_width == 0
            || self.logical_height == 0
            || self.physical_width > 65_535
            || self.physical_height > 65_535
            || self.logical_width > 65_535
            || self.logical_height > 65_535
            || !(120..=960).contains(&self.host_surface_scale_120)
            || !(120..=960).contains(&self.guest_ui_scale_120)
            || self.geometry_generation == 0
        {
            anyhow::bail!("Buzzard OS output-state contains out-of-range geometry");
        }
        let expected_width = u64::from(self.physical_width)
            .saturating_mul(120)
            .checked_div(u64::from(self.guest_ui_scale_120))
            .unwrap_or(1)
            .max(1) as u32;
        let expected_height = u64::from(self.physical_height)
            .saturating_mul(120)
            .checked_div(u64::from(self.guest_ui_scale_120))
            .unwrap_or(1)
            .max(1) as u32;
        if (self.logical_width, self.logical_height) != (expected_width, expected_height) {
            anyhow::bail!(
                "Buzzard OS output-state logical mode {}x{} is incoherent with native {}x{} physical pixels at guest UI scale {}/120; expected {}x{}",
                self.logical_width,
                self.logical_height,
                self.physical_width,
                self.physical_height,
                self.guest_ui_scale_120,
                expected_width,
                expected_height
            );
        }
        Ok(self)
    }

    fn metadata(self) -> CanonicalOutputMetadata {
        CanonicalOutputMetadata {
            physical_width: self.physical_width,
            physical_height: self.physical_height,
            host_surface_scale_120: self.host_surface_scale_120,
            guest_ui_scale_120: self.guest_ui_scale_120,
            logical_width: self.logical_width,
            logical_height: self.logical_height,
            geometry_generation: self.geometry_generation,
        }
    }

    fn guest_logical_dimensions(self) -> (u32, u32) {
        (self.logical_width, self.logical_height)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct CanonicalOutputMetadata {
    pub physical_width: u32,
    pub physical_height: u32,
    pub host_surface_scale_120: u32,
    pub guest_ui_scale_120: u32,
    pub logical_width: u32,
    pub logical_height: u32,
    /// Zero only outside Buzzard OS, where no generation contract exists.
    pub geometry_generation: u64,
}

fn read_buzzardos_output_state() -> anyhow::Result<Option<BuzzardOSOutputState>> {
    const LIMIT: u64 = 1024 * 1024;
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(BUZZARDOS_OUTPUT_STATE)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if buzzardos_output_state_required() {
                anyhow::bail!(
                    "Buzzard OS output-state is missing; canonical screenshot and input geometry are unavailable"
                );
            }
            return Ok(None);
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "reading Buzzard OS output-state failed: {error}"
            ));
        }
    };
    let metadata = file
        .metadata()
        .map_err(|error| anyhow::anyhow!("inspecting Buzzard OS output-state failed: {error}"))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        anyhow::bail!(
            "Buzzard OS output-state must be a session-owned regular file with no group/world write permission"
        );
    }
    if metadata.len() > LIMIT {
        anyhow::bail!("Buzzard OS output-state exceeds the 1 MiB limit");
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| anyhow::anyhow!("reading Buzzard OS output-state failed: {error}"))?;
    if bytes.len() as u64 > LIMIT {
        anyhow::bail!("Buzzard OS output-state exceeds the 1 MiB limit");
    }
    let state: BuzzardOSOutputState = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("parsing Buzzard OS output-state failed: {error}"))?;
    state.validate().map(Some)
}

fn buzzardos_output_state() -> Option<BuzzardOSOutputState> {
    read_buzzardos_output_state().ok().flatten()
}

fn require_same_output_generation(
    before: Option<BuzzardOSOutputState>,
    after: Option<BuzzardOSOutputState>,
) -> anyhow::Result<()> {
    match (before, after) {
        (None, None) => Ok(()),
        (Some(before), Some(after)) if before == after => Ok(()),
        (Some(before), Some(after)) => anyhow::bail!(
            "stale_output_geometry: output changed from generation {} to {} during the operation; discard its screenshot or window geometry",
            before.geometry_generation,
            after.geometry_generation
        ),
        (Some(before), None) => anyhow::bail!(
            "stale_output_geometry: Buzzard OS output generation {} disappeared during the operation",
            before.geometry_generation
        ),
        (None, Some(after)) => anyhow::bail!(
            "stale_output_geometry: Buzzard OS output generation {} appeared during the operation",
            after.geometry_generation
        ),
    }
}

fn with_stable_output_generation<T>(
    operation: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let before = read_buzzardos_output_state()?;
    let result = operation();
    let after = read_buzzardos_output_state()?;
    require_same_output_generation(before, after)?;
    result
}

/// Active Buzzard OS guest UI scale in 1/120 units.
pub(crate) fn buzzardos_guest_ui_scale_120() -> Option<u32> {
    buzzardos_output_state().map(|state| state.guest_ui_scale_120)
}

fn scale_signed_ratio(value: i64, numerator: u32, denominator: u32) -> i64 {
    let scaled = value.saturating_mul(i64::from(numerator.max(1)));
    let divisor = i64::from(denominator.max(1));
    if scaled >= 0 {
        scaled.saturating_add(divisor / 2) / divisor
    } else {
        scaled.saturating_sub(divisor / 2) / divisor
    }
}

fn scale_floor_ratio(value: i64, numerator: u32, denominator: u32) -> i64 {
    let scaled = value.saturating_mul(i64::from(numerator.max(1)));
    let divisor = i64::from(denominator.max(1));
    scaled.div_euclid(divisor)
}

fn scale_ceil_ratio(value: i64, numerator: u32, denominator: u32) -> i64 {
    -scale_floor_ratio(value.saturating_neg(), numerator, denominator)
}

fn scaled_rect(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    target_width: u32,
    target_height: u32,
    source_width: u32,
    source_height: u32,
    cover_source_pixels: bool,
) -> (i32, i32, u32, u32) {
    let x2 = i64::from(x).saturating_add(i64::from(width));
    let y2 = i64::from(y).saturating_add(i64::from(height));
    let (left, top, right, bottom) = if cover_source_pixels {
        (
            scale_floor_ratio(i64::from(x), target_width, source_width),
            scale_floor_ratio(i64::from(y), target_height, source_height),
            scale_ceil_ratio(x2, target_width, source_width),
            scale_ceil_ratio(y2, target_height, source_height),
        )
    } else {
        (
            scale_signed_ratio(i64::from(x), target_width, source_width),
            scale_signed_ratio(i64::from(y), target_height, source_height),
            scale_signed_ratio(x2, target_width, source_width),
            scale_signed_ratio(y2, target_height, source_height),
        )
    };
    (
        left.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        top.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        right
            .saturating_sub(left)
            .max(i64::from(width > 0))
            .clamp(0, i64::from(u32::MAX)) as u32,
        bottom
            .saturating_sub(top)
            .max(i64::from(height > 0))
            .clamp(0, i64::from(u32::MAX)) as u32,
    )
}

/// Convert a rectangle reported in physical guest-buffer pixels into the
/// canonical guest physical-pixel coordinate space.
pub(crate) fn physical_rect_to_canonical(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> (i32, i32, u32, u32) {
    (x, y, width, height)
}

/// Return Buzzard OS's canonical guest-output physical extent when this
/// process is running in a machine, otherwise retain the compositor extent.
///
/// This is exactly the dmabuf mode captured by screencopy. No image resize,
/// downscale, or host-window chrome enters the CUA screenshot.
pub fn canonical_output_dimensions(fallback_width: u32, fallback_height: u32) -> (u32, u32) {
    buzzardos_output_state()
        .map(|state| (state.physical_width, state.physical_height))
        .unwrap_or((fallback_width.max(1), fallback_height.max(1)))
}

pub fn canonical_output_metadata(
    fallback_width: u32,
    fallback_height: u32,
) -> CanonicalOutputMetadata {
    match buzzardos_output_state() {
        Some(state) => state.metadata(),
        None => CanonicalOutputMetadata {
            physical_width: fallback_width.max(1),
            physical_height: fallback_height.max(1),
            host_surface_scale_120: 120,
            guest_ui_scale_120: 120,
            logical_width: fallback_width.max(1),
            logical_height: fallback_height.max(1),
            geometry_generation: 0,
        },
    }
}

/// Return canonical metadata while preserving upstream CUA behavior outside
/// Buzzard OS. Inside a machine, `read_buzzardos_output_state` rejects a
/// missing, malformed, untrusted, or incoherent state instead of silently
/// fabricating generation-zero geometry.
pub fn canonical_output_metadata_checked(
    fallback_width: u32,
    fallback_height: u32,
) -> anyhow::Result<CanonicalOutputMetadata> {
    Ok(match read_buzzardos_output_state()? {
        Some(state) => state.metadata(),
        None => canonical_output_metadata(fallback_width, fallback_height),
    })
}

fn normalize_capture_for_state(
    png: Vec<u8>,
    state: BuzzardOSOutputState,
) -> anyhow::Result<Vec<u8>> {
    let image = image::load_from_memory(&png)?;
    let actual = (image.width(), image.height());
    let physical = (state.physical_width, state.physical_height);
    let guest_logical = state.guest_logical_dimensions();
    if actual == physical {
        return Ok(png);
    }
    anyhow::bail!(
        "stale_output_geometry: captured {}x{} but current Buzzard OS output is \
         a native {}x{} physical dmabuf (guest logical mode {}x{}); refusing to \
         resample the screenshot",
        actual.0,
        actual.1,
        physical.0,
        physical.1,
        guest_logical.0,
        guest_logical.1
    );
}

fn normalize_capture_for_generation(
    png: Vec<u8>,
    before: Option<BuzzardOSOutputState>,
    after: Option<BuzzardOSOutputState>,
) -> anyhow::Result<Vec<u8>> {
    require_same_output_generation(before, after)?;
    match before {
        Some(state) => normalize_capture_for_state(png, state),
        None => Ok(png),
    }
}

/// Convert compositor/AT-SPI logical geometry into Buzzard OS's canonical
/// guest-output physical-pixel coordinate space.
pub fn logical_rect_to_canonical(x: i32, y: i32, width: u32, height: u32) -> (i32, i32, u32, u32) {
    logical_rect_to_canonical_for_state(buzzardos_output_state(), x, y, width, height)
}

fn logical_rect_to_canonical_for_state(
    state: Option<BuzzardOSOutputState>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> (i32, i32, u32, u32) {
    let Some(state) = state else {
        return (x, y, width, height);
    };
    let logical = state.guest_logical_dimensions();
    scaled_rect(
        x,
        y,
        width,
        height,
        state.physical_width,
        state.physical_height,
        logical.0,
        logical.1,
        true,
    )
}

pub(crate) fn physical_rect_to_logical(
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> (i32, i32, u32, u32) {
    physical_rect_to_logical_for_state(buzzardos_output_state(), x, y, width, height)
}

fn physical_rect_to_logical_for_state(
    state: Option<BuzzardOSOutputState>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> (i32, i32, u32, u32) {
    let Some(state) = state else {
        return (x, y, width, height);
    };
    let logical = state.guest_logical_dimensions();
    scaled_rect(
        x,
        y,
        width,
        height,
        logical.0,
        logical.1,
        state.physical_width,
        state.physical_height,
        false,
    )
}

/// Whether the user has opted into the experimental native-Wayland backend.
///
/// The Wayland backend covers toplevel enumeration, per-output capture
/// (native screencopy + a `grim` fallback), and virtual-pointer /
/// virtual-keyboard input via the wlroots protocols. Per-window image
/// capture still depends on the staging `ext-image-copy-capture-v1`
/// protocol, so the backend stays OFF by default and a pure-Wayland
/// session is reported as unsupported unless the user explicitly sets
/// `CUA_DRIVER_RS_ENABLE_WAYLAND=1`. Any value other than empty / `0` /
/// `false` enables it.
pub fn wayland_enabled() -> bool {
    match std::env::var(ENABLE_WAYLAND_ENV) {
        Ok(v) => {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

/// True when this is an opted-in Wayland desktop session.
///
/// GNOME and KDE export `DISPLAY` for XWayland even when the target and the
/// desktop are native Wayland. Treating that compatibility variable as proof
/// of an X11 session routed native windows, capture, video, geometry, and focus
/// through invalid XIDs. Backend selection below is capability based; the mere
/// presence of `DISPLAY` must not disable Wayland.
pub fn is_wayland() -> bool {
    wayland_enabled() && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// True when input tools should attempt the Wayland input path (wlroots
/// virtual-pointer, falling back to libei/portal via [`with_libei_fallback`]).
///
/// Input-specific alias retained to make dispatch intent explicit. The
/// wlroots-vs-portal decision is made from live compositor capabilities inside
/// the `wayland::*` input functions, not from XWayland's `DISPLAY` variable.
pub fn wayland_input_enabled() -> bool {
    wayland_enabled() && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Reason string when X11 input injection cannot possibly work, so callers
/// **fail loudly** instead of falling through to an X11 path that no-ops yet
/// reports success. Triggers only on a *pure* Wayland session — `WAYLAND_DISPLAY`
/// set, no X11 `DISPLAY` — with the native-Wayland backend NOT opted in (so
/// [`is_wayland`] is false and the X11 path would be chosen). XWayland sessions
/// (where `DISPLAY` is set) and X11 sessions return `None` and proceed normally.
/// See #1921.
pub fn wayland_input_unavailable_reason() -> Option<String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && std::env::var_os("DISPLAY").is_none()
        && !wayland_enabled()
    {
        Some(format!(
            "input cannot be delivered: pure Wayland session (no X11 DISPLAY) and \
             the native-Wayland input backend is not enabled. Set {}=1 to enable \
             the Wayland backend (wlroots compositors: sway, labwc, hyprland), or \
             run the target under XWayland so an X11 DISPLAY is available.",
            ENABLE_WAYLAND_ENV
        ))
    } else {
        None
    }
}

fn wl_sockets(dir: &str) -> std::collections::HashSet<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| {
            n.strip_prefix("wayland-")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect()
}

/// "Bring your own compositor": if `CUA_WAYLAND_NEST` is set, spawn a private
/// **headless wlroots compositor** (labwc by default) and point this process —
/// and therefore every app it launches (`launch_app`), every capture (`grim`),
/// and all enumeration/injection — at it via `WAYLAND_DISPLAY`. This lets
/// cua-driver automate apps in its **own** Wayland session on ANY host,
/// including KDE (kwin) and GNOME (mutter) which expose no client protocols for
/// this, without ever touching the host compositor or its focus. Idempotent.
pub fn ensure_nested_session() {
    use std::sync::OnceLock;
    static DONE: OnceLock<()> = OnceLock::new();
    if std::env::var_os("CUA_WAYLAND_NEST").is_none() {
        return;
    }
    DONE.get_or_init(|| {
        let xdg = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/0".into());
        let comp = std::env::var("CUA_WAYLAND_NEST_COMPOSITOR").unwrap_or_else(|_| "labwc".into());
        let before = wl_sockets(&xdg);
        let spawned = std::process::Command::new(&comp)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("WLR_RENDERER_ALLOW_SOFTWARE", "1")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_HEADLESS_OUTPUTS", "1")
            .env_remove("WAYLAND_DISPLAY") // headless: do not nest into the host compositor
            .env_remove("DISPLAY")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => {
                std::mem::forget(child); // keep the compositor alive for our lifetime
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                loop {
                    if let Some(sock) = wl_sockets(&xdg).difference(&before).min().cloned() {
                        std::env::set_var("WAYLAND_DISPLAY", &sock);
                        std::env::remove_var("DISPLAY");
                        // Publish the nested socket so external tools (e.g. a
                        // `grim` recorder) can target the same session we drive.
                        let _ = std::fs::write(format!("{xdg}/.cua-nested-display"), &sock);
                        tracing::info!("cua nested compositor '{comp}' up: WAYLAND_DISPLAY={sock}");
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        tracing::error!(
                            "cua nested compositor '{comp}': no Wayland socket appeared"
                        );
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
            Err(e) => tracing::error!("cua nested compositor '{comp}' spawn failed: {e}"),
        }
    });
}

#[derive(Default)]
struct Toplevel {
    title: String,
    app_id: String,
    closed: bool,
    maximized: bool,
    minimized: bool,
    activated: bool,
    fullscreen: bool,
}

fn apply_toplevel_state_array(toplevel: &mut Toplevel, bytes: &[u8]) {
    let states: HashSet<u32> = bytes
        .chunks_exact(4)
        .map(|chunk| u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    // wlr-foreign-toplevel-management-v1 state values are fixed by protocol:
    // maximized=0, minimized=1, activated=2, fullscreen=3.
    toplevel.maximized = states.contains(&0);
    toplevel.minimized = states.contains(&1);
    toplevel.activated = states.contains(&2);
    toplevel.fullscreen = states.contains(&3);
}

#[derive(Clone, Debug)]
struct ToplevelIdentity {
    title: String,
    app_id: String,
}

fn identity_registry() -> &'static Mutex<HashMap<u64, ToplevelIdentity>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, ToplevelIdentity>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn observed_origin_registry() -> &'static Mutex<HashMap<u32, (i32, i32)>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u32, (i32, i32)>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn remember_observed_window_origins(windows: &[WindowInfo]) {
    if let Ok(mut registry) = observed_origin_registry().lock() {
        for window in windows {
            if let Some(pid) = window.pid {
                // Generic foreign-toplevel and AT-SPI fallbacks use (0,0) when
                // they do not know compositor geometry. Do not let that
                // placeholder erase a previously observed real origin or
                // prevent the caller from falling through to Sway/GNOME data.
                if (window.x, window.y) != (0, 0) {
                    registry.insert(pid, (window.x, window.y));
                }
            }
        }
    }
}

pub fn observed_window_origin(pid: u32) -> Option<(i32, i32)> {
    observed_origin_registry()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&pid).copied())
}

fn remember_identity(id: u64, toplevel: &Toplevel) {
    if let Ok(mut registry) = identity_registry().lock() {
        registry.insert(
            id,
            ToplevelIdentity {
                title: toplevel.title.clone(),
                app_id: toplevel.app_id.clone(),
            },
        );
    }
}

fn identity_for(id: u64) -> Option<ToplevelIdentity> {
    identity_registry()
        .lock()
        .ok()
        .and_then(|registry| registry.get(&id).cloned())
        .or_else(|| {
            sway_ipc::resolve_public_window(id, None)
                .ok()
                .map(|window| ToplevelIdentity {
                    title: window.title,
                    app_id: window.app_id,
                })
        })
        .or_else(|| {
            compositor_ipc::window_for_id(id).map(|window| ToplevelIdentity {
                title: window.title,
                app_id: window.app_id,
            })
        })
        .or_else(|| {
            crate::atspi::list_windows(None)
                .into_iter()
                .find(|window| window.xid == id || u64::from(window.xid as u32) == id)
                .map(|window| ToplevelIdentity {
                    title: window.title,
                    app_id: window.app_name,
                })
        })
}

fn matching_handle(state: &State, id: u64) -> Option<ZwlrForeignToplevelHandleV1> {
    if let Some(protocol_id) = state
        .stable_toplevel_ids
        .iter()
        .find_map(|(protocol_id, stable_id)| (*stable_id == id).then_some(*protocol_id))
    {
        return state
            .handles
            .get(&protocol_id)
            .filter(|_| {
                state
                    .toplevels
                    .get(&protocol_id)
                    .is_some_and(|toplevel| !toplevel.closed)
            })
            .cloned();
    }
    // IDs in this namespace were minted by the daemon-owned persistent
    // foreign-toplevel connection. A missing one is stale or unknown and must
    // never fall through to title/app-id guessing, especially when several
    // windows share the same title.
    if id >= STABLE_TOPLEVEL_ID_BASE {
        return None;
    }
    if let Some(identity) = identity_for(id) {
        let by_title = state.toplevels.iter().find_map(|(protocol_id, toplevel)| {
            (!identity.title.is_empty() && toplevel.title == identity.title)
                .then(|| state.handles.get(protocol_id).cloned())
                .flatten()
        });
        return by_title.or_else(|| {
            state.toplevels.iter().find_map(|(protocol_id, toplevel)| {
                (!identity.app_id.is_empty() && toplevel.app_id == identity.app_id)
                    .then(|| state.handles.get(protocol_id).cloned())
                    .flatten()
            })
        });
    }

    let protocol_id = u32::try_from(id).ok()?;
    state.handles.get(&protocol_id).cloned()
}

/// Per-capture in-flight state populated by the screencopy frame Dispatch.
#[derive(Default)]
struct CaptureState {
    /// wl_shm format code (Argb8888 / Xrgb8888 / …).
    format: Option<u32>,
    width: u32,
    height: u32,
    stride: u32,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

#[derive(Default)]
struct State {
    manager: Option<ZwlrForeignToplevelManagerV1>,
    toplevels: HashMap<u32, Toplevel>,
    // Live handles + a seat, kept so `click` can `activate` a target toplevel by
    // its window_id (foreign-toplevel protocol id) — the focus-based input model.
    handles: HashMap<u32, ZwlrForeignToplevelHandleV1>,
    stable_toplevel_ids: HashMap<u32, u64>,
    next_stable_toplevel_id: u64,
    seat: Option<WlSeat>,
    // Virtual-pointer manager + output dimensions, so `click` can land a real
    // button press at the output centre (over the just-activated window).
    vptr_manager: Option<ZwlrVirtualPointerManagerV1>,
    output: Option<WlOutput>,
    output_w: u32,
    output_h: u32,
    // Native screencopy capture state.
    scrcopy_manager: Option<ZwlrScreencopyManagerV1>,
    shm: Option<WlShm>,
    capture: CaptureState,
}

const STABLE_TOPLEVEL_ID_BASE: u64 = 0xFC00_0000;

fn stable_toplevel_id(state: &mut State, protocol_id: u32) -> u64 {
    if let Some(id) = state.stable_toplevel_ids.get(&protocol_id) {
        return *id;
    }
    let id = STABLE_TOPLEVEL_ID_BASE
        .checked_add(state.next_stable_toplevel_id)
        .expect("foreign-toplevel stable id space exhausted");
    state.next_stable_toplevel_id = state
        .next_stable_toplevel_id
        .checked_add(1)
        .expect("foreign-toplevel stable id counter exhausted");
    state.stable_toplevel_ids.insert(protocol_id, id);
    id
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == ZwlrForeignToplevelManagerV1::interface().name {
                let v = version.min(3);
                state.manager =
                    Some(registry.bind::<ZwlrForeignToplevelManagerV1, _, _>(name, v, qh, ()));
            } else if interface == WlSeat::interface().name {
                let v = version.min(7);
                state.seat = Some(registry.bind::<WlSeat, _, _>(name, v, qh, ()));
            } else if interface == ZwlrVirtualPointerManagerV1::interface().name {
                state.vptr_manager = Some(registry.bind::<ZwlrVirtualPointerManagerV1, _, _>(
                    name,
                    version.min(2),
                    qh,
                    (),
                ));
            } else if interface == WlOutput::interface().name {
                let out = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ());
                if state.output.is_none() {
                    state.output = Some(out);
                }
            } else if interface == ZwlrScreencopyManagerV1::interface().name {
                state.scrcopy_manager = Some(registry.bind::<ZwlrScreencopyManagerV1, _, _>(
                    name,
                    version.min(3),
                    qh,
                    (),
                ));
            } else if interface == WlShm::interface().name {
                state.shm = Some(registry.bind::<WlShm, _, _>(name, version.min(1), qh, ()));
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Seat name/capabilities events are irrelevant here — we only need the
        // seat object to pass to foreign-toplevel `activate`.
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Remember the output resolution so `click` can aim at its centre.
        if let wl_output::Event::Mode { width, height, .. } = event {
            state.output_w = width.max(0) as u32;
            state.output_h = height.max(0) as u32;
        }
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerManagerV1,
        _: <ZwlrVirtualPointerManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: <ZwlrVirtualPointerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // wl_shm advertises supported formats via `format` events; we don't
        // need to track them — screencopy tells us exactly which format to use
        // for the frame buffer.
    }
}

impl Dispatch<WlShmPool, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: wayland_client::protocol::wl_shm_pool::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlBuffer,
        _: wayland_client::protocol::wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrScreencopyFrameV1,
        event: scrcopy_frame::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            scrcopy_frame::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                if let WEnum::Value(fmt) = format {
                    state.capture.format = Some(fmt as u32);
                }
                state.capture.width = width;
                state.capture.height = height;
                state.capture.stride = stride;
            }
            scrcopy_frame::Event::Flags { flags } => {
                if let WEnum::Value(f) = flags {
                    state.capture.y_invert = f.contains(scrcopy_frame::Flags::YInvert);
                }
            }
            scrcopy_frame::Event::Ready { .. } => {
                state.capture.ready = true;
            }
            scrcopy_frame::Event::Failed => {
                state.capture.failed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        _event: ftl_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The `toplevel` event creates a handle object (see event_created_child!);
        // the handle's own events carry the title/app_id we collect below.
    }

    event_created_child!(State, ZwlrForeignToplevelManagerV1, [
        EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for State {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: ftl_handle::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = handle.id().protocol_id();
        let is_reused_protocol_id = !matches!(event, ftl_handle::Event::Closed)
            && state
                .toplevels
                .get(&id)
                .is_some_and(|toplevel| toplevel.closed);
        if is_reused_protocol_id {
            // Wayland may recycle a server-created object id after the former
            // toplevel's `closed` event. Treat that as a new lifetime and mint
            // a new public id rather than aliasing the closed window.
            state.handles.insert(id, handle.clone());
            state.stable_toplevel_ids.remove(&id);
            state.toplevels.insert(id, Toplevel::default());
        } else {
            state.handles.entry(id).or_insert_with(|| handle.clone());
        }
        stable_toplevel_id(state, id);
        let tl = state.toplevels.entry(id).or_default();
        match event {
            ftl_handle::Event::Title { title } => tl.title = title,
            ftl_handle::Event::AppId { app_id } => tl.app_id = app_id,
            ftl_handle::Event::State { state } => apply_toplevel_state_array(tl, &state),
            ftl_handle::Event::Closed => tl.closed = true,
            _ => {}
        }
    }
}

fn windows_from_foreign_toplevel_state(state: &State) -> Vec<WindowInfo> {
    let compositor_windows = compositor_ipc::list_windows().unwrap_or_default();
    let mut used_compositor_ids = HashSet::new();
    let mut out = Vec::new();
    for (protocol_id, tl) in &state.toplevels {
        if tl.closed {
            continue;
        }
        let title = if tl.app_id.is_empty() {
            tl.title.clone()
        } else {
            format!("{} [{}]", tl.title, tl.app_id)
        };
        // Non-Sway wlroots compositors do not expose a cross-protocol opaque
        // identifier. Enrich only a genuinely unique title/app-id match;
        // iteration order must never choose between indistinguishable windows.
        let mut compositor_matches = compositor_windows.iter().filter(|window| {
            !used_compositor_ids.contains(&window.id)
                && (tl.title.is_empty() || window.title == tl.title)
                && (tl.app_id.is_empty() || window.app_id == tl.app_id)
                && (!tl.title.is_empty() || !tl.app_id.is_empty())
        });
        let first = compositor_matches.next();
        let compositor = first.filter(|_| compositor_matches.next().is_none());
        let stable_id = state
            .stable_toplevel_ids
            .get(protocol_id)
            .copied()
            .expect("every observed foreign toplevel has a stable id");
        if let Some(window) = compositor {
            used_compositor_ids.insert(window.id);
        }
        remember_identity(stable_id, tl);
        out.push(WindowInfo {
            xid: stable_id,
            pid: compositor.map(|window| window.pid),
            app_name: tl.app_id.clone(),
            title,
            // The compositor's foreign-toplevel minimized state is
            // authoritative. Geometry helpers may retain the last mapped
            // rectangle while a window is minimized.
            is_on_screen: foreign_toplevel_is_on_screen(
                tl,
                compositor.map(|window| window.visible),
            ),
            z_index: None,
            x: compositor.map(|window| window.x).unwrap_or(0),
            y: compositor.map(|window| window.y).unwrap_or(0),
            width: compositor.map(|window| window.width).unwrap_or(0),
            height: compositor.map(|window| window.height).unwrap_or(0),
        });
    }
    out.sort_by_key(|window| window.xid);
    out
}

fn foreign_toplevel_is_on_screen(toplevel: &Toplevel, geometry_visible: Option<bool>) -> bool {
    !toplevel.minimized && geometry_visible.unwrap_or(true)
}

enum ForeignToplevelCommand {
    List(mpsc::Sender<Result<Vec<WindowInfo>, String>>),
    Activate {
        window_id: u64,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Control {
        window_id: u64,
        action: WindowControlAction,
        reply: mpsc::Sender<Result<(WindowControlState, WindowControlState), String>>,
    },
}

fn activate_window_on_worker(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    window_id: u64,
) -> anyhow::Result<()> {
    let handle = matching_handle(state, window_id).ok_or_else(|| {
        anyhow::anyhow!("no live native Wayland toplevel for window_id {window_id}")
    })?;
    let seat = state
        .seat
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor exposed no wl_seat for activation"))?;
    handle.activate(&seat);
    conn.flush()?;
    for _ in 0..20 {
        queue.roundtrip(state)?;
        if state
            .toplevels
            .get(&handle.id().protocol_id())
            .is_some_and(|toplevel| !toplevel.closed && toplevel.activated)
        {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    anyhow::bail!(
        "foreground_unavailable: compositor did not confirm activation of window {window_id}"
    )
}

fn foreign_toplevel_worker(commands: mpsc::Receiver<ForeignToplevelCommand>) -> anyhow::Result<()> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut state = State::default();
    queue.roundtrip(&mut state)?; // registry globals -> bind manager
    if state.manager.is_none() {
        anyhow::bail!("compositor does not expose zwlr_foreign_toplevel_manager_v1");
    }
    for _ in 0..4 {
        queue.roundtrip(&mut state)?;
    }

    while let Ok(command) = commands.recv() {
        for _ in 0..2 {
            queue.roundtrip(&mut state)?;
        }
        match command {
            ForeignToplevelCommand::List(reply) => {
                let _ = reply.send(Ok(windows_from_foreign_toplevel_state(&state)));
            }
            ForeignToplevelCommand::Activate { window_id, reply } => {
                let result = activate_window_on_worker(&conn, &mut queue, &mut state, window_id)
                    .map_err(|error| error.to_string());
                let _ = reply.send(result);
            }
            ForeignToplevelCommand::Control {
                window_id,
                action,
                reply,
            } => {
                let result =
                    control_window_on_worker(&conn, &mut queue, &mut state, window_id, action)
                        .map_err(|error| error.to_string());
                let _ = reply.send(result);
            }
        }
    }
    Ok(())
}

fn foreign_toplevel_sender() -> &'static mpsc::Sender<ForeignToplevelCommand> {
    static WORKER: OnceLock<mpsc::Sender<ForeignToplevelCommand>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("cua-foreign-toplevel".into())
            .spawn(move || {
                if let Err(error) = foreign_toplevel_worker(receiver) {
                    tracing::error!("foreign-toplevel worker stopped: {error:#}");
                }
            })
            .expect("spawning foreign-toplevel worker");
        sender
    })
}

/// Enumerate native Wayland toplevels through one daemon-owned connection.
/// Stable ids remain bound to the same handle until its compositor `closed`
/// event, including when several windows share one pid/title/app-id.
pub fn list_windows() -> anyhow::Result<Vec<WindowInfo>> {
    if std::env::var_os("SWAYSOCK").is_some() {
        return sway_ipc::list_public_windows().map(|windows| {
            windows
                .into_iter()
                .map(|(public_id, window)| {
                    let title = if window.app_id.is_empty() {
                        window.title.clone()
                    } else {
                        format!("{} [{}]", window.title, window.app_id)
                    };
                    let toplevel = Toplevel {
                        title: window.title.clone(),
                        app_id: window.app_id.clone(),
                        minimized: window.minimized,
                        maximized: window.maximized,
                        activated: window.focused,
                        fullscreen: window.fullscreen,
                        ..Toplevel::default()
                    };
                    remember_identity(public_id, &toplevel);
                    WindowInfo {
                        xid: public_id,
                        pid: Some(window.pid),
                        app_name: window.app_id,
                        title,
                        is_on_screen: window.visible && window.width > 0 && window.height > 0,
                        z_index: None,
                        x: window.x,
                        y: window.y,
                        width: window.width,
                        height: window.height,
                    }
                })
                .collect()
        });
    }

    let (reply, result) = mpsc::channel();
    foreign_toplevel_sender()
        .send(ForeignToplevelCommand::List(reply))
        .map_err(|_| anyhow::anyhow!("foreign-toplevel worker is unavailable"))?;
    result
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| anyhow::anyhow!("foreign-toplevel list reply failed: {error}"))?
        .map_err(anyhow::Error::msg)
}

fn activate_stable_foreign_toplevel(window_id: u64) -> anyhow::Result<()> {
    let (reply, result) = mpsc::channel();
    foreign_toplevel_sender()
        .send(ForeignToplevelCommand::Activate { window_id, reply })
        .map_err(|_| anyhow::anyhow!("foreign-toplevel worker is unavailable"))?;
    result
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| anyhow::anyhow!("foreign-toplevel activation reply failed: {error}"))?
        .map_err(anyhow::Error::msg)
}

// ── Capture (native screencopy + grim fallback) ──────────────────────────────

/// Capture the Wayland output as PNG bytes via `zwlr_screencopy_manager_v1`.
///
/// Binds the screencopy manager plus `wl_shm`, asks the compositor to copy the
/// next frame of the first advertised output into a shm buffer, channel-swaps
/// from the compositor's pixel format to RGBA, and encodes a PNG via the
/// existing `image` crate. Falls back to shelling out to `grim` when the
/// screencopy manager or `wl_shm` is unavailable so users on lighter wlroots
/// builds stay supported.
pub fn screenshot_bytes() -> anyhow::Result<Vec<u8>> {
    match capture_via_screencopy() {
        Ok(bytes) => Ok(bytes),
        Err(e) => {
            tracing::warn!("native screencopy failed, falling back to grim: {e}");
            request_buzzardos_repaint();
            capture_via_grim()
        }
    }
}

/// Wake Buzzard OS's otherwise idle nested output so an in-guest
/// screencopy request receives a newly committed frame. This is a private
/// guest-runtime handshake: it neither captures nor exposes the host desktop.
fn request_buzzardos_repaint() {
    if std::env::var_os("BUZZARDOS_MACHINE_ID").is_none()
        && !std::path::Path::new("/run/buzzardos-host").is_dir()
    {
        return;
    }
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let target = std::path::PathBuf::from(runtime).join("buzzardos-shell-repaint");
    let temporary = target.with_file_name(format!(
        "buzzardos-shell-repaint.{}.tmp",
        std::process::id()
    ));
    let generation = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string();
    let result = std::fs::write(&temporary, generation.as_bytes())
        .and_then(|()| std::fs::rename(&temporary, &target));
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        tracing::debug!(
            "could not request Buzzard OS idle-output repaint at {}: {error}",
            target.display()
        );
    }
}

/// Shell out to `grim -t png -` — the wlroots reference screenshot tool. Kept
/// as the last-resort fallback for compositors that hide screencopy.
fn capture_via_grim() -> anyhow::Result<Vec<u8>> {
    let mut child = std::process::Command::new("grim")
        .args(["-t", "png", "-"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("grim stdout pipe was not created"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("grim stderr pipe was not created"))?;
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    // The child now has an opportunity to register its screencopy request;
    // the shell keeps submitting frames for a bounded settling interval, so
    // this handshake is race-free even if grim has not flushed yet.
    request_buzzardos_repaint();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            anyhow::bail!("grim timed out waiting for a compositor frame");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("grim stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("grim stderr reader panicked"))??;
    if !status.success() {
        anyhow::bail!("grim failed: {}", String::from_utf8_lossy(&stderr));
    }
    if stdout.is_empty() {
        anyhow::bail!("grim produced no output");
    }
    Ok(stdout)
}

/// Native screencopy path: bind manager + shm, allocate an anon mmap buffer,
/// request a copy, wait for Ready, swap channels, encode PNG. Returns an error
/// if any global is missing or the compositor flags the capture as failed.
fn capture_via_screencopy() -> anyhow::Result<Vec<u8>> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?; // outputs report their Mode

    let manager = state
        .scrcopy_manager
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor does not expose zwlr_screencopy_manager_v1"))?;
    let shm = state
        .shm
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor does not expose wl_shm"))?;
    let output = state
        .output
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor exposed no wl_output to capture"))?;

    let frame = manager.capture_output(0, &output, &qh, ());
    // Make the capture request visible to the nested compositor before asking the desktop
    // shell to damage the idle nested output.
    conn.flush()?;
    request_buzzardos_repaint();
    // Drain Buffer / Flags events; spin until Ready or Failed (or timeout).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut buffer: Option<WlBuffer> = None;
    let mut pool: Option<WlShmPool> = None;
    let mut mmap_ptr: *mut libc::c_void = std::ptr::null_mut();
    let mut mmap_len: usize = 0;
    let mut fd: i32 = -1;

    loop {
        queue.roundtrip(&mut state)?;
        if state.capture.failed {
            anyhow::bail!("compositor signalled screencopy failure");
        }
        if state.capture.ready {
            break;
        }
        // Once we know the buffer params, allocate + send copy exactly once.
        if buffer.is_none()
            && state.capture.format.is_some()
            && state.capture.stride > 0
            && state.capture.height > 0
        {
            let size = (state.capture.stride as usize)
                .checked_mul(state.capture.height as usize)
                .ok_or_else(|| anyhow::anyhow!("screencopy buffer size overflow"))?;
            let (anon_fd, p) = anon_shm(size)?;
            fd = anon_fd;
            mmap_ptr = p;
            mmap_len = size;
            use std::os::fd::AsFd as _;
            let pool_fd = unsafe { borrowed_fd(fd) };
            let p = shm.create_pool(pool_fd.as_fd(), size as i32, &qh, ());
            let fmt_raw = state.capture.format.unwrap();
            let fmt: wl_shm::Format = match wl_shm::Format::try_from(fmt_raw) {
                Ok(f) => f,
                Err(_) => {
                    cleanup_mmap(mmap_ptr, mmap_len, fd);
                    anyhow::bail!("compositor advertised unsupported wl_shm format {fmt_raw:#x}");
                }
            };
            let b = p.create_buffer(
                0,
                state.capture.width as i32,
                state.capture.height as i32,
                state.capture.stride as i32,
                fmt,
                &qh,
                (),
            );
            frame.copy(&b);
            buffer = Some(b);
            pool = Some(p);
        }
        if std::time::Instant::now() >= deadline {
            cleanup_mmap(mmap_ptr, mmap_len, fd);
            anyhow::bail!("screencopy timed out waiting for frame");
        }
    }

    let result = (|| -> anyhow::Result<Vec<u8>> {
        let w = state.capture.width;
        let h = state.capture.height;
        let stride = state.capture.stride as usize;
        let format = state.capture.format.unwrap_or(0);
        if mmap_ptr.is_null() || mmap_len == 0 {
            anyhow::bail!("screencopy ready without a backing buffer");
        }
        let raw = unsafe { std::slice::from_raw_parts(mmap_ptr as *const u8, mmap_len) };
        let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for row in 0..(h as usize) {
            let src_row = if state.capture.y_invert {
                (h as usize) - 1 - row
            } else {
                row
            };
            let base = src_row * stride;
            for col in 0..(w as usize) {
                let px = &raw[base + col * 4..base + col * 4 + 4];
                let (r, g, b, a) = match wl_shm::Format::try_from(format).ok() {
                    // Argb8888 / Xrgb8888 over wl_shm are little-endian BGRA / BGRX.
                    Some(wl_shm::Format::Argb8888) => (px[2], px[1], px[0], px[3]),
                    Some(wl_shm::Format::Xrgb8888) => (px[2], px[1], px[0], 255),
                    Some(wl_shm::Format::Abgr8888) => (px[0], px[1], px[2], px[3]),
                    Some(wl_shm::Format::Xbgr8888) => (px[0], px[1], px[2], 255),
                    _ => (px[2], px[1], px[0], px[3]),
                };
                rgba.extend_from_slice(&[r, g, b, a]);
            }
        }
        cua_driver_core::image_utils::encode_rgba_to_png(&rgba, w, h)
    })();

    // Always tear down regardless of result.
    if let Some(b) = buffer {
        b.destroy();
    }
    if let Some(p) = pool {
        p.destroy();
    }
    frame.destroy();
    let _ = queue.roundtrip(&mut state);
    cleanup_mmap(mmap_ptr, mmap_len, fd);

    result
}

/// Allocate an anonymous shared-memory file of `size` bytes and mmap it RW.
/// Returns the raw fd and the mmap pointer; the caller is responsible for
/// passing both to [`cleanup_mmap`] when done.
pub(crate) fn anon_shm(size: usize) -> anyhow::Result<(i32, *mut libc::c_void)> {
    // memfd_create is Linux-only and is the cleanest path; fall back to
    // shm_open if memfd isn't available for any reason.
    let name = b"cua-scrcopy\0";
    let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(anyhow::anyhow!(
            "memfd_create failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let rc = unsafe { libc::ftruncate(fd, size as libc::off_t) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(anyhow::anyhow!("ftruncate failed: {err}"));
    }
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(anyhow::anyhow!("mmap failed: {err}"));
    }
    Ok((fd, p))
}

/// Unmap and close the screencopy backing buffer; safe to call with the
/// sentinel values left from a never-allocated buffer.
pub(crate) fn cleanup_mmap(ptr: *mut libc::c_void, len: usize, fd: i32) {
    if !ptr.is_null() && len > 0 {
        unsafe { libc::munmap(ptr, len) };
    }
    if fd >= 0 {
        unsafe { libc::close(fd) };
    }
}

/// Borrow a raw fd as an `OwnedFd` for wl_shm.create_pool. The pool keeps
/// its own reference; we close our copy via [`cleanup_mmap`].
///
/// SAFETY: caller must guarantee `fd` is a valid open file descriptor.
/// `libc::dup` may fail (returning -1, errno set), in which case we panic
/// instead of constructing an `OwnedFd` from -1 (which would have UB on
/// drop). Callers that need fallible behaviour should use the dup syscall
/// directly and check the result before wrapping.
pub(crate) unsafe fn borrowed_fd(fd: i32) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd;
    let dup = libc::dup(fd);
    if dup < 0 {
        let err = std::io::Error::last_os_error();
        panic!("dup({fd}) failed: {err}");
    }
    std::os::fd::OwnedFd::from_raw_fd(dup)
}

/// Capture dispatcher: native Wayland (screencopy with grim fallback) when
/// applicable, else X11. Mirrors `screenshot_window_dispatch` for the
/// output-level path used by `get_window_state`'s vision payload.
pub fn screenshot_dispatch(xid: u64) -> anyhow::Result<Vec<u8>> {
    if is_wayland() {
        let before = read_buzzardos_output_state()?;
        let bytes = screenshot_display_dispatch_unchecked()?;
        let bytes = normalize_capture_for_generation(bytes, before, before)?;
        let result = if let Some((x, y, width, height)) = window_geometry_logical(xid) {
            let (x, y, width, height) =
                logical_rect_to_canonical_for_state(before, x, y, width, height);
            crop_png_to_rect(
                &bytes,
                x,
                y,
                width,
                height,
                &format!("Wayland window {xid}"),
            )?
        } else {
            bytes
        };
        let after = read_buzzardos_output_state()?;
        require_same_output_generation(before, after)?;
        Ok(result)
    } else {
        crate::capture::screenshot_window_bytes(xid)
    }
}

fn crop_png_to_rect(
    output_png: &[u8],
    rect_x: i32,
    rect_y: i32,
    rect_width: u32,
    rect_height: u32,
    label: &str,
) -> anyhow::Result<Vec<u8>> {
    let image = image::load_from_memory(output_png)?;
    let image_width = image.width();
    let image_height = image.height();
    if rect_width == 0 || rect_height == 0 {
        anyhow::bail!("{label} has empty capture geometry");
    }
    if rect_width > 8192
        || rect_height > 8192
        || u64::from(rect_width) * u64::from(rect_height) > 64 * 1024 * 1024
    {
        anyhow::bail!(
            "{label} capture geometry {rect_width}x{rect_height} exceeds the safety limit"
        );
    }

    let left = i64::from(rect_x);
    let top = i64::from(rect_y);
    let right = left + i64::from(rect_width);
    let bottom = top + i64::from(rect_height);
    let source_left = left.max(0).min(i64::from(image_width));
    let source_top = top.max(0).min(i64::from(image_height));
    let source_right = right.max(0).min(i64::from(image_width));
    let source_bottom = bottom.max(0).min(i64::from(image_height));
    if source_left >= source_right || source_top >= source_bottom {
        anyhow::bail!("{label} does not intersect captured output {image_width}x{image_height}");
    }

    let source_width = u32::try_from(source_right - source_left)?;
    let source_height = u32::try_from(source_bottom - source_top)?;
    let source = image
        .crop_imm(
            u32::try_from(source_left)?,
            u32::try_from(source_top)?,
            source_width,
            source_height,
        )
        .to_rgba8();
    let mut cropped = image::RgbaImage::new(rect_width, rect_height);
    image::imageops::overlay(&mut cropped, &source, source_left - left, source_top - top);
    let mut cursor = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(cropped).write_to(&mut cursor, image::ImageFormat::Png)?;
    Ok(cursor.into_inner())
}

/// Display-level capture dispatcher. Cascade:
/// 1. Opt-in GNOME compositor helper. If available, capture failure is
///    terminal rather than cascading into GNOME's portal implementation.
/// 2. Native Wayland on wlroots: zwlr_screencopy_manager_v1 (fast, zero
///    consent).
/// 3. ext-image-copy-capture-v1 on supported compositors.
/// 4. xdg-desktop-portal Screenshot via ashpd. Triggers a consent prompt on
///    first use per session.
/// 5. X11: existing root-window path.
pub fn screenshot_display_dispatch() -> anyhow::Result<Vec<u8>> {
    screenshot_display_dispatch_with_metadata().map(|(bytes, _)| bytes)
}

/// Capture one complete output and return metadata from the exact same
/// geometry generation. The post-capture state check rejects even a
/// same-sized UI-scale transition.
pub fn screenshot_display_dispatch_with_metadata(
) -> anyhow::Result<(Vec<u8>, CanonicalOutputMetadata)> {
    let before = read_buzzardos_output_state()?;
    let bytes = screenshot_display_dispatch_unchecked()?;
    let after = read_buzzardos_output_state()?;
    let bytes = normalize_capture_for_generation(bytes, before, after)?;
    let metadata = match before {
        Some(state) => state.metadata(),
        None => {
            let image = image::load_from_memory(&bytes)?;
            canonical_output_metadata(image.width(), image.height())
        }
    };
    Ok((bytes, metadata))
}

fn screenshot_display_dispatch_unchecked() -> anyhow::Result<Vec<u8>> {
    if is_wayland() {
        // Tier 1: the opt-in GNOME compositor helper. It avoids probing
        // wlroots-only protocols and captures the Shell stage without consent.
        // If the helper is present but capture fails, do not fall through to
        // GNOME's portal implementation: on GNOME 50 a malformed 0x0 cursor
        // sprite can crash Shell in GNOME's unsafe stage-content capture path.
        if let Some(result) = checked_shell_helper_capture(
            shell_helper::available(),
            shell_helper::screenshot_display,
        ) {
            return result;
        }
        // Tier 2: native wlroots screencopy (fast, zero consent).
        match screenshot_bytes() {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                tracing::debug!(
                    "wlroots screencopy unavailable ({e}); trying ext-image-copy-capture-v1"
                );
            }
        }
        // Tier 3: ext-image-copy-capture-v1 (sway 1.10+, labwc 0.8+, niri,
        // hyprland, KDE 6.2+, GNOME 47+).
        match ext_screencopy::screenshot_via_ext_copy() {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                tracing::debug!(
                    "ext-image-copy-capture-v1 unavailable ({e}); trying xdg-desktop-portal"
                );
            }
        }
        // Tier 4: xdg-desktop-portal (GNOME, KDE, COSMIC fallback).
        match portal_screenshot::screenshot_via_portal() {
            Ok(bytes) => return Ok(bytes),
            Err(e) => {
                tracing::debug!(
                    "xdg-desktop-portal Screenshot unavailable ({e}); falling through to X11"
                );
            }
        }
    }
    // Final fallback: X11 root window. Call the X11-only path explicitly
    // so we don't re-enter screenshot_display_bytes (which routes back here
    // on Wayland — would loop forever).
    crate::capture::screenshot_display_bytes_x11()
}

fn checked_shell_helper_capture(
    available: bool,
    capture: impl FnOnce() -> Option<Vec<u8>>,
) -> Option<anyhow::Result<Vec<u8>>> {
    available
        .then(|| capture().ok_or_else(|| anyhow::anyhow!("GNOME compositor helper capture failed")))
}

/// Per-window capture dispatcher. On X11 forwards to the existing window
/// capture path; on pure Wayland returns a typed error pointing at the
/// staging `ext-image-copy-capture-v1` protocol — wlr-screencopy is
/// output-only, and `foreign-toplevel` exposes no per-window geometry to
/// crop with.
pub fn screenshot_window_dispatch(xid: u64) -> anyhow::Result<Vec<u8>> {
    if is_wayland() {
        let before = read_buzzardos_output_state()?;
        if let Some((x, y, width, height)) = window_geometry_logical(xid) {
            let bytes = screenshot_display_dispatch_unchecked()?;
            let bytes = normalize_capture_for_generation(bytes, before, before)?;
            let (x, y, width, height) =
                logical_rect_to_canonical_for_state(before, x, y, width, height);
            let result = crop_png_to_rect(
                &bytes,
                x,
                y,
                width,
                height,
                &format!("Wayland window {xid}"),
            )?;
            let after = read_buzzardos_output_state()?;
            require_same_output_generation(before, after)?;
            return Ok(result);
        }
        anyhow::bail!(
            "per-window screenshot is not yet supported on native Wayland — \
             zwlr_screencopy_manager_v1 is output-only and ext-image-copy-capture-v1 \
             is not yet shipped in wayland-protocols-wlr. Run under XWayland to crop \
             to a single window, or capture the full output instead."
        );
    }
    crate::capture::screenshot_window_bytes(xid)
}

// ── Input session helper ─────────────────────────────────────────────────────

/// Sentinel substring carried by the `open_vptr_session` error when the
/// compositor exposes no `zwlr_virtual_pointer_manager_v1` (KWin/Plasma,
/// Mutter/GNOME). The input dispatch matches on this to decide whether the
/// libei/portal fallback ([`libei`]) can recover the call. Kept as a string
/// marker (rather than a typed error) so the existing `anyhow::Result`
/// signatures of every input fn are unchanged. See #1982.
pub const NO_VPTR_MARKER: &str = "no-zwlr-virtual-pointer";

/// True when `err` is the "compositor has no wlroots virtual-pointer" failure
/// from [`open_vptr_session`] — i.e. the point where a non-wlroots compositor
/// needs the libei fallback rather than a hard error.
fn is_no_vptr(err: &anyhow::Error) -> bool {
    err.to_string().contains(NO_VPTR_MARKER)
}

/// Run the wlroots virtual-pointer closure `f`; if it fails specifically
/// because the compositor exposes no `zwlr_virtual_pointer_manager_v1` and this
/// binary carries the `portal-input` feature, run the libei `fallback` instead.
/// Any other wlroots error (and the no-vptr error in a build without the
/// feature) propagates unchanged. This is the single seam through which #1982's
/// KDE/GNOME input recovery flows.
fn with_libei_fallback<T>(
    f: impl FnOnce() -> anyhow::Result<T>,
    #[allow(unused_variables)] fallback: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    match f() {
        Ok(v) => Ok(v),
        Err(e) if is_no_vptr(&e) => {
            #[cfg(feature = "portal-input")]
            {
                tracing::info!(
                    "wlroots virtual-pointer unavailable ({e}); falling back to libei/portal"
                );
                return fallback();
            }
            #[cfg(not(feature = "portal-input"))]
            {
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Live virtual-pointer session: connection + queue + the bound objects every
/// pointer op (click, scroll, drag) needs. Returned by [`open_vptr_session`].
pub struct VptrSession {
    pub conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
    pub seat: WlSeat,
    pub vptr: ZwlrVirtualPointerV1,
    pub output_w: u32,
    pub output_h: u32,
}

/// Bind manager + seat + virtual-pointer + first output, optionally activate a
/// foreign-toplevel by `window_id` so the synthesised events land on it, and
/// return the live session that scroll / drag / click reuse. Wayland forbids a
/// client from knowing another window's on-screen geometry, so we drive every
/// pointer event in *output* coordinates and rely on the activated toplevel
/// covering the centre.
pub fn open_vptr_session(activate_window_id: Option<u64>) -> anyhow::Result<VptrSession> {
    // Public wlroots ids are scoped to the daemon-owned persistent
    // foreign-toplevel connection. Activate them there; a fresh virtual
    // pointer connection cannot resolve those ids safely.
    let local_activate_window_id = match activate_window_id {
        Some(id) if sway_ipc::is_public_window_id(id) => {
            sway_ipc::focus_public_window(id, None)?;
            None
        }
        Some(id) if id >= STABLE_TOPLEVEL_ID_BASE => {
            activate_stable_foreign_toplevel(id)?;
            None
        }
        other => other,
    };

    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());

    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    for _ in 0..4 {
        queue.roundtrip(&mut state)?;
    }

    // Evaluate the virtual-pointer / NO_VPTR_MARKER path FIRST: on compositors
    // that expose neither zwlr_virtual_pointer nor zwlr_foreign_toplevel
    // (KWin/Plasma, Mutter/GNOME) we must surface the marker so
    // `with_libei_fallback` re-routes through libei/portal. Requiring
    // foreign-toplevel up front would mask the marker and leave the libei
    // fallback dead. See #1982.
    let mgr = state.vptr_manager.clone().ok_or_else(|| {
        if PORTAL_INPUT_ENABLED {
            // The caller (via `with_libei_fallback`) recognises NO_VPTR_MARKER
            // and re-routes the op through the libei/portal backend, which DOES
            // reach KWin/Plasma and Mutter/GNOME. Keep the marker in the text.
            anyhow::anyhow!(
                "compositor does not expose zwlr_virtual_pointer_manager_v1 \
                 ({NO_VPTR_MARKER})"
            )
        } else {
            // KWin/Plasma and Mutter/GNOME don't implement zwlr_virtual_pointer,
            // and this build has no libei/portal fallback — so input has no
            // backend at all rather than silently no-op'ing. The marker still
            // lets the dispatch layer classify the failure uniformly. See #1982.
            anyhow::anyhow!(
                "no input backend for this compositor ({NO_VPTR_MARKER}): it \
                 exposes no zwlr_virtual_pointer_manager_v1 and this build was \
                 compiled without libei/portal support (#1982). Use the \
                 portal-enabled Linux build for input on KDE Plasma / GNOME, or \
                 a wlroots compositor (sway, labwc, hyprland)."
            )
        }
    })?;

    // foreign-toplevel is only needed to activate a specific window before
    // synthesising input; require it only when a caller actually asks for that.
    if local_activate_window_id.is_some() && state.manager.is_none() {
        anyhow::bail!("compositor does not expose zwlr_foreign_toplevel_manager_v1");
    }

    let seat = state.seat.clone().ok_or_else(|| {
        anyhow::anyhow!("compositor exposed no wl_seat for virtual-pointer input")
    })?;

    if let Some(id) = local_activate_window_id {
        let handle = matching_handle(&state, id)
            .ok_or_else(|| anyhow::anyhow!("no native Wayland toplevel for window_id {id}"))?;
        handle.activate(&seat);
        queue.roundtrip(&mut state)?;
    }

    // Bind the virtual pointer to the concrete guest output when protocol
    // version 2 is available.  An unbound absolute pointer is mapped against
    // the compositor's whole output layout; nested fractional outputs can
    // consequently accept the request while leaving the active output seat at
    // its previous coordinates.  Buzzard OS exposes one canonical output,
    // so output-bound motion is both unambiguous and directly observable.
    let vptr = match state.output.as_ref() {
        Some(output) if mgr.version() >= 2 => {
            mgr.create_virtual_pointer_with_output(Some(&seat), Some(output), &qh, ())
        }
        _ => mgr.create_virtual_pointer(Some(&seat), &qh, ()),
    };
    let (output_w, output_h) = canonical_output_dimensions(state.output_w, state.output_h);
    Ok(VptrSession {
        conn,
        queue,
        state,
        seat,
        vptr,
        output_w,
        output_h,
    })
}

/// Focus and raise a specific native Wayland toplevel before focus-bound
/// keyboard or portal/libei input. wlroots exposes an activation request on its
/// foreign-toplevel protocol; GNOME uses the bundled compositor helper. Other
/// compositors must refuse until they provide an equally target-addressable
/// adapter, because global injection without this gate can affect the wrong app.
pub fn activate_window_for_input(window_id: u64) -> anyhow::Result<()> {
    let pid = crate::atspi::list_windows(None)
        .into_iter()
        .find(|window| window.xid == window_id)
        .and_then(|window| window.pid);
    activate_window_for_input_target(window_id, pid)
}

/// Activate a Wayland target with an explicit process identity when available.
/// The bundled compositor does not depend on connection-local Wayland object
/// ids: its control protocol resolves the one mapped toplevel owned by `pid`.
pub fn activate_window_for_input_target(
    window_id: u64,
    target_pid: Option<u32>,
) -> anyhow::Result<()> {
    if is_inject_mode() {
        let pid = target_pid.ok_or_else(|| {
            anyhow::anyhow!(
                "foreground_unavailable: cua-compositor activation requires a verified target pid"
            )
        })?;
        inject_send(&[format!("f {pid}")])?;
        remember_inject_focused_target(pid, window_id);
        std::thread::sleep(std::time::Duration::from_millis(60));
        return Ok(());
    }

    if sway_ipc::is_public_window_id(window_id) {
        sway_ipc::focus_public_window(window_id, target_pid)?;
        return Ok(());
    }

    if window_id >= STABLE_TOPLEVEL_ID_BASE {
        activate_stable_foreign_toplevel(window_id)?;
        std::thread::sleep(std::time::Duration::from_millis(60));
        return Ok(());
    }

    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    for _ in 0..4 {
        queue.roundtrip(&mut state)?;
    }

    if let (Some(_), Some(seat), Some(handle)) = (
        state.manager.as_ref(),
        state.seat.clone(),
        matching_handle(&state, window_id),
    ) {
        handle.activate(&seat);
        queue.roundtrip(&mut state)?;
        std::thread::sleep(std::time::Duration::from_millis(60));
        return Ok(());
    }

    if shell_helper::activate_window(window_id) {
        std::thread::sleep(std::time::Duration::from_millis(60));
        return Ok(());
    }

    anyhow::bail!(
        "foreground_unavailable: this Wayland compositor does not expose a verified, \
         target-addressable activation adapter for window {window_id}; refusing global \
         input because it could affect the wrong application"
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowControlAction {
    Close,
    Minimize,
    Maximize,
    Restore,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowControlState {
    pub present: bool,
    pub minimized: bool,
    pub maximized: bool,
    pub fullscreen: bool,
    pub activated: bool,
}

impl WindowControlState {
    fn from_toplevel(toplevel: Option<&Toplevel>) -> Self {
        match toplevel.filter(|toplevel| !toplevel.closed) {
            Some(toplevel) => Self {
                present: true,
                minimized: toplevel.minimized,
                maximized: toplevel.maximized,
                fullscreen: toplevel.fullscreen,
                activated: toplevel.activated,
            },
            None => Self::default(),
        }
    }

    fn from_sway(state: sway_ipc::WindowControlState) -> Self {
        Self {
            present: state.present,
            minimized: state.minimized,
            maximized: state.maximized,
            fullscreen: state.fullscreen,
            activated: state.focused,
        }
    }
}

fn control_state_for_handle(
    state: &State,
    handle: &ZwlrForeignToplevelHandleV1,
) -> WindowControlState {
    WindowControlState::from_toplevel(state.toplevels.get(&handle.id().protocol_id()))
}

fn window_control_satisfied(action: WindowControlAction, state: WindowControlState) -> bool {
    match action {
        WindowControlAction::Close => !state.present,
        WindowControlAction::Minimize => state.present && state.minimized,
        WindowControlAction::Maximize => state.present && state.maximized,
        WindowControlAction::Restore => {
            state.present && !state.minimized && !state.maximized && !state.fullscreen
        }
    }
}

fn control_window_on_worker(
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    state: &mut State,
    window_id: u64,
    action: WindowControlAction,
) -> anyhow::Result<(WindowControlState, WindowControlState)> {
    let handle = matching_handle(state, window_id)
        .ok_or_else(|| anyhow::anyhow!("no native Wayland toplevel for window_id {window_id}"))?;
    let before = control_state_for_handle(state, &handle);
    if !before.present {
        anyhow::bail!("Wayland toplevel {window_id} is already closed");
    }

    match action {
        WindowControlAction::Close => handle.close(),
        WindowControlAction::Minimize => handle.set_minimized(),
        WindowControlAction::Maximize => handle.set_maximized(),
        WindowControlAction::Restore => {
            handle.unset_minimized();
            handle.unset_maximized();
            handle.unset_fullscreen();
        }
    }
    conn.flush()?;

    let mut after = before;
    for _ in 0..20 {
        queue.roundtrip(state)?;
        after = control_state_for_handle(state, &handle);
        if window_control_satisfied(action, after) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Ok((before, after))
}

/// Request one exact compositor-owned toplevel state change through the same
/// persistent wlr-foreign-toplevel connection that assigned `window_id`, then
/// read the resulting protocol state back.
pub fn control_window(
    window_id: u64,
    expected_pid: u32,
    action: WindowControlAction,
) -> anyhow::Result<(WindowControlState, WindowControlState)> {
    if sway_ipc::is_public_window_id(window_id) {
        let window = sway_ipc::resolve_public_window(window_id, Some(expected_pid))?;
        let sway_action = match action {
            WindowControlAction::Close => sway_ipc::WindowControlAction::Close,
            WindowControlAction::Minimize => sway_ipc::WindowControlAction::Minimize,
            WindowControlAction::Maximize => sway_ipc::WindowControlAction::Maximize,
            WindowControlAction::Restore => sway_ipc::WindowControlAction::Restore,
        };
        let (before, after) = sway_ipc::control_window(window.id, sway_action)?;
        return Ok((
            WindowControlState::from_sway(before),
            WindowControlState::from_sway(after),
        ));
    }

    let (reply, result) = mpsc::channel();
    foreign_toplevel_sender()
        .send(ForeignToplevelCommand::Control {
            window_id,
            action,
            reply,
        })
        .map_err(|_| anyhow::anyhow!("foreign-toplevel worker is unavailable"))?;
    result
        .recv_timeout(std::time::Duration::from_secs(5))
        .map_err(|error| anyhow::anyhow!("foreign-toplevel control reply failed: {error}"))?
        .map_err(anyhow::Error::msg)
}

/// Query the first `wl_output`'s pixel dimensions via a short Wayland
/// roundtrip, independent of the virtual-pointer protocol. Used by the libei
/// fallback (which never opens a `VptrSession`) to reproduce the vptr path's
/// default-to-centre and clamp behaviour so both backends treat coordinates
/// identically. Falls back to `(1, 1)` when no output reports a mode.
#[cfg(feature = "portal-input")]
fn output_dimensions() -> anyhow::Result<(u32, u32)> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    for _ in 0..4 {
        queue.roundtrip(&mut state)?;
    }
    Ok(canonical_output_dimensions(state.output_w, state.output_h))
}

/// Reproduce the wlroots vptr path's coordinate handling for the libei
/// fallback: `(0, 0)` defaults to the output centre, and any value is clamped
/// to `[0, dim-1]`. Keeps `click(.., 0, 0, ..)` landing on centre rather than
/// the top-left corner across both backends.
#[cfg(feature = "portal-input")]
fn normalize_click_xy(x: i32, y: i32, w: u32, h: u32) -> (i32, i32) {
    let (px, py) = if x == 0 && y == 0 {
        ((w / 2) as i32, (h / 2) as i32)
    } else {
        (x, y)
    };
    (
        px.clamp(0, (w as i32).saturating_sub(1)),
        py.clamp(0, (h as i32).saturating_sub(1)),
    )
}

/// Map a cua/X11 pointer button (1=left / 2=middle / 3=right) to its evdev
/// code, which is what `zwlr_virtual_pointer_v1::button` expects.
pub fn evdev_pointer_button(button: u8) -> u32 {
    match button {
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        _ => 0x110, // BTN_LEFT
    }
}

fn event_time_ms() -> u32 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis()
        .clamp(1, u32::MAX as u128) as u32
}

/// Click a native Wayland toplevel identified by its `window_id` (the
/// foreign-toplevel protocol id from `list_windows`) at output-relative
/// `(x, y)`, with `button` (1/2/3 = left/middle/right) emitted `count` times.
/// Coordinates default to the output centre when both x and y are zero so
/// the legacy focus-based behaviour is preserved when callers can't supply
/// real coords. A short delay between iterations gives the compositor time
/// to discriminate single vs. double clicks.
pub fn click(window_id: u64, x: i32, y: i32, count: u32, button: u8) -> anyhow::Result<()> {
    with_stable_output_generation(|| {
        with_libei_fallback(
            || click_vptr(Some(window_id), x, y, count, button),
            || {
                libei_wait_pointer_ready()?;
                activate_window_for_input(window_id)?;
                libei_click(x, y, count, button)
            },
        )
    })
}

/// Click a desktop-absolute point without selecting or activating a toplevel.
/// This is the Wayland peer of an XTest root-window click and is used only by
/// the explicit desktop capture scope.
pub fn click_desktop(x: i32, y: i32, count: u32, button: u8) -> anyhow::Result<()> {
    with_stable_output_generation(|| {
        if is_inject_mode() {
            let btn = evdev_button(button as u32);
            return inject_send(&[format!("d {x} {y} {} {btn}", count.max(1))]);
        }
        with_libei_fallback(
            || click_vptr(None, x, y, count, button),
            || libei_click(x, y, count, button),
        )
    })
}

/// wlroots virtual-pointer implementation of [`click`]. Falls back to libei via
/// [`with_libei_fallback`] when the compositor exposes no virtual-pointer.
fn click_vptr(
    window_id: Option<u64>,
    x: i32,
    y: i32,
    count: u32,
    button: u8,
) -> anyhow::Result<()> {
    let mut sess = open_vptr_session(window_id)?;
    std::thread::sleep(std::time::Duration::from_millis(40));
    let (w, h) = (sess.output_w, sess.output_h);
    let (px, py) = if x == 0 && y == 0 {
        ((w / 2) as i32, (h / 2) as i32)
    } else {
        (x, y)
    };
    let px = px.clamp(0, w as i32 - 1) as u32;
    let py = py.clamp(0, h as i32 - 1) as u32;
    let btn = evdev_pointer_button(button);
    for i in 0..count.max(1) {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        sess.vptr.motion_absolute(event_time_ms(), px, py, w, h);
        sess.vptr.frame();
        sess.queue.roundtrip(&mut sess.state)?;
        std::thread::sleep(std::time::Duration::from_millis(15));
        sess.vptr.button(event_time_ms(), btn, ButtonState::Pressed);
        sess.vptr.frame();
        sess.queue.roundtrip(&mut sess.state)?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        sess.vptr
            .button(event_time_ms(), btn, ButtonState::Released);
        sess.vptr.frame();
        sess.queue.roundtrip(&mut sess.state)?;
    }
    // Keep the synthetic-cursor registry in sync with the warp we just
    // performed so a subsequent `get_cursor_position` reflects reality.
    record_synth_cursor(px as i32, py as i32);
    sess.vptr.destroy();
    sess.queue.roundtrip(&mut sess.state)?;
    Ok(())
}

/// Synthesize a vertical or horizontal scroll on the activated toplevel. Each
/// tick emits an `axis_source(wheel)` + `axis_discrete(1)` pair through the
/// virtual-pointer protocol, mirroring how a real wheel notch decomposes. The
/// magnitude follows wl_pointer convention: ±10 (in wl_fixed = ×256) per tick.
pub fn scroll(window_id: u64, direction: &str, amount: u32) -> anyhow::Result<()> {
    scroll_at(window_id, None, direction, amount)
}

/// Translate window-local screenshot coordinates into compositor output
/// coordinates when the active compositor exposes the target geometry.
pub fn window_local_to_output(window_id: u64, x: i32, y: i32) -> (i32, i32) {
    window_geometry(window_id)
        .map(|(window_x, window_y, _, _)| (window_x.saturating_add(x), window_y.saturating_add(y)))
        .unwrap_or((x, y))
}

/// Resolve geometry through stable title/app identity when a foreign-toplevel
/// object ID came from an earlier Wayland connection. Protocol object IDs are
/// connection-local, so direct equality is only a fast path.
pub fn window_geometry(window_id: u64) -> Option<(i32, i32, u32, u32)> {
    window_geometry_logical(window_id)
        .map(|(x, y, width, height)| logical_rect_to_canonical(x, y, width, height))
}

/// Internal compositor/AT-SPI geometry in compositor-logical coordinates,
/// before the one canonical physical-pixel transform.
pub(crate) fn window_geometry_logical(window_id: u64) -> Option<(i32, i32, u32, u32)> {
    if sway_ipc::is_public_window_id(window_id) {
        let window = sway_ipc::resolve_public_window(window_id, None).ok()?;
        return Some((window.x, window.y, window.width, window.height));
    }

    if let Some(window) = compositor_ipc::window_for_id(window_id) {
        return Some((window.x, window.y, window.width, window.height));
    }

    let identity = identity_for(window_id);
    if let Some(identity) = identity.as_ref() {
        if let Some(windows) = compositor_ipc::list_windows() {
            let title_matches = windows
                .iter()
                .filter(|window| !identity.title.is_empty() && window.title == identity.title)
                .collect::<Vec<_>>();
            if title_matches.len() == 1 {
                let window = title_matches[0];
                return Some((window.x, window.y, window.width, window.height));
            }
            let app_matches = windows
                .iter()
                .filter(|window| !identity.app_id.is_empty() && window.app_id == identity.app_id)
                .collect::<Vec<_>>();
            if app_matches.len() == 1 {
                let window = app_matches[0];
                return Some((window.x, window.y, window.width, window.height));
            }
        }
    }

    let windows = list_windows_dispatch_logical(None);
    if let Some(window) = windows
        .iter()
        .find(|window| window.xid == window_id && window.width > 0 && window.height > 0)
    {
        return Some((window.x, window.y, window.width, window.height));
    }
    let identity = identity?;
    let title_matches = windows
        .iter()
        .filter(|window| {
            window.width > 0
                && window.height > 0
                && !identity.title.is_empty()
                && undecorated_native_title(window) == identity.title
        })
        .collect::<Vec<_>>();
    if title_matches.len() == 1 {
        let window = title_matches[0];
        return Some((window.x, window.y, window.width, window.height));
    }
    let app_matches = windows
        .iter()
        .filter(|window| {
            window.width > 0
                && window.height > 0
                && !identity.app_id.is_empty()
                && window.app_name == identity.app_id
        })
        .collect::<Vec<_>>();
    (app_matches.len() == 1).then(|| {
        let window = app_matches[0];
        (window.x, window.y, window.width, window.height)
    })
}

/// Set a Sway toplevel's outer frame using canonical physical coordinates.
/// The request is transformed once into compositor-logical coordinates, then
/// verified through an independent compositor IPC readback.
pub fn set_window_frame(
    window_id: u64,
    pid: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> anyhow::Result<(Option<WindowInfo>, bool, bool)> {
    let output_before = read_buzzardos_output_state()?;
    if std::env::var_os("SWAYSOCK").is_none() {
        anyhow::bail!("Sway IPC is unavailable for setting another toplevel's frame");
    }
    let target = sway_ipc::resolve_public_window(window_id, Some(pid))?;
    let compositor_id = target.id;
    let before = logical_rect_to_canonical_for_state(
        output_before,
        target.x,
        target.y,
        target.width,
        target.height,
    );
    let logical = physical_rect_to_logical_for_state(output_before, x, y, width, height);
    sway_ipc::set_window_frame_checked(compositor_id, logical.0, logical.1, logical.2, logical.3)?;

    let requested = (x, y, width, height);
    // `set_window_frame_checked` already subscribes before mutation and
    // confirms the exact compositor-logical frame. Convert that one
    // authoritative readback into canonical physical coordinates once.
    let observed = sway_ipc::window_for_id(compositor_id).map(|window| {
        let frame = logical_rect_to_canonical_for_state(
            output_before,
            window.x,
            window.y,
            window.width,
            window.height,
        );
        WindowInfo {
            xid: window_id,
            pid: Some(window.pid),
            app_name: window.app_id,
            title: window.title,
            is_on_screen: window.visible,
            z_index: None,
            x: frame.0,
            y: frame.1,
            width: frame.2,
            height: frame.3,
        }
    });
    let confirmed = observed
        .as_ref()
        .is_some_and(|window| (window.x, window.y, window.width, window.height) == requested);
    let changed = observed
        .as_ref()
        .is_some_and(|window| (window.x, window.y, window.width, window.height) != before);
    let output_after = read_buzzardos_output_state()?;
    require_same_output_generation(output_before, output_after)?;
    Ok((observed, confirmed, changed))
}

/// Scroll after positioning the synthetic pointer over an output-relative
/// target. Wayland routes wheel events to the surface beneath the pointer, so
/// pixel-addressed scrolls must not inherit an unrelated cursor position.
pub fn scroll_at(
    window_id: u64,
    point: Option<(i32, i32)>,
    direction: &str,
    amount: u32,
) -> anyhow::Result<()> {
    let direction = direction.to_string();
    with_stable_output_generation(|| {
        with_libei_fallback(
            || scroll_vptr(Some(window_id), point, &direction, amount),
            || {
                libei_wait_scroll_ready()?;
                activate_window_for_input(window_id)?;
                if let Some((x, y)) = point {
                    libei_move_absolute(x, y)?;
                }
                libei_scroll(&direction, amount)
            },
        )
    })
}

/// Scroll at a desktop-absolute point without activating a named toplevel.
pub fn scroll_desktop(x: i32, y: i32, direction: &str, amount: u32) -> anyhow::Result<()> {
    let direction = direction.to_string();
    with_stable_output_generation(|| {
        if is_inject_mode() {
            return inject_scroll_desktop(x, y, &direction, amount);
        }
        with_libei_fallback(
            || scroll_vptr(None, Some((x, y)), &direction, amount),
            || {
                libei_wait_scroll_ready()?;
                libei_move_absolute(x, y)?;
                libei_scroll(&direction, amount)
            },
        )
    })
}

/// wlroots virtual-pointer implementation of [`scroll`].
fn scroll_vptr(
    window_id: Option<u64>,
    point: Option<(i32, i32)>,
    direction: &str,
    amount: u32,
) -> anyhow::Result<()> {
    let mut sess = open_vptr_session(window_id)?;
    if let Some((x, y)) = point {
        let px = x.clamp(0, (sess.output_w as i32).saturating_sub(1)) as u32;
        let py = y.clamp(0, (sess.output_h as i32).saturating_sub(1)) as u32;
        sess.vptr
            .motion_absolute(event_time_ms(), px, py, sess.output_w, sess.output_h);
        sess.vptr.frame();
        sess.queue.roundtrip(&mut sess.state)?;
        record_synth_cursor(px as i32, py as i32);
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    let (axis, sign): (Axis, i32) = match direction.to_ascii_lowercase().as_str() {
        "up" => (Axis::VerticalScroll, -1),
        "down" => (Axis::VerticalScroll, 1),
        "left" => (Axis::HorizontalScroll, -1),
        "right" => (Axis::HorizontalScroll, 1),
        other => anyhow::bail!("unknown scroll direction: {other}"),
    };
    // axis_discrete: `value` is logical units (the wayland-rs wrapper
    // converts to wl_fixed internally); `discrete` is the tick count.
    let value: f64 = (sign as f64) * 10.0;
    for i in 0..amount.max(1) {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        sess.vptr.axis_source(AxisSource::Wheel);
        sess.vptr.axis_discrete(event_time_ms(), axis, value, sign);
        sess.vptr.frame();
        sess.queue.roundtrip(&mut sess.state)?;
    }
    sess.vptr.destroy();
    sess.queue.roundtrip(&mut sess.state)?;
    Ok(())
}

/// Last cursor position the agent warped to via `move_cursor_absolute`.
/// Wayland exposes no protocol for clients to read the real global cursor
/// position; a Wayland-conformant `get_cursor_position` can therefore only
/// report what THIS process synthesized. Updated every `motion_absolute`
/// emitted from `move_cursor_absolute` / `click` / `drag`.
static SYNTH_CURSOR_POS: std::sync::OnceLock<std::sync::Mutex<Option<(i32, i32)>>> =
    std::sync::OnceLock::new();

fn record_synth_cursor(x: i32, y: i32) {
    let cell = SYNTH_CURSOR_POS.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut g) = cell.lock() {
        *g = Some((x, y));
    }
}

/// Returns the last `(x, y)` this process warped the cursor to via the
/// Wayland virtual-pointer protocol, or `None` if no warp has happened in
/// this process. The reading is "synthetic": Wayland forbids clients from
/// querying the real cursor position, so this value diverges from reality
/// the moment the user moves their physical mouse. Callers should surface
/// `source: "synthetic"` in the structured payload.
pub fn last_synth_cursor_pos() -> Option<(i32, i32)> {
    SYNTH_CURSOR_POS
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .ok()
        .and_then(|g| *g)
}

/// Warp the cursor to absolute output coordinates `(x, y)` using
/// `zwlr_virtual_pointer_v1::motion_absolute`. Clamps to the output bounds
/// reported by `open_vptr_session`. Emits a motion + frame and roundtrips so
/// the compositor commits the warp before returning. Records the position in
/// the synthetic-cursor registry so `last_synth_cursor_pos` can report it.
pub fn move_cursor_absolute(window_id: Option<u64>, x: i32, y: i32) -> anyhow::Result<()> {
    with_stable_output_generation(|| {
        with_libei_fallback(
            || move_cursor_absolute_vptr(window_id, x, y),
            || libei_move_absolute(x, y),
        )
    })
}

/// wlroots virtual-pointer implementation of [`move_cursor_absolute`].
fn move_cursor_absolute_vptr(window_id: Option<u64>, x: i32, y: i32) -> anyhow::Result<()> {
    let mut sess = open_vptr_session(window_id)?;
    let (w, h) = (sess.output_w, sess.output_h);
    let px = x.clamp(0, (w as i32).saturating_sub(1)) as u32;
    let py = y.clamp(0, (h as i32).saturating_sub(1)) as u32;
    sess.vptr.motion_absolute(event_time_ms(), px, py, w, h);
    sess.vptr.frame();
    sess.queue.roundtrip(&mut sess.state)?;
    record_synth_cursor(px as i32, py as i32);
    sess.vptr.destroy();
    sess.queue.roundtrip(&mut sess.state)?;
    Ok(())
}

/// Press-drag-release on a native Wayland toplevel. Emits one button press at
/// `(from_x, from_y)`, then `steps` interpolated motion events along the
/// straight segment to `(to_x, to_y)`, then a release. Coordinates are
/// output-relative; window-local coords need the nested cua-compositor
/// injection socket (`CUA_INJECT_SOCKET`).
pub fn drag(
    keyboard_admission: &KeyboardAdmission,
    window_id: u64,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    steps: u32,
    duration_ms: u64,
    button: u8,
    modifiers: &[String],
) -> anyhow::Result<()> {
    with_stable_output_generation(|| {
        virtual_keyboard::with_modifiers_held(keyboard_admission, modifiers, || {
            drag_vptr(
                keyboard_admission,
                Some(window_id),
                from_x,
                from_y,
                to_x,
                to_y,
                steps,
                duration_ms,
                button,
            )
        })
    })
}

/// Drag through desktop-absolute points without activating a named toplevel.
pub fn drag_desktop(
    keyboard_admission: &KeyboardAdmission,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    steps: u32,
    duration_ms: u64,
    button: u8,
    modifiers: &[String],
) -> anyhow::Result<()> {
    with_stable_output_generation(|| {
        virtual_keyboard::with_modifiers_held(keyboard_admission, modifiers, || {
            drag_vptr(
                keyboard_admission,
                None,
                from_x,
                from_y,
                to_x,
                to_y,
                steps,
                duration_ms,
                button,
            )
        })
    })
}

/// wlroots virtual-pointer implementation of [`drag`].
#[allow(clippy::too_many_arguments)]
fn drag_vptr(
    keyboard_admission: &KeyboardAdmission,
    window_id: Option<u64>,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    steps: u32,
    duration_ms: u64,
    button: u8,
) -> anyhow::Result<()> {
    // Use the same long-lived virtual pointer implementation as the explicit
    // mouse_button_down/mouse_drag/mouse_button_up tools.  Creating and
    // destroying a second virtual pointer inside this one-shot helper allowed
    // titlebar moves, but Sway did not retain compositor edge/corner grabs for
    // its motions.  One pointer object must own the whole press/move/release
    // sequence.
    static NEXT_DRAG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let cursor_id = format!(
        "__one_shot_drag_{}_{}",
        std::process::id(),
        NEXT_DRAG.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    match window_id {
        Some(id) => persistent_vptr::press(&cursor_id, id, from_x, from_y, button)?,
        None => persistent_vptr::press_desktop(&cursor_id, from_x, from_y, button)?,
    }

    let gesture = (|| {
        // Give Sway one event-loop turn to install its titlebar/border grab
        // before the first motion. The pointer remains held during the wait,
        // so cancellation must still pass through the unconditional release
        // below.
        virtual_keyboard::sleep_cancellable(
            keyboard_admission,
            std::time::Duration::from_millis(30),
        )?;
        let n = steps.max(1);
        let step_delay = std::time::Duration::from_millis(duration_ms / u64::from(n));
        for s in 1..=n {
            virtual_keyboard::ensure_current(keyboard_admission)?;
            let t = s as f64 / n as f64;
            let ix = (from_x as f64 + (to_x - from_x) as f64 * t).round() as i32;
            let iy = (from_y as f64 + (to_y - from_y) as f64 * t).round() as i32;
            persistent_vptr::move_to(&cursor_id, ix, iy)?;
            if !step_delay.is_zero() {
                virtual_keyboard::sleep_cancellable(keyboard_admission, step_delay)?;
            }
        }
        virtual_keyboard::ensure_current(keyboard_admission)?;
        persistent_vptr::move_to(&cursor_id, to_x, to_y)
    })();
    let release = persistent_vptr::release(&cursor_id, button);
    gesture?;
    release?;
    // Sync the synthetic-cursor registry with the drag endpoint so a
    // subsequent `get_cursor_position` reports where we left the pointer.
    record_synth_cursor(to_x, to_y);
    Ok(())
}

/// Type Unicode text into the focused Wayland surface through the daemon-owned
/// persistent `zwp_virtual_keyboard_v1`. Its generated one-keysym-per-key map
/// preserves pinned wtype's Unicode behavior without destroying the active
/// keyboard after each call. This mirrors the X11 backend's event typing.
/// foreign-toplevel exposes no pid and Wayland delivers keys to the *focused*
/// surface, so this is window_id-free; pair it with `click`/`activate` to put
/// the intended window in focus first.
const VIRTUAL_KEYBOARD_TEXT_DELAY_MS: u64 = 8;

pub fn type_text(admission: &KeyboardAdmission, window_id: u64, text: &str) -> anyhow::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    activate_window_for_input(window_id)?;
    virtual_keyboard::type_text(admission, text, VIRTUAL_KEYBOARD_TEXT_DELAY_MS)
}

/// Type into a surface whose exact Sway container is already held focused by
/// the caller. This avoids re-resolving a title that can change mid-sequence
/// (for example after opening a Chromium tab).
pub fn type_text_focused(admission: &KeyboardAdmission, text: &str) -> anyhow::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    virtual_keyboard::type_text(admission, text, VIRTUAL_KEYBOARD_TEXT_DELAY_MS)
}

/// Type text and press one key while an outer exact-container focus guard is
/// active. Keeping both operations in one virtual-keyboard lifetime avoids a
/// headless wlroots seat dropping the first event from a second `wtype`
/// process after the text has landed.
pub fn type_text_then_key_focused(
    admission: &KeyboardAdmission,
    text: &str,
    key: &str,
) -> anyhow::Result<()> {
    let keysym = key_to_keysym(key);
    virtual_keyboard::type_text_then_key(admission, text, VIRTUAL_KEYBOARD_TEXT_DELAY_MS, &keysym)
}

pub fn type_text_with_delay(
    admission: &KeyboardAdmission,
    window_id: u64,
    text: &str,
    delay_ms: u64,
) -> anyhow::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    activate_window_for_input(window_id)?;
    virtual_keyboard::type_text(admission, text, delay_ms)
}

/// Press a single named key into the focused Wayland surface through the
/// daemon-owned persistent virtual keyboard.
pub fn press_key(admission: &KeyboardAdmission, window_id: u64, key: &str) -> anyhow::Result<()> {
    activate_window_for_input(window_id)?;
    virtual_keyboard::press_key(admission, key)
}

/// Press one key while an outer exact-container focus guard is active.
pub fn press_key_focused(admission: &KeyboardAdmission, key: &str) -> anyhow::Result<()> {
    if is_inject_mode() {
        let (pid, window_id) = inject_focused_target()?;
        return inject_press_key(pid, window_id, key);
    }
    virtual_keyboard::press_key(admission, key)
}

/// Press a key combination through the persistent virtual keyboard. Each
/// modifier is pressed before the key and released in reverse order. This is
/// the Wayland equivalent of the X11 `send_key` modifier mask.
pub fn hotkey(
    admission: &KeyboardAdmission,
    window_id: u64,
    keys: &[String],
) -> anyhow::Result<()> {
    activate_window_for_input(window_id)?;
    let (mods, final_key) = partition_modifiers(keys)?;
    virtual_keyboard::hotkey(admission, &mods, &final_key)
}

/// Send a chord while an outer exact-container focus guard is active.
pub fn hotkey_focused(admission: &KeyboardAdmission, keys: &[String]) -> anyhow::Result<()> {
    if is_inject_mode() {
        let (pid, window_id) = inject_focused_target()?;
        return inject_hotkey(pid, window_id, keys);
    }
    let (mods, final_key) = partition_modifiers(keys)?;
    virtual_keyboard::hotkey(admission, &mods, &final_key)
}

/// Split a `keys` array into virtual-keyboard modifier names and a single
/// final key. Recognised modifier inputs: ctrl/control, alt, shift,
/// super/meta/cmd/command/win/windows. The final key must be the one
/// non-modifier in the list.
fn partition_modifiers(keys: &[String]) -> anyhow::Result<(Vec<String>, String)> {
    let mut mods: Vec<String> = Vec::new();
    let mut non_mods: Vec<String> = Vec::new();
    for k in keys {
        match k.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods.push("ctrl".into()),
            "alt" => mods.push("alt".into()),
            "shift" => mods.push("shift".into()),
            "super" | "meta" | "cmd" | "command" | "win" | "windows" => mods.push("logo".into()),
            _ => non_mods.push(k.clone()),
        }
    }
    let final_key = non_mods
        .last()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("hotkey requires at least one non-modifier key"))?;
    Ok((mods, final_key))
}

/// Map CUA key names to XKB keysym names. Unknown values pass through (single
/// characters and valid keysym names work as-is).
fn key_to_keysym(key: &str) -> String {
    match key.to_lowercase().as_str() {
        "enter" | "return" => "Return",
        "tab" => "Tab",
        "esc" | "escape" => "Escape",
        "space" => "space",
        "backspace" => "BackSpace",
        "delete" | "del" => "Delete",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" => "Prior",
        "pagedown" | "page_down" => "Next",
        _ => return key.to_string(),
    }
    .to_string()
}

// ── libei / portal fallback adapters ───────────────────────────────────────
//
// These bridge the wlroots-shaped public input API (output-relative integer
// coordinates, cua button codes, X-keysym key names) onto the libei worker
// (`libei` module), which speaks logical device-region floats and evdev
// codes. They are the recovery path for compositors with no
// `zwlr_virtual_pointer_v1` (KWin/Plasma, Mutter/GNOME) — see #1982.
//
// In a build WITHOUT the `portal-input` feature the pointer/scroll libei
// module does not exist, so these pointer-only adapters compile to stubs.
#[cfg(not(feature = "portal-input"))]
fn libei_wait_pointer_ready() -> anyhow::Result<()> {
    unreachable!("libei fallback compiled out (no portal-input feature)")
}
#[cfg(not(feature = "portal-input"))]
fn libei_wait_scroll_ready() -> anyhow::Result<()> {
    unreachable!("libei fallback compiled out (no portal-input feature)")
}
#[cfg(not(feature = "portal-input"))]
fn libei_click(_x: i32, _y: i32, _count: u32, _button: u8) -> anyhow::Result<()> {
    unreachable!("libei fallback compiled out (no portal-input feature)")
}
#[cfg(not(feature = "portal-input"))]
fn libei_scroll(_direction: &str, _amount: u32) -> anyhow::Result<()> {
    unreachable!("libei fallback compiled out (no portal-input feature)")
}
#[cfg(not(feature = "portal-input"))]
fn libei_move_absolute(_x: i32, _y: i32) -> anyhow::Result<()> {
    unreachable!("libei fallback compiled out (no portal-input feature)")
}
#[cfg(feature = "portal-input")]
fn cua_button_to_libei(button: u8) -> libei::Button {
    match button {
        2 => libei::Button::Middle,
        3 => libei::Button::Right,
        _ => libei::Button::Left,
    }
}

#[cfg(feature = "portal-input")]
fn libei_wait_pointer_ready() -> anyhow::Result<()> {
    libei::wait_pointer_ready()
}

#[cfg(feature = "portal-input")]
fn libei_wait_scroll_ready() -> anyhow::Result<()> {
    libei::wait_scroll_ready()
}

#[cfg(feature = "portal-input")]
fn libei_click(x: i32, y: i32, count: u32, button: u8) -> anyhow::Result<()> {
    let btn = cua_button_to_libei(button);
    let (w, h) = output_dimensions()?;
    let (px, py) = normalize_click_xy(x, y, w, h);
    libei::move_absolute(px as f64, py as f64)?;
    for i in 0..count.max(1) {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        libei::click(px as f64, py as f64, btn)?;
    }
    record_synth_cursor(px, py);
    Ok(())
}

#[cfg(feature = "portal-input")]
fn libei_scroll(direction: &str, amount: u32) -> anyhow::Result<()> {
    // libei scroll is logical-unit deltas; mirror the wlroots ±10/tick step.
    let (dx, dy): (f64, f64) = match direction.to_ascii_lowercase().as_str() {
        "up" => (0.0, -10.0),
        "down" => (0.0, 10.0),
        "left" => (-10.0, 0.0),
        "right" => (10.0, 0.0),
        other => anyhow::bail!("unknown scroll direction: {other}"),
    };
    for i in 0..amount.max(1) {
        if i > 0 {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        libei::scroll(dx, dy)?;
    }
    Ok(())
}

#[cfg(feature = "portal-input")]
fn libei_move_absolute(x: i32, y: i32) -> anyhow::Result<()> {
    // Match `move_cursor_absolute_vptr`: clamp to output bounds (no
    // default-to-centre — an explicit (0,0) move means the top-left corner).
    let (w, h) = output_dimensions()?;
    let px = x.clamp(0, (w as i32).saturating_sub(1));
    let py = y.clamp(0, (h as i32).saturating_sub(1));
    libei::move_absolute(px as f64, py as f64)?;
    record_synth_cursor(px, py);
    Ok(())
}

fn key_to_evdev(key: &str) -> Option<u32> {
    let code = match key.to_lowercase().as_str() {
        "enter" | "return" => 28,        // KEY_ENTER
        "tab" => 15,                     // KEY_TAB
        "esc" | "escape" => 1,           // KEY_ESC
        "space" => 57,                   // KEY_SPACE
        "backspace" => 14,               // KEY_BACKSPACE
        "delete" | "del" => 111,         // KEY_DELETE
        "up" => 103,                     // KEY_UP
        "down" => 108,                   // KEY_DOWN
        "left" => 105,                   // KEY_LEFT
        "right" => 106,                  // KEY_RIGHT
        "home" => 102,                   // KEY_HOME
        "end" => 107,                    // KEY_END
        "pageup" | "page_up" => 104,     // KEY_PAGEUP
        "pagedown" | "page_down" => 109, // KEY_PAGEDOWN
        // Letters a-z. evdev codes follow the QWERTY scancode layout, not the
        // alphabet, so each is listed explicitly (linux/input-event-codes.h).
        "a" => 30, // KEY_A
        "b" => 48, // KEY_B
        "c" => 46, // KEY_C
        "d" => 32, // KEY_D
        "e" => 18, // KEY_E
        "f" => 33, // KEY_F
        "g" => 34, // KEY_G
        "h" => 35, // KEY_H
        "i" => 23, // KEY_I
        "j" => 36, // KEY_J
        "k" => 37, // KEY_K
        "l" => 38, // KEY_L
        "m" => 50, // KEY_M
        "n" => 49, // KEY_N
        "o" => 24, // KEY_O
        "p" => 25, // KEY_P
        "q" => 16, // KEY_Q
        "r" => 19, // KEY_R
        "s" => 31, // KEY_S
        "t" => 20, // KEY_T
        "u" => 22, // KEY_U
        "v" => 47, // KEY_V
        "w" => 17, // KEY_W
        "x" => 45, // KEY_X
        "y" => 21, // KEY_Y
        "z" => 44, // KEY_Z
        // Digits. KEY_1=2 .. KEY_9=10, KEY_0=11 (input-event-codes.h).
        "1" => 2,  // KEY_1
        "2" => 3,  // KEY_2
        "3" => 4,  // KEY_3
        "4" => 5,  // KEY_4
        "5" => 6,  // KEY_5
        "6" => 7,  // KEY_6
        "7" => 8,  // KEY_7
        "8" => 9,  // KEY_8
        "9" => 10, // KEY_9
        "0" => 11, // KEY_0
        // Function keys. KEY_F1=59 .. KEY_F10=68, then KEY_F11=87, KEY_F12=88.
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        _ => return None,
    };
    Some(code)
}

// ── Nested cua-compositor injection ────────────────────────────────────────
//
// When cua-driver's nested compositor is `cua-compositor` (our patched wlroots,
// see nix/cua-driver/compositor/), it exposes a line-protocol control socket at
// $CUA_INJECT_SOCKET for what stock Wayland forbids: focus-FREE per-surface
// keyboard injection and MULTI-cursor pointer injection, routed by stable PID
// when available and xdg app_id only as a fallback. These helpers speak that
// protocol.
//
// The protocol is a simple line-based v1 exchange with per-command
// acknowledgement: the client sends `INJECT_PROTO_HELLO` and the compositor
// echoes it (or replies `err ...`), then every command line is answered by
// exactly one `ok` / `err <reason>` line. The client fails on a protocol
// mismatch, a read timeout, an EOF, or any compositor error line — an
// acknowledgement is transport evidence only, not proof the target changed.

/// Version banner exchanged at connect time: the client sends this line and the
/// compositor must echo it back verbatim to confirm both speak v1.
const INJECT_PROTO_HELLO: &str = "cua-inject v1";

/// The exact named keys the nested compositor's `k` command can emit — the
/// whitelist in `cua_key_named` (cua_compositor_patch.py). Compared
/// case-insensitively, matching the compositor's `strcasecmp`.
const INJECT_NAMED_KEYS: &[&str] = &[
    "enter",
    "return",
    "tab",
    "escape",
    "esc",
    "backspace",
    "space",
    "up",
    "down",
    "left",
    "right",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
];

/// The control socket path, when running against the nested cua-compositor.
pub fn inject_socket_path() -> Option<String> {
    std::env::var("CUA_INJECT_SOCKET")
        .ok()
        .filter(|s| !s.is_empty())
}

/// True when input should be routed through the nested cua-compositor's control
/// socket (focus-free / multi-cursor) rather than wtype / virtual-pointer.
pub fn is_inject_mode() -> bool {
    inject_socket_path().is_some()
}

static INJECT_FOCUSED_TARGET: OnceLock<Mutex<Option<(u32, u64)>>> = OnceLock::new();

fn remember_inject_focused_target(pid: u32, window_id: u64) {
    let target = INJECT_FOCUSED_TARGET.get_or_init(|| Mutex::new(None));
    if let Ok(mut target) = target.lock() {
        *target = Some((pid, window_id));
    }
}

fn inject_focused_target() -> anyhow::Result<(u32, u64)> {
    INJECT_FOCUSED_TARGET
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|target| *target)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "foreground_unavailable: cua-compositor has no verified foreground target; \
                 call bring_to_front before desktop keyboard input"
            )
        })
}

fn inject_scroll_desktop(x: i32, y: i32, direction: &str, amount: u32) -> anyhow::Result<()> {
    let windows = crate::atspi::list_windows(None);
    let target = windows
        .iter()
        .filter(|window| {
            window.is_on_screen
                && x >= window.x
                && y >= window.y
                && x < window.x.saturating_add(window.width as i32)
                && y < window.y.saturating_add(window.height as i32)
        })
        .max_by_key(|window| window.z_index.unwrap_or_default())
        .or_else(|| {
            let (pid, _) = inject_focused_target().ok()?;
            windows.iter().find(|window| window.pid == Some(pid))
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "foreground_unavailable: no cua-compositor window contains desktop point ({x},{y})"
            )
        })?;
    let pid = target.pid.ok_or_else(|| {
        anyhow::anyhow!(
            "foreground_unavailable: desktop point ({x},{y}) resolved to a window without a pid"
        )
    })?;
    inject_scroll(
        pid,
        target.xid,
        f64::from(x.saturating_sub(target.x)),
        f64::from(y.saturating_sub(target.y)),
        direction,
        amount,
    )
}

/// Reject any character the nested compositor cannot type before it reaches the
/// wire. The compositor's chartab (`cua_init_keymap`) only covers printable
/// ASCII (`0x20..=0x7E`) plus newline and tab; anything else — Unicode, other
/// control bytes — would be silently dropped, so fail loudly instead.
fn validate_injectable_text(text: &str) -> anyhow::Result<()> {
    for ch in text.chars() {
        let ok = ch == '\n' || ch == '\t' || (ch.is_ascii() && !ch.is_ascii_control());
        if !ok {
            anyhow::bail!(
                "cua-compositor cannot type {ch:?}: only printable ASCII plus newline and tab \
                 are supported in the v1 injection protocol"
            );
        }
    }
    Ok(())
}

/// Reject any key name outside the compositor's named-key whitelist before
/// sending. Mirrors `cua_key_named`; unsupported names must fail here rather
/// than being silently ignored by the compositor.
fn validate_injectable_key(key: &str) -> anyhow::Result<()> {
    let normalized = key.trim().to_ascii_lowercase();
    if INJECT_NAMED_KEYS.contains(&normalized.as_str()) {
        Ok(())
    } else {
        anyhow::bail!(
            "cua-compositor does not support key {key:?}; supported keys: {}",
            INJECT_NAMED_KEYS.join(", ")
        );
    }
}

fn validate_injectable_hotkey(keys: &[String]) -> anyhow::Result<(String, String)> {
    let (key, modifiers) = keys
        .split_last()
        .ok_or_else(|| anyhow::anyhow!("cua-compositor hotkey requires a non-modifier key"))?;
    let key = key.trim().to_ascii_lowercase();
    if !(key.len() == 1 && key.is_ascii()) && !INJECT_NAMED_KEYS.contains(&key.as_str()) {
        anyhow::bail!("cua-compositor does not support hotkey key {key:?}");
    }
    let mut normalized = Vec::with_capacity(modifiers.len());
    for modifier in modifiers {
        let modifier = modifier.trim().to_ascii_lowercase();
        let canonical = match modifier.as_str() {
            "ctrl" | "control" => "ctrl",
            "shift" => "shift",
            "alt" | "option" => "alt",
            "meta" | "super" | "win" | "cmd" => "meta",
            _ => anyhow::bail!("cua-compositor does not support modifier {modifier:?}"),
        };
        normalized.push(canonical);
    }
    if normalized.is_empty() {
        anyhow::bail!("cua-compositor hotkey requires at least one modifier");
    }
    Ok((normalized.join(","), key))
}

/// Interpret the compositor's handshake reply. Accepts only the verbatim v1
/// banner; a compositor `err ...` line or anything else is a protocol mismatch.
fn parse_inject_hello(line: &str) -> anyhow::Result<()> {
    let trimmed = line.trim();
    if trimmed == INJECT_PROTO_HELLO {
        Ok(())
    } else if let Some(reason) = trimmed.strip_prefix("err") {
        anyhow::bail!(
            "cua-compositor rejected the v1 handshake:{}",
            if reason.trim().is_empty() {
                String::new()
            } else {
                format!(" {}", reason.trim())
            }
        )
    } else {
        anyhow::bail!(
            "cua-compositor protocol mismatch: expected {INJECT_PROTO_HELLO:?}, got {trimmed:?}"
        )
    }
}

/// Interpret a single per-command acknowledgement line. `ok` succeeds; `err
/// <reason>` and any unrecognised line fail.
fn parse_inject_reply(line: &str) -> anyhow::Result<()> {
    let trimmed = line.trim();
    if trimmed == "ok" {
        Ok(())
    } else if let Some(reason) = trimmed.strip_prefix("err") {
        let reason = reason.trim();
        if reason.is_empty() {
            anyhow::bail!("cua-compositor rejected the command");
        }
        anyhow::bail!("cua-compositor rejected the command: {reason}")
    } else {
        anyhow::bail!("unexpected cua-compositor response: {trimmed:?}")
    }
}

/// Read one newline-terminated response line, mapping timeout and EOF to clear
/// errors so the caller never blocks forever on an unresponsive compositor.
fn read_inject_line(reader: &mut impl std::io::BufRead) -> anyhow::Result<String> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            anyhow::anyhow!("cua-compositor did not respond within the timeout")
        } else {
            anyhow::anyhow!("cua-compositor read failed: {e}")
        }
    })?;
    if n == 0 {
        anyhow::bail!("cua-compositor closed the connection before responding");
    }
    Ok(line)
}

/// Connect to the nested cua-compositor control socket, perform the v1
/// handshake, then send each command line and require exactly one
/// acknowledgement per command. Fails on protocol mismatch, timeout, EOF, or a
/// compositor error line — the earlier fire-and-forget path hid all of these.
fn inject_exchange(lines: &[String]) -> anyhow::Result<Vec<String>> {
    use std::io::{BufReader, Write};
    use std::os::unix::net::UnixStream;
    let path = inject_socket_path().ok_or_else(|| anyhow::anyhow!("CUA_INJECT_SOCKET not set"))?;
    // The nested compositor may still be starting; retry the connect briefly.
    let mut stream = None;
    for _ in 0..60 {
        match UnixStream::connect(&path) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
    let stream =
        stream.ok_or_else(|| anyhow::anyhow!("could not connect to inject socket {path}"))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    // v1 handshake: send our banner and require the compositor to echo it.
    writeln!(writer, "{INJECT_PROTO_HELLO}")?;
    writer.flush()?;
    parse_inject_hello(&read_inject_line(&mut reader)?)?;

    let mut replies = Vec::with_capacity(lines.len());
    // One command per line; block on its response before the next.
    for l in lines {
        writeln!(writer, "{l}")?;
        writer.flush()?;
        replies.push(read_inject_line(&mut reader)?);
    }
    Ok(replies)
}

fn inject_send(lines: &[String]) -> anyhow::Result<()> {
    for reply in inject_exchange(lines)? {
        parse_inject_reply(&reply)?;
    }
    Ok(())
}

fn parse_inject_geometry(line: &str) -> anyhow::Result<((i32, i32), (i32, i32))> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() == 5 && fields[0] == "geometry" {
        return Ok((
            (fields[1].parse()?, fields[2].parse()?),
            (fields[3].parse()?, fields[4].parse()?),
        ));
    }
    if let Some(reason) = line.trim().strip_prefix("err") {
        anyhow::bail!("cua-compositor geometry query failed: {}", reason.trim());
    }
    anyhow::bail!(
        "unexpected cua-compositor geometry response: {:?}",
        line.trim()
    )
}

/// Return the offset that rebases native Wayland accessibility coordinates into
/// the nested compositor's root-surface/output coordinate space.
pub fn inject_accessibility_offset(pid: u32) -> Option<(i32, i32)> {
    if !is_inject_mode() || pid == 0 {
        return None;
    }
    let replies = inject_exchange(&[format!("g {pid}")]).ok()?;
    replies
        .first()
        .and_then(|line| parse_inject_geometry(line).ok())
        .map(|geometry| geometry.0)
}

fn inject_window_origin(pid: u32) -> Option<(i32, i32)> {
    if !is_inject_mode() || pid == 0 {
        return None;
    }
    let replies = inject_exchange(&[format!("g {pid}")]).ok()?;
    replies
        .first()
        .and_then(|line| parse_inject_geometry(line).ok())
        .map(|geometry| geometry.1)
}

/// Resolve a window_id to its xdg app_id via the stable identity registry that
/// [`list_windows`] populates (falling back to sway IPC / AT-SPI through
/// [`identity_for`]) — the nested cua-compositor injection protocol addresses
/// windows by app_id. Returns `None` when no identity is registered or the
/// resolved app_id is empty, so callers can surface a clear error.
pub fn app_id_for_window(window_id: u64) -> Option<String> {
    identity_for(window_id)
        .map(|identity| identity.app_id)
        .filter(|s| !s.is_empty())
}

/// Resolve the WM_CLASS-equivalent (instance, class) pair for a window. On
/// X11 reads `WM_CLASS`; on Wayland reuses [`app_id_for_window`] and returns
/// `(app_id, app_id)` — the closest analogue, since foreign-toplevel exposes
/// a single app_id and not the X11 instance/class split. Used by terminal
/// emulator detection on `is_terminal_window` so Ghostty / kitty / alacritty
/// are recognised on Wayland too.
pub fn wm_class_dispatch(window_id: u64) -> Option<(String, String)> {
    if is_wayland() {
        let app = app_id_for_window(window_id)?;
        return Some((app.clone(), app));
    }
    crate::x11::wm_class_for_window(window_id)
}

fn to_hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

/// Map a cua/X11 mouse-button number to its evdev (wl_pointer) button code.
fn evdev_button(x_button: u32) -> u32 {
    match x_button {
        3 => 0x111, // BTN_RIGHT
        2 => 0x112, // BTN_MIDDLE
        _ => 0x110, // BTN_LEFT
    }
}

/// Sentinel error when a window_id has no registered cua-compositor identity.
fn no_app_id(window_id: u64) -> anyhow::Error {
    anyhow::anyhow!(
        "no known cua-compositor app_id for window {window_id}; call list_windows first so its \
         Wayland identity is registered"
    )
}

/// Resolve the strongest target token understood by the private nested
/// compositor. AT-SPI window IDs are synthetic on Wayland, but its process ID
/// is the same credential the compositor observes on the owning wl_client.
/// Fall back to app_id for clients whose accessibility metadata has no PID.
pub fn inject_target_for_window(window_id: u64) -> anyhow::Result<String> {
    inject_target_for_window_with_pid(window_id, None)
}

fn inject_target_for_window_with_pid(
    window_id: u64,
    target_pid: Option<u32>,
) -> anyhow::Result<String> {
    if let Some(pid) = target_pid {
        anyhow::ensure!(pid > 0, "cua-compositor target pid must be positive");
        // Electron/Chromium may create the xdg_toplevel from a renderer child
        // rather than the public tool target. The compositor verifies the
        // wl_client owner is this process or one of its descendants.
        return Ok(format!("root:{pid}"));
    }
    let atspi = crate::atspi::list_windows(None);
    let direct_pid = atspi
        .iter()
        .find(|window| window.xid == window_id)
        .and_then(|window| window.pid);
    let correlated_pid = identity_for(window_id)
        .as_ref()
        .and_then(|identity| unique_atspi_pid_for_identity(identity, &atspi));
    if let Some(pid) = direct_pid.or(correlated_pid) {
        return Ok(format!("pid:{pid}"));
    }
    app_id_for_window(window_id).ok_or_else(|| no_app_id(window_id))
}

/// Correlate a connection-local native toplevel with its AT-SPI process. Exact
/// titles are the same bridge used by window enumeration; requiring one unique
/// PID prevents a shared toolkit app_id from silently selecting another app.
fn unique_atspi_pid_for_identity(
    identity: &ToplevelIdentity,
    windows: &[WindowInfo],
) -> Option<u32> {
    if identity.title.is_empty() {
        return None;
    }
    let mut pids = windows
        .iter()
        .filter(|window| window.title == identity.title)
        .filter_map(|window| window.pid)
        .collect::<Vec<_>>();
    pids.sort_unstable();
    pids.dedup();
    (pids.len() == 1).then(|| pids[0])
}

/// Focus-free type into the window's surface (no focus change). Rejects any
/// character the compositor cannot emit before touching the socket.
pub fn inject_type_text(target_pid: u32, window_id: u64, text: &str) -> anyhow::Result<()> {
    validate_injectable_text(text)?;
    let app = inject_target_for_window_with_pid(window_id, Some(target_pid))?;
    inject_send(&[format!("t {app} {}", to_hex(text))])
}

/// Focus-free named-key press into the window's surface. Rejects any key
/// outside the compositor's whitelist before touching the socket.
pub fn inject_press_key(target_pid: u32, window_id: u64, key: &str) -> anyhow::Result<()> {
    validate_injectable_key(key)?;
    let app = inject_target_for_window_with_pid(window_id, Some(target_pid))?;
    inject_send(&[format!("k {app} {}", key.trim())])
}

/// Focus-free modifier chord into the target surface.
pub fn inject_hotkey(target_pid: u32, window_id: u64, keys: &[String]) -> anyhow::Result<()> {
    let (modifiers, key) = validate_injectable_hotkey(keys)?;
    let app = inject_target_for_window_with_pid(window_id, Some(target_pid))?;
    inject_send(&[format!("h {app} {modifiers} {key}")])
}

/// Focus-free wheel/axis input at one target-local point.
pub fn inject_scroll(
    target_pid: u32,
    window_id: u64,
    x: f64,
    y: f64,
    direction: &str,
    amount: u32,
) -> anyhow::Result<()> {
    let app = inject_target_for_window_with_pid(window_id, Some(target_pid))?;
    let (axis, value) = match direction.to_ascii_lowercase().as_str() {
        "up" => (0, -15.0),
        "down" | "page" => (0, 15.0),
        "left" => (1, -15.0),
        "right" => (1, 15.0),
        _ => anyhow::bail!("unsupported cua-compositor scroll direction {direction:?}"),
    };
    let mut lines = vec![format!("m {app} 0 {x:.1} {y:.1}")];
    lines.extend((0..amount.max(1)).map(|_| format!("a {app} 0 {axis} {value:.1}")));
    inject_send(&lines)
}

/// Focus-free click into the window's surface via the nested cua-compositor.
/// Coordinates are window-local, matching the rest of the inject protocol.
pub fn inject_click(
    target_pid: u32,
    window_id: u64,
    x: f64,
    y: f64,
    count: u32,
    button: u8,
) -> anyhow::Result<()> {
    let app = inject_target_for_window_with_pid(window_id, Some(target_pid))?;
    let btn = evdev_button(button as u32);
    let n = count.max(1);
    let mut lines = Vec::with_capacity((n as usize) * 4);
    for i in 0..n {
        if i > 0 {
            // The line protocol is batch-oriented, so use a tiny move-only
            // separator between clicks to give the compositor a frame boundary
            // without introducing a protocol-level sleep primitive.
            lines.push(format!("m {app} 0 {x:.1} {y:.1}"));
        }
        lines.push(format!("m {app} 0 {x:.1} {y:.1}"));
        lines.push(format!("b {app} 0 {btn} 1"));
        lines.push(format!("b {app} 0 {btn} 0"));
    }
    inject_send(&lines)
}

/// A single pointer drag for `inject_parallel_drags`: window-local waypoints,
/// driven by cursor `idx` so several run concurrently on one window.
pub struct InjectDrag {
    pub app_id: String,
    pub idx: usize,
    pub x_button: u32,
    pub path: Vec<(f64, f64)>,
    pub steps: usize,
}

fn resample(path: &[(f64, f64)], steps: usize) -> Vec<(f64, f64)> {
    if path.len() < 2 || steps == 0 {
        return path.to_vec();
    }
    let seglen: Vec<f64> = path
        .windows(2)
        .map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt())
        .collect();
    let total: f64 = seglen.iter().sum();
    if total == 0.0 {
        return vec![path[0]; steps + 1];
    }
    let mut out = Vec::with_capacity(steps + 1);
    for s in 0..=steps {
        let target = total * (s as f64) / (steps as f64);
        let mut acc = 0.0;
        let mut pt = path[path.len() - 1];
        for (i, &l) in seglen.iter().enumerate() {
            if acc + l >= target || i == seglen.len() - 1 {
                let f = if l > 0.0 { (target - acc) / l } else { 0.0 };
                pt = (
                    path[i].0 + (path[i + 1].0 - path[i].0) * f,
                    path[i].1 + (path[i + 1].1 - path[i].1) * f,
                );
                break;
            }
            acc += l;
        }
        out.push(pt);
    }
    out
}

/// Run N pointer drags concurrently on their target windows: each cursor presses
/// at its start, glides through its (interleaved) waypoints, and releases. This
/// is true multi-cursor — each `idx` is an independent cursor in the compositor.
pub fn inject_parallel_drags(drags: &[InjectDrag]) -> anyhow::Result<()> {
    if drags.is_empty() {
        return Ok(());
    }
    let resampled: Vec<Vec<(f64, f64)>> = drags
        .iter()
        .map(|d| resample(&d.path, d.steps.max(1)))
        .collect();
    let max_steps = resampled.iter().map(|p| p.len()).max().unwrap_or(0);
    let mut lines = Vec::new();
    // Press each cursor at its start point.
    for (d, pts) in drags.iter().zip(&resampled) {
        let (x, y) = pts[0];
        lines.push(format!("m {} {} {x:.1} {y:.1}", d.app_id, d.idx));
        lines.push(format!(
            "b {} {} {} 1",
            d.app_id,
            d.idx,
            evdev_button(d.x_button)
        ));
    }
    // Glide all cursors together, one interleaved step at a time.
    for s in 1..max_steps {
        for (d, pts) in drags.iter().zip(&resampled) {
            let (x, y) = pts[s.min(pts.len() - 1)];
            lines.push(format!("m {} {} {x:.1} {y:.1}", d.app_id, d.idx));
        }
    }
    // Release each cursor.
    for (d, _) in drags.iter().zip(&resampled) {
        lines.push(format!(
            "b {} {} {} 0",
            d.app_id,
            d.idx,
            evdev_button(d.x_button)
        ));
    }
    inject_send(&lines)
}

/// Focus-free single drag using the same per-surface path as parallel drags.
pub fn inject_drag(
    keyboard_admission: &KeyboardAdmission,
    target_pid: u32,
    window_id: u64,
    from: (f64, f64),
    to: (f64, f64),
    steps: usize,
    x_button: u32,
    modifiers: &[String],
) -> anyhow::Result<()> {
    let app_id = inject_target_for_window_with_pid(window_id, Some(target_pid))?;
    virtual_keyboard::with_modifiers_held(keyboard_admission, modifiers, || {
        inject_parallel_drags(&[InjectDrag {
            app_id,
            idx: 0,
            x_button,
            path: vec![from, to],
            steps,
        }])
    })
}

fn wayland_atspi_windows(filter_pid: Option<u32>) -> Vec<WindowInfo> {
    let mut windows = crate::atspi::list_windows(filter_pid);
    // AT-SPI can retain a toolkit's default placement (commonly 120,120)
    // after Sway has placed the real toplevel at another origin. Reconcile the
    // fallback records with compositor-owned metadata before exposing them to
    // callers; element bounds already use this same authoritative Sway tree.
    for window in &mut windows {
        let compositor = window
            .pid
            .and_then(compositor_ipc::window_for_pid)
            .or_else(|| compositor_ipc::window_for_title(&window.title))
            .or_else(|| compositor_ipc::window_for_app_id(&window.app_name));
        if let Some(compositor) = compositor {
            window.xid = compositor.id;
            window.x = compositor.x;
            window.y = compositor.y;
            window.width = compositor.width;
            window.height = compositor.height;
            window.is_on_screen =
                compositor.visible && compositor.width > 0 && compositor.height > 0;
        }
    }
    if is_inject_mode() {
        for window in &mut windows {
            if let Some(pid) = window.pid {
                if let Some((window_x, window_y)) = inject_window_origin(pid) {
                    window.x = window_x;
                    window.y = window_y;
                }
            }
        }
    }
    windows
}

/// Window-enumeration dispatcher: native Wayland when available, else X11.
fn list_windows_dispatch_logical(filter_pid: Option<u32>) -> Vec<WindowInfo> {
    if wayland_enabled() && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        // Prefer the richer wlroots protocol. The generic staging protocol is
        // only consulted when wlroots yields no windows (including when its
        // manager global is absent).
        let native = match list_windows() {
            Ok(ws) if !ws.is_empty() => Ok(enrich_native_windows(
                ws,
                wayland_atspi_windows(filter_pid),
                is_inject_mode(),
            )),
            Ok(_) => ext_toplevel::list_windows(),
            Err(wlr_error) => ext_toplevel::list_windows().map_err(|ext_error| {
                anyhow::anyhow!(
                    "wlr provider failed ({wlr_error}); ext provider failed ({ext_error})"
                )
            }),
        };
        match native {
            Ok(ws) if !ws.is_empty() => {
                if let Some(pid) = filter_pid {
                    if let Some(filtered) = native_windows_for_pid(ws, pid) {
                        return filtered;
                    }
                } else {
                    return ws;
                }
                // A compositor window without pid metadata cannot satisfy a
                // pid-scoped request. Continue to the AT-SPI registry.
                let ws = wayland_atspi_windows(filter_pid);
                if !ws.is_empty() {
                    return ws;
                }
            }
            Ok(_) => {
                if let Some(ws) = shell_helper::list_windows(filter_pid).filter(|ws| !ws.is_empty())
                {
                    return ws;
                }
                let ws = wayland_atspi_windows(filter_pid);
                if !ws.is_empty() {
                    return ws;
                }
            }
            Err(e) => {
                if let Some(ws) = shell_helper::list_windows(filter_pid).filter(|ws| !ws.is_empty())
                {
                    tracing::debug!(
                        "native Wayland protocols unavailable ({e}); using compositor helper"
                    );
                    return ws;
                }
                tracing::warn!("native Wayland list_windows failed: {e}; trying AT-SPI registry");
                let ws = wayland_atspi_windows(filter_pid);
                if !ws.is_empty() {
                    return ws;
                }
            }
        }
        // Last resort under Wayland: an Xwayland app may still have an X11 XID.
    }
    // If native enumeration and its AT-SPI fallback found nothing, X11 may still
    // expose XWayland clients. Merge one final AT-SPI snapshot so native windows
    // remain visible on hybrid sessions even when neither foreign-toplevel
    // protocol is advertised (#1978). Gated on the native-Wayland opt-in.
    //
    // Caveats for the merged AT-SPI entries: they carry a synthetic (non-X11)
    // xid and zero geometry (x/y/w/h = 0), like the existing wlroots AT-SPI
    // fallback — so `bring_to_front` / `screenshot_window` / pixel translation
    // against them error cleanly rather than acting (input on GNOME/KDE routes
    // by pid + screen coords, not xid, so it's unaffected). Dedup is per-pid, so
    // the rare app owning BOTH an XWayland window and a separate native-Wayland
    // toplevel would list only the XWayland one.
    let mut ws = crate::x11::list_windows(filter_pid);
    if wayland_enabled() && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let seen: std::collections::HashSet<u32> = ws.iter().filter_map(|w| w.pid).collect();
        // A specific pid already resolved via X11 needs no AT-SPI walk (a full
        // D-Bus enumeration of every registered app): it can only add duplicates.
        let already_covered = filter_pid.map_or(false, |p| seen.contains(&p));
        if !already_covered {
            merge_atspi_windows(&mut ws, &seen, wayland_atspi_windows(filter_pid));
        }
    }
    ws
}

/// Enumerate windows in the same guest-output physical-pixel coordinate space
/// as desktop and window screenshots.
pub fn list_windows_dispatch(filter_pid: Option<u32>) -> Vec<WindowInfo> {
    match list_windows_dispatch_checked(filter_pid) {
        Ok((windows, _)) => windows,
        Err(error) => {
            tracing::warn!("window enumeration rejected: {error}");
            Vec::new()
        }
    }
}

/// Generation-checked window enumeration. User-facing tools use this form so
/// a resize or UI-scale transition cannot return rectangles transformed with
/// a different output state than the one attached to the response.
pub fn list_windows_dispatch_checked(
    filter_pid: Option<u32>,
) -> anyhow::Result<(Vec<WindowInfo>, Option<CanonicalOutputMetadata>)> {
    let before = read_buzzardos_output_state()?;
    let mut windows = list_windows_dispatch_logical(filter_pid);
    for window in &mut windows {
        (window.x, window.y, window.width, window.height) = logical_rect_to_canonical_for_state(
            before,
            window.x,
            window.y,
            window.width,
            window.height,
        );
    }
    let after = read_buzzardos_output_state()?;
    require_same_output_generation(before, after)?;
    Ok((windows, before.map(BuzzardOSOutputState::metadata)))
}

fn merge_atspi_windows(
    windows: &mut Vec<WindowInfo>,
    x11_pids: &std::collections::HashSet<u32>,
    atspi_windows: Vec<WindowInfo>,
) {
    for window in atspi_windows {
        // XWayland apps appear in both lists; keep the X11 entry (real XID +
        // geometry) and retain every native frame whose pid X11 did not expose.
        if window.pid.is_none_or(|pid| !x11_pids.contains(&pid)) {
            windows.push(window);
        }
    }
}

fn process_name_matches_app_id(process: &crate::proc_fs::ProcessInfo, app_id: &str) -> bool {
    let normalized = app_id
        .trim()
        .strip_suffix(".desktop")
        .unwrap_or(app_id.trim())
        .to_ascii_lowercase();
    let short = normalized.rsplit(['.', '/']).next().unwrap_or(&normalized);
    let command = std::path::Path::new(&process.cmdline)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&process.cmdline)
        .to_ascii_lowercase();
    let name = process.name.to_ascii_lowercase();
    name == normalized || name == short || command == normalized || command == short
}

fn process_is_live_current_user(pid: u32) -> bool {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    let live = status
        .lines()
        .find_map(|line| line.strip_prefix("State:"))
        .is_some_and(|state| !state.trim_start().starts_with('Z'));
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|uids| uids.split_whitespace().next())
        .and_then(|uid| uid.parse::<u32>().ok());
    let self_uid = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .and_then(|uids| uids.split_whitespace().next())
                .and_then(|uid| uid.parse::<u32>().ok())
        });
    live && uid.is_some() && uid == self_uid
}

/// Recover the owning process only when one live process belonging to this
/// interactive user has an executable/comm name exactly matching the
/// compositor app-id. The stable foreign-toplevel id remains the authoritative
/// window identity; this PID supplies the process guard required by public
/// tool contracts for non-AT-SPI clients such as terminals and games.
fn unique_process_pid_for_app_id(app_id: &str) -> Option<u32> {
    if app_id.trim().is_empty() {
        return None;
    }
    let mut matches = crate::proc_fs::list_processes()
        .into_iter()
        .filter(|process| {
            process_is_live_current_user(process.pid)
                && process_name_matches_app_id(process, app_id)
        })
        .map(|process| process.pid)
        .collect::<Vec<_>>();
    matches.sort_unstable();
    matches.dedup();
    (matches.len() == 1).then(|| matches[0])
}

fn enrich_native_windows(
    mut native: Vec<WindowInfo>,
    atspi: Vec<WindowInfo>,
    adopt_atspi_ids: bool,
) -> Vec<WindowInfo> {
    let mut claimed = std::collections::HashSet::new();
    let mut claimed_pids = std::collections::HashSet::new();
    for window in &mut native {
        if window.pid.is_some() {
            let native_title = undecorated_native_title(window);
            let pid = window.pid;
            if let Some(pid) = pid {
                claimed_pids.insert(pid);
            }
            if let Some(index) = atspi
                .iter()
                .enumerate()
                .find_map(|(index, candidate)| {
                    (!claimed.contains(&index)
                        && candidate.pid == pid
                        && (native_title.is_empty() || candidate.title == native_title))
                        .then_some(index)
                })
                .or_else(|| {
                    atspi.iter().enumerate().find_map(|(index, candidate)| {
                        (!claimed.contains(&index) && candidate.pid == pid).then_some(index)
                    })
                })
            {
                claimed.insert(index);
            }
            continue;
        }
        let native_title = undecorated_native_title(window);
        let title_match = atspi.iter().enumerate().find_map(|(index, candidate)| {
            (!claimed.contains(&index)
                && !native_title.is_empty()
                && candidate.title == native_title)
                .then_some(index)
        });
        let app_match = title_match.or_else(|| {
            let matches = atspi
                .iter()
                .enumerate()
                .filter(|(index, candidate)| {
                    !claimed.contains(index)
                        && !window.app_name.is_empty()
                        && candidate.app_name == window.app_name
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            (matches.len() == 1).then(|| matches[0])
        });
        let Some(index) = app_match else {
            window.pid = unique_process_pid_for_app_id(&window.app_name);
            continue;
        };
        claimed.insert(index);
        let candidate = &atspi[index];
        window.pid = candidate.pid;
        if let Some(pid) = candidate.pid {
            claimed_pids.insert(pid);
        }
        if adopt_atspi_ids {
            let toplevel = Toplevel {
                title: undecorated_native_title(window).to_owned(),
                app_id: window.app_name.clone(),
                closed: false,
                ..Toplevel::default()
            };
            window.xid = candidate.xid;
            remember_identity(window.xid, &toplevel);
        }
        if window.width == 0 || window.height == 0 {
            window.x = candidate.x;
            window.y = candidate.y;
            window.width = candidate.width;
            window.height = candidate.height;
        }
        if adopt_atspi_ids {
            if let Some(pid) = candidate.pid {
                if let Some((window_x, window_y)) = inject_window_origin(pid) {
                    window.x = window_x;
                    window.y = window_y;
                }
            }
        }
        // Either provider can prove a window is hidden. In particular AT-SPI
        // often retains its last mapped extents while the compositor reports
        // the toplevel minimized, so accessibility metadata must not turn a
        // compositor-hidden window visible again.
        window.is_on_screen &= candidate.is_on_screen;
    }
    // Layer-shell surfaces are intentionally absent from foreign-toplevel
    // protocols, but remain first-class AT-SPI applications. Preserve an
    // uncorrelated accessible application only when its process owns no native
    // toplevel. Toolkits also expose internal dialogs, popovers, and web
    // documents as AT-SPI application/window roots; once their process is
    // represented by compositor-owned toplevels those objects must not become
    // duplicate public windows.
    native.extend(atspi.into_iter().enumerate().filter_map(|(index, window)| {
        (!claimed.contains(&index) && window.pid.is_none_or(|pid| !claimed_pids.contains(&pid)))
            .then_some(window)
    }));
    native
}

fn undecorated_native_title(window: &WindowInfo) -> &str {
    if window.app_name.is_empty() {
        return &window.title;
    }
    let suffix = format!(" [{}]", window.app_name);
    window.title.strip_suffix(&suffix).unwrap_or(&window.title)
}

/// Return native records only when they contain a real match for a pid-scoped
/// request. Ext records whose AT-SPI merge left pid unknown must not suppress
/// the later AT-SPI and X11 fallback providers.
fn native_windows_for_pid(windows: Vec<WindowInfo>, pid: u32) -> Option<Vec<WindowInfo>> {
    let matching: Vec<_> = windows
        .into_iter()
        .filter(|window| window.pid == Some(pid))
        .collect();
    (!matching.is_empty()).then_some(matching)
}

/// Snapshot of which wlroots manager globals the running compositor advertises.
/// Used by the `health_report` Wayland backend probe to distinguish a working
/// session from one missing screencopy or virtual-pointer support.
#[derive(Default, Clone, Debug)]
pub struct WaylandManagers {
    pub foreign_toplevel: bool,
    pub screencopy: bool,
    pub virtual_pointer: bool,
    pub wl_shm: bool,
    /// Staging `ext-image-copy-capture-v1` manager — sway 1.10+, labwc
    /// 0.8+, niri, KDE 6.2+, GNOME mutter 47+.
    pub ext_image_copy_capture: bool,
    /// Companion `ext-output-image-capture-source-v1` source manager —
    /// required to capture a `wl_output` via the staging protocol.
    pub ext_output_image_capture_source: bool,
}

/// Perform a single registry roundtrip and report which of the manager
/// interfaces the doctor cares about advertise themselves. Returns `Err` only
/// when we can't even open a Wayland connection — a successful connect with
/// no managers still resolves to an all-false snapshot.
pub fn probe_managers() -> anyhow::Result<WaylandManagers> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    // Reuse the existing State for wlroots managers, then do a parallel
    // probe for staging ext-image-copy-capture interfaces by walking the
    // raw registry events (no binding required — we only need presence).
    let ext_probe = probe_ext_interfaces().unwrap_or_default();
    Ok(WaylandManagers {
        foreign_toplevel: state.manager.is_some(),
        screencopy: state.scrcopy_manager.is_some(),
        virtual_pointer: state.vptr_manager.is_some(),
        wl_shm: state.shm.is_some(),
        ext_image_copy_capture: ext_probe.image_copy_capture,
        ext_output_image_capture_source: ext_probe.output_image_capture_source,
    })
}

#[derive(Default, Clone, Copy, Debug)]
struct ExtInterfaceProbe {
    image_copy_capture: bool,
    output_image_capture_source: bool,
}

/// Probe registry for `ext-image-copy-capture-v1` + companion source manager
/// presence without binding them. Cheap (one roundtrip) and side-effect
/// free.
fn probe_ext_interfaces() -> anyhow::Result<ExtInterfaceProbe> {
    let conn = Connection::connect_to_env()?;
    let mut queue = conn.new_event_queue::<ExtProbeState>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = ExtProbeState::default();
    queue.roundtrip(&mut state)?;
    Ok(state.probe)
}

#[derive(Default)]
struct ExtProbeState {
    probe: ExtInterfaceProbe,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ExtProbeState {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { interface, .. } = event {
            match interface.as_str() {
                "ext_image_copy_capture_manager_v1" => {
                    state.probe.image_copy_capture = true;
                }
                "ext_output_image_capture_source_manager_v1" => {
                    state.probe.output_image_capture_source = true;
                }
                _ => {}
            }
        }
    }
}

// Suppress dead-code warning for the unused BTN_LEFT alias kept for backward
// compatibility with earlier slice constants.
#[allow(dead_code)]
const _BTN_LEFT_ALIAS: u32 = BTN_LEFT;

#[cfg(test)]
mod tests {
    use super::*;

    fn output_state(
        physical_width: u32,
        physical_height: u32,
        host_surface_scale_120: u32,
        guest_ui_scale_120: u32,
        geometry_generation: u64,
    ) -> BuzzardOSOutputState {
        BuzzardOSOutputState {
            schema: 7,
            physical_width,
            physical_height,
            host_surface_scale_120,
            guest_ui_scale_120,
            logical_width: (u64::from(physical_width) * 120 / u64::from(guest_ui_scale_120)).max(1)
                as u32,
            logical_height: (u64::from(physical_height) * 120 / u64::from(guest_ui_scale_120))
                .max(1) as u32,
            geometry_generation,
        }
    }

    #[test]
    fn physical_and_logical_rect_transforms_share_exact_output_edges() {
        assert_eq!(
            scaled_rect(0, 0, 1280, 800, 1707, 1067, 1280, 800, true),
            (0, 0, 1707, 1067)
        );
        assert_eq!(
            scaled_rect(0, 0, 1707, 1067, 1280, 800, 1707, 1067, false),
            (0, 0, 1280, 800)
        );
        assert_eq!(
            scaled_rect(-4, -4, 4, 4, 160, 160, 120, 120, true),
            (-6, -6, 6, 6)
        );
    }

    #[test]
    fn output_geometry_matrix_separates_host_and_guest_scale() {
        let guest_presets = [None, Some(120), Some(150), Some(180), Some(210), Some(240)];
        for host_scale in [120_u32, 150, 160, 180, 210, 240] {
            for preset in guest_presets {
                let guest_scale = preset.unwrap_or(host_scale);
                let state = output_state(1919, 1079, host_scale, guest_scale, 41);
                let validated = state.validate().expect("matrix geometry must validate");
                assert_eq!(
                    (validated.physical_width, validated.physical_height),
                    (1919, 1079)
                );
                assert_eq!(validated.host_surface_scale_120, host_scale);
                assert_eq!(validated.guest_ui_scale_120, guest_scale);
                assert_eq!(validated.logical_width, 1919 * 120 / guest_scale);
                assert_eq!(validated.logical_height, 1079 * 120 / guest_scale);
            }
        }
    }

    #[test]
    fn output_state_schema_rejects_legacy_aliases_and_unknown_fields() {
        let legacy = serde_json::json!({
            "schema": 7,
            "physical_width": 1600,
            "physical_height": 1000,
            "host_surface_scale_120": 150,
            "scale_120": 150,
            "logical_width": 1280,
            "logical_height": 800,
            "geometry_generation": 1,
        });
        assert!(serde_json::from_value::<BuzzardOSOutputState>(legacy).is_err());

        let mut exact = serde_json::to_value(output_state(1600, 1000, 150, 150, 1)).unwrap();
        exact["host_viewport_width"] = serde_json::json!(1280);
        assert!(serde_json::from_value::<BuzzardOSOutputState>(exact).is_err());
    }

    #[test]
    fn same_sized_new_generation_is_stale() {
        let before = output_state(1600, 1000, 150, 150, 7);
        let after = output_state(1600, 1000, 150, 150, 8);
        let error = require_same_output_generation(Some(before), Some(after)).unwrap_err();
        assert!(error.to_string().contains("stale_output_geometry"));
        assert!(error.to_string().contains("generation 7 to 8"));
    }

    #[test]
    fn foreign_toplevel_state_array_replaces_the_complete_state_set() {
        let mut toplevel = Toplevel::default();
        let bytes = [0_u32, 2_u32]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect::<Vec<_>>();
        apply_toplevel_state_array(&mut toplevel, &bytes);
        assert!(toplevel.maximized);
        assert!(toplevel.activated);
        assert!(!toplevel.minimized);
        assert!(!toplevel.fullscreen);

        let bytes = [1_u32]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect::<Vec<_>>();
        apply_toplevel_state_array(&mut toplevel, &bytes);
        assert!(toplevel.minimized);
        assert!(!toplevel.maximized);
        assert!(!toplevel.activated);
    }

    #[test]
    fn minimized_protocol_state_overrides_stale_visible_geometry() {
        let mut toplevel = Toplevel::default();
        assert!(foreign_toplevel_is_on_screen(&toplevel, Some(true)));
        toplevel.minimized = true;
        assert!(!foreign_toplevel_is_on_screen(&toplevel, Some(true)));
        toplevel.minimized = false;
        assert!(!foreign_toplevel_is_on_screen(&toplevel, Some(false)));
    }

    #[test]
    fn exact_process_name_matches_desktop_style_app_id() {
        let process = crate::proc_fs::ProcessInfo {
            pid: 100,
            name: "foot".into(),
            cmdline: "/usr/bin/foot".into(),
        };
        assert!(process_name_matches_app_id(&process, "foot"));
        assert!(process_name_matches_app_id(
            &process,
            "org.codeberg.dnkl.foot.desktop"
        ));
        assert!(!process_name_matches_app_id(&process, "footclient"));
    }

    #[test]
    fn persistent_foreign_toplevel_ids_do_not_alias_after_close() {
        let mut state = State::default();
        let first = stable_toplevel_id(&mut state, 0xff00_0000);
        assert_eq!(stable_toplevel_id(&mut state, 0xff00_0000), first);
        state.toplevels.entry(0xff00_0000).or_default().closed = true;
        let second = stable_toplevel_id(&mut state, 0xff00_0001);
        assert_ne!(first, second);
        assert!(second > first);
    }

    #[test]
    fn window_control_confirmation_matches_the_requested_state() {
        let normal = WindowControlState {
            present: true,
            ..WindowControlState::default()
        };
        assert!(window_control_satisfied(
            WindowControlAction::Close,
            WindowControlState::default()
        ));
        assert!(window_control_satisfied(
            WindowControlAction::Minimize,
            WindowControlState {
                minimized: true,
                ..normal
            }
        ));
        assert!(window_control_satisfied(
            WindowControlAction::Maximize,
            WindowControlState {
                maximized: true,
                ..normal
            }
        ));
        assert!(window_control_satisfied(
            WindowControlAction::Restore,
            normal
        ));
    }

    fn window(xid: u64, pid: Option<u32>, title: &str) -> WindowInfo {
        WindowInfo {
            xid,
            pid,
            app_name: String::new(),
            title: title.to_owned(),
            is_on_screen: true,
            z_index: None,
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        }
    }

    #[test]
    fn atspi_merge_keeps_x11_geometry_owner_and_native_only_frames() {
        let mut windows = vec![window(10, Some(100), "XWayland")];
        let x11_pids = std::collections::HashSet::from([100]);
        merge_atspi_windows(
            &mut windows,
            &x11_pids,
            vec![
                window(100 << 16, Some(100), "XWayland duplicate"),
                window(200 << 16, Some(200), "Native Wayland"),
                window(1, None, "Unknown native frame"),
            ],
        );
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].xid, 10);
        assert_eq!(windows[1].pid, Some(200));
        assert_eq!(windows[2].pid, None);
    }

    #[test]
    fn native_window_suppresses_same_process_atspi_internal_roots() {
        let mut native = window(10, Some(100), "Browser [browser]");
        native.app_name = "browser".into();
        let browser_frame = window(100 << 16, Some(100), "Browser");
        let browser_internal = window((100 << 16) + 1, Some(100), "Restore pages?");
        let shell = window(200 << 16, Some(200), "Desktop shell");

        let enriched = enrich_native_windows(
            vec![native],
            vec![browser_frame, browser_internal, shell],
            false,
        );

        assert_eq!(enriched.len(), 2);
        assert_eq!(enriched[0].pid, Some(100));
        assert_eq!(enriched[1].pid, Some(200));
    }

    #[test]
    fn zero_geometry_does_not_replace_a_real_observed_origin() {
        let pid = u32::MAX - 17;
        let mut observed = window(1, Some(pid), "Observed");
        observed.x = 120;
        observed.y = 80;
        remember_observed_window_origins(&[observed]);
        assert_eq!(observed_window_origin(pid), Some((120, 80)));

        remember_observed_window_origins(&[window(2, Some(pid), "Unknown")]);
        assert_eq!(observed_window_origin(pid), Some((120, 80)));
    }

    #[test]
    fn native_enrichment_matches_plain_atspi_title() {
        let mut native = window(42, None, "CUA Fixture [cua-fixture]");
        native.app_name = "cua-fixture".into();
        let mut accessible = window(123 << 16, Some(123), "CUA Fixture");
        accessible.x = 20;
        accessible.y = 30;
        accessible.width = 800;
        accessible.height = 600;

        let enriched = enrich_native_windows(vec![native], vec![accessible], false);

        assert_eq!(enriched[0].xid, 42);
        assert_eq!(enriched[0].pid, Some(123));
        assert_eq!((enriched[0].x, enriched[0].y), (20, 30));
        assert_eq!((enriched[0].width, enriched[0].height), (800, 600));
    }

    #[test]
    fn native_enrichment_keeps_unrepresented_accessible_layer_shell() {
        let native = window(42, Some(200), "Terminal");
        let terminal = window(200 << 16, Some(200), "Terminal");
        let shell = window(78 << 16, Some(78), "buzzardos-shell");

        let enriched = enrich_native_windows(vec![native], vec![terminal, shell], false);

        assert_eq!(enriched.len(), 2);
        assert_eq!(enriched[0].pid, Some(200));
        assert_eq!(enriched[1].pid, Some(78));
        assert_eq!(enriched[1].title, "buzzardos-shell");
    }

    #[test]
    fn unmatched_ext_windows_do_not_satisfy_pid_filter() {
        let windows = vec![window(0xF000_0000, None, "Protocol-only")];
        assert!(native_windows_for_pid(windows, 4242).is_none());
    }

    #[test]
    fn native_title_match_recovers_pid_without_replacing_native_id() {
        let native = vec![window(77, None, "CuaTestHarness")];
        let mut accessible = window(123 << 16, Some(123), "CuaTestHarness");
        accessible.x = 20;
        accessible.y = 30;
        accessible.width = 800;
        accessible.height = 600;
        let enriched = enrich_native_windows(native, vec![accessible], false);
        assert_eq!(enriched[0].xid, 77);
        assert_eq!(enriched[0].pid, Some(123));
        assert_eq!(
            (
                enriched[0].x,
                enriched[0].y,
                enriched[0].width,
                enriched[0].height
            ),
            (20, 30, 800, 600)
        );
    }

    #[test]
    fn atspi_enrichment_cannot_resurrect_a_compositor_hidden_window() {
        let mut native = window(77, None, "CuaTestHarness");
        native.is_on_screen = false;
        let accessible = window(123 << 16, Some(123), "CuaTestHarness");
        let enriched = enrich_native_windows(vec![native], vec![accessible], false);
        assert!(!enriched[0].is_on_screen);
    }

    #[test]
    fn nested_enrichment_adopts_stable_atspi_id() {
        let native = vec![window(77, None, "CuaTestHarness")];
        let accessible = window(123 << 16, Some(123), "CuaTestHarness");
        let enriched = enrich_native_windows(native, vec![accessible], true);
        assert_eq!(enriched[0].xid, 123 << 16);
        assert_eq!(enriched[0].pid, Some(123));
        assert_eq!(
            identity_for(enriched[0].xid).unwrap().title,
            "CuaTestHarness"
        );
    }

    #[test]
    fn inject_target_correlates_native_identity_to_unique_atspi_pid() {
        let identity = ToplevelIdentity {
            title: "Unique sentinel".into(),
            app_id: "electron".into(),
        };
        let windows = vec![
            window(10, Some(100), "Background fixture"),
            window(20, Some(200), "Unique sentinel"),
        ];
        assert_eq!(
            unique_atspi_pid_for_identity(&identity, &windows),
            Some(200)
        );
    }

    #[test]
    fn inject_target_prefers_explicit_positive_pid() {
        assert_eq!(
            inject_target_for_window_with_pid(99, Some(123)).unwrap(),
            "root:123"
        );
        assert!(inject_target_for_window_with_pid(99, Some(0)).is_err());
    }

    #[test]
    fn inject_target_refuses_ambiguous_title_pid_correlation() {
        let identity = ToplevelIdentity {
            title: "Shared title".into(),
            app_id: "electron".into(),
        };
        let windows = vec![
            window(10, Some(100), "Shared title"),
            window(20, Some(200), "Shared title"),
        ];
        assert_eq!(unique_atspi_pid_for_identity(&identity, &windows), None);
    }

    #[test]
    fn sway_window_capture_is_cropped_to_compositor_geometry() {
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            8,
            6,
            image::Rgba([20, 40, 60, 255]),
        ));
        let mut encoded = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("encode fixture PNG");
        let cropped =
            crop_png_to_rect(encoded.get_ref(), 2, 1, 3, 4, "fixture").expect("crop fixture PNG");
        let decoded = image::load_from_memory(&cropped).expect("decode cropped PNG");
        assert_eq!((decoded.width(), decoded.height()), (3, 4));
    }

    #[test]
    fn offscreen_window_capture_preserves_window_local_coordinates() {
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            8,
            6,
            image::Rgba([20, 40, 60, 255]),
        ));
        let mut encoded = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("encode fixture PNG");

        let cropped = crop_png_to_rect(encoded.get_ref(), -2, -1, 6, 5, "fixture")
            .expect("pad offscreen window capture");
        let decoded = image::load_from_memory(&cropped)
            .expect("decode padded PNG")
            .to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (6, 5));
        assert_eq!(decoded.get_pixel(0, 0), &image::Rgba([0, 0, 0, 0]));
        assert_eq!(decoded.get_pixel(2, 1), &image::Rgba([20, 40, 60, 255]));
    }

    #[test]
    fn oversized_window_capture_fails_before_allocation() {
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 6));
        let mut encoded = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("encode fixture PNG");
        let error = crop_png_to_rect(encoded.get_ref(), 0, 0, 8193, 1, "fixture")
            .expect_err("reject oversized geometry");
        assert!(error.to_string().contains("exceeds the safety limit"));
    }

    #[test]
    fn fractional_capture_preserves_native_physical_pixels_byte_for_byte() {
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            15,
            9,
            image::Rgba([20, 40, 60, 255]),
        ));
        let mut encoded = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("encode fixture PNG");
        let state = output_state(15, 9, 160, 180, 7);
        let original = encoded.into_inner();
        let captured = normalize_capture_for_state(original.clone(), state)
            .expect("accept native physical screenshot");
        let decoded = image::load_from_memory(&captured).expect("decode physical screenshot");
        assert_eq!((decoded.width(), decoded.height()), (15, 9));
        assert_eq!(captured, original);
    }

    #[test]
    fn mismatched_capture_fails_as_stale_output_geometry() {
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::new(11, 7));
        let mut encoded = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("encode fixture PNG");
        let error =
            normalize_capture_for_state(encoded.into_inner(), output_state(15, 9, 160, 180, 7))
                .unwrap_err();
        assert!(error.to_string().contains("stale_output_geometry"));
    }

    #[test]
    fn capture_rejects_generation_change_even_when_pixels_keep_the_same_extent() {
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::new(15, 9));
        let mut encoded = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Png)
            .expect("encode fixture PNG");
        let before = output_state(15, 9, 160, 180, 91);
        let after = output_state(15, 9, 160, 180, 92);
        let error =
            normalize_capture_for_generation(encoded.into_inner(), Some(before), Some(after))
                .unwrap_err();
        assert!(error.to_string().contains("stale_output_geometry"));
        assert!(error.to_string().contains("generation 91 to 92"));
    }

    #[test]
    fn shell_helper_capture_failure_is_terminal() {
        let result = checked_shell_helper_capture(true, || None)
            .expect("available helper must produce a terminal result");
        assert_eq!(
            result.unwrap_err().to_string(),
            "GNOME compositor helper capture failed"
        );
    }

    #[test]
    fn unavailable_shell_helper_does_not_attempt_capture() {
        let called = std::cell::Cell::new(false);
        let result = checked_shell_helper_capture(false, || {
            called.set(true);
            Some(vec![1, 2, 3])
        });
        assert!(result.is_none());
        assert!(!called.get());
    }

    #[test]
    fn injectable_text_accepts_printable_ascii_newline_and_tab() {
        validate_injectable_text("Hello, World! 123 @#$%\t\n").expect("printable ASCII is typable");
        // The full printable ASCII span the compositor chartab covers.
        let printable: String = (0x20u8..=0x7e).map(|b| b as char).collect();
        validate_injectable_text(&printable).expect("every printable ASCII byte is typable");
    }

    #[test]
    fn injectable_text_rejects_unicode_and_other_controls() {
        for bad in [
            "café",
            "emoji 😀",
            "bell\u{07}",
            "null\u{00}",
            "delete\u{7f}",
            "cr\r",
        ] {
            assert!(
                validate_injectable_text(bad).is_err(),
                "{bad:?} must be rejected before it reaches the compositor"
            );
        }
    }

    #[test]
    fn injectable_key_accepts_whitelist_case_insensitively() {
        for good in [
            "enter", "Enter", "RETURN", "tab", "Escape", "esc", "space", "up", "Left", "f1", "F12",
        ] {
            validate_injectable_key(good).unwrap_or_else(|e| panic!("{good:?} should pass: {e}"));
        }
    }

    #[test]
    fn injectable_key_rejects_unsupported_names() {
        for bad in ["f13", "ctrl", "a", "delete", "home", "pageup", ""] {
            assert!(
                validate_injectable_key(bad).is_err(),
                "{bad:?} is not in the compositor whitelist"
            );
        }
    }

    #[test]
    fn injectable_hotkey_normalizes_supported_chords() {
        let keys = vec!["control".to_owned(), "SHIFT".to_owned(), "7".to_owned()];
        assert_eq!(
            validate_injectable_hotkey(&keys).expect("supported chord"),
            ("ctrl,shift".to_owned(), "7".to_owned())
        );
        assert!(validate_injectable_hotkey(&["7".to_owned()]).is_err());
        assert!(validate_injectable_hotkey(&["hyper".to_owned(), "k".to_owned()]).is_err());
    }

    #[test]
    fn hello_reply_parses_exact_banner_and_rejects_mismatch() {
        parse_inject_hello("cua-inject v1\n").expect("verbatim banner is accepted");
        parse_inject_hello("  cua-inject v1  ").expect("surrounding whitespace is tolerated");
        assert!(parse_inject_hello("cua-inject v2").is_err());
        assert!(parse_inject_hello("err unsupported-version").is_err());
        assert!(parse_inject_hello("garbage").is_err());
    }

    #[test]
    fn command_reply_parses_ok_and_surfaces_error_reason() {
        parse_inject_reply("ok\n").expect("ok is success");
        parse_inject_reply("ok").expect("ok without newline is success");
        let err = parse_inject_reply("err ambiguous-app-id\n").unwrap_err();
        assert!(err.to_string().contains("ambiguous-app-id"));
        assert!(parse_inject_reply("err").is_err());
        assert!(parse_inject_reply("maybe").is_err());
    }

    #[test]
    fn geometry_reply_is_strict_and_signed() {
        assert_eq!(
            parse_inject_geometry("geometry -4 23 10 20\n").unwrap(),
            ((-4, 23), (10, 20))
        );
        assert!(parse_inject_geometry("geometry 1").is_err());
        assert!(parse_inject_geometry("err target-not-found").is_err());
        assert!(parse_inject_geometry("ok").is_err());
    }
}
