// SPDX-License-Identifier: AGPL-3.0-or-later

mod clipboard;
mod drm_syncobj;
mod frame_paintable;
mod gateway;
mod guest_display;
mod host_app;
mod host_theme;
mod keyboard;
mod launch;
mod machine_manager;

use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};
use clap::Parser;

use crate::gateway::GatewaySockets;
use crate::host_app::HostApplication;
use crate::launch::Launch;

fn main() {
    if let Err(error) = run() {
        eprintln!("buzzardos-display: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    if let Some(result) = clipboard::maybe_run_image_worker() {
        return result;
    }
    configure_gtk_portal_policy();
    reexec_with_display_desktop_identity()?;
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--machine-manager")) {
        return machine_manager::run_from_args();
    }
    let launch = Launch::parse().validate()?;
    launch.configure_native_backend();
    let (gateway, connection) = GatewaySockets::bind(&launch)?;
    let application = HostApplication::connect(launch, connection)?;

    // Socket ownership is deliberately independent of the native Wayland
    // connection. Dropping `gateway` is the one place that removes the
    // private guest and host-control endpoints.
    application.run(gateway)
}

/// Buzzard OS does not use host file chooser, screenshot, or inhibit portals.
/// Apply GTK's documented no-portals policy before GTK initialization and
/// before any display or manager worker threads can exist. Preserve any
/// independently configured GDK diagnostics instead of replacing them.
fn configure_gtk_portal_policy() {
    let mut flags = std::env::var("GDK_DEBUG").unwrap_or_default();
    if flags
        .split(',')
        .any(|flag| flag.trim().eq_ignore_ascii_case("no-portals"))
    {
        return;
    }
    if !flags.is_empty() {
        flags.push(',');
    }
    flags.push_str("no-portals");
    // SAFETY: `run` calls this before GTK initialization and before any
    // display, gateway, or machine-manager worker thread can be created.
    unsafe { std::env::set_var("GDK_DEBUG", flags) };
}

/// GLib records the PID that was launched from a desktop file. The manager
/// starts this host-window process, so the inherited PID no longer identifies
/// the Wayland client. Re-exec this same
/// process once with its own PID recorded in the initial environment. Keeping
/// the PID across exec also makes `/proc/<pid>/environ` truthful to GNOME
/// Shell, allowing shortcut-inhibition consent to be attributed to Buzzard OS
/// instead of the parent terminal, file manager, or IDE.
fn reexec_with_display_desktop_identity() -> Result<()> {
    const REEXEC_MARKER: &str = "BUZZARDOS_DISPLAY_DESKTOP_REEXEC";
    if std::env::var_os("GIO_LAUNCHED_DESKTOP_FILE").is_none()
        || std::env::var_os(REEXEC_MARKER).is_some()
    {
        return Ok(());
    }
    let current_pid = std::process::id().to_string();
    if std::env::var("GIO_LAUNCHED_DESKTOP_FILE_PID").is_ok_and(|value| value == current_pid) {
        return Ok(());
    }
    let executable = std::env::current_exe().context("resolving display executable for re-exec")?;
    let error = std::process::Command::new(executable)
        .args(std::env::args_os().skip(1))
        .env("GIO_LAUNCHED_DESKTOP_FILE_PID", current_pid)
        .env(REEXEC_MARKER, "1")
        .exec();
    Err(error).context("re-executing display with its desktop-file identity")
}
