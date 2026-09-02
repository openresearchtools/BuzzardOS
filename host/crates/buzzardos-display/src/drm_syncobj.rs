// SPDX-License-Identifier: AGPL-3.0-or-later

//! Minimal, auditable libdrm bindings for DRM syncobj timeline hand-off.
//!
//! The Wayland syncobj protocol transfers an opaque timeline FD. Importing it
//! into the explicitly selected render node gives this process only the
//! ability to wait for and signal points on that timeline; it does not expose
//! another Wayland connection or any host desktop capability to the guest.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};

const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 1 << 1;
const ACQUIRE_TIMEOUT_NS: i64 = 2_000_000_000;

#[link(name = "drm")]
unsafe extern "C" {
    fn drmSyncobjDestroy(fd: libc::c_int, handle: u32) -> libc::c_int;
    fn drmSyncobjFDToHandle(fd: libc::c_int, obj_fd: libc::c_int, handle: *mut u32) -> libc::c_int;
    fn drmSyncobjTimelineWait(
        fd: libc::c_int,
        handles: *mut u32,
        points: *mut u64,
        num_handles: libc::c_uint,
        timeout_nsec: i64,
        flags: libc::c_uint,
        first_signaled: *mut u32,
    ) -> libc::c_int;
    fn drmSyncobjTimelineSignal(
        fd: libc::c_int,
        handles: *const u32,
        points: *mut u64,
        num_handles: u32,
    ) -> libc::c_int;
}

#[derive(Debug)]
pub(crate) struct SyncobjDevice {
    file: File,
    dev_t: u64,
}

impl SyncobjDevice {
    pub(crate) fn open(path: &Path) -> Result<Arc<Self>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
            .with_context(|| format!("opening syncobj render node {}", path.display()))?;
        let dev_t = file
            .metadata()
            .with_context(|| format!("inspecting syncobj render node {}", path.display()))?
            .rdev();
        Ok(Arc::new(Self { file, dev_t }))
    }

    pub(crate) fn dev_t(&self) -> u64 {
        self.dev_t
    }

    pub(crate) fn import_timeline(self: &Arc<Self>, fd: OwnedFd) -> Result<SyncobjTimeline> {
        let mut handle = 0_u32;
        // SAFETY: both descriptors are live and `handle` is writable.
        let result = unsafe {
            drmSyncobjFDToHandle(
                self.file.as_raw_fd(),
                fd.as_raw_fd(),
                &mut handle as *mut u32,
            )
        };
        libdrm_result(result, "importing DRM syncobj timeline")?;
        if handle == 0 {
            bail!("libdrm imported a zero DRM syncobj handle");
        }
        Ok(SyncobjTimeline(Arc::new(SyncobjTimelineInner {
            device: Arc::clone(self),
            handle,
        })))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SyncobjTimeline(Arc<SyncobjTimelineInner>);

impl SyncobjTimeline {
    pub(crate) fn same_timeline(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub(crate) fn wait(&self, point: u64) -> Result<u64> {
        let started = Instant::now();
        let mut handle = self.0.handle;
        let mut point = point;
        let mut first_signaled = 0_u32;
        let deadline = monotonic_ns().saturating_add(ACQUIRE_TIMEOUT_NS);
        // SAFETY: all pointers reference one initialized value for the
        // duration of this synchronous ioctl wrapper.
        let result = unsafe {
            drmSyncobjTimelineWait(
                self.0.device.file.as_raw_fd(),
                &mut handle,
                &mut point,
                1,
                deadline,
                DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
                &mut first_signaled,
            )
        };
        libdrm_result(result, "waiting for guest acquire timeline point")?;
        Ok(started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64)
    }

    pub(crate) fn signal(&self, point: u64) -> Result<()> {
        let handle = self.0.handle;
        let mut point = point;
        // SAFETY: the arrays each contain exactly one initialized value.
        let result = unsafe {
            drmSyncobjTimelineSignal(self.0.device.file.as_raw_fd(), &handle, &mut point, 1)
        };
        libdrm_result(result, "signaling guest release timeline point")
    }
}

#[derive(Debug)]
struct SyncobjTimelineInner {
    device: Arc<SyncobjDevice>,
    handle: u32,
}

impl Drop for SyncobjTimelineInner {
    fn drop(&mut self) {
        // SAFETY: `handle` was imported on this still-live DRM file.
        let result = unsafe { drmSyncobjDestroy(self.device.file.as_raw_fd(), self.handle) };
        if result != 0 {
            eprintln!(
                "buzzardos-display: destroying DRM syncobj handle failed: {}",
                libdrm_error(result)
            );
        }
    }
}

fn libdrm_result(result: libc::c_int, operation: &str) -> Result<()> {
    if result == 0 {
        return Ok(());
    }
    bail!("{operation}: {}", libdrm_error(result))
}

fn libdrm_error(result: libc::c_int) -> std::io::Error {
    let errno = if result < 0 { -result } else { result };
    std::io::Error::from_raw_os_error(errno)
}

fn monotonic_ns() -> i64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a valid writable timespec.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time);
    }
    time.tv_sec
        .saturating_mul(1_000_000_000)
        .saturating_add(time.tv_nsec)
}
