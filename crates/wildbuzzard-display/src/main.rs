// SPDX-License-Identifier: AGPL-3.0-or-later

mod drm_syncobj;
mod gateway;
mod guest_display;
mod host_app;
mod launch;

use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};
use clap::Parser;

use crate::gateway::GatewaySockets;
use crate::host_app::HostApplication;
use crate::launch::Launch;

fn main() {
    if let Err(error) = run() {
        eprintln!("wildbuzzard-display: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    reexec_with_display_desktop_identity()?;
    let launch = Launch::parse().validate()?;
    launch.configure_native_backend();
    let (gateway, connection) = GatewaySockets::bind(&launch)?;
    let application = HostApplication::connect(launch, connection)?;

    // Socket ownership is deliberately independent of the native Wayland
    // connection. Dropping `gateway` is the one place that removes the
    // private guest and host-control endpoints.
    application.run(gateway)
}

/// GLib records the PID that was launched from a desktop file. AppRun starts
/// the lifecycle launcher, which then starts this host-window process, so the
/// inherited PID no longer identifies the Wayland client. Re-exec this same
/// process once with its own PID recorded in the initial environment. Keeping
/// the PID across exec also makes `/proc/<pid>/environ` truthful to GNOME
/// Shell, allowing shortcut-inhibition consent to be attributed to Wild
/// Buzzard instead of the parent terminal, file manager, or IDE.
fn reexec_with_display_desktop_identity() -> Result<()> {
    const REEXEC_MARKER: &str = "WILDBUZZARD_DISPLAY_DESKTOP_REEXEC";
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
