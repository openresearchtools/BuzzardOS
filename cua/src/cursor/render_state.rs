//! Linux agent-cursor animation state and tiny-skia rendering.

use crate::cursor::{
    CompiledTheme, CursorAction, CursorConfig, CursorVisualState, MotionConfig, OverlayCommand,
    PathPlanner, PathState, PlannedPath, Spring,
};
use std::sync::Arc;

pub struct RenderStateCore {
    /// Frozen copy of the launch-time CursorConfig.
    pub cfg: CursorConfig,
    /// Current motion / timing config (mutable via [`OverlayCommand::SetMotion`]).
    pub motion: MotionConfig,
    /// Current rendered position in screen / overlay-window coordinates.
    pub pos: (f64, f64),
    /// Visual heading in radians (tip direction = motion_dir + π).
    pub heading: f64,
    /// In-flight planned path; `None` = at rest.
    pub path: Option<PlannedPath>,
    /// Arc-distance travelled along the current path so far.
    pub dist: f64,
    /// Post-arrival spring-settle state.
    pub spring: Option<Spring>,
    /// Target the spring is settling toward: `(x, y, heading)`.
    pub spring_tgt: Option<(f64, f64, f64)>,
    /// Click-pulse phase 0..1; `None` = no pulse in flight.
    pub click_t: Option<f64>,
    /// Whether a button is currently being held for this cursor.
    pub pressed: bool,
    /// Semantic action and animation playback state.
    pub visual: CursorVisualState,
    /// Decoded installed or embedded theme.
    pub theme: Option<Arc<CompiledTheme>>,
    /// Non-fatal launch-time fallback reason, if an installed theme failed.
    pub theme_fallback: Option<String>,
    /// User-controlled visibility.
    pub visible: bool,
    /// Idle-hide: elapsed seconds since last activity.
    pub idle_secs: f64,
    /// Idle-hide fade: 1.0 = fully visible, 0.0 = fully hidden.
    pub idle_alpha: f64,
    /// Window id the overlay should be pinned above (for z-ordering).
    pub pinned_wid: Option<u64>,
}

impl RenderStateCore {
    /// Build the core from a launch-time CursorConfig.
    /// `pos` starts at the off-screen sentinel `(-200, -200)` to indicate
    /// "never placed on screen yet" — the click path uses this to detect
    /// first-placement and snap rather than animate.
    pub fn new(cfg: CursorConfig) -> Self {
        let motion = cfg.motion.clone();
        let visual = CursorVisualState {
            reduced_motion: cfg.reduced_motion,
            ..CursorVisualState::default()
        };
        let (theme, theme_fallback) = match crate::cursor::load_installed_theme(&cfg.theme_id) {
            Ok(theme) => (theme, None),
            Err(error) => (
                Some(crate::cursor::embedded_default_theme()),
                Some(format!(
                    "theme `{}` could not be loaded; using {}: {error}",
                    cfg.theme_id,
                    crate::cursor::DEFAULT_THEME_ID
                )),
            ),
        };
        Self {
            cfg,
            motion,
            visual,
            theme,
            theme_fallback,
            pos: (-200.0, -200.0),
            heading: std::f64::consts::FRAC_PI_4,
            path: None,
            dist: 0.0,
            spring: None,
            spring_tgt: None,
            click_t: None,
            pressed: false,
            visible: true,
            idle_secs: 0.0,
            idle_alpha: 1.0,
            pinned_wid: None,
        }
    }

    /// Return the theme that is actually being painted, including any
    /// non-fatal fallback from an unavailable launch-time selection.
    pub fn active_theme_metadata(&self) -> (String, String, String, Option<String>) {
        match self.theme.as_deref() {
            Some(theme) => (
                theme.id.clone(),
                theme.version.clone(),
                theme.profile.clone(),
                self.theme_fallback.clone(),
            ),
            None => (
                crate::cursor::DEFAULT_THEME_ID.into(),
                crate::cursor::DEFAULT_THEME_VERSION.into(),
                crate::cursor::THEME_PROFILE.into(),
                self.theme_fallback.clone(),
            ),
        }
    }

    /// Advance the animation by `dt` seconds using runtime [`MotionConfig`]
    /// for peak / floor / spring constants. Used by Windows + Linux.
    ///
    /// The speed profile is `16·u²·(1-u)²` (peaks at 1.0 at u=0.5) — the
    /// 1:1 port of `AgentCursorRenderer`'s smootherstep envelope. Floor
    /// speed switches from `min_start_speed` to `min_end_speed` at the
    /// midpoint so the cursor decelerates as it approaches the target.
    /// Spring overshoot is `0.5` (Windows/Linux convention).
    ///
    /// Returns `true` when the planned path just ended (so the caller can
    /// fire an arrival oneshot to unblock `animate_cursor_to`).
    pub fn tick_motion(&mut self, dt: f64) -> bool {
        let spring_k = self.motion.spring * 400.0;
        let spring_c = self.motion.spring * 20.0;

        let mut fire_arrival = false;

        if let Some(ref p) = self.path {
            let path_len = p.length.max(1.0);
            let path_frac = (self.dist / path_len).clamp(0.0, 1.0);
            let profile = 16.0 * path_frac * path_frac * (1.0 - path_frac) * (1.0 - path_frac);
            let floor = if path_frac < 0.5 {
                self.motion.min_start_speed
            } else {
                self.motion.min_end_speed
            };
            let speed_based = (floor + (self.motion.peak_speed - floor) * profile).max(floor);
            // Fixed-duration override: when `glide_duration_ms > 0` the move
            // takes exactly that long regardless of distance, so an orchestrator
            // can lock glides to a known cadence. `0` (the default) keeps the
            // speed-based timing untouched.
            let speed = if self.motion.glide_duration_ms > 0.0 {
                path_len / (self.motion.glide_duration_ms / 1000.0)
            } else {
                speed_based
            };
            self.dist += speed * dt;

            if self.dist >= path_len {
                let end = p.sample(path_len);
                let end_heading = p.end_visual_heading;
                let vh = end.heading;
                // In fixed-duration mode the constant speed can be large; base
                // the settle impulse on the normal end-floor so the landing
                // stays as crisp as a speed-based glide instead of overshooting
                // proportionally to a short duration.
                let impulse = if self.motion.glide_duration_ms > 0.0 {
                    self.motion.min_end_speed
                } else {
                    speed
                };
                self.spring = Some(Spring {
                    ox: 0.0,
                    oy: 0.0,
                    vx: impulse * 0.5 * vh.cos(),
                    vy: impulse * 0.5 * vh.sin(),
                });
                self.spring_tgt = Some((end.x, end.y, end_heading));
                self.pos = (end.x, end.y);
                self.heading = end_heading;
                self.path = None;
                self.dist = 0.0;
                fire_arrival = true;
            } else {
                let s: PathState = p.sample(self.dist);
                self.pos = (s.x, s.y);
                // Point the arrow exactly along the path tangent (the renderer
                // adds π, so we store tangent+π). Assigned directly rather than
                // rate-limited toward it, so the tip actually tracks the
                // trajectory instead of lagging behind on fast/short glides.
                self.heading = s.heading + std::f64::consts::PI;
            }
        } else if let Some(mut s) = self.spring {
            if let Some((tx, ty, th)) = self.spring_tgt {
                let substeps = 4;
                let sdt = dt / substeps as f64;
                for _ in 0..substeps {
                    s.vx += (-spring_k * s.ox - spring_c * s.vx) * sdt;
                    s.vy += (-spring_k * s.oy - spring_c * s.vy) * sdt;
                    s.ox += s.vx * sdt;
                    s.oy += s.vy * sdt;
                }
                self.pos = (tx + s.ox, ty + s.oy);
                self.heading = th;
                if s.ox.hypot(s.oy) < 0.3 && s.vx.hypot(s.vy) < 2.0 {
                    self.pos = (tx, ty);
                    self.spring = None;
                } else {
                    self.spring = Some(s);
                }
            }
        }

        if let Some(t) = self.click_t {
            let next = t + dt * 4.0;
            self.click_t = if next >= 1.0 { None } else { Some(next) };
        }

        self.tick_idle(dt);

        fire_arrival
    }

    /// Shared idle-hide / fade logic — accumulate idle time when nothing is
    /// moving, then fade `idle_alpha` from 1→0 over 180ms once
    /// `motion.idle_hide_ms` has elapsed.  Identical across all platforms.
    fn tick_idle(&mut self, dt: f64) {
        self.visual.tick(dt);
        let idle_hide_ms = self.motion.idle_hide_ms;
        if idle_hide_ms > 0.0 {
            let moving = self.path.is_some() || self.spring.is_some() || self.click_t.is_some();
            if moving {
                self.idle_secs = 0.0;
                self.idle_alpha = 1.0;
            } else {
                self.idle_secs += dt;
                let fade_start = idle_hide_ms / 1000.0;
                let fade_end = fade_start + 0.18; // 180ms fade like Windows ref
                if self.idle_secs > fade_end {
                    self.idle_alpha = 0.0;
                } else if self.idle_secs > fade_start {
                    let t = (self.idle_secs - fade_start) / 0.18;
                    self.idle_alpha = 1.0 - t.clamp(0.0, 1.0);
                }
            }
        } else {
            self.idle_alpha = 1.0;
        }
    }

    /// Apply one Linux overlay command.
    pub fn apply_command_base(&mut self, cmd: OverlayCommand) -> bool {
        match cmd {
            OverlayCommand::MoveTo {
                x,
                y,
                end_heading_radians,
            } => {
                // Offset the artwork so its tip lands on the requested point.
                const CLICK_OFFSET: f64 = 16.0;
                let turn_radius = self.motion.turn_radius;
                let tx = x + end_heading_radians.cos() * CLICK_OFFSET;
                let ty = y + end_heading_radians.sin() * CLICK_OFFSET;

                if self.pos.0 < -50.0 {
                    self.pos = (tx, ty);
                }
                let (x0, y0) = self.pos;
                let th0 = self.heading + std::f64::consts::PI;
                let th1 = end_heading_radians + std::f64::consts::PI;
                let plan =
                    PathPlanner::plan(x0, y0, th0, tx, ty, th1, end_heading_radians, turn_radius);
                self.path = Some(plan);
                self.dist = 0.0;
                self.spring = None;
                self.spring_tgt = None;
                if matches!(
                    self.visual.resolved_action,
                    CursorAction::Idle | CursorAction::Navigate
                ) {
                    let delivery = self.visual.delivery;
                    let target = self.visual.target;
                    self.visual.begin(CursorAction::Navigate, delivery, target);
                }
                self.idle_secs = 0.0;
                self.idle_alpha = 1.0;
                true
            }
            OverlayCommand::SnapTo {
                x,
                y,
                heading_radians,
            } => {
                self.pos = (x, y);
                if let Some(heading) = heading_radians {
                    self.heading = heading;
                }
                self.path = None;
                self.dist = 0.0;
                self.spring = None;
                self.spring_tgt = None;
                if matches!(
                    self.visual.resolved_action,
                    CursorAction::Idle | CursorAction::Navigate
                ) {
                    let delivery = self.visual.delivery;
                    let target = self.visual.target;
                    self.visual.begin(CursorAction::Navigate, delivery, target);
                }
                self.idle_secs = 0.0;
                self.idle_alpha = 1.0;
                true
            }
            OverlayCommand::ClickPulse { x, y } => {
                self.pos = (x, y);
                self.click_t = Some(0.0);
                if matches!(
                    self.visual.resolved_action,
                    CursorAction::Idle | CursorAction::Navigate | CursorAction::Click
                ) {
                    let delivery = self.visual.delivery;
                    let target = self.visual.target;
                    self.visual.begin(CursorAction::Click, delivery, target);
                }
                self.idle_secs = 0.0;
                self.idle_alpha = 1.0;
                true
            }
            OverlayCommand::SetPressed(v) => {
                self.pressed = v;
                if v {
                    let delivery = self.visual.delivery;
                    let target = self.visual.target;
                    self.visual.begin(CursorAction::Drag, delivery, target);
                } else {
                    self.visual.end(CursorAction::Drag);
                }
                self.idle_secs = 0.0;
                self.idle_alpha = 1.0;
                true
            }
            OverlayCommand::SetEnabled(v) => {
                self.visible = v;
                true
            }
            OverlayCommand::SetMotion(m) => {
                self.motion = m;
                true
            }
            OverlayCommand::PinAbove(wid) => {
                self.pinned_wid = Some(wid);
                true
            }
            OverlayCommand::SetTheme {
                theme_id,
                reduced_motion,
            } => {
                match crate::cursor::resolve_theme_selection(&theme_id) {
                    Ok(theme) => {
                        self.theme = theme;
                        self.theme_fallback = None;
                        self.cfg.theme_id = theme_id;
                        self.cfg.reduced_motion = reduced_motion;
                        self.visual.reduced_motion = reduced_motion;
                    }
                    Err(error) => {
                        tracing::warn!(
                            theme_id,
                            error = %error,
                            "keeping the active cursor theme after selection failed"
                        );
                    }
                }
                true
            }
        }
    }
}

// ── tiny-skia rendering ──────────────────────────────────────────────────

/// Paint a single cursor (bloom + click-pulse + optional focus-rect + arrow)
/// into a caller-owned [`tiny_skia::Pixmap`]. tiny-skia's `fill_*` / `stroke_*`
/// are alpha-over, so painting several cursors into the same pixmap composites
/// them with later calls drawn on top — this is what lets the macOS overlay
/// render N owned cursors into one buffer / one NSWindow.
///
/// `origin_x` / `origin_y` are subtracted from `core.pos` before drawing
/// (Windows passes the virtual-screen origin; macOS / Linux pass `(0.0, 0.0)`).
/// Both are in **logical** screen points, just like `core.pos`.
///
/// `backing_scale` is the destination-pixmap-pixels per logical-point ratio.
/// On a 2× retina macOS display the caller sizes the pixmap at the screen's
/// PHYSICAL pixel dimensions (logical × backing_scale) and passes `2.0` so
/// the cursor renders at native resolution instead of being upsampled by
/// Core Animation. When the pixmap is sized at LOGICAL pixels, pass `1.0`.
///
/// Everything that operates in pixmap-pixel space (the cursor anchor `px/py`,
/// bloom radius, click-pulse ring radius, stroke widths, focus-rect coords,
/// arrow `display_size`) is multiplied by `backing_scale` so the cursor still
/// occupies the same on-screen logical footprint but at higher pixel fidelity.
///
/// Quiescent / hidden cursors early-return before touching the pixmap, so an
/// idle session costs essentially nothing in the per-frame composite loop.
pub fn paint_cursor(
    pm: &mut tiny_skia::Pixmap,
    core: &RenderStateCore,
    origin_x: f64,
    origin_y: f64,
    backing_scale: f32,
) {
    if !core.visible || core.pos.0 < -100.0 || core.idle_alpha < 0.004 {
        return;
    }

    let s = backing_scale.max(1.0) as f64; // logical-pt → pixmap-pixel scale
                                           // Cursor anchor in pixmap-pixel space: subtract the (logical) origin
                                           // first, then scale into pixmap pixels.
    let (px, py) = ((core.pos.0 - origin_x) * s, (core.pos.1 - origin_y) * s);
    let heading = core.heading;
    let alpha_scale = core.idle_alpha as f32;

    if let Some(theme) = core.theme.as_deref() {
        let tint = (theme.id == crate::cursor::DEFAULT_THEME_ID)
            .then(|| crate::cursor::session_fill_rgba(&core.cfg.cursor_id));
        crate::cursor::paint_compiled_theme_with_tint(
            pm,
            theme,
            &core.visual,
            px as f32,
            py as f32,
            heading as f32,
            backing_scale.max(1.0),
            alpha_scale,
            tint,
        );
    } else {
        // Defensive fallback for a manually constructed RenderStateCore. The
        // normal constructor always resolves either the requested theme or the
        // embedded default.
        crate::cursor::theme::paint_default_theme_with_fill(
            pm,
            &core.visual,
            px as f32,
            py as f32,
            heading as f32,
            backing_scale.max(1.0),
            alpha_scale,
            crate::cursor::session_fill_rgba(&core.cfg.cursor_id),
        );
    }
}

#[cfg(test)]
mod glide_duration_tests {
    use super::*;
    use crate::cursor::{CursorConfig, PathPlanner};

    /// Run a Linux glide to completion and return its duration.
    fn arrival_secs(glide_ms: f64, dist_pts: f64) -> f64 {
        let mut core = RenderStateCore::new(CursorConfig::default());
        core.motion.glide_duration_ms = glide_ms;
        core.motion.idle_hide_ms = 0.0;
        core.pos = (0.0, 0.0);
        // Aligned headings → an effectively straight path of length ~dist_pts.
        core.path = Some(PathPlanner::plan(
            0.0, 0.0, 0.0, dist_pts, 0.0, 0.0, 0.0, 80.0,
        ));
        core.dist = 0.0;
        let dt = 1.0 / 240.0;
        let mut t = 0.0;
        for _ in 0..200_000 {
            let arrived = core.tick_motion(dt);
            t += dt;
            if arrived {
                break;
            }
        }
        t
    }

    #[test]
    fn fixed_duration_is_distance_independent_on_both_paths() {
        let short = arrival_secs(300.0, 120.0);
        let long = arrival_secs(300.0, 1400.0);
        assert!((short - 0.3).abs() < 0.05, "short={short}");
        assert!((long - 0.3).abs() < 0.05, "long={long}");
    }

    #[test]
    fn zero_keeps_speed_based_timing() {
        // glide_duration_ms == 0 (the default) → longer paths take longer, on
        let short = arrival_secs(0.0, 120.0);
        let long = arrival_secs(0.0, 1400.0);
        assert!(long > short + 0.2, "short={short} long={long}");
    }
}

#[cfg(test)]
mod backing_scale_tests {
    use super::*;
    use crate::cursor::CursorConfig;

    fn visible_pixel_count(pm: &tiny_skia::Pixmap) -> u32 {
        // Count strongly visible coverage, not the halo's feather pixels.
        // Low-alpha gradient coverage is quantized differently across scales
        // and is not useful evidence for the backing-scale regression.
        pm.data().chunks_exact(4).filter(|px| px[3] > 96).count() as u32
    }

    fn visible_bounds(pm: &tiny_skia::Pixmap) -> (u32, u32) {
        let mut min_x = u32::MAX;
        let mut min_y = u32::MAX;
        let mut max_x = 0;
        let mut max_y = 0;
        for (index, pixel) in pm.data().chunks_exact(4).enumerate() {
            if pixel[3] <= 96 {
                continue;
            }
            let x = index as u32 % pm.width();
            let y = index as u32 / pm.width();
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        assert_ne!(min_x, u32::MAX, "render should have visible pixels");
        (max_x - min_x + 1, max_y - min_y + 1)
    }

    fn render_at(backing_scale: f32, logical_size: u32) -> tiny_skia::Pixmap {
        let mut core = RenderStateCore::new(CursorConfig::default());
        // Place the cursor at the centre of the logical area and disable
        // idle-fade so the arrow paints at full alpha regardless of timing.
        let centre = logical_size as f64 / 2.0;
        core.pos = (centre, centre);
        core.idle_alpha = 1.0;
        core.visible = true;

        // The pixmap is sized in *pixmap* pixels (logical × backing_scale)
        // — that's the macOS retina pipeline: allocate at physical pixels,
        // then let paint_cursor scale into them.
        let pm_size = (logical_size as f32 * backing_scale) as u32;
        let mut pm = tiny_skia::Pixmap::new(pm_size, pm_size).unwrap();
        paint_cursor(&mut pm, &core, 0.0, 0.0, backing_scale);
        pm
    }

    /// The compiled artifact contains vector geometry. Skia must rasterize it
    /// at the destination backing scale, so linear dimensions grow 1:2:3 and
    /// strongly visible coverage grows approximately with the square.
    #[test]
    fn compiled_vectors_render_at_one_two_and_three_x() {
        let pm_1x = render_at(1.0, 200);
        let pm_2x = render_at(2.0, 200);
        let pm_3x = render_at(3.0, 200);

        let n_1x = visible_pixel_count(&pm_1x);
        let n_2x = visible_pixel_count(&pm_2x);
        let n_3x = visible_pixel_count(&pm_3x);

        assert!(n_1x > 0, "1× render should paint SOMETHING (got {n_1x})");
        assert!(n_2x > 0, "2× render should paint SOMETHING (got {n_2x})");
        assert!(n_3x > 0, "3× render should paint SOMETHING (got {n_3x})");

        let ratio_2x = n_2x as f64 / n_1x as f64;
        let ratio_3x = n_3x as f64 / n_1x as f64;
        assert!(
            ratio_2x > 3.0 && ratio_2x < 5.0,
            "2× backing_scale should produce ~4× more visible pixels: \
             got n_1x={n_1x}, n_2x={n_2x}, ratio={ratio_2x:.2}"
        );
        assert!(
            ratio_3x > 7.0 && ratio_3x < 11.0,
            "3× backing_scale should produce ~9× more visible pixels: \
             got n_1x={n_1x}, n_3x={n_3x}, ratio={ratio_3x:.2}"
        );

        let bounds_1x = visible_bounds(&pm_1x);
        let bounds_2x = visible_bounds(&pm_2x);
        let bounds_3x = visible_bounds(&pm_3x);
        for (one, two, three) in [
            (bounds_1x.0, bounds_2x.0, bounds_3x.0),
            (bounds_1x.1, bounds_2x.1, bounds_3x.1),
        ] {
            assert!(
                (two as f64 / one as f64 - 2.0).abs() < 0.15,
                "2× visible bounds should double: {one}, {two}"
            );
            assert!(
                (three as f64 / one as f64 - 3.0).abs() < 0.20,
                "3× visible bounds should triple: {one}, {three}"
            );
        }
    }
}
