// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

pub const PANEL_HEIGHT: i32 = 42;
pub const MENU_WIDTH: i32 = 420;
pub const MENU_ROW_HEIGHT: i32 = 36;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Application {
    pub id: String,
    pub name: String,
    pub generic_name: Option<String>,
    pub command: Vec<String>,
    pub icon: Option<String>,
    pub categories: Vec<String>,
    pub source: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GuestWindow {
    pub id: u32,
    pub title: String,
    pub app_id: String,
    pub focused: bool,
    pub minimized: bool,
    pub maximized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.x.saturating_add(self.width))
            && y < f64::from(self.y.saturating_add(self.height))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellAction {
    ToggleApplications,
    OpenFiles,
    OpenShared,
    LaunchApplication(String),
    ActivateWindow(u32),
    TaskbarPrevious,
    TaskbarNext,
    ShutdownMachine,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitTarget {
    pub rect: Rect,
    pub label: String,
    pub action: ShellAction,
}

pub fn panel_targets(width: u32, windows: &[GuestWindow], page: usize) -> Vec<HitTarget> {
    const APPLICATIONS_WIDTH: i32 = 126;
    const STATUS_WIDTH: i32 = 92;
    const NAV_WIDTH: i32 = 24;
    const MIN_TASK_WIDTH: i32 = 148;

    let width = i32::try_from(width).unwrap_or(i32::MAX).max(1);
    let mut x = 0;
    let mut targets = vec![HitTarget {
        rect: Rect {
            x,
            y: 0,
            width: APPLICATIONS_WIDTH.min(width),
            height: PANEL_HEIGHT,
        },
        label: "Applications".to_owned(),
        action: ShellAction::ToggleApplications,
    }];
    x += APPLICATIONS_WIDTH.min(width);

    let right_edge = width.saturating_sub(STATUS_WIDTH).max(x);
    let available = right_edge.saturating_sub(x).max(0);
    let initial_slots = usize::try_from((available / MIN_TASK_WIDTH).max(1)).unwrap_or(1);
    let needs_paging = windows.len() > initial_slots;
    let task_space = available.saturating_sub(if needs_paging { NAV_WIDTH * 2 } else { 0 });
    let slots = usize::try_from((task_space / MIN_TASK_WIDTH).max(1)).unwrap_or(1);
    let pages = windows.len().div_ceil(slots).max(1);
    let start = (page % pages).saturating_mul(slots).min(windows.len());
    let end = start.saturating_add(slots).min(windows.len());
    let visible = &windows[start..end];

    if needs_paging {
        targets.push(HitTarget {
            rect: Rect {
                x,
                y: 0,
                width: NAV_WIDTH,
                height: PANEL_HEIGHT,
            },
            label: "Previous running applications".to_owned(),
            action: ShellAction::TaskbarPrevious,
        });
        x += NAV_WIDTH;
    }

    let task_width = if visible.is_empty() {
        task_space
    } else {
        task_space / i32::try_from(visible.len()).unwrap_or(1)
    };
    for window in visible {
        targets.push(HitTarget {
            rect: Rect {
                x,
                y: 0,
                width: task_width,
                height: PANEL_HEIGHT,
            },
            label: format!("Switch to {}", window.title),
            action: ShellAction::ActivateWindow(window.id),
        });
        x += task_width;
    }

    if needs_paging {
        targets.push(HitTarget {
            rect: Rect {
                x,
                y: 0,
                width: NAV_WIDTH,
                height: PANEL_HEIGHT,
            },
            label: "Next running applications".to_owned(),
            action: ShellAction::TaskbarNext,
        });
    }
    targets
}

pub fn menu_targets(
    menu_height: u32,
    applications: &[Application],
    scroll: usize,
) -> Vec<HitTarget> {
    const HEADER_HEIGHT: i32 = 54;
    const SECTION_HEIGHT: i32 = 26;
    const FOOTER_HEIGHT: i32 = 50;
    let menu_height = i32::try_from(menu_height).unwrap_or(i32::MAX);
    let mut targets = Vec::new();
    let mut y = HEADER_HEIGHT + SECTION_HEIGHT;

    let available = menu_height
        .saturating_sub(y)
        .saturating_sub(FOOTER_HEIGHT)
        .max(MENU_ROW_HEIGHT);
    let visible_rows = usize::try_from((available / MENU_ROW_HEIGHT).max(1)).unwrap_or(1);
    for application in applications.iter().skip(scroll).take(visible_rows) {
        targets.push(HitTarget {
            rect: Rect {
                x: 8,
                y,
                width: MENU_WIDTH - 16,
                height: MENU_ROW_HEIGHT,
            },
            label: application.name.clone(),
            action: ShellAction::LaunchApplication(application.id.clone()),
        });
        y += MENU_ROW_HEIGHT;
    }

    targets.push(HitTarget {
        rect: Rect {
            x: 8,
            y: menu_height.saturating_sub(FOOTER_HEIGHT).saturating_add(6),
            width: MENU_WIDTH - 16,
            height: MENU_ROW_HEIGHT,
        },
        label: "Shut Down Machine".to_owned(),
        action: ShellAction::ShutdownMachine,
    });
    targets
}

pub fn desktop_targets() -> Vec<HitTarget> {
    [
        (
            Rect {
                x: 18,
                y: 20,
                width: 88,
                height: 92,
            },
            "Files",
            ShellAction::OpenFiles,
        ),
        (
            Rect {
                x: 18,
                y: 120,
                width: 88,
                height: 92,
            },
            "Shared",
            ShellAction::OpenShared,
        ),
    ]
    .into_iter()
    .map(|(rect, label, action)| HitTarget {
        rect,
        label: label.to_owned(),
        action,
    })
    .collect()
}

pub fn scan_applications() -> Result<Vec<Application>> {
    let mut directories = vec![PathBuf::from("/usr/local/share/applications")];
    directories.push(PathBuf::from("/usr/share/applications"));
    if let Some(home) = std::env::var_os("HOME") {
        directories.insert(0, PathBuf::from(home).join(".local/share/applications"));
    }
    scan_application_directories(&directories)
}

pub fn scan_application_directories(directories: &[PathBuf]) -> Result<Vec<Application>> {
    let mut by_id = BTreeMap::new();
    let mut claimed_ids = HashSet::new();
    for directory in directories {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries {
            let entry = entry.with_context(|| {
                format!("reading application directory {}", directory.display())
            })?;
            let path = entry.path();
            if path.extension() != Some(OsStr::new("desktop")) {
                continue;
            }
            let id = path
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or_default()
                .to_owned();
            // FreeDesktop data-directory precedence applies to the desktop-file
            // ID itself, including hidden, NoDisplay, and otherwise unusable
            // entries. A lower-priority copy must never make an application
            // reappear after a higher-priority override suppressed it.
            if !claimed_ids.insert(id.clone()) {
                continue;
            }
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(_) => continue,
            };
            match parse_desktop_entry(&id, &path, &contents) {
                DesktopParse::Visible(application) => {
                    by_id.insert(id, application);
                }
                DesktopParse::Hidden | DesktopParse::Ignore => {}
            }
        }
    }
    let mut applications: Vec<_> = by_id.into_values().collect();
    applications.sort_by_key(|application| application.name.to_lowercase());
    Ok(applications)
}

enum DesktopParse {
    Visible(Application),
    Hidden,
    Ignore,
}

fn parse_desktop_entry(id: &str, source: &Path, contents: &str) -> DesktopParse {
    let mut in_entry = false;
    let mut values = BTreeMap::<&str, &str>::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            in_entry = section == "Desktop Entry";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            values.entry(key.trim()).or_insert(value.trim());
        }
    }
    if values.get("Hidden") == Some(&"true") {
        return DesktopParse::Hidden;
    }
    if values.get("NoDisplay") == Some(&"true")
        || values
            .get("Type")
            .is_some_and(|value| *value != "Application")
    {
        return DesktopParse::Ignore;
    }
    let Some(name) = values.get("Name").filter(|value| !value.is_empty()) else {
        return DesktopParse::Ignore;
    };
    let Some(exec) = values.get("Exec") else {
        return DesktopParse::Ignore;
    };
    let mut command = desktop_exec_arguments(exec);
    if command.is_empty() {
        return DesktopParse::Ignore;
    }
    if values.get("Terminal") == Some(&"true") {
        command.insert(0, "-e".to_owned());
        command.insert(0, "foot".to_owned());
    }
    DesktopParse::Visible(Application {
        id: id.to_owned(),
        name: (*name).to_owned(),
        generic_name: values
            .get("GenericName")
            .filter(|value| !value.is_empty())
            .map(|value| (*value).to_owned()),
        command,
        icon: values
            .get("Icon")
            .filter(|value| !value.is_empty())
            .map(|value| (*value).to_owned()),
        categories: values
            .get("Categories")
            .map(|value| {
                value
                    .split(';')
                    .filter(|category| !category.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        source: source.to_path_buf(),
    })
}

fn desktop_exec_arguments(exec: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut field_code = false;
    for character in exec.chars() {
        if field_code {
            if character == '%' {
                current.push('%');
            }
            field_code = false;
        } else if escaped {
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character.is_whitespace() && !quoted {
            if !current.is_empty() {
                arguments.push(std::mem::take(&mut current));
            }
        } else if character == '%' {
            field_code = true;
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() {
        arguments.push(current);
    }
    arguments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> Application {
        Application {
            id: format!("{name}.desktop"),
            name: name.to_owned(),
            generic_name: None,
            command: vec![name.to_lowercase()],
            icon: None,
            categories: Vec::new(),
            source: PathBuf::new(),
        }
    }

    #[test]
    fn installed_desktop_entries_appear_and_hidden_overrides_are_respected() {
        let temp = tempfile::tempdir().unwrap();
        let system = temp.path().join("system");
        let user = temp.path().join("user");
        fs::create_dir(&system).unwrap();
        fs::create_dir(&user).unwrap();
        fs::write(
            system.join("browser.desktop"),
            "[Desktop Entry]\nType=Application\nName=Browser\nExec=browser %U\n",
        )
        .unwrap();
        fs::write(
            user.join("browser.desktop"),
            "[Desktop Entry]\nType=Application\nName=Private Browser\nExec=private-browser\n",
        )
        .unwrap();
        let applications = scan_application_directories(&[user, system]).unwrap();
        assert_eq!(applications.len(), 1);
        assert_eq!(applications[0].name, "Private Browser");
    }

    #[test]
    fn hidden_and_no_display_entries_shadow_lower_priority_launchers() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("local");
        let system = temp.path().join("system");
        fs::create_dir(&local).unwrap();
        fs::create_dir(&system).unwrap();
        for id in ["helper.desktop", "private.desktop"] {
            fs::write(
                system.join(id),
                format!("[Desktop Entry]\nType=Application\nName={id}\nExec={id}\n"),
            )
            .unwrap();
        }
        fs::write(
            local.join("helper.desktop"),
            "[Desktop Entry]\nType=Application\nName=Helper\nHidden=true\n",
        )
        .unwrap();
        fs::write(
            local.join("private.desktop"),
            "[Desktop Entry]\nType=Application\nName=Private\nNoDisplay=true\n",
        )
        .unwrap();

        let applications = scan_application_directories(&[local, system]).unwrap();
        assert!(applications.is_empty());
    }

    #[test]
    fn taskbar_has_one_simple_button_per_visible_window() {
        let windows = vec![
            GuestWindow {
                id: 10,
                title: "Browser".into(),
                ..GuestWindow::default()
            },
            GuestWindow {
                id: 20,
                title: "Editor".into(),
                ..GuestWindow::default()
            },
        ];
        let targets = panel_targets(1280, &windows, 0);
        let actions: Vec<_> = targets
            .iter()
            .filter_map(|target| match target.action {
                ShellAction::ActivateWindow(id) => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(actions, [10, 20]);
        assert!(!targets.iter().any(|target| matches!(
            target.action,
            ShellAction::OpenFiles | ShellAction::OpenShared
        )));
        assert!(
            !targets
                .iter()
                .any(|target| target.label.contains("Minimize")
                    || target.label.contains("Maximize")
                    || target.label.contains("Close"))
        );
    }

    #[test]
    fn classic_menu_keeps_shutdown_separate_from_host_chrome() {
        let targets = menu_targets(560, &[app("Browser")], 0);
        assert!(
            targets
                .iter()
                .any(|target| target.action == ShellAction::ShutdownMachine
                    && target.label == "Shut Down Machine")
        );
        assert_eq!(
            targets
                .iter()
                .filter(|target| matches!(target.action, ShellAction::LaunchApplication(_)))
                .count(),
            1
        );
        assert!(!targets.iter().any(|target| {
            matches!(
                target.action,
                ShellAction::OpenFiles | ShellAction::OpenShared
            )
        }));
    }

    #[test]
    fn desktop_has_files_and_shared_shortcuts() {
        let labels: Vec<_> = desktop_targets()
            .into_iter()
            .map(|target| target.label)
            .collect();
        assert_eq!(labels, ["Files", "Shared"]);
    }

    #[test]
    fn desktop_exec_field_codes_are_not_passed_as_fake_arguments() {
        assert_eq!(
            desktop_exec_arguments("dolphin %u --title \"My Files\" %%"),
            ["dolphin", "--title", "My Files", "%"]
        );
    }
}
