// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::Result;
use buzzardos_desktop_core::{DesktopItemKind, XdgPaths, discover_applications};
#[cfg(test)]
use std::fs;
use std::path::PathBuf;

pub const PANEL_HEIGHT: i32 = 42;
pub const APPLICATIONS_BUTTON_WIDTH: i32 = 126;
pub const SHOW_DESKTOP_WIDTH: i32 = 18;
pub const APPLICATIONS_MENU_HEADER_HEIGHT: i32 = 54;
pub const APPLICATIONS_MENU_SECTION_HEIGHT: i32 = 26;
pub const APPLICATIONS_MENU_FOOTER_HEIGHT: i32 = 50;
pub const MENU_ROW_HEIGHT: i32 = 36;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Application {
    pub id: String,
    pub name: String,
    pub generic_name: Option<String>,
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
    OpenDesktopItem(PathBuf, DesktopItemKind),
    LaunchApplication(String),
    AddApplicationDesktopShortcut(String),
    RemoveApplicationDesktopShortcut(String),
    DesktopOpenSelection,
    DesktopCut,
    DesktopCopy,
    DesktopPaste,
    DesktopRename,
    DesktopDelete,
    DesktopNewFolder,
    DesktopArrangeIcons,
    DesktopAddToApplications,
    DesktopRemoveFromApplications,
    DesktopEditConfirm,
    DesktopDeleteConfirm,
    DesktopCollisionReplace,
    DesktopCollisionKeepBoth,
    DesktopCollisionCancel,
    DismissContext,
    ActivateWindow(u32),
    BringIntoViewWindow(u32),
    MinimizeWindow(u32),
    ToggleMaximizeWindow(u32),
    CloseWindow(u32),
    TaskbarPrevious,
    TaskbarNext,
    ShowDesktop,
    CloseApplicationsMenu,
    ShutdownMachine,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitTarget {
    pub rect: Rect,
    pub label: String,
    pub action: ShellAction,
}

pub fn panel_targets(width: u32, windows: &[GuestWindow], page: usize) -> Vec<HitTarget> {
    const NAV_WIDTH: i32 = 24;
    const MIN_TASK_WIDTH: i32 = 148;

    let width = i32::try_from(width).unwrap_or(i32::MAX).max(1);
    let show_desktop_width = SHOW_DESKTOP_WIDTH.min(width);
    let taskbar_right = width.saturating_sub(show_desktop_width);
    let applications_width = APPLICATIONS_BUTTON_WIDTH.min(taskbar_right);
    let mut x = 0;
    let mut targets = vec![HitTarget {
        rect: Rect {
            x,
            y: 0,
            width: applications_width,
            height: PANEL_HEIGHT,
        },
        label: "Applications".to_owned(),
        action: ShellAction::ToggleApplications,
    }];
    x += applications_width;

    let right_edge = taskbar_right.max(x);
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
    targets.push(HitTarget {
        rect: Rect {
            x: taskbar_right,
            y: 0,
            width: show_desktop_width,
            height: PANEL_HEIGHT,
        },
        label: "Show desktop".to_owned(),
        action: ShellAction::ShowDesktop,
    });
    targets
}

pub fn menu_targets(
    menu_width: u32,
    menu_height: u32,
    applications: &[Application],
    scroll: usize,
) -> Vec<HitTarget> {
    let menu_width = i32::try_from(menu_width).unwrap_or(i32::MAX);
    let menu_height = i32::try_from(menu_height).unwrap_or(i32::MAX);
    let mut targets = Vec::new();
    let mut y = APPLICATIONS_MENU_HEADER_HEIGHT + APPLICATIONS_MENU_SECTION_HEIGHT;

    let available = menu_height
        .saturating_sub(y)
        .saturating_sub(APPLICATIONS_MENU_FOOTER_HEIGHT)
        .max(0);
    let visible_rows = usize::try_from(available / MENU_ROW_HEIGHT).unwrap_or_default();
    for application in applications.iter().skip(scroll).take(visible_rows) {
        targets.push(HitTarget {
            rect: Rect {
                x: 8,
                y,
                width: menu_width.saturating_sub(16),
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
            y: menu_height
                .saturating_sub(APPLICATIONS_MENU_FOOTER_HEIGHT)
                .saturating_add(6)
                .max(0),
            width: menu_width.saturating_sub(16),
            height: MENU_ROW_HEIGHT,
        },
        label: "Shut Down Machine".to_owned(),
        action: ShellAction::ShutdownMachine,
    });
    targets
}

pub fn applications_menu_close_target(menu_width: u32) -> HitTarget {
    const CLOSE_WIDTH: i32 = 46;
    let menu_width = i32::try_from(menu_width).unwrap_or(i32::MAX);
    HitTarget {
        rect: Rect {
            x: menu_width.saturating_sub(CLOSE_WIDTH),
            y: 0,
            width: CLOSE_WIDTH.min(menu_width),
            height: APPLICATIONS_MENU_HEADER_HEIGHT,
        },
        label: "Close Applications menu".to_owned(),
        action: ShellAction::CloseApplicationsMenu,
    }
}

pub fn window_menu_targets(window: &GuestWindow) -> Vec<HitTarget> {
    const HEADER_HEIGHT: i32 = 44;
    const MENU_WIDTH: i32 = 260;
    [
        ("Focus", ShellAction::ActivateWindow(window.id)),
        (
            "Bring Into View",
            ShellAction::BringIntoViewWindow(window.id),
        ),
        ("Minimize", ShellAction::MinimizeWindow(window.id)),
        (
            if window.minimized || window.maximized {
                "Restore"
            } else {
                "Maximize"
            },
            ShellAction::ToggleMaximizeWindow(window.id),
        ),
        ("Close", ShellAction::CloseWindow(window.id)),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (label, action))| HitTarget {
        rect: Rect {
            x: 8,
            y: HEADER_HEIGHT + i32::try_from(index).unwrap_or_default() * MENU_ROW_HEIGHT,
            width: MENU_WIDTH - 16,
            height: MENU_ROW_HEIGHT,
        },
        label: label.to_owned(),
        action,
    })
    .collect()
}

pub fn application_context_targets(
    application: &Application,
    shortcut_exists: bool,
) -> Vec<HitTarget> {
    const CONTEXT_WIDTH: i32 = 252;
    [
        (
            "Open",
            ShellAction::LaunchApplication(application.id.clone()),
        ),
        if shortcut_exists {
            (
                "Remove Desktop Shortcut",
                ShellAction::RemoveApplicationDesktopShortcut(application.id.clone()),
            )
        } else {
            (
                "Add Desktop Shortcut",
                ShellAction::AddApplicationDesktopShortcut(application.id.clone()),
            )
        },
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (label, action))| HitTarget {
        rect: Rect {
            x: 6,
            y: 6 + i32::try_from(index).unwrap_or_default() * MENU_ROW_HEIGHT,
            width: CONTEXT_WIDTH - 12,
            height: MENU_ROW_HEIGHT,
        },
        label: label.to_owned(),
        action,
    })
    .collect()
}

pub fn builtin_desktop_targets() -> Vec<HitTarget> {
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
    let paths = XdgPaths::discover()?;
    Ok(adapt_catalog(discover_applications(&paths)))
}

#[cfg(test)]
pub fn scan_application_directories(directories: &[PathBuf]) -> Result<Vec<Application>> {
    Ok(adapt_catalog(
        buzzardos_desktop_core::desktop_entry::discover_application_directories(
            directories,
            &["sway".to_owned()],
        ),
    ))
}

fn adapt_catalog(catalog: buzzardos_desktop_core::ApplicationCatalog) -> Vec<Application> {
    for diagnostic in catalog.diagnostics {
        eprintln!(
            "buzzardos-shell: ignored desktop entry {}: {}",
            diagnostic.path.display(),
            diagnostic.message
        );
    }
    catalog
        .applications
        .into_iter()
        .map(|application| Application {
            id: application.id.as_str().to_owned(),
            name: application.name,
            generic_name: application.generic_name,
            icon: application.icon,
            categories: application.categories,
            source: application.source,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str) -> Application {
        Application {
            id: format!("{name}.desktop"),
            name: name.to_owned(),
            generic_name: None,
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
            "[Desktop Entry]\nVersion=1.0\nType=Application\nName=Browser\nExec=/usr/bin/true %U\n",
        )
        .unwrap();
        fs::write(
            user.join("browser.desktop"),
            "[Desktop Entry]\nVersion=1.0\nType=Application\nName=Private Browser\nExec=/usr/bin/true\n",
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
        let show_desktop = targets.last().expect("show-desktop target");
        assert_eq!(show_desktop.action, ShellAction::ShowDesktop);
        assert_eq!(show_desktop.rect.x + show_desktop.rect.width, 1280);
    }

    #[test]
    fn classic_menu_keeps_shutdown_separate_from_host_chrome() {
        let targets = menu_targets(300, 166, &[app("Browser")], 0);
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
    fn applications_menu_close_target_tracks_the_responsive_width() {
        let target = applications_menu_close_target(287);
        assert_eq!(target.action, ShellAction::CloseApplicationsMenu);
        assert_eq!(target.rect.x + target.rect.width, 287);
    }

    #[test]
    fn applications_menu_can_have_no_visual_rows_on_an_extremely_short_output() {
        let targets = menu_targets(220, 120, &[app("Browser")], 0);
        assert!(
            targets
                .iter()
                .all(|target| !matches!(target.action, ShellAction::LaunchApplication(_)))
        );
        assert!(
            targets
                .iter()
                .any(|target| target.action == ShellAction::ShutdownMachine)
        );
    }

    #[test]
    fn task_context_menu_exposes_complete_window_controls() {
        let window = GuestWindow {
            id: 42,
            title: "Editor".into(),
            maximized: false,
            ..GuestWindow::default()
        };
        let targets = window_menu_targets(&window);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.label.as_str())
                .collect::<Vec<_>>(),
            ["Focus", "Bring Into View", "Minimize", "Maximize", "Close",]
        );
        let maximized = GuestWindow {
            maximized: true,
            ..window
        };
        assert_eq!(window_menu_targets(&maximized)[3].label, "Restore");
        let minimized = GuestWindow {
            minimized: true,
            maximized: false,
            ..maximized
        };
        assert_eq!(window_menu_targets(&minimized)[3].label, "Restore");
    }

    #[test]
    fn desktop_has_files_and_shared_shortcuts() {
        let labels: Vec<_> = builtin_desktop_targets()
            .into_iter()
            .map(|target| target.label)
            .collect();
        assert_eq!(labels, ["Files", "Shared"]);
    }
}
