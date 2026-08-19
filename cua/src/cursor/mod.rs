//! Agent-cursor state and rendering for the stock-Sway Linux CLI.

pub mod capture_utils;
pub mod motion;
pub mod path_planner;
pub mod render_state;
pub mod theme;
pub mod theme_artifact;

pub use motion::{MotionConfig, Spring};
pub use path_planner::{PathPlanner, PathState, PlannedPath};
pub use render_state::{paint_cursor, RenderStateCore};
pub use theme::{
    session_fill_rgba, CursorAction, CursorVisualState, ReducedMotion, DEFAULT_THEME_ID,
    DEFAULT_THEME_VERSION, THEME_PROFILE,
};
pub use theme_artifact::{
    embedded_default_theme, load_installed_theme, paint_compiled_theme_with_tint,
    resolve_theme_selection, CompiledTheme,
};
#[cfg(feature = "theme-authoring")]
pub use theme_artifact::{encode_theme, install_artifact, uninstall_theme};

/// Configuration assembled from CLI arguments and passed to every
/// platform backend when it initialises the overlay window.
#[derive(Debug, Clone)]
pub struct CursorConfig {
    /// Multi-cursor instance identifier. Defaults to `"default"`.
    pub cursor_id: String,

    /// Installed theme selected at launch.
    pub theme_id: String,

    /// Accessibility motion preference.
    pub reduced_motion: ReducedMotion,

    /// Initial motion config.
    pub motion: MotionConfig,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            cursor_id: "default".into(),
            theme_id: DEFAULT_THEME_ID.into(),
            reduced_motion: ReducedMotion::Auto,
            motion: MotionConfig::default(),
        }
    }
}

// ── Shared cursor instance registry ──────────────────────────────────────────

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

/// Per-instance cursor configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorInstanceConfig {
    pub cursor_id: String,
    pub theme_id: String,
    pub reduced_motion: ReducedMotion,
    pub enabled: bool,
}

impl Default for CursorInstanceConfig {
    fn default() -> Self {
        Self {
            cursor_id: "default".into(),
            theme_id: DEFAULT_THEME_ID.into(),
            reduced_motion: ReducedMotion::Auto,
            enabled: true,
        }
    }
}

/// Runtime state for a cursor instance (config + last known position).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorInstanceState {
    pub config: CursorInstanceConfig,
    pub x: Option<f64>,
    pub y: Option<f64>,
}

/// Global registry of cursor instances, keyed by `cursor_id`.
pub struct CursorRegistry {
    inner: Mutex<HashMap<String, CursorInstanceState>>,
}

impl CursorRegistry {
    pub fn new() -> Self {
        let mut map = HashMap::new();
        map.insert(
            "default".into(),
            CursorInstanceState {
                config: CursorInstanceConfig::default(),
                x: None,
                y: None,
            },
        );
        Self {
            inner: Mutex::new(map),
        }
    }

    pub fn update_position(&self, cursor_id: &str, x: f64, y: f64) {
        let mut inner = self.inner.lock().unwrap();
        let state = inner
            .entry(cursor_id.to_owned())
            .or_insert_with(|| CursorInstanceState {
                config: CursorInstanceConfig {
                    cursor_id: cursor_id.to_owned(),
                    ..Default::default()
                },
                x: None,
                y: None,
            });
        state.x = Some(x);
        state.y = Some(y);
    }

    pub fn set_enabled(&self, cursor_id: &str, enabled: bool) {
        let mut inner = self.inner.lock().unwrap();
        let state = inner
            .entry(cursor_id.to_owned())
            .or_insert_with(|| CursorInstanceState {
                config: CursorInstanceConfig {
                    cursor_id: cursor_id.to_owned(),
                    ..Default::default()
                },
                x: None,
                y: None,
            });
        state.config.enabled = enabled;
    }

    pub fn update_config(&self, cursor_id: &str, f: impl FnOnce(&mut CursorInstanceConfig)) {
        let mut inner = self.inner.lock().unwrap();
        let state = inner
            .entry(cursor_id.to_owned())
            .or_insert_with(|| CursorInstanceState {
                config: CursorInstanceConfig {
                    cursor_id: cursor_id.to_owned(),
                    ..Default::default()
                },
                x: None,
                y: None,
            });
        f(&mut state.config);
    }

    pub fn all_states(&self) -> Vec<CursorInstanceState> {
        self.inner.lock().unwrap().values().cloned().collect()
    }

    /// Drop a session's cursor metadata entry (fired from the `session_end`
    /// hook). The `"default"` key backs the anonymous / one-shot path and is
    /// guarded against removal; an empty or absent key is a harmless no-op.
    pub fn remove(&self, cursor_id: &str) {
        if cursor_id.is_empty() || cursor_id == "default" {
            return;
        }
        self.inner.lock().unwrap().remove(cursor_id);
    }
}

impl Default for CursorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Identifier for one owned cursor in the keyed render collection.
///
/// Resolved by the macOS tool layer (see `resolve_cursor_key`) with the
/// precedence: explicit `cursor_id` arg > injected `_session_id` > `"default"`.
/// The render side treats it as an opaque insertion-ordered map key; the
/// `"default"` key is special-cased (never removed) so the anonymous /
/// one-shot `cua-driver call` path is backward compatible.
pub type CursorKey = String;

/// Commands sent from CLI tool handlers to the in-process overlay thread.
#[derive(Debug, Clone)]
pub enum OverlayCommand {
    /// Animate the cursor to a new screen position.
    MoveTo {
        x: f64,
        y: f64,
        end_heading_radians: f64,
    },
    /// Snap the cursor immediately to a screen position, optionally updating heading.
    SnapTo {
        x: f64,
        y: f64,
        heading_radians: Option<f64>,
    },
    /// Start the click-press visual.
    ClickPulse { x: f64, y: f64 },
    /// Toggle the held-button visual state.
    SetPressed(bool),
    /// Show or hide the overlay.
    SetEnabled(bool),
    /// Update the motion/timing config live.
    SetMotion(MotionConfig),
    /// Pin the overlay above a specific window (by platform window id).
    PinAbove(u64),
    /// Select an already-installed cursor theme for this cursor instance.
    SetTheme {
        theme_id: String,
        reduced_motion: ReducedMotion,
    },
}

/// Build the shared overlay command for one native pointer position.
///
/// Native drag implementations report the actual event coordinate while the
/// cursor artwork is centred 16 points down-right so its tip lands on that
/// coordinate. Keeping this transform here prevents platform-specific drag
/// loops from drifting apart.
pub fn track_pointer_command(x: f64, y: f64) -> OverlayCommand {
    const CLICK_OFFSET: f64 = 16.0;
    let heading = std::f64::consts::FRAC_PI_4;
    OverlayCommand::SnapTo {
        x: x + heading.cos() * CLICK_OFFSET,
        y: y + heading.sin() * CLICK_OFFSET,
        heading_radians: Some(heading),
    }
}

#[cfg(test)]
mod pointer_tracking_tests {
    use super::*;

    #[test]
    fn tracked_artwork_keeps_its_tip_on_the_native_pointer() {
        let OverlayCommand::SnapTo {
            x,
            y,
            heading_radians: Some(heading),
        } = track_pointer_command(120.0, 80.0)
        else {
            panic!("pointer tracking must produce an anchored snap");
        };
        assert!((x - (120.0 + heading.cos() * 16.0)).abs() < f64::EPSILON);
        assert!((y - (80.0 + heading.sin() * 16.0)).abs() < f64::EPSILON);
    }
}
