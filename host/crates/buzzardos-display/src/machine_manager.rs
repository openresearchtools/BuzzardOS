// SPDX-License-Identifier: AGPL-3.0-or-later

use std::cell::RefCell;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use clap::Parser;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use wb_core::{
    DEFAULT_PODMAN_ARGUMENTS, HostMediaDevice, HostMediaKind, MachineConfig, MachineRegistry,
    MachineState, NetworkMode, Podman, PodmanContainerState, PodmanDefinition, PodmanRuntimePaths,
    PortDirection, PortForward, PortProtocol, ResourceLocator, RuntimeState, SharedPath,
    discover_host_media,
};

const HOST_PROJECT_LICENSE: &str = include_str!("../../../../LICENSE");
const HOST_CARGO_INVENTORY: &str = include_str!("../../../../LICENSES/generated/cargo-host.tsv");
const HOST_RUST_DEPENDENCY_NOTICES: &str =
    include_str!("../../../../LICENSES/generated/RUST_DEPENDENCY_LICENSES.buzzardos.txt");
const HOST_ICON_PNG: &[u8] = include_bytes!("../../../packaging/icons/buzzardos-256.png");
const BUILTIN_CONTAINERFILE_STANDARD: &[u8] =
    include_bytes!("../../../../oci/desktop/Containerfile");
const BUILTIN_CONTAINERFILE_CUDA: &[u8] =
    include_bytes!("../../../../oci/desktop/Containerfile.cuda");
const BUILTIN_PROVISION_IMAGE: &[u8] = include_bytes!("../../../../oci/desktop/provision-image.sh");
const BUILTIN_APT_SNAPSHOT_SOURCES: &[u8] =
    include_bytes!("../../../../oci/desktop/apt/debian-sid-snapshot.sources");
const BUILTIN_APT_LIVE_SOURCES: &[u8] =
    include_bytes!("../../../../oci/desktop/apt/debian-sid-live.sources");
const BUILTIN_APT_SNAPSHOT_CONFIG: &[u8] =
    include_bytes!("../../../../oci/desktop/apt/99buzzardos-snapshot");
const MACHINE_LICENSE_EXCLUSION: &str = "This About view covers only the installed Buzzard OS host package. It does not cover machine images or root filesystems, software installed inside a machine, or the separately packaged Buzzard guest components. Those retain their own license records.";
const EXTERNAL_HOST_DEPENDENCIES: &str = "The following runtime packages are installed separately by APT and are not bundled into the Buzzard OS host package:\n\nPodman, Buildah, their native OCI runtime and networking dependencies, GStreamer and its PipeWire plugins, GTK 4, GLib, Wayland, libxkbcommon, xkb-data, and PipeWire.\n\nTheir package metadata and /usr/share/doc/<package>/copyright files are authoritative for the versions installed on this host.";

#[derive(Debug, Parser)]
#[command(name = "buzzardos-display --machine-manager")]
struct ManagerArgs {
    #[arg(long)]
    launcher: PathBuf,

    /// Open this machine's manager-owned settings page immediately.
    #[arg(long)]
    settings_machine: Option<PathBuf>,
}

pub(crate) fn run_from_args() -> Result<()> {
    let args = std::env::args_os()
        .enumerate()
        .filter_map(|(index, value)| (index != 1).then_some(value));
    let args = ManagerArgs::parse_from(args);
    if !args.launcher.is_file() {
        bail!(
            "machine manager launcher is missing: {}",
            args.launcher.display()
        );
    }
    let application_id = manager_application_id(&args.launcher);
    let invocation = manager_invocation(&args)?;
    let application = gtk::Application::builder()
        .application_id(application_id)
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    let system_theme = Rc::new(RefCell::new(None::<gio::Settings>));
    let system_theme_for_command_line = Rc::clone(&system_theme);
    // GTK owns the application window after it is presented, but the window
    // does not own `ManagerUi`.  Keep the controller alive for the complete
    // application lifetime so its weak button callbacks remain usable.
    let active_manager = Rc::new(RefCell::new(None::<Rc<ManagerUi>>));
    let active_manager_for_command_line = Rc::clone(&active_manager);
    application.connect_command_line(move |application, command_line| {
        let request = match ManagerArgs::try_parse_from(command_line.arguments()) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("Buzzard OS machine manager: {error}");
                return glib::ExitCode::FAILURE;
            }
        };
        if system_theme_for_command_line.borrow().is_none() {
            system_theme_for_command_line.replace(crate::host_theme::follow_system_color_scheme());
        }
        if let Some(manager) = active_manager_for_command_line.borrow().as_ref().cloned() {
            manager.window.present();
            if let Some(machine_dir) = request.settings_machine {
                manager.show_machine_settings(&machine_dir);
            }
            return glib::ExitCode::SUCCESS;
        }
        match ManagerUi::build(application, request.launcher) {
            Ok(manager) => {
                manager.window.present();
                if let Some(machine_dir) = request.settings_machine {
                    manager.show_machine_settings(&machine_dir);
                }
                active_manager_for_command_line.replace(Some(manager));
                glib::ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Buzzard OS machine manager: {error:#}");
                application.quit();
                glib::ExitCode::FAILURE
            }
        }
    });
    let status = application.run_with_args(&invocation);
    drop(active_manager);
    drop(system_theme);
    if status != glib::ExitCode::SUCCESS {
        bail!("machine manager exited with {status:?}");
    }
    Ok(())
}

fn manager_application_id(launcher: &Path) -> String {
    // One primary manager belongs to one portable application folder.  The
    // path-derived suffix prevents an independently copied portable folder
    // from stealing another copy's Settings requests.
    let identity = launcher
        .canonicalize()
        .unwrap_or_else(|_| launcher.to_path_buf());
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in identity.as_os_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("org.openresearchtools.buzzardos.manager.x{hash:016x}")
}

fn manager_invocation(args: &ManagerArgs) -> Result<Vec<String>> {
    let launcher = args
        .launcher
        .to_str()
        .context("machine manager launcher path is not valid UTF-8")?;
    let mut invocation = vec![
        "BuzzardOS".to_owned(),
        "--launcher".to_owned(),
        launcher.to_owned(),
    ];
    if let Some(machine_dir) = &args.settings_machine {
        invocation.push("--settings-machine".to_owned());
        invocation.push(
            machine_dir
                .to_str()
                .context("machine Settings path is not valid UTF-8")?
                .to_owned(),
        );
    }
    Ok(invocation)
}

struct ManagerUi {
    window: gtk::ApplicationWindow,
    launcher: PathBuf,
    list: gtk::ListBox,
    content: gtk::Stack,
    settings_page: RefCell<Option<gtk::Box>>,
    header_title: gtk::Label,
    back: gtk::Button,
    create: gtk::Button,
    refresh_button: gtk::Button,
    about_button: gtk::Button,
    command_result: Arc<Mutex<Option<String>>>,
    machine_list_snapshot: RefCell<Vec<MachineListSignature>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MachineListSignature {
    directory: PathBuf,
    name: String,
    width: u32,
    height: u32,
    network: String,
    state: MachineState,
}

struct MachineListItem {
    directory: PathBuf,
    config: MachineConfig,
    state: MachineState,
}

#[derive(Clone, Copy)]
enum BuiltInMachine {
    Cuda,
    Standard,
}

enum CommandProgressEvent {
    Output(String),
    Finished { success: bool, cancelled: bool },
}

struct CommandPresentation {
    title: String,
    running: &'static str,
    success: &'static str,
    failure: &'static str,
}

#[derive(Clone)]
struct ManagerPortRow {
    id: uuid::Uuid,
    row: gtk::ListBoxRow,
    enabled: gtk::Switch,
    direction: gtk::DropDown,
    protocol: gtk::DropDown,
    host_address: gtk::Entry,
    host_port: gtk::SpinButton,
    guest_address: gtk::Entry,
    guest_port: gtk::SpinButton,
}

#[derive(Clone)]
struct ManagerShareRow {
    id: uuid::Uuid,
    row: gtk::ListBoxRow,
    host_path: PathBuf,
    guest_name: gtk::Entry,
    read_only: gtk::Switch,
}

impl ManagerUi {
    fn build(application: &gtk::Application, launcher: PathBuf) -> Result<Rc<Self>> {
        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .title("Buzzard OS Machines")
            .icon_name("buzzardos")
            .default_width(820)
            .default_height(560)
            .build();
        let header = gtk::HeaderBar::builder().show_title_buttons(true).build();
        let header_title = gtk::Label::new(Some("Buzzard OS Machines"));
        header_title.add_css_class("heading");
        header.set_title_widget(Some(&header_title));
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .label("Machines")
            .tooltip_text("Return to the machine list")
            .visible(false)
            .build();
        let create = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .label("Add Machine")
            .tooltip_text("Create, pull, or import a machine")
            .build();
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.set_tooltip_text(Some("Refresh machines"));
        let about = gtk::Button::from_icon_name("help-about-symbolic");
        about.set_tooltip_text(Some("About Buzzard OS and host-package licenses"));
        header.pack_start(&back);
        header.pack_start(&create);
        header.pack_end(&refresh);
        header.pack_end(&about);
        window.set_titlebar(Some(&header));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let scroller = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        list.set_margin_start(12);
        list.set_margin_end(12);
        list.set_margin_top(12);
        list.set_margin_bottom(12);
        scroller.set_child(Some(&list));
        root.append(&scroller);
        let content = gtk::Stack::builder()
            .hexpand(true)
            .vexpand(true)
            .transition_type(gtk::StackTransitionType::SlideLeftRight)
            .build();
        content.add_named(&root, Some("machines"));
        content.set_visible_child_name("machines");
        window.set_child(Some(&content));

        let manager = Rc::new(Self {
            window,
            launcher,
            list,
            content,
            settings_page: RefCell::new(None),
            header_title,
            back: back.clone(),
            create: create.clone(),
            refresh_button: refresh.clone(),
            about_button: about.clone(),
            command_result: Arc::new(Mutex::new(None)),
            machine_list_snapshot: RefCell::new(Vec::new()),
        });
        manager.refresh()?;

        let weak = Rc::downgrade(&manager);
        back.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                manager.show_machine_list();
            }
        });
        let weak = Rc::downgrade(&manager);
        refresh.connect_clicked(move |_| refresh_weak(&weak));
        let weak = Rc::downgrade(&manager);
        create.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                manager.show_create_dialog();
            }
        });
        let weak = Rc::downgrade(&manager);
        about.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                manager.show_about_dialog();
            }
        });
        let weak = Rc::downgrade(&manager);
        glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
            let Some(manager) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let result = manager
                .command_result
                .lock()
                .ok()
                .and_then(|mut result| result.take());
            if let Some(result) = result {
                if !result.is_empty() {
                    show_manager_error(
                        &manager.window,
                        "Machine action failed",
                        &anyhow::anyhow!(result),
                    );
                }
                if let Err(error) = manager.refresh() {
                    show_manager_error(&manager.window, "Could not refresh machines", &error);
                }
            }
            glib::ControlFlow::Continue
        });
        let weak = Rc::downgrade(&manager);
        glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
            let Some(manager) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if let Err(error) = manager.refresh_if_changed() {
                eprintln!("Buzzard OS machine manager: refreshing lifecycle state: {error:#}");
            }
            glib::ControlFlow::Continue
        });
        Ok(manager)
    }

    fn show_about_dialog(&self) {
        let dialog = independent_manager_window(&self.window, "About Buzzard OS", 780, 660, true);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.set_margin_start(16);
        root.set_margin_end(16);
        root.set_margin_top(16);
        root.set_margin_bottom(16);

        let stack = gtk::Stack::builder()
            .hexpand(true)
            .vexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        let switcher = gtk::StackSwitcher::new();
        switcher.set_halign(gtk::Align::Center);
        switcher.set_stack(Some(&stack));
        root.append(&switcher);

        let about_page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        about_page.set_margin_start(18);
        about_page.set_margin_end(18);
        about_page.set_margin_top(24);
        about_page.set_margin_bottom(18);
        let icon = embedded_buzzard_icon(96);
        let title = gtk::Label::new(Some("Buzzard OS"));
        title.add_css_class("title-1");
        let version = gtk::Label::new(Some(&format!(
            "Host package version {}",
            env!("CARGO_PKG_VERSION")
        )));
        version.add_css_class("dim-label");
        let description =
            gtk::Label::new(Some("Rootless, persistent Linux desktop-machine manager"));
        description.set_wrap(true);
        description.set_justify(gtk::Justification::Center);
        let scope = gtk::Label::new(Some(MACHINE_LICENSE_EXCLUSION));
        scope.set_wrap(true);
        scope.set_xalign(0.0);
        scope.set_selectable(true);
        scope.set_margin_top(12);
        scope.set_margin_start(12);
        scope.set_margin_end(12);
        scope.set_margin_bottom(12);
        scope.add_css_class("card");
        let source = gtk::LinkButton::with_label(
            "https://github.com/openresearchtools/BuzzardOS",
            "Source code",
        );
        about_page.append(&icon);
        about_page.append(&title);
        about_page.append(&version);
        about_page.append(&description);
        about_page.append(&scope);
        about_page.append(&source);
        stack.add_titled(&about_page, Some("about"), "About");

        let license_document = host_license_document();
        stack.add_titled(
            &document_page(&license_document, false),
            Some("licenses"),
            "Bundled licenses",
        );
        let dependency_document = format!(
            "HOST RUST DEPENDENCIES EMBEDDED IN THE EXECUTABLES\n\n{}\n\nSYSTEM PACKAGES — NOT BUNDLED\n\n{}",
            cargo_dependency_summary(HOST_CARGO_INVENTORY),
            EXTERNAL_HOST_DEPENDENCIES
        );
        stack.add_titled(
            &document_page(&dependency_document, true),
            Some("dependencies"),
            "Dependencies",
        );
        root.append(&stack);

        dialog.set_child(Some(&root));
        dialog.present();
    }

    fn refresh(self: &Rc<Self>) -> Result<()> {
        self.render_machines(discover_machine_list()?)
    }

    fn refresh_if_changed(self: &Rc<Self>) -> Result<()> {
        let machines = discover_machine_list()?;
        let snapshot = machine_list_signature(&machines);
        if *self.machine_list_snapshot.borrow() != snapshot {
            self.render_machines(machines)?;
        }
        Ok(())
    }

    fn render_machines(self: &Rc<Self>, machines: Vec<MachineListItem>) -> Result<()> {
        self.machine_list_snapshot
            .replace(machine_list_signature(&machines));
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        if machines.is_empty() {
            let empty = gtk::Box::new(gtk::Orientation::Vertical, 10);
            empty.set_margin_top(56);
            empty.set_margin_bottom(56);
            let icon = gtk::Image::from_icon_name("computer-symbolic");
            icon.set_pixel_size(48);
            icon.add_css_class("dim-label");
            let title = gtk::Label::new(Some("No machines installed"));
            title.add_css_class("heading");
            let detail = gtk::Label::new(Some(
                "Use Add Machine to build a Buzzard desktop, pull a container image, or import a machine.",
            ));
            detail.set_wrap(true);
            detail.set_justify(gtk::Justification::Center);
            detail.add_css_class("dim-label");
            empty.append(&icon);
            empty.append(&title);
            empty.append(&detail);
            self.list.append(&empty);
            return Ok(());
        }
        for machine in machines {
            self.list
                .append(&self.machine_row(&machine.directory, &machine.config, machine.state));
        }
        Ok(())
    }

    fn machine_row(
        self: &Rc<Self>,
        directory: &Path,
        config: &MachineConfig,
        runtime_state: MachineState,
    ) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(true);
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        content.set_margin_start(12);
        content.set_margin_end(8);
        content.set_margin_top(8);
        content.set_margin_bottom(8);
        let icon = gtk::Image::from_icon_name("computer-symbolic");
        icon.set_pixel_size(32);
        icon.set_valign(gtk::Align::Center);
        content.append(&icon);
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 3);
        let name = gtk::Label::new(Some(&config.name));
        name.set_xalign(0.0);
        name.add_css_class("heading");
        let state_text = match runtime_state {
            MachineState::Starting => "Starting",
            MachineState::Running => "Running",
            MachineState::Stopping => "Stopping",
            MachineState::Stopped => "Stopped",
            MachineState::Failed => "Failed",
        };
        let state = gtk::Label::new(Some(&format!(
            "{state_text}  ·  {} × {}  ·  {}",
            config.width,
            config.height,
            network_label(config.network)
        )));
        state.set_xalign(0.0);
        state.add_css_class("dim-label");
        state.set_ellipsize(gtk::pango::EllipsizeMode::End);
        labels.append(&name);
        labels.append(&state);
        labels.set_hexpand(true);
        content.append(&labels);

        let running = matches!(
            runtime_state,
            MachineState::Starting | MachineState::Running | MachineState::Stopping
        );
        let lifecycle = gtk::Button::with_label(if running { "Stop" } else { "Start" });
        lifecycle.set_tooltip_text(Some(if running {
            "Shut down this machine"
        } else {
            "Start this machine and open its window"
        }));
        let weak = Rc::downgrade(self);
        let machine = config.name.clone();
        let machine_dir = directory.to_path_buf();
        lifecycle.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                if running {
                    manager.run_command(
                        Some(machine_dir.clone()),
                        vec!["stop".into(), machine.clone()],
                    );
                } else {
                    manager.open_machine(&machine, &machine_dir);
                }
            }
        });
        content.append(&lifecycle);

        let settings = gtk::Button::builder()
            .icon_name("emblem-system-symbolic")
            .label("Settings")
            .build();
        settings.set_tooltip_text(Some("Machine settings"));
        let weak = Rc::downgrade(self);
        let machine_dir = directory.to_path_buf();
        settings.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                manager.show_machine_settings(&machine_dir);
            }
        });
        content.append(&settings);

        let menu = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("More machine actions")
            .build();
        let popover = gtk::Popover::new();
        let actions = gtk::Box::new(gtk::Orientation::Vertical, 2);
        actions.set_margin_start(6);
        actions.set_margin_end(6);
        actions.set_margin_top(6);
        actions.set_margin_bottom(6);
        for (label, action) in MACHINE_OVERFLOW_ACTIONS {
            let button = gtk::Button::with_label(label);
            button.set_halign(gtk::Align::Fill);
            if action == "delete" {
                button.add_css_class("destructive-action");
            } else {
                button.add_css_class("flat");
            }
            let weak = Rc::downgrade(self);
            let machine = config.name.clone();
            let machine_dir = directory.to_path_buf();
            let popover_for_action = popover.clone();
            button.connect_clicked(move |_| {
                popover_for_action.popdown();
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                match action {
                    "export" => manager.show_export_dialog(&machine, &machine_dir),
                    "clone" => manager.show_clone_dialog(&machine),
                    "delete" => manager.show_delete_dialog(&machine, &machine_dir),
                    _ => unreachable!(),
                }
            });
            actions.append(&button);
        }
        popover.set_child(Some(&actions));
        menu.set_popover(Some(&popover));
        content.append(&menu);

        let weak = Rc::downgrade(self);
        let machine = config.name.clone();
        let machine_dir = directory.to_path_buf();
        row.connect_activate(move |_| {
            if let Some(manager) = weak.upgrade() {
                manager.open_machine(&machine, &machine_dir);
            }
        });
        let secondary_click = gtk::GestureClick::new();
        secondary_click.set_button(3);
        let weak = Rc::downgrade(self);
        let machine_dir = directory.to_path_buf();
        secondary_click.connect_pressed(move |_, _, _, _| {
            if let Some(manager) = weak.upgrade() {
                manager.show_machine_settings(&machine_dir);
            }
        });
        row.add_controller(secondary_click);
        row.set_child(Some(&content));
        row
    }

    fn open_machine(&self, machine: &str, machine_dir: &Path) {
        self.run_command(
            Some(machine_dir.to_path_buf()),
            vec!["start".into(), machine.to_owned(), "--detach".into()],
        );
    }

    fn show_machine_list(&self) {
        self.content.set_visible_child_name("machines");
        if let Some(settings_page) = self.settings_page.borrow_mut().take() {
            self.content.remove(&settings_page);
        }
        self.window.set_title(Some("Buzzard OS Machines"));
        self.header_title.set_text("Buzzard OS Machines");
        self.back.set_visible(false);
        self.create.set_visible(true);
        self.refresh_button.set_visible(true);
        self.about_button.set_visible(true);
    }

    fn show_machine_settings(self: &Rc<Self>, machine_dir: &Path) {
        let config = match MachineConfig::load(machine_dir) {
            Ok(config) => config,
            Err(error) => {
                show_manager_error(&self.window, "Could not load machine settings", &error);
                return;
            }
        };
        self.show_machine_list();
        let parent: gtk::Window = self.window.clone().upcast();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let editor = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        editor.set_vexpand(true);
        let stack = gtk::Stack::builder()
            .hexpand(true)
            .vexpand(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();
        let sidebar = gtk::StackSidebar::new();
        sidebar.set_stack(&stack);
        sidebar.set_size_request(180, -1);
        sidebar.add_css_class("navigation-sidebar");
        editor.append(&sidebar);
        let separator = gtk::Separator::new(gtk::Orientation::Vertical);
        editor.append(&separator);
        editor.append(&stack);
        root.append(&editor);

        let general = settings_page("General", "Display, network, and GPU settings");
        let general_content = general.1;
        let machine_location = machine_location_control(&parent, machine_dir);
        let title = gtk::Entry::builder()
            .text(&config.title)
            .hexpand(true)
            .build();
        let width = gtk::SpinButton::with_range(320.0, 16_384.0, 1.0);
        width.set_value(config.width as f64);
        let height = gtk::SpinButton::with_range(240.0, 16_384.0, 1.0);
        height.set_value(config.height as f64);
        let guest_scale = gtk::DropDown::from_strings(&[
            "Follow host (recommended)",
            "100%",
            "125%",
            "150%",
            "175%",
            "200%",
        ]);
        guest_scale.set_selected(match config.guest_scale_120 {
            None => 0,
            Some(120) => 1,
            Some(150) => 2,
            Some(180) => 3,
            Some(210) => 4,
            Some(240) => 5,
            Some(_) => 0,
        });
        let network = gtk::DropDown::from_strings(&[
            "Private user-mode network",
            "Host network (reduced isolation)",
            "No network",
        ]);
        network.set_selected(match config.network {
            NetworkMode::User => 0,
            NetworkMode::Host => 1,
            NetworkMode::None => 2,
        });
        let gpus = gtk::Entry::builder()
            .text(config.gpus.join(","))
            .placeholder_text("all, index, or GPU UUIDs")
            .hexpand(true)
            .build();
        let podman_arguments = gtk::Entry::builder()
            .text(&config.custom_podman_arguments)
            .placeholder_text(
                "Native modes: --userns=host, keep-id, auto, nomap, or explicit UID/GID maps",
            )
            .tooltip_text("Any native podman create arguments, including host, keep-id, auto, nomap, explicit UID/GID maps, devices, CDI, and other Podman-supported flags")
            .hexpand(true)
            .build();
        let general_grid = gtk::Grid::builder()
            .row_spacing(12)
            .column_spacing(18)
            .hexpand(true)
            .build();
        attach_manager_setting(&general_grid, 0, "Machine location", &machine_location);
        attach_manager_setting(&general_grid, 1, "Window title", &title);
        attach_manager_setting(&general_grid, 2, "Initial monitor width", &width);
        attach_manager_setting(&general_grid, 3, "Initial monitor height", &height);
        attach_manager_setting(&general_grid, 4, "Desktop scale", &guest_scale);
        attach_manager_setting(&general_grid, 5, "Network mode", &network);
        attach_manager_setting(&general_grid, 6, "GPU passthrough", &gpus);
        attach_manager_setting(
            &general_grid,
            7,
            "Native Podman create arguments",
            &podman_arguments,
        );
        general_content.append(&general_grid);
        let restart_note = gtk::Label::new(Some(
            "Podman definition changes take effect at the next start or restart. Arguments are passed directly to stock Podman without filtering or rewriting.",
        ));
        restart_note.set_xalign(0.0);
        restart_note.set_wrap(true);
        restart_note.add_css_class("dim-label");
        general_content.append(&restart_note);
        stack.add_titled(&general.0, Some("general"), "General");

        let ports = settings_page("Port mappings", "Host ↔ guest network forwarding");
        let port_rows: Rc<RefCell<Vec<ManagerPortRow>>> = Rc::new(RefCell::new(Vec::new()));
        let port_list = gtk::ListBox::new();
        port_list.add_css_class("boxed-list");
        port_list.set_selection_mode(gtk::SelectionMode::None);
        for mapping in &config.integrations.ports {
            append_manager_port_row(&port_list, &port_rows, mapping.clone());
        }
        let port_scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&port_list)
            .build();
        ports.1.append(&port_scroll);
        let port_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let add_inbound = gtk::Button::with_label("Add Host → Guest");
        let add_reverse = gtk::Button::with_label("Add Guest → Host");
        port_actions.append(&add_inbound);
        port_actions.append(&add_reverse);
        ports.1.append(&port_actions);
        let list_for_inbound = port_list.clone();
        let rows_for_inbound = Rc::clone(&port_rows);
        add_inbound.connect_clicked(move |_| {
            append_manager_port_row(
                &list_for_inbound,
                &rows_for_inbound,
                PortForward::new(PortDirection::HostToGuest),
            );
        });
        let list_for_reverse = port_list.clone();
        let rows_for_reverse = Rc::clone(&port_rows);
        add_reverse.connect_clicked(move |_| {
            append_manager_port_row(
                &list_for_reverse,
                &rows_for_reverse,
                PortForward::new(PortDirection::GuestToHost),
            );
        });
        stack.add_titled(&ports.0, Some("ports"), "Ports");

        let devices = settings_page(
            "Devices",
            "Audio output, microphone, and camera authorization",
        );
        let device_grid = gtk::Grid::builder()
            .row_spacing(14)
            .column_spacing(18)
            .hexpand(true)
            .build();
        let audio = gtk::Switch::builder()
            .active(config.integrations.media.guest_audio_output)
            .halign(gtk::Align::End)
            .build();
        let microphone = gtk::Switch::builder()
            .active(config.integrations.media.host_microphone)
            .halign(gtk::Align::End)
            .build();
        let camera = gtk::Switch::builder()
            .active(config.integrations.media.host_camera)
            .halign(gtk::Align::End)
            .build();
        let media_devices = ResourceLocator::discover()
            .and_then(|resources| discover_host_media(&resources))
            .unwrap_or_default();
        let (audio_target, audio_targets) = manager_media_device_dropdown(
            &media_devices,
            HostMediaKind::AudioSink,
            config.integrations.media.audio_target.as_deref(),
        );
        let (microphone_target, microphone_targets) = manager_media_device_dropdown(
            &media_devices,
            HostMediaKind::Microphone,
            config.integrations.media.microphone_target.as_deref(),
        );
        let (camera_target, camera_targets) = manager_media_device_dropdown(
            &media_devices,
            HostMediaKind::Camera,
            config.integrations.media.camera_target.as_deref(),
        );
        attach_manager_setting(&device_grid, 0, "Guest audio → host speakers", &audio);
        attach_manager_setting(&device_grid, 1, "Audio output", &audio_target);
        attach_manager_setting(&device_grid, 2, "Host microphone → guest", &microphone);
        attach_manager_setting(&device_grid, 3, "Microphone", &microphone_target);
        attach_manager_setting(&device_grid, 4, "Host camera → guest", &camera);
        attach_manager_setting(&device_grid, 5, "Camera", &camera_target);
        devices.1.append(&device_grid);
        let devices_note = gtk::Label::new(Some(
            "Automatic follows the host default. Media changes apply when the machine next starts or restarts.",
        ));
        devices_note.set_xalign(0.0);
        devices_note.set_wrap(true);
        devices_note.add_css_class("dim-label");
        devices.1.append(&devices_note);
        stack.add_titled(&devices.0, Some("devices"), "Devices");

        let sharing = settings_page("Shared paths", "Files and folders exposed below /shared");
        let share_rows: Rc<RefCell<Vec<ManagerShareRow>>> = Rc::new(RefCell::new(Vec::new()));
        let share_list = gtk::ListBox::new();
        share_list.add_css_class("boxed-list");
        share_list.set_selection_mode(gtk::SelectionMode::None);
        for share in &config.shares {
            append_manager_share_row(&share_list, &share_rows, share.clone());
        }
        let share_scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&share_list)
            .build();
        sharing.1.append(&share_scroll);
        let share_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let add_file = gtk::Button::with_label("Add File…");
        let add_folder = gtk::Button::with_label("Add Folder…");
        share_actions.append(&add_file);
        share_actions.append(&add_folder);
        sharing.1.append(&share_actions);
        connect_manager_share_picker(&add_file, &parent, &share_list, &share_rows, false);
        connect_manager_share_picker(&add_folder, &parent, &share_list, &share_rows, true);
        stack.add_titled(&sharing.0, Some("sharing"), "Sharing");

        let actions = gtk::ActionBar::new();
        let cancel = gtk::Button::with_label("Cancel");
        let save = gtk::Button::with_label("Save");
        actions.pack_end(&save);
        actions.pack_end(&cancel);
        root.append(&actions);
        self.content.add_named(&root, Some("settings"));
        self.settings_page.replace(Some(root));
        self.content.set_visible_child_name("settings");
        self.window
            .set_title(Some(&format!("{} Settings — Buzzard OS", config.name)));
        self.header_title
            .set_text(&format!("{} Settings", config.name));
        self.back.set_visible(true);
        self.create.set_visible(false);
        self.refresh_button.set_visible(false);
        self.about_button.set_visible(false);

        let weak = Rc::downgrade(self);
        cancel.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                manager.show_machine_list();
            }
        });
        let machine_dir = machine_dir.to_path_buf();
        let weak = Rc::downgrade(self);
        save.connect_clicked(move |_| {
            let mut updated = config.clone();
            updated.title = title.text().trim().to_owned();
            updated.width = width.value_as_int() as u32;
            updated.height = height.value_as_int() as u32;
            updated.guest_scale_120 = match guest_scale.selected() {
                1 => Some(120),
                2 => Some(150),
                3 => Some(180),
                4 => Some(210),
                5 => Some(240),
                _ => None,
            };
            updated.network = match network.selected() {
                1 => NetworkMode::Host,
                2 => NetworkMode::None,
                _ => NetworkMode::User,
            };
            updated.gpus = comma_separated(&gpus.text());
            updated.custom_podman_arguments = podman_arguments.text().trim().to_owned();
            updated.integrations.ports =
                port_rows.borrow().iter().map(manager_port_value).collect();
            updated.integrations.media.guest_audio_output = audio.is_active();
            updated.integrations.media.host_microphone = microphone.is_active();
            updated.integrations.media.host_camera = camera.is_active();
            updated.integrations.media.audio_target =
                selected_manager_media_target(&audio_target, &audio_targets);
            updated.integrations.media.microphone_target =
                selected_manager_media_target(&microphone_target, &microphone_targets);
            updated.integrations.media.camera_target =
                selected_manager_media_target(&camera_target, &camera_targets);
            updated.shares = share_rows
                .borrow()
                .iter()
                .map(manager_share_value)
                .collect();
            if let Err(error) = updated.save(&machine_dir) {
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                show_manager_error(&manager.window, "Could not save machine settings", &error);
                return;
            }
            if let Some(manager) = weak.upgrade() {
                let _ = manager.refresh();
                manager.show_machine_list();
            }
        });
    }

    fn run_command(&self, machine_dir: Option<PathBuf>, arguments: Vec<String>) {
        let presentation = command_presentation(&arguments);
        let launcher = self.launcher.clone();
        let result = self.command_result.clone();
        std::thread::spawn(move || {
            let mut command = Command::new(&launcher);
            if let Some(machine_dir) = machine_dir {
                command.arg("--machine-dir").arg(machine_dir);
            }
            let output = command.args(&arguments).stdin(Stdio::null()).output();
            let message = match output {
                Ok(output) if output.status.success() => String::new(),
                Ok(output) => concise_failure(
                    presentation.failure,
                    &String::from_utf8_lossy(&output.stderr),
                ),
                Err(error) => format!("{}: {error}", presentation.failure),
            };
            if let Ok(mut slot) = result.lock() {
                *slot = Some(message);
            }
        });
    }

    fn run_command_with_cleanup(
        &self,
        machine_dir: Option<PathBuf>,
        arguments: Vec<String>,
        cleanup: Option<PathBuf>,
        completion_notice: Option<&'static str>,
    ) {
        let presentation = command_presentation(&arguments);
        let dialog = independent_manager_window(&self.window, &presentation.title, 640, 260, true);
        dialog.set_titlebar(Some(&gtk::HeaderBar::new()));
        let root = gtk::Box::new(gtk::Orientation::Vertical, 14);
        root.set_margin_start(20);
        root.set_margin_end(20);
        root.set_margin_top(20);
        root.set_margin_bottom(16);

        let heading_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let spinner = gtk::Spinner::new();
        spinner.set_spinning(true);
        spinner.set_valign(gtk::Align::Center);
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 3);
        labels.set_hexpand(true);
        let heading = gtk::Label::new(Some(&presentation.title));
        heading.set_xalign(0.0);
        heading.add_css_class("heading");
        let stage = gtk::Label::new(Some(presentation.running));
        stage.set_xalign(0.0);
        stage.set_ellipsize(gtk::pango::EllipsizeMode::End);
        stage.set_single_line_mode(true);
        stage.add_css_class("dim-label");
        labels.append(&heading);
        labels.append(&stage);
        heading_row.append(&spinner);
        heading_row.append(&labels);
        root.append(&heading_row);

        let progress = gtk::ProgressBar::new();
        progress.set_hexpand(true);
        root.append(&progress);

        let completion = gtk::Label::new(None);
        completion.set_xalign(0.0);
        completion.set_wrap(true);
        completion.set_selectable(true);
        completion.add_css_class("dim-label");
        completion.set_visible(false);
        root.append(&completion);

        let log = gtk::TextView::new();
        log.set_editable(false);
        log.set_cursor_visible(false);
        log.set_monospace(true);
        log.set_wrap_mode(gtk::WrapMode::WordChar);
        log.set_left_margin(8);
        log.set_right_margin(8);
        log.set_top_margin(8);
        log.set_bottom_margin(8);
        let log_scroller = gtk::ScrolledWindow::builder()
            .min_content_height(240)
            .hexpand(true)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&log)
            .build();
        let details = gtk::Expander::builder()
            .label("Show details")
            .expanded(false)
            .child(&log_scroller)
            .build();
        root.append(&details);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::End);
        let cancel_button = gtk::Button::with_label("Cancel");
        actions.append(&cancel_button);
        root.append(&actions);
        dialog.set_child(Some(&root));

        let cancelled = Arc::new(AtomicBool::new(false));
        let finished = Rc::new(std::cell::Cell::new(false));
        let cancelled_for_button = Arc::clone(&cancelled);
        let finished_for_button = Rc::clone(&finished);
        let dialog_for_button = dialog.clone();
        let stage_for_button = stage.clone();
        cancel_button.connect_clicked(move |button| {
            if finished_for_button.get() {
                dialog_for_button.close();
            } else {
                cancelled_for_button.store(true, Ordering::Release);
                stage_for_button.set_text("Cancelling…");
                button.set_label("Cancelling…");
                button.set_sensitive(false);
            }
        });
        let cancelled_for_close = Arc::clone(&cancelled);
        let finished_for_close = Rc::clone(&finished);
        let stage_for_close = stage.clone();
        dialog.connect_close_request(move |_| {
            if finished_for_close.get() {
                glib::Propagation::Proceed
            } else {
                cancelled_for_close.store(true, Ordering::Release);
                stage_for_close.set_text("Cancelling…");
                glib::Propagation::Stop
            }
        });

        let launcher = self.launcher.clone();
        let result = self.command_result.clone();
        let (events_tx, events_rx) = mpsc::channel();
        let cancelled_for_worker = Arc::clone(&cancelled);
        std::thread::spawn(move || {
            let mut command = Command::new(&launcher);
            if let Some(machine_dir) = machine_dir {
                command.arg("--machine-dir").arg(machine_dir);
            }
            command
                .args(&arguments)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            command.process_group(0);
            let spawned = command.spawn();
            let (success, was_cancelled) = match spawned {
                Ok(mut child) => {
                    let stdout_thread = child
                        .stdout
                        .take()
                        .map(|stdout| forward_command_output(stdout, events_tx.clone()));
                    let stderr_thread = child
                        .stderr
                        .take()
                        .map(|stderr| forward_command_output(stderr, events_tx.clone()));
                    let mut was_cancelled = false;
                    let mut cancellation_started = None;
                    let success = loop {
                        if cancelled_for_worker.load(Ordering::Acquire) && !was_cancelled {
                            was_cancelled = true;
                            cancellation_started = Some(std::time::Instant::now());
                            signal_command_group(&mut child, libc::SIGTERM);
                        }
                        if cancellation_started
                            .is_some_and(|started| started.elapsed().as_secs() >= 2)
                        {
                            signal_command_group(&mut child, libc::SIGKILL);
                        }
                        match child.try_wait() {
                            Ok(Some(status)) => break status.success() && !was_cancelled,
                            Ok(None) => {
                                std::thread::sleep(std::time::Duration::from_millis(80));
                            }
                            Err(error) => {
                                let _ = events_tx.send(CommandProgressEvent::Output(format!(
                                    "Could not monitor Buzzard OS: {error}"
                                )));
                                signal_command_group(&mut child, libc::SIGKILL);
                                let _ = child.wait();
                                break false;
                            }
                        }
                    };
                    if let Some(thread) = stdout_thread {
                        let _ = thread.join();
                    }
                    if let Some(thread) = stderr_thread {
                        let _ = thread.join();
                    }
                    (success, was_cancelled)
                }
                Err(error) => {
                    let _ = events_tx.send(CommandProgressEvent::Output(format!(
                        "Could not start Buzzard OS: {error}"
                    )));
                    (false, false)
                }
            };
            finish_command_worker(events_tx, result, cleanup, success, was_cancelled);
        });

        let mut last_detail = String::new();
        let mut determinate_progress = false;
        let finished_for_poll = Rc::clone(&finished);
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            while let Ok(event) = events_rx.try_recv() {
                match event {
                    CommandProgressEvent::Output(line) => {
                        if !line.trim().is_empty() {
                            last_detail = line.trim().to_owned();
                            append_progress_log(&log, &line);
                            if let Some(next_stage) = progress_stage(&line) {
                                stage.set_text(next_stage);
                            }
                            if let Some(fraction) = progress_fraction(&line) {
                                determinate_progress = true;
                                progress.set_show_text(true);
                                progress.set_fraction(fraction);
                            }
                        }
                    }
                    CommandProgressEvent::Finished { success, cancelled } => {
                        finished_for_poll.set(true);
                        spinner.set_spinning(false);
                        spinner.set_visible(false);
                        progress.set_show_text(success);
                        progress.set_fraction(if success { 1.0 } else { 0.0 });
                        cancel_button.set_sensitive(true);
                        cancel_button.set_label("Close");
                        if cancelled {
                            heading.set_text("Cancelled");
                            stage.set_text("No machine was created.");
                        } else if success {
                            heading.set_text(presentation.success);
                            stage.set_text("The machine is ready.");
                            if let Some(notice) = completion_notice {
                                completion.set_text(notice);
                                completion.set_visible(true);
                            }
                        } else {
                            heading.set_text(presentation.failure);
                            stage.set_text(if last_detail.is_empty() {
                                "Open details for the error output."
                            } else {
                                &last_detail
                            });
                            details.set_expanded(true);
                        }
                        return glib::ControlFlow::Break;
                    }
                }
            }
            if !determinate_progress {
                progress.pulse();
            }
            glib::ControlFlow::Continue
        });
        dialog.present();
    }

    fn show_create_dialog(self: &Rc<Self>) {
        let dialog = independent_manager_window(&self.window, "Add Machine", 620, 540, false);
        dialog.set_titlebar(Some(&gtk::HeaderBar::new()));
        let root = gtk::Box::new(gtk::Orientation::Vertical, 14);
        root.set_margin_start(18);
        root.set_margin_end(18);
        root.set_margin_top(18);
        root.set_margin_bottom(18);

        let choices = gtk::ListBox::new();
        choices.add_css_class("boxed-list");
        choices.set_selection_mode(gtk::SelectionMode::None);
        let built_in = add_machine_choice(
            &choices,
            "applications-engineering-symbolic",
            "Buzzard Desktop",
            "Create a ready-to-use desktop machine",
            Some("Recommended"),
        );
        let pull = add_machine_choice(
            &choices,
            "folder-download-symbolic",
            "Pull container image",
            "Paste a container image address from a registry",
            None,
        );
        let custom = add_machine_choice(
            &choices,
            "text-x-generic-symbolic",
            "Custom Containerfile",
            "Choose a Containerfile or Dockerfile and its build context",
            None,
        );
        let import = add_machine_choice(
            &choices,
            "document-open-symbolic",
            "Import existing machine",
            "Choose a machine archive or unpacked container-image folder",
            None,
        );
        root.append(&choices);
        dialog.set_child(Some(&root));

        let close = dialog.clone();
        let weak = Rc::downgrade(self);
        built_in.connect_clicked(move |_| {
            close.close();
            if let Some(manager) = weak.upgrade() {
                manager.show_build_dialog_for(Some(BuiltInMachine::Cuda));
            }
        });
        let close = dialog.clone();
        let weak = Rc::downgrade(self);
        pull.connect_clicked(move |_| {
            close.close();
            if let Some(manager) = weak.upgrade() {
                manager.show_oci_machine_dialog(false);
            }
        });
        let close = dialog.clone();
        let weak = Rc::downgrade(self);
        custom.connect_clicked(move |_| {
            close.close();
            if let Some(manager) = weak.upgrade() {
                manager.show_build_dialog();
            }
        });
        let close = dialog.clone();
        let weak = Rc::downgrade(self);
        import.connect_clicked(move |_| {
            close.close();
            if let Some(manager) = weak.upgrade() {
                manager.show_import_dialog();
            }
        });
        dialog.present();
    }

    fn show_import_dialog(self: &Rc<Self>) {
        self.show_oci_machine_dialog(true);
    }

    fn show_oci_machine_dialog(self: &Rc<Self>, importing: bool) {
        let dialog = independent_manager_window(
            &self.window,
            if importing {
                "Import Machine"
            } else {
                "Pull Container Image"
            },
            620,
            -1,
            true,
        );
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.set_margin_start(16);
        root.set_margin_end(16);
        root.set_margin_top(16);
        root.set_margin_bottom(16);
        let grid = gtk::Grid::builder()
            .column_spacing(10)
            .row_spacing(8)
            .build();
        let source_label = gtk::Label::new(Some(if importing {
            "Import from"
        } else {
            "Image address"
        }));
        source_label.set_xalign(0.0);
        let source = gtk::Entry::new();
        source.set_hexpand(true);
        source.set_placeholder_text(Some(if importing {
            "Choose an archive or image-layout folder"
        } else {
            "For example: docker.io/organization/image:tag"
        }));
        source.set_editable(!importing);
        let source_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        source_row.append(&source);
        let choose_archive = gtk::Button::with_label("Choose Archive…");
        let choose_layout = gtk::Button::with_label("Choose Image Folder…");
        if importing {
            source_row.append(&choose_archive);
            source_row.append(&choose_layout);
        }
        grid.attach(&source_label, 0, 0, 1, 1);
        grid.attach(&source_row, 1, 0, 1, 1);
        let name_label = gtk::Label::new(Some("Machine name"));
        name_label.set_xalign(0.0);
        let name = gtk::Entry::new();
        name.set_hexpand(true);
        name.set_placeholder_text(Some("letters, digits, - and _"));
        grid.attach(&name_label, 0, 1, 1, 1);
        grid.attach(&name, 1, 1, 1, 1);
        let destination_label = gtk::Label::new(Some("Machine location"));
        destination_label.set_xalign(0.0);
        let destination_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let destination = gtk::Entry::new();
        destination.set_hexpand(true);
        destination.set_editable(false);
        destination.set_placeholder_text(Some("Choose a folder"));
        let browse_destination = gtk::Button::with_label("Choose…");
        destination_row.append(&destination);
        destination_row.append(&browse_destination);
        grid.attach(&destination_label, 0, 2, 1, 1);
        grid.attach(&destination_row, 1, 2, 1, 1);
        let podman_arguments_label = gtk::Label::new(Some("Native Podman create arguments"));
        podman_arguments_label.set_xalign(0.0);
        let podman_arguments = podman_arguments_entry(None);
        grid.attach(&podman_arguments_label, 0, 3, 1, 1);
        grid.attach(&podman_arguments, 1, 3, 1, 1);
        root.append(&grid);
        let import_as_copy = gtk::CheckButton::with_label("Import as a new copy");
        if importing {
            root.append(&import_as_copy);
        }

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let accept = gtk::Button::with_label(if importing { "Import" } else { "Create" });
        actions.append(&cancel);
        actions.append(&accept);
        root.append(&actions);
        dialog.set_child(Some(&root));

        let chooser_parent = dialog.clone();
        let source_for_archive = source.clone();
        choose_archive.connect_clicked(move |_| {
            let chooser = gtk::FileDialog::builder()
                .title("Choose a machine archive")
                .modal(true)
                .build();
            let parent = chooser_parent.clone();
            let source = source_for_archive.clone();
            glib::spawn_future_local(async move {
                if let Ok(file) = chooser.open_future(Some(&parent)).await
                    && let Some(path) = file.path()
                {
                    source.set_text(&path.to_string_lossy());
                }
            });
        });
        let chooser_parent = dialog.clone();
        let source_for_layout = source.clone();
        choose_layout.connect_clicked(move |_| {
            let source = source_for_layout.clone();
            choose_folder_with_creation(
                &chooser_parent,
                "Choose an unpacked container-image folder",
                None,
                move |path| {
                    source.set_text(&path.to_string_lossy());
                },
            );
        });

        connect_machine_location_picker(&browse_destination, &dialog, &name, &destination);

        let close = dialog.clone();
        cancel.connect_clicked(move |_| close.close());
        let close = dialog.clone();
        let weak = Rc::downgrade(self);
        accept.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                let machine_name = name.text().trim().to_owned();
                let machine_dir = PathBuf::from(destination.text().trim());
                if let Err(error) = validate_machine_destination(&machine_name, &machine_dir) {
                    show_manager_error(&close, "Check the machine details", &error);
                    return;
                }
                let source_value = source.text().trim().to_owned();
                if source_value.is_empty() {
                    show_manager_error(
                        &close,
                        "Check the image source",
                        &anyhow::anyhow!(if importing {
                            "choose a machine archive or unpacked container-image folder"
                        } else {
                            "enter a container image address"
                        }),
                    );
                    return;
                }
                let mut arguments = if importing {
                    vec![
                        "import".into(),
                        source_value,
                        "--name".into(),
                        machine_name,
                        "--mode".into(),
                        if import_as_copy.is_active() {
                            "clone".into()
                        } else {
                            "restore".into()
                        },
                    ]
                } else {
                    vec!["pull".into(), machine_name, source_value]
                };
                append_podman_arguments(&mut arguments, &podman_arguments.text());
                manager.run_command_with_cleanup(Some(machine_dir), arguments, None, None);
                close.close();
            }
        });
        dialog.present();
    }

    fn show_build_dialog(self: &Rc<Self>) {
        self.show_build_dialog_for(None);
    }

    fn show_build_dialog_for(self: &Rc<Self>, built_in: Option<BuiltInMachine>) {
        let dialog = independent_manager_window(
            &self.window,
            if built_in.is_some() {
                "Create Buzzard Desktop"
            } else {
                "Build Custom Containerfile"
            },
            560,
            -1,
            true,
        );
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.set_margin_start(16);
        root.set_margin_end(16);
        root.set_margin_top(16);
        root.set_margin_bottom(16);
        let grid = gtk::Grid::builder()
            .column_spacing(10)
            .row_spacing(8)
            .build();
        let name = gtk::Entry::new();
        let context = gtk::Entry::new();
        let containerfile = gtk::Entry::new();
        let destination = gtk::Entry::new();
        let podman_arguments = podman_arguments_entry(None);
        let cuda_support =
            gtk::CheckButton::with_label("Include NVIDIA CUDA support (recommended)");
        cuda_support.set_active(!matches!(built_in, Some(BuiltInMachine::Standard)));
        cuda_support.set_visible(built_in.is_some());
        for entry in [&name, &context, &containerfile, &destination] {
            entry.set_hexpand(true);
        }
        destination.set_editable(false);
        destination.set_placeholder_text(Some("Choose a folder"));
        if built_in.is_some() {
            context.set_sensitive(false);
            containerfile.set_sensitive(false);
        }
        let context_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let context_browse = gtk::Button::with_label("Browse…");
        context_row.append(&context);
        context_row.append(&context_browse);
        let file_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let file_browse = gtk::Button::with_label("Browse…");
        file_row.append(&containerfile);
        file_row.append(&file_browse);
        let destination_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let destination_browse = gtk::Button::with_label("Choose…");
        destination_row.append(&destination);
        destination_row.append(&destination_browse);
        for (row, (label, widget)) in [
            ("Machine name", name.clone().upcast::<gtk::Widget>()),
            (
                "Machine location",
                destination_row.clone().upcast::<gtk::Widget>(),
            ),
            ("Build context", context_row.clone().upcast::<gtk::Widget>()),
            (
                "Containerfile (optional)",
                file_row.clone().upcast::<gtk::Widget>(),
            ),
            (
                "Native Podman create arguments",
                podman_arguments.clone().upcast::<gtk::Widget>(),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let field_label = gtk::Label::new(Some(label));
            field_label.set_xalign(0.0);
            if built_in.is_some() && matches!(label, "Build context" | "Containerfile (optional)") {
                field_label.set_visible(false);
                widget.set_visible(false);
            }
            grid.attach(&field_label, 0, row as i32, 1, 1);
            grid.attach(&widget, 1, row as i32, 1, 1);
        }
        root.append(&grid);
        root.append(&cuda_support);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let accept = gtk::Button::with_label("Create");
        actions.append(&cancel);
        actions.append(&accept);
        root.append(&actions);
        dialog.set_child(Some(&root));

        if built_in.is_none() {
            connect_folder_entry_picker(
                &context_browse,
                &dialog,
                &context,
                "Choose Containerfile build context",
            );
        } else {
            context_browse.set_visible(false);
            file_browse.set_visible(false);
        }
        let chooser_parent = dialog.clone();
        let containerfile_for_picker = containerfile.clone();
        file_browse.connect_clicked(move |_| {
            let chooser = gtk::FileDialog::builder()
                .title("Choose Containerfile")
                .modal(true)
                .build();
            let parent = chooser_parent.clone();
            let entry = containerfile_for_picker.clone();
            glib::spawn_future_local(async move {
                if let Ok(file) = chooser.open_future(Some(&parent)).await
                    && let Some(path) = file.path()
                {
                    entry.set_text(&path.to_string_lossy());
                }
            });
        });
        connect_machine_location_picker(&destination_browse, &dialog, &name, &destination);

        let close = dialog.clone();
        cancel.connect_clicked(move |_| close.close());
        let close = dialog.clone();
        let weak = Rc::downgrade(self);
        accept.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                let machine_name = name.text().trim().to_owned();
                let machine_dir = PathBuf::from(destination.text().trim());
                if let Err(error) = validate_machine_destination(&machine_name, &machine_dir) {
                    show_manager_error(&close, "Check the machine details", &error);
                    return;
                }
                let (build_context, selected_file, cleanup) = match built_in {
                    Some(_) => {
                        let prepared = prepare_builtin_context(if cuda_support.is_active() {
                            BuiltInMachine::Cuda
                        } else {
                            BuiltInMachine::Standard
                        });
                        match prepared {
                            Ok(context) => {
                                let file = context.join("Containerfile");
                                (context.clone(), file, Some(context))
                            }
                            Err(error) => {
                                show_manager_error(
                                    &close,
                                    "Could not prepare the Buzzard build",
                                    &error,
                                );
                                return;
                            }
                        }
                    }
                    None => {
                        let context = PathBuf::from(context.text().trim());
                        let file = PathBuf::from(containerfile.text().trim());
                        if !context.is_dir() {
                            show_manager_error(
                                &close,
                                "Check the build context",
                                &anyhow::anyhow!(
                                    "build context is not a directory: {}",
                                    context.display()
                                ),
                            );
                            return;
                        }
                        (context, file, None)
                    }
                };
                let mut arguments = vec![
                    "build".into(),
                    machine_name,
                    "--context".into(),
                    build_context.to_string_lossy().into_owned(),
                ];
                if !selected_file.as_os_str().is_empty() {
                    arguments.push("--file".into());
                    arguments.push(selected_file.to_string_lossy().into_owned());
                }
                append_podman_arguments(&mut arguments, &podman_arguments.text());
                manager.run_command_with_cleanup(
                    Some(machine_dir),
                    arguments,
                    cleanup,
                    built_in.is_some().then_some(
                        "Default user: user\nDefault password: buzzard\nChange the password in Settings → Security.",
                    ),
                );
            }
            close.close();
        });
        dialog.present();
    }

    fn show_export_dialog(self: &Rc<Self>, machine: &str, machine_dir: &Path) {
        let dialog = independent_manager_window(&self.window, "Export machine", 620, -1, false);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.set_margin_start(16);
        root.set_margin_end(16);
        root.set_margin_top(16);
        root.set_margin_bottom(16);

        let explanation = gtk::Label::new(Some(
            "Choose the folder that will receive the portable machine archive.",
        ));
        explanation.set_xalign(0.0);
        explanation.set_wrap(true);
        root.append(&explanation);

        let folder_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let folder = gtk::Entry::builder()
            .editable(false)
            .hexpand(true)
            .text(machine_dir.to_string_lossy())
            .build();
        let browse = gtk::Button::with_label("Choose Folder…");
        folder_row.append(&folder);
        folder_row.append(&browse);
        root.append(&folder_row);

        let filename = format!("{machine}.oci.tar");
        let output_note = gtk::Label::new(Some(&format!("Archive name: {filename}")));
        output_note.set_xalign(0.0);
        output_note.add_css_class("dim-label");
        root.append(&output_note);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let accept = gtk::Button::with_label("Export");
        actions.append(&cancel);
        actions.append(&accept);
        root.append(&actions);
        dialog.set_child(Some(&root));

        connect_folder_entry_picker(&browse, &dialog, &folder, "Choose export folder");
        let close = dialog.clone();
        cancel.connect_clicked(move |_| close.close());
        let close = dialog.clone();
        let weak = Rc::downgrade(self);
        let machine = machine.to_owned();
        let machine_dir = machine_dir.to_path_buf();
        accept.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                let selected_folder = PathBuf::from(folder.text().trim());
                match export_destination(&selected_folder, &machine) {
                    Ok(output) => {
                        manager.run_command_with_cleanup(
                            Some(machine_dir.clone()),
                            vec![
                                "export".into(),
                                machine.clone(),
                                "--output".into(),
                                output.to_string_lossy().into_owned(),
                            ],
                            None,
                            None,
                        );
                        close.close();
                    }
                    Err(error) => {
                        show_manager_error(&close, "Choose an export folder", &error);
                    }
                }
            }
        });
        dialog.present();
    }

    fn show_clone_dialog(self: &Rc<Self>, machine: &str) {
        let dialog = independent_manager_window(&self.window, "Clone machine", 620, -1, true);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 10);
        root.set_margin_start(16);
        root.set_margin_end(16);
        root.set_margin_top(16);
        root.set_margin_bottom(16);
        let name = gtk::Entry::builder()
            .placeholder_text("New machine name")
            .build();
        root.append(&name);
        let destination_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let destination = gtk::Entry::builder()
            .placeholder_text("Choose a parent folder; Buzzard creates the named machine inside it")
            .hexpand(true)
            .editable(false)
            .build();
        let browse = gtk::Button::with_label("Choose Location…");
        destination_row.append(&destination);
        destination_row.append(&browse);
        root.append(&destination_row);
        let podman_arguments = podman_arguments_entry(Some(
            "Leave blank to inherit the source machine's native Podman arguments",
        ));
        root.append(&podman_arguments);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let accept = gtk::Button::with_label("Clone");
        actions.append(&cancel);
        actions.append(&accept);
        root.append(&actions);
        dialog.set_child(Some(&root));

        connect_machine_location_picker(&browse, &dialog, &name, &destination);
        let close = dialog.clone();
        cancel.connect_clicked(move |_| close.close());
        let close = dialog.clone();
        let weak = Rc::downgrade(self);
        let source = machine.to_owned();
        accept.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                let new_name = name.text().trim().to_owned();
                let machine_dir = PathBuf::from(destination.text().trim());
                if let Err(error) = validate_machine_destination(&new_name, &machine_dir) {
                    show_manager_error(&close, "Check the machine details", &error);
                    return;
                }
                let mut arguments = vec!["clone".into(), source.clone(), new_name];
                append_optional_podman_arguments(&mut arguments, &podman_arguments.text());
                manager.run_command_with_cleanup(Some(machine_dir), arguments, None, None);
                close.close();
            }
        });
        dialog.present();
    }

    fn show_delete_dialog(self: &Rc<Self>, machine: &str, machine_dir: &Path) {
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(format!("Delete machine “{machine}”?"))
            .detail("This permanently deletes its complete persistent rootfs and cannot be undone.")
            .buttons(["Cancel", "Delete"])
            .cancel_button(0)
            .default_button(0)
            .build();
        let weak = Rc::downgrade(self);
        let machine = machine.to_owned();
        let machine_dir = machine_dir.to_path_buf();
        dialog.choose(
            Some(&self.window),
            None::<&gio::Cancellable>,
            move |choice| {
                if choice == Ok(1)
                    && let Some(manager) = weak.upgrade()
                {
                    manager.run_command(
                        Some(machine_dir.clone()),
                        vec!["delete".into(), machine.clone(), "--yes".into()],
                    );
                }
            },
        );
    }
}

fn command_presentation(arguments: &[String]) -> CommandPresentation {
    match arguments.first().map(String::as_str) {
        Some("build") => CommandPresentation {
            title: "Creating Buzzard Desktop".into(),
            running: "Building container image…",
            success: "Machine created",
            failure: "Machine creation failed",
        },
        Some("pull") => CommandPresentation {
            title: "Pulling Container Image".into(),
            running: "Downloading container image…",
            success: "Machine created",
            failure: "Container download failed",
        },
        Some("import") => CommandPresentation {
            title: "Importing Machine".into(),
            running: "Importing machine…",
            success: "Machine imported",
            failure: "Machine import failed",
        },
        Some("export") => CommandPresentation {
            title: "Exporting Machine".into(),
            running: "Exporting machine…",
            success: "Machine exported",
            failure: "Machine export failed",
        },
        Some("clone") => CommandPresentation {
            title: "Cloning Machine".into(),
            running: "Cloning machine…",
            success: "Machine cloned",
            failure: "Machine clone failed",
        },
        Some("start") => CommandPresentation {
            title: "Starting Machine".into(),
            running: "Starting machine…",
            success: "Machine started",
            failure: "Machine could not start",
        },
        Some("stop") => CommandPresentation {
            title: "Stopping Machine".into(),
            running: "Stopping machine…",
            success: "Machine stopped",
            failure: "Machine could not stop",
        },
        Some("delete") => CommandPresentation {
            title: "Deleting Machine".into(),
            running: "Deleting machine…",
            success: "Machine deleted",
            failure: "Machine could not be deleted",
        },
        _ => CommandPresentation {
            title: "Buzzard OS".into(),
            running: "Working…",
            success: "Done",
            failure: "Operation failed",
        },
    }
}

fn concise_failure(prefix: &str, output: &str) -> String {
    let detail = output
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("no error details were returned");
    let detail = detail.strip_prefix("Buzzard OS: ").unwrap_or(detail);
    let mut short = detail.chars().take(180).collect::<String>();
    if detail.chars().count() > 180 {
        short.push('…');
    }
    format!("{prefix}: {short}")
}

fn forward_command_output(
    reader: impl Read + Send + 'static,
    sender: Sender<CommandProgressEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&bytes).replace('\r', "\n");
                    for line in text.lines().filter(|line| !line.trim().is_empty()) {
                        let line = sanitize_progress_line(line);
                        if line.is_empty() {
                            continue;
                        }
                        if sender.send(CommandProgressEvent::Output(line)).is_err() {
                            return;
                        }
                    }
                }
                Err(error) => {
                    let _ = sender.send(CommandProgressEvent::Output(format!(
                        "Could not read command output: {error}"
                    )));
                    break;
                }
            }
        }
    })
}

fn finish_command_worker(
    events: Sender<CommandProgressEvent>,
    result: Arc<Mutex<Option<String>>>,
    cleanup: Option<PathBuf>,
    success: bool,
    cancelled: bool,
) {
    if let Some(cleanup) = cleanup
        && let Err(error) = std::fs::remove_dir_all(&cleanup)
    {
        eprintln!(
            "Buzzard OS machine manager: removing temporary build context {}: {error}",
            cleanup.display()
        );
    }
    let _ = events.send(CommandProgressEvent::Finished { success, cancelled });
    if let Ok(mut slot) = result.lock() {
        // The progress window owns operation feedback. This empty completion
        // event asks the manager to refresh without leaking command output.
        *slot = Some(String::new());
    }
}

fn signal_command_group(child: &mut Child, signal: i32) {
    let process_group = i32::try_from(child.id()).map_or(0, |pid| -pid);
    if process_group != 0 {
        let _ = unsafe { libc::kill(process_group, signal) };
    }
}

fn sanitize_progress_line(line: &str) -> String {
    let mut clean = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            if characters.next_if_eq(&'[').is_some() {
                for control in characters.by_ref() {
                    if ('@'..='~').contains(&control) {
                        break;
                    }
                }
            }
            continue;
        }
        if !character.is_control() || character == '\t' {
            clean.push(character);
        }
    }
    clean.trim().to_owned()
}

fn append_progress_log(view: &gtk::TextView, line: &str) {
    const MAX_LOG_CHARACTERS: i32 = 200_000;
    let buffer = view.buffer();
    let mut end = buffer.end_iter();
    buffer.insert(&mut end, line);
    buffer.insert(&mut end, "\n");
    let excess = buffer.char_count() - MAX_LOG_CHARACTERS;
    if excess > 0 {
        let mut start = buffer.start_iter();
        let mut trim = buffer.iter_at_offset(excess);
        buffer.delete(&mut start, &mut trim);
    }
    let mut end = buffer.end_iter();
    view.scroll_to_iter(&mut end, 0.0, false, 0.0, 1.0);
}

fn progress_stage(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("getting image source signatures") || lower.contains("copying blob") {
        Some("Downloading base image…")
    } else if lower.contains("apt-get")
        || lower.contains("setting up ")
        || lower.contains("created symlink")
    {
        Some("Installing system packages…")
    } else if lower.contains("writing manifest") || lower.contains("committing") {
        Some("Finalizing container image…")
    } else if lower.contains("applying layer") || lower.contains("extracting") {
        Some("Preparing persistent machine storage…")
    } else if lower.contains("exporting") {
        Some("Exporting container image…")
    } else {
        None
    }
}

fn progress_fraction(line: &str) -> Option<f64> {
    let step = line.strip_prefix("STEP ")?.split_once(':')?.0;
    let (current, total) = step.split_once('/')?;
    let current = current.parse::<u32>().ok()?;
    let total = total.parse::<u32>().ok()?;
    if current == 0 || total == 0 || current > total {
        return None;
    }
    // Reserve the final fifth for OCI export, verification, flattening, and
    // the atomic machine-directory commit after the Containerfile completes.
    Some((f64::from(current) / f64::from(total)) * 0.8)
}

fn network_label(network: NetworkMode) -> &'static str {
    match network {
        NetworkMode::User => "Private network",
        NetworkMode::Host => "Host network",
        NetworkMode::None => "Offline",
    }
}

const MACHINE_OVERFLOW_ACTIONS: [(&str, &str); 3] = [
    ("Export…", "export"),
    ("Clone…", "clone"),
    ("Delete…", "delete"),
];

fn discover_machine_list() -> Result<Vec<MachineListItem>> {
    let registry = MachineRegistry::discover()?;
    let resources = ResourceLocator::discover()?;
    let podman = Podman::discover(&resources)?;
    let mut machines = registry
        .entries()
        .iter()
        .filter_map(|entry| {
            let config = MachineConfig::load(&entry.machine_dir).ok()?;
            let runtime = PodmanRuntimePaths::discover(config.id).ok()?;
            let definition =
                PodmanDefinition::for_machine(&config, &entry.machine_dir, &runtime).ok()?;
            let inspection = podman.inspect(&definition.container_name).ok().flatten();
            let state = inspection
                .as_ref()
                .map(|inspection| match inspection.state {
                    PodmanContainerState::Running | PodmanContainerState::Paused => {
                        MachineState::Running
                    }
                    PodmanContainerState::Stopping => MachineState::Stopping,
                    PodmanContainerState::Unknown => MachineState::Failed,
                    PodmanContainerState::Configured
                    | PodmanContainerState::Created
                    | PodmanContainerState::Stopped
                    | PodmanContainerState::Exited => MachineState::Stopped,
                })
                .unwrap_or(MachineState::Stopped);
            let mut runtime_state = RuntimeState::new(state);
            if let Some(inspection) = inspection {
                runtime_state.container_id = Some(inspection.id);
                runtime_state.definition_digest = inspection.definition_digest;
            }
            let _ = runtime_state.save(&entry.machine_dir);
            Some(MachineListItem {
                directory: entry.machine_dir.clone(),
                config,
                state,
            })
        })
        .collect::<Vec<_>>();
    machines.sort_by(|left, right| left.config.name.cmp(&right.config.name));
    Ok(machines)
}

fn machine_list_signature(machines: &[MachineListItem]) -> Vec<MachineListSignature> {
    machines
        .iter()
        .map(|machine| MachineListSignature {
            directory: machine.directory.clone(),
            name: machine.config.name.clone(),
            width: machine.config.width,
            height: machine.config.height,
            network: network_label(machine.config.network).to_owned(),
            state: machine.state,
        })
        .collect()
}

fn add_machine_choice(
    list: &gtk::ListBox,
    icon_name: &str,
    title: &str,
    detail: &str,
    badge: Option<&str>,
) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    content.set_margin_start(10);
    content.set_margin_end(10);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(32);
    content.append(&icon);
    let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
    labels.set_hexpand(true);
    let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let title = gtk::Label::new(Some(title));
    title.set_xalign(0.0);
    title.add_css_class("heading");
    title_row.append(&title);
    if let Some(badge) = badge {
        let badge = gtk::Label::new(Some(badge));
        badge.add_css_class("accent");
        badge.add_css_class("caption");
        title_row.append(&badge);
    }
    let detail = gtk::Label::new(Some(detail));
    detail.set_xalign(0.0);
    detail.set_wrap(true);
    detail.add_css_class("dim-label");
    labels.append(&title_row);
    labels.append(&detail);
    content.append(&labels);
    let arrow = gtk::Image::from_icon_name("go-next-symbolic");
    content.append(&arrow);
    button.set_child(Some(&content));
    list.append(&button);
    button
}

fn validate_machine_destination(name: &str, destination: &Path) -> Result<()> {
    MachineConfig::validate_name(name)?;
    if destination.as_os_str().is_empty() || !destination.is_absolute() {
        bail!("choose an absolute machine folder with the file picker");
    }
    if destination.exists() {
        bail!(
            "the selected machine folder already exists: {}",
            destination.display()
        );
    }
    Ok(())
}

fn podman_arguments_entry(placeholder: Option<&str>) -> gtk::Entry {
    let entry = gtk::Entry::builder()
        .placeholder_text(placeholder.unwrap_or(
            "Native modes: --userns=host, keep-id, auto, nomap, or explicit UID/GID maps",
        ))
        .tooltip_text(
            "Unrestricted native podman create arguments. Buzzard parses quoting into argv and passes every argument to stock Podman without filtering or rewriting.",
        )
        .hexpand(true)
        .build();
    if placeholder.is_none() {
        entry.set_text(DEFAULT_PODMAN_ARGUMENTS);
    }
    entry
}

fn append_podman_arguments(arguments: &mut Vec<String>, value: &str) {
    arguments.push("--podman-arguments".into());
    arguments.push(value.trim().into());
}

fn append_optional_podman_arguments(arguments: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if !value.is_empty() {
        append_podman_arguments(arguments, value);
    }
}

fn export_destination(folder: &Path, machine: &str) -> Result<PathBuf> {
    MachineConfig::validate_name(machine)?;
    if folder.as_os_str().is_empty() || !folder.is_absolute() {
        bail!("choose an absolute export folder with the file picker");
    }
    let metadata = std::fs::symlink_metadata(folder)
        .with_context(|| format!("inspecting export folder {}", folder.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("the export destination must be a real folder");
    }
    let destination = folder.join(format!("{machine}.oci.tar"));
    if destination.exists() {
        bail!(
            "the export archive already exists: {}",
            destination.display()
        );
    }
    Ok(destination)
}

fn prepare_builtin_context(variant: BuiltInMachine) -> Result<PathBuf> {
    let context = std::env::temp_dir().join(format!(
        "buzzardos-container-context-{}",
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        std::fs::create_dir_all(context.join("apt"))?;
        let containerfile = match variant {
            BuiltInMachine::Cuda => BUILTIN_CONTAINERFILE_CUDA,
            BuiltInMachine::Standard => BUILTIN_CONTAINERFILE_STANDARD,
        };
        write_embedded_build_asset(&context.join("Containerfile"), containerfile)?;
        write_embedded_build_asset(&context.join("provision-image.sh"), BUILTIN_PROVISION_IMAGE)?;
        for (name, contents) in [
            ("debian-sid-snapshot.sources", BUILTIN_APT_SNAPSHOT_SOURCES),
            ("debian-sid-live.sources", BUILTIN_APT_LIVE_SOURCES),
            ("99buzzardos-snapshot", BUILTIN_APT_SNAPSHOT_CONFIG),
        ] {
            write_embedded_build_asset(&context.join("apt").join(name), contents)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&context);
        return Err(error).context("preparing temporary built-in Containerfile context");
    }
    Ok(context)
}

fn write_embedded_build_asset(destination: &Path, contents: &[u8]) -> Result<()> {
    std::fs::write(destination, contents)
        .with_context(|| format!("writing built-in build asset {}", destination.display()))
}

fn settings_page(title: &str, detail: &str) -> (gtk::Box, gtk::Box) {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 14);
    root.set_margin_start(22);
    root.set_margin_end(22);
    root.set_margin_top(20);
    root.set_margin_bottom(20);
    let title = gtk::Label::new(Some(title));
    title.set_xalign(0.0);
    title.add_css_class("heading");
    let detail = gtk::Label::new(Some(detail));
    detail.set_xalign(0.0);
    detail.set_wrap(true);
    detail.add_css_class("dim-label");
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_vexpand(true);
    root.append(&title);
    root.append(&detail);
    root.append(&content);
    (root, content)
}

fn attach_manager_setting(grid: &gtk::Grid, row: i32, name: &str, value: &impl IsA<gtk::Widget>) {
    let label = gtk::Label::new(Some(name));
    label.set_xalign(0.0);
    label.set_mnemonic_widget(Some(value));
    grid.attach(&label, 0, row, 1, 1);
    grid.attach(value, 1, row, 1, 1);
}

fn machine_location_control(parent: &gtk::Window, machine_dir: &Path) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let location = gtk::Entry::builder()
        .text(machine_dir.to_string_lossy())
        .editable(false)
        .hexpand(true)
        .tooltip_text("The machine location is fixed; move a stopped machine folder and re-register it to change this path")
        .build();
    let open = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .label("Open Folder")
        .tooltip_text("Open this machine folder in the system file manager")
        .build();
    row.append(&location);
    row.append(&open);

    let parent = parent.clone();
    let machine_dir = machine_dir.to_path_buf();
    open.connect_clicked(move |_| {
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&machine_dir)));
        let parent = parent.clone();
        glib::spawn_future_local(async move {
            if let Err(error) = launcher.launch_future(Some(&parent)).await {
                show_manager_error(
                    &parent,
                    "Could not open the machine folder",
                    &anyhow::Error::new(error),
                );
            }
        });
    });
    row
}

fn manager_media_device_dropdown(
    devices: &[HostMediaDevice],
    kind: HostMediaKind,
    current: Option<&str>,
) -> (gtk::DropDown, Vec<Option<String>>) {
    let matching: Vec<_> = devices
        .iter()
        .filter(|device| device.kind == kind)
        .collect();
    let default = matching.iter().find(|device| device.is_default);
    let mut labels = vec![default.map_or_else(
        || "Automatic — no device currently advertised".to_owned(),
        |device| format!("Automatic — {}", device.description),
    )];
    let mut targets = vec![None];
    for device in matching {
        let duplicate_description = devices.iter().any(|other| {
            other.kind == kind
                && other.node_name != device.node_name
                && other.description == device.description
        });
        let mut label = if duplicate_description {
            format!("{} — {}", device.description, device.node_name)
        } else {
            device.description.clone()
        };
        if device.is_default {
            label.push_str(" (default)");
        }
        labels.push(label);
        targets.push(Some(device.node_name.clone()));
    }
    if let Some(current) = current
        && !targets.iter().flatten().any(|target| target == current)
    {
        labels.push(format!("Unavailable — {current}"));
        targets.push(Some(current.to_owned()));
    }
    let label_refs: Vec<_> = labels.iter().map(String::as_str).collect();
    let dropdown = gtk::DropDown::from_strings(&label_refs);
    dropdown.set_hexpand(true);
    let selected = current
        .and_then(|current| {
            targets
                .iter()
                .position(|target| target.as_deref() == Some(current))
        })
        .unwrap_or(0);
    dropdown.set_selected(u32::try_from(selected).unwrap_or(0));
    (dropdown, targets)
}

fn selected_manager_media_target(
    dropdown: &gtk::DropDown,
    targets: &[Option<String>],
) -> Option<String> {
    usize::try_from(dropdown.selected())
        .ok()
        .and_then(|index| targets.get(index))
        .cloned()
        .flatten()
}

fn comma_separated(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn manager_port_value(row: &ManagerPortRow) -> PortForward {
    PortForward {
        id: row.id,
        enabled: row.enabled.is_active(),
        direction: if row.direction.selected() == 1 {
            PortDirection::GuestToHost
        } else {
            PortDirection::HostToGuest
        },
        protocol: if row.protocol.selected() == 1 {
            PortProtocol::Udp
        } else {
            PortProtocol::Tcp
        },
        host_address: row.host_address.text().trim().to_owned(),
        host_port: row.host_port.value_as_int() as u16,
        guest_address: row.guest_address.text().trim().to_owned(),
        guest_port: row.guest_port.value_as_int() as u16,
    }
}

fn append_manager_port_row(
    list: &gtk::ListBox,
    rows: &Rc<RefCell<Vec<ManagerPortRow>>>,
    mapping: PortForward,
) {
    let row = gtk::ListBoxRow::new();
    let grid = gtk::Grid::builder()
        .column_spacing(8)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    let enabled = gtk::Switch::builder().active(mapping.enabled).build();
    let direction = gtk::DropDown::from_strings(&["Host → Guest", "Guest → Host"]);
    direction.set_selected(if mapping.direction == PortDirection::GuestToHost {
        1
    } else {
        0
    });
    let protocol = gtk::DropDown::from_strings(&["TCP", "UDP"]);
    protocol.set_selected(if mapping.protocol == PortProtocol::Udp {
        1
    } else {
        0
    });
    let host_address = gtk::Entry::builder()
        .text(&mapping.host_address)
        .width_chars(13)
        .build();
    let host_port = gtk::SpinButton::with_range(1.0, 65_535.0, 1.0);
    host_port.set_value(mapping.host_port as f64);
    host_port.set_width_chars(6);
    let guest_address = gtk::Entry::builder()
        .text(&mapping.guest_address)
        .width_chars(13)
        .build();
    let guest_port = gtk::SpinButton::with_range(1.0, 65_535.0, 1.0);
    guest_port.set_value(mapping.guest_port as f64);
    guest_port.set_width_chars(6);
    let remove = gtk::Button::from_icon_name("edit-delete-symbolic");
    remove.set_tooltip_text(Some("Remove mapping"));
    for (column, widget) in [
        enabled.clone().upcast::<gtk::Widget>(),
        direction.clone().upcast(),
        protocol.clone().upcast(),
        host_address.clone().upcast(),
        host_port.clone().upcast(),
        guest_address.clone().upcast(),
        guest_port.clone().upcast(),
        remove.clone().upcast(),
    ]
    .into_iter()
    .enumerate()
    {
        grid.attach(&widget, column as i32, 0, 1, 1);
    }
    row.set_child(Some(&grid));
    list.append(&row);
    let editor = ManagerPortRow {
        id: mapping.id,
        row: row.clone(),
        enabled,
        direction,
        protocol,
        host_address,
        host_port,
        guest_address,
        guest_port,
    };
    rows.borrow_mut().push(editor);
    let rows_for_remove = Rc::clone(rows);
    let list_for_remove = list.clone();
    let id = mapping.id;
    remove.connect_clicked(move |_| {
        if let Some(index) = rows_for_remove
            .borrow()
            .iter()
            .position(|candidate| candidate.id == id)
        {
            let removed = rows_for_remove.borrow_mut().remove(index);
            list_for_remove.remove(&removed.row);
        }
    });
}

fn manager_share_value(row: &ManagerShareRow) -> SharedPath {
    SharedPath {
        id: row.id,
        host_path: row.host_path.clone(),
        guest_name: row.guest_name.text().trim().to_owned(),
        read_only: row.read_only.is_active(),
    }
}

fn append_manager_share_row(
    list: &gtk::ListBox,
    rows: &Rc<RefCell<Vec<ManagerShareRow>>>,
    share: SharedPath,
) {
    let row = gtk::ListBoxRow::new();
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.set_margin_start(10);
    content.set_margin_end(10);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    let path = gtk::Label::new(Some(&share.host_path.to_string_lossy()));
    path.set_xalign(0.0);
    path.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    path.set_hexpand(true);
    path.set_tooltip_text(Some(&share.host_path.to_string_lossy()));
    let guest_name = gtk::Entry::builder()
        .text(&share.guest_name)
        .placeholder_text("Guest name")
        .width_chars(16)
        .build();
    let read_only = gtk::Switch::builder()
        .active(share.read_only)
        .tooltip_text("Read-only")
        .build();
    let remove = gtk::Button::from_icon_name("edit-delete-symbolic");
    remove.set_tooltip_text(Some("Remove shared path"));
    content.append(&path);
    content.append(&guest_name);
    content.append(&read_only);
    content.append(&remove);
    row.set_child(Some(&content));
    list.append(&row);
    rows.borrow_mut().push(ManagerShareRow {
        id: share.id,
        row: row.clone(),
        host_path: share.host_path,
        guest_name,
        read_only,
    });
    let rows_for_remove = Rc::clone(rows);
    let list_for_remove = list.clone();
    let id = share.id;
    remove.connect_clicked(move |_| {
        if let Some(index) = rows_for_remove
            .borrow()
            .iter()
            .position(|candidate| candidate.id == id)
        {
            let removed = rows_for_remove.borrow_mut().remove(index);
            list_for_remove.remove(&removed.row);
        }
    });
}

fn connect_manager_share_picker(
    button: &gtk::Button,
    parent: &gtk::Window,
    list: &gtk::ListBox,
    rows: &Rc<RefCell<Vec<ManagerShareRow>>>,
    directory: bool,
) {
    let parent = parent.clone();
    let list = list.clone();
    let rows = Rc::clone(rows);
    button.connect_clicked(move |_| {
        let add_share = {
            let parent = parent.clone();
            let list = list.clone();
            let rows = Rc::clone(&rows);
            move |path| match SharedPath::from_host_path(path) {
                Ok(share) => append_manager_share_row(&list, &rows, share),
                Err(error) => show_manager_error(&parent, "Could not share that path", &error),
            }
        };
        if directory {
            choose_folder_with_creation(&parent, "Choose folder to share", None, add_share);
        } else {
            let chooser = gtk::FileDialog::builder()
                .title("Choose file to share")
                .modal(true)
                .build();
            let parent = parent.clone();
            glib::spawn_future_local(async move {
                let Ok(file) = chooser.open_future(Some(&parent)).await else {
                    return;
                };
                let Some(path) = file.path() else {
                    return;
                };
                add_share(path);
            });
        }
    });
}

/// Editors and progress views are ordinary application windows. Pairing
/// `transient_for` with `modal` makes Mutter attach a child to the manager and
/// prevents the user from moving or using the manager independently. Reserve
/// that relationship for short confirmations and native file pickers.
fn independent_manager_window(
    parent: &gtk::ApplicationWindow,
    title: &str,
    default_width: i32,
    default_height: i32,
    resizable: bool,
) -> gtk::Window {
    let window = gtk::Window::builder()
        .title(title)
        .icon_name("buzzardos")
        .default_width(default_width)
        .default_height(default_height)
        .resizable(resizable)
        .build();
    window.set_application(parent.application().as_ref());
    debug_assert!(window.transient_for().is_none());
    debug_assert!(!window.is_modal());
    window
}

fn embedded_buzzard_icon(pixel_size: i32) -> gtk::Image {
    let bytes = glib::Bytes::from_static(HOST_ICON_PNG);
    let image = gtk::gdk::Texture::from_bytes(&bytes).map_or_else(
        |_| gtk::Image::from_icon_name("buzzardos"),
        |texture| gtk::Image::from_paintable(Some(&texture)),
    );
    image.set_pixel_size(pixel_size);
    image
}

fn show_manager_error(parent: &impl IsA<gtk::Window>, heading: &str, error: &anyhow::Error) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(heading)
        .detail(format!("{error:#}"))
        .buttons(["Close"])
        .cancel_button(0)
        .default_button(0)
        .build();
    dialog.show(Some(parent));
}

fn connect_machine_location_picker(
    button: &gtk::Button,
    parent: &gtk::Window,
    name: &gtk::Entry,
    destination: &gtk::Entry,
) {
    let selected_parent = Rc::new(RefCell::new(None::<PathBuf>));
    let parent_window = parent.clone();
    let name_for_picker = name.clone();
    let destination_for_picker = destination.clone();
    let selected_for_picker = Rc::clone(&selected_parent);
    button.connect_clicked(move |_| {
        let name = name_for_picker.clone();
        let destination = destination_for_picker.clone();
        let selected_parent = Rc::clone(&selected_for_picker);
        let initial_folder = selected_parent.borrow().clone();
        choose_folder_with_creation(
            &parent_window,
            "Choose where to store the machine",
            initial_folder.as_deref(),
            move |path| {
                selected_parent.replace(Some(path.clone()));
                let machine_name = name.text().trim().to_owned();
                if machine_name.is_empty() {
                    destination.set_text("");
                } else {
                    destination.set_text(&path.join(machine_name).to_string_lossy());
                }
            },
        );
    });
    let destination_for_name = destination.clone();
    name.connect_changed(move |name| {
        let Some(parent) = selected_parent.borrow().as_ref().cloned() else {
            return;
        };
        let machine_name = name.text().trim().to_owned();
        if machine_name.is_empty() {
            destination_for_name.set_text("");
        } else {
            destination_for_name.set_text(&parent.join(machine_name).to_string_lossy());
        }
    });
}

fn connect_folder_entry_picker(
    button: &gtk::Button,
    parent: &gtk::Window,
    entry: &gtk::Entry,
    title: &'static str,
) {
    let parent = parent.clone();
    let entry = entry.clone();
    button.connect_clicked(move |_| {
        let entry = entry.clone();
        let initial = PathBuf::from(entry.text().trim());
        choose_folder_with_creation(
            &parent,
            title,
            initial.is_dir().then_some(initial.as_path()),
            move |path| {
                entry.set_text(&path.to_string_lossy());
            },
        );
    });
}

// GtkFileDialog delegates its contents to the desktop portal, and some portal
// implementations omit folder creation from select-folder mode. Keep the
// native GTK file browser, but own the window and its visible New Folder
// action so the control cannot disappear across portal implementations.
#[allow(deprecated)]
fn choose_folder_with_creation(
    parent: &impl IsA<gtk::Window>,
    title: &str,
    initial_folder: Option<&Path>,
    on_accept: impl Fn(PathBuf) + 'static,
) {
    let parent = parent.upcast_ref::<gtk::Window>();
    let dialog = gtk::Window::builder()
        .title(title)
        .icon_name("buzzardos")
        .transient_for(parent)
        .modal(true)
        .default_width(820)
        .default_height(560)
        .build();
    if let Some(application) = parent.application() {
        dialog.set_application(Some(&application));
    }
    let header = gtk::HeaderBar::builder().show_title_buttons(true).build();
    dialog.set_titlebar(Some(&header));
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let chooser = gtk::FileChooserWidget::new(gtk::FileChooserAction::SelectFolder);
    chooser.set_create_folders(true);
    chooser.set_hexpand(true);
    chooser.set_vexpand(true);
    let initial_folder = initial_folder
        .filter(|folder| folder.is_dir())
        .map(Path::to_path_buf)
        .unwrap_or_else(glib::home_dir);
    let _ = chooser.set_current_folder(Some(&gio::File::for_path(initial_folder)));
    root.append(&chooser);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_margin_start(12);
    actions.set_margin_end(12);
    actions.set_margin_top(12);
    actions.set_margin_bottom(12);
    let new_folder = gtk::Button::with_label("New Folder…");
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    let cancel = gtk::Button::with_label("Cancel");
    let select = gtk::Button::with_label("Select");
    actions.append(&new_folder);
    actions.append(&spacer);
    actions.append(&cancel);
    actions.append(&select);
    root.append(&actions);
    dialog.set_child(Some(&root));

    let new_folder_parent = dialog.clone();
    let chooser_for_new_folder = chooser.clone();
    new_folder.connect_clicked(move |_| {
        show_new_folder_dialog(&new_folder_parent, &chooser_for_new_folder);
    });
    let close = dialog.clone();
    cancel.connect_clicked(move |_| close.close());
    let close = dialog.clone();
    select.connect_clicked(move |_| {
        if let Some(path) = chooser.file().and_then(|file| file.path()) {
            on_accept(path);
            close.close();
        }
    });
    dialog.present();
}

#[allow(deprecated)]
fn show_new_folder_dialog(parent: &gtk::Window, chooser: &gtk::FileChooserWidget) {
    let Some(parent_directory) = chooser.current_folder().and_then(|folder| folder.path()) else {
        show_manager_error(
            parent,
            "Choose a folder first",
            &anyhow::anyhow!("the current location cannot contain folders"),
        );
        return;
    };
    let dialog = gtk::Window::builder()
        .title("New Folder")
        .icon_name("buzzardos")
        .transient_for(parent)
        .modal(true)
        .resizable(false)
        .default_width(380)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.set_margin_start(16);
    root.set_margin_end(16);
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    let label = gtk::Label::new(Some("Folder name"));
    label.set_xalign(0.0);
    let name = gtk::Entry::new();
    name.set_hexpand(true);
    root.append(&label);
    root.append(&name);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let create = gtk::Button::with_label("Create");
    actions.append(&cancel);
    actions.append(&create);
    root.append(&actions);
    dialog.set_child(Some(&root));

    let close = dialog.clone();
    cancel.connect_clicked(move |_| close.close());
    let close = dialog.clone();
    let chooser = chooser.clone();
    let name_for_create = name.clone();
    create.connect_clicked(move |_| {
        let folder_name = name_for_create.text().trim().to_owned();
        if !valid_new_folder_name(&folder_name) {
            show_manager_error(
                &close,
                "Enter a folder name",
                &anyhow::anyhow!("use one name without /, . or .."),
            );
            return;
        }
        let destination = parent_directory.join(folder_name);
        if let Err(error) = std::fs::create_dir(&destination) {
            show_manager_error(
                &close,
                "Could not create the folder",
                &anyhow::Error::new(error).context(destination.display().to_string()),
            );
            return;
        }
        let _ = chooser.set_current_folder(Some(&gio::File::for_path(destination)));
        close.close();
    });
    dialog.present();
    name.grab_focus();
}

fn valid_new_folder_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

fn refresh_weak(weak: &Weak<ManagerUi>) {
    if let Some(manager) = weak.upgrade()
        && let Err(error) = manager.refresh()
    {
        show_manager_error(&manager.window, "Could not refresh machines", &error);
    }
}

fn cargo_dependency_summary(inventory: &str) -> String {
    let mut output = String::new();
    for line in inventory
        .lines()
        .filter(|line| !line.starts_with('#'))
        .skip(1)
    {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() < 7 {
            continue;
        }
        let license = if fields[5].is_empty() {
            fields[4]
        } else {
            fields[5]
        };
        let _ = writeln!(output, "{} {} — {}", fields[0], fields[1], license);
    }
    output
}

fn host_license_document() -> String {
    let rust_standard_library_notice = std::fs::read_to_string(
        "/usr/share/doc/buzzardos/rust/COPYRIGHT-library.html",
    )
    .unwrap_or_else(|_| {
        "The complete checksum-verified Rust standard-library notice is installed at /usr/share/doc/buzzardos/rust/COPYRIGHT-library.html in the packaged application."
            .to_owned()
    });
    format!(
        "BUZZARD OS — AGPL-3.0-OR-LATER\n\n{HOST_PROJECT_LICENSE}\n\nAPPLICATION METADATA — CC0-1.0\n\norg.openresearchtools.BuzzardOS.metainfo.xml is licensed under CC0-1.0. The full text is installed at /usr/share/common-licenses/CC0-1.0.\n\nRUST STANDARD LIBRARY\n\n{rust_standard_library_notice}\n\nRUST DEPENDENCIES\n\n{HOST_RUST_DEPENDENCY_NOTICES}"
    )
}

fn document_page(contents: &str, monospace: bool) -> gtk::ScrolledWindow {
    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_monospace(monospace);
    view.set_left_margin(12);
    view.set_right_margin(12);
    view.set_top_margin(12);
    view.set_bottom_margin(12);
    view.buffer().set_text(contents);
    gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&view)
        .build()
}

#[cfg(test)]
mod license_tests {
    use super::*;

    #[test]
    fn manager_identity_is_stable_per_portable_launcher() {
        let first = Path::new("/opt/BuzzardOS/BuzzardOS");
        let second = Path::new("/mnt/portable/BuzzardOS/BuzzardOS");
        assert_eq!(manager_application_id(first), manager_application_id(first));
        assert_ne!(
            manager_application_id(first),
            manager_application_id(second)
        );
        assert!(
            manager_application_id(first).starts_with("org.openresearchtools.buzzardos.manager.x")
        );
    }

    #[test]
    fn manager_command_line_preserves_settings_destination() {
        let request = ManagerArgs {
            launcher: PathBuf::from("/opt/BuzzardOS/BuzzardOS"),
            settings_machine: Some(PathBuf::from("/data/Machines/demo")),
        };
        let invocation = manager_invocation(&request).unwrap();
        let parsed = ManagerArgs::try_parse_from(invocation).unwrap();
        assert_eq!(parsed.launcher, request.launcher);
        assert_eq!(parsed.settings_machine, request.settings_machine);
    }

    #[test]
    fn new_folder_name_is_one_safe_path_component() {
        assert!(valid_new_folder_name("My machine"));
        assert!(valid_new_folder_name("machine-2"));
        for invalid in ["", ".", "..", "nested/folder", "bad\0name"] {
            assert!(!valid_new_folder_name(invalid));
        }
    }

    #[test]
    fn host_inventory_is_presented_without_guest_package_material() {
        let dependencies = cargo_dependency_summary(HOST_CARGO_INVENTORY);
        assert!(dependencies.contains("anyhow "));
        let licenses = host_license_document();
        assert!(!licenses.contains("trycua/cua"));
        assert!(!licenses.contains("Sway, wlroots"));
    }

    #[test]
    fn about_scope_excludes_machine_and_guest_licenses() {
        assert!(MACHINE_LICENSE_EXCLUSION.contains("machine images"));
        assert!(MACHINE_LICENSE_EXCLUSION.contains("guest components"));
        assert!(EXTERNAL_HOST_DEPENDENCIES.contains("not bundled"));
        assert!(host_license_document().contains("AGPL-3.0-OR-LATER"));
        assert!(!host_license_document().contains("PACKAGE SCOPE"));
        assert!(!host_license_document().contains("PACKAGE NOTICE"));
        assert!(!host_license_document().contains("DEBIAN PACKAGE COPYRIGHT"));
        assert!(!cargo_dependency_summary(HOST_CARGO_INVENTORY).is_empty());
    }

    #[test]
    fn built_in_context_bootstraps_the_signed_apt_repository() {
        for (variant, cuda) in [
            (BuiltInMachine::Cuda, true),
            (BuiltInMachine::Standard, false),
        ] {
            let context = prepare_builtin_context(variant).unwrap();
            assert!(context.join("Containerfile").is_file());
            assert!(context.join("apt/debian-sid-live.sources").is_file());
            assert!(!context.join("debs").exists());
            let recipe = std::fs::read_to_string(context.join("Containerfile")).unwrap();
            assert!(recipe.contains("https://keyring.openresearchtools.com"));
            assert!(recipe.contains("buzzardos-guest=${BUZZARDOS_GUEST_VERSION}"));
            assert_eq!(recipe.contains("CUDA_VERSION=13.3.1"), cuda);
            std::fs::remove_dir_all(context).unwrap();
        }
    }

    #[test]
    fn machine_creation_requires_an_exact_new_absolute_folder() {
        let temp = tempfile::tempdir().unwrap();
        assert!(validate_machine_destination("demo", &temp.path().join("demo")).is_ok());
        assert!(validate_machine_destination("bad/name", &temp.path().join("demo")).is_err());
        assert!(validate_machine_destination("demo", Path::new("relative/demo")).is_err());
        assert!(validate_machine_destination("demo", temp.path()).is_err());
    }

    #[test]
    fn export_uses_a_selected_folder_and_a_safe_automatic_filename() {
        let temp = tempfile::tempdir().unwrap();
        let output = export_destination(temp.path(), "portable-demo").unwrap();
        assert_eq!(output, temp.path().join("portable-demo.oci.tar"));
        std::fs::write(&output, b"existing archive").unwrap();
        assert!(export_destination(temp.path(), "portable-demo").is_err());
        assert!(export_destination(Path::new("relative"), "portable-demo").is_err());
    }

    #[test]
    fn progress_window_uses_concise_phases_and_status_messages() {
        let build = command_presentation(&["build".into(), "demo".into()]);
        assert_eq!(build.running, "Building container image…");
        assert_eq!(build.success, "Machine created");
        assert_eq!(
            concise_failure(
                build.failure,
                "verbose output\nBuzzard OS: Buildah rejected the image\n"
            ),
            "Machine creation failed: Buildah rejected the image"
        );
        assert_eq!(
            progress_stage("Copying blob abc"),
            Some("Downloading base image…")
        );
        assert_eq!(
            progress_stage("Writing manifest to image destination"),
            Some("Finalizing container image…")
        );
        assert_eq!(progress_fraction("STEP 6/24: RUN apt-get"), Some(0.2));
        assert_eq!(progress_fraction("STEP 0/24: FROM debian"), None);
        assert_eq!(progress_fraction("not a build step"), None);
    }

    #[test]
    fn progress_log_removes_terminal_control_sequences() {
        assert_eq!(
            sanitize_progress_line("\u{1b}[2A\u{1b}[JCopying blob 50%\r"),
            "Copying blob 50%"
        );
    }

    #[test]
    fn overflow_menu_has_no_redundant_open_machine_action() {
        assert_eq!(
            MACHINE_OVERFLOW_ACTIONS,
            [
                ("Export…", "export"),
                ("Clone…", "clone"),
                ("Delete…", "delete"),
            ]
        );
        assert!(
            MACHINE_OVERFLOW_ACTIONS
                .iter()
                .all(|(_, action)| *action != "open")
        );
    }

    #[test]
    fn lifecycle_state_change_invalidates_the_manager_snapshot() {
        let stopped = MachineListSignature {
            directory: PathBuf::from("/tmp/machine"),
            name: "machine".into(),
            width: 1280,
            height: 800,
            network: "Private network".into(),
            state: MachineState::Stopped,
        };
        let mut running = stopped.clone();
        running.state = MachineState::Running;
        assert_ne!(stopped, running);
    }
}
