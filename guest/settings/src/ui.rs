// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::model::{
    BridgeState, INTEGRATION_CONTROL_PATH, INTEGRATION_STATUS_PATH, OUTPUT_STATE_PATH, PageId,
    RuntimeGeometryView, SettingsStore, UPDATE_STATE_PATH, about_build, display_scale_socket_path,
    keyboard_settings_socket_path, load_media_bridges, load_registrations, load_runtime_geometry,
    load_update_view, set_guest_keyboard, set_guest_scale, theme_compatibility_diagnostic,
    validate_display_scale_socket, validate_keyboard_settings_socket,
};
use crate::sound::{
    DeviceId, SoundClientError, SoundConnection, SoundDevice, SoundOperationStatus, SoundRequestId,
    SoundService, SoundState, SoundStreamInfo, SoundTestState, UserVolumePercent,
};
use crate::updater::{self as updater_client, UpdateRequest};
use crate::{ChangeBus, ChangeSection};
use gtk::gdk;
use gtk::prelude::*;
use gtk4 as gtk;
use serde::Deserialize;
use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use wildbuzzard_desktop_core::{
    BackgroundChoice, GuestScalePreset, KeyboardSettings, SolidColor, ThemeMode, UpdateAction,
    UpdateProgressUnit, UpdateState, UpdateStatus,
};

const COMPACT_BREAKPOINT: i32 = 720;
const PAGE_MARGIN: i32 = 24;
const ROW_SPACING: i32 = 12;

#[cfg(test)]
const ACCESSIBLE_CONTROL_NAMES: &[&str] = &[
    "Settings notification service unavailable",
    "Settings navigation",
    "Settings page",
    "Dark theme",
    "Light theme",
    "Dark Plain background",
    "Dark + Logo background",
    "Light Plain background",
    "Light + Logo background",
    "Custom Solid Colour background",
    "Custom desktop colour",
    "Guest UI scale Automatic",
    "Guest UI scale 100 percent",
    "Guest UI scale 125 percent",
    "Guest UI scale 150 percent",
    "Guest UI scale 175 percent",
    "Guest UI scale 200 percent",
    "Refresh display information",
    "Common keyboard layout",
    "XKB keyboard model",
    "XKB keyboard layout",
    "XKB keyboard variant",
    "XKB keyboard options",
    "Apply keyboard settings",
    "Keyboard input test",
    "Default output device",
    "Output volume",
    "Mute output",
    "Default input device",
    "Input volume",
    "Mute input",
    "Test speakers",
    "Show microphone level",
    "Microphone level",
    "Active playback streams",
    "Active recording streams",
    "Refresh sound information",
    "Registered AppImages",
    "Refresh AppImage registrations",
    "Check for updates",
    "Install available updates",
    "Retry or repair update",
    "Refresh update information",
];

pub(crate) fn build_fatal_window(
    application: &gtk::Application,
    error: &str,
) -> gtk::ApplicationWindow {
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Settings")
        .icon_name("wildbuzzard-settings")
        .default_width(560)
        .default_height(320)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(PAGE_MARGIN);
    content.set_margin_bottom(PAGE_MARGIN);
    content.set_margin_start(PAGE_MARGIN);
    content.set_margin_end(PAGE_MARGIN);
    let heading = heading("Settings could not start");
    let detail = wrapped_label(error);
    detail.add_css_class("error");
    accessible(
        &detail,
        "Settings startup error",
        "The persistent guest settings directories could not be opened.",
    );
    content.append(&heading);
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
        .icon_name("wildbuzzard-settings")
        .default_width(900)
        .default_height(640)
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    if let Some(error) = bus.diagnostic() {
        let diagnostic = wrapped_label(&format!(
            "Live Settings notifications are unavailable: {error}. Changes are still persisted, but compatible desktop components may require reopening."
        ));
        diagnostic.set_margin_top(8);
        diagnostic.set_margin_bottom(8);
        diagnostic.set_margin_start(12);
        diagnostic.set_margin_end(12);
        diagnostic.add_css_class("warning");
        accessible(
            &diagnostic,
            "Settings notification service unavailable",
            "The private guest session D-Bus interface could not be registered; no live propagation is claimed.",
        );
        root.append(&diagnostic);
    }
    let page_titles = PageId::ALL.map(PageId::title);
    let compact_navigation = gtk::DropDown::from_strings(&page_titles);
    compact_navigation.set_margin_top(8);
    compact_navigation.set_margin_bottom(8);
    compact_navigation.set_margin_start(12);
    compact_navigation.set_margin_end(12);
    accessible(
        &compact_navigation,
        "Settings navigation",
        "Choose a Settings page. This compact control appears on narrow displays.",
    );
    compact_navigation.set_visible(false);
    root.append(&compact_navigation);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.set_hexpand(true);
    body.set_vexpand(true);
    let sidebar = gtk::ListBox::new();
    sidebar.set_selection_mode(gtk::SelectionMode::Single);
    sidebar.set_activate_on_single_click(true);
    sidebar.add_css_class("navigation-sidebar");
    sidebar.set_size_request(210, -1);
    accessible(
        &sidebar,
        "Settings navigation",
        "Choose a Settings page. All pages remain available at every window size.",
    );
    let mut navigation_rows = Vec::new();
    for page in PageId::ALL {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(true);
        row.set_selectable(true);
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
        accessible(
            &row,
            page.title(),
            &format!("Open the {} Settings page.", page.title()),
        );
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
    pages.add_named(
        &build_appearance_page(&window, Rc::clone(&store), Rc::clone(&bus)),
        Some(PageId::Appearance.stack_name()),
    );
    pages.add_named(
        &build_display_page(&window, Rc::clone(&store), Rc::clone(&bus)),
        Some(PageId::Display.stack_name()),
    );
    pages.add_named(
        &build_keyboard_page(&window, Rc::clone(&store), Rc::clone(&bus)),
        Some(PageId::Keyboard.stack_name()),
    );
    pages.add_named(&build_sound_page(), Some(PageId::Sound.stack_name()));
    pages.add_named(
        &build_applications_page(&window, Rc::clone(&store)),
        Some(PageId::ApplicationsDesktop.stack_name()),
    );
    pages.add_named(
        &build_updates_page(&window),
        Some(PageId::Updates.stack_name()),
    );
    pages.add_named(&build_about_page(), Some(PageId::About.stack_name()));
    accessible(
        &pages,
        "Settings page",
        "Content for the selected Settings category.",
    );
    body.append(&pages);
    root.append(&body);
    window.set_child(Some(&root));

    let synchronizing_navigation = Rc::new(Cell::new(false));
    {
        let pages = pages.clone();
        let compact_navigation = compact_navigation.clone();
        let synchronizing = Rc::clone(&synchronizing_navigation);
        sidebar.connect_row_selected(move |_list, row| {
            if synchronizing.get() {
                return;
            }
            let Some(row) = row else { return };
            let index = row.index();
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let Some(page) = PageId::ALL.get(index).copied() else {
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
        let synchronizing = Rc::clone(&synchronizing_navigation);
        compact_navigation.connect_selected_notify(move |dropdown| {
            if synchronizing.get() {
                return;
            }
            let index = dropdown.selected() as usize;
            let Some(page) = PageId::ALL.get(index).copied() else {
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

    let update_adaptive = {
        let compact_navigation = compact_navigation.clone();
        let sidebar = sidebar.clone();
        let separator = separator.clone();
        move |width: i32| {
            let compact = width > 0 && width < COMPACT_BREAKPOINT;
            compact_navigation.set_visible(compact);
            sidebar.set_visible(!compact);
            separator.set_visible(!compact);
        }
    };
    update_adaptive(root.width());
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

fn page(title: &str, description: &str, contents: &gtk::Box) -> gtk::ScrolledWindow {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 18);
    page.set_margin_top(PAGE_MARGIN);
    page.set_margin_bottom(PAGE_MARGIN);
    page.set_margin_start(PAGE_MARGIN);
    page.set_margin_end(PAGE_MARGIN);
    page.append(&heading(title));
    let description = wrapped_label(description);
    description.add_css_class("dim-label");
    page.append(&description);
    page.append(contents);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&page)
        .build();
    accessible(
        &scroll,
        title,
        &format!("Scrollable {title} Settings page."),
    );
    scroll
}

fn section(title: &str, description: Option<&str>) -> gtk::Box {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let heading = gtk::Label::new(Some(title));
    heading.set_xalign(0.0);
    heading.add_css_class("heading");
    section.append(&heading);
    if let Some(description) = description {
        let description = wrapped_label(description);
        description.add_css_class("dim-label");
        section.append(&description);
    }
    section
}

fn setting_row<W: IsA<gtk::Widget>>(title: &str, description: &str, control: &W) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, ROW_SPACING);
    row.set_margin_top(4);
    row.set_margin_bottom(4);
    let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
    labels.set_hexpand(true);
    let title_label = gtk::Label::new(Some(title));
    title_label.set_xalign(0.0);
    labels.append(&title_label);
    let description_label = wrapped_label(description);
    description_label.set_xalign(0.0);
    description_label.add_css_class("dim-label");
    labels.append(&description_label);
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

fn value_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_selectable(true);
    label.set_xalign(1.0);
    label
}

fn accessible<W: IsA<gtk::Accessible>>(widget: &W, label: &str, description: &str) {
    widget.update_property(&[
        gtk::accessible::Property::Label(label),
        gtk::accessible::Property::Description(description),
    ]);
}

fn show_error(parent: &gtk::ApplicationWindow, title: &str, detail: &str) {
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
        settings.set_gtk_application_prefer_dark_theme(mode == ThemeMode::Dark);
    }
}

fn disabled_reason<W: IsA<gtk::Widget> + IsA<gtk::Accessible>>(
    widget: &W,
    label: &str,
    reason: &str,
) {
    widget.set_sensitive(false);
    widget.set_tooltip_text(Some(reason));
    accessible(widget, label, reason);
    widget.update_state(&[gtk::accessible::State::Disabled(true)]);
}

fn build_appearance_page(
    window: &gtk::ApplicationWindow,
    store: Rc<RefCell<SettingsStore>>,
    bus: Rc<ChangeBus>,
) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);
    let writable = store.borrow().writable;
    if let Some(diagnostic) = store.borrow().diagnostic.as_deref() {
        let status = wrapped_label(diagnostic);
        status.add_css_class(if writable { "dim-label" } else { "error" });
        accessible(
            &status,
            "Settings file status",
            "Persistent settings schema and migration status.",
        );
        contents.append(&status);
    }
    if let Some(diagnostic) = theme_compatibility_diagnostic() {
        let status = wrapped_label(&diagnostic);
        status.add_css_class("warning");
        accessible(
            &status,
            "Theme compatibility warning",
            "An older persistent guest is missing optional theme-propagation packages. The desktop remains bootable and the recovery command is shown.",
        );
        contents.append(&status);
    }

    let theme_section = section(
        "Theme",
        Some(
            "Dark and Light use identical geometry. Compatible applications update live; some third-party applications may need to be reopened.",
        ),
    );
    let theme_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let dark = gtk::CheckButton::with_label("Dark");
    let light = gtk::CheckButton::with_label("Light");
    light.set_group(Some(&dark));
    dark.set_active(store.borrow().settings.appearance.theme == ThemeMode::Dark);
    light.set_active(store.borrow().settings.appearance.theme == ThemeMode::Light);
    accessible(
        &dark,
        "Dark theme",
        "Use the Wild Buzzard graphite dark palette without changing layout geometry.",
    );
    accessible(
        &light,
        "Light theme",
        "Use the Wild Buzzard restrained warm-light palette without changing layout geometry.",
    );
    dark.set_sensitive(writable);
    light.set_sensitive(writable);
    theme_buttons.append(&dark);
    theme_buttons.append(&light);
    theme_section.append(&theme_buttons);

    let preview = gtk::Frame::new(Some("Preview"));
    let preview_content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    preview_content.set_margin_top(14);
    preview_content.set_margin_bottom(14);
    preview_content.set_margin_start(14);
    preview_content.set_margin_end(14);
    let preview_header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let logo = gtk::Image::from_icon_name("wildbuzzard-settings");
    logo.set_pixel_size(36);
    accessible(
        &logo,
        "Buzzard mark preview",
        "The project-owned Settings icon. Its vector geometry is replaceable independently from this application.",
    );
    preview_header.append(&logo);
    let preview_title = gtk::Label::new(Some("Wild Buzzard"));
    preview_title.set_xalign(0.0);
    preview_title.set_hexpand(true);
    preview_header.append(&preview_title);
    let preview_button = gtk::Button::with_label("Control");
    preview_button.set_sensitive(false);
    preview_header.append(&preview_button);
    preview_content.append(&preview_header);
    let selected = gtk::Label::new(Some("Selected item · cinnamon accent"));
    selected.set_xalign(0.0);
    selected.add_css_class("accent");
    preview_content.append(&selected);
    let folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    folder_row.append(&gtk::Image::from_icon_name("folder"));
    folder_row.append(&gtk::Label::new(Some("Folder and taskbar preview")));
    preview_content.append(&folder_row);
    let taskbar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    taskbar.add_css_class("toolbar");
    taskbar.append(&gtk::Button::with_label("Applications"));
    let task = gtk::Button::with_label("Settings");
    task.set_hexpand(true);
    taskbar.append(&task);
    preview_content.append(&taskbar);
    preview.set_child(Some(&preview_content));
    theme_section.append(&preview);
    contents.append(&theme_section);

    let background_section = section(
        "Desktop Background",
        Some(
            "Background choice is independent from the application theme. Built-in backgrounds are rendered at the guest output's native physical size.",
        ),
    );
    let background_buttons = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let dark_plain = gtk::CheckButton::with_label("Dark Plain");
    let dark_logo = gtk::CheckButton::with_label("Dark + Logo");
    let light_plain = gtk::CheckButton::with_label("Light Plain");
    let light_logo = gtk::CheckButton::with_label("Light + Logo");
    let custom = gtk::CheckButton::with_label("Custom Solid Colour");
    for button in [&dark_logo, &light_plain, &light_logo, &custom] {
        button.set_group(Some(&dark_plain));
    }
    let current_background = store.borrow().settings.appearance.background;
    match current_background {
        BackgroundChoice::DarkPlain => dark_plain.set_active(true),
        BackgroundChoice::DarkLogo => dark_logo.set_active(true),
        BackgroundChoice::LightPlain => light_plain.set_active(true),
        BackgroundChoice::LightLogo => light_logo.set_active(true),
        BackgroundChoice::CustomSolid { .. } => custom.set_active(true),
    }
    let background_controls = [
        (&dark_plain, "Dark Plain background", "Use solid #202225."),
        (
            &dark_logo,
            "Dark + Logo background",
            "Use solid #202225 with the centered Buzzard mark.",
        ),
        (&light_plain, "Light Plain background", "Use solid #F4F1EC."),
        (
            &light_logo,
            "Light + Logo background",
            "Use solid #F4F1EC with the centered Buzzard mark.",
        ),
        (
            &custom,
            "Custom Solid Colour background",
            "Use one local solid colour without images, gradients, or remote content.",
        ),
    ];
    for (button, label, description) in background_controls {
        accessible(button, label, description);
        button.set_sensitive(writable);
        background_buttons.append(button);
    }
    let custom_dialog = gtk::ColorDialog::builder()
        .title("Choose Desktop Colour")
        .modal(true)
        .with_alpha(false)
        .build();
    let color_button = gtk::ColorDialogButton::new(Some(custom_dialog));
    let initial_color = match current_background {
        BackgroundChoice::CustomSolid { color } => color,
        _ => SolidColor::new(0x20, 0x22, 0x25),
    };
    color_button.set_rgba(&solid_to_rgba(initial_color));
    color_button.set_sensitive(writable);
    accessible(
        &color_button,
        "Custom desktop colour",
        "Choose an accessible solid RGB desktop background colour. Alpha is disabled.",
    );
    background_buttons.append(&setting_row(
        "Colour",
        "Used only when Custom Solid Colour is selected.",
        &color_button,
    ));
    background_section.append(&background_buttons);
    contents.append(&background_section);

    let background_controls = BackgroundControls {
        dark_plain: dark_plain.clone(),
        dark_logo: dark_logo.clone(),
        light_plain: light_plain.clone(),
        light_logo: light_logo.clone(),
        custom: custom.clone(),
        color: color_button.clone(),
    };

    let changing = Rc::new(Cell::new(false));
    connect_theme_choice(
        &dark,
        ThemeMode::Dark,
        &light,
        window,
        Rc::clone(&store),
        Rc::clone(&bus),
        Rc::clone(&changing),
    );
    connect_theme_choice(
        &light,
        ThemeMode::Light,
        &dark,
        window,
        Rc::clone(&store),
        Rc::clone(&bus),
        Rc::clone(&changing),
    );
    connect_background_choice(
        &dark_plain,
        BackgroundChoice::DarkPlain,
        window,
        Rc::clone(&store),
        Rc::clone(&bus),
        Rc::clone(&changing),
        background_controls.clone(),
    );
    connect_background_choice(
        &dark_logo,
        BackgroundChoice::DarkLogo,
        window,
        Rc::clone(&store),
        Rc::clone(&bus),
        Rc::clone(&changing),
        background_controls.clone(),
    );
    connect_background_choice(
        &light_plain,
        BackgroundChoice::LightPlain,
        window,
        Rc::clone(&store),
        Rc::clone(&bus),
        Rc::clone(&changing),
        background_controls.clone(),
    );
    connect_background_choice(
        &light_logo,
        BackgroundChoice::LightLogo,
        window,
        Rc::clone(&store),
        Rc::clone(&bus),
        Rc::clone(&changing),
        background_controls.clone(),
    );
    {
        let window = window.clone();
        let store = Rc::clone(&store);
        let bus = Rc::clone(&bus);
        let changing = Rc::clone(&changing);
        let color_button = color_button.clone();
        let controls = background_controls.clone();
        custom.connect_toggled(move |button| {
            if !button.is_active() || changing.get() {
                return;
            }
            commit_background(
                rgba_to_solid(color_button.rgba()),
                &window,
                &store,
                &bus,
                &changing,
                &controls,
            );
        });
    }
    {
        let window = window.clone();
        let store = Rc::clone(&store);
        let bus = Rc::clone(&bus);
        let changing = Rc::clone(&changing);
        let custom = custom.clone();
        let controls = background_controls.clone();
        color_button.connect_rgba_notify(move |button| {
            if !custom.is_active() || changing.get() {
                return;
            }
            commit_background(
                rgba_to_solid(button.rgba()),
                &window,
                &store,
                &bus,
                &changing,
                &controls,
            );
        });
    }

    page(
        "Appearance",
        "Choose the guest application palette and desktop background. Changes persist in this machine's guest home.",
        &contents,
    )
}

#[derive(Clone)]
struct BackgroundControls {
    dark_plain: gtk::CheckButton,
    dark_logo: gtk::CheckButton,
    light_plain: gtk::CheckButton,
    light_logo: gtk::CheckButton,
    custom: gtk::CheckButton,
    color: gtk::ColorDialogButton,
}

impl BackgroundControls {
    fn restore(&self, choice: BackgroundChoice) {
        match choice {
            BackgroundChoice::DarkPlain => self.dark_plain.set_active(true),
            BackgroundChoice::DarkLogo => self.dark_logo.set_active(true),
            BackgroundChoice::LightPlain => self.light_plain.set_active(true),
            BackgroundChoice::LightLogo => self.light_logo.set_active(true),
            BackgroundChoice::CustomSolid { color } => {
                self.color.set_rgba(&solid_to_rgba(color));
                self.custom.set_active(true);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn connect_theme_choice(
    button: &gtk::CheckButton,
    mode: ThemeMode,
    fallback: &gtk::CheckButton,
    window: &gtk::ApplicationWindow,
    store: Rc<RefCell<SettingsStore>>,
    bus: Rc<ChangeBus>,
    changing: Rc<Cell<bool>>,
) {
    let fallback = fallback.clone();
    let window = window.clone();
    button.connect_toggled(move |button| {
        if !button.is_active() || changing.get() {
            return;
        }
        changing.set(true);
        match store.borrow_mut().set_theme(mode) {
            Ok(generation) => {
                apply_current_process_theme(mode);
                if let Err(error) = bus.emit_changed(generation, &[ChangeSection::Appearance]) {
                    show_error(&window, "Theme saved, notification failed", &error);
                }
            }
            Err(error) => {
                fallback.set_active(true);
                show_error(&window, "Theme was not changed", &error.to_string());
            }
        }
        changing.set(false);
    });
}

fn connect_background_choice(
    button: &gtk::CheckButton,
    choice: BackgroundChoice,
    window: &gtk::ApplicationWindow,
    store: Rc<RefCell<SettingsStore>>,
    bus: Rc<ChangeBus>,
    changing: Rc<Cell<bool>>,
    controls: BackgroundControls,
) {
    let window = window.clone();
    button.connect_toggled(move |button| {
        if !button.is_active() || changing.get() {
            return;
        }
        changing.set(true);
        match commit_background_choice(choice, &store, &bus) {
            Ok(Some(error)) => show_error(
                &window,
                "Desktop background saved, notification failed",
                &error,
            ),
            Ok(None) => {}
            Err(error) => {
                controls.restore(store.borrow().settings.appearance.background);
                show_error(&window, "Desktop background was not changed", &error);
            }
        }
        changing.set(false);
    });
}

fn commit_background(
    color: SolidColor,
    window: &gtk::ApplicationWindow,
    store: &Rc<RefCell<SettingsStore>>,
    bus: &Rc<ChangeBus>,
    changing: &Rc<Cell<bool>>,
    controls: &BackgroundControls,
) {
    changing.set(true);
    match commit_background_choice(BackgroundChoice::CustomSolid { color }, store, bus) {
        Ok(Some(error)) => show_error(
            window,
            "Desktop background saved, notification failed",
            &error,
        ),
        Ok(None) => {}
        Err(error) => {
            controls.restore(store.borrow().settings.appearance.background);
            show_error(window, "Desktop background was not changed", &error);
        }
    }
    changing.set(false);
}

fn commit_background_choice(
    choice: BackgroundChoice,
    store: &Rc<RefCell<SettingsStore>>,
    bus: &Rc<ChangeBus>,
) -> Result<Option<String>, String> {
    let generation = store
        .borrow_mut()
        .set_background(choice)
        .map_err(|error| error.to_string())?;
    Ok(bus
        .emit_changed(generation, &[ChangeSection::Appearance])
        .err())
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

#[derive(Clone)]
struct DisplayScaleControls {
    automatic: gtk::CheckButton,
    percent100: gtk::CheckButton,
    percent125: gtk::CheckButton,
    percent150: gtk::CheckButton,
    percent175: gtk::CheckButton,
    percent200: gtk::CheckButton,
}

impl DisplayScaleControls {
    fn entries(&self) -> [(&gtk::CheckButton, GuestScalePreset, &'static str); 6] {
        [
            (
                &self.automatic,
                GuestScalePreset::Automatic,
                "Guest UI scale Automatic",
            ),
            (
                &self.percent100,
                GuestScalePreset::Percent100,
                "Guest UI scale 100 percent",
            ),
            (
                &self.percent125,
                GuestScalePreset::Percent125,
                "Guest UI scale 125 percent",
            ),
            (
                &self.percent150,
                GuestScalePreset::Percent150,
                "Guest UI scale 150 percent",
            ),
            (
                &self.percent175,
                GuestScalePreset::Percent175,
                "Guest UI scale 175 percent",
            ),
            (
                &self.percent200,
                GuestScalePreset::Percent200,
                "Guest UI scale 200 percent",
            ),
        ]
    }

    fn restore(&self, preset: GuestScalePreset) {
        match preset {
            GuestScalePreset::Automatic => self.automatic.set_active(true),
            GuestScalePreset::Percent100 => self.percent100.set_active(true),
            GuestScalePreset::Percent125 => self.percent125.set_active(true),
            GuestScalePreset::Percent150 => self.percent150.set_active(true),
            GuestScalePreset::Percent175 => self.percent175.set_active(true),
            GuestScalePreset::Percent200 => self.percent200.set_active(true),
        }
    }

    fn set_available(&self, available: bool, reason: &str) {
        for (button, preset, accessible_name) in self.entries() {
            button.set_sensitive(available);
            button.set_tooltip_text((!reason.is_empty()).then_some(reason));
            let description = if available {
                format!(
                    "Set the guest desktop UI density to {} without changing the physical framebuffer size.",
                    scale_preset_label(preset)
                )
            } else {
                reason.to_owned()
            };
            accessible(button, accessible_name, &description);
            button.update_state(&[gtk::accessible::State::Disabled(!available)]);
        }
    }
}

#[derive(Clone)]
struct DisplayWidgets {
    physical: gtk::Label,
    logical: gtk::Label,
    host_scale: gtk::Label,
    guest_scale: gtk::Label,
    generation: gtk::Label,
    geometry_diagnostic: gtk::Label,
    scale_status: gtk::Label,
}

fn build_display_page(
    window: &gtk::ApplicationWindow,
    store: Rc<RefCell<SettingsStore>>,
    bus: Rc<ChangeBus>,
) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);
    let geometry_section = section(
        "Current Monitor",
        Some(
            "Physical mode is the native guest framebuffer and screenshot size. Logical mode is the desktop coordinate space after guest UI scaling.",
        ),
    );
    let physical = value_label("Unavailable");
    let logical = value_label("Unavailable");
    let host_scale = value_label("Unavailable");
    let guest_scale = value_label("Unavailable");
    let generation = value_label("Unavailable");
    geometry_section.append(&setting_row(
        "Physical mode",
        "Native dmabuf and CUA screenshot pixels.",
        &physical,
    ));
    geometry_section.append(&setting_row(
        "Logical mode",
        "Guest desktop layout coordinates.",
        &logical,
    ));
    geometry_section.append(&setting_row(
        "Host surface scale",
        "Scale of the host monitor containing the machine window.",
        &host_scale,
    ));
    geometry_section.append(&setting_row(
        "Guest UI scale",
        "Independent logical density requested for the guest.",
        &guest_scale,
    ));
    geometry_section.append(&setting_row(
        "Geometry generation",
        "Coordinates and screenshots are valid only for this generation.",
        &generation,
    ));
    let geometry_diagnostic = wrapped_label("");
    geometry_diagnostic.add_css_class("dim-label");
    geometry_section.append(&geometry_diagnostic);
    let refresh = gtk::Button::with_label("Refresh");
    accessible(
        &refresh,
        "Refresh display information",
        "Read the current guest monitor geometry without changing it.",
    );
    refresh.set_halign(gtk::Align::Start);
    geometry_section.append(&refresh);
    contents.append(&geometry_section);

    let scale_section = section(
        "Internal UI Scale",
        Some(
            "Manual UI scale changes logical density only; they never reduce the physical monitor mode or resample the complete frame.",
        ),
    );
    let automatic = gtk::CheckButton::with_label("Automatic");
    let percent100 = gtk::CheckButton::with_label("100%");
    let percent125 = gtk::CheckButton::with_label("125%");
    let percent150 = gtk::CheckButton::with_label("150%");
    let percent175 = gtk::CheckButton::with_label("175%");
    let percent200 = gtk::CheckButton::with_label("200%");
    for button in [
        &percent100,
        &percent125,
        &percent150,
        &percent175,
        &percent200,
    ] {
        button.set_group(Some(&automatic));
    }
    let controls = DisplayScaleControls {
        automatic,
        percent100,
        percent125,
        percent150,
        percent175,
        percent200,
    };
    controls.restore(store.borrow().settings.display.guest_ui_scale);
    for (button, _, _) in controls.entries() {
        scale_section.append(button);
    }
    let scale_status = wrapped_label("");
    scale_status.add_css_class("dim-label");
    accessible(
        &scale_status,
        "Display scale status",
        "Availability and last confirmed state of the generation-aware display scale service.",
    );
    scale_section.append(&scale_status);
    contents.append(&scale_section);

    let widgets = DisplayWidgets {
        physical,
        logical,
        host_scale,
        guest_scale,
        generation,
        geometry_diagnostic,
        scale_status,
    };
    let changing = Rc::new(Cell::new(false));
    for (button, preset, _) in controls.entries() {
        connect_display_scale_choice(
            button,
            preset,
            window,
            Rc::clone(&store),
            Rc::clone(&bus),
            Rc::clone(&changing),
            controls.clone(),
            widgets.clone(),
        );
    }
    refresh_display_widgets(&widgets, &controls, &store, &changing);
    {
        let widgets = widgets.clone();
        let controls = controls.clone();
        let store = Rc::clone(&store);
        let changing = Rc::clone(&changing);
        refresh.connect_clicked(move |_| {
            refresh_display_widgets(&widgets, &controls, &store, &changing);
        });
    }

    page(
        "Display",
        "Inspect exact guest monitor geometry and choose internal UI density through the coordinated generation-aware scale service.",
        &contents,
    )
}

fn refresh_display_widgets(
    widgets: &DisplayWidgets,
    controls: &DisplayScaleControls,
    store: &Rc<RefCell<SettingsStore>>,
    changing: &Rc<Cell<bool>>,
) -> RuntimeGeometryView {
    let view = load_runtime_geometry(Path::new(OUTPUT_STATE_PATH));
    set_runtime_geometry_labels(widgets, &view);

    let was_changing = changing.replace(true);
    controls.restore(store.borrow().settings.display.guest_ui_scale);
    changing.set(was_changing);

    let availability = if !store.borrow().writable {
        Err(store
            .borrow()
            .diagnostic
            .clone()
            .unwrap_or_else(|| "Persistent Settings are read-only.".into()))
    } else if let Some(diagnostic) = &view.diagnostic {
        Err(diagnostic.clone())
    } else if view.geometry().is_none() {
        Err("The display runtime has not published a complete geometry generation.".into())
    } else {
        display_scale_socket_path()
            .and_then(|path| validate_display_scale_socket(&path))
            .map_err(|error| error.to_string())
    };

    match availability {
        Ok(()) => {
            controls.set_available(true, "");
            widgets.scale_status.remove_css_class("warning");
            widgets.scale_status.set_label(
                "Ready. A scale change keeps the physical framebuffer unchanged and atomically advances the guest geometry generation.",
            );
        }
        Err(reason) => {
            controls.set_available(false, &reason);
            widgets.scale_status.add_css_class("warning");
            widgets
                .scale_status
                .set_label(&format!("Display scale controls are unavailable: {reason}"));
        }
    }
    view
}

fn set_runtime_geometry_labels(widgets: &DisplayWidgets, view: &RuntimeGeometryView) {
    widgets.physical.set_label(&format_dimensions(
        view.physical_width,
        view.physical_height,
    ));
    widgets
        .logical
        .set_label(&format_dimensions(view.logical_width, view.logical_height));
    widgets
        .host_scale
        .set_label(&format_scale(view.host_scale_120));
    widgets
        .guest_scale
        .set_label(&format_scale(view.guest_scale_120));
    widgets.generation.set_label(
        &view
            .geometry_generation
            .map_or_else(|| "Not published".into(), |value| value.to_string()),
    );
    widgets.geometry_diagnostic.set_label(
        view.diagnostic
            .as_deref()
            .unwrap_or("Current runtime geometry is internally coherent."),
    );
}

#[allow(clippy::too_many_arguments)]
fn connect_display_scale_choice(
    button: &gtk::CheckButton,
    preset: GuestScalePreset,
    window: &gtk::ApplicationWindow,
    store: Rc<RefCell<SettingsStore>>,
    bus: Rc<ChangeBus>,
    changing: Rc<Cell<bool>>,
    controls: DisplayScaleControls,
    widgets: DisplayWidgets,
) {
    let window = window.clone();
    button.connect_toggled(move |button| {
        if !button.is_active() || changing.get() {
            return;
        }
        changing.set(true);
        controls.set_available(false, "Applying the requested display scale…");
        let previous = store.borrow().settings.display.guest_ui_scale;
        let current = load_runtime_geometry(Path::new(OUTPUT_STATE_PATH));
        let Some(current_geometry) = current.geometry() else {
            controls.restore(previous);
            changing.set(false);
            refresh_display_widgets(&widgets, &controls, &store, &changing);
            show_error(
                &window,
                "Display scale was not changed",
                current
                    .diagnostic
                    .as_deref()
                    .unwrap_or("The current display geometry is incomplete."),
            );
            return;
        };
        let socket = match display_scale_socket_path() {
            Ok(socket) => socket,
            Err(error) => {
                controls.restore(previous);
                changing.set(false);
                refresh_display_widgets(&widgets, &controls, &store, &changing);
                show_error(
                    &window,
                    "Display scale was not changed",
                    &error.to_string(),
                );
                return;
            }
        };

        match set_guest_scale(&socket, preset, current_geometry.geometry_generation) {
            Ok(confirmed) => {
                if confirmed.physical_width != current_geometry.physical_width
                    || confirmed.physical_height != current_geometry.physical_height
                {
                    let rollback = set_guest_scale(
                        &socket,
                        previous,
                        confirmed.geometry_generation,
                    )
                    .map(|_| "Runtime rollback succeeded.".to_owned())
                    .unwrap_or_else(|error| format!("Runtime rollback failed: {error}"));
                    controls.restore(previous);
                    changing.set(false);
                    refresh_display_widgets(&widgets, &controls, &store, &changing);
                    show_error(
                        &window,
                        "Display scale response was rejected",
                        &format!(
                            "The scale service changed the physical framebuffer from {} × {} to {} × {}; this violates the native-pixel contract. {rollback}",
                            current_geometry.physical_width,
                            current_geometry.physical_height,
                            confirmed.physical_width,
                            confirmed.physical_height,
                        ),
                    );
                    return;
                }
                match store.borrow_mut().persist_confirmed_display_scale(preset) {
                    Ok(settings_generation) => {
                        if let Err(error) =
                            bus.emit_changed(settings_generation, &[ChangeSection::Display])
                        {
                            show_error(
                                &window,
                                "Display scale saved, notification failed",
                                &error,
                            );
                        }
                    }
                    Err(error) => {
                        let rollback = set_guest_scale(
                            &socket,
                            previous,
                            confirmed.geometry_generation,
                        )
                        .map(|_| "Runtime rollback succeeded.".to_owned())
                        .unwrap_or_else(|rollback_error| {
                            format!("Runtime rollback failed: {rollback_error}")
                        });
                        controls.restore(previous);
                        changing.set(false);
                        refresh_display_widgets(&widgets, &controls, &store, &changing);
                        show_error(
                            &window,
                            "Display scale was not saved",
                            &format!("{error}. {rollback}"),
                        );
                        return;
                    }
                }
            }
            Err(error) => {
                controls.restore(previous);
                changing.set(false);
                refresh_display_widgets(&widgets, &controls, &store, &changing);
                let title = if error.is_stale_geometry() {
                    "Display changed while scaling"
                } else {
                    "Display scale was not changed"
                };
                let suffix = if error.is_stale_geometry() {
                    " Refresh completed; choose the scale again using the new geometry."
                } else {
                    ""
                };
                show_error(&window, title, &format!("{error}.{suffix}"));
                return;
            }
        }

        changing.set(false);
        refresh_display_widgets(&widgets, &controls, &store, &changing);
    });
}

fn scale_preset_label(preset: GuestScalePreset) -> &'static str {
    match preset {
        GuestScalePreset::Automatic => "Automatic",
        GuestScalePreset::Percent100 => "100%",
        GuestScalePreset::Percent125 => "125%",
        GuestScalePreset::Percent150 => "150%",
        GuestScalePreset::Percent175 => "175%",
        GuestScalePreset::Percent200 => "200%",
    }
}

fn format_dimensions(width: Option<u32>, height: Option<u32>) -> String {
    match (width, height) {
        (Some(width), Some(height)) => format!("{width} × {height}"),
        _ => "Unavailable".into(),
    }
}

fn format_scale(scale_120: Option<u32>) -> String {
    scale_120.map_or_else(
        || "Unavailable".into(),
        |scale| format!("{:.2}% ({scale}/120)", f64::from(scale) / 1.2),
    )
}

const COMMON_KEYBOARD_LAYOUTS: &[(&str, &str)] = &[
    ("Custom code below", ""),
    ("English (US)", "us"),
    ("English (UK)", "gb"),
    ("French", "fr"),
    ("German", "de"),
    ("Spanish", "es"),
    ("Italian", "it"),
    ("Portuguese", "pt"),
    ("Brazilian Portuguese", "br"),
    ("Dutch", "nl"),
    ("Polish", "pl"),
    ("Czech", "cz"),
    ("Swedish", "se"),
    ("Norwegian", "no"),
    ("Danish", "dk"),
    ("Finnish", "fi"),
    ("Greek", "gr"),
    ("Turkish", "tr"),
    ("Ukrainian", "ua"),
    ("Russian", "ru"),
    ("Arabic", "ara"),
    ("Hebrew", "il"),
    ("Japanese", "jp"),
    ("Korean", "kr"),
];

fn build_keyboard_page(
    window: &gtk::ApplicationWindow,
    store: Rc<RefCell<SettingsStore>>,
    bus: Rc<ChangeBus>,
) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);
    let keyboard = store.borrow().settings.keyboard.clone();
    let writable = store.borrow().writable;

    let layout_section = section(
        "Keyboard Layout",
        Some(
            "The guest owns this XKB layout like a physical Linux machine. It applies to human input forwarded into Sway; CUA remains a separate synthetic keyboard on the same private seat.",
        ),
    );
    let labels = COMMON_KEYBOARD_LAYOUTS
        .iter()
        .map(|(label, _)| *label)
        .collect::<Vec<_>>();
    let common = gtk::DropDown::from_strings(&labels);
    let common_index = COMMON_KEYBOARD_LAYOUTS
        .iter()
        .position(|(_, code)| *code == keyboard.layout)
        .unwrap_or(0);
    common.set_selected(common_index as u32);
    accessible(
        &common,
        "Common keyboard layout",
        "Choose a common language layout or use a custom XKB layout code below.",
    );
    layout_section.append(&setting_row(
        "Language and layout",
        "Selecting a common entry fills the authoritative XKB layout code.",
        &common,
    ));

    let model = gtk::Entry::new();
    model.set_text(&keyboard.model);
    model.set_max_length(64);
    accessible(
        &model,
        "XKB keyboard model",
        "XKB hardware model, normally pc105.",
    );
    layout_section.append(&setting_row(
        "Keyboard model",
        "Usually pc105. Change this only for a keyboard model supported by XKB.",
        &model,
    ));

    let layout = gtk::Entry::new();
    layout.set_text(&keyboard.layout);
    layout.set_max_length(256);
    layout.set_placeholder_text(Some("us, gb, de, fr…"));
    accessible(
        &layout,
        "XKB keyboard layout",
        "One XKB layout code, or a comma-separated group of layout codes.",
    );
    layout_section.append(&setting_row(
        "Layout code",
        "Use standard XKB codes such as us, gb, de, fr, or comma-separated layouts.",
        &layout,
    ));

    let variant = gtk::Entry::new();
    variant.set_text(&keyboard.variant);
    variant.set_max_length(256);
    variant.set_placeholder_text(Some("Optional, for example intl"));
    accessible(
        &variant,
        "XKB keyboard variant",
        "Optional XKB variant aligned with the selected layout or layout group.",
    );
    layout_section.append(&setting_row(
        "Variant",
        "Leave empty for the layout default. Multiple layouts use comma-aligned variants.",
        &variant,
    ));

    let options = gtk::Entry::new();
    options.set_text(&keyboard.options);
    options.set_max_length(512);
    options.set_placeholder_text(Some("Optional, for example compose:ralt"));
    accessible(
        &options,
        "XKB keyboard options",
        "Optional comma-separated XKB options, including Compose-key and layout switching choices.",
    );
    layout_section.append(&setting_row(
        "Options",
        "Examples: compose:ralt or grp:alt_shift_toggle. Leave empty for defaults.",
        &options,
    ));

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let apply = gtk::Button::with_label("Apply");
    apply.add_css_class("suggested-action");
    accessible(
        &apply,
        "Apply keyboard settings",
        "Compile the requested XKB keymap, apply it to stock Sway, confirm it, and persist it.",
    );
    let status = wrapped_label("Checking the private keyboard service…");
    status.set_hexpand(true);
    status.add_css_class("dim-label");
    accessible(
        &status,
        "Keyboard settings status",
        "Availability and last confirmed active Sway keyboard layout.",
    );
    actions.append(&apply);
    actions.append(&status);
    layout_section.append(&actions);
    contents.append(&layout_section);

    let test_section = section(
        "Test Input",
        Some(
            "Type, use Backspace, hold modifiers, and try layout-specific symbols here before closing Settings.",
        ),
    );
    let test_entry = gtk::Entry::new();
    test_entry.set_placeholder_text(Some("Type here to test this guest keyboard"));
    accessible(
        &test_entry,
        "Keyboard input test",
        "Editable field for testing human keyboard input, Backspace, modifiers, Compose, and the selected layout.",
    );
    test_section.append(&test_entry);
    contents.append(&test_section);

    for widget in [&model, &layout, &variant, &options] {
        widget.set_sensitive(writable);
    }
    common.set_sensitive(writable);

    let availability = keyboard_settings_socket_path().and_then(|path| {
        validate_keyboard_settings_socket(&path)?;
        Ok(path)
    });
    let socket = match availability {
        Ok(path) if writable => {
            status.set_label("Ready. Changes are applied live without restarting the machine.");
            Some(path)
        }
        Ok(_) => {
            status.set_label("Persistent Settings are read-only.");
            None
        }
        Err(error) => {
            status.add_css_class("warning");
            status.set_label(&format!("Keyboard controls are unavailable: {error}"));
            None
        }
    };
    apply.set_sensitive(socket.is_some());

    {
        let layout = layout.clone();
        common.connect_selected_notify(move |dropdown| {
            let index = dropdown.selected() as usize;
            let Some((_, code)) = COMMON_KEYBOARD_LAYOUTS.get(index) else {
                return;
            };
            if !code.is_empty() {
                layout.set_text(code);
            }
        });
    }
    if let Some(socket) = socket {
        let window = window.clone();
        apply.connect_clicked(move |button| {
            button.set_sensitive(false);
            status.set_label("Compiling and applying the requested keymap…");
            let requested = KeyboardSettings {
                model: model.text().trim().to_owned(),
                layout: layout.text().trim().to_owned(),
                variant: variant.text().trim().to_owned(),
                options: options.text().trim().to_owned(),
            };
            if let Err(error) = requested.validate() {
                status.set_label("The requested keyboard settings are invalid.");
                show_error(&window, "Keyboard was not changed", &error.to_string());
                button.set_sensitive(true);
                return;
            }
            let previous = store.borrow().settings.keyboard.clone();
            match set_guest_keyboard(&socket, &requested) {
                Ok(active_name) => match store
                    .borrow_mut()
                    .persist_confirmed_keyboard(requested.clone())
                {
                    Ok(generation) => {
                        status.set_label(&format!("Active layout: {active_name}"));
                        if let Err(error) = bus.emit_changed(generation, &[ChangeSection::Keyboard])
                        {
                            show_error(&window, "Keyboard saved, notification failed", &error);
                        }
                    }
                    Err(error) => {
                        let rollback_result = set_guest_keyboard(&socket, &previous);
                        let (status_text, rollback) = match rollback_result {
                            Ok(_) => (
                                "The previous keyboard layout was restored.",
                                "Runtime rollback succeeded.".to_owned(),
                            ),
                            Err(rollback) => (
                                "The requested layout is still active because rollback failed.",
                                format!("Runtime rollback failed: {rollback}"),
                            ),
                        };
                        status.set_label(status_text);
                        show_error(
                            &window,
                            "Keyboard setting was not saved",
                            &format!("{error}. {rollback}"),
                        );
                    }
                },
                Err(error) => {
                    status.set_label("Sway did not accept the requested keyboard layout.");
                    show_error(&window, "Keyboard was not changed", &error.to_string());
                }
            }
            button.set_sensitive(true);
        });
    }

    page(
        "Keyboard",
        "Choose the physical keyboard language/layout used inside this machine. The setting is private to the persistent guest and applies at every boot.",
        &contents,
    )
}

#[derive(Clone)]
struct SoundWidgets {
    connection: gtk::Label,
    operation: gtk::Label,
    output_device: gtk::DropDown,
    output_volume: gtk::Scale,
    output_volume_label: gtk::Label,
    output_mute: gtk::Switch,
    speaker_test: gtk::Button,
    speaker_status: gtk::Label,
    input_device: gtk::DropDown,
    input_volume: gtk::Scale,
    input_volume_label: gtk::Label,
    input_mute: gtk::Switch,
    microphone_test: gtk::Button,
    microphone_meter: gtk::ProgressBar,
    playback_streams: gtk::ListBox,
    recording_streams: gtk::ListBox,
}

#[derive(Default)]
struct SoundUiState {
    synchronizing: bool,
    output_ids: Vec<DeviceId>,
    input_ids: Vec<DeviceId>,
    previous: Option<SoundState>,
}

fn build_sound_page() -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);
    let bridge_section = section(
        "Host Bridges",
        Some(
            "These statuses are read-only. Host speakers, microphone, and camera permissions remain in the host application's Devices control.",
        ),
    );
    let audio_bridge = value_label("Unavailable");
    let microphone_bridge = value_label("Unavailable");
    let camera_bridge = value_label("Unavailable");
    bridge_section.append(&setting_row(
        "Guest audio to host",
        "Host-owned output bridge.",
        &audio_bridge,
    ));
    bridge_section.append(&setting_row(
        "Host microphone to guest",
        "Host-owned recording bridge. Opening this page never activates it.",
        &microphone_bridge,
    ));
    bridge_section.append(&setting_row(
        "Host camera to guest",
        "Camera permission is not duplicated in guest Settings.",
        &camera_bridge,
    ));
    let bridge_diagnostic = wrapped_label("");
    bridge_diagnostic.add_css_class("dim-label");
    bridge_section.append(&bridge_diagnostic);
    contents.append(&bridge_section);

    let server_section = section(
        "Guest Sound Server",
        Some(
            "Settings connects directly to the guest-private PipeWire-Pulse service. It never invokes a command-line mixer or accesses the host sound server.",
        ),
    );
    let connection = wrapped_label("Connecting to the guest sound server…");
    accessible(
        &connection,
        "Guest sound server status",
        "Connection and subscription state for the guest-private PipeWire graph.",
    );
    let operation = wrapped_label("No sound operation is pending.");
    operation.add_css_class("dim-label");
    accessible(
        &operation,
        "Sound operation status",
        "Result of the latest typed sound request.",
    );
    let refresh = gtk::Button::with_label("Refresh");
    accessible(
        &refresh,
        "Refresh sound information",
        "Refresh guest sound devices, streams, and read-only host bridge status without opening a microphone.",
    );
    refresh.set_halign(gtk::Align::Start);
    server_section.append(&connection);
    server_section.append(&operation);
    server_section.append(&refresh);
    contents.append(&server_section);

    let output_section = section(
        "Output",
        Some("Controls the guest's private PipeWire default output and its current streams."),
    );
    let output_device = gtk::DropDown::from_strings(&["Unavailable"]);
    output_device.set_size_request(260, -1);
    let output_volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 150.0, 1.0);
    output_volume.set_value(100.0);
    output_volume.set_hexpand(true);
    output_volume.set_size_request(180, -1);
    output_volume.set_draw_value(false);
    let output_volume_label = value_label("100%");
    let output_volume_control = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    output_volume_control.append(&output_volume);
    output_volume_control.append(&output_volume_label);
    let output_mute = gtk::Switch::new();
    let speaker_test = gtk::Button::with_label("Test Speakers");
    let speaker_status = wrapped_label("No speaker test is active.");
    speaker_status.add_css_class("dim-label");
    accessible(
        &output_device,
        "Default output device",
        "Choose the default output in the guest-private sound graph.",
    );
    accessible(
        &output_volume,
        "Output volume",
        "Set guest output volume from 0 through 150 percent.",
    );
    accessible(
        &output_mute,
        "Mute output",
        "Mute or unmute the selected guest output.",
    );
    accessible(
        &speaker_test,
        "Test speakers",
        "Explicitly play a short left-then-right test through the selected guest output.",
    );
    output_section.append(&setting_row(
        "Default device",
        "Current guest PipeWire default sink.",
        &output_device,
    ));
    output_section.append(&setting_row(
        "Volume",
        "Guest output volume, persisted by WirePlumber.",
        &output_volume_control,
    ));
    output_section.append(&setting_row(
        "Mute",
        "Mute the guest default output.",
        &output_mute,
    ));
    output_section.append(&speaker_test);
    output_section.append(&speaker_status);
    contents.append(&output_section);

    let input_section = section(
        "Input",
        Some(
            "Opening Sound does not activate a microphone. The level meter captures only after an explicit request and releases its stream when stopped, hidden, failed, or timed out.",
        ),
    );
    let input_device = gtk::DropDown::from_strings(&["Unavailable"]);
    input_device.set_size_request(260, -1);
    let input_volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 150.0, 1.0);
    input_volume.set_value(100.0);
    input_volume.set_hexpand(true);
    input_volume.set_size_request(180, -1);
    input_volume.set_draw_value(false);
    let input_volume_label = value_label("100%");
    let input_volume_control = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    input_volume_control.append(&input_volume);
    input_volume_control.append(&input_volume_label);
    let input_mute = gtk::Switch::new();
    let microphone_test = gtk::Button::with_label("Show Microphone Level");
    let microphone_meter = gtk::ProgressBar::new();
    microphone_meter.set_fraction(0.0);
    microphone_meter.set_show_text(true);
    microphone_meter.set_text(Some("Not active — no microphone is open"));
    accessible(
        &input_device,
        "Default input device",
        "Choose the default input in the guest-private sound graph.",
    );
    accessible(
        &input_volume,
        "Input volume",
        "Set guest input volume from 0 through 150 percent.",
    );
    accessible(
        &input_mute,
        "Mute input",
        "Mute or unmute the selected guest input.",
    );
    accessible(
        &microphone_test,
        "Show microphone level",
        "Explicitly open a bounded microphone level test in the guest, or stop it and release capture.",
    );
    accessible(
        &microphone_meter,
        "Microphone level",
        "The microphone is not open.",
    );
    input_section.append(&setting_row(
        "Default device",
        "Current guest PipeWire default source.",
        &input_device,
    ));
    input_section.append(&setting_row(
        "Volume",
        "Guest input volume, persisted by WirePlumber.",
        &input_volume_control,
    ));
    input_section.append(&setting_row(
        "Mute",
        "Mute the guest default input.",
        &input_mute,
    ));
    input_section.append(&microphone_test);
    input_section.append(&microphone_meter);
    contents.append(&input_section);

    let streams_section = section(
        "Active Streams",
        Some(
            "Live subscriptions report playback and recording streams from the private guest graph.",
        ),
    );
    let playback_heading = gtk::Label::new(Some("Playback"));
    playback_heading.set_xalign(0.0);
    playback_heading.add_css_class("heading");
    let playback_streams = gtk::ListBox::new();
    playback_streams.set_selection_mode(gtk::SelectionMode::None);
    playback_streams.add_css_class("boxed-list");
    accessible(
        &playback_streams,
        "Active playback streams",
        "Playback streams currently reported by guest PipeWire-Pulse.",
    );
    let recording_heading = gtk::Label::new(Some("Recording"));
    recording_heading.set_xalign(0.0);
    recording_heading.add_css_class("heading");
    let recording_streams = gtk::ListBox::new();
    recording_streams.set_selection_mode(gtk::SelectionMode::None);
    recording_streams.add_css_class("boxed-list");
    accessible(
        &recording_streams,
        "Active recording streams",
        "Recording streams currently reported by guest PipeWire-Pulse.",
    );
    streams_section.append(&playback_heading);
    streams_section.append(&playback_streams);
    streams_section.append(&recording_heading);
    streams_section.append(&recording_streams);
    contents.append(&streams_section);

    let update_bridges: Rc<dyn Fn()> =
        Rc::new(move || {
            let view = load_media_bridges(
                Path::new(INTEGRATION_CONTROL_PATH),
                Path::new(INTEGRATION_STATUS_PATH),
            );
            set_bridge_label(&audio_bridge, view.guest_audio_output);
            set_bridge_label(&microphone_bridge, view.host_microphone);
            set_bridge_label(&camera_bridge, view.host_camera);
            bridge_diagnostic.set_label(view.diagnostic.as_deref().unwrap_or(
                "Bridge status is current. Device permission changes remain host-owned.",
            ));
        });
    update_bridges();

    let widgets = SoundWidgets {
        connection,
        operation,
        output_device,
        output_volume,
        output_volume_label,
        output_mute,
        speaker_test,
        speaker_status,
        input_device,
        input_volume,
        input_volume_label,
        input_mute,
        microphone_test,
        microphone_meter,
        playback_streams,
        recording_streams,
    };
    let sound_page = page(
        "Sound",
        "Inspect host bridge state and control the guest's private PipeWire graph through bounded asynchronous requests.",
        &contents,
    );

    match SoundService::spawn() {
        Ok(service) => wire_sound_page(
            &sound_page,
            &refresh,
            &widgets,
            Rc::clone(&update_bridges),
            service,
        ),
        Err(error) => {
            let update_bridges = Rc::clone(&update_bridges);
            refresh.connect_clicked(move |_| update_bridges());
            disable_sound_widgets(
                &widgets,
                &format!("The asynchronous sound worker could not start: {error}"),
            );
        }
    }

    sound_page
}

fn wire_sound_page(
    page: &gtk::ScrolledWindow,
    refresh: &gtk::Button,
    widgets: &SoundWidgets,
    update_bridges: Rc<dyn Fn()>,
    service: SoundService,
) {
    let controller = service.controller();
    let ui_state = Rc::new(RefCell::new(SoundUiState::default()));
    render_sound_state(widgets, &ui_state, &controller.state());

    {
        let controller = controller.clone();
        let operation = widgets.operation.clone();
        let update_bridges = Rc::clone(&update_bridges);
        refresh.connect_clicked(move |_| {
            update_bridges();
            report_sound_submission(&operation, controller.refresh());
        });
    }
    {
        let controller = controller.clone();
        let ui_state = Rc::clone(&ui_state);
        let operation = widgets.operation.clone();
        widgets
            .output_device
            .connect_selected_notify(move |dropdown| {
                if ui_state.borrow().synchronizing {
                    return;
                }
                let device = selected_device(dropdown, &ui_state.borrow().output_ids);
                if let Some(device) = device {
                    report_sound_submission(&operation, controller.set_default_output(&device));
                }
            });
    }
    {
        let controller = controller.clone();
        let ui_state = Rc::clone(&ui_state);
        let dropdown = widgets.output_device.clone();
        let operation = widgets.operation.clone();
        widgets.output_volume.connect_value_changed(move |scale| {
            if ui_state.borrow().synchronizing {
                return;
            }
            let device = selected_device(&dropdown, &ui_state.borrow().output_ids);
            let Some(device) = device else { return };
            let percent = scale.value().round().clamp(0.0, 150.0) as u16;
            match UserVolumePercent::new(percent) {
                Ok(volume) => report_sound_submission(
                    &operation,
                    controller.set_output_volume(&device, volume),
                ),
                Err(error) => report_sound_submission(&operation, Err(error)),
            }
        });
    }
    {
        let controller = controller.clone();
        let ui_state = Rc::clone(&ui_state);
        let dropdown = widgets.output_device.clone();
        let operation = widgets.operation.clone();
        widgets.output_mute.connect_active_notify(move |control| {
            if ui_state.borrow().synchronizing {
                return;
            }
            let device = selected_device(&dropdown, &ui_state.borrow().output_ids);
            if let Some(device) = device {
                report_sound_submission(
                    &operation,
                    controller.set_output_mute(&device, control.is_active()),
                );
            }
        });
    }
    {
        let controller = controller.clone();
        let ui_state = Rc::clone(&ui_state);
        let dropdown = widgets.output_device.clone();
        let operation = widgets.operation.clone();
        widgets.speaker_test.connect_clicked(move |_| {
            let result = match controller.state().speaker_test {
                SoundTestState::Starting | SoundTestState::Running => {
                    controller.stop_speaker_test()
                }
                SoundTestState::Idle | SoundTestState::Completed | SoundTestState::Failed => {
                    let device = selected_device(&dropdown, &ui_state.borrow().output_ids);
                    controller.start_speaker_test(device.as_ref())
                }
            };
            report_sound_submission(&operation, result);
        });
    }
    {
        let controller = controller.clone();
        let ui_state = Rc::clone(&ui_state);
        let operation = widgets.operation.clone();
        widgets
            .input_device
            .connect_selected_notify(move |dropdown| {
                if ui_state.borrow().synchronizing {
                    return;
                }
                let device = selected_device(dropdown, &ui_state.borrow().input_ids);
                if let Some(device) = device {
                    report_sound_submission(&operation, controller.set_default_input(&device));
                }
            });
    }
    {
        let controller = controller.clone();
        let ui_state = Rc::clone(&ui_state);
        let dropdown = widgets.input_device.clone();
        let operation = widgets.operation.clone();
        widgets.input_volume.connect_value_changed(move |scale| {
            if ui_state.borrow().synchronizing {
                return;
            }
            let device = selected_device(&dropdown, &ui_state.borrow().input_ids);
            let Some(device) = device else { return };
            let percent = scale.value().round().clamp(0.0, 150.0) as u16;
            match UserVolumePercent::new(percent) {
                Ok(volume) => report_sound_submission(
                    &operation,
                    controller.set_input_volume(&device, volume),
                ),
                Err(error) => report_sound_submission(&operation, Err(error)),
            }
        });
    }
    {
        let controller = controller.clone();
        let ui_state = Rc::clone(&ui_state);
        let dropdown = widgets.input_device.clone();
        let operation = widgets.operation.clone();
        widgets.input_mute.connect_active_notify(move |control| {
            if ui_state.borrow().synchronizing {
                return;
            }
            let device = selected_device(&dropdown, &ui_state.borrow().input_ids);
            if let Some(device) = device {
                report_sound_submission(
                    &operation,
                    controller.set_input_mute(&device, control.is_active()),
                );
            }
        });
    }
    {
        let controller = controller.clone();
        let ui_state = Rc::clone(&ui_state);
        let dropdown = widgets.input_device.clone();
        let operation = widgets.operation.clone();
        widgets.microphone_test.connect_clicked(move |_| {
            let result = match controller.state().microphone_test {
                SoundTestState::Starting | SoundTestState::Running => {
                    controller.stop_microphone_test()
                }
                SoundTestState::Idle | SoundTestState::Completed | SoundTestState::Failed => {
                    let device = selected_device(&dropdown, &ui_state.borrow().input_ids);
                    controller.start_microphone_test(device.as_ref())
                }
            };
            report_sound_submission(&operation, result);
        });
    }
    {
        let controller = controller.clone();
        page.connect_unmap(move |_| {
            // Always enqueue Stop after any possibly queued Start. This also
            // covers switching pages before the worker has published Starting.
            let _ = controller.stop_microphone_test();
        });
    }

    let weak_page = page.downgrade();
    let widgets = widgets.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let _keep_worker_alive = &service;
        if weak_page.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        let state = controller.state();
        if ui_state
            .borrow()
            .previous
            .as_ref()
            .map(|old| old.generation)
            != Some(state.generation)
        {
            render_sound_state(&widgets, &ui_state, &state);
        }
        glib::ControlFlow::Continue
    });
}

fn render_sound_state(
    widgets: &SoundWidgets,
    ui_state: &Rc<RefCell<SoundUiState>>,
    state: &SoundState,
) {
    let (outputs_changed, inputs_changed, playback_changed, recording_changed) = {
        let ui = ui_state.borrow();
        let previous = ui.previous.as_ref();
        (
            previous.is_none_or(|old| old.outputs != state.outputs),
            previous.is_none_or(|old| old.inputs != state.inputs),
            previous.is_none_or(|old| old.playback_streams != state.playback_streams),
            previous.is_none_or(|old| old.recording_streams != state.recording_streams),
        )
    };
    {
        let mut ui = ui_state.borrow_mut();
        ui.synchronizing = true;
        if outputs_changed {
            ui.output_ids = state
                .outputs
                .iter()
                .map(|device| device.id.clone())
                .collect();
        }
        if inputs_changed {
            ui.input_ids = state
                .inputs
                .iter()
                .map(|device| device.id.clone())
                .collect();
        }
    }

    if outputs_changed {
        install_device_model(&widgets.output_device, &state.outputs);
    }
    if inputs_changed {
        install_device_model(&widgets.input_device, &state.inputs);
    }
    select_default_device(
        &widgets.output_device,
        &state.outputs,
        state.default_output_name.as_deref(),
    );
    select_default_device(
        &widgets.input_device,
        &state.inputs,
        state.default_input_name.as_deref(),
    );

    let default_output = state.default_output();
    let output_percent = default_output
        .map(|device| device.volume_percent)
        .unwrap_or(0.0)
        .clamp(0.0, 150.0);
    widgets.output_volume.set_value(output_percent);
    widgets
        .output_volume_label
        .set_label(&format!("{output_percent:.0}%"));
    widgets
        .output_mute
        .set_active(default_output.is_some_and(|device| device.muted));

    let default_input = state.default_input();
    let input_percent = default_input
        .map(|device| device.volume_percent)
        .unwrap_or(0.0)
        .clamp(0.0, 150.0);
    widgets.input_volume.set_value(input_percent);
    widgets
        .input_volume_label
        .set_label(&format!("{input_percent:.0}%"));
    widgets
        .input_mute
        .set_active(default_input.is_some_and(|device| device.muted));

    if playback_changed || outputs_changed {
        rebuild_stream_list(
            &widgets.playback_streams,
            &state.playback_streams,
            &state.outputs,
            "playback",
        );
    }
    if recording_changed || inputs_changed {
        rebuild_stream_list(
            &widgets.recording_streams,
            &state.recording_streams,
            &state.inputs,
            "recording",
        );
    }

    render_sound_connection(widgets, state);
    render_sound_tests(widgets, state);
    let connected = state.connection == SoundConnection::Ready;
    let speaker_active = matches!(
        state.speaker_test,
        SoundTestState::Starting | SoundTestState::Running
    );
    let output_reason = if connected {
        "No default guest output is available."
    } else {
        "The guest-private sound server is not ready."
    };
    set_sound_control_available(
        &widgets.output_device,
        connected && !state.outputs.is_empty(),
        "Default output device",
        "Choose the default output in the guest-private sound graph.",
        output_reason,
    );
    set_sound_control_available(
        &widgets.output_volume,
        connected && default_output.is_some(),
        "Output volume",
        "Set guest output volume from 0 through 150 percent.",
        output_reason,
    );
    set_sound_control_available(
        &widgets.output_mute,
        connected && default_output.is_some(),
        "Mute output",
        "Mute or unmute the selected guest output.",
        output_reason,
    );
    set_sound_control_available(
        &widgets.speaker_test,
        speaker_active || (connected && default_output.is_some()),
        "Test speakers",
        "Explicitly play or stop a bounded left-then-right speaker test.",
        output_reason,
    );
    let input_reason = if connected {
        "No default guest input is available."
    } else {
        "The guest-private sound server is not ready; no microphone is open."
    };
    let microphone_active = matches!(
        state.microphone_test,
        SoundTestState::Starting | SoundTestState::Running
    );
    set_sound_control_available(
        &widgets.input_device,
        connected && !state.inputs.is_empty(),
        "Default input device",
        "Choose the default input in the guest-private sound graph.",
        input_reason,
    );
    set_sound_control_available(
        &widgets.input_volume,
        connected && default_input.is_some(),
        "Input volume",
        "Set guest input volume from 0 through 150 percent.",
        input_reason,
    );
    set_sound_control_available(
        &widgets.input_mute,
        connected && default_input.is_some(),
        "Mute input",
        "Mute or unmute the selected guest input.",
        input_reason,
    );
    set_sound_control_available(
        &widgets.microphone_test,
        microphone_active || (connected && default_input.is_some()),
        "Show microphone level",
        "Explicitly start or stop the bounded guest microphone level test.",
        input_reason,
    );

    let mut ui = ui_state.borrow_mut();
    ui.previous = Some(state.clone());
    ui.synchronizing = false;
}

fn render_sound_connection(widgets: &SoundWidgets, state: &SoundState) {
    let connection = match state.connection {
        SoundConnection::Connecting => state
            .diagnostic
            .clone()
            .unwrap_or_else(|| "Connecting to the guest-private sound server…".into()),
        SoundConnection::Ready => {
            let server = state.server_name.as_deref().unwrap_or("PipeWire-Pulse");
            let version = state
                .server_version
                .as_deref()
                .map(|value| format!(" {value}"))
                .unwrap_or_default();
            let subscription = if state.subscription_active {
                "live subscriptions active"
            } else {
                "live subscriptions unavailable; manual refresh remains available"
            };
            match state.diagnostic.as_deref() {
                Some(diagnostic) => {
                    format!("Connected to {server}{version}; {subscription}. {diagnostic}")
                }
                None => format!("Connected to {server}{version}; {subscription}."),
            }
        }
        SoundConnection::Unavailable => state
            .diagnostic
            .clone()
            .unwrap_or_else(|| "The guest-private PipeWire-Pulse service is unavailable.".into()),
    };
    widgets.connection.set_label(&connection);
    widgets.connection.remove_css_class("error");
    if state.connection == SoundConnection::Unavailable {
        widgets.connection.add_css_class("error");
    } else {
        widgets.connection.add_css_class("dim-label");
    }
    accessible(
        &widgets.connection,
        "Guest sound server status",
        &connection,
    );

    if let Some(feedback) = state.last_operation.as_ref() {
        set_sound_status(
            &widgets.operation,
            &feedback.message,
            feedback.status == SoundOperationStatus::Failed,
        );
    } else {
        set_sound_status(&widgets.operation, "No sound operation is pending.", false);
    }
}

fn render_sound_tests(widgets: &SoundWidgets, state: &SoundState) {
    let (speaker_label, speaker_status) = match state.speaker_test {
        SoundTestState::Idle => ("Test Speakers", "No speaker test is active."),
        SoundTestState::Starting => ("Stop Speaker Test", "Starting the speaker test…"),
        SoundTestState::Running => ("Stop Speaker Test", "Playing left, then right."),
        SoundTestState::Completed => ("Test Speakers", "Speaker test completed."),
        SoundTestState::Failed => ("Test Speakers", "Speaker test failed; see status above."),
    };
    widgets.speaker_test.set_label(speaker_label);
    widgets.speaker_status.set_label(speaker_status);

    match state.microphone_test {
        SoundTestState::Idle => {
            widgets.microphone_test.set_label("Show Microphone Level");
            set_microphone_meter(
                &widgets.microphone_meter,
                0.0,
                "Not active — no microphone is open",
            );
        }
        SoundTestState::Starting => {
            widgets.microphone_test.set_label("Stop Microphone Level");
            set_microphone_meter(
                &widgets.microphone_meter,
                0.0,
                "Starting microphone capture…",
            );
        }
        SoundTestState::Running => {
            widgets.microphone_test.set_label("Stop Microphone Level");
            if let Some(level) = state.microphone_level {
                set_microphone_meter(
                    &widgets.microphone_meter,
                    level.meter_fraction,
                    &format!(
                        "{:.0}% ({:.1} dBFS)",
                        level.meter_fraction * 100.0,
                        level.dbfs
                    ),
                );
            } else {
                set_microphone_meter(
                    &widgets.microphone_meter,
                    0.0,
                    "Listening — no samples received yet",
                );
            }
        }
        SoundTestState::Completed => {
            widgets.microphone_test.set_label("Show Microphone Level");
            set_microphone_meter(
                &widgets.microphone_meter,
                0.0,
                "Test ended — microphone released",
            );
        }
        SoundTestState::Failed => {
            widgets.microphone_test.set_label("Show Microphone Level");
            set_microphone_meter(
                &widgets.microphone_meter,
                0.0,
                "Test failed — microphone released",
            );
        }
    }
}

fn set_microphone_meter(meter: &gtk::ProgressBar, fraction: f64, text: &str) {
    meter.set_fraction(fraction.clamp(0.0, 1.0));
    meter.set_text(Some(text));
    accessible(meter, "Microphone level", text);
}

fn install_device_model(dropdown: &gtk::DropDown, devices: &[SoundDevice]) {
    let labels = devices
        .iter()
        .map(|device| {
            if device.description == device.id.name() {
                device.description.clone()
            } else {
                format!("{} — {}", device.description, device.id.name())
            }
        })
        .collect::<Vec<_>>();
    let label_refs = labels.iter().map(String::as_str).collect::<Vec<_>>();
    let model = gtk::StringList::new(&label_refs);
    dropdown.set_model(Some(&model));
}

fn select_default_device(
    dropdown: &gtk::DropDown,
    devices: &[SoundDevice],
    default_name: Option<&str>,
) {
    let selected = default_name
        .and_then(|name| devices.iter().position(|device| device.id.name() == name))
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(gtk::INVALID_LIST_POSITION);
    dropdown.set_selected(selected);
}

fn selected_device(dropdown: &gtk::DropDown, devices: &[DeviceId]) -> Option<DeviceId> {
    let selected = usize::try_from(dropdown.selected()).ok()?;
    devices.get(selected).cloned()
}

fn rebuild_stream_list(
    list: &gtk::ListBox,
    streams: &[SoundStreamInfo],
    devices: &[SoundDevice],
    kind: &str,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    if streams.is_empty() {
        let row = gtk::ListBoxRow::new();
        let label = wrapped_label(&format!("No active {kind} streams."));
        label.set_margin_top(8);
        label.set_margin_bottom(8);
        label.set_margin_start(10);
        label.set_margin_end(10);
        label.add_css_class("dim-label");
        row.set_child(Some(&label));
        list.append(&row);
    } else {
        for stream in streams {
            let row = gtk::ListBoxRow::new();
            let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
            content.set_margin_top(7);
            content.set_margin_bottom(7);
            content.set_margin_start(10);
            content.set_margin_end(10);
            let title = stream.application_name.as_deref().unwrap_or(&stream.name);
            let title_label = gtk::Label::new(Some(title));
            title_label.set_xalign(0.0);
            let route = devices
                .iter()
                .find(|device| device.id.index() == stream.route_device_index)
                .map(|device| device.description.as_str())
                .unwrap_or("unknown device");
            let volume = stream
                .volume_percent
                .map(|percent| format!("{percent:.0}%"))
                .unwrap_or_else(|| "server controlled".into());
            let state = if stream.corked { "paused" } else { "active" };
            let mute = if stream.muted { ", muted" } else { "" };
            let detail = format!("{} · {volume} · {state}{mute} · {route}", stream.name);
            let detail_label = wrapped_label(&detail);
            detail_label.add_css_class("dim-label");
            content.append(&title_label);
            content.append(&detail_label);
            row.set_child(Some(&content));
            accessible(&row, title, &detail);
            list.append(&row);
        }
    }
    accessible(
        list,
        &format!("Active {kind} streams"),
        &format!(
            "{} active {kind} streams in the guest-private graph.",
            streams.len()
        ),
    );
}

fn set_sound_control_available<W: IsA<gtk::Widget> + IsA<gtk::Accessible>>(
    widget: &W,
    enabled: bool,
    label: &str,
    description: &str,
    disabled: &str,
) {
    widget.set_sensitive(enabled);
    widget.update_state(&[gtk::accessible::State::Disabled(!enabled)]);
    if enabled {
        widget.set_tooltip_text(None);
        accessible(widget, label, description);
    } else {
        widget.set_tooltip_text(Some(disabled));
        accessible(widget, label, disabled);
    }
}

fn report_sound_submission(label: &gtk::Label, result: Result<SoundRequestId, SoundClientError>) {
    match result {
        Ok(_) => set_sound_status(label, "Sound request queued…", false),
        Err(error) => set_sound_status(label, &format!("Sound request rejected: {error}"), true),
    }
}

fn set_sound_status(label: &gtk::Label, message: &str, error: bool) {
    label.set_label(message);
    label.remove_css_class("dim-label");
    label.remove_css_class("error");
    label.add_css_class(if error { "error" } else { "dim-label" });
    accessible(label, "Sound operation status", message);
}

fn disable_sound_widgets(widgets: &SoundWidgets, reason: &str) {
    widgets.connection.set_label(reason);
    widgets.connection.add_css_class("error");
    set_sound_status(&widgets.operation, reason, true);
    disabled_reason(&widgets.output_device, "Default output device", reason);
    disabled_reason(&widgets.output_volume, "Output volume", reason);
    disabled_reason(&widgets.output_mute, "Mute output", reason);
    disabled_reason(&widgets.speaker_test, "Test speakers", reason);
    disabled_reason(&widgets.input_device, "Default input device", reason);
    disabled_reason(&widgets.input_volume, "Input volume", reason);
    disabled_reason(&widgets.input_mute, "Mute input", reason);
    disabled_reason(&widgets.microphone_test, "Show microphone level", reason);
    set_microphone_meter(
        &widgets.microphone_meter,
        0.0,
        "Unavailable — no microphone is open",
    );
    rebuild_stream_list(&widgets.playback_streams, &[], &[], "playback");
    rebuild_stream_list(&widgets.recording_streams, &[], &[], "recording");
}

fn set_bridge_label(label: &gtk::Label, state: BridgeState) {
    // Settings exposes only the approved read-only state vocabulary. Runtime
    // failure detail remains in the separate diagnostic label.
    let visible = match state {
        BridgeState::Failed => BridgeState::Unavailable.label(),
        state => state.label(),
    };
    label.set_label(visible);
    accessible(
        label,
        &format!("Bridge status: {visible}"),
        "Current host-authorized media bridge state. Permission can only be changed in the host Devices control.",
    );
}

const SHORTCUT_HELPER_PATH: &str = "/usr/libexec/wildbuzzard-shortcut-helper";
const MAX_HELPER_RESPONSE_BYTES: usize = 1024 * 1024;

fn build_applications_page(
    window: &gtk::ApplicationWindow,
    store: Rc<RefCell<SettingsStore>>,
) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);
    let registrations_section = section(
        "Registered AppImages",
        Some(
            "Registrations link to their original guest-visible file. The AppImage is not copied into the machine.",
        ),
    );
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    accessible(
        &list,
        "Registered AppImages",
        "Every valid AppImage registration, including registrations whose target is currently missing.",
    );
    let registrations_diagnostic = wrapped_label("");
    registrations_diagnostic.add_css_class("dim-label");
    let refresh = gtk::Button::with_label("Refresh");
    accessible(
        &refresh,
        "Refresh AppImage registrations",
        "Read the persistent AppImage registration directory again.",
    );
    refresh.set_halign(gtk::Align::Start);
    registrations_section.append(&list);
    registrations_section.append(&registrations_diagnostic);
    registrations_section.append(&refresh);
    contents.append(&registrations_section);

    let desktop_section = section(
        "Desktop",
        Some(
            "Applications-menu and desktop shortcut actions use the same opaque registration IDs. Desktop file operations remain guest-local and confirmed before deletion.",
        ),
    );
    let helper_available = Path::new(SHORTCUT_HELPER_PATH).is_file();
    let desktop_status = wrapped_label(if helper_available {
        "The audited shortcut helper is ready. AppImage launch, relink, Applications-menu, desktop-shortcut, and reveal actions execute without a shell and refresh only after a confirmed helper response."
    } else {
        "The dedicated shortcut helper is not installed. Registration state remains visible, but mutation and launch controls are disabled."
    });
    desktop_status.add_css_class("dim-label");
    accessible(
        &desktop_status,
        "Desktop integration status",
        if helper_available {
            "The dedicated helper is connected and registration actions require confirmed structured responses."
        } else {
            "The dedicated helper is unavailable; no unsupported action is reported as successful."
        },
    );
    desktop_section.append(&desktop_status);
    contents.append(&desktop_section);

    let registration_directory = store.borrow().paths.appimage_registration_dir();
    let update_registrations = {
        let list = list.clone();
        let diagnostic = registrations_diagnostic.clone();
        let window = window.clone();
        move || populate_registration_list(&list, &diagnostic, &registration_directory, &window)
    };
    update_registrations();
    refresh.connect_clicked(move |_| update_registrations());

    page(
        "Applications & Desktop",
        "Inspect and manage link-in-place AppImages through the audited guest-local shortcut helper.",
        &contents,
    )
}

fn populate_registration_list(
    list: &gtk::ListBox,
    diagnostic: &gtk::Label,
    directory: &Path,
    window: &gtk::ApplicationWindow,
) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let catalog = load_registrations(directory);
    if catalog.registrations.is_empty() {
        let empty = gtk::ListBoxRow::new();
        empty.set_activatable(false);
        empty.set_selectable(false);
        let label = wrapped_label("No AppImages are registered.");
        label.set_margin_top(12);
        label.set_margin_bottom(12);
        label.set_margin_start(12);
        label.set_margin_end(12);
        empty.set_child(Some(&label));
        list.append(&empty);
    } else {
        for registration in catalog.registrations {
            let row = gtk::ListBoxRow::new();
            row.set_activatable(false);
            row.set_selectable(false);
            let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
            content.set_margin_top(10);
            content.set_margin_bottom(10);
            content.set_margin_start(12);
            content.set_margin_end(12);
            let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let icon_name = registration.id.icon_name();
            let icon = gtk::Image::from_icon_name(&icon_name);
            icon.set_pixel_size(32);
            header.append(&icon);
            let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
            labels.set_hexpand(true);
            let name = gtk::Label::new(Some(&registration.display_name));
            name.set_xalign(0.0);
            name.add_css_class("heading");
            labels.append(&name);
            let path = gtk::Label::new(Some(&registration.target_path.display().to_string()));
            path.set_xalign(0.0);
            path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            path.set_selectable(true);
            path.add_css_class("dim-label");
            labels.append(&path);
            header.append(&labels);
            let target_present = registration.target_path.is_file();
            let target_status = if target_present {
                "Target present"
            } else {
                "Target missing — Relink will use the guest file chooser"
            };
            let target = gtk::Label::new(Some(target_status));
            target.set_xalign(0.0);
            if !target_present {
                target.add_css_class("warning");
            }
            content.append(&header);
            content.append(&target);
            let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let launch = gtk::Button::with_label("Launch");
            let relink = gtk::Button::with_label("Relink");
            let desktop_command = if registration.desktop_shortcut {
                "remove-desktop"
            } else {
                "add-desktop"
            };
            let desktop = gtk::Button::with_label(if registration.desktop_shortcut {
                "Remove Desktop Shortcut"
            } else {
                "Add Desktop Shortcut"
            });
            let applications_command = if registration.applications_launcher {
                "remove-applications"
            } else {
                "add-applications"
            };
            let applications = gtk::Button::with_label(if registration.applications_launcher {
                "Remove from Applications"
            } else {
                "Add to Applications"
            });
            let reveal = gtk::Button::with_label("Reveal Target");
            let action_specs = [
                (&launch, "launch", "Launch registered AppImage"),
                (&relink, "choose-relink", "Relink registered AppImage"),
                (&desktop, desktop_command, "Change desktop shortcut"),
                (
                    &applications,
                    applications_command,
                    "Change Applications-menu registration",
                ),
                (&reveal, "reveal", "Reveal registered AppImage target"),
            ];
            if Path::new(SHORTCUT_HELPER_PATH).is_file() {
                for (button, command, label) in action_specs {
                    accessible(
                        button,
                        &format!("{label}: {}", registration.display_name),
                        "Run this action through the audited guest-local helper and refresh only after its structured response is confirmed.",
                    );
                    connect_registration_action(
                        button,
                        command,
                        registration.id.to_string(),
                        window,
                        list,
                        diagnostic,
                        directory.to_path_buf(),
                    );
                    actions.append(button);
                }
                if !target_present {
                    reveal.set_sensitive(false);
                    reveal.set_tooltip_text(Some(
                        "The target is missing. Relink it before revealing its location.",
                    ));
                }
            } else {
                let reason = "The audited shortcut helper is not installed; Settings will not execute or mutate this registration directly.";
                for (button, _, label) in action_specs {
                    disabled_reason(button, label, reason);
                    actions.append(button);
                }
            }
            content.append(&actions);
            accessible(
                &row,
                &format!("{} AppImage registration", registration.display_name),
                &format!(
                    "Registered target {}. Applications launcher: {}. Desktop shortcut: {}.",
                    registration.target_path.display(),
                    registration.applications_launcher,
                    registration.desktop_shortcut
                ),
            );
            row.set_child(Some(&content));
            list.append(&row);
        }
    }
    if catalog.diagnostics.is_empty() {
        diagnostic.set_label("Registration records were read successfully.");
    } else {
        diagnostic.set_label(&catalog.diagnostics.join("\n"));
    }
}

#[derive(Debug, Deserialize)]
struct ShortcutHelperEnvelope {
    ok: bool,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShortcutHelperOutcome {
    Confirmed,
    Cancelled,
}

fn connect_registration_action(
    button: &gtk::Button,
    command: &'static str,
    registration_id: String,
    window: &gtk::ApplicationWindow,
    list: &gtk::ListBox,
    diagnostic: &gtk::Label,
    registration_directory: PathBuf,
) {
    let window = window.clone();
    let list = list.clone();
    let diagnostic = diagnostic.clone();
    button.connect_clicked(move |button| {
        button.set_sensitive(false);
        diagnostic.set_label(&format!("Running {command}…"));
        let id = OsStr::new(&registration_id);
        let subprocess = match gio::Subprocess::newv(
            [OsStr::new(SHORTCUT_HELPER_PATH), OsStr::new(command), id].as_slice(),
            gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
        ) {
            Ok(subprocess) => subprocess,
            Err(error) => {
                button.set_sensitive(true);
                diagnostic.set_label("The shortcut helper could not be started.");
                show_error(
                    &window,
                    "AppImage action failed",
                    &format!("Cannot start {SHORTCUT_HELPER_PATH}: {error}"),
                );
                return;
            }
        };
        let completed_process = subprocess.clone();
        let button = button.clone();
        let window = window.clone();
        let list = list.clone();
        let diagnostic = diagnostic.clone();
        let directory = registration_directory.clone();
        subprocess.communicate_utf8_async(None, None::<&gio::Cancellable>, move |result| {
            button.set_sensitive(true);
            match result
                .map_err(|error| error.to_string())
                .and_then(|(stdout, stderr)| {
                    parse_shortcut_helper_response(
                        completed_process.is_successful(),
                        stdout.as_deref().unwrap_or_default(),
                        stderr.as_deref().unwrap_or_default(),
                    )
                }) {
                Ok(ShortcutHelperOutcome::Confirmed) => {
                    diagnostic.set_label("The AppImage action completed successfully.");
                    populate_registration_list(&list, &diagnostic, &directory, &window);
                }
                Ok(ShortcutHelperOutcome::Cancelled) => {
                    diagnostic.set_label("Relink was cancelled; registration was unchanged.");
                }
                Err(error) => {
                    diagnostic.set_label("The AppImage action failed; registration was refreshed.");
                    populate_registration_list(&list, &diagnostic, &directory, &window);
                    show_error(&window, "AppImage action failed", &error);
                }
            }
        });
    });
}

fn parse_shortcut_helper_response(
    process_succeeded: bool,
    stdout: &str,
    stderr: &str,
) -> Result<ShortcutHelperOutcome, String> {
    if stdout.len() > MAX_HELPER_RESPONSE_BYTES || stderr.len() > MAX_HELPER_RESPONSE_BYTES {
        return Err("The shortcut helper response exceeded the 1 MiB safety limit.".into());
    }
    if !process_succeeded {
        let detail = serde_json::from_str::<ShortcutHelperEnvelope>(stderr)
            .ok()
            .and_then(|response| response.error)
            .or_else(|| (!stderr.trim().is_empty()).then(|| stderr.trim().to_owned()))
            .unwrap_or_else(|| "The shortcut helper exited unsuccessfully.".into());
        return Err(detail);
    }
    let response: ShortcutHelperEnvelope = serde_json::from_str(stdout)
        .map_err(|error| format!("The shortcut helper returned invalid JSON: {error}"))?;
    if response.outcome.as_deref() == Some("cancelled") {
        return Ok(ShortcutHelperOutcome::Cancelled);
    }
    if !response.ok {
        return Err(response.error.unwrap_or_else(|| {
            "The shortcut helper did not confirm the requested action.".into()
        }));
    }
    Ok(ShortcutHelperOutcome::Confirmed)
}

#[derive(Clone)]
struct UpdateWidgets {
    status: gtk::Label,
    checked: gtk::Label,
    count: gtk::Label,
    download: gtk::Label,
    runtime: gtk::Label,
    progress: gtk::ProgressBar,
    diagnostic: gtk::Label,
    operation: gtk::Label,
    packages: gtk::ListBox,
    check_now: gtk::Button,
    update_now: gtk::Button,
    retry_repair: gtk::Button,
    cancel_download: gtk::Button,
}

fn set_update_action_available(
    button: &gtk::Button,
    available: bool,
    accessible_name: &str,
    unavailable_reason: &str,
) {
    button.set_sensitive(available);
    button.set_tooltip_text((!available).then_some(unavailable_reason));
    accessible(
        button,
        accessible_name,
        if available {
            "This fixed updater action is currently available."
        } else {
            unavailable_reason
        },
    );
    button.update_state(&[gtk::accessible::State::Disabled(!available)]);
}

fn update_progress_text(state: &UpdateState) -> Option<(f64, String)> {
    let progress = state.progress.as_ref()?;
    let fraction = if progress.total == 0 {
        0.0
    } else {
        progress.completed as f64 / progress.total as f64
    };
    let amount = match progress.unit {
        UpdateProgressUnit::Bytes => format!(
            "{} of {}",
            format_bytes(progress.completed),
            format_bytes(progress.total)
        ),
        UpdateProgressUnit::Packages => {
            format!("{} of {} packages", progress.completed, progress.total)
        }
        UpdateProgressUnit::Steps => format!("{} of {} steps", progress.completed, progress.total),
    };
    Some((
        fraction.clamp(0.0, 1.0),
        progress
            .detail
            .as_ref()
            .map_or(amount.clone(), |detail| format!("{detail} · {amount}")),
    ))
}

fn render_update_page(
    widgets: &UpdateWidgets,
    state: &UpdateState,
    state_diagnostic: Option<&str>,
    service_available: bool,
    service_diagnostic: Option<&str>,
) {
    widgets.status.set_label(update_status_label(state.status));
    widgets.checked.set_label(
        &state
            .checked_at_unix_seconds
            .map_or_else(|| "Never".into(), |value| value.to_string()),
    );
    widgets.count.set_label(&state.packages.len().to_string());
    widgets
        .download
        .set_label(&format_bytes(state.download_size));
    widgets.runtime.set_label(
        state
            .runtime_revision
            .as_deref()
            .filter(|_| state.runtime_ready)
            .unwrap_or("Not ready"),
    );

    if let Some((fraction, text)) = update_progress_text(state) {
        widgets.progress.set_visible(true);
        widgets.progress.set_fraction(fraction);
        widgets.progress.set_text(Some(&text));
        accessible(&widgets.progress, "Update progress", &text);
    } else {
        widgets.progress.set_visible(false);
        widgets.progress.set_fraction(0.0);
        widgets.progress.set_text(None);
    }

    let active = matches!(
        state.status,
        UpdateStatus::Checking | UpdateStatus::Installing
    );
    set_update_action_available(
        &widgets.check_now,
        service_available && !active,
        "Check for updates",
        if service_available {
            "Wait for the active updater operation to finish."
        } else {
            "The fixed-operation updater service is unavailable."
        },
    );
    set_update_action_available(
        &widgets.update_now,
        service_available
            && state.runtime_ready
            && state.status == UpdateStatus::Available
            && state.plan_generation.is_some(),
        "Install available updates",
        "Update Now requires an available exact plan and a validated protected runtime revision.",
    );
    set_update_action_available(
        &widgets.retry_repair,
        service_available
            && state.status == UpdateStatus::Failed
            && state.repair_available
            && state.plan_generation.is_some(),
        "Retry or repair update",
        "Repair is enabled only when the updater proves that its own failed dpkg transaction remains incomplete.",
    );
    let cancellable = service_available
        && state.status == UpdateStatus::Installing
        && state
            .progress
            .as_ref()
            .is_some_and(|progress| progress.cancellable);
    widgets.cancel_download.set_visible(cancellable);
    set_update_action_available(
        &widgets.cancel_download,
        cancellable,
        "Cancel package download",
        "Only the download phase can be cancelled; dpkg installation is never interrupted.",
    );

    let mut messages = Vec::new();
    if let Some(message) = state_diagnostic {
        messages.push(message.to_owned());
    }
    if let Some(message) = service_diagnostic {
        messages.push(message.to_owned());
    }
    if !state.runtime_ready {
        messages.push(
            "Package installation is blocked until the protected versioned desktop runtime passes readiness verification."
                .into(),
        );
    }
    if let Some(failure) = &state.failure {
        messages.push(format!("Failure: {failure}"));
    }
    if !state.repository_errors.is_empty() {
        for error in state.repository_errors.iter().take(12) {
            messages.push(format!("Repository: {error}"));
        }
        if state.repository_errors.len() > 12 {
            messages.push(format!(
                "{} additional repository errors are retained in updater state.",
                state.repository_errors.len() - 12
            ));
        }
    }
    messages.extend(state.restart_reasons.iter().cloned());
    if let Some(log) = &state.last_log_id {
        messages.push(format!("Operation evidence: {log}"));
    }
    widgets.diagnostic.set_label(&messages.join("\n"));

    while let Some(child) = widgets.packages.first_child() {
        widgets.packages.remove(&child);
    }
    if state.packages.is_empty() {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(false);
        row.set_selectable(false);
        let label = wrapped_label("No package plan is available.");
        label.set_margin_top(10);
        label.set_margin_bottom(10);
        label.set_margin_start(12);
        label.set_margin_end(12);
        row.set_child(Some(&label));
        widgets.packages.append(&row);
    } else {
        for package in &state.packages {
            let row = gtk::ListBoxRow::new();
            row.set_activatable(false);
            row.set_selectable(false);
            let box_ = gtk::Box::new(gtk::Orientation::Vertical, 3);
            box_.set_margin_top(8);
            box_.set_margin_bottom(8);
            box_.set_margin_start(12);
            box_.set_margin_end(12);
            let action = match package.action {
                UpdateAction::Upgrade => "Upgrade",
                UpdateAction::Install => "Install dependency",
            };
            let name = gtk::Label::new(Some(&format!("{action}: {}", package.name)));
            name.set_xalign(0.0);
            name.add_css_class("heading");
            box_.append(&name);
            let versions = wrapped_label(&format!(
                "{} → {} · {}{}",
                package.installed_version,
                package.candidate_version,
                format_bytes(package.download_size),
                package
                    .security_origin
                    .as_ref()
                    .map_or_else(String::new, |origin| {
                        format!(" · security origin: {origin}")
                    })
            ));
            versions.add_css_class("dim-label");
            box_.append(&versions);
            accessible(
                &row,
                &format!("{} package update", package.name),
                &versions.label(),
            );
            row.set_child(Some(&box_));
            widgets.packages.append(&row);
        }
    }
}

fn poll_update_state(
    state: Rc<RefCell<UpdateState>>,
    state_diagnostic: Rc<RefCell<Option<String>>>,
    render: Rc<dyn Fn()>,
    baseline_generation: u64,
) {
    let attempts = Rc::new(Cell::new(0_u32));
    let saw_active_state = Rc::new(Cell::new(false));
    glib::timeout_add_local(Duration::from_millis(250), move || {
        attempts.set(attempts.get().saturating_add(1));
        let view = load_update_view(Path::new(UPDATE_STATE_PATH));
        state.replace(view.state);
        state_diagnostic.replace(view.diagnostic);
        render();
        let active = matches!(
            state.borrow().status,
            UpdateStatus::Checking | UpdateStatus::Installing
        );
        saw_active_state.set(saw_active_state.get() || active);
        let generation = state.borrow().state_generation;
        if update_poll_is_complete(
            attempts.get(),
            active,
            saw_active_state.get(),
            generation,
            baseline_generation,
        ) {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn update_poll_is_complete(
    attempts: u32,
    active: bool,
    saw_active_state: bool,
    generation: u64,
    baseline_generation: u64,
) -> bool {
    attempts >= 2_400
        || (attempts >= 3 && !active && (saw_active_state || generation > baseline_generation))
}

fn show_update_confirmation(
    parent: &gtk::ApplicationWindow,
    state: &UpdateState,
    confirmed: impl FnOnce() + 'static,
) {
    let Some(generation) = state.plan_generation.as_deref() else {
        show_error(
            parent,
            "No update plan",
            "Run Check before installing package updates.",
        );
        return;
    };
    let dialog = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title("Confirm Exact Update Plan")
        .default_width(680)
        .default_height(520)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&heading("Install this exact package plan?"));
    content.append(&wrapped_label(&format!(
        "{} packages · {} · opaque generation {}. Versions are revalidated under the apt lock before dpkg runs.",
        state.packages.len(),
        format_bytes(state.download_size),
        generation
    )));
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    for package in &state.packages {
        let row = gtk::ListBoxRow::new();
        row.set_selectable(false);
        row.set_activatable(false);
        let label = wrapped_label(&format!(
            "{}: {} → {} ({})",
            package.name,
            package.installed_version,
            package.candidate_version,
            format_bytes(package.download_size)
        ));
        label.set_margin_top(7);
        label.set_margin_bottom(7);
        label.set_margin_start(10);
        label.set_margin_end(10);
        row.set_child(Some(&label));
        list.append(&row);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&list)
        .build();
    content.append(&scroll);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let install = gtk::Button::with_label("Install Exact Plan");
    install.add_css_class("suggested-action");
    accessible(
        &install,
        "Confirm exact update plan",
        "Install only the displayed opaque package plan after one final candidate check.",
    );
    actions.append(&cancel);
    actions.append(&install);
    content.append(&actions);
    dialog.set_child(Some(&content));
    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }
    let callback = Rc::new(RefCell::new(Some(confirmed)));
    {
        let dialog = dialog.clone();
        let callback = Rc::clone(&callback);
        install.connect_clicked(move |_| {
            dialog.close();
            if let Some(callback) = callback.borrow_mut().take() {
                callback();
            }
        });
    }
    dialog.present();
}

fn build_updates_page(window: &gtk::ApplicationWindow) -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);
    let status_section = section(
        "Debian Package Updates",
        Some(
            "Guest package updates are separate from replacing the host AppImage. Updates are checked on request or by the fixed timer and are never installed automatically.",
        ),
    );
    let status = value_label("Never checked");
    let checked = value_label("Never");
    let count = value_label("0");
    let download = value_label("0 bytes");
    let runtime = value_label("Not ready");
    status_section.append(&setting_row("Status", "Validated updater state.", &status));
    status_section.append(&setting_row(
        "Last checked",
        "Unix timestamp published by the updater.",
        &checked,
    ));
    status_section.append(&setting_row(
        "Available packages",
        "Exact package candidates in the current plan.",
        &count,
    ));
    status_section.append(&setting_row(
        "Download size",
        "Checked sum of package download sizes.",
        &download,
    ));
    status_section.append(&setting_row(
        "Protected runtime",
        "Exact versioned desktop runtime proven outside dpkg ownership.",
        &runtime,
    ));
    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);
    progress.set_visible(false);
    status_section.append(&progress);
    let diagnostic = wrapped_label("");
    diagnostic.add_css_class("dim-label");
    status_section.append(&diagnostic);
    let operation = wrapped_label("");
    operation.add_css_class("dim-label");
    status_section.append(&operation);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let check_now = gtk::Button::with_label("Check Now");
    let update_now = gtk::Button::with_label("Update Now");
    let retry_repair = gtk::Button::with_label("Retry / Repair");
    let cancel_download = gtk::Button::with_label("Cancel Download");
    let refresh = gtk::Button::with_label("Refresh");
    cancel_download.set_visible(false);
    accessible(
        &refresh,
        "Refresh update information",
        "Read the validated updater state file without starting a package operation.",
    );
    for button in [
        &check_now,
        &update_now,
        &retry_repair,
        &cancel_download,
        &refresh,
    ] {
        actions.append(button);
    }
    status_section.append(&actions);
    contents.append(&status_section);

    let packages_section = section(
        "Plan",
        Some("Exact installed and candidate versions from the current opaque updater plan."),
    );
    let packages = gtk::ListBox::new();
    packages.set_selection_mode(gtk::SelectionMode::None);
    packages.add_css_class("boxed-list");
    accessible(
        &packages,
        "Update package plan",
        "Exact packages, versions, download sizes, and security origins in the current plan.",
    );
    packages_section.append(&packages);
    contents.append(&packages_section);

    let widgets = UpdateWidgets {
        status,
        checked,
        count,
        download,
        runtime,
        progress,
        diagnostic,
        operation,
        packages,
        check_now: check_now.clone(),
        update_now: update_now.clone(),
        retry_repair: retry_repair.clone(),
        cancel_download: cancel_download.clone(),
    };
    let initial = load_update_view(Path::new(UPDATE_STATE_PATH));
    let state = Rc::new(RefCell::new(initial.state));
    let state_diagnostic = Rc::new(RefCell::new(initial.diagnostic));
    let service_available = Rc::new(Cell::new(false));
    let service_diagnostic = Rc::new(RefCell::new(None::<String>));
    let render: Rc<dyn Fn()> = {
        let widgets = widgets.clone();
        let state = Rc::clone(&state);
        let state_diagnostic = Rc::clone(&state_diagnostic);
        let service_available = Rc::clone(&service_available);
        let service_diagnostic = Rc::clone(&service_diagnostic);
        Rc::new(move || {
            render_update_page(
                &widgets,
                &state.borrow(),
                state_diagnostic.borrow().as_deref(),
                service_available.get(),
                service_diagnostic.borrow().as_deref(),
            );
        })
    };
    render();

    let probe_service: Rc<dyn Fn()> = {
        let state = Rc::clone(&state);
        let state_diagnostic = Rc::clone(&state_diagnostic);
        let service_available = Rc::clone(&service_available);
        let service_diagnostic = Rc::clone(&service_diagnostic);
        let render = Rc::clone(&render);
        Rc::new(move || {
            let state = Rc::clone(&state);
            let state_diagnostic = Rc::clone(&state_diagnostic);
            let service_available = Rc::clone(&service_available);
            let service_diagnostic = Rc::clone(&service_diagnostic);
            let render = Rc::clone(&render);
            updater_client::get_state(move |result| {
                match result {
                    Ok(remote) => {
                        state.replace(remote);
                        state_diagnostic.replace(None);
                        service_available.set(true);
                        service_diagnostic.replace(None);
                    }
                    Err(error) => {
                        service_available.set(false);
                        service_diagnostic.replace(Some(error));
                    }
                }
                render();
            });
        })
    };
    probe_service();

    {
        let probe_service = Rc::clone(&probe_service);
        refresh.connect_clicked(move |_| probe_service());
    }
    {
        let operation = widgets.operation.clone();
        let service_available = Rc::clone(&service_available);
        let service_diagnostic = Rc::clone(&service_diagnostic);
        let state = Rc::clone(&state);
        let state_diagnostic = Rc::clone(&state_diagnostic);
        let render = Rc::clone(&render);
        check_now.connect_clicked(move |_| {
            let baseline_generation = state.borrow().state_generation;
            operation.set_label("Submitting a fixed repository check…");
            let operation = operation.clone();
            let service_available = Rc::clone(&service_available);
            let service_diagnostic = Rc::clone(&service_diagnostic);
            let state = Rc::clone(&state);
            let state_diagnostic = Rc::clone(&state_diagnostic);
            let render = Rc::clone(&render);
            updater_client::submit(UpdateRequest::Check, move |result| match result {
                Ok(_) => {
                    service_available.set(true);
                    service_diagnostic.replace(None);
                    operation.set_label("Repository check accepted.");
                    poll_update_state(state, state_diagnostic, render, baseline_generation);
                }
                Err(error) => {
                    operation.set_label(&error);
                    service_diagnostic.replace(Some(error));
                    render();
                }
            });
        });
    }
    {
        let parent = window.clone();
        let operation = widgets.operation.clone();
        let current_state = Rc::clone(&state);
        let service_diagnostic = Rc::clone(&service_diagnostic);
        let state_diagnostic = Rc::clone(&state_diagnostic);
        let render = Rc::clone(&render);
        update_now.connect_clicked(move |_| {
            let snapshot = current_state.borrow().clone();
            let Some(generation) = snapshot.plan_generation.clone() else {
                show_error(
                    &parent,
                    "No update plan",
                    "Run Check before installing updates.",
                );
                return;
            };
            let operation = operation.clone();
            let current_state = Rc::clone(&current_state);
            let service_diagnostic = Rc::clone(&service_diagnostic);
            let state_diagnostic = Rc::clone(&state_diagnostic);
            let render = Rc::clone(&render);
            show_update_confirmation(&parent, &snapshot, move || {
                let baseline_generation = current_state.borrow().state_generation;
                operation.set_label("Submitting the confirmed exact plan…");
                let operation = operation.clone();
                let current_state = Rc::clone(&current_state);
                let service_diagnostic = Rc::clone(&service_diagnostic);
                let state_diagnostic = Rc::clone(&state_diagnostic);
                let render = Rc::clone(&render);
                updater_client::submit(UpdateRequest::InstallPlan(generation), move |result| {
                    match result {
                        Ok(_) => {
                            service_diagnostic.replace(None);
                            operation.set_label("Exact update plan accepted.");
                            poll_update_state(
                                current_state,
                                state_diagnostic,
                                render,
                                baseline_generation,
                            );
                        }
                        Err(error) => {
                            operation.set_label(&error);
                            service_diagnostic.replace(Some(error));
                            render();
                        }
                    }
                });
            });
        });
    }
    {
        let operation = widgets.operation.clone();
        let current_state = Rc::clone(&state);
        let service_diagnostic = Rc::clone(&service_diagnostic);
        let state_diagnostic = Rc::clone(&state_diagnostic);
        let render = Rc::clone(&render);
        retry_repair.connect_clicked(move |_| {
            let Some(generation) = current_state.borrow().plan_generation.clone() else {
                return;
            };
            let baseline_generation = current_state.borrow().state_generation;
            operation.set_label("Submitting authorized package repair…");
            let operation = operation.clone();
            let current_state = Rc::clone(&current_state);
            let service_diagnostic = Rc::clone(&service_diagnostic);
            let state_diagnostic = Rc::clone(&state_diagnostic);
            let render = Rc::clone(&render);
            updater_client::submit(UpdateRequest::RetryRepair(generation), move |result| {
                match result {
                    Ok(_) => {
                        service_diagnostic.replace(None);
                        operation.set_label("Authorized repair accepted.");
                        poll_update_state(
                            current_state,
                            state_diagnostic,
                            render,
                            baseline_generation,
                        );
                    }
                    Err(error) => {
                        operation.set_label(&error);
                        service_diagnostic.replace(Some(error));
                        render();
                    }
                }
            });
        });
    }
    {
        let operation = widgets.operation.clone();
        let current_state = Rc::clone(&state);
        let service_diagnostic = Rc::clone(&service_diagnostic);
        let render = Rc::clone(&render);
        cancel_download.connect_clicked(move |_| {
            let Some(generation) = current_state.borrow().plan_generation.clone() else {
                return;
            };
            operation.set_label("Requesting download cancellation…");
            let operation = operation.clone();
            let service_diagnostic = Rc::clone(&service_diagnostic);
            let render = Rc::clone(&render);
            updater_client::submit(UpdateRequest::CancelDownload(generation), move |result| {
                match result {
                    Ok(_) => {
                        service_diagnostic.replace(None);
                        operation.set_label("Download cancellation requested.");
                    }
                    Err(error) => {
                        operation.set_label(&error);
                        service_diagnostic.replace(Some(error));
                    }
                }
                render();
            });
        });
    }

    page(
        "Updates",
        "Inspect the fixed Debian package plan. Installation remains manual, confirmed, and disabled until the protected runtime gate is ready.",
        &contents,
    )
}

fn update_status_label(status: UpdateStatus) -> &'static str {
    match status {
        UpdateStatus::NeverChecked => "Never checked",
        UpdateStatus::Checking => "Checking",
        UpdateStatus::UpToDate => "Up to date",
        UpdateStatus::Available => "Updates available",
        UpdateStatus::Installing => "Installing",
        UpdateStatus::Failed => "Failed",
        UpdateStatus::RestartRecommended => "Restart recommended",
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes_f64 = bytes as f64;
    if bytes_f64 >= GIB {
        format!("{:.2} GiB", bytes_f64 / GIB)
    } else if bytes_f64 >= MIB {
        format!("{:.1} MiB", bytes_f64 / MIB)
    } else if bytes_f64 >= KIB {
        format!("{:.1} KiB", bytes_f64 / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

fn build_about_page() -> gtk::ScrolledWindow {
    let contents = gtk::Box::new(gtk::Orientation::Vertical, 22);
    let identity = section("Wild Buzzard Settings", None);
    let identity_row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    let icon = gtk::Image::from_icon_name("wildbuzzard-settings");
    icon.set_pixel_size(96);
    accessible(
        &icon,
        "Wild Buzzard Settings icon",
        "Project-owned icon loaded by name from the active Wild Buzzard icon theme.",
    );
    identity_row.append(&icon);
    let build = about_build();
    let details = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let product = gtk::Label::new(Some(&build.product));
    product.set_xalign(0.0);
    product.add_css_class("title-2");
    details.append(&product);
    let version = gtk::Label::new(Some(&format!("Version {}", build.version)));
    version.set_xalign(0.0);
    details.append(&version);
    let application_id = gtk::Label::new(Some(&build.application_id));
    application_id.set_xalign(0.0);
    application_id.set_selectable(true);
    application_id.add_css_class("dim-label");
    details.append(&application_id);
    identity_row.append(&details);
    identity.append(&identity_row);
    contents.append(&identity);

    let product_section = section(
        "System",
        Some(
            "This unprivileged application controls only the persistent guest. It cannot access the host clipboard, host input outside the machine window, host desktop services, or host devices.",
        ),
    );
    product_section.append(&setting_row(
        "Settings service",
        "Single-instance session-bus identity.",
        &value_label(crate::APPLICATION_ID),
    ));
    product_section.append(&setting_row(
        "Persistent settings",
        "Stored in the guest XDG configuration directory.",
        &value_label("~/.config/wildbuzzard/settings.json"),
    ));
    product_section.append(&setting_row(
        "License",
        "Wild Buzzard source license.",
        &value_label("AGPL-3.0-or-later"),
    ));
    let source = gtk::LinkButton::with_label(
        "https://github.com/openresearchtools/BuzzardOS",
        "Source code",
    );
    accessible(
        &source,
        "Wild Buzzard source code",
        "Open the project source repository in a guest application.",
    );
    product_section.append(&source);
    contents.append(&product_section);

    let capability = section(
        "Capability Status",
        Some(
            "Appearance, display scale, private guest sound, AppImage integration, and Debian updates use their typed guest backends. A control is disabled only when its required runtime service is unavailable, and this application never fakes a successful operation.",
        ),
    );
    capability.append(&wrapped_label(
        "The Buzzard icon is resolved by the stable wildbuzzard-settings icon name, so final audited artwork can replace the temporary asset without changing Settings code.",
    ));
    if let Some(diagnostic) = theme_compatibility_diagnostic() {
        let warning = wrapped_label(&diagnostic);
        warning.add_css_class("warning");
        accessible(
            &warning,
            "Theme compatibility warning",
            "Actionable compatibility status for an older persistent machine.",
        );
        capability.append(&warning);
    }
    contents.append(&capability);

    page(
        "About",
        "Product identity, persistent-state boundary, source, licensing, and current capability status.",
        &contents,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn accessibility_contract_names_are_nonempty_and_unique() {
        let names = ACCESSIBLE_CONTROL_NAMES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), ACCESSIBLE_CONTROL_NAMES.len());
        assert!(names.iter().all(|name| !name.trim().is_empty()));
        for required in [
            "Custom desktop colour",
            "Guest UI scale Automatic",
            "Show microphone level",
            "Registered AppImages",
            "Install available updates",
        ] {
            assert!(names.contains(required));
        }
    }

    #[test]
    fn byte_format_is_human_readable_and_deterministic() {
        assert_eq!(format_bytes(999), "999 bytes");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn updater_poll_does_not_stop_on_a_not_yet_started_worker() {
        assert!(!update_poll_is_complete(3, false, false, 7, 7));
        assert!(!update_poll_is_complete(20, true, true, 8, 7));
        assert!(update_poll_is_complete(3, false, true, 9, 7));
        assert!(update_poll_is_complete(3, false, false, 8, 7));
        assert!(update_poll_is_complete(2_400, false, false, 7, 7));
    }

    #[test]
    fn background_rgba_conversion_is_exact_for_byte_channels() {
        for color in [
            SolidColor::new(0, 0, 0),
            SolidColor::new(255, 113, 57),
            SolidColor::new(244, 241, 236),
        ] {
            assert_eq!(rgba_to_solid(solid_to_rgba(color)), color);
        }
    }
}
