// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.
// Buzzard modifications: AGPL-3.0-or-later

//! Diagnostics for the one supported runtime: Buzzard OS stock Sway.

use crate::core::health_report::{
    CheckData, CheckEntry, HealthCheckProvider, NAME_AX_CAPABILITY, NAME_BINARY_VERSION,
    NAME_PLATFORM_SUPPORTED, NAME_SCREEN_CAPTURE_CAPABILITY, NAME_SESSION_ACTIVE,
};
use async_trait::async_trait;

pub const NAME_SWAY_BACKEND: &str = "sway_backend";

const CHECKS: &[&str] = &[
    NAME_BINARY_VERSION,
    NAME_PLATFORM_SUPPORTED,
    NAME_SESSION_ACTIVE,
    NAME_AX_CAPABILITY,
    NAME_SCREEN_CAPTURE_CAPABILITY,
    NAME_SWAY_BACKEND,
];

pub struct LinuxHealthProvider;

#[async_trait]
impl HealthCheckProvider for LinuxHealthProvider {
    fn platform(&self) -> &'static str {
        "linux"
    }

    fn check_names(&self) -> &'static [&'static str] {
        CHECKS
    }

    async fn run_check(&self, name: &str) -> CheckEntry {
        match name {
            NAME_BINARY_VERSION => {
                CheckEntry::pass(name, format!("Buzzard CUA {}", env!("CARGO_PKG_VERSION")))
            }
            NAME_PLATFORM_SUPPORTED => {
                CheckEntry::pass(name, "Linux/Sway runtime").with_data(CheckData {
                    os_version: Some("Linux".into()),
                    architecture: Some(std::env::consts::ARCH.into()),
                    ..Default::default()
                })
            }
            NAME_SESSION_ACTIVE => check_session(),
            NAME_AX_CAPABILITY => check_accessibility().await,
            NAME_SCREEN_CAPTURE_CAPABILITY => check_capture().await,
            NAME_SWAY_BACKEND => check_sway().await,
            other => CheckEntry::skip(other, "Unknown Buzzard CUA health check"),
        }
    }
}

fn check_session() -> CheckEntry {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("SWAYSOCK").is_some() {
        CheckEntry::pass(NAME_SESSION_ACTIVE, "Private Sway session is active")
    } else {
        CheckEntry::fail(
            NAME_SESSION_ACTIVE,
            "Private Sway session variables are missing",
            "Run cua as the interactive Buzzard OS guest user inside its Sway session",
        )
    }
}

async fn manager_snapshot() -> Result<crate::platform::wayland::WaylandManagers, String> {
    tokio::task::spawn_blocking(crate::platform::wayland::probe_managers)
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

async fn check_sway() -> CheckEntry {
    match manager_snapshot().await {
        Ok(managers)
            if managers.foreign_toplevel
                && managers.screencopy
                && managers.virtual_pointer
                && managers.wl_shm =>
        {
            CheckEntry::pass(NAME_SWAY_BACKEND, "Required Sway/wlroots protocols are available")
        }
        Ok(managers) => CheckEntry::fail(
            NAME_SWAY_BACKEND,
            format!(
                "Missing Sway protocols: foreign_toplevel={}, screencopy={}, virtual_pointer={}, wl_shm={}",
                managers.foreign_toplevel,
                managers.screencopy,
                managers.virtual_pointer,
                managers.wl_shm
            ),
            "Use the stock Sway session supplied by buzzardos-guest",
        ),
        Err(error) => CheckEntry::fail(
            NAME_SWAY_BACKEND,
            format!("Cannot connect to the private Sway socket: {error}"),
            "Run cua inside the active Buzzard OS guest session",
        ),
    }
}

async fn check_capture() -> CheckEntry {
    match manager_snapshot().await {
        Ok(managers) if managers.screencopy && managers.wl_shm => CheckEntry::pass(
            NAME_SCREEN_CAPTURE_CAPABILITY,
            "Sway wlroots screencopy is available",
        ),
        Ok(_) => CheckEntry::fail(
            NAME_SCREEN_CAPTURE_CAPABILITY,
            "Sway wlroots screencopy is unavailable",
            "Use the supported stock Sway/wlroots guest session",
        ),
        Err(error) => CheckEntry::fail(
            NAME_SCREEN_CAPTURE_CAPABILITY,
            format!("Wayland probe failed: {error}"),
            "Run cua inside the active Buzzard OS guest session",
        ),
    }
}

async fn check_accessibility() -> CheckEntry {
    let available = tokio::task::spawn_blocking(probe_a11y_bus)
        .await
        .unwrap_or(false);
    if available {
        CheckEntry::pass(NAME_AX_CAPABILITY, "AT-SPI accessibility bus is available")
    } else {
        CheckEntry::fail(
            NAME_AX_CAPABILITY,
            "AT-SPI accessibility bus is unavailable",
            "Start the guest AT-SPI services from buzzardos-guest",
        )
    }
}

pub(crate) fn probe_a11y_bus() -> bool {
    use atspi::zbus;
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return false,
    };
    runtime.block_on(async {
        let Ok(bus) = zbus::Connection::session().await else {
            return false;
        };
        let Ok(proxy) = zbus::fdo::DBusProxy::new(&bus).await else {
            return false;
        };
        let Ok(name) = "org.a11y.Bus".try_into() else {
            return false;
        };
        proxy.name_has_owner(name).await.unwrap_or(false)
    })
}
