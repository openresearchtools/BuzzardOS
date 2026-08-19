// SPDX-License-Identifier: AGPL-3.0-or-later

//! Short-lived agent-cursor state for one daemonless CLI invocation.

use crate::cursor::{
    CursorConfig, CursorKey, CursorVisualState, MotionConfig, OverlayCommand, RenderStateCore,
};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

struct CursorState {
    template: CursorConfig,
    cursors: HashMap<CursorKey, RenderStateCore>,
}

fn state() -> &'static Mutex<CursorState> {
    static STATE: OnceLock<Mutex<CursorState>> = OnceLock::new();
    STATE.get_or_init(|| {
        let template = CursorConfig::default();
        let mut cursors = HashMap::new();
        cursors.insert("default".into(), RenderStateCore::new(template.clone()));
        Mutex::new(CursorState { template, cursors })
    })
}

fn with_cursor<T>(key: &str, action: impl FnOnce(&mut RenderStateCore) -> T) -> T {
    let mut state = state().lock().expect("cursor state lock");
    let template = state.template.clone();
    let cursor = state.cursors.entry(key.to_owned()).or_insert_with(|| {
        let mut config = template;
        config.cursor_id = key.to_owned();
        RenderStateCore::new(config)
    });
    action(cursor)
}

pub fn send_command_for(key: CursorKey, command: OverlayCommand) {
    if key.is_empty() {
        return;
    }
    with_cursor(&key, |cursor| {
        cursor.apply_command_base(command.clone());
    });
    if crate::platform::wayland::is_wayland() {
        let _ = crate::platform::wayland::overlay::forward(&command);
    }
}

pub fn is_enabled_for(key: &str) -> bool {
    with_cursor(key, |cursor| cursor.visible)
}

pub fn current_position_for(key: &str) -> (f64, f64) {
    with_cursor(key, |cursor| cursor.pos)
}

pub fn current_motion_for(key: &str) -> MotionConfig {
    with_cursor(key, |cursor| cursor.motion.clone())
}

pub fn current_theme_state_for(
    key: &str,
) -> Option<(String, String, String, Option<String>, CursorVisualState)> {
    Some(with_cursor(key, |cursor| {
        let (id, version, profile, fallback) = cursor.active_theme_metadata();
        (id, version, profile, fallback, cursor.visual.clone())
    }))
}

pub async fn animate_cursor_to_for(key: CursorKey, x: f64, y: f64) {
    if key.is_empty() {
        return;
    }
    let sentinel = current_position_for(&key).0 < -50.0;
    if sentinel {
        send_command_for(
            key.clone(),
            OverlayCommand::SnapTo {
                x,
                y,
                heading_radians: Some(std::f64::consts::FRAC_PI_4),
            },
        );
    }
    let wait_ms = current_motion_for(&key)
        .glide_duration_ms
        .clamp(0.0, 2_000.0) as u64;
    send_command_for(
        key,
        OverlayCommand::MoveTo {
            x,
            y,
            end_heading_radians: std::f64::consts::FRAC_PI_4,
        },
    );
    if wait_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
    }
}

pub fn remove_cursor(key: CursorKey) {
    if key.is_empty() || key == "default" {
        return;
    }
    state()
        .lock()
        .expect("cursor state lock")
        .cursors
        .remove(&key);
    if crate::platform::wayland::is_wayland() {
        crate::platform::wayland::overlay::remove();
    }
}
