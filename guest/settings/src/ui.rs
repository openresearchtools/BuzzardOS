// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::model::{
    PageId, SettingsStore, display_scale_socket_path, keyboard_settings_socket_path,
    load_runtime_geometry, set_guest_keyboard, set_guest_scale, validate_display_scale_socket,
    validate_keyboard_settings_socket,
};
use crate::sound::{SoundConnection, SoundController, SoundService, UserVolumePercent};
use crate::{ChangeBus, ChangeSection};
use buzzardos_desktop_core::{
    BackgroundChoice, DARK_WALLPAPER, GuestScalePreset, KeyboardSettings, LIGHT_WALLPAPER,
    SolidColor, ThemeMode,
};
use gtk::gdk;
use gtk::prelude::*;
use gtk4 as gtk;
use std::cell::{Cell, RefCell};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Duration;
use zeroize::Zeroize;

const COMPACT_BREAKPOINT: i32 = 720;
const PAGE_MARGIN: i32 = 24;
const ROW_SPACING: i32 = 12;
const ZONE_TAB_PATH: &str = "/usr/share/zoneinfo/zone.tab";
const ZONEINFO_ROOT: &str = "/usr/share/zoneinfo";
const MAX_ZONE_TAB_BYTES: u64 = 2 * 1024 * 1024;
const INTERACTIVE_USER: &str = "user";
const GUEST_SUDO: &str = "/usr/bin/sudo";
const CHPASSWD: &str = "/usr/sbin/chpasswd";
const SUDO_POLICY_HELPER: &str = "/usr/libexec/buzzardos-guest/sudo-policy";
const PASSWORDLESS_POLICY: &str = "/etc/sudoers.d/91-buzzardos-passwordless";
const PASSWORDLESS_POLICY_CONTENT: &[u8] = b"user ALL=(ALL:ALL) NOPASSWD: ALL\n";
const MAX_PASSWORD_BYTES: usize = 4096;

#[cfg(test)]
const ACCESSIBLE_CONTROL_NAMES: &[&str] = &[
    "Settings navigation",
    "Settings page",
    "Display scaling",
    "Output volume",
    "Output mute",
    "Microphone input volume",
    "Microphone mute",
    "Keyboard language",
    "Keyboard layout",
    "Keyboard hardware",
    "Automatic date and time",
    "Current local date and time",
    "Time zone",
    "Light theme",
    "Dark theme",
    "Desktop background colour",
    "Capped task buttons",
    "Change password",
    "Passwordless sudo",
    "Automatic software updates",
];

pub(crate) fn build_fatal_window(
    application: &gtk::Application,
    error: &str,
) -> gtk::ApplicationWindow {
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Settings")
        .icon_name("buzzardos-settings")
        .default_width(560)
        .default_height(320)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    set_margins(&content, PAGE_MARGIN);
    content.append(&heading("Settings could not start"));
    let detail = wrapped_label(error);
    detail.add_css_class("error");
    content.append(&detail);
    window.set_child(Some(&content));
    window
}

pub(crate) fn build_window(
    application: &gtk::Application,
    store: Rc<RefCell<SettingsStore>>,
    bus: Rc<ChangeBus>,
) -> gtk::ApplicationWindow {
    apply_current_process_theme(store.borrow().settings.appearance.theme);
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Settings")
        .icon_name("buzzardos-settings")
        .default_width(850)
        .default_height(620)
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    // GtkBox is transparent by default. Give the window-filling Settings
    // root its own opaque paint instead of relying on whatever backing the
    // toolkit theme happens to leave beneath transparent page containers.
    root.add_css_class("wb-settings-root");
    let page_titles = PageId::ALL.map(PageId::title);
    let compact_navigation = gtk::DropDown::from_strings(&page_titles);
    compact_navigation.add_css_class("wb-settings-compact-navigation");
    compact_navigation.set_margin_top(8);
    compact_navigation.set_margin_bottom(8);
    compact_navigation.set_margin_start(12);
    compact_navigation.set_margin_end(12);
    compact_navigation.set_visible(false);
    accessible(
        &compact_navigation,
        "Settings navigation",
        "Choose a Settings page.",
    );
    root.append(&compact_navigation);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.add_css_class("wb-settings-body");
    body.set_hexpand(true);
    body.set_vexpand(true);
    let sidebar = gtk::ListBox::new();
    sidebar.set_selection_mode(gtk::SelectionMode::Single);
    sidebar.set_activate_on_single_click(true);
    sidebar.add_css_class("navigation-sidebar");
    sidebar.add_css_class("wb-settings-sidebar");
    sidebar.set_size_request(190, -1);
    accessible(&sidebar, "Settings navigation", "Choose a Settings page.");
    let mut navigation_rows = Vec::new();
    for page in PageId::ALL {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("wb-settings-navigation-row");
        let row_content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row_content.set_margin_top(9);
        row_content.set_margin_bottom(9);
        row_content.set_margin_start(12);
        row_content.set_margin_end(12);
        row_content.append(&gtk::Image::from_icon_name(page.icon_name()));
        let label = gtk::Label::new(Some(page.title()));
        label.set_xalign(0.0);
        row_content.append(&label);
        row.set_child(Some(&row_content));
        sidebar.append(&row);
        navigation_rows.push(row);
    }
    body.append(&sidebar);
    let separator = gtk::Separator::new(gtk::Orientation::Vertical);
    body.append(&separator);

    let pages = gtk::Stack::builder()
        .hexpand(true)
        .vexpand(true)
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    pages.add_css_class("wb-settings-pages");
    let page_background = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .build();
    {
        let store = Rc::clone(&store);
        page_background.set_draw_func(move |_area, context, width, height| {
            let color = store.borrow().settings.appearance.theme.palette().surface;
            let rgba = solid_to_rgba(color);
            context.set_source_rgba(
                f64::from(rgba.red()),
                f64::from(rgba.green()),
                f64::from(rgba.blue()),
                1.0,
            );
            context.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
            let _ = context.fill();
        });
    }
    pages.add_named(
        &build_display_page(&window, Rc::clone(&store), Rc::clone(&bus)),
        Some(PageId::Display.stack_name()),
    );
    pages.add_named(&build_sound_page(&window), Some(PageId::Sound.stack_name()));
    pages.add_named(
        &build_keyboard_page(&window, Rc::clone(&store), Rc::clone(&bus)),
        Some(PageId::Keyboard.stack_name()),
    );
    pages.add_named(
        &build_time_location_page(&window),
        Some(PageId::TimeLocation.stack_name()),
    );
    pages.add_named(
        &build_appearance_page(
            &window,
            Rc::clone(&store),
            Rc::clone(&bus),
            &page_background,
        ),
        Some(PageId::Appearance.stack_name()),
    );
    pages.add_named(
        &build_security_page(&window),
        Some(PageId::Security.stack_name()),
    );
    pages.add_named(
        &build_updates_page(&window),
        Some(PageId::Updates.stack_name()),
    );
    accessible(&pages, "Settings page", "The selected Settings page.");
    let page_layer = gtk::Overlay::new();
    page_layer.set_hexpand(true);
    page_layer.set_vexpand(true);
    page_layer.set_child(Some(&page_background));
    page_layer.add_overlay(&pages);
    body.append(&page_layer);
    root.append(&body);
    window.set_child(Some(&root));

    let synchronizing = Rc::new(Cell::new(false));
    {
        let pages = pages.clone();
        let compact_navigation = compact_navigation.clone();
        let synchronizing = Rc::clone(&synchronizing);
        sidebar.connect_row_selected(move |_list, row| {
            if synchronizing.get() {
                return;
            }
            let Some(row) = row else { return };
            let Ok(index) = usize::try_from(row.index()) else {
                return;
            };
            let Some(page) = PageId::ALL.get(index) else {
                return;
            };
            synchronizing.set(true);
            pages.set_visible_child_name(page.stack_name());
            compact_navigation.set_selected(index as u32);
            synchronizing.set(false);
        });
    }
    {
        let pages = pages.clone();
        let sidebar = sidebar.clone();
        let rows = navigation_rows.clone();
        let synchronizing = Rc::clone(&synchronizing);
        compact_navigation.connect_selected_notify(move |dropdown| {
            if synchronizing.get() {
                return;
            }
            let index = dropdown.selected() as usize;
            let Some(page) = PageId::ALL.get(index) else {
                return;
            };
            synchronizing.set(true);
            pages.set_visible_child_name(page.stack_name());
            if let Some(row) = rows.get(index) {
                sidebar.select_row(Some(row));
            }
            synchronizing.set(false);
        });
    }
    sidebar.select_row(navigation_rows.first());
    {
        let compact_navigation = compact_navigation.clone();
        let sidebar = sidebar.clone();
        let separator = separator.clone();
        root.connect_notify_local(Some("width"), move |widget, _| {
            let compact = widget.width() > 0 && widget.width() < COMPACT_BREAKPOINT;
            compact_navigation.set_visible(compact);
            sidebar.set_visible(!compact);
            separator.set_visible(!compact);
        });
    }
    window
}

fn build_display_page(
    window: &gtk::ApplicationWindow,
    store: Rc<RefCell<SettingsStore>>,
    bus: Rc<ChangeBus>,
) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);
    let labels = ["Automatic", "100%", "125%", "150%", "175%", "200%"];
    let presets = GuestScalePreset::ALL;
    let scale = gtk::DropDown::from_strings(&labels);
    let current = store.borrow().settings.display.guest_ui_scale;
    scale.set_selected(
        presets
            .iter()
            .position(|candidate| *candidate == current)
            .unwrap_or(0) as u32,
    );
    accessible(
        &scale,
        "Display scaling",
        "Choose the size of text and controls inside this desktop.",
    );
    let availability = scale_service().map_err(|error| error.to_string());
    scale.set_sensitive(store.borrow().writable && availability.is_ok());
    contents.append(&setting_row(
        "Scaling",
        "Changes text and control size without stretching the desktop image.",
        &scale,
    ));

    if let Ok(socket) = availability {
        let changing = Rc::new(Cell::new(false));
        let window = window.clone();
        scale.connect_selected_notify(move |dropdown| {
            if changing.get() {
                return;
            }
            let Some(preset) = presets.get(dropdown.selected() as usize).copied() else {
                return;
            };
            let previous = store.borrow().settings.display.guest_ui_scale;
            if preset == previous {
                return;
            }
            let runtime = load_runtime_geometry(Path::new(crate::model::OUTPUT_STATE_PATH));
            let Some(geometry) = runtime.geometry() else {
                restore_scale(dropdown, previous);
                show_error(
                    &window,
                    "Scaling was not changed",
                    "The display is not ready.",
                );
                return;
            };
            changing.set(true);
            match set_guest_scale(&socket, preset, geometry.geometry_generation) {
                Ok(confirmed) => match store.borrow_mut().persist_confirmed_display_scale(preset) {
                    Ok(generation) => {
                        let _ = bus.emit_changed(generation, &[ChangeSection::Display]);
                    }
                    Err(error) => {
                        let _ = set_guest_scale(&socket, previous, confirmed.geometry_generation);
                        restore_scale(dropdown, previous);
                        show_error(&window, "Scaling was not saved", &error.to_string());
                    }
                },
                Err(error) => {
                    restore_scale(dropdown, previous);
                    show_error(&window, "Scaling was not changed", &error.to_string());
                }
            }
            changing.set(false);
        });
    }
    page("Display", &contents)
}

fn scale_service() -> Result<std::path::PathBuf, String> {
    let path = display_scale_socket_path().map_err(|error| error.to_string())?;
    validate_display_scale_socket(&path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn restore_scale(dropdown: &gtk::DropDown, preset: GuestScalePreset) {
    let index = GuestScalePreset::ALL
        .iter()
        .position(|candidate| *candidate == preset)
        .unwrap_or(0);
    dropdown.set_selected(index as u32);
}

#[derive(Clone)]
struct SoundWidgets {
    output_volume: gtk::Scale,
    output_percent: gtk::Label,
    output_mute: gtk::Switch,
    input_volume: gtk::Scale,
    input_percent: gtk::Label,
    input_mute: gtk::Switch,
}

fn build_sound_page(window: &gtk::ApplicationWindow) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);
    let output = section("Output");
    let output_volume = volume_scale();
    let output_percent = gtk::Label::new(Some("Unavailable"));
    let output_control = volume_control(&output_volume, &output_percent);
    let output_mute = gtk::Switch::new();
    accessible(&output_volume, "Output volume", "Set speaker volume.");
    accessible(
        &output_mute,
        "Output mute",
        "Mute or unmute speaker output.",
    );
    output.append(&setting_row(
        "Volume",
        "Speaker output volume.",
        &output_control,
    ));
    output.append(&setting_row("Mute", "Mute speaker output.", &output_mute));
    contents.append(&output);

    let input = section("Microphone input");
    let input_volume = volume_scale();
    let input_percent = gtk::Label::new(Some("Unavailable"));
    let input_control = volume_control(&input_volume, &input_percent);
    let input_mute = gtk::Switch::new();
    accessible(
        &input_volume,
        "Microphone input volume",
        "Set microphone input volume.",
    );
    accessible(
        &input_mute,
        "Microphone mute",
        "Mute or unmute microphone input.",
    );
    input.append(&setting_row(
        "Volume",
        "Microphone input volume.",
        &input_control,
    ));
    input.append(&setting_row("Mute", "Mute microphone input.", &input_mute));
    contents.append(&input);

    let widgets = SoundWidgets {
        output_volume,
        output_percent,
        output_mute,
        input_volume,
        input_percent,
        input_mute,
    };
    let page = page("Sound", &contents);
    match SoundService::spawn() {
        Ok(service) => wire_sound(window, &page, widgets, service),
        Err(error) => {
            set_sound_sensitive(&widgets, false);
            show_error(window, "Sound controls are unavailable", &error.to_string());
        }
    }
    page
}

fn volume_scale() -> gtk::Scale {
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 150.0, 1.0);
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    scale.set_size_request(240, -1);
    scale
}

fn volume_control(scale: &gtk::Scale, percent: &gtk::Label) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_size_request(300, -1);
    row.append(scale);
    row.append(percent);
    row
}

fn set_sound_sensitive(widgets: &SoundWidgets, sensitive: bool) {
    widgets.output_volume.set_sensitive(sensitive);
    widgets.output_mute.set_sensitive(sensitive);
    widgets.input_volume.set_sensitive(sensitive);
    widgets.input_mute.set_sensitive(sensitive);
}

fn wire_sound(
    window: &gtk::ApplicationWindow,
    page: &gtk::ScrolledWindow,
    widgets: SoundWidgets,
    service: SoundService,
) {
    let service = Rc::new(service);
    let controller = service.controller();
    let synchronizing = Rc::new(Cell::new(false));
    let output_id = Rc::new(RefCell::new(None));
    let input_id = Rc::new(RefCell::new(None));

    {
        let controller = controller.clone();
        let output_id = Rc::clone(&output_id);
        let synchronizing = Rc::clone(&synchronizing);
        let window = window.clone();
        widgets.output_volume.connect_value_changed(move |scale| {
            if synchronizing.get() {
                return;
            }
            let Some(device) = output_id.borrow().clone() else {
                return;
            };
            let value = scale.value().round().clamp(0.0, 150.0) as u16;
            if let Ok(volume) = UserVolumePercent::new(value)
                && let Err(error) = controller.set_output_volume(&device, volume)
            {
                show_error(&window, "Output volume was not changed", &error.to_string());
            }
        });
    }
    {
        let controller = controller.clone();
        let output_id = Rc::clone(&output_id);
        let synchronizing = Rc::clone(&synchronizing);
        let window = window.clone();
        widgets.output_mute.connect_active_notify(move |switch| {
            if synchronizing.get() {
                return;
            }
            let Some(device) = output_id.borrow().clone() else {
                return;
            };
            if let Err(error) = controller.set_output_mute(&device, switch.is_active()) {
                show_error(&window, "Output mute was not changed", &error.to_string());
            }
        });
    }
    {
        let controller = controller.clone();
        let input_id = Rc::clone(&input_id);
        let synchronizing = Rc::clone(&synchronizing);
        let window = window.clone();
        widgets.input_volume.connect_value_changed(move |scale| {
            if synchronizing.get() {
                return;
            }
            let Some(device) = input_id.borrow().clone() else {
                return;
            };
            let value = scale.value().round().clamp(0.0, 150.0) as u16;
            if let Ok(volume) = UserVolumePercent::new(value)
                && let Err(error) = controller.set_input_volume(&device, volume)
            {
                show_error(
                    &window,
                    "Microphone volume was not changed",
                    &error.to_string(),
                );
            }
        });
    }
    {
        let controller = controller.clone();
        let input_id = Rc::clone(&input_id);
        let synchronizing = Rc::clone(&synchronizing);
        let window = window.clone();
        widgets.input_mute.connect_active_notify(move |switch| {
            if synchronizing.get() {
                return;
            }
            let Some(device) = input_id.borrow().clone() else {
                return;
            };
            if let Err(error) = controller.set_input_mute(&device, switch.is_active()) {
                show_error(
                    &window,
                    "Microphone mute was not changed",
                    &error.to_string(),
                );
            }
        });
    }

    let weak_page = page.downgrade();
    glib::timeout_add_local(Duration::from_millis(250), move || {
        let Some(_page) = weak_page.upgrade() else {
            return glib::ControlFlow::Break;
        };
        // Keeping the service in this source closure ties its lifetime to the
        // Sound page. Returning Break drops both the source and the worker.
        let _service = &service;
        render_sound(&controller, &widgets, &synchronizing, &output_id, &input_id);
        glib::ControlFlow::Continue
    });
}

fn render_sound(
    controller: &SoundController,
    widgets: &SoundWidgets,
    synchronizing: &Cell<bool>,
    output_id: &RefCell<Option<crate::sound::DeviceId>>,
    input_id: &RefCell<Option<crate::sound::DeviceId>>,
) {
    let state = controller.state();
    let ready = state.connection == SoundConnection::Ready;
    let output = state.default_output();
    let input = state.default_input();
    synchronizing.set(true);
    output_id.replace(output.map(|device| device.id.clone()));
    input_id.replace(input.map(|device| device.id.clone()));
    if let Some(device) = output {
        widgets.output_volume.set_value(device.volume_percent);
        widgets
            .output_percent
            .set_label(&format!("{}%", device.volume_percent.round() as u16));
        widgets.output_mute.set_active(device.muted);
    } else {
        widgets.output_volume.set_value(0.0);
        widgets.output_percent.set_label("Unavailable");
        widgets.output_mute.set_active(false);
    }
    if let Some(device) = input {
        widgets.input_volume.set_value(device.volume_percent);
        widgets
            .input_percent
            .set_label(&format!("{}%", device.volume_percent.round() as u16));
        widgets.input_mute.set_active(device.muted);
    } else {
        widgets.input_volume.set_value(0.0);
        widgets.input_percent.set_label("Unavailable");
        widgets.input_mute.set_active(false);
    }
    widgets
        .output_volume
        .set_sensitive(ready && output.is_some());
    widgets.output_mute.set_sensitive(ready && output.is_some());
    widgets.input_volume.set_sensitive(ready && input.is_some());
    widgets.input_mute.set_sensitive(ready && input.is_some());
    synchronizing.set(false);
}

struct KeyboardLanguage {
    name: &'static str,
    layouts: &'static [(&'static str, &'static str)],
}

const ENGLISH_LAYOUTS: &[(&str, &str)] = &[("English (US)", "us"), ("English (UK)", "gb")];
const FRENCH_LAYOUTS: &[(&str, &str)] = &[("French", "fr"), ("French (Canada)", "ca")];
const GERMAN_LAYOUTS: &[(&str, &str)] = &[("German", "de"), ("German (Switzerland)", "ch")];
const SPANISH_LAYOUTS: &[(&str, &str)] = &[("Spanish", "es"), ("Spanish (Latin America)", "latam")];
const PORTUGUESE_LAYOUTS: &[(&str, &str)] = &[("Portuguese", "pt"), ("Portuguese (Brazil)", "br")];
const OTHER_LAYOUTS: &[(&str, &str)] = &[
    ("Arabic", "ara"),
    ("Czech", "cz"),
    ("Danish", "dk"),
    ("Dutch", "nl"),
    ("Finnish", "fi"),
    ("Greek", "gr"),
    ("Hebrew", "il"),
    ("Italian", "it"),
    ("Japanese", "jp"),
    ("Korean", "kr"),
    ("Norwegian", "no"),
    ("Polish", "pl"),
    ("Russian", "ru"),
    ("Swedish", "se"),
    ("Turkish", "tr"),
    ("Ukrainian", "ua"),
];
const KEYBOARD_LANGUAGES: &[KeyboardLanguage] = &[
    KeyboardLanguage {
        name: "English",
        layouts: ENGLISH_LAYOUTS,
    },
    KeyboardLanguage {
        name: "French",
        layouts: FRENCH_LAYOUTS,
    },
    KeyboardLanguage {
        name: "German",
        layouts: GERMAN_LAYOUTS,
    },
    KeyboardLanguage {
        name: "Spanish",
        layouts: SPANISH_LAYOUTS,
    },
    KeyboardLanguage {
        name: "Portuguese",
        layouts: PORTUGUESE_LAYOUTS,
    },
    KeyboardLanguage {
        name: "Other",
        layouts: OTHER_LAYOUTS,
    },
];
const KEYBOARD_MODELS: &[(&str, &str)] = &[
    ("Generic 105-key PC", "pc105"),
    ("Generic 104-key PC", "pc104"),
    ("Generic 101-key PC", "pc101"),
];

fn build_keyboard_page(
    window: &gtk::ApplicationWindow,
    store: Rc<RefCell<SettingsStore>>,
    bus: Rc<ChangeBus>,
) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);
    let keyboard = store.borrow().settings.keyboard.clone();
    // The page heading already names this single group.  Adding another
    // "Keyboard" heading here only repeats the title above the controls.
    let controls = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let language_names = KEYBOARD_LANGUAGES
        .iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    let language = gtk::DropDown::from_strings(&language_names);
    let language_index = KEYBOARD_LANGUAGES
        .iter()
        .position(|entry| {
            entry
                .layouts
                .iter()
                .any(|(_, code)| *code == keyboard.layout)
        })
        .unwrap_or(0);
    language.set_selected(language_index as u32);
    let layout = gtk::DropDown::new(None::<gtk::StringList>, None::<gtk::Expression>);
    install_layouts(&layout, language_index, &keyboard.layout);
    let model_names = KEYBOARD_MODELS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();
    let hardware = gtk::DropDown::from_strings(&model_names);
    hardware.set_selected(
        KEYBOARD_MODELS
            .iter()
            .position(|(_, model)| *model == keyboard.model)
            .unwrap_or(0) as u32,
    );
    accessible(
        &language,
        "Keyboard language",
        "Choose a keyboard language.",
    );
    accessible(&layout, "Keyboard layout", "Choose a keyboard layout.");
    accessible(
        &hardware,
        "Keyboard hardware",
        "Choose the physical key arrangement. Generic 105-key PC is correct for most keyboards.",
    );
    controls.append(&setting_row("Language", "Keyboard language.", &language));
    controls.append(&setting_row("Layout", "Keyboard key arrangement.", &layout));
    controls.append(&setting_row(
        "Hardware",
        "Generic 105-key PC is correct for most keyboards.",
        &hardware,
    ));
    contents.append(&controls);

    let socket = keyboard_settings_socket_path()
        .and_then(|path| {
            validate_keyboard_settings_socket(&path)?;
            Ok(path)
        })
        .ok();
    let available = store.borrow().writable && socket.is_some();
    language.set_sensitive(available);
    layout.set_sensitive(available);
    hardware.set_sensitive(available);
    let applying = Rc::new(Cell::new(false));
    let apply: Rc<dyn Fn()> = Rc::new({
        let window = window.clone();
        let language = language.clone();
        let layout = layout.clone();
        let hardware = hardware.clone();
        let store = Rc::clone(&store);
        let bus = Rc::clone(&bus);
        let applying = Rc::clone(&applying);
        move || {
            if applying.get() {
                return;
            }
            let Some(socket) = socket.as_ref() else {
                return;
            };
            let language_index = language.selected() as usize;
            let layout_index = layout.selected() as usize;
            let model_index = hardware.selected() as usize;
            let Some((_, layout_code)) = KEYBOARD_LANGUAGES
                .get(language_index)
                .and_then(|entry| entry.layouts.get(layout_index))
            else {
                return;
            };
            let Some((_, model_code)) = KEYBOARD_MODELS.get(model_index) else {
                return;
            };
            let requested = KeyboardSettings {
                model: (*model_code).into(),
                layout: (*layout_code).into(),
                variant: String::new(),
                options: String::new(),
            };
            if requested == store.borrow().settings.keyboard {
                return;
            }
            applying.set(true);
            let previous = store.borrow().settings.keyboard.clone();
            match set_guest_keyboard(socket, &requested) {
                Ok(_) => match store.borrow_mut().persist_confirmed_keyboard(requested) {
                    Ok(generation) => {
                        let _ = bus.emit_changed(generation, &[ChangeSection::Keyboard]);
                    }
                    Err(error) => {
                        let _ = set_guest_keyboard(socket, &previous);
                        show_error(
                            &window,
                            "Keyboard setting was not saved",
                            &error.to_string(),
                        );
                    }
                },
                Err(error) => show_error(&window, "Keyboard was not changed", &error.to_string()),
            }
            applying.set(false);
        }
    });
    {
        let layout = layout.clone();
        let apply = Rc::clone(&apply);
        language.connect_selected_notify(move |language| {
            install_layouts(&layout, language.selected() as usize, "");
            apply();
        });
    }
    {
        let apply = Rc::clone(&apply);
        layout.connect_selected_notify(move |_| apply());
    }
    hardware.connect_selected_notify(move |_| apply());
    page("Keyboard", &contents)
}

fn install_layouts(dropdown: &gtk::DropDown, language_index: usize, selected_code: &str) {
    let Some(language) = KEYBOARD_LANGUAGES.get(language_index) else {
        return;
    };
    let names = language
        .layouts
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();
    let model = gtk::StringList::new(&names);
    dropdown.set_model(Some(&model));
    dropdown.set_selected(
        language
            .layouts
            .iter()
            .position(|(_, code)| *code == selected_code)
            .unwrap_or(0) as u32,
    );
}

fn build_time_location_page(window: &gtk::ApplicationWindow) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);

    let automatic = gtk::Switch::new();
    automatic.set_active(true);
    automatic.set_sensitive(false);
    accessible(
        &automatic,
        "Automatic date and time",
        "The system clock is kept up to date automatically.",
    );
    contents.append(&setting_row(
        "Automatic date and time",
        "The system clock is kept up to date automatically.",
        &automatic,
    ));

    let current_time = gtk::Label::new(Some(&formatted_local_time()));
    current_time.set_xalign(1.0);
    current_time.add_css_class("heading");
    accessible(
        &current_time,
        "Current local date and time",
        "Current guest-local date and time in the selected time zone.",
    );
    contents.append(&setting_row(
        "Date and time",
        "Shown using the selected time-zone location.",
        &current_time,
    ));
    {
        let current_time = current_time.clone();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            current_time.set_label(&formatted_local_time());
            glib::ControlFlow::Continue
        });
    }

    let zones = Rc::new(
        load_time_zones(Path::new(ZONE_TAB_PATH)).unwrap_or_else(|error| {
            eprintln!("buzzardos-settings: cannot load time-zone locations: {error}");
            vec!["Etc/UTC".to_owned()]
        }),
    );
    let zone_labels = zones.iter().map(String::as_str).collect::<Vec<_>>();
    let time_zone = gtk::DropDown::from_strings(&zone_labels);
    time_zone.set_enable_search(true);
    let selected = current_time_zone()
        .ok()
        .and_then(|current| zones.iter().position(|zone| zone == &current))
        .unwrap_or_else(|| zones.iter().position(|zone| zone == "Etc/UTC").unwrap_or(0));
    time_zone.set_selected(selected as u32);
    accessible(
        &time_zone,
        "Time zone",
        "Search IANA time-zone locations and choose the guest-local time zone.",
    );
    contents.append(&setting_row(
        "Time zone",
        "Choose a searchable city/region location for local time.",
        &time_zone,
    ));

    let changing = Rc::new(Cell::new(false));
    let confirmed = Rc::new(Cell::new(selected as u32));
    {
        let zones = Rc::clone(&zones);
        let changing = Rc::clone(&changing);
        let confirmed = Rc::clone(&confirmed);
        let window = window.clone();
        time_zone.connect_selected_notify(move |dropdown| {
            if changing.get() {
                return;
            }
            let selected = dropdown.selected();
            let Some(zone) = zones.get(selected as usize) else {
                return;
            };
            dropdown.set_sensitive(false);
            let zone = zone.clone();
            let dropdown = dropdown.clone();
            let changing = Rc::clone(&changing);
            let confirmed = Rc::clone(&confirmed);
            let window_for_result = window.clone();
            if sudo_runs_without_password() {
                dropdown.set_sensitive(true);
                match set_time_zone(&zone, None) {
                    Ok(()) => confirmed.set(selected),
                    Err(error) => {
                        changing.set(true);
                        dropdown.set_selected(confirmed.get());
                        changing.set(false);
                        show_error(&window_for_result, "Time zone was not changed", &error);
                    }
                }
                return;
            }
            request_machine_password(
                &window,
                "Enter this machine's password to change the time zone.",
                move |password| {
                    dropdown.set_sensitive(true);
                    let Some(mut password) = password else {
                        changing.set(true);
                        dropdown.set_selected(confirmed.get());
                        changing.set(false);
                        return;
                    };
                    let result = set_time_zone(&zone, Some(&password));
                    password.zeroize();
                    match result {
                        Ok(()) => confirmed.set(selected),
                        Err(error) => {
                            changing.set(true);
                            dropdown.set_selected(confirmed.get());
                            changing.set(false);
                            show_error(&window_for_result, "Time zone was not changed", &error);
                        }
                    }
                },
            );
        });
    }

    page("Time & Location", &contents)
}

fn formatted_local_time() -> String {
    glib::DateTime::now_local()
        .and_then(|value| value.format("%A, %e %B %Y, %H:%M:%S"))
        .map(|value| value.to_string())
        .unwrap_or_else(|_| "Local time unavailable".to_owned())
}

fn valid_time_zone_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.starts_with('/')
        && value.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+'))
        })
}

fn parse_time_zones(contents: &str) -> Vec<String> {
    let mut zones = contents
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split('\t').nth(2))
        .filter(|zone| valid_time_zone_name(zone))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    zones.push("Etc/UTC".to_owned());
    zones.sort();
    zones.dedup();
    zones
}

fn load_time_zones(path: &Path) -> Result<Vec<String>, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_ZONE_TAB_BYTES {
        return Err("the installed IANA zone table is not a bounded regular file".to_owned());
    }
    let contents = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let zones = parse_time_zones(&contents);
    if zones.is_empty() {
        return Err("the installed IANA zone table contains no usable locations".to_owned());
    }
    Ok(zones)
}

fn current_time_zone() -> Result<String, String> {
    let output = Command::new("/usr/bin/timedatectl")
        .args(["show", "--property=Timezone", "--value"])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let value = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
    let value = value.trim();
    if !valid_time_zone_name(value) {
        return Err("timedatectl returned an invalid time-zone name".to_owned());
    }
    Ok(value.to_owned())
}

fn set_time_zone(zone: &str, password: Option<&[u8]>) -> Result<(), String> {
    if !valid_time_zone_name(zone) {
        return Err("the selected time-zone name is invalid".to_owned());
    }
    let zone_path = Path::new(ZONEINFO_ROOT).join(zone);
    if !zone_path.exists() {
        return Err("the selected time zone is not installed".to_owned());
    }
    let mut input = Vec::new();
    let arguments = if let Some(password) = password {
        input.extend_from_slice(password);
        input.push(b'\n');
        vec![
            "-k",
            "-S",
            "-p",
            "",
            "--",
            "/usr/bin/timedatectl",
            "set-timezone",
            zone,
        ]
    } else {
        vec![
            "-k",
            "-n",
            "--",
            "/usr/bin/timedatectl",
            "set-timezone",
            zone,
        ]
    };
    if let Err(error) = run_guest_sudo(&arguments, &mut input) {
        return Err(if error.is_empty() {
            "timedatectl rejected the selected time zone".to_owned()
        } else {
            error
        });
    }
    Ok(())
}

fn request_machine_password(
    parent: &gtk::ApplicationWindow,
    explanation_text: &str,
    callback: impl FnOnce(Option<Vec<u8>>) + 'static,
) {
    let dialog = gtk::Window::builder()
        .title("Authenticate")
        .transient_for(parent)
        .modal(true)
        .resizable(false)
        .default_width(420)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    let explanation = wrapped_label(explanation_text);
    let password = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .placeholder_text("Password")
        .activates_default(true)
        .build();
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let authenticate = gtk::Button::with_label("Authenticate");
    authenticate.add_css_class("suggested-action");
    actions.append(&cancel);
    actions.append(&authenticate);
    content.append(&explanation);
    content.append(&password);
    content.append(&actions);
    dialog.set_default_widget(Some(&authenticate));
    dialog.set_child(Some(&content));

    let callback = Rc::new(RefCell::new(Some(callback)));
    let callback_for_cancel = Rc::clone(&callback);
    let dialog_for_cancel = dialog.clone();
    cancel.connect_clicked(move |_| {
        if let Some(callback) = callback_for_cancel.borrow_mut().take() {
            callback(None);
        }
        dialog_for_cancel.close();
    });
    let callback_for_close = Rc::clone(&callback);
    dialog.connect_close_request(move |_| {
        if let Some(callback) = callback_for_close.borrow_mut().take() {
            callback(None);
        }
        glib::Propagation::Proceed
    });
    let callback_for_accept = Rc::clone(&callback);
    let dialog_for_accept = dialog.clone();
    let password_for_accept = password.clone();
    authenticate.connect_clicked(move |_| {
        let mut value = password_for_accept.text().to_string();
        if value.is_empty() || value.len() > MAX_PASSWORD_BYTES {
            value.zeroize();
            return;
        }
        let bytes = value.as_bytes().to_vec();
        value.zeroize();
        password_for_accept.set_text("");
        if let Some(callback) = callback_for_accept.borrow_mut().take() {
            callback(Some(bytes));
        }
        dialog_for_accept.close();
    });
    dialog.present();
    password.grab_focus();
}

struct PasswordChangeRequest {
    current: Option<Vec<u8>>,
    new: Vec<u8>,
}

impl Drop for PasswordChangeRequest {
    fn drop(&mut self) {
        if let Some(current) = self.current.as_mut() {
            current.zeroize();
        }
        self.new.zeroize();
    }
}

fn build_security_page(window: &gtk::ApplicationWindow) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);

    let password_section = section("Password");
    let change_password = gtk::Button::with_label("Change…");
    accessible(
        &change_password,
        "Change password",
        "Change the password for the user account in this machine.",
    );
    password_section.append(&setting_row(
        "Machine password",
        "Changes the password for the user account inside this machine.",
        &change_password,
    ));
    contents.append(&password_section);

    let sudo_section = section("Administrator access");
    let passwordless = gtk::Switch::new();
    let initial_passwordless = passwordless_sudo_enabled();
    passwordless.set_active(initial_passwordless.as_ref().copied().unwrap_or(false));
    passwordless.set_sensitive(initial_passwordless.is_ok());
    accessible(
        &passwordless,
        "Passwordless sudo",
        "Allow commands inside this machine to use sudo without entering the machine password.",
    );
    sudo_section.append(&setting_row(
        "Passwordless sudo",
        "Convenient for automated development, but any program running as this user can then become root inside the machine.",
        &passwordless,
    ));
    if let Err(error) = initial_passwordless {
        let warning = wrapped_label(&format!(
            "This control is unavailable because the installed sudo policy could not be verified: {error}"
        ));
        warning.add_css_class("error");
        sudo_section.append(&warning);
    }
    contents.append(&sudo_section);

    {
        let window = window.clone();
        change_password.connect_clicked(move |_| {
            let passwordless = sudo_runs_without_password();
            let window_for_result = window.clone();
            request_password_change(&window, passwordless, move |request| {
                let Some(request) = request else {
                    return;
                };
                match change_machine_password(&request, passwordless) {
                    Ok(()) => show_info(
                        &window_for_result,
                        "Password changed",
                        "The password for user was changed inside this machine.",
                    ),
                    Err(error) => {
                        show_error(&window_for_result, "Password could not be changed", &error)
                    }
                }
            });
        });
    }

    let changing = Rc::new(Cell::new(false));
    let confirmed = Rc::new(Cell::new(passwordless.is_active()));
    {
        let window = window.clone();
        let changing = Rc::clone(&changing);
        let confirmed = Rc::clone(&confirmed);
        passwordless.connect_active_notify(move |control| {
            if changing.get() {
                return;
            }
            let requested = control.is_active();
            control.set_sensitive(false);
            if requested {
                let control = control.clone();
                let window_for_result = window.clone();
                let changing = Rc::clone(&changing);
                let confirmed = Rc::clone(&confirmed);
                request_machine_password(
                    &window,
                    "Enter this machine's password to enable passwordless sudo.",
                    move |password| {
                        control.set_sensitive(true);
                        let Some(mut password) = password else {
                            changing.set(true);
                            control.set_active(confirmed.get());
                            changing.set(false);
                            return;
                        };
                        let result = set_passwordless_sudo(true, Some(&password));
                        password.zeroize();
                        match result {
                            Ok(()) => confirmed.set(true),
                            Err(error) => {
                                changing.set(true);
                                control.set_active(confirmed.get());
                                changing.set(false);
                                show_error(
                                    &window_for_result,
                                    "Passwordless sudo was not enabled",
                                    &error,
                                );
                            }
                        }
                    },
                );
            } else {
                let result = set_passwordless_sudo(false, None);
                control.set_sensitive(true);
                match result {
                    Ok(()) => confirmed.set(false),
                    Err(error) => {
                        changing.set(true);
                        control.set_active(confirmed.get());
                        changing.set(false);
                        show_error(&window, "Passwordless sudo was not disabled", &error);
                    }
                }
            }
        });
    }

    page("Security", &contents)
}

fn request_password_change(
    parent: &gtk::ApplicationWindow,
    passwordless: bool,
    callback: impl FnOnce(Option<PasswordChangeRequest>) + 'static,
) {
    let dialog = gtk::Window::builder()
        .title("Change Password")
        .transient_for(parent)
        .modal(true)
        .resizable(false)
        .default_width(440)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    set_margins(&content, 18);
    content.append(&wrapped_label(if passwordless {
        "Set a new password for user. Passwordless sudo is enabled, so the current password is not required."
    } else {
        "Enter the current machine password, then choose a new password for user."
    }));
    let current = (!passwordless).then(|| {
        gtk::PasswordEntry::builder()
            .show_peek_icon(true)
            .placeholder_text("Current password")
            .build()
    });
    if let Some(current) = current.as_ref() {
        content.append(current);
    }
    let new = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .placeholder_text("New password")
        .build();
    let confirm = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .placeholder_text("Confirm new password")
        .activates_default(true)
        .build();
    content.append(&new);
    content.append(&confirm);
    let validation = wrapped_label("");
    validation.add_css_class("error");
    validation.set_visible(false);
    content.append(&validation);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let apply = gtk::Button::with_label("Change Password");
    apply.add_css_class("suggested-action");
    actions.append(&cancel);
    actions.append(&apply);
    content.append(&actions);
    dialog.set_default_widget(Some(&apply));
    dialog.set_child(Some(&content));

    let callback = Rc::new(RefCell::new(Some(callback)));
    let callback_for_cancel = Rc::clone(&callback);
    let dialog_for_cancel = dialog.clone();
    cancel.connect_clicked(move |_| {
        if let Some(callback) = callback_for_cancel.borrow_mut().take() {
            callback(None);
        }
        dialog_for_cancel.close();
    });
    let callback_for_close = Rc::clone(&callback);
    dialog.connect_close_request(move |_| {
        if let Some(callback) = callback_for_close.borrow_mut().take() {
            callback(None);
        }
        glib::Propagation::Proceed
    });
    let callback_for_apply = Rc::clone(&callback);
    let dialog_for_apply = dialog.clone();
    let current_for_apply = current.clone();
    let new_for_apply = new.clone();
    let confirm_for_apply = confirm.clone();
    let validation_for_apply = validation.clone();
    apply.connect_clicked(move |_| {
        let mut current_text = current_for_apply
            .as_ref()
            .map(|entry| entry.text().to_string());
        let mut new_text = new_for_apply.text().to_string();
        let mut confirm_text = confirm_for_apply.text().to_string();
        let invalid_current = current_text
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > MAX_PASSWORD_BYTES);
        let invalid_new = new_text.is_empty()
            || new_text.len() > MAX_PASSWORD_BYTES
            || new_text.as_bytes().contains(&b'\n');
        if invalid_current || invalid_new || new_text != confirm_text {
            validation_for_apply.set_label(if new_text != confirm_text {
                "The new passwords do not match."
            } else if invalid_current {
                "Enter the current machine password."
            } else {
                "Enter a new password between 1 and 4096 bytes without a line break."
            });
            validation_for_apply.set_visible(true);
            current_text.as_mut().map(Zeroize::zeroize);
            new_text.zeroize();
            confirm_text.zeroize();
            return;
        }
        let request = PasswordChangeRequest {
            current: current_text.as_ref().map(|value| value.as_bytes().to_vec()),
            new: new_text.as_bytes().to_vec(),
        };
        current_text.as_mut().map(Zeroize::zeroize);
        new_text.zeroize();
        confirm_text.zeroize();
        if let Some(current) = current_for_apply.as_ref() {
            current.set_text("");
        }
        new_for_apply.set_text("");
        confirm_for_apply.set_text("");
        if let Some(callback) = callback_for_apply.borrow_mut().take() {
            callback(Some(request));
        }
        dialog_for_apply.close();
    });
    dialog.present();
    if let Some(current) = current.as_ref() {
        current.grab_focus();
    } else {
        new.grab_focus();
    }
}

fn passwordless_sudo_enabled() -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(PASSWORDLESS_POLICY) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != 0
        || metadata.gid() != 0
        || metadata.mode() & 0o777 != 0o440
        || metadata.len() != PASSWORDLESS_POLICY_CONTENT.len() as u64
    {
        return Err("the passwordless-sudo policy is not a trusted root-owned file".to_owned());
    }
    let output = Command::new(GUEST_SUDO)
        .args(["-k", "-n", "--", SUDO_POLICY_HELPER, "status-passwordless"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() {
            format!("sudo exited with {}", output.status)
        } else {
            detail
        });
    }
    match output.stdout.as_slice() {
        b"enabled\n" => Ok(true),
        _ => Err("the sudo policy helper returned an invalid status".to_owned()),
    }
}

fn sudo_runs_without_password() -> bool {
    Command::new(GUEST_SUDO)
        .args(["-k", "-n", "--", "/usr/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn change_machine_password(
    request: &PasswordChangeRequest,
    passwordless: bool,
) -> Result<(), String> {
    let mut input = Vec::new();
    let arguments = if passwordless {
        vec!["-k", "-n", "--", CHPASSWD]
    } else {
        let current = request
            .current
            .as_ref()
            .ok_or_else(|| "the current machine password is required".to_owned())?;
        input.extend_from_slice(current);
        input.push(b'\n');
        vec!["-k", "-S", "-p", "", "--", CHPASSWD]
    };
    input.extend_from_slice(INTERACTIVE_USER.as_bytes());
    input.push(b':');
    input.extend_from_slice(&request.new);
    input.push(b'\n');
    run_guest_sudo(&arguments, &mut input)
}

fn set_passwordless_sudo(enable: bool, password: Option<&[u8]>) -> Result<(), String> {
    let mut input = Vec::new();
    let action = if enable {
        let password = password.ok_or_else(|| "the machine password is required".to_owned())?;
        input.extend_from_slice(password);
        input.push(b'\n');
        "enable-passwordless"
    } else {
        "disable-passwordless"
    };
    let arguments = if enable {
        vec!["-k", "-S", "-p", "", "--", SUDO_POLICY_HELPER, action]
    } else {
        vec!["-k", "-n", "--", SUDO_POLICY_HELPER, action]
    };
    run_guest_sudo(&arguments, &mut input)
}

fn run_guest_sudo(arguments: &[&str], input: &mut Vec<u8>) -> Result<(), String> {
    let spawned = Command::new(GUEST_SUDO)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            input.zeroize();
            return Err(error.to_string());
        }
    };
    let write_result = match child.stdin.take() {
        Some(mut standard_input) => standard_input
            .write_all(input)
            .map_err(|error| error.to_string()),
        None => Err("sudo did not open its standard input".to_owned()),
    };
    input.zeroize();
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if detail.is_empty() {
        format!("sudo exited with {}", output.status)
    } else {
        detail
    })
}

fn build_appearance_page(
    window: &gtk::ApplicationWindow,
    store: Rc<RefCell<SettingsStore>>,
    bus: Rc<ChangeBus>,
    page_background: &gtk::DrawingArea,
) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);
    let theme_section = section("Theme");
    let theme_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let light = gtk::CheckButton::with_label("Light");
    let dark = gtk::CheckButton::with_label("Dark");
    dark.set_group(Some(&light));
    let current_theme = store.borrow().settings.appearance.theme;
    light.set_active(current_theme == ThemeMode::Light);
    dark.set_active(current_theme == ThemeMode::Dark);
    accessible(&light, "Light theme", "Use the light desktop theme.");
    accessible(&dark, "Dark theme", "Use the dark desktop theme.");
    theme_buttons.append(&light);
    theme_buttons.append(&dark);
    theme_section.append(&theme_buttons);
    contents.append(&theme_section);

    let background_section = section("Background colour");
    let dialog = gtk::ColorDialog::builder()
        .title("Choose Background Colour")
        .modal(true)
        .with_alpha(false)
        .build();
    let colour = gtk::ColorDialogButton::new(Some(dialog));
    let initial = store.borrow().settings.appearance.background.solid_color();
    colour.set_rgba(&solid_to_rgba(initial));
    accessible(
        &colour,
        "Desktop background colour",
        "Choose a solid desktop background colour.",
    );
    background_section.append(&setting_row(
        "Colour",
        "Choose any solid desktop background colour.",
        &colour,
    ));
    contents.append(&background_section);

    let taskbar_section = section("Taskbar");
    let capped = gtk::Switch::new();
    capped.set_active(store.borrow().settings.appearance.capped_task_buttons);
    accessible(
        &capped,
        "Capped task buttons",
        "Keep task buttons at or below 260 pixels and page overflowing windows five at a time.",
    );
    taskbar_section.append(&setting_row(
        "Capped task buttons",
        "Keep buttons compact. The < and > controls appear only when minimum-width buttons no longer fit.",
        &capped,
    ));
    contents.append(&taskbar_section);

    let writable = store.borrow().writable;
    light.set_sensitive(writable);
    dark.set_sensitive(writable);
    colour.set_sensitive(writable);
    capped.set_sensitive(writable);
    let changing = Rc::new(Cell::new(false));
    for (button, mode, background, color) in [
        (
            &light,
            ThemeMode::Light,
            BackgroundChoice::LightPlain,
            LIGHT_WALLPAPER,
        ),
        (
            &dark,
            ThemeMode::Dark,
            BackgroundChoice::DarkPlain,
            DARK_WALLPAPER,
        ),
    ] {
        let window = window.clone();
        let store = Rc::clone(&store);
        let bus = Rc::clone(&bus);
        let changing = Rc::clone(&changing);
        let colour = colour.clone();
        let page_background = page_background.clone();
        button.connect_toggled(move |button| {
            if !button.is_active() || changing.get() {
                return;
            }
            changing.set(true);
            match store.borrow_mut().set_appearance(mode, background) {
                Ok(generation) => {
                    colour.set_rgba(&solid_to_rgba(color));
                    apply_current_process_theme(mode);
                    page_background.queue_draw();
                    let _ = bus.emit_changed(generation, &[ChangeSection::Appearance]);
                }
                Err(error) => show_error(&window, "Appearance was not changed", &error.to_string()),
            }
            changing.set(false);
        });
    }
    {
        let window = window.clone();
        let store = Rc::clone(&store);
        let bus = Rc::clone(&bus);
        let changing = Rc::clone(&changing);
        colour.connect_rgba_notify(move |button| {
            if changing.get() {
                return;
            }
            let color = rgba_to_solid(button.rgba());
            match store
                .borrow_mut()
                .set_background(BackgroundChoice::CustomSolid { color })
            {
                Ok(generation) => {
                    let _ = bus.emit_changed(generation, &[ChangeSection::Appearance]);
                }
                Err(error) => show_error(&window, "Background was not changed", &error.to_string()),
            }
        });
    }
    {
        let window = window.clone();
        let store = Rc::clone(&store);
        let bus = Rc::clone(&bus);
        let changing = Rc::clone(&changing);
        capped.connect_active_notify(move |button| {
            if changing.get() {
                return;
            }
            let requested = button.is_active();
            match store.borrow_mut().set_capped_task_buttons(requested) {
                Ok(generation) => {
                    let _ = bus.emit_changed(generation, &[ChangeSection::Appearance]);
                }
                Err(error) => {
                    changing.set(true);
                    button.set_active(!requested);
                    changing.set(false);
                    show_error(&window, "Taskbar was not changed", &error.to_string());
                }
            }
        });
    }
    page("Appearance", &contents)
}

fn build_updates_page(_window: &gtk::ApplicationWindow) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let heading = gtk::Label::new(Some("Automatic software updates"));
    heading.set_xalign(0.0);
    heading.add_css_class("heading");
    accessible(
        &heading,
        "Automatic software updates",
        "Buzzard OS uses the guest distribution's standard APT update system.",
    );
    contents.append(&heading);

    let explanation = wrapped_label(
        "This machine uses Debian's standard APT and unattended-upgrades configuration. \
         There is no Buzzard OS updater service. Package updates are applied by the normal \
         guest operating-system update mechanism; use apt in Foot when manual control is needed.",
    );
    contents.append(&explanation);
    page("Updates", &contents)
}

#[cfg(any())]
#[derive(Clone)]
struct UpdateWidgets {
    status: gtk::Label,
    packages: gtk::ListBox,
    check: gtk::Button,
    install: gtk::Button,
    progress: gtk::ProgressBar,
    progress_detail: gtk::Label,
    download_rate: Rc<RefCell<DownloadRate>>,
}

#[cfg(any())]
#[derive(Debug, Default)]
struct DownloadRate {
    previous: Option<(u64, Instant)>,
    bytes_per_second: Option<f64>,
}

#[cfg(any())]
impl DownloadRate {
    fn observe(&mut self, progress: Option<&UpdateProgress>, now: Instant) -> Option<f64> {
        let Some(progress) = progress.filter(|value| {
            value.phase == UpdateProgressPhase::Downloading
                && value.unit == UpdateProgressUnit::Bytes
        }) else {
            self.previous = None;
            self.bytes_per_second = None;
            return None;
        };
        if let Some((previous_bytes, previous_at)) = self.previous {
            if progress.completed < previous_bytes {
                self.bytes_per_second = None;
            } else if progress.completed > previous_bytes {
                let elapsed = now.saturating_duration_since(previous_at).as_secs_f64();
                if elapsed > 0.0 {
                    let measured = (progress.completed - previous_bytes) as f64 / elapsed;
                    self.bytes_per_second = Some(match self.bytes_per_second {
                        Some(previous) => previous * 0.65 + measured * 0.35,
                        None => measured,
                    });
                }
            }
        }
        self.previous = Some((progress.completed, now));
        self.bytes_per_second
    }
}

#[cfg(any())]
fn build_legacy_updates_page(window: &gtk::ApplicationWindow) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 16);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let check = gtk::Button::with_label("Check for updates");
    check.add_css_class("wb-primary-action");
    let status = gtk::Label::new(Some("Not checked"));
    status.set_xalign(0.0);
    status.set_hexpand(true);
    status.set_wrap(true);
    accessible(
        &check,
        "Check for updates",
        "Check Debian repositories for package updates.",
    );
    actions.append(&check);
    actions.append(&status);
    contents.append(&actions);

    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);
    progress.set_hexpand(true);
    progress.set_visible(false);
    accessible(
        &progress,
        "Update progress",
        "Current Debian update download or installation progress.",
    );
    let progress_detail = gtk::Label::new(None);
    progress_detail.set_xalign(0.0);
    progress_detail.set_wrap(true);
    progress_detail.add_css_class("dim-label");
    progress_detail.set_visible(false);
    contents.append(&progress);
    contents.append(&progress_detail);

    let list_heading = gtk::Label::new(Some("Available updates"));
    list_heading.set_xalign(0.0);
    list_heading.add_css_class("heading");
    contents.append(&list_heading);
    let packages = gtk::ListBox::new();
    packages.set_selection_mode(gtk::SelectionMode::None);
    packages.add_css_class("boxed-list");
    accessible(
        &packages,
        "Available updates",
        "Scrollable list of available Debian package updates.",
    );
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(260)
        .vexpand(true)
        .child(&packages)
        .build();
    contents.append(&list_scroll);
    let install = gtk::Button::with_label("Install now");
    install.add_css_class("wb-primary-action");
    install.set_halign(gtk::Align::Start);
    accessible(
        &install,
        "Install now",
        "Install the available Debian package updates.",
    );
    contents.append(&install);

    let widgets = UpdateWidgets {
        status,
        packages,
        check: check.clone(),
        install: install.clone(),
        progress,
        progress_detail,
        download_rate: Rc::new(RefCell::new(DownloadRate::default())),
    };
    let initial = load_update_view(Path::new(UPDATE_STATE_PATH));
    let state = Rc::new(RefCell::new(initial.state));
    render_updates(&widgets, &state.borrow());
    updater_client::get_state({
        let state = Rc::clone(&state);
        let widgets = widgets.clone();
        move |result| {
            if let Ok(remote) = result {
                state.replace(remote);
                render_updates(&widgets, &state.borrow());
            }
        }
    });
    {
        let state = Rc::clone(&state);
        let widgets = widgets.clone();
        let window = window.clone();
        check.connect_clicked(move |_| {
            let baseline = state.borrow().state_generation;
            widgets.status.set_label("Checking…");
            widgets.check.set_sensitive(false);
            let state = Rc::clone(&state);
            let widgets = widgets.clone();
            let window = window.clone();
            updater_client::submit(UpdateRequest::Check, move |result| match result {
                Ok(_) => poll_updates(state, widgets, baseline),
                Err(error) => {
                    widgets.check.set_sensitive(true);
                    widgets.status.set_label("Check failed");
                    show_error(&window, "Could not check for updates", &error);
                }
            });
        });
    }
    {
        let state = Rc::clone(&state);
        let widgets = widgets.clone();
        let window = window.clone();
        install.connect_clicked(move |_| {
            let Some(generation) = state.borrow().plan_generation.clone() else {
                return;
            };
            let baseline = state.borrow().state_generation;
            widgets.status.set_label("Installing…");
            widgets.install.set_sensitive(false);
            let state = Rc::clone(&state);
            let widgets = widgets.clone();
            let window = window.clone();
            updater_client::submit(UpdateRequest::InstallPlan(generation), move |result| {
                match result {
                    Ok(_) => poll_updates(state, widgets, baseline),
                    Err(error) => {
                        widgets.install.set_sensitive(true);
                        widgets.status.set_label("Install failed");
                        show_error(&window, "Could not install updates", &error);
                    }
                }
            });
        });
    }
    page("Updates", &contents)
}

#[cfg(any())]
fn poll_updates(state: Rc<RefCell<UpdateState>>, widgets: UpdateWidgets, baseline: u64) {
    let attempts = Rc::new(Cell::new(0_u16));
    glib::timeout_add_local(Duration::from_millis(350), move || {
        attempts.set(attempts.get().saturating_add(1));
        let view = load_update_view(Path::new(UPDATE_STATE_PATH));
        state.replace(view.state);
        render_updates(&widgets, &state.borrow());
        let current = state.borrow();
        let active = matches!(
            current.status,
            UpdateStatus::Checking | UpdateStatus::Installing
        );
        if attempts.get() >= 1_200 || (!active && current.state_generation > baseline) {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

#[cfg(any())]
fn render_updates(widgets: &UpdateWidgets, state: &UpdateState) {
    let status = match state.status {
        UpdateStatus::NeverChecked => "Not checked".to_owned(),
        UpdateStatus::Checking => "Checking…".to_owned(),
        UpdateStatus::UpToDate => "Complete — system is up to date".to_owned(),
        UpdateStatus::Available => "Updates available".to_owned(),
        UpdateStatus::Installing => "Installing updates…".to_owned(),
        UpdateStatus::Failed => state
            .failure
            .as_deref()
            .map(|failure| format!("Failed — {failure}"))
            .unwrap_or_else(|| "Failed".to_owned()),
        UpdateStatus::RestartRecommended => "Complete — restart recommended".to_owned(),
    };
    widgets.status.set_label(&status);

    let speed = widgets
        .download_rate
        .borrow_mut()
        .observe(state.progress.as_ref(), Instant::now());
    if let Some(progress) = state.progress.as_ref() {
        let fraction = if progress.total == 0 {
            0.0
        } else {
            progress.completed as f64 / progress.total as f64
        };
        let percent = (fraction * 100.0).clamp(0.0, 100.0).round() as u64;
        widgets.progress.set_fraction(fraction.clamp(0.0, 1.0));
        widgets.progress.set_text(Some(&format!("{percent}%")));
        widgets.progress.set_visible(true);
        widgets
            .progress_detail
            .set_label(&progress_description(progress, speed));
        widgets.progress_detail.set_visible(true);
    } else {
        widgets.progress.set_visible(false);
        widgets.progress_detail.set_visible(false);
    }
    while let Some(child) = widgets.packages.first_child() {
        if let Ok(row) = child.downcast::<gtk::ListBoxRow>() {
            widgets.packages.remove(&row);
        } else {
            break;
        }
    }
    if state.packages.is_empty() {
        let row = gtk::ListBoxRow::new();
        row.set_selectable(false);
        let label = gtk::Label::new(Some(if state.status == UpdateStatus::UpToDate {
            "No updates available"
        } else {
            "Check for updates to see available packages"
        }));
        label.set_xalign(0.0);
        set_margins(&label, 12);
        row.set_child(Some(&label));
        widgets.packages.append(&row);
    } else {
        for package in &state.packages {
            let row = gtk::ListBoxRow::new();
            row.set_selectable(false);
            let box_ = gtk::Box::new(gtk::Orientation::Vertical, 3);
            set_margins(&box_, 10);
            let name = gtk::Label::new(Some(&package.name));
            name.set_xalign(0.0);
            name.add_css_class("heading");
            let versions = gtk::Label::new(Some(&format!(
                "{} → {} · {}",
                package.installed_version,
                package.candidate_version,
                format_bytes(package.download_size)
            )));
            versions.set_xalign(0.0);
            versions.add_css_class("dim-label");
            box_.append(&name);
            box_.append(&versions);
            if state
                .progress
                .as_ref()
                .is_some_and(|progress| progress.phase == UpdateProgressPhase::Installing)
                && state
                    .progress
                    .as_ref()
                    .and_then(|progress| progress.detail.as_deref())
                    .is_some_and(|detail| detail.starts_with(&format!("{}:", package.name)))
            {
                let current = gtk::Label::new(Some("Installing now"));
                current.set_xalign(0.0);
                current.add_css_class("accent");
                box_.append(&current);
            }
            row.set_child(Some(&box_));
            widgets.packages.append(&row);
        }
    }
    let active = matches!(
        state.status,
        UpdateStatus::Checking | UpdateStatus::Installing
    );
    widgets.check.set_sensitive(!active);
    widgets.install.set_sensitive(
        !active
            && state.status == UpdateStatus::Available
            && state.plan_generation.is_some()
            && state.runtime_ready,
    );
}

#[cfg(any())]
fn progress_description(progress: &UpdateProgress, speed: Option<f64>) -> String {
    let detail = progress.detail.as_deref().unwrap_or_default();
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(" — {detail}")
    };
    match progress.phase {
        UpdateProgressPhase::Downloading => {
            let mut text = format!(
                "Downloading — {} of {}",
                format_bytes(progress.completed),
                format_bytes(progress.total)
            );
            if let Some(speed) = speed.filter(|value| value.is_finite() && *value >= 0.0) {
                text.push_str(&format!(" — {}/s", format_bytes(speed.round() as u64)));
            }
            if !detail.is_empty() {
                text.push_str(&format!(" — {detail}"));
            }
            text
        }
        UpdateProgressPhase::Installing => format!(
            "Installing — {} of {} packages{}",
            progress.completed, progress.total, suffix
        ),
        UpdateProgressPhase::Repairing => format!(
            "Repairing — {} of {} packages{}",
            progress.completed, progress.total, suffix
        ),
        UpdateProgressPhase::Refreshing => {
            format!("Refreshing package information{suffix}")
        }
        UpdateProgressPhase::Resolving => format!("Resolving available updates{suffix}"),
    }
}

fn page(title: &str, contents: &gtk::Box) -> gtk::ScrolledWindow {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.add_css_class("wb-settings-page-content");
    set_margins(&content, PAGE_MARGIN);
    content.append(&heading(title));
    content.append(contents);
    let page = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&content)
        .build();
    page.add_css_class("wb-settings-page");
    page
}

fn section(title: &str) -> gtk::Box {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 8);
    section.add_css_class("wb-settings-section");
    let heading = gtk::Label::new(Some(title));
    heading.set_xalign(0.0);
    heading.add_css_class("heading");
    section.append(&heading);
    section
}

fn setting_row<W: IsA<gtk::Widget>>(title: &str, description: &str, control: &W) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, ROW_SPACING);
    row.add_css_class("wb-settings-row");
    row.set_margin_top(4);
    row.set_margin_bottom(4);
    let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
    labels.set_hexpand(true);
    let title = gtk::Label::new(Some(title));
    title.set_xalign(0.0);
    let description = wrapped_label(description);
    description.add_css_class("dim-label");
    labels.append(&title);
    labels.append(&description);
    row.append(&labels);
    control.set_valign(gtk::Align::Center);
    row.append(control);
    row
}

fn heading(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("title-1");
    label
}

fn wrapped_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label
}

fn accessible<W: IsA<gtk::Accessible>>(widget: &W, label: &str, description: &str) {
    widget.update_property(&[
        gtk::accessible::Property::Label(label),
        gtk::accessible::Property::Description(description),
    ]);
}

fn set_margins<W: IsA<gtk::Widget>>(widget: &W, margin: i32) {
    widget.set_margin_top(margin);
    widget.set_margin_bottom(margin);
    widget.set_margin_start(margin);
    widget.set_margin_end(margin);
}

fn show_error(parent: &gtk::ApplicationWindow, title: &str, detail: &str) {
    gtk::AlertDialog::builder()
        .message(title)
        .detail(detail)
        .modal(true)
        .build()
        .show(Some(parent));
}

fn show_info(parent: &gtk::ApplicationWindow, title: &str, detail: &str) {
    gtk::AlertDialog::builder()
        .message(title)
        .detail(detail)
        .modal(true)
        .build()
        .show(Some(parent));
}

fn apply_current_process_theme(mode: ThemeMode) {
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_theme_name(Some(mode.gtk_theme_name()));
        // BuzzardOS-Dark is already the explicit dark theme. Asking GTK for
        // a dark *variant* of that name can fall back to Adwaita-dark, which
        // reintroduces its blue accent instead of the Cinnamon palette.
        settings.set_gtk_application_prefer_dark_theme(false);
    }
}

fn solid_to_rgba(color: SolidColor) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(color.red) / 255.0,
        f32::from(color.green) / 255.0,
        f32::from(color.blue) / 255.0,
        1.0,
    )
}

fn rgba_to_solid(color: gdk::RGBA) -> SolidColor {
    let component = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    SolidColor::new(
        component(color.red()),
        component(color.green()),
        component(color.blue()),
    )
}

#[cfg(any())]
fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.2} GiB", value / GIB)
    } else if value >= MIB {
        format!("{:.1} MiB", value / MIB)
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn settings_exposes_only_the_requested_pages() {
        assert_eq!(
            PageId::ALL.map(PageId::title),
            [
                "Display",
                "Sound",
                "Keyboard",
                "Time & Location",
                "Appearance",
                "Security",
                "Updates"
            ]
        );
    }

    #[test]
    fn accessibility_names_are_unique_and_match_the_visible_contract() {
        let names = ACCESSIBLE_CONTROL_NAMES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), ACCESSIBLE_CONTROL_NAMES.len());
        assert!(names.contains("Desktop background colour"));
        assert!(names.contains("Microphone mute"));
        assert!(names.contains("Passwordless sudo"));
        assert!(names.contains("Automatic software updates"));
    }

    #[test]
    fn background_rgba_conversion_is_exact_for_byte_channels() {
        for color in [
            DARK_WALLPAPER,
            LIGHT_WALLPAPER,
            SolidColor::new(255, 113, 57),
        ] {
            assert_eq!(rgba_to_solid(solid_to_rgba(color)), color);
        }
    }

    #[cfg(any())]
    #[test]
    fn byte_format_is_human_readable() {
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
    }

    #[cfg(any())]
    #[test]
    fn update_progress_is_phase_specific_and_human_readable() {
        let progress = UpdateProgress {
            phase: UpdateProgressPhase::Downloading,
            completed: 4 * 1024 * 1024,
            total: 16 * 1024 * 1024,
            unit: UpdateProgressUnit::Bytes,
            detail: Some("Downloading firefox-esr".to_owned()),
            cancellable: true,
        };
        assert_eq!(
            progress_description(&progress, Some(2.0 * 1024.0 * 1024.0)),
            "Downloading — 4.0 MiB of 16.0 MiB — 2.0 MiB/s — Downloading firefox-esr"
        );

        let installing = UpdateProgress {
            phase: UpdateProgressPhase::Installing,
            completed: 3,
            total: 10,
            unit: UpdateProgressUnit::Packages,
            detail: Some("firefox-esr: unpacking".to_owned()),
            cancellable: false,
        };
        assert_eq!(
            progress_description(&installing, None),
            "Installing — 3 of 10 packages — firefox-esr: unpacking"
        );
    }

    #[cfg(any())]
    #[test]
    fn download_rate_uses_progress_deltas_and_resets_between_phases() {
        let start = Instant::now();
        let mut tracker = DownloadRate::default();
        let mut progress = UpdateProgress {
            phase: UpdateProgressPhase::Downloading,
            completed: 1024,
            total: 4096,
            unit: UpdateProgressUnit::Bytes,
            detail: None,
            cancellable: true,
        };
        assert_eq!(tracker.observe(Some(&progress), start), None);
        progress.completed = 3072;
        let speed = tracker
            .observe(Some(&progress), start + Duration::from_secs(2))
            .unwrap();
        assert!((speed - 1024.0).abs() < f64::EPSILON);
        progress.phase = UpdateProgressPhase::Installing;
        progress.unit = UpdateProgressUnit::Packages;
        assert_eq!(
            tracker.observe(Some(&progress), start + Duration::from_secs(3)),
            None
        );
    }

    #[test]
    fn time_zone_table_accepts_iana_locations_without_manual_paths() {
        let zones = parse_time_zones(
            "# country\tcoordinates\tzone\nGB\t+513030-0000731\tEurope/London\nUS\t+404251-0740023\tAmerica/New_York\nXX\t+0000\t../escape\n",
        );
        assert_eq!(zones, ["America/New_York", "Etc/UTC", "Europe/London"]);
        assert!(valid_time_zone_name("Asia/Kathmandu"));
        assert!(!valid_time_zone_name("../Europe/London"));
        assert!(!valid_time_zone_name("Europe//London"));
    }
}
