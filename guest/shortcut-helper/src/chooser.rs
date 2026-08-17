// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{LaunchResult, LaunchStatus, RegistrationStore, StoreError};
use buzzardos_desktop_core::{AppImageRegistration, RegistrationId};
use gtk::prelude::*;
use gtk4 as gtk;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelinkOutcome {
    Relinked(AppImageRegistration),
    Cancelled(AppImageRegistration),
}

#[derive(Debug, Error)]
pub enum ChooserError {
    #[error("cannot initialize the guest GTK session: {0}")]
    GtkInitialization(#[from] glib::BoolError),
    #[error("guest GTK operation failed: {0}")]
    Gtk(#[from] glib::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("selected item is not a guest-visible local file")]
    NonLocalSelection,
}

/// Launch a registration and enter the same native relink flow regardless of
/// whether activation came from the menu, desktop, Settings, AT-SPI, or CUA.
pub fn launch_with_relink(
    store: &RegistrationStore,
    id: RegistrationId,
) -> Result<LaunchResult, ChooserError> {
    let first = store.launch(id)?;
    if first.status == LaunchStatus::Started {
        return Ok(first);
    }
    match choose_relink(store, id)? {
        RelinkOutcome::Relinked(_) => Ok(store.launch(id)?),
        RelinkOutcome::Cancelled(registration) => Ok(LaunchResult {
            status: first.status,
            registration,
            child: None,
            diagnostic: Some("target_missing: relink cancelled; registration unchanged".into()),
        }),
    }
}

pub fn choose_relink(
    store: &RegistrationStore,
    id: RegistrationId,
) -> Result<RelinkOutcome, ChooserError> {
    gtk::init()?;
    let current = store.load(id)?;
    let context = glib::MainContext::default();
    let appimages = gtk::FileFilter::new();
    appimages.set_name(Some("AppImage applications"));
    appimages.add_pattern("*.AppImage");
    appimages.add_pattern("*.appimage");
    let all_files = gtk::FileFilter::new();
    all_files.set_name(Some("All files"));
    all_files.add_pattern("*");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&appimages);
    filters.append(&all_files);

    loop {
        let mut builder = gtk::FileDialog::builder()
            .title(format!("Locate {}", current.display_name))
            .accept_label("Relink")
            .modal(true)
            .filters(&filters)
            .default_filter(&appimages);
        if let Some(parent) = current.target_path.parent().filter(|path| path.is_dir()) {
            builder = builder.initial_folder(&gio::File::for_path(parent));
        }
        let dialog = builder.build();
        let selected = match context.block_on(dialog.open_future(None::<&gtk::Window>)) {
            Ok(file) => file,
            Err(error) if error.matches(gio::IOErrorEnum::Cancelled) => {
                return Ok(RelinkOutcome::Cancelled(current));
            }
            Err(error) => return Err(ChooserError::Gtk(error)),
        };
        let path = selected.path().ok_or(ChooserError::NonLocalSelection)?;
        let preview = match store.preview_relink(id, &path) {
            Ok(preview) => preview,
            Err(error) => {
                if !show_retry_dialog(&context, "That file cannot be used", &error.to_string())? {
                    return Ok(RelinkOutcome::Cancelled(current));
                }
                continue;
            }
        };
        let accept_different_identity = if preview.identity_differs {
            confirm_different_identity(
                &context,
                &preview.current.display_name,
                &preview.candidate.display_name,
                &path,
            )?
        } else {
            true
        };
        if !accept_different_identity {
            continue;
        }
        match store.commit_relink(preview, accept_different_identity) {
            Ok(registration) => return Ok(RelinkOutcome::Relinked(registration)),
            Err(error) => {
                if !show_retry_dialog(
                    &context,
                    "The AppImage was not relinked",
                    &error.to_string(),
                )? {
                    return Ok(RelinkOutcome::Cancelled(current));
                }
            }
        }
    }
}

fn show_retry_dialog(
    context: &glib::MainContext,
    message: &str,
    detail: &str,
) -> Result<bool, ChooserError> {
    let alert = gtk::AlertDialog::builder()
        .message(message)
        .detail(detail)
        .buttons(["Cancel", "Choose Another File"])
        .cancel_button(0)
        .default_button(1)
        .modal(true)
        .build();
    context
        .block_on(alert.choose_future(None::<&gtk::Window>))
        .map(|choice| choice == 1)
        .map_err(ChooserError::Gtk)
}

fn confirm_different_identity(
    context: &glib::MainContext,
    expected: &str,
    found: &str,
    path: &Path,
) -> Result<bool, ChooserError> {
    let detail = format!(
        "This registration was for “{expected}”, but the selected file identifies as “{found}”.\n\nSelected file: {}",
        display_path(path)
    );
    let alert = gtk::AlertDialog::builder()
        .message("Use a different application for this launcher?")
        .detail(detail)
        .buttons(["Choose Another File", "Use This AppImage"])
        .cancel_button(0)
        .default_button(0)
        .modal(true)
        .build();
    context
        .block_on(alert.choose_future(None::<&gtk::Window>))
        .map(|choice| choice == 1)
        .map_err(ChooserError::Gtk)
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_display_preserves_unicode_and_newlines_without_interpretation() {
        let path = PathBuf::from("/shared/odd ' 100%\n日本語.AppImage");
        assert_eq!(display_path(&path), "/shared/odd ' 100%\n日本語.AppImage");
    }
}
