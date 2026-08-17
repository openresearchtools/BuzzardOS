// SPDX-License-Identifier: AGPL-3.0-or-later

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use clap::Parser;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use wb_core::{MachineConfig, MachineRegistry, RuntimeState};

#[derive(Debug, Parser)]
#[command(name = "buzzardos-display --machine-manager")]
struct ManagerArgs {
    #[arg(long)]
    launcher: PathBuf,
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
    let application = gtk::Application::builder()
        .application_id("org.openresearchtools.buzzardos.manager")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let activation = Rc::new(RefCell::new(Some(args.launcher)));
    // GTK owns the application window after it is presented, but the window
    // does not own `ManagerUi`.  Keep the controller alive for the complete
    // application lifetime so its weak button callbacks remain usable.
    let active_manager = Rc::new(RefCell::new(None::<Rc<ManagerUi>>));
    let active_manager_for_activation = Rc::clone(&active_manager);
    application.connect_activate(move |application| {
        if let Some(manager) = active_manager_for_activation.borrow().as_ref() {
            manager.window.present();
            return;
        }
        let Some(launcher) = activation.borrow_mut().take() else {
            if let Some(window) = application.active_window() {
                window.present();
            }
            return;
        };
        match ManagerUi::build(application, launcher) {
            Ok(manager) => {
                manager.window.present();
                active_manager_for_activation.replace(Some(manager));
            }
            Err(error) => {
                eprintln!("Buzzard OS machine manager: {error:#}");
                application.quit();
            }
        }
    });
    let status = application.run_with_args(&["BuzzardOS"]);
    drop(active_manager);
    if status != glib::ExitCode::SUCCESS {
        bail!("machine manager exited with {status:?}");
    }
    Ok(())
}

struct ManagerUi {
    window: gtk::ApplicationWindow,
    launcher: PathBuf,
    list: gtk::ListBox,
    status: gtk::Label,
    command_result: Arc<Mutex<Option<String>>>,
}

impl ManagerUi {
    fn build(application: &gtk::Application, launcher: PathBuf) -> Result<Rc<Self>> {
        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .title("Buzzard OS Machines")
            .default_width(760)
            .default_height(520)
            .build();
        let header = gtk::HeaderBar::builder()
            .title_widget(&gtk::Label::new(Some("Buzzard OS Machines")))
            .show_title_buttons(true)
            .build();
        let create = gtk::Button::with_label("Create");
        create.set_tooltip_text(Some("Create a machine from the bundled OCI image"));
        let import = gtk::Button::with_label("Import OCI");
        let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
        refresh.set_tooltip_text(Some("Refresh machines"));
        header.pack_start(&create);
        header.pack_start(&import);
        header.pack_end(&refresh);
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
        scroller.set_child(Some(&list));
        root.append(&scroller);
        let status = gtk::Label::new(Some("Ready"));
        status.set_xalign(0.0);
        status.set_margin_start(12);
        status.set_margin_end(12);
        status.set_margin_top(8);
        status.set_margin_bottom(8);
        status.add_css_class("dim-label");
        root.append(&status);
        window.set_child(Some(&root));

        let manager = Rc::new(Self {
            window,
            launcher,
            list,
            status,
            command_result: Arc::new(Mutex::new(None)),
        });
        manager.refresh()?;

        let weak = Rc::downgrade(&manager);
        refresh.connect_clicked(move |_| refresh_weak(&weak));
        let weak = Rc::downgrade(&manager);
        create.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                manager.show_create_dialog();
            }
        });
        let weak = Rc::downgrade(&manager);
        import.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                manager.show_import_dialog();
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
                manager.status.set_text(&result);
                let _ = manager.refresh();
            }
            glib::ControlFlow::Continue
        });
        Ok(manager)
    }

    fn refresh(self: &Rc<Self>) -> Result<()> {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        let registry = MachineRegistry::discover()?;
        let mut machines = registry
            .entries()
            .iter()
            .filter_map(|entry| {
                MachineConfig::load(&entry.machine_dir)
                    .ok()
                    .map(|config| (entry.machine_dir.clone(), config))
            })
            .collect::<Vec<_>>();
        machines.sort_by(|left, right| left.1.name.cmp(&right.1.name));
        if machines.is_empty() {
            let empty =
                gtk::Label::new(Some("No machines yet. Create one or import an OCI image."));
            empty.set_margin_top(48);
            empty.set_margin_bottom(48);
            empty.add_css_class("dim-label");
            self.list.append(&empty);
            return Ok(());
        }
        for (directory, config) in machines {
            self.list.append(&self.machine_row(&directory, &config));
        }
        Ok(())
    }

    fn machine_row(self: &Rc<Self>, directory: &Path, config: &MachineConfig) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_margin_top(10);
        content.set_margin_bottom(10);
        let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let name = gtk::Label::new(Some(&config.name));
        name.set_xalign(0.0);
        name.add_css_class("title-4");
        let state = RuntimeState::load(directory)
            .ok()
            .flatten()
            .map(|state| format!("{:?}", state.state))
            .unwrap_or_else(|| "Stopped".into());
        let state = gtk::Label::new(Some(&state));
        state.set_xalign(0.0);
        state.add_css_class("dim-label");
        labels.append(&name);
        labels.append(&state);
        labels.set_hexpand(true);
        content.append(&labels);
        for (label, action) in [
            ("Start", "start"),
            ("Stop", "stop"),
            ("Export", "export"),
            ("Clone", "clone"),
            ("Delete", "delete"),
        ] {
            let button = gtk::Button::with_label(label);
            if action == "delete" {
                button.add_css_class("destructive-action");
            }
            let weak = Rc::downgrade(self);
            let machine = config.name.clone();
            let machine_dir = directory.to_path_buf();
            button.connect_clicked(move |_| {
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                match action {
                    "start" => manager.run_command(
                        Some(machine_dir.clone()),
                        vec!["start".into(), machine.clone(), "--detach".into()],
                    ),
                    "stop" => manager.run_command(
                        Some(machine_dir.clone()),
                        vec!["stop".into(), machine.clone()],
                    ),
                    "export" => manager.show_export_dialog(&machine, &machine_dir),
                    "clone" => manager.show_clone_dialog(&machine),
                    "delete" => manager.show_delete_dialog(&machine, &machine_dir),
                    _ => unreachable!(),
                }
            });
            content.append(&button);
        }
        row.set_child(Some(&content));
        row
    }

    fn run_command(&self, machine_dir: Option<PathBuf>, arguments: Vec<String>) {
        self.status.set_text("Working…");
        let launcher = self.launcher.clone();
        let result = self.command_result.clone();
        std::thread::spawn(move || {
            let mut command = Command::new(&launcher);
            if let Some(machine_dir) = machine_dir {
                command.arg("--machine-dir").arg(machine_dir);
            }
            let output = command.args(&arguments).stdin(Stdio::null()).output();
            let message = match output {
                Ok(output) if output.status.success() => {
                    String::from_utf8_lossy(&output.stdout).trim().to_owned()
                }
                Ok(output) => format!(
                    "Command failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                Err(error) => format!("Could not start Buzzard OS: {error}"),
            };
            if let Ok(mut slot) = result.lock() {
                *slot = Some(if message.is_empty() {
                    "Done".into()
                } else {
                    message
                });
            }
        });
    }

    fn show_create_dialog(self: &Rc<Self>) {
        self.show_oci_machine_dialog(false);
    }

    fn show_import_dialog(self: &Rc<Self>) {
        self.show_oci_machine_dialog(true);
    }

    fn show_oci_machine_dialog(self: &Rc<Self>, importing: bool) {
        let dialog = gtk::Window::builder()
            .transient_for(&self.window)
            .modal(true)
            .title(if importing {
                "Import OCI machine"
            } else {
                "Create OCI machine"
            })
            .default_width(680)
            .build();
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
            "OCI path or reference"
        } else {
            "OCI image reference"
        }));
        source_label.set_xalign(0.0);
        let source = gtk::Entry::new();
        source.set_hexpand(true);
        grid.attach(&source_label, 0, 0, 1, 1);
        grid.attach(&source, 1, 0, 1, 1);
        let name_label = gtk::Label::new(Some("Machine name"));
        name_label.set_xalign(0.0);
        let name = gtk::Entry::new();
        name.set_hexpand(true);
        grid.attach(&name_label, 0, 1, 1, 1);
        grid.attach(&name, 1, 1, 1, 1);
        let mode = gtk::DropDown::from_strings(&[
            "Restore — preserve exported identity",
            "Clone — generate a new identity",
        ]);
        if importing {
            let mode_label = gtk::Label::new(Some("Identity"));
            mode_label.set_xalign(0.0);
            mode.set_hexpand(true);
            grid.attach(&mode_label, 0, 2, 1, 1);
            grid.attach(&mode, 1, 2, 1, 1);
        }
        let destination_label = gtk::Label::new(Some("Machine parent folder"));
        destination_label.set_xalign(0.0);
        let destination_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let destination = gtk::Entry::new();
        destination.set_hexpand(true);
        let browse_destination = gtk::Button::with_label("Browse…");
        destination_row.append(&destination);
        destination_row.append(&browse_destination);
        grid.attach(&destination_label, 0, 3, 1, 1);
        grid.attach(&destination_row, 1, 3, 1, 1);
        root.append(&grid);

        let explanation = gtk::Label::new(Some(
            "The machine folder is created below the selected parent. Shared files and folders are optional and appear as separate entries below /shared in the guest.",
        ));
        explanation.set_wrap(true);
        explanation.set_xalign(0.0);
        explanation.add_css_class("dim-label");
        root.append(&explanation);

        let shares_title = gtk::Label::new(Some("Optional shared files and folders"));
        shares_title.set_xalign(0.0);
        shares_title.add_css_class("heading");
        root.append(&shares_title);
        let shares_list = gtk::ListBox::new();
        shares_list.add_css_class("boxed-list");
        shares_list.set_selection_mode(gtk::SelectionMode::None);
        root.append(&shares_list);
        let shares = Rc::new(RefCell::new(Vec::<PathBuf>::new()));
        let share_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let add_file = gtk::Button::with_label("Add File…");
        let add_folder = gtk::Button::with_label("Add Folder…");
        share_actions.append(&add_file);
        share_actions.append(&add_folder);
        root.append(&share_actions);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        actions.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let accept = gtk::Button::with_label(if importing { "Import" } else { "Create" });
        accept.add_css_class("suggested-action");
        actions.append(&cancel);
        actions.append(&accept);
        root.append(&actions);
        dialog.set_child(Some(&root));

        let chooser_parent = dialog.clone();
        let destination_for_picker = destination.clone();
        browse_destination.connect_clicked(move |_| {
            let chooser = gtk::FileDialog::builder()
                .title("Choose machine parent folder")
                .modal(true)
                .build();
            let chooser_parent = chooser_parent.clone();
            let destination = destination_for_picker.clone();
            glib::spawn_future_local(async move {
                if let Ok(file) = chooser.select_folder_future(Some(&chooser_parent)).await
                    && let Some(path) = file.path()
                {
                    destination.set_text(&path.to_string_lossy());
                }
            });
        });

        connect_share_picker(&add_file, &dialog, &shares_list, &shares, false);
        connect_share_picker(&add_folder, &dialog, &shares_list, &shares, true);

        let close = dialog.clone();
        cancel.connect_clicked(move |_| close.close());
        let close = dialog.clone();
        let weak = Rc::downgrade(self);
        accept.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                let machine_name = name.text().trim().to_owned();
                let parent = PathBuf::from(destination.text().trim());
                let machine_dir = parent.join(&machine_name);
                let mut arguments = if importing {
                    vec![
                        "import".into(),
                        source.text().trim().to_owned(),
                        "--name".into(),
                        machine_name,
                        "--mode".into(),
                        if mode.selected() == 1 {
                            "clone".into()
                        } else {
                            "restore".into()
                        },
                    ]
                } else {
                    vec![
                        "create".into(),
                        machine_name,
                        "--image".into(),
                        source.text().trim().to_owned(),
                    ]
                };
                for share in shares.borrow().iter() {
                    arguments.push("--share".into());
                    arguments.push(share.to_string_lossy().into_owned());
                }
                manager.run_command(Some(machine_dir), arguments);
            }
            close.close();
        });
        dialog.present();
    }

    fn show_export_dialog(self: &Rc<Self>, machine: &str, machine_dir: &Path) {
        let destination = machine_dir.join(format!("{machine}.oci.tar.zst"));
        let destination = destination.to_string_lossy().into_owned();
        let weak = Rc::downgrade(self);
        let machine = machine.to_owned();
        let machine_dir = machine_dir.to_path_buf();
        show_text_dialog(
            &self.window,
            "Export OCI machine",
            &[("Destination", destination.as_str())],
            move |values| {
                if let Some(manager) = weak.upgrade() {
                    manager.run_command(
                        Some(machine_dir.clone()),
                        vec![
                            "export".into(),
                            machine.clone(),
                            "--output".into(),
                            values[0].clone(),
                        ],
                    );
                }
            },
        );
    }

    fn show_clone_dialog(self: &Rc<Self>, machine: &str) {
        let dialog = gtk::Window::builder()
            .transient_for(&self.window)
            .modal(true)
            .title("Clone machine")
            .default_width(620)
            .build();
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
            .placeholder_text("Machine parent folder")
            .hexpand(true)
            .build();
        let browse = gtk::Button::with_label("Browse…");
        destination_row.append(&destination);
        destination_row.append(&browse);
        root.append(&destination_row);
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.set_halign(gtk::Align::End);
        let cancel = gtk::Button::with_label("Cancel");
        let accept = gtk::Button::with_label("Clone");
        accept.add_css_class("suggested-action");
        actions.append(&cancel);
        actions.append(&accept);
        root.append(&actions);
        dialog.set_child(Some(&root));

        let chooser_parent = dialog.clone();
        let destination_for_picker = destination.clone();
        browse.connect_clicked(move |_| {
            let chooser = gtk::FileDialog::builder()
                .title("Choose machine parent folder")
                .modal(true)
                .build();
            let chooser_parent = chooser_parent.clone();
            let destination = destination_for_picker.clone();
            glib::spawn_future_local(async move {
                if let Ok(file) = chooser.select_folder_future(Some(&chooser_parent)).await
                    && let Some(path) = file.path()
                {
                    destination.set_text(&path.to_string_lossy());
                }
            });
        });
        let close = dialog.clone();
        cancel.connect_clicked(move |_| close.close());
        let close = dialog.clone();
        let weak = Rc::downgrade(self);
        let source = machine.to_owned();
        accept.connect_clicked(move |_| {
            if let Some(manager) = weak.upgrade() {
                let new_name = name.text().trim().to_owned();
                let machine_dir = PathBuf::from(destination.text().trim()).join(&new_name);
                manager.run_command(
                    Some(machine_dir),
                    vec!["clone".into(), source.clone(), new_name],
                );
            }
            close.close();
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

fn connect_share_picker(
    button: &gtk::Button,
    parent: &gtk::Window,
    list: &gtk::ListBox,
    shares: &Rc<RefCell<Vec<PathBuf>>>,
    directory: bool,
) {
    let parent = parent.clone();
    let list = list.clone();
    let shares = Rc::clone(shares);
    button.connect_clicked(move |_| {
        let chooser = gtk::FileDialog::builder()
            .title(if directory {
                "Choose folder to share"
            } else {
                "Choose file to share"
            })
            .modal(true)
            .build();
        let parent = parent.clone();
        let list = list.clone();
        let shares = Rc::clone(&shares);
        glib::spawn_future_local(async move {
            let selected = if directory {
                chooser.select_folder_future(Some(&parent)).await
            } else {
                chooser.open_future(Some(&parent)).await
            };
            if let Ok(file) = selected
                && let Some(path) = file.path()
            {
                append_share_row(&list, &shares, path);
            }
        });
    });
}

fn append_share_row(list: &gtk::ListBox, shares: &Rc<RefCell<Vec<PathBuf>>>, path: PathBuf) {
    if shares.borrow().contains(&path) {
        return;
    }
    shares.borrow_mut().push(path.clone());
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.set_margin_start(8);
    row.set_margin_end(8);
    row.set_margin_top(6);
    row.set_margin_bottom(6);
    let label = gtk::Label::new(Some(&path.to_string_lossy()));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let remove = gtk::Button::with_label("Remove");
    row.append(&label);
    row.append(&remove);
    list.append(&row);
    let list = list.clone();
    let row_for_remove = row.clone();
    let shares = Rc::clone(shares);
    remove.connect_clicked(move |_| {
        shares.borrow_mut().retain(|candidate| candidate != &path);
        list.remove(&row_for_remove);
    });
}

fn show_text_dialog(
    parent: &gtk::ApplicationWindow,
    title: &str,
    fields: &[(&str, &str)],
    on_accept: impl Fn(Vec<String>) + 'static,
) {
    let dialog = gtk::Window::builder()
        .transient_for(parent)
        .modal(true)
        .title(title)
        .resizable(false)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.set_margin_start(16);
    root.set_margin_end(16);
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    let grid = gtk::Grid::builder()
        .column_spacing(10)
        .row_spacing(8)
        .build();
    let mut entries = Vec::new();
    for (row, (field, initial)) in fields.iter().enumerate() {
        let label = gtk::Label::new(Some(*field));
        label.set_xalign(0.0);
        let entry = gtk::Entry::new();
        entry.set_text(initial);
        entry.set_hexpand(true);
        grid.attach(&label, 0, row as i32, 1, 1);
        grid.attach(&entry, 1, row as i32, 1, 1);
        entries.push(entry);
    }
    root.append(&grid);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let accept = gtk::Button::with_label("Continue");
    accept.add_css_class("suggested-action");
    actions.append(&cancel);
    actions.append(&accept);
    root.append(&actions);
    dialog.set_child(Some(&root));

    let close_window = dialog.clone();
    cancel.connect_clicked(move |_| close_window.close());
    let close_window = dialog.clone();
    accept.connect_clicked(move |_| {
        on_accept(
            entries
                .iter()
                .map(|entry| entry.text().trim().to_owned())
                .collect(),
        );
        close_window.close();
    });
    dialog.present();
}

fn refresh_weak(weak: &Weak<ManagerUi>) {
    if let Some(manager) = weak.upgrade()
        && let Err(error) = manager.refresh()
    {
        manager
            .status
            .set_text(&format!("Refresh failed: {error:#}"));
    }
}
