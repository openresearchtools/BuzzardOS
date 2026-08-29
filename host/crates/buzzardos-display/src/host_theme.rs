// SPDX-License-Identifier: AGPL-3.0-or-later

use gtk::gio;
use gtk::prelude::*;
use gtk4 as gtk;

const DESKTOP_INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";
const COLOR_SCHEME_KEY: &str = "color-scheme";

/// Make plain GTK host windows honor the desktop-wide light/dark preference.
///
/// GTK follows the selected GTK theme on its own, but unlike libadwaita it
/// does not translate GNOME's separate `color-scheme` preference into the
/// dark theme variant. Keep the GSettings object alive for the application
/// lifetime so changes made while Buzzard OS is running apply immediately.
pub(crate) fn follow_system_color_scheme() -> Option<gio::Settings> {
    let source = gio::SettingsSchemaSource::default()?;
    let schema = source.lookup(DESKTOP_INTERFACE_SCHEMA, true)?;
    if !schema.has_key(COLOR_SCHEME_KEY) {
        return None;
    }
    let desktop = gio::Settings::new_full(&schema, gio::SettingsBackend::NONE, None);
    apply_color_scheme(&desktop);
    desktop.connect_changed(Some(COLOR_SCHEME_KEY), |desktop, _| {
        apply_color_scheme(desktop);
    });
    Some(desktop)
}

fn apply_color_scheme(desktop: &gio::Settings) {
    let Some(gtk_settings) = gtk::Settings::default() else {
        return;
    };
    gtk_settings.set_gtk_application_prefer_dark_theme(prefers_dark(
        desktop.string(COLOR_SCHEME_KEY).as_str(),
    ));
}

fn prefers_dark(color_scheme: &str) -> bool {
    color_scheme == "prefer-dark"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_color_scheme_maps_to_the_gtk_dark_variant() {
        assert!(prefers_dark("prefer-dark"));
        assert!(!prefers_dark("prefer-light"));
        assert!(!prefers_dark("default"));
        assert!(!prefers_dark("unknown"));
    }
}
