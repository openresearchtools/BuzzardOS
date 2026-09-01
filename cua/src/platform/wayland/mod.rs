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
pub mod persistent_vptr;
pub mod sway_ipc;
mod virtual_keyboard;
pub(crate) use virtual_keyboard::Admission as KeyboardAdmission;

/// Initialize keyboard shutdown handling before any input tool is admitted.
/// This is separate from starting the lazy Wayland owner.
pub fn initialize_keyboard_cancellation() {
    virtual_keyboard::initialize();
}

pub fn keyboard_admission() -> anyhow::Result<KeyboardAdmission> {
    virtual_keyboard::admit()
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

use crate::platform::x11::WindowInfo;

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
    // Sway uses one global logical coordinate space for every virtual output,
    // whereas the selected wl_output's screencopy and virtual-pointer
    // protocols start at (0,0). Translate to that output before scaling.
    let origin = sway_ipc::caller_output_origin().unwrap_or((0, 0));
    logical_rect_to_canonical_for_state_at_origin(state, x, y, width, height, origin)
}

fn logical_rect_to_canonical_for_state_at_origin(
    state: Option<BuzzardOSOutputState>,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    (origin_x, origin_y): (i32, i32),
) -> (i32, i32, u32, u32) {
    let x = x.saturating_sub(origin_x);
    let y = y.saturating_sub(origin_y);
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
    let logical = match state {
        Some(state) => {
            let dimensions = state.guest_logical_dimensions();
            scaled_rect(
                x,
                y,
                width,
                height,
                dimensions.0,
                dimensions.1,
                state.physical_width,
                state.physical_height,
                false,
            )
        }
        None => (x, y, width, height),
    };
    let (origin_x, origin_y) = sway_ipc::caller_output_origin().unwrap_or((0, 0));
    (
        logical.0.saturating_add(origin_x),
        logical.1.saturating_add(origin_y),
        logical.2,
        logical.3,
    )
}

/// Buzzard CUA has one backend: the private stock-Sway Wayland session.
pub fn wayland_enabled() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// True when the private Sway Wayland socket is available.
pub fn is_wayland() -> bool {
    wayland_enabled() && std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Input-specific alias retained to make dispatch intent explicit.
pub fn wayland_input_enabled() -> bool {
    is_wayland()
}

#[derive(Default)]
struct Toplevel {
    title: String,
    app_id: String,
    outputs: HashSet<u32>,
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
            crate::platform::atspi::list_windows(None)
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
    // IDs in this namespace were minted by the invocation-owned
    // foreign-toplevel connection. A missing one is stale or unknown and must
    // never fall through to title/app-id guessing, especially when several
    // windows share the same title.
    if id >= STABLE_TOPLEVEL_ID_BASE {
        return None;
    }
    if let Ok(window) = sway_ipc::resolve_public_window(id, None) {
        let output_ids = state
            .outputs
            .iter()
            .filter_map(|(output, name, _, _)| {
                (name == &window.output).then_some(output.id().protocol_id())
            })
            .collect::<HashSet<_>>();
        let mut candidates = state
            .toplevels
            .iter()
            .filter_map(|(protocol_id, toplevel)| {
                (!toplevel.closed
                    && !output_ids.is_disjoint(&toplevel.outputs)
                    && (window.title.is_empty() || toplevel.title == window.title)
                    && (window.app_id.is_empty() || toplevel.app_id == window.app_id))
                    .then(|| state.handles.get(protocol_id).cloned())
                    .flatten()
            });
        let one = candidates.next()?;
        return candidates.next().is_none().then_some(one);
    }
    if let Some(identity) = identity_for(id) {
        let mut candidates = state
            .toplevels
            .iter()
            .filter_map(|(protocol_id, toplevel)| {
                (!toplevel.closed
                    && (identity.title.is_empty() || toplevel.title == identity.title)
                    && (identity.app_id.is_empty() || toplevel.app_id == identity.app_id))
                    .then(|| state.handles.get(protocol_id).cloned())
                    .flatten()
            });
        let one = candidates.next()?;
        return candidates.next().is_none().then_some(one);
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
    seats: Vec<(WlSeat, String)>,
    // Virtual-pointer manager + output dimensions, so `click` can land a real
    // button press at the output centre (over the just-activated window).
    vptr_manager: Option<ZwlrVirtualPointerManagerV1>,
    output: Option<WlOutput>,
    outputs: Vec<(WlOutput, String, u32, u32)>,
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
                let seat = registry.bind::<WlSeat, _, _>(name, v, qh, ());
                state.seats.push((seat, String::new()));
            } else if interface == ZwlrVirtualPointerManagerV1::interface().name {
                state.vptr_manager = Some(registry.bind::<ZwlrVirtualPointerManagerV1, _, _>(
                    name,
                    version.min(2),
                    qh,
                    (),
                ));
            } else if interface == WlOutput::interface().name {
                let out = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, ());
                state.outputs.push((out, String::new(), 0, 0));
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
        state: &mut Self,
        seat: &WlSeat,
        event: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_seat::Event::Name { name } = event {
            if let Some(candidate) = state
                .seats
                .iter_mut()
                .find(|candidate| candidate.0 == *seat)
            {
                candidate.1 = name.clone();
            }
            if std::env::var(crate::core::seat_context::CUA_SEAT_ENV)
                .ok()
                .as_deref()
                == Some(name.as_str())
            {
                state.seat = Some(seat.clone());
            }
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_output::Event::Mode { width, height, .. } => {
                if let Some(candidate) = state
                    .outputs
                    .iter_mut()
                    .find(|candidate| candidate.0 == *output)
                {
                    candidate.2 = width.max(0) as u32;
                    candidate.3 = height.max(0) as u32;
                    if state.output.as_ref() == Some(output) {
                        state.output_w = candidate.2;
                        state.output_h = candidate.3;
                    }
                }
            }
            wl_output::Event::Name { name } => {
                if let Some(candidate) = state
                    .outputs
                    .iter_mut()
                    .find(|candidate| candidate.0 == *output)
                {
                    candidate.1 = name.clone();
                    if std::env::var(crate::core::seat_context::CUA_OUTPUT_ENV)
                        .ok()
                        .as_deref()
                        == Some(name.as_str())
                    {
                        state.output = Some(output.clone());
                        state.output_w = candidate.2;
                        state.output_h = candidate.3;
                    }
                }
            }
            _ => {}
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
            ftl_handle::Event::OutputEnter { output } => {
                tl.outputs.insert(output.id().protocol_id());
            }
            ftl_handle::Event::OutputLeave { output } => {
                tl.outputs.remove(&output.id().protocol_id());
            }
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
                let _ = foreign_toplevel_worker(receiver);
            })
            .expect("spawning foreign-toplevel worker");
        sender
    })
}

/// Enumerate native Wayland toplevels through one invocation-owned connection.
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
        Err(_) => {
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
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
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
    // A daemonless command's virtual-pointer object disappears when that
    // command exits. Recreate this numbered seat's native Sway pointer at its
    // bounded last position and keep the connection alive for the capture so
    // overlay_cursor=1 contains a normal compositor cursor, not a custom
    // layer-shell surface.
    let _native_cursor = open_native_cursor_for_capture()?;
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

    // Include Sway's native cursor for this invocation's numbered seat. The
    // output is private to that seat, so no human or other CUA cursor can be
    // composited into this screenshot.
    let frame = manager.capture_output(1, &output, &qh, ());
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
        crate::core::image_utils::encode_rgba_to_png(&rgba, w, h)
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
        crate::platform::capture::screenshot_window_bytes(xid)
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

/// Capture the private Sway output through wlroots screencopy.
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
    if !is_wayland() {
        anyhow::bail!("Buzzard CUA requires the private Sway WAYLAND_DISPLAY");
    }
    screenshot_bytes()
}

/// Per-window capture dispatcher. On X11 forwards to the existing window
/// capture path; on pure Wayland returns a typed error pointing at the
/// staging `ext-image-copy-capture-v1` protocol — wlr-screencopy is
/// output-only, and `foreign-toplevel` exposes no per-window geometry to
/// crop with.
pub fn screenshot_window_dispatch(xid: u64) -> anyhow::Result<Vec<u8>> {
    if is_wayland() {
        if sway_ipc::is_public_window_id(xid) {
            sway_ipc::move_public_window_to_cua(
                xid,
                None,
                crate::core::seat_context::current_index(),
            )?;
        }
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
    crate::platform::capture::screenshot_window_bytes(xid)
}

// ── Input session helper ─────────────────────────────────────────────────────

pub const NO_VPTR_MARKER: &str = "no-zwlr-virtual-pointer";

/// Live virtual-pointer session: connection + queue + the bound objects every
/// pointer op (click, scroll, drag) needs. Returned by [`open_vptr_session`].
pub struct VptrSession {
    pub conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
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
    // Public wlroots ids are scoped to the invocation-owned
    // foreign-toplevel connection. Activate them there; a fresh virtual
    // pointer connection cannot resolve those ids safely.
    let local_activate_window_id = match activate_window_id {
        Some(id) if sway_ipc::is_public_window_id(id) => {
            activate_window_for_input_target(id, None)?;
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

    let mgr = state.vptr_manager.clone().ok_or_else(|| {
        anyhow::anyhow!("Sway does not expose zwlr_virtual_pointer_manager_v1 ({NO_VPTR_MARKER})")
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
        std::thread::sleep(std::time::Duration::from_millis(60));
        if sway_ipc::is_public_window_id(id) {
            sway_ipc::require_caller_seat_focus(Some(id))?;
        }
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
        vptr,
        output_w,
        output_h,
    })
}

fn open_native_cursor_for_capture() -> anyhow::Result<VptrSession> {
    let mut session = open_vptr_session(None)?;
    let (width, height) = (session.output_w, session.output_h);
    let default = ((width / 2) as i32, (height / 2) as i32);
    let (x, y) = last_synth_cursor_pos().unwrap_or(default);
    let x = x.clamp(0, width.saturating_sub(1) as i32) as u32;
    let y = y.clamp(0, height.saturating_sub(1) as i32) as u32;
    session
        .vptr
        .motion_absolute(event_time_ms(), x, y, width, height);
    session.vptr.frame();
    session.queue.roundtrip(&mut session.state)?;
    record_synth_cursor(x as i32, y as i32);
    Ok(session)
}

/// Focus and raise a specific native Wayland toplevel before focus-bound
/// keyboard or portal/libei input. wlroots exposes an activation request on its
/// foreign-toplevel protocol; GNOME uses the bundled compositor helper. Other
/// compositors must refuse until they provide an equally target-addressable
/// adapter, because global injection without this gate can affect the wrong app.
pub fn activate_window_for_input(window_id: u64) -> anyhow::Result<()> {
    let pid = crate::platform::atspi::list_windows(None)
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
    let public_window = if sway_ipc::is_public_window_id(window_id) {
        let window = sway_ipc::resolve_public_window(window_id, target_pid)?;
        remember_identity(
            window_id,
            &Toplevel {
                title: window.title.clone(),
                app_id: window.app_id.clone(),
                ..Toplevel::default()
            },
        );
        Some(window)
    } else {
        None
    };

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
        if public_window.is_some() {
            // Disambiguate duplicate titles/app-ids on their source outputs,
            // confirm the exact seat focus there, then move the exact Sway
            // container. Per-seat focus follows the container across outputs.
            sway_ipc::require_caller_seat_exact_focus(window_id)?;
            sway_ipc::move_public_window_to_cua(
                window_id,
                target_pid,
                crate::core::seat_context::current_index(),
            )?;
            sway_ipc::require_caller_seat_focus(Some(window_id))?;
        }
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
    with_stable_output_generation(|| click_vptr(Some(window_id), x, y, count, button))
}

/// Click a desktop-absolute point without selecting or activating a toplevel.
/// This is the Wayland peer of an XTest root-window click and is used only by
/// the explicit desktop capture scope.
pub fn click_desktop(x: i32, y: i32, count: u32, button: u8) -> anyhow::Result<()> {
    with_stable_output_generation(|| click_vptr(None, x, y, count, button))
}

/// wlroots virtual-pointer implementation of [`click`].
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
    with_stable_output_generation(|| scroll_vptr(Some(window_id), point, &direction, amount))
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
        let time = event_time_ms();
        sess.vptr.axis_discrete(time, axis, value, sign);
        sess.vptr.axis_stop(time, axis);
        sess.vptr.frame();
        sess.queue.roundtrip(&mut sess.state)?;
    }
    sess.vptr.destroy();
    sess.queue.roundtrip(&mut sess.state)?;
    Ok(())
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct SyntheticCursorState {
    x: i32,
    y: i32,
    geometry_generation: u64,
}

fn record_synth_cursor(x: i32, y: i32) {
    let state = SyntheticCursorState {
        x,
        y,
        geometry_generation: buzzardos_output_state()
            .map(|state| state.geometry_generation)
            .unwrap_or(0),
    };
    let result = serde_json::to_vec(&state)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| crate::core::seat_context::write_state("cursor-position", &bytes, 256));
    let _ = result;
}

/// Returns the last `(x, y)` this numbered CUA seat warped to via the Wayland
/// virtual-pointer protocol. The latest point is a bounded, mode-0600 record
/// in the user's RAM-backed XDG runtime directory, so independent daemonless
/// CLI invocations share it without a service, unbounded history, or durable
/// telemetry. A geometry-generation mismatch invalidates it after resize.
pub fn last_synth_cursor_pos() -> Option<(i32, i32)> {
    let bytes = crate::core::seat_context::read_state("cursor-position", 256)
        .ok()
        .flatten()?;
    let state: SyntheticCursorState = serde_json::from_slice(&bytes).ok()?;
    let output = buzzardos_output_state();
    if output.is_some_and(|output| {
        state.geometry_generation != output.geometry_generation
            || state.x < 0
            || state.y < 0
            || state.x >= output.physical_width as i32
            || state.y >= output.physical_height as i32
    }) {
        return None;
    }
    Some((state.x, state.y))
}

/// Warp the cursor to absolute output coordinates `(x, y)` using
/// `zwlr_virtual_pointer_v1::motion_absolute`. Clamps to the output bounds
/// reported by `open_vptr_session`. Emits a motion + frame and roundtrips so
/// the compositor commits the warp before returning. Records the position in
/// the synthetic-cursor registry so `last_synth_cursor_pos` can report it.
pub fn move_cursor_absolute(window_id: Option<u64>, x: i32, y: i32) -> anyhow::Result<()> {
    with_stable_output_generation(|| move_cursor_absolute_vptr(window_id, x, y))
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

/// Type Unicode text into the focused Wayland surface through the invocation-owned
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
    sway_ipc::require_caller_seat_focus(None)?;
    virtual_keyboard::type_text(admission, text, VIRTUAL_KEYBOARD_TEXT_DELAY_MS)
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
/// invocation-owned virtual keyboard.
pub fn press_key(admission: &KeyboardAdmission, window_id: u64, key: &str) -> anyhow::Result<()> {
    activate_window_for_input(window_id)?;
    virtual_keyboard::press_key(admission, key)
}

/// Press one key while an outer exact-container focus guard is active.
pub fn press_key_focused(admission: &KeyboardAdmission, key: &str) -> anyhow::Result<()> {
    sway_ipc::require_caller_seat_focus(None)?;
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
    sway_ipc::require_caller_seat_focus(None)?;
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

/// Resolve a window identifier to its compositor application identifier.
pub fn app_id_for_window(window_id: u64) -> Option<String> {
    identity_for(window_id)
        .map(|identity| identity.app_id)
        .filter(|app_id| !app_id.is_empty())
}

/// Return the X11 WM_CLASS pair, or the equivalent Sway application id twice
/// for a native Wayland toplevel.
pub fn wm_class_dispatch(window_id: u64) -> Option<(String, String)> {
    if is_wayland() {
        let app_id = app_id_for_window(window_id)?;
        return Some((app_id.clone(), app_id));
    }
    crate::platform::x11::wm_class_for_window(window_id)
}

fn wayland_atspi_windows(filter_pid: Option<u32>) -> Vec<WindowInfo> {
    let mut windows = crate::platform::atspi::list_windows(filter_pid);
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
    // A toolkit may register on AT-SPI without mapping a toplevel (portals are
    // a common example). Keep that synthetic handle for an explicit by-PID
    // accessibility request, but never advertise it as an open window in the
    // global window list.
    if filter_pid.is_none() {
        windows.retain(|window| window.width > 0 && window.height > 0);
    }
    windows
}

/// Window-enumeration dispatcher: native Wayland when available, else X11.
fn list_windows_dispatch_logical(filter_pid: Option<u32>) -> Vec<WindowInfo> {
    if wayland_enabled() && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let native = match list_windows() {
            Ok(ws) if !ws.is_empty() => {
                Ok(enrich_native_windows(ws, wayland_atspi_windows(filter_pid)))
            }
            Ok(_) => Ok(Vec::new()),
            Err(error) => Err(error),
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
                let ws = wayland_atspi_windows(filter_pid);
                if !ws.is_empty() {
                    return ws;
                }
            }
            Err(_) => {
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
    let mut ws = crate::platform::x11::list_windows(filter_pid);
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
        Err(_) => Vec::new(),
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
    let window_origins = sway_ipc::public_window_output_origins().unwrap_or_default();
    let caller_origin = sway_ipc::caller_output_origin().unwrap_or((0, 0));
    for window in &mut windows {
        let origin = window_origins
            .get(&window.xid)
            .copied()
            .unwrap_or(caller_origin);
        (window.x, window.y, window.width, window.height) =
            logical_rect_to_canonical_for_state_at_origin(
                before,
                window.x,
                window.y,
                window.width,
                window.height,
                origin,
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

fn process_name_matches_app_id(
    process: &crate::platform::proc_fs::ProcessInfo,
    app_id: &str,
) -> bool {
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
    let mut matches = crate::platform::proc_fs::list_processes()
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

fn enrich_native_windows(mut native: Vec<WindowInfo>, atspi: Vec<WindowInfo>) -> Vec<WindowInfo> {
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
        if window.width == 0 || window.height == 0 {
            window.x = candidate.x;
            window.y = candidate.y;
            window.width = candidate.width;
            window.height = candidate.height;
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
    Ok(WaylandManagers {
        foreign_toplevel: state.manager.is_some(),
        screencopy: state.scrcopy_manager.is_some(),
        virtual_pointer: state.vptr_manager.is_some(),
        wl_shm: state.shm.is_some(),
    })
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
        let process = crate::platform::proc_fs::ProcessInfo {
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

        let enriched =
            enrich_native_windows(vec![native], vec![browser_frame, browser_internal, shell]);

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

        let enriched = enrich_native_windows(vec![native], vec![accessible]);

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

        let enriched = enrich_native_windows(vec![native], vec![terminal, shell]);

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
        let enriched = enrich_native_windows(native, vec![accessible]);
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
        let enriched = enrich_native_windows(vec![native], vec![accessible]);
        assert!(!enriched[0].is_on_screen);
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
}
