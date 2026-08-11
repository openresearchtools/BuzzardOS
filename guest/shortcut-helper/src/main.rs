// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use gio::prelude::*;
use serde::Serialize;
use serde_json::json;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use wildbuzzard_desktop_core::{DesktopDirectory, RegistrationId, XdgPaths};
use wildbuzzard_shortcut_helper::{
    LaunchStatus, RegistrationFlags, RegistrationStore, install_thunar_actions, validate_appimage,
};

#[cfg(feature = "chooser")]
use wildbuzzard_shortcut_helper::{RelinkOutcome, choose_relink, launch_with_relink};

fn main() {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string(&value).expect("JSON result is serializable")
            );
        }
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::to_string(&json!({
                    "ok": false,
                    "error": format!("{error:#}"),
                }))
                .expect("JSON error is serializable")
            );
            std::process::exit(1);
        }
    }
}

fn run(arguments: Vec<OsString>) -> Result<serde_json::Value> {
    let (command, rest) = arguments
        .split_first()
        .context("usage: wildbuzzard-shortcut-helper COMMAND [ARGUMENTS]")?;
    let command = command
        .to_str()
        .context("command name must be valid UTF-8")?;
    // Store construction is also the deterministic recovery boundary for an
    // interrupted registered-AppImage Desktop rename. The session invokes
    // `install-thunar-actions` at every login, so construct the store before
    // even that otherwise-independent startup command.
    let store = RegistrationStore::discover().context("initialize AppImage registrations")?;
    if command == "install-thunar-actions" {
        if !rest.is_empty() {
            bail!("install-thunar-actions does not accept arguments");
        }
        return Ok(json!({ "ok": true, "install": install_thunar_actions()? }));
    }
    match command {
        "inspect" => {
            let path = exactly_one_path(rest)?;
            let inspected = validate_appimage(path)?.inspect_metadata()?;
            Ok(json!({
                "ok": true,
                "display_name": inspected.display_name,
                "identity_key": inspected.identity_key,
                "observation": inspected.observation,
                "squashfs_offset": inspected.squashfs_offset,
                "icon": inspected.icon.as_ref().map(|icon| json!({
                    "source_name": icon.source_name,
                    "content_sha256": icon.content_sha256,
                    "normalized_size": icon.png_256.len(),
                })),
            }))
        }
        "register-applications" => registration_json(
            store.register(exactly_one_path(rest)?, RegistrationFlags::APPLICATIONS)?,
        ),
        "register-desktop" => {
            registration_json(store.register(exactly_one_path(rest)?, RegistrationFlags::DESKTOP)?)
        }
        "add-applications" => registration_json(store.add_applications(one_id(rest)?)?),
        "add-desktop" => registration_json(store.add_desktop(one_id(rest)?)?),
        "remove-applications" => {
            optional_registration_json(store.remove_applications(one_id(rest)?)?)
        }
        "remove-desktop" => optional_registration_json(store.remove_desktop(one_id(rest)?)?),
        "remove-applications-for" => remove_for_target(&store, exactly_one_path(rest)?, true),
        "remove-desktop-for" => remove_for_target(&store, exactly_one_path(rest)?, false),
        "status-for" => optional_registration_json(store.find_by_target(exactly_one_path(rest)?)?),
        "list" => Ok(json!({ "ok": true, "registrations": store.list()? })),
        "launch" => launch_json(&store, one_id(rest)?),
        "choose-relink" => choose_relink_json(&store, one_id(rest)?),
        "relink" => relink(&store, rest),
        "reveal" => {
            store.reveal_target(one_id(rest)?)?;
            Ok(json!({ "ok": true }))
        }
        "open" => {
            open_path(exactly_one_path(rest)?)?;
            Ok(json!({ "ok": true }))
        }
        "desktop-list" => desktop_list(rest),
        "desktop-new-folder" => desktop_new_folder(rest),
        "desktop-rename" => desktop_rename(&store, rest),
        "desktop-delete-after-confirmation" => desktop_delete(rest),
        _ => bail!("unknown command: {command}"),
    }
}

#[cfg(feature = "chooser")]
fn choose_relink_json(store: &RegistrationStore, id: RegistrationId) -> Result<serde_json::Value> {
    Ok(match choose_relink(store, id)? {
        RelinkOutcome::Relinked(registration) => json!({
            "ok": true,
            "outcome": "relinked",
            "registration": registration,
        }),
        RelinkOutcome::Cancelled(registration) => json!({
            "ok": false,
            "outcome": "cancelled",
            "registration": registration,
        }),
    })
}

#[cfg(not(feature = "chooser"))]
fn choose_relink_json(store: &RegistrationStore, id: RegistrationId) -> Result<serde_json::Value> {
    let _ = (store, id);
    bail!("this helper build does not include the native relink chooser")
}

fn registration_json<T: Serialize>(registration: T) -> Result<serde_json::Value> {
    Ok(json!({ "ok": true, "registration": registration }))
}

fn optional_registration_json<T: Serialize>(registration: Option<T>) -> Result<serde_json::Value> {
    Ok(json!({ "ok": true, "registration": registration }))
}

fn remove_for_target(
    store: &RegistrationStore,
    target: &Path,
    applications: bool,
) -> Result<serde_json::Value> {
    let Some(registration) = store.find_by_target(target)? else {
        return Ok(json!({ "ok": true, "registration": null, "already_absent": true }));
    };
    if applications {
        optional_registration_json(store.remove_applications(registration.id)?)
    } else {
        optional_registration_json(store.remove_desktop(registration.id)?)
    }
}

fn launch_json(store: &RegistrationStore, id: RegistrationId) -> Result<serde_json::Value> {
    #[cfg(feature = "chooser")]
    let result = launch_with_relink(store, id)?;
    #[cfg(not(feature = "chooser"))]
    let result = store.launch(id)?;
    let process_id = result.child.as_ref().map(std::process::Child::id);
    let ok = result.status == LaunchStatus::Started;
    Ok(json!({
        "ok": ok,
        "status": result.status,
        "registration": result.registration,
        "process_id": process_id,
        "diagnostic": result.diagnostic,
    }))
}

fn relink(store: &RegistrationStore, arguments: &[OsString]) -> Result<serde_json::Value> {
    if arguments.len() < 2 || arguments.len() > 3 {
        bail!("relink requires ID PATH [--accept-different-identity]");
    }
    let id = parse_id(&arguments[0])?;
    let path = Path::new(&arguments[1]);
    let accept = match arguments.get(2) {
        None => false,
        Some(value) if value == OsStr::new("--accept-different-identity") => true,
        Some(_) => bail!("unknown relink option"),
    };
    let preview = store.preview_relink(id, path)?;
    let differs = preview.identity_differs;
    let registration = store.commit_relink(preview, accept)?;
    Ok(json!({
        "ok": true,
        "identity_differed": differs,
        "registration": registration,
    }))
}

fn open_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("open path must be absolute");
    }
    let metadata =
        std::fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!(
            "refusing to follow a symbolic link for Open: {}",
            path.display()
        );
    }
    let uri = gio::File::for_path(path).uri();
    gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
        .with_context(|| format!("open {}", path.display()))
}

#[derive(Serialize)]
struct DesktopItemJson {
    name: String,
    display_name: String,
    path: PathBuf,
    identity: wildbuzzard_desktop_core::FileIdentity,
    kind: wildbuzzard_desktop_core::DesktopItemKind,
    size: u64,
}

fn desktop_directory(arguments: &[OsString]) -> Result<DesktopDirectory> {
    if !arguments.is_empty() {
        bail!("desktop command received unexpected arguments");
    }
    let paths = XdgPaths::discover()?;
    Ok(DesktopDirectory::create_and_open(&paths.desktop_dir)?)
}

fn desktop_list(arguments: &[OsString]) -> Result<serde_json::Value> {
    let directory = desktop_directory(arguments)?;
    let items = directory
        .list()?
        .into_iter()
        .map(|item| DesktopItemJson {
            name: item.name.to_string_lossy().into_owned(),
            display_name: item.display_name,
            path: item.path,
            identity: item.identity,
            kind: item.kind,
            size: item.size,
        })
        .collect::<Vec<_>>();
    Ok(json!({ "ok": true, "items": items }))
}

fn desktop_new_folder(arguments: &[OsString]) -> Result<serde_json::Value> {
    let name = exactly_one_name(arguments)?;
    let directory = desktop_directory(&[])?;
    let created = directory.create_folder(name)?;
    Ok(json!({ "ok": true, "name": created.to_string_lossy() }))
}

fn desktop_rename(store: &RegistrationStore, arguments: &[OsString]) -> Result<serde_json::Value> {
    if arguments.len() != 2 {
        bail!("desktop-rename requires OLD_NAME NEW_NAME");
    }
    let path = store.rename_desktop_item(&arguments[0], &arguments[1])?;
    Ok(json!({ "ok": true, "path": path }))
}

fn desktop_delete(arguments: &[OsString]) -> Result<serde_json::Value> {
    let name = exactly_one_name(arguments)?;
    let directory = desktop_directory(&[])?;
    let consequence = directory.consequence(name)?;
    directory.delete_confirmed(name)?;
    Ok(json!({ "ok": true, "consequence": format!("{consequence:?}") }))
}

fn exactly_one_path(arguments: &[OsString]) -> Result<&Path> {
    Ok(Path::new(exactly_one_name(arguments)?))
}

fn exactly_one_name(arguments: &[OsString]) -> Result<&OsStr> {
    if arguments.len() != 1 {
        bail!("command requires exactly one argument");
    }
    Ok(&arguments[0])
}

fn one_id(arguments: &[OsString]) -> Result<RegistrationId> {
    parse_id(exactly_one_name(arguments)?)
}

fn parse_id(value: &OsStr) -> Result<RegistrationId> {
    RegistrationId::from_str(value.to_str().context("registration ID must be UTF-8")?)
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_parser_rejects_shell_text_and_noncanonical_ids() {
        assert!(parse_id(OsStr::new("$(touch /tmp/no)")).is_err());
        assert!(parse_id(OsStr::new("D5C17711-86D2-4989-9090-66BB94202D76")).is_err());
    }

    #[test]
    fn path_arguments_are_never_split_or_interpreted() {
        let argument = OsString::from("/shared/odd ' 100%\n日本語.AppImage");
        assert_eq!(
            exactly_one_path(&[argument]).unwrap(),
            Path::new("/shared/odd ' 100%\n日本語.AppImage")
        );
    }
}
