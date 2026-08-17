// SPDX-License-Identifier: AGPL-3.0-or-later

//! Idempotent projection of Buzzard OS's fixed AppImage actions into
//! Thunar's user-owned custom-action file.
//!
//! Thunar resolves one `Thunar/uca.xml` through the XDG configuration search
//! path rather than merging system and user files.  Once a user edits any
//! custom action, their user file shadows the system file.  This module
//! therefore replaces only actions identified by Buzzard OS's two fixed
//! IDs while preserving every other byte in a valid user document.

use buzzardos_desktop_core::{atomic_write, read_bounded};
use roxmltree::{Document, Node};
use serde::Serialize;
use std::fs;
use std::ops::Range;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

const MAX_UCA_BYTES: usize = 1024 * 1024;
const TEMPLATE_PATH: &str = "/etc/buzzardos/xdg/Thunar/uca.xml";
const APPLICATIONS_ID: &str = "buzzardos-appimage-add-applications-v1";
const DESKTOP_ID: &str = "buzzardos-appimage-add-desktop-v1";
const MANAGED_IDS: [&str; 2] = [APPLICATIONS_ID, DESKTOP_ID];
const APPLICATIONS_COMMAND: &str =
    "/usr/libexec/buzzardos-shortcut-helper register-applications %f";
const DESKTOP_COMMAND: &str = "/usr/libexec/buzzardos-shortcut-helper register-desktop %f";
const PATTERNS: &str = "*.AppImage;*.appimage";
const RANGE: &str = "1-1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThunarActionInstall {
    pub changed: bool,
    pub preserved_custom_actions: usize,
    pub managed_actions: usize,
    pub path: PathBuf,
}

#[derive(Debug, Error)]
pub enum ThunarActionInstallError {
    #[error("Thunar configuration base must be an absolute normalized path: {0}")]
    InvalidConfigBase(String),
    #[error("Thunar configuration directory is not a real directory: {0}")]
    UnsafeConfigDirectory(String),
    #[error("Thunar custom-action file is not a regular file: {0}")]
    UnsafeUserFile(String),
    #[error("Thunar custom-action XML is invalid and was preserved unchanged: {0}")]
    InvalidUserXml(String),
    #[error("managed Thunar action template is invalid: {0}")]
    InvalidTemplate(String),
    #[error("I/O error for {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Persistence(#[from] buzzardos_desktop_core::persistence::PersistenceError),
}

fn io_error(path: &Path, source: std::io::Error) -> ThunarActionInstallError {
    ThunarActionInstallError::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Install or refresh the two managed actions for the current guest user.
pub fn install_thunar_actions() -> Result<ThunarActionInstall, ThunarActionInstallError> {
    install_thunar_actions_at(&glib::user_config_dir(), Path::new(TEMPLATE_PATH))
}

fn valid_absolute_base(path: &Path) -> bool {
    path.is_absolute()
        && path != Path::new("/")
        && !path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
}

fn ensure_real_directory(path: &Path) -> Result<(), ThunarActionInstallError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(ThunarActionInstallError::UnsafeConfigDirectory(
            path.display().to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|source| io_error(path, source))
        }
        Err(source) => Err(io_error(path, source)),
    }
}

fn install_thunar_actions_at(
    config_home: &Path,
    template_path: &Path,
) -> Result<ThunarActionInstall, ThunarActionInstallError> {
    if !valid_absolute_base(config_home) {
        return Err(ThunarActionInstallError::InvalidConfigBase(
            config_home.display().to_string(),
        ));
    }
    ensure_real_directory(config_home)?;
    let thunar_directory = config_home.join("Thunar");
    ensure_real_directory(&thunar_directory)?;
    let target = thunar_directory.join("uca.xml");

    let template_bytes = read_bounded(template_path, MAX_UCA_BYTES)?;
    let template = std::str::from_utf8(&template_bytes)
        .map_err(|error| ThunarActionInstallError::InvalidTemplate(error.to_string()))?;
    let managed = ManagedTemplate::parse(template)?;

    let mut mode_is_private = true;
    let original = match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            mode_is_private = metadata.permissions().mode() & 0o777 == 0o600;
            read_bounded(&target, MAX_UCA_BYTES)?
        }
        Ok(_) => {
            return Err(ThunarActionInstallError::UnsafeUserFile(
                target.display().to_string(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n".to_vec()
        }
        Err(source) => return Err(io_error(&target, source)),
    };
    let original_text = std::str::from_utf8(&original)
        .map_err(|error| ThunarActionInstallError::InvalidUserXml(error.to_string()))?;
    let parsed = ParsedActions::parse(original_text).map_err(|error| {
        ThunarActionInstallError::InvalidUserXml(format!("{}: {error}", target.display()))
    })?;
    let replacement = parsed.merge(original_text, &managed.fragment);
    let changed = replacement.as_bytes() != original || !mode_is_private;
    if changed {
        atomic_write(&target, replacement.as_bytes(), 0o600)?;
    }
    Ok(ThunarActionInstall {
        changed,
        preserved_custom_actions: parsed.custom_action_count,
        managed_actions: MANAGED_IDS.len(),
        path: target,
    })
}

#[derive(Debug)]
struct ManagedTemplate {
    fragment: String,
}

impl ManagedTemplate {
    fn parse(xml: &str) -> Result<Self, ThunarActionInstallError> {
        let parsed =
            ParsedActions::parse(xml).map_err(ThunarActionInstallError::InvalidTemplate)?;
        if parsed.actions.len() != MANAGED_IDS.len()
            || parsed
                .actions
                .iter()
                .any(|action| !MANAGED_IDS.contains(&action.id.as_str()))
        {
            return Err(ThunarActionInstallError::InvalidTemplate(
                "template must contain exactly the two fixed Buzzard OS actions".into(),
            ));
        }
        validate_template_action(
            parsed.action_by_id(APPLICATIONS_ID),
            APPLICATIONS_COMMAND,
            "Add to Applications",
        )?;
        validate_template_action(
            parsed.action_by_id(DESKTOP_ID),
            DESKTOP_COMMAND,
            "Add Desktop Shortcut",
        )?;

        let mut fragment = String::new();
        for action in &parsed.actions {
            fragment.push_str("  ");
            fragment.push_str(&xml[action.range.clone()]);
            fragment.push('\n');
        }
        Ok(Self { fragment })
    }
}

fn validate_template_action(
    action: Option<&ParsedAction>,
    command: &str,
    name: &str,
) -> Result<(), ThunarActionInstallError> {
    let Some(action) = action else {
        return Err(ThunarActionInstallError::InvalidTemplate(format!(
            "managed action for {name} is missing"
        )));
    };
    if action.command.as_deref() != Some(command)
        || action.name.as_deref() != Some(name)
        || action.patterns.as_deref() != Some(PATTERNS)
        || action.range_value.as_deref() != Some(RANGE)
        || !action.other_files
    {
        return Err(ThunarActionInstallError::InvalidTemplate(format!(
            "managed action {name} does not match its fixed command and conditions"
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct ParsedActions {
    root_range: Range<usize>,
    root_close_start: usize,
    empty_root: bool,
    actions: Vec<ParsedAction>,
    managed_ranges: Vec<Range<usize>>,
    custom_action_count: usize,
}

#[derive(Debug)]
struct ParsedAction {
    id: String,
    range: Range<usize>,
    name: Option<String>,
    command: Option<String>,
    patterns: Option<String>,
    range_value: Option<String>,
    other_files: bool,
}

impl ParsedActions {
    fn parse(xml: &str) -> Result<Self, String> {
        if xml.as_bytes().contains(&0) {
            return Err("NUL bytes are not allowed".into());
        }
        // UCA has no use for a DTD or custom entity. Refusing those constructs
        // avoids turning a login migration into an XML entity-expansion path.
        if xml.contains("<!DOCTYPE") || xml.contains("<!ENTITY") {
            return Err("DOCTYPE and ENTITY declarations are not allowed".into());
        }
        let document = Document::parse(xml).map_err(|error| error.to_string())?;
        let root = document.root_element();
        if root.tag_name().name() != "actions" || root.tag_name().namespace().is_some() {
            return Err("root element must be unnamespaced <actions>".into());
        }
        let root_range = root.range();
        let root_source = &xml[root_range.clone()];
        let empty_root = root_source.trim_end().ends_with("/>");
        let root_close_start = if empty_root {
            root_range.end
                - root_source
                    .rfind("/>")
                    .map(|offset| root_source.len() - offset)
                    .ok_or_else(|| "empty actions root has no closing delimiter".to_owned())?
        } else {
            root_range.start
                + root_source
                    .rfind("</actions")
                    .ok_or_else(|| "actions root has no closing tag".to_owned())?
        };

        let mut actions = Vec::new();
        let mut managed_ranges = Vec::new();
        let mut custom_action_count = 0;
        for node in root.children().filter(Node::is_element) {
            if node.tag_name().name() != "action" || node.tag_name().namespace().is_some() {
                continue;
            }
            let id = child_text(node, "unique-id").unwrap_or_default();
            let action = ParsedAction {
                id: id.clone(),
                range: node.range(),
                name: child_text(node, "name"),
                command: child_text(node, "command"),
                patterns: child_text(node, "patterns"),
                range_value: child_text(node, "range"),
                other_files: node
                    .children()
                    .any(|child| child.is_element() && child.has_tag_name("other-files")),
            };
            if MANAGED_IDS.contains(&id.as_str()) {
                managed_ranges.push(whole_line_range(xml, action.range.clone()));
            } else {
                custom_action_count += 1;
            }
            actions.push(action);
        }
        managed_ranges.sort_by_key(|range| range.start);
        for adjacent in managed_ranges.windows(2) {
            if adjacent[0].end > adjacent[1].start {
                return Err("managed action ranges overlap".into());
            }
        }
        Ok(Self {
            root_range,
            root_close_start,
            empty_root,
            actions,
            managed_ranges,
            custom_action_count,
        })
    }

    fn action_by_id(&self, id: &str) -> Option<&ParsedAction> {
        self.actions.iter().find(|action| action.id == id)
    }

    fn merge(&self, xml: &str, managed_fragment: &str) -> String {
        if self.empty_root {
            let mut output = String::with_capacity(xml.len() + managed_fragment.len() + 24);
            output.push_str(&xml[..self.root_close_start]);
            output.push_str(">\n");
            output.push_str(managed_fragment);
            output.push_str("</actions>");
            output.push_str(&xml[self.root_range.end..]);
            return output;
        }

        let mut without_managed = String::with_capacity(xml.len() + managed_fragment.len());
        let mut cursor = 0;
        for range in &self.managed_ranges {
            without_managed.push_str(&xml[cursor..range.start]);
            cursor = range.end;
        }
        let adjusted_close = self.root_close_start
            - self
                .managed_ranges
                .iter()
                .filter(|range| range.end <= self.root_close_start)
                .map(|range| range.len())
                .sum::<usize>();
        without_managed.push_str(&xml[cursor..]);

        let mut output = String::with_capacity(without_managed.len() + managed_fragment.len() + 1);
        output.push_str(&without_managed[..adjusted_close]);
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(managed_fragment);
        output.push_str(&without_managed[adjusted_close..]);
        output
    }
}

fn child_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    let child = node
        .children()
        .find(|child| child.is_element() && child.has_tag_name(name))?;
    Some(child.text().unwrap_or_default().trim().to_owned())
}

fn whole_line_range(xml: &str, range: Range<usize>) -> Range<usize> {
    let bytes = xml.as_bytes();
    let line_start = bytes[..range.start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    let start = if bytes[line_start..range.start]
        .iter()
        .all(u8::is_ascii_whitespace)
    {
        line_start
    } else {
        range.start
    };
    let after_end = &bytes[range.end..];
    let newline_offset = after_end.iter().position(|byte| *byte == b'\n');
    let end = match newline_offset {
        Some(offset) if after_end[..offset].iter().all(u8::is_ascii_whitespace) => {
            range.end + offset + 1
        }
        _ => range.end,
    };
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    const TEMPLATE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<actions>
  <action>
    <icon>application-x-executable</icon>
    <name>Add to Applications</name>
    <unique-id>buzzardos-appimage-add-applications-v1</unique-id>
    <command>/usr/libexec/buzzardos-shortcut-helper register-applications %f</command>
    <description>Register this AppImage in the Applications menu</description>
    <range>1-1</range>
    <patterns>*.AppImage;*.appimage</patterns>
    <other-files/>
  </action>
  <action>
    <icon>user-desktop</icon>
    <name>Add Desktop Shortcut</name>
    <unique-id>buzzardos-appimage-add-desktop-v1</unique-id>
    <command>/usr/libexec/buzzardos-shortcut-helper register-desktop %f</command>
    <description>Create a desktop shortcut linked to this AppImage</description>
    <range>1-1</range>
    <patterns>*.AppImage;*.appimage</patterns>
    <other-files/>
  </action>
</actions>
"#;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("config");
        let template_dir = temporary.path().join("system/Thunar");
        fs::create_dir(&config).unwrap();
        fs::create_dir_all(&template_dir).unwrap();
        let template = template_dir.join("uca.xml");
        fs::write(&template, TEMPLATE).unwrap();
        (temporary, config, template)
    }

    #[test]
    fn installs_two_fixed_actions_and_is_byte_idempotent() {
        let (_temporary, config, template) = setup();
        let first = install_thunar_actions_at(&config, &template).unwrap();
        assert!(first.changed);
        assert_eq!(first.preserved_custom_actions, 0);
        let first_bytes = fs::read(&first.path).unwrap();
        assert_eq!(
            fs::metadata(&first.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let second = install_thunar_actions_at(&config, &template).unwrap();
        assert!(!second.changed);
        assert_eq!(fs::read(&second.path).unwrap(), first_bytes);
        let text = String::from_utf8(first_bytes).unwrap();
        for id in MANAGED_IDS {
            assert_eq!(text.matches(id).count(), 1);
        }
        assert!(!text.contains("sh -c"));
        assert!(!text.contains("%F"));
    }

    #[test]
    fn preserves_user_actions_comments_and_commands_exactly() {
        let (_temporary, config, template) = setup();
        let thunar = config.join("Thunar");
        fs::create_dir(&thunar).unwrap();
        let custom = r#"<?xml version="1.0" encoding="UTF-8"?>
<actions>
<!-- user comment must remain byte-identical -->
<action><name>User &amp; Action</name><unique-id>1700000000000000-1</unique-id><command>printf '%s' %f</command><patterns>*</patterns><other-files/></action>
</actions>
"#;
        fs::write(thunar.join("uca.xml"), custom).unwrap();
        let installed = install_thunar_actions_at(&config, &template).unwrap();
        assert_eq!(installed.preserved_custom_actions, 1);
        let result = fs::read_to_string(installed.path).unwrap();
        assert!(result.contains("<!-- user comment must remain byte-identical -->"));
        assert!(result.contains("<command>printf '%s' %f</command>"));
        assert!(result.contains("<unique-id>1700000000000000-1</unique-id>"));
    }

    #[test]
    fn refreshes_only_managed_ids_after_thunar_reserializes_the_file() {
        let (_temporary, config, template) = setup();
        let thunar = config.join("Thunar");
        fs::create_dir(&thunar).unwrap();
        let stale = TEMPLATE.replace(
            "register-applications %f",
            "register-applications --obsolete %f",
        );
        fs::write(thunar.join("uca.xml"), stale).unwrap();
        install_thunar_actions_at(&config, &template).unwrap();
        let result = fs::read_to_string(thunar.join("uca.xml")).unwrap();
        assert!(!result.contains("--obsolete"));
        assert_eq!(result.matches(APPLICATIONS_ID).count(), 1);
        assert_eq!(result.matches(DESKTOP_ID).count(), 1);
    }

    #[test]
    fn malformed_or_unsafe_user_file_is_never_replaced() {
        let (_temporary, config, template) = setup();
        let thunar = config.join("Thunar");
        fs::create_dir(&thunar).unwrap();
        let target = thunar.join("uca.xml");
        fs::write(&target, b"<actions><action>").unwrap();
        let before = fs::read(&target).unwrap();
        assert!(matches!(
            install_thunar_actions_at(&config, &template),
            Err(ThunarActionInstallError::InvalidUserXml(_))
        ));
        assert_eq!(fs::read(&target).unwrap(), before);

        fs::remove_file(&target).unwrap();
        let victim = thunar.join("victim");
        fs::write(&victim, b"do not replace").unwrap();
        symlink(&victim, &target).unwrap();
        assert!(matches!(
            install_thunar_actions_at(&config, &template),
            Err(ThunarActionInstallError::UnsafeUserFile(_))
        ));
        assert_eq!(fs::read(&victim).unwrap(), b"do not replace");
    }

    #[test]
    fn oversized_or_entity_bearing_user_file_is_preserved() {
        let (_temporary, config, template) = setup();
        let thunar = config.join("Thunar");
        fs::create_dir(&thunar).unwrap();
        let target = thunar.join("uca.xml");
        let oversized = vec![b'x'; MAX_UCA_BYTES + 1];
        fs::write(&target, &oversized).unwrap();
        assert!(install_thunar_actions_at(&config, &template).is_err());
        assert_eq!(fs::read(&target).unwrap(), oversized);

        let entity = b"<!DOCTYPE actions [<!ENTITY x 'unsafe'>]><actions></actions>";
        fs::write(&target, entity).unwrap();
        assert!(matches!(
            install_thunar_actions_at(&config, &template),
            Err(ThunarActionInstallError::InvalidUserXml(_))
        ));
        assert_eq!(fs::read(&target).unwrap(), entity);
    }

    #[test]
    fn tampered_managed_template_is_rejected() {
        let (_temporary, config, template) = setup();
        fs::write(
            &template,
            TEMPLATE.replace(APPLICATIONS_COMMAND, "sh -c 'unsafe' %f"),
        )
        .unwrap();
        assert!(matches!(
            install_thunar_actions_at(&config, &template),
            Err(ThunarActionInstallError::InvalidTemplate(_))
        ));
        assert!(!config.join("Thunar/uca.xml").exists());
    }

    #[test]
    fn empty_actions_root_is_expanded_without_losing_surrounding_bytes() {
        let (_temporary, config, template) = setup();
        let thunar = config.join("Thunar");
        fs::create_dir(&thunar).unwrap();
        let target = thunar.join("uca.xml");
        fs::write(
            &target,
            b"<?xml version=\"1.0\"?>\n<actions/>\n<!-- tail -->\n",
        )
        .unwrap();
        install_thunar_actions_at(&config, &template).unwrap();
        let result = fs::read_to_string(target).unwrap();
        assert!(result.contains("</actions>\n<!-- tail -->"));
        assert_eq!(result.matches(APPLICATIONS_ID).count(), 1);
        assert_eq!(result.matches(DESKTOP_ID).count(), 1);
    }
}
