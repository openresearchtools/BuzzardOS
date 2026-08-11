// SPDX-License-Identifier: AGPL-3.0-or-later

//! Typed appearance tokens and deterministic per-user theme projections.
//!
//! The shell and Settings application consume this module rather than keeping
//! independent colour literals or configuration templates.  Static GTK theme
//! assets use the same token names and are contract-tested against these
//! values.

use crate::persistence::{PersistenceError, atomic_write};
use crate::state::{SolidColor, ThemeMode};
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemePalette {
    pub canvas: SolidColor,
    pub desktop: SolidColor,
    pub field: SolidColor,
    pub menu: SolidColor,
    pub surface: SolidColor,
    pub raised: SolidColor,
    pub hover: SolidColor,
    pub border: SolidColor,
    pub control_border: SolidColor,
    pub text: SolidColor,
    pub selected_text: SolidColor,
    pub text_secondary: SolidColor,
    pub text_muted: SolidColor,
    pub disabled: SolidColor,
    pub selection: SolidColor,
    pub focus: SolidColor,
    pub folder: SolidColor,
    pub folder_tab: SolidColor,
    pub link: SolidColor,
    pub destructive: SolidColor,
    pub destructive_icon: SolidColor,
    pub warning: SolidColor,
    pub success: SolidColor,
}

pub const DARK_PALETTE: ThemePalette = ThemePalette {
    canvas: SolidColor::new(0x18, 0x18, 0x18),
    desktop: SolidColor::new(0x20, 0x22, 0x25),
    field: SolidColor::new(0x1d, 0x1d, 0x1d),
    menu: SolidColor::new(0x22, 0x22, 0x22),
    surface: SolidColor::new(0x28, 0x28, 0x28),
    raised: SolidColor::new(0x30, 0x30, 0x30),
    hover: SolidColor::new(0x3f, 0x3f, 0x3f),
    border: SolidColor::new(0x54, 0x54, 0x54),
    control_border: SolidColor::new(0x7a, 0x7a, 0x7a),
    text: SolidColor::new(0xe6, 0xe6, 0xe6),
    // Cinnamon is a bright selection surface. Dark ink gives it substantially
    // better contrast than white and remains identical in both modes.
    selected_text: SolidColor::new(0x18, 0x18, 0x18),
    text_secondary: SolidColor::new(0xb8, 0xb8, 0xb8),
    text_muted: SolidColor::new(0x98, 0x98, 0x98),
    disabled: SolidColor::new(0x85, 0x85, 0x85),
    selection: SolidColor::new(0xff, 0x71, 0x39),
    focus: SolidColor::new(0xff, 0x9b, 0x73),
    folder: SolidColor::new(0xff, 0x71, 0x39),
    folder_tab: SolidColor::new(0xff, 0x9b, 0x73),
    link: SolidColor::new(0xff, 0x9b, 0x73),
    destructive: SolidColor::new(0x5c, 0x28, 0x28),
    destructive_icon: SolidColor::new(0xf0, 0x7a, 0x7a),
    warning: SolidColor::new(0xe6, 0xb8, 0x5c),
    success: SolidColor::new(0x6c, 0xcb, 0x7a),
};

pub const LIGHT_PALETTE: ThemePalette = ThemePalette {
    canvas: SolidColor::new(0xe8, 0xe4, 0xde),
    desktop: SolidColor::new(0xf4, 0xf1, 0xec),
    field: SolidColor::new(0xff, 0xfd, 0xf9),
    menu: SolidColor::new(0xee, 0xea, 0xe4),
    surface: SolidColor::new(0xf4, 0xf1, 0xec),
    raised: SolidColor::new(0xe3, 0xde, 0xd6),
    hover: SolidColor::new(0xd5, 0xce, 0xc5),
    border: SolidColor::new(0xa5, 0x9c, 0x90),
    control_border: SolidColor::new(0x76, 0x6d, 0x63),
    text: SolidColor::new(0x28, 0x23, 0x1f),
    selected_text: SolidColor::new(0x18, 0x18, 0x18),
    text_secondary: SolidColor::new(0x5f, 0x57, 0x4f),
    text_muted: SolidColor::new(0x70, 0x67, 0x5e),
    disabled: SolidColor::new(0x7c, 0x74, 0x6c),
    selection: SolidColor::new(0xff, 0x71, 0x39),
    focus: SolidColor::new(0xb5, 0x3b, 0x12),
    folder: SolidColor::new(0xff, 0x71, 0x39),
    folder_tab: SolidColor::new(0xd8, 0x58, 0x27),
    link: SolidColor::new(0x9d, 0x35, 0x11),
    destructive: SolidColor::new(0xf2, 0xd5, 0xd2),
    destructive_icon: SolidColor::new(0xa8, 0x22, 0x22),
    warning: SolidColor::new(0x82, 0x56, 0x00),
    success: SolidColor::new(0x1f, 0x6b, 0x2d),
};

impl ThemeMode {
    pub const fn palette(self) -> &'static ThemePalette {
        match self {
            Self::Dark => &DARK_PALETTE,
            Self::Light => &LIGHT_PALETTE,
        }
    }

    pub const fn gtk_theme_name(self) -> &'static str {
        match self {
            Self::Dark => "WildBuzzard-Dark",
            Self::Light => "WildBuzzard-Light",
        }
    }

    pub const fn color_scheme_preference(self) -> &'static str {
        match self {
            Self::Dark => "prefer-dark",
            Self::Light => "prefer-light",
        }
    }

    pub const fn kde_color_scheme(self) -> &'static str {
        self.gtk_theme_name()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeConfigSet {
    pub mode: ThemeMode,
    pub gtk3_settings: String,
    pub gtk4_settings: String,
    pub kde_globals: String,
    pub foot: String,
    pub mako: String,
}

impl ThemeConfigSet {
    pub fn for_mode(mode: ThemeMode) -> Self {
        Self {
            mode,
            gtk3_settings: render_gtk_settings(mode, 3),
            gtk4_settings: render_gtk_settings(mode, 4),
            kde_globals: render_kde_globals(mode),
            foot: render_foot(mode),
            mako: render_mako(mode),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedThemeFiles {
    pub gtk3_settings: PathBuf,
    pub gtk4_settings: PathBuf,
    pub kde_globals: PathBuf,
    pub foot: PathBuf,
    pub mako: PathBuf,
}

#[derive(Debug, Error)]
pub enum ThemeApplyError {
    #[error("cannot create theme configuration directory {path}: {source}")]
    CreateDirectory {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

/// Write every toolkit/application projection as a same-directory atomic
/// replacement. The caller sends live settings notifications only after this
/// complete set succeeds.
pub fn apply_theme_files(
    config_home: &Path,
    configs: &ThemeConfigSet,
) -> Result<AppliedThemeFiles, ThemeApplyError> {
    let files = AppliedThemeFiles {
        gtk3_settings: config_home.join("gtk-3.0/settings.ini"),
        gtk4_settings: config_home.join("gtk-4.0/settings.ini"),
        kde_globals: config_home.join("kdeglobals"),
        foot: config_home.join("foot/foot.ini"),
        mako: config_home.join("mako/config"),
    };
    for directory in [
        config_home.join("gtk-3.0"),
        config_home.join("gtk-4.0"),
        config_home.join("foot"),
        config_home.join("mako"),
    ] {
        fs::create_dir_all(&directory).map_err(|source| ThemeApplyError::CreateDirectory {
            path: directory.display().to_string(),
            source,
        })?;
    }
    for (path, contents) in [
        (&files.gtk3_settings, configs.gtk3_settings.as_bytes()),
        (&files.gtk4_settings, configs.gtk4_settings.as_bytes()),
        (&files.kde_globals, configs.kde_globals.as_bytes()),
        (&files.foot, configs.foot.as_bytes()),
        (&files.mako, configs.mako.as_bytes()),
    ] {
        atomic_write(path, contents, 0o600)?;
    }
    Ok(files)
}

fn css(color: SolidColor) -> String {
    color.to_string().to_ascii_lowercase()
}

fn hex(color: SolidColor) -> String {
    css(color).trim_start_matches('#').to_owned()
}

fn rgb(color: SolidColor) -> String {
    format!("{},{},{}", color.red, color.green, color.blue)
}

fn render_gtk_settings(mode: ThemeMode, major: u8) -> String {
    // Dark and Light are complete, separately named themes.  GTK's
    // prefer-dark flag asks for a dark variant of the selected theme; because
    // WildBuzzard-Dark has no second "-dark" variant, that request can fall
    // back to Adwaita-dark and its blue accent.
    let prefer_dark = 0;
    let button_images = if major == 3 {
        "gtk-menu-images=1\ngtk-button-images=1\n"
    } else {
        ""
    };
    format!(
        "[Settings]\n\
         gtk-theme-name={}\n\
         gtk-icon-theme-name=WildBuzzard\n\
         gtk-cursor-theme-name=Adwaita\n\
         gtk-application-prefer-dark-theme={prefer_dark}\n\
         gtk-enable-animations=1\n\
         {button_images}",
        mode.gtk_theme_name()
    )
}

fn render_kde_globals(mode: ThemeMode) -> String {
    let p = mode.palette();
    let alternate = if mode == ThemeMode::Dark {
        p.raised
    } else {
        p.canvas
    };
    format!(
        "[General]\nColorScheme={}\nName={}\nshadeSortColumn=true\n\n\
         [KDE]\ncontrast=4\n\n\
         [Icons]\nTheme=WildBuzzard\n\n\
         [Colors:Button]\nBackgroundAlternate={}\nBackgroundNormal={}\nDecorationFocus={}\nDecorationHover={}\nForegroundActive={}\nForegroundInactive={}\nForegroundLink={}\nForegroundNegative={}\nForegroundNeutral={}\nForegroundNormal={}\nForegroundPositive={}\nForegroundVisited={}\n\n\
         [Colors:Selection]\nBackgroundAlternate={}\nBackgroundNormal={}\nDecorationFocus={}\nDecorationHover={}\nForegroundActive={}\nForegroundInactive={}\nForegroundLink={}\nForegroundNegative={}\nForegroundNeutral={}\nForegroundNormal={}\nForegroundPositive={}\nForegroundVisited={}\n\n\
         [Colors:Tooltip]\nBackgroundAlternate={}\nBackgroundNormal={}\nDecorationFocus={}\nDecorationHover={}\nForegroundActive={}\nForegroundInactive={}\nForegroundLink={}\nForegroundNegative={}\nForegroundNeutral={}\nForegroundNormal={}\nForegroundPositive={}\nForegroundVisited={}\n\n\
         [Colors:View]\nBackgroundAlternate={}\nBackgroundNormal={}\nDecorationFocus={}\nDecorationHover={}\nForegroundActive={}\nForegroundInactive={}\nForegroundLink={}\nForegroundNegative={}\nForegroundNeutral={}\nForegroundNormal={}\nForegroundPositive={}\nForegroundVisited={}\n\n\
         [Colors:Window]\nBackgroundAlternate={}\nBackgroundNormal={}\nDecorationFocus={}\nDecorationHover={}\nForegroundActive={}\nForegroundInactive={}\nForegroundLink={}\nForegroundNegative={}\nForegroundNeutral={}\nForegroundNormal={}\nForegroundPositive={}\nForegroundVisited={}\n\n\
         [WM]\nactiveBackground={}\nactiveBlend={}\nactiveForeground={}\ninactiveBackground={}\ninactiveBlend={}\ninactiveForeground={}\n",
        mode.kde_color_scheme(),
        mode.gtk_theme_name(),
        rgb(alternate),
        rgb(p.raised),
        rgb(p.focus),
        rgb(p.selection),
        rgb(p.selected_text),
        rgb(p.disabled),
        rgb(p.link),
        rgb(p.destructive_icon),
        rgb(p.warning),
        rgb(p.text),
        rgb(p.success),
        rgb(p.link),
        rgb(p.folder_tab),
        rgb(p.selection),
        rgb(p.focus),
        rgb(p.selection),
        rgb(p.selected_text),
        rgb(p.selected_text),
        rgb(p.selected_text),
        rgb(p.selected_text),
        rgb(p.selected_text),
        rgb(p.selected_text),
        rgb(p.selected_text),
        rgb(p.selected_text),
        rgb(p.menu),
        rgb(p.field),
        rgb(p.focus),
        rgb(p.selection),
        rgb(p.text),
        rgb(p.disabled),
        rgb(p.link),
        rgb(p.destructive_icon),
        rgb(p.warning),
        rgb(p.text),
        rgb(p.success),
        rgb(p.link),
        rgb(p.menu),
        rgb(p.field),
        rgb(p.focus),
        rgb(p.selection),
        rgb(p.text),
        rgb(p.disabled),
        rgb(p.link),
        rgb(p.destructive_icon),
        rgb(p.warning),
        rgb(p.text),
        rgb(p.success),
        rgb(p.link),
        rgb(p.raised),
        rgb(p.surface),
        rgb(p.focus),
        rgb(p.selection),
        rgb(p.text),
        rgb(p.disabled),
        rgb(p.link),
        rgb(p.destructive_icon),
        rgb(p.warning),
        rgb(p.text),
        rgb(p.success),
        rgb(p.link),
        rgb(p.raised),
        rgb(p.raised),
        rgb(p.text),
        rgb(p.menu),
        rgb(p.menu),
        rgb(p.text_secondary),
    )
}

fn render_foot(mode: ThemeMode) -> String {
    let p = mode.palette();
    let theme_name = if mode == ThemeMode::Dark {
        "dark"
    } else {
        "light"
    };
    format!(
        "[main]\nfont=Noto Sans Mono:size=11\npad=8x6\ninitial-color-theme={theme_name}\n\n\
         [colors-{theme_name}]\nforeground={}\nbackground={}\nselection-foreground={}\nselection-background={}\ncursor={} {}\nurls={}\n\n\
         regular0={}\nregular1={}\nregular2={}\nregular3={}\nregular4={}\nregular5={}\nregular6={}\nregular7={}\n\n\
         bright0={}\nbright1={}\nbright2={}\nbright3={}\nbright4={}\nbright5={}\nbright6={}\nbright7={}\n",
        hex(p.text),
        hex(p.canvas),
        hex(p.selected_text),
        hex(p.selection),
        hex(p.canvas),
        hex(p.selection),
        hex(p.link),
        hex(p.canvas),
        hex(p.destructive_icon),
        hex(p.success),
        hex(p.warning),
        hex(p.link),
        "a17ed2",
        "68c0c0",
        hex(p.text_secondary),
        hex(p.control_border),
        "ff9292",
        "86df94",
        "f4ca73",
        "91bcff",
        "bd9ae8",
        "88dada",
        if mode == ThemeMode::Dark {
            "ffffff"
        } else {
            "28231f"
        },
    )
}

fn render_mako(mode: ThemeMode) -> String {
    let p = mode.palette();
    format!(
        "font=Noto Sans 10\nbackground-color={}\ntext-color={}\nborder-color={}\n\
         border-size=1\nborder-radius=4\npadding=10\nmargin=10\nicons=1\nmax-icon-size=48\n\
         progress-color=over {}\n\n[urgency=low]\nborder-color={}\n\n\
         [urgency=high]\nborder-color={}\ndefault-timeout=0\n",
        css(p.surface),
        css(p.text),
        css(p.border),
        css(p.selection),
        css(p.border),
        css(p.destructive_icon),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn relative_luminance(color: SolidColor) -> f64 {
        fn channel(value: u8) -> f64 {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(color.red) + 0.7152 * channel(color.green) + 0.0722 * channel(color.blue)
    }

    fn contrast(a: SolidColor, b: SolidColor) -> f64 {
        let (lighter, darker) = if relative_luminance(a) >= relative_luminance(b) {
            (relative_luminance(a), relative_luminance(b))
        } else {
            (relative_luminance(b), relative_luminance(a))
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    #[test]
    fn palettes_keep_readable_text_and_distinct_interaction_states() {
        for mode in [ThemeMode::Dark, ThemeMode::Light] {
            let palette = mode.palette();
            assert!(contrast(palette.text, palette.surface) >= 7.0);
            assert!(contrast(palette.text_secondary, palette.surface) >= 4.5);
            assert!(contrast(palette.selected_text, palette.selection) >= 4.5);
            assert_ne!(palette.selection, palette.hover);
            assert_ne!(palette.selection, palette.focus);
            assert_ne!(palette.hover, palette.focus);
            assert_ne!(palette.selected_text, SolidColor::new(0xff, 0xff, 0xff));
        }
    }

    #[test]
    fn mode_projection_is_deterministic_and_palette_only() {
        let dark = ThemeConfigSet::for_mode(ThemeMode::Dark);
        let light = ThemeConfigSet::for_mode(ThemeMode::Light);
        assert_eq!(dark, ThemeConfigSet::for_mode(ThemeMode::Dark));
        assert_eq!(light, ThemeConfigSet::for_mode(ThemeMode::Light));
        assert!(dark.gtk3_settings.contains("WildBuzzard-Dark"));
        assert!(light.gtk3_settings.contains("WildBuzzard-Light"));
        assert!(dark.gtk3_settings.contains("prefer-dark-theme=0"));
        assert!(light.gtk3_settings.contains("prefer-dark-theme=0"));
        assert!(dark.kde_globals.contains("ForegroundNormal=24,24,24"));
        assert!(light.kde_globals.contains("ForegroundNormal=24,24,24"));
        assert!(!dark.kde_globals.contains("ForegroundNormal=255,255,255"));
        assert!(!light.kde_globals.contains("ForegroundNormal=255,255,255"));
    }

    #[test]
    fn foot_uses_the_current_non_deprecated_color_section() {
        let dark = render_foot(ThemeMode::Dark);
        assert!(dark.contains("initial-color-theme=dark\n"));
        assert!(dark.contains("[colors-dark]\n"));
        assert!(!dark.contains("[colors]\n"));

        let light = render_foot(ThemeMode::Light);
        assert!(light.contains("initial-color-theme=light\n"));
        assert!(light.contains("[colors-light]\n"));
        assert!(!light.contains("[colors]\n"));
    }

    #[test]
    fn applying_projection_writes_exact_private_files_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let configs = ThemeConfigSet::for_mode(ThemeMode::Light);
        let files = apply_theme_files(temp.path(), &configs).unwrap();
        for (path, expected) in [
            (&files.gtk3_settings, &configs.gtk3_settings),
            (&files.gtk4_settings, &configs.gtk4_settings),
            (&files.kde_globals, &configs.kde_globals),
            (&files.foot, &configs.foot),
            (&files.mako, &configs.mako),
        ] {
            assert_eq!(fs::read_to_string(path).unwrap(), *expected);
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let dark = ThemeConfigSet::for_mode(ThemeMode::Dark);
        apply_theme_files(temp.path(), &dark).unwrap();
        assert_eq!(fs::read_to_string(files.foot).unwrap(), dark.foot);
        assert!(
            fs::read_dir(temp.path().join("foot"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp"))
        );
    }

    #[test]
    fn checked_in_themes_share_geometry_and_match_typed_palette_tokens() {
        let dark3 = include_str!("../../assets/themes/WildBuzzard-Dark/gtk-3.0/gtk.css");
        let light3 = include_str!("../../assets/themes/WildBuzzard-Light/gtk-3.0/gtk.css");
        let dark4 = include_str!("../../assets/themes/WildBuzzard-Dark/gtk-4.0/gtk.css");
        let light4 = include_str!("../../assets/themes/WildBuzzard-Light/gtk-4.0/gtk.css");
        assert_eq!(dark3, light3);
        assert_eq!(dark4, light4);
        for stylesheet in [dark3, dark4] {
            assert!(stylesheet.contains("WildBuzzard-Shared"));
            assert!(!stylesheet.contains('{'));
        }
        for geometry in [
            include_str!("../../assets/themes/WildBuzzard-Shared/gtk-3.0/geometry.css"),
            include_str!("../../assets/themes/WildBuzzard-Shared/gtk-4.0/geometry.css"),
        ] {
            assert!(
                !geometry.contains('#'),
                "geometry contains a palette literal"
            );
            assert!(geometry.contains("@wb_selection"));
            assert!(geometry.contains("@wb_focus"));
            assert!(geometry.contains("@wb_hover"));
            assert!(geometry.contains("scale trough {\n  min-width: 6px;\n  min-height: 6px;"));
            assert!(geometry.contains("scale slider {\n  min-width: 18px;\n  min-height: 18px;"));
        }
        let gtk4_geometry =
            include_str!("../../assets/themes/WildBuzzard-Shared/gtk-4.0/geometry.css");
        assert!(gtk4_geometry.contains("button.wb-primary-action label,"));
        assert!(gtk4_geometry.contains("color: @wb_selected_text;"));

        for (mode, palette_css) in [
            (
                ThemeMode::Dark,
                include_str!("../../assets/themes/WildBuzzard-Dark/gtk-3.0/palette.css"),
            ),
            (
                ThemeMode::Light,
                include_str!("../../assets/themes/WildBuzzard-Light/gtk-3.0/palette.css"),
            ),
        ] {
            let p = mode.palette();
            for (token, value) in [
                ("wb_canvas", p.canvas),
                ("wb_desktop", p.desktop),
                ("wb_field", p.field),
                ("wb_menu", p.menu),
                ("wb_surface", p.surface),
                ("wb_raised", p.raised),
                ("wb_hover", p.hover),
                ("wb_border", p.border),
                ("wb_control_border", p.control_border),
                ("wb_text", p.text),
                ("wb_selected_text", p.selected_text),
                ("wb_secondary", p.text_secondary),
                ("wb_muted", p.text_muted),
                ("wb_disabled", p.disabled),
                ("wb_selection", p.selection),
                ("wb_focus", p.focus),
                ("wb_error", p.destructive_icon),
                ("wb_warning", p.warning),
                ("wb_success", p.success),
            ] {
                assert!(
                    palette_css.contains(&format!(
                        "@define-color {token} {};",
                        value.to_string().to_ascii_lowercase()
                    )),
                    "{mode:?} {token} differs from the typed palette"
                );
            }
            assert!(!palette_css.contains("wb_selected_text #ffffff"));
        }
    }
}
