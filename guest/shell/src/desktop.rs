// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use gio::prelude::AppInfoExt;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use wildbuzzard_desktop_core::{
    DesktopDirectory, DesktopItem, DesktopLayout, DesktopPosition, XdgPaths,
};

use crate::model::Rect;

pub const ICON_CELL_WIDTH: i32 = 96;
pub const ICON_CELL_HEIGHT: i32 = 100;
pub const ICON_LEFT: i32 = 14;
pub const ICON_TOP: i32 = 16;

#[derive(Debug, Clone)]
pub struct PositionedDesktopItem {
    pub item: DesktopItem,
    pub rect: Rect,
    pub page: u32,
}

#[derive(Debug)]
pub struct DesktopModel {
    directory: DesktopDirectory,
    layout_path: PathBuf,
    layout: DesktopLayout,
    layout_writable: bool,
    items: Vec<DesktopItem>,
    page: u32,
}

impl DesktopModel {
    pub fn discover() -> Result<Self> {
        let paths = XdgPaths::discover().context("discovering XDG Desktop")?;
        Self::open(paths)
    }

    fn open(paths: XdgPaths) -> Result<Self> {
        let directory =
            DesktopDirectory::create_and_open(&paths.desktop_dir).context("opening XDG Desktop")?;
        let layout_path = paths.managed_state_dir().join("desktop-layout.json");
        let (layout, layout_writable) = if layout_path.exists() {
            match DesktopLayout::load(&layout_path) {
                Ok(layout) => (layout, true),
                Err(error) => {
                    // Preserve malformed and unknown-newer state exactly. The
                    // shell can still lay items out in memory, but must not
                    // replace a document it cannot interpret.
                    eprintln!(
                        "wildbuzzard-shell: desktop layout at {} was preserved and is read-only: {error}",
                        layout_path.display()
                    );
                    (DesktopLayout::default(), false)
                }
            }
        } else {
            (DesktopLayout::default(), true)
        };
        let mut model = Self {
            directory,
            layout_path,
            layout,
            layout_writable,
            items: Vec::new(),
            page: 0,
        };
        model.rescan()?;
        Ok(model)
    }

    pub fn directory_path(&self) -> &Path {
        self.directory.path()
    }

    pub fn page(&self) -> u32 {
        self.page
    }

    pub fn show_first_page(&mut self) {
        self.page = 0;
    }

    pub fn scroll_page(&mut self, amount: f64) -> bool {
        if amount == 0.0 {
            return false;
        }
        let maximum = self
            .layout
            .positions
            .values()
            .map(|position| position.page)
            .max()
            .unwrap_or_default();
        let next = if amount > 0.0 {
            self.page.saturating_add(1).min(maximum)
        } else {
            self.page.saturating_sub(1)
        };
        let changed = next != self.page;
        self.page = next;
        changed
    }

    pub fn rescan(&mut self) -> Result<bool> {
        let mut items = self.directory.list().context("listing XDG Desktop")?;
        for item in &mut items {
            if item.kind == wildbuzzard_desktop_core::DesktopItemKind::Launcher {
                item.display_name = launcher_display_name(item);
            }
        }
        items.sort_by(|left, right| {
            left.display_name
                .to_lowercase()
                .cmp(&right.display_name.to_lowercase())
                .then_with(|| left.name.cmp(&right.name))
        });
        let changed = items != self.items;
        self.items = items;
        self.layout.retain_items(&self.items);
        Ok(changed)
    }

    /// Persist a user drag in the same adaptive grid used for rendering.
    /// Dropping on another item swaps the two positions; dropping on either
    /// immutable built-in shortcut is ignored.
    pub fn move_item(
        &mut self,
        path: &Path,
        point: (f64, f64),
        desktop_size: (u32, u32),
    ) -> Result<bool> {
        if !self.layout_writable {
            bail!("desktop layout is read-only because its persisted schema is unusable");
        }
        self.positioned(desktop_size)?;
        let Some(key) = self
            .items
            .iter()
            .find(|item| item.path == path)
            .map(|item| item.identity.layout_key())
        else {
            return Ok(false);
        };
        let Some(previous) = self.layout.positions.get(&key).copied() else {
            return Ok(false);
        };
        let (columns, rows) = grid_extent(desktop_size);
        let column = ((point.0.floor() as i32).saturating_sub(ICON_LEFT) / ICON_CELL_WIDTH).clamp(
            0,
            i32::try_from(columns.saturating_sub(1)).unwrap_or(i32::MAX),
        ) as u32;
        let row = ((point.1.floor() as i32).saturating_sub(ICON_TOP) / ICON_CELL_HEIGHT)
            .clamp(0, i32::try_from(rows.saturating_sub(1)).unwrap_or(i32::MAX))
            as u32;
        let next = DesktopPosition {
            column,
            row,
            page: self.page,
        };
        if next == previous {
            return Ok(false);
        }
        let linear = column.saturating_mul(rows).saturating_add(row);
        if next.page == 0 && linear < 2 {
            return Ok(false);
        }
        if let Some(occupied_key) =
            self.layout
                .positions
                .iter()
                .find_map(|(candidate, position)| {
                    (candidate != &key && *position == next).then(|| candidate.clone())
                })
        {
            self.layout.positions.insert(occupied_key, previous);
        }
        self.layout.positions.insert(key, next);
        self.layout.generation = self.layout.generation.saturating_add(1);
        self.layout.save(&self.layout_path)?;
        Ok(true)
    }

    pub fn arrange_icons(&mut self) -> Result<()> {
        if !self.layout_writable {
            bail!("desktop layout is read-only because its persisted schema is unusable");
        }
        self.layout.positions.clear();
        self.layout.generation = self.layout.generation.saturating_add(1);
        self.layout.save(&self.layout_path)?;
        self.page = 0;
        Ok(())
    }

    pub fn positioned(&mut self, desktop_size: (u32, u32)) -> Result<Vec<PositionedDesktopItem>> {
        let (columns, rows) = grid_extent(desktop_size);
        let slots_per_page = columns.saturating_mul(rows).max(1);
        // Files and Shared are immutable shell shortcuts occupying the first
        // two cells of page zero.
        let mut occupied = (0..2.min(slots_per_page))
            .map(|slot| (0, slot / rows, slot % rows))
            .collect::<BTreeSet<_>>();
        let mut valid_layout_keys = BTreeSet::new();
        for (key, position) in &self.layout.positions {
            if position.column < columns
                && position.row < rows
                && occupied.insert((position.page, position.column, position.row))
            {
                valid_layout_keys.insert(key.clone());
            }
        }
        let mut layout_changed = false;
        for item in &self.items {
            let key = item.identity.layout_key();
            let valid = valid_layout_keys.contains(&key);
            if valid {
                continue;
            }
            let position = first_free_position(&occupied, columns, rows, slots_per_page);
            occupied.insert((position.page, position.column, position.row));
            self.layout.positions.insert(key, position);
            layout_changed = true;
        }
        if layout_changed && self.layout_writable {
            self.layout.generation = self.layout.generation.saturating_add(1);
            self.layout.save(&self.layout_path)?;
        }
        self.page = self.page.min(
            occupied
                .iter()
                .map(|(page, _, _)| *page)
                .max()
                .unwrap_or_default(),
        );
        let by_identity = self
            .items
            .iter()
            .map(|item| (item.identity.layout_key(), item.clone()))
            .collect::<BTreeMap<_, _>>();
        let positioned = self
            .layout
            .positions
            .iter()
            .filter_map(|(identity, position)| {
                let item = by_identity.get(identity)?.clone();
                Some(PositionedDesktopItem {
                    item,
                    rect: Rect {
                        x: ICON_LEFT
                            + i32::try_from(position.column).unwrap_or(i32::MAX) * ICON_CELL_WIDTH,
                        y: ICON_TOP
                            + i32::try_from(position.row).unwrap_or(i32::MAX) * ICON_CELL_HEIGHT,
                        width: ICON_CELL_WIDTH - 8,
                        height: ICON_CELL_HEIGHT - 8,
                    },
                    page: position.page,
                })
            })
            .collect();
        Ok(positioned)
    }
}

/// Desktop launchers keep their real `.desktop` filename on disk (and Thunar
/// continues to show that filename), while the desktop surface presents the
/// localized FreeDesktop `Name=` just like the Applications menu. A malformed
/// launcher still never leaks the implementation suffix into the visual label.
fn launcher_display_name(item: &DesktopItem) -> String {
    gio::DesktopAppInfo::from_filename(&item.path)
        .map(|info| info.display_name().to_string())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| {
            item.display_name
                .strip_suffix(".desktop")
                .unwrap_or(&item.display_name)
                .to_owned()
        })
}

fn grid_extent(desktop_size: (u32, u32)) -> (u32, u32) {
    let usable_width = i32::try_from(desktop_size.0)
        .unwrap_or(i32::MAX)
        .saturating_sub(ICON_LEFT * 2)
        .max(ICON_CELL_WIDTH);
    let usable_height = i32::try_from(desktop_size.1)
        .unwrap_or(i32::MAX)
        .saturating_sub(crate::model::PANEL_HEIGHT)
        .saturating_sub(ICON_TOP * 2)
        .max(ICON_CELL_HEIGHT);
    (
        u32::try_from(usable_width / ICON_CELL_WIDTH)
            .unwrap_or(1)
            .max(1),
        u32::try_from(usable_height / ICON_CELL_HEIGHT)
            .unwrap_or(1)
            .max(1),
    )
}

fn first_free_position(
    occupied: &BTreeSet<(u32, u32, u32)>,
    columns: u32,
    rows: u32,
    slots_per_page: u32,
) -> DesktopPosition {
    for linear in 0..u32::MAX {
        let page = linear / slots_per_page;
        let slot = linear % slots_per_page;
        let column = slot / rows;
        let row = slot % rows;
        if column < columns && !occupied.contains(&(page, column, row)) {
            return DesktopPosition { column, row, page };
        }
    }
    DesktopPosition {
        column: 0,
        row: 0,
        page: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn grid_is_adaptive_and_pages_instead_of_dropping_items() {
        assert_eq!(grid_extent((300, 300)), (2, 2));
        let occupied = (0..4)
            .map(|slot| (0, slot / 2, slot % 2))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            first_free_position(&occupied, 2, 2, 4),
            DesktopPosition {
                column: 0,
                row: 0,
                page: 1,
            }
        );
    }

    #[test]
    fn unsupported_layout_schema_is_preserved_during_in_memory_layout() {
        let temp = tempfile::tempdir().unwrap();
        let paths = XdgPaths::from_bases(
            temp.path().join("home"),
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("state"),
            vec![temp.path().join("share")],
            temp.path().join("Desktop"),
        )
        .unwrap();
        fs::create_dir_all(paths.managed_state_dir()).unwrap();
        fs::create_dir_all(&paths.desktop_dir).unwrap();
        fs::write(paths.desktop_dir.join("note.txt"), b"hello").unwrap();
        let future = b"{\"schema_version\":999,\"future\":true}\n";
        let layout_path = paths.managed_state_dir().join("desktop-layout.json");
        fs::write(&layout_path, future).unwrap();

        let mut model = DesktopModel::open(paths).unwrap();
        assert!(!model.layout_writable);
        assert_eq!(model.positioned((800, 600)).unwrap().len(), 1);
        assert_eq!(fs::read(layout_path).unwrap(), future);
    }

    #[test]
    fn desktop_pages_scroll_and_clamp_without_hiding_the_model() {
        let temp = tempfile::tempdir().unwrap();
        let paths = XdgPaths::from_bases(
            temp.path().join("home"),
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("state"),
            vec![temp.path().join("share")],
            temp.path().join("Desktop"),
        )
        .unwrap();
        fs::create_dir_all(paths.managed_state_dir()).unwrap();
        fs::create_dir_all(&paths.desktop_dir).unwrap();
        for index in 0..5 {
            fs::write(paths.desktop_dir.join(format!("item-{index}")), b"fixture").unwrap();
        }
        let mut model = DesktopModel::open(paths).unwrap();
        let all = model.positioned((300, 300)).unwrap();
        assert!(all.iter().any(|item| item.page > 0));
        assert!(model.scroll_page(1.0));
        assert_eq!(model.page(), 1);
        assert!(model.scroll_page(-1.0));
        assert_eq!(model.page(), 0);
        assert!(!model.scroll_page(-1.0));
    }

    #[test]
    fn new_shortcut_uses_the_first_visible_cell_after_files_and_shared() {
        let temp = tempfile::tempdir().unwrap();
        let paths = XdgPaths::from_bases(
            temp.path().join("home"),
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("state"),
            vec![temp.path().join("share")],
            temp.path().join("Desktop"),
        )
        .unwrap();
        fs::create_dir_all(paths.managed_state_dir()).unwrap();
        fs::create_dir_all(&paths.desktop_dir).unwrap();
        let shortcut = paths.desktop_dir.join("firefox-esr.desktop");
        fs::write(
            &shortcut,
            b"[Desktop Entry]\nType=Application\nName=Firefox ESR\nExec=/bin/true\n",
        )
        .unwrap();

        let mut model = DesktopModel::open(paths).unwrap();
        let item = model
            .positioned((800, 600))
            .unwrap()
            .into_iter()
            .find(|item| item.item.path == shortcut)
            .unwrap();
        assert_eq!(item.item.display_name, "Firefox ESR");
        assert!(!item.item.display_name.ends_with(".desktop"));
        assert_eq!(item.page, 0);
        assert_eq!(item.rect.x, ICON_LEFT);
        assert_eq!(item.rect.y, ICON_TOP + 2 * ICON_CELL_HEIGHT);

        model.page = 3;
        model.show_first_page();
        assert_eq!(model.page(), 0);
    }

    #[test]
    fn dragging_swaps_items_and_persists_the_layout() {
        let temp = tempfile::tempdir().unwrap();
        let paths = XdgPaths::from_bases(
            temp.path().join("home"),
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("state"),
            vec![temp.path().join("share")],
            temp.path().join("Desktop"),
        )
        .unwrap();
        fs::create_dir_all(paths.managed_state_dir()).unwrap();
        fs::create_dir_all(&paths.desktop_dir).unwrap();
        let alpha = paths.desktop_dir.join("alpha");
        let beta = paths.desktop_dir.join("beta");
        fs::write(&alpha, b"a").unwrap();
        fs::write(&beta, b"b").unwrap();
        let mut model = DesktopModel::open(paths.clone()).unwrap();
        let before = model.positioned((800, 600)).unwrap();
        let alpha_before = before
            .iter()
            .find(|item| item.item.path == alpha)
            .unwrap()
            .rect;
        let beta_before = before
            .iter()
            .find(|item| item.item.path == beta)
            .unwrap()
            .rect;

        assert!(
            model
                .move_item(
                    &alpha,
                    (f64::from(beta_before.x + 4), f64::from(beta_before.y + 4)),
                    (800, 600),
                )
                .unwrap()
        );
        let after = model.positioned((800, 600)).unwrap();
        assert_eq!(
            after
                .iter()
                .find(|item| item.item.path == alpha)
                .unwrap()
                .rect,
            beta_before
        );
        assert_eq!(
            after
                .iter()
                .find(|item| item.item.path == beta)
                .unwrap()
                .rect,
            alpha_before
        );

        let mut reopened = DesktopModel::open(paths).unwrap();
        assert_eq!(
            reopened
                .positioned((800, 600))
                .unwrap()
                .iter()
                .find(|item| item.item.path == alpha)
                .unwrap()
                .rect,
            beta_before
        );
    }

    #[test]
    fn arranging_returns_items_to_stable_name_order() {
        let temp = tempfile::tempdir().unwrap();
        let paths = XdgPaths::from_bases(
            temp.path().join("home"),
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("state"),
            vec![temp.path().join("share")],
            temp.path().join("Desktop"),
        )
        .unwrap();
        fs::create_dir_all(paths.managed_state_dir()).unwrap();
        fs::create_dir_all(&paths.desktop_dir).unwrap();
        let alpha = paths.desktop_dir.join("alpha");
        let beta = paths.desktop_dir.join("beta");
        fs::write(&alpha, b"a").unwrap();
        fs::write(&beta, b"b").unwrap();
        let mut model = DesktopModel::open(paths).unwrap();
        let initial = model.positioned((800, 600)).unwrap();
        let alpha_initial = initial
            .iter()
            .find(|item| item.item.path == alpha)
            .unwrap()
            .rect;
        let beta_initial = initial
            .iter()
            .find(|item| item.item.path == beta)
            .unwrap()
            .rect;
        model
            .move_item(
                &alpha,
                (f64::from(beta_initial.x + 4), f64::from(beta_initial.y + 4)),
                (800, 600),
            )
            .unwrap();
        model.arrange_icons().unwrap();
        let arranged = model.positioned((800, 600)).unwrap();
        assert_eq!(
            arranged
                .iter()
                .find(|item| item.item.path == alpha)
                .unwrap()
                .rect,
            alpha_initial
        );
        assert_eq!(
            arranged
                .iter()
                .find(|item| item.item.path == beta)
                .unwrap()
                .rect,
            beta_initial
        );
    }
}
