// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::Result;
use buzzardos_desktop_core::{DesktopItemKind, XdgPaths, discover_applications};
#[cfg(test)]
use std::fs;
use std::path::PathBuf;

pub const PANEL_HEIGHT: i32 = 42;
pub const TOP_BAR_HEIGHT: i32 = 28;
pub const APPLICATIONS_BUTTON_WIDTH: i32 = 126;
pub const SHOW_DESKTOP_WIDTH: i32 = 18;
pub const APPLICATIONS_MENU_HEADER_HEIGHT: i32 = 54;
pub const APPLICATIONS_MENU_SECTION_HEIGHT: i32 = 26;
pub const APPLICATIONS_MENU_FOOTER_HEIGHT: i32 = 50;
pub const MENU_ROW_HEIGHT: i32 = 36;
pub const CAPPED_TASK_BUTTON_WIDTH: i32 = 260;
pub const MIN_TASK_BUTTON_WIDTH: i32 = 96;
pub const TASK_PAGE_STEP: usize = 5;
const TASK_PAGE_BUTTON_WIDTH: i32 = 28;
const WORKSPACE_ADD_WIDTH: i32 = 34;
const WORKSPACE_TAB_MAX_WIDTH: i32 = 150;
pub const MANUAL_WORKSPACE_FLAG: u32 = 1 << 31;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceTab {
    pub index: u32,
    pub label: String,
    pub active: bool,
}

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
    pub workspace_index: Option<u32>,
    pub workspace: String,
    pub output: String,
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
    ExtractApplication(String),
    ExtractApplicationNoSandbox(String),
    PinApplication(String),
    UnpinApplication(String),
    RenameApplication(String),
    DeleteApplication(String),
    DesktopOpenSelection,
    DesktopCut,
    DesktopCopy,
    DesktopPaste,
    DesktopRename,
    DesktopDelete,
    DesktopNewFolder,
    DesktopArrangeIcons,
    DesktopAddToApplications,
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
    MoveWindowToWorkspace {
        window_id: u32,
        workspace_index: u32,
    },
    SwitchWorkspace(u32),
    CreateWorkspace,
    CloseWorkspace(u32),
    TaskbarPrevious,
    TaskbarNext,
    ShowDesktop,
    CloseApplicationsMenu,
}

pub fn workspace_name(index: u32) -> String {
    match index {
        0 => "Desktop".to_owned(),
        1 => "CUA".to_owned(),
        other if other & MANUAL_WORKSPACE_FLAG != 0 => {
            format!("Workspace{}", other & !MANUAL_WORKSPACE_FLAG)
        }
        other => format!("CUA{other}"),
    }
}

pub fn workspace_index(name: &str) -> Option<u32> {
    match name {
        "Desktop" => Some(0),
        "CUA" => Some(1),
        _ => name
            .strip_prefix("CUA")
            .filter(|suffix| !suffix.is_empty() && !suffix.starts_with('0'))
            .and_then(|suffix| suffix.parse::<u32>().ok())
            .filter(|index| *index >= 2 && *index < MANUAL_WORKSPACE_FLAG)
            .or_else(|| {
                name.strip_prefix("Workspace")
                    .filter(|suffix| !suffix.is_empty() && !suffix.starts_with('0'))
                    .and_then(|suffix| suffix.parse::<u32>().ok())
                    .filter(|index| *index > 0 && *index < MANUAL_WORKSPACE_FLAG)
                    .map(|index| MANUAL_WORKSPACE_FLAG | index)
            }),
    }
}

pub fn is_cua_workspace(index: u32) -> bool {
    index > 0 && index & MANUAL_WORKSPACE_FLAG == 0
}

pub fn next_manual_workspace(workspaces: &[WorkspaceTab]) -> u32 {
    let next = workspaces
        .iter()
        .filter_map(|workspace| {
            (workspace.index & MANUAL_WORKSPACE_FLAG != 0)
                .then_some(workspace.index & !MANUAL_WORKSPACE_FLAG)
        })
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1);
    MANUAL_WORKSPACE_FLAG | next
}

pub fn top_bar_targets(width: u32, workspaces: &[WorkspaceTab]) -> Vec<HitTarget> {
    let width = i32::try_from(width).unwrap_or(i32::MAX).max(1);
    let add_width = WORKSPACE_ADD_WIDTH.min(width);
    let available = width.saturating_sub(add_width);
    let tab_width = if workspaces.is_empty() {
        0
    } else {
        (available / i32::try_from(workspaces.len()).unwrap_or(1)).clamp(1, WORKSPACE_TAB_MAX_WIDTH)
    };
    let mut x = 0;
    let mut targets = Vec::with_capacity(workspaces.len() + 1);
    for workspace in workspaces {
        targets.push(HitTarget {
            rect: Rect {
                x,
                y: 0,
                width: tab_width,
                height: TOP_BAR_HEIGHT,
            },
            label: workspace.label.clone(),
            action: ShellAction::SwitchWorkspace(workspace.index),
        });
        x = x.saturating_add(tab_width);
    }
    targets.push(HitTarget {
        rect: Rect {
            x: x.min(width.saturating_sub(add_width)),
            y: 0,
            width: add_width,
            height: TOP_BAR_HEIGHT,
        },
        label: "Create CUA workspace".to_owned(),
        action: ShellAction::CreateWorkspace,
    });
    targets
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitTarget {
    pub rect: Rect,
    pub label: String,
    pub action: ShellAction,
}

pub fn panel_targets(
    width: u32,
    windows: &[GuestWindow],
    offset: usize,
    capped_task_buttons: bool,
) -> Vec<HitTarget> {
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
    let needs_paging = capped_task_buttons
        && i32::try_from(windows.len())
            .unwrap_or(i32::MAX)
            .saturating_mul(MIN_TASK_BUTTON_WIDTH)
            > available;
    let task_space = available.saturating_sub(if needs_paging {
        TASK_PAGE_BUTTON_WIDTH * 2
    } else {
        0
    });
    let slots = if needs_paging {
        usize::try_from((task_space / MIN_TASK_BUTTON_WIDTH).max(1)).unwrap_or(1)
    } else {
        windows.len()
    };
    let maximum_offset = windows.len().saturating_sub(slots);
    let start = if needs_paging {
        offset.min(maximum_offset)
    } else {
        0
    };
    let end = start.saturating_add(slots).min(windows.len());
    let visible = &windows[start..end];

    if needs_paging {
        targets.push(HitTarget {
            rect: Rect {
                x,
                y: 0,
                width: TASK_PAGE_BUTTON_WIDTH,
                height: PANEL_HEIGHT,
            },
            label: "Previous running applications".to_owned(),
            action: ShellAction::TaskbarPrevious,
        });
        x += TASK_PAGE_BUTTON_WIDTH;
        targets.push(HitTarget {
            rect: Rect {
                x,
                y: 0,
                width: TASK_PAGE_BUTTON_WIDTH,
                height: PANEL_HEIGHT,
            },
            label: "Next running applications".to_owned(),
            action: ShellAction::TaskbarNext,
        });
        x += TASK_PAGE_BUTTON_WIDTH;
    }

    let task_width = if visible.is_empty() {
        0
    } else {
        let equal_width = task_space / i32::try_from(visible.len()).unwrap_or(1);
        if capped_task_buttons {
            equal_width.clamp(MIN_TASK_BUTTON_WIDTH, CAPPED_TASK_BUTTON_WIDTH)
        } else {
            equal_width
        }
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

pub fn taskbar_max_offset(width: u32, window_count: usize, capped_task_buttons: bool) -> usize {
    if !capped_task_buttons {
        return 0;
    }
    let width = i32::try_from(width).unwrap_or(i32::MAX).max(1);
    let available = width
        .saturating_sub(SHOW_DESKTOP_WIDTH.min(width))
        .saturating_sub(APPLICATIONS_BUTTON_WIDTH.min(width))
        .saturating_sub(TASK_PAGE_BUTTON_WIDTH * 2)
        .max(MIN_TASK_BUTTON_WIDTH);
    let slots = usize::try_from((available / MIN_TASK_BUTTON_WIDTH).max(1)).unwrap_or(1);
    window_count.saturating_sub(slots)
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

pub fn window_menu_targets(window: &GuestWindow, workspaces: &[WorkspaceTab]) -> Vec<HitTarget> {
    const HEADER_HEIGHT: i32 = 44;
    const MENU_WIDTH: i32 = 260;
    let mut entries: Vec<(String, ShellAction)> = vec![
        ("Focus".to_owned(), ShellAction::ActivateWindow(window.id)),
        (
            "Bring Into View".to_owned(),
            ShellAction::BringIntoViewWindow(window.id),
        ),
        (
            "Minimize".to_owned(),
            ShellAction::MinimizeWindow(window.id),
        ),
        (
            if window.minimized || window.maximized {
                "Restore".to_owned()
            } else {
                "Maximize".to_owned()
            },
            ShellAction::ToggleMaximizeWindow(window.id),
        ),
        ("Close".to_owned(), ShellAction::CloseWindow(window.id)),
    ];
    entries.extend(
        workspaces
            .iter()
            .filter(|workspace| window.workspace_index != Some(workspace.index))
            .map(|workspace| {
                (
                    format!("Move to {}", workspace.label),
                    ShellAction::MoveWindowToWorkspace {
                        window_id: window.id,
                        workspace_index: workspace.index,
                    },
                )
            }),
    );
    entries
        .into_iter()
        .enumerate()
        .map(|(index, (label, action))| HitTarget {
            rect: Rect {
                x: 8,
                y: HEADER_HEIGHT + i32::try_from(index).unwrap_or_default() * MENU_ROW_HEIGHT,
                width: MENU_WIDTH - 16,
                height: MENU_ROW_HEIGHT,
            },
            label,
            action,
        })
        .collect()
}

pub fn window_menu_height(window: &GuestWindow, workspaces: &[WorkspaceTab]) -> u32 {
    let rows = window_menu_targets(window, workspaces).len();
    44_u32.saturating_add(
        u32::try_from(rows)
            .unwrap_or(u32::MAX)
            .saturating_mul(MENU_ROW_HEIGHT as u32),
    )
}

pub fn workspace_menu_targets(index: u32) -> Vec<HitTarget> {
    if index == 0 {
        return Vec::new();
    }
    vec![HitTarget {
        rect: Rect {
            x: 8,
            y: 44,
            width: 244,
            height: MENU_ROW_HEIGHT,
        },
        label: format!("Close {}", workspace_name(index)),
        action: ShellAction::CloseWorkspace(index),
    }]
}

pub fn application_context_targets(
    application: &Application,
    pinned: bool,
    managed_appimage: bool,
) -> Vec<HitTarget> {
    const CONTEXT_WIDTH: i32 = 252;
    let mut entries = vec![(
        "Open",
        ShellAction::LaunchApplication(application.id.clone()),
    )];
    if managed_appimage {
        entries.extend([
            (
                "Extract and Run",
                ShellAction::ExtractApplication(application.id.clone()),
            ),
            (
                "Extract and Run --no-sandbox",
                ShellAction::ExtractApplicationNoSandbox(application.id.clone()),
            ),
        ]);
    }
    entries.push(if pinned {
        (
            "Unpin",
            ShellAction::UnpinApplication(application.id.clone()),
        )
    } else {
        ("Pin", ShellAction::PinApplication(application.id.clone()))
    });
    entries.push((
        "Add to Desktop",
        ShellAction::AddApplicationDesktopShortcut(application.id.clone()),
    ));
    if managed_appimage {
        entries.extend([
            (
                "Rename…",
                ShellAction::RenameApplication(application.id.clone()),
            ),
            (
                "Delete from Applications",
                ShellAction::DeleteApplication(application.id.clone()),
            ),
        ]);
    }
    entries
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
    fn cua_and_manual_workspace_identities_do_not_overlap() {
        assert_eq!(workspace_index("Desktop"), Some(0));
        assert_eq!(workspace_index("CUA"), Some(1));
        assert_eq!(workspace_index("CUA19"), Some(19));
        assert_eq!(
            workspace_index("Workspace1"),
            Some(MANUAL_WORKSPACE_FLAG | 1)
        );
        assert_eq!(workspace_name(MANUAL_WORKSPACE_FLAG | 7), "Workspace7");
        assert!(is_cua_workspace(1));
        assert!(!is_cua_workspace(MANUAL_WORKSPACE_FLAG | 1));
    }

    #[test]
    fn plus_allocates_only_the_next_manual_workspace() {
        let workspaces = vec![
            WorkspaceTab {
                index: 0,
                label: "Desktop".into(),
                active: true,
            },
            WorkspaceTab {
                index: 8,
                label: "CUA8".into(),
                active: false,
            },
            WorkspaceTab {
                index: MANUAL_WORKSPACE_FLAG | 2,
                label: "Workspace2".into(),
                active: false,
            },
        ];
        assert_eq!(
            next_manual_workspace(&workspaces),
            MANUAL_WORKSPACE_FLAG | 3
        );
    }

    #[test]
    fn top_bar_keeps_selectors_and_plus_adjacent() {
        let workspaces = vec![
            WorkspaceTab {
                index: 0,
                label: "Desktop".into(),
                active: true,
            },
            WorkspaceTab {
                index: 1,
                label: "CUA".into(),
                active: false,
            },
        ];
        let targets = top_bar_targets(640, &workspaces);
        assert_eq!(targets.len(), 3);
        assert!(matches!(targets[0].action, ShellAction::SwitchWorkspace(0)));
        assert!(matches!(targets[1].action, ShellAction::SwitchWorkspace(1)));
        assert_eq!(targets[2].action, ShellAction::CreateWorkspace);
        assert_eq!(targets[1].rect.x + targets[1].rect.width, targets[2].rect.x);
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
    fn application_context_matches_gnozzard_menu_ownership() {
        let application = app("Fixture");
        let managed = application_context_targets(&application, false, true)
            .into_iter()
            .map(|target| target.label)
            .collect::<Vec<_>>();
        assert_eq!(
            managed,
            [
                "Open",
                "Extract and Run",
                "Extract and Run --no-sandbox",
                "Pin",
                "Add to Desktop",
                "Rename…",
                "Delete from Applications",
            ]
        );

        let ordinary = application_context_targets(&application, true, false)
            .into_iter()
            .map(|target| target.label)
            .collect::<Vec<_>>();
        assert_eq!(ordinary, ["Open", "Unpin", "Add to Desktop"]);
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
        let targets = panel_targets(1280, &windows, 0, true);
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
    fn capped_taskbar_pages_by_five_only_after_minimum_width_is_exhausted() {
        let windows = (0..10)
            .map(|index| GuestWindow {
                id: index + 1,
                title: format!("Window {}", index + 1),
                ..GuestWindow::default()
            })
            .collect::<Vec<_>>();
        let first = panel_targets(640, &windows, 0, true);
        assert!(
            first
                .iter()
                .any(|target| target.action == ShellAction::TaskbarPrevious)
        );
        assert!(
            first
                .iter()
                .any(|target| target.action == ShellAction::TaskbarNext)
        );
        let first_ids = first
            .iter()
            .filter_map(|target| match target.action {
                ShellAction::ActivateWindow(id) => Some(id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let second_ids = panel_targets(640, &windows, TASK_PAGE_STEP, true)
            .into_iter()
            .filter_map(|target| match target.action {
                ShellAction::ActivateWindow(id) => Some(id),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(first_ids, [1, 2, 3, 4]);
        assert_eq!(second_ids, [6, 7, 8, 9]);
        let applications = &first[0];
        let previous = first
            .iter()
            .find(|target| target.action == ShellAction::TaskbarPrevious)
            .expect("previous-page target");
        let next = first
            .iter()
            .find(|target| target.action == ShellAction::TaskbarNext)
            .expect("next-page target");
        assert_eq!(
            applications.rect.x + applications.rect.width,
            previous.rect.x
        );
        assert_eq!(previous.rect.x + previous.rect.width, next.rect.x);
        assert!(first_ids.iter().all(|id| {
            first
                .iter()
                .find(|target| target.action == ShellAction::ActivateWindow(*id))
                .is_some_and(|target| target.rect.x >= next.rect.x + next.rect.width)
        }));

        let fitting = panel_targets(640, &windows[..5], 0, true);
        assert!(!fitting.iter().any(|target| matches!(
            target.action,
            ShellAction::TaskbarPrevious | ShellAction::TaskbarNext
        )));
        for pair in fitting.windows(2) {
            if !matches!(pair[1].action, ShellAction::ShowDesktop) {
                assert_eq!(pair[0].rect.x + pair[0].rect.width, pair[1].rect.x);
            }
        }
    }

    #[test]
    fn classic_menu_contains_applications_but_no_host_lifecycle_actions() {
        let targets = menu_targets(300, 208, &[app("Browser")], 0);
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
        assert!(targets.is_empty());
    }

    #[test]
    fn task_context_menu_exposes_complete_window_controls() {
        let window = GuestWindow {
            id: 42,
            title: "Editor".into(),
            maximized: false,
            ..GuestWindow::default()
        };
        let targets = window_menu_targets(&window, &[]);
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
        assert_eq!(window_menu_targets(&maximized, &[])[3].label, "Restore");
        let minimized = GuestWindow {
            minimized: true,
            maximized: false,
            ..maximized
        };
        assert_eq!(window_menu_targets(&minimized, &[])[3].label, "Restore");
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
