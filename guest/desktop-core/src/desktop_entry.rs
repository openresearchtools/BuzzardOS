// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::appimage::RegistrationId;
use crate::persistence::{PersistenceError, atomic_write, read_bounded};
use crate::xdg::XdgPaths;
use gio::prelude::AppInfoExt;
use glib::{KeyFile, KeyFileFlags};
use std::collections::{BTreeMap, BinaryHeap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

const DESKTOP_ENTRY_GROUP: &str = "Desktop Entry";
const MAX_DESKTOP_ENTRY_BYTES: usize = 1024 * 1024;
const MAX_DESKTOP_FILES: usize = 16_384;
const MAX_APPLICATION_ROOTS: usize = 64;
const MAX_ENTRIES_PER_ROOT: usize = 65_536;
const MAX_DIRECTORY_DEPTH: usize = 16;
const MAX_DESKTOP_ID_BYTES: usize = 1024;
const MAX_NAME_BYTES: usize = 512;
const MAX_EXEC_BYTES: usize = 16_384;
const MAX_CATEGORIES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DesktopEntryId(String);

impl DesktopEntryId {
    pub fn from_relative_path(path: &Path) -> Result<Self, DesktopEntryError> {
        if path.is_absolute() || path.extension() != Some(OsStr::new("desktop")) {
            return Err(DesktopEntryError::InvalidId(
                "desktop-entry ID source must be a relative .desktop path".into(),
            ));
        }
        if path
            .as_os_str()
            .as_bytes()
            .split(|byte| *byte == b'/')
            .any(|component| component.is_empty() || component == b"." || component == b"..")
        {
            return Err(DesktopEntryError::InvalidId(
                "desktop-entry ID source must be lexically normalized".into(),
            ));
        }
        let mut parts = Vec::new();
        for component in path.components() {
            let Component::Normal(component) = component else {
                return Err(DesktopEntryError::InvalidId(
                    "desktop-entry ID source contains traversal".into(),
                ));
            };
            let component = component.to_str().ok_or_else(|| {
                DesktopEntryError::InvalidId("desktop-entry path is not valid UTF-8".into())
            })?;
            if component.is_empty() || component.chars().any(char::is_control) {
                return Err(DesktopEntryError::InvalidId(
                    "desktop-entry path has an empty or control-character component".into(),
                ));
            }
            parts.push(component);
        }
        let id = parts.join("-");
        if id.is_empty() || id.len() > MAX_DESKTOP_ID_BYTES {
            return Err(DesktopEntryError::InvalidId(format!(
                "desktop-entry ID must contain at most {MAX_DESKTOP_ID_BYTES} bytes"
            )));
        }
        Ok(Self(id))
    }

    pub fn managed_appimage(id: RegistrationId) -> Self {
        Self(id.desktop_file_id())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopApplication {
    pub id: DesktopEntryId,
    pub name: String,
    pub generic_name: Option<String>,
    pub executable: PathBuf,
    pub commandline: String,
    pub terminal: bool,
    pub icon: Option<String>,
    pub categories: Vec<String>,
    pub source: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressionReason {
    Hidden,
    NoDisplay,
    DesktopVisibility,
    HelperOrService,
    MissingTryExec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopEntryOutcome {
    Visible(DesktopApplication),
    Suppressed(SuppressionReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntryDiagnostic {
    pub path: PathBuf,
    pub desktop_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplicationCatalog {
    pub applications: Vec<DesktopApplication>,
    pub diagnostics: Vec<DesktopEntryDiagnostic>,
}

#[derive(Debug, Error)]
pub enum DesktopEntryError {
    #[error("invalid desktop-entry ID: {0}")]
    InvalidId(String),
    #[error("unsafe desktop-entry path: {0}")]
    UnsafePath(String),
    #[error("desktop entry is not valid UTF-8")]
    InvalidUtf8,
    #[error("desktop entry is malformed: {0}")]
    Malformed(String),
    #[error("desktop entry has invalid field {field}: {message}")]
    InvalidField {
        field: &'static str,
        message: String,
    },
    #[error(transparent)]
    Persistence(#[from] PersistenceError),
}

/// Discover visible applications in XDG precedence order. GLib supplies the
/// XDG paths and GIO supplies the FreeDesktop parser/model.
pub fn discover_applications(paths: &XdgPaths) -> ApplicationCatalog {
    let current_desktops = std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .map(|value| {
            value
                .split(':')
                .filter(|desktop| !desktop.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    discover_application_directories(&paths.application_dirs(), &current_desktops)
}

pub fn discover_application_directories(
    directories: &[PathBuf],
    current_desktops: &[String],
) -> ApplicationCatalog {
    discover_application_directories_with_limits(
        directories,
        current_desktops,
        ScanLimits {
            max_roots: MAX_APPLICATION_ROOTS,
            max_entries_per_root: MAX_ENTRIES_PER_ROOT,
            max_desktop_files: MAX_DESKTOP_FILES,
        },
    )
}

#[derive(Debug, Clone, Copy)]
struct ScanLimits {
    max_roots: usize,
    max_entries_per_root: usize,
    max_desktop_files: usize,
}

fn discover_application_directories_with_limits(
    directories: &[PathBuf],
    current_desktops: &[String],
    limits: ScanLimits,
) -> ApplicationCatalog {
    let mut catalog = ApplicationCatalog::default();
    let mut applications = BTreeMap::new();
    let mut claimed = HashSet::new();
    let mut remaining_desktop_files = limits.max_desktop_files;
    let root_count = directories.len().min(limits.max_roots);
    if directories.len() > root_count {
        catalog.diagnostics.push(DesktopEntryDiagnostic {
            path: PathBuf::new(),
            desktop_id: None,
            message: format!(
                "application scan ignored roots beyond the deterministic limit of {}",
                limits.max_roots
            ),
        });
    }

    for (root_index, root) in directories.iter().take(root_count).enumerate() {
        let metadata = match fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                catalog.diagnostics.push(diagnostic(root, None, error));
                continue;
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            catalog.diagnostics.push(DesktopEntryDiagnostic {
                path: root.clone(),
                desktop_id: None,
                message: "application root is not a real directory".into(),
            });
            continue;
        }
        let mut files = Vec::new();
        // Give this root a fair share of the candidates that remain while
        // reserving capacity for every lower-precedence root. Recomputing the
        // share after each scan lets missing, invalid, empty, and under-quota
        // roots donate their unused capacity to the roots that follow.
        let remaining_roots = root_count - root_index;
        let candidate_quota = remaining_desktop_files.div_ceil(remaining_roots);
        let mut budget = RootScanBudget {
            entries: 0,
            candidates: 0,
            max_entries: limits.max_entries_per_root,
            max_candidates: candidate_quota,
            saturated: false,
        };
        collect_desktop_files(
            root,
            root,
            0,
            &mut budget,
            &mut files,
            &mut catalog.diagnostics,
        );
        remaining_desktop_files -= budget.candidates;
        if budget.saturated {
            catalog.diagnostics.push(DesktopEntryDiagnostic {
                path: root.clone(),
                desktop_id: None,
                message: format!(
                    "application root reached its deterministic scan budget ({} entries, {} desktop candidates)",
                    budget.max_entries, budget.max_candidates
                ),
            });
        }
        files.sort();
        for (relative, path) in files {
            let id = match DesktopEntryId::from_relative_path(&relative) {
                Ok(id) => id,
                Err(error) => {
                    catalog.diagnostics.push(diagnostic(&path, None, error));
                    continue;
                }
            };
            if !claimed.insert(id.clone()) {
                continue;
            }
            match parse_desktop_entry(id.clone(), &path, current_desktops) {
                Ok(DesktopEntryOutcome::Visible(application)) => {
                    applications.insert(id, application);
                }
                Ok(DesktopEntryOutcome::Suppressed(_)) => {}
                Err(error) => {
                    catalog
                        .diagnostics
                        .push(diagnostic(&path, Some(id.as_str().to_owned()), error))
                }
            }
        }
    }
    catalog.applications = applications.into_values().collect();
    catalog.applications.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.id.cmp(&right.id))
    });
    catalog
}

#[derive(Debug)]
struct RootScanBudget {
    entries: usize,
    candidates: usize,
    max_entries: usize,
    max_candidates: usize,
    saturated: bool,
}

fn collect_desktop_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    budget: &mut RootScanBudget,
    files: &mut Vec<(PathBuf, PathBuf)>,
    diagnostics: &mut Vec<DesktopEntryDiagnostic>,
) {
    if depth > MAX_DIRECTORY_DEPTH
        || budget.entries >= budget.max_entries
        || budget.candidates >= budget.max_candidates
    {
        budget.saturated = true;
        return;
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(diagnostic(directory, None, error));
            return;
        }
    };
    // Retain only the lexicographically first paths that fit the remaining
    // per-root entry budget. This keeps memory bounded and selection stable
    // even when a hostile directory contains many irrelevant files.
    let remaining = budget.max_entries.saturating_sub(budget.entries);
    let mut selected = BinaryHeap::with_capacity(remaining.min(1024));
    let mut exceeded_entry_budget = false;
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if selected.len() < remaining {
            selected.push(path);
        } else {
            exceeded_entry_budget = true;
            if selected.peek().is_some_and(|largest| path < *largest) {
                selected.pop();
                selected.push(path);
            }
        }
    }
    budget.saturated |= exceeded_entry_budget;
    let entries = selected.into_sorted_vec();
    for path in entries {
        if budget.entries >= budget.max_entries || budget.candidates >= budget.max_candidates {
            budget.saturated = true;
            return;
        }
        budget.entries += 1;
        let file_type = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata.file_type(),
            Err(error) => {
                diagnostics.push(diagnostic(&path, None, error));
                continue;
            }
        };
        if file_type.is_symlink() {
            diagnostics.push(DesktopEntryDiagnostic {
                path,
                desktop_id: None,
                message: "symbolic links are not followed during application discovery".into(),
            });
            continue;
        }
        if file_type.is_dir() {
            collect_desktop_files(root, &path, depth + 1, budget, files, diagnostics);
        } else if file_type.is_file() && path.extension() == Some(OsStr::new("desktop")) {
            budget.candidates += 1;
            match path.strip_prefix(root) {
                Ok(relative) => files.push((relative.to_path_buf(), path)),
                Err(_) => diagnostics.push(DesktopEntryDiagnostic {
                    path,
                    desktop_id: None,
                    message: "application entry escaped its discovery root".into(),
                }),
            }
        }
    }
}

pub fn parse_desktop_entry(
    id: DesktopEntryId,
    path: &Path,
    current_desktops: &[String],
) -> Result<DesktopEntryOutcome, DesktopEntryError> {
    let bytes = read_bounded(path, MAX_DESKTOP_ENTRY_BYTES)?;
    let contents = std::str::from_utf8(&bytes).map_err(|_| DesktopEntryError::InvalidUtf8)?;
    let key_file = KeyFile::new();
    key_file
        .load_from_data(contents, KeyFileFlags::NONE)
        .map_err(|error| DesktopEntryError::Malformed(error.to_string()))?;
    if !key_file.has_group(DESKTOP_ENTRY_GROUP) {
        return Err(DesktopEntryError::Malformed(
            "missing [Desktop Entry] group".into(),
        ));
    }
    let entry_type = required_string(&key_file, "Type")?;
    if entry_type != "Application" {
        return Err(DesktopEntryError::InvalidField {
            field: "Type",
            message: "must be Application".into(),
        });
    }
    if optional_bool(&key_file, "Hidden")?.unwrap_or(false) {
        return Ok(DesktopEntryOutcome::Suppressed(SuppressionReason::Hidden));
    }
    if optional_bool(&key_file, "NoDisplay")?.unwrap_or(false) {
        return Ok(DesktopEntryOutcome::Suppressed(
            SuppressionReason::NoDisplay,
        ));
    }
    if is_helper_or_service(&key_file)? {
        return Ok(DesktopEntryOutcome::Suppressed(
            SuppressionReason::HelperOrService,
        ));
    }

    let name = key_file
        .locale_string(DESKTOP_ENTRY_GROUP, "Name", None)
        .map_err(|error| DesktopEntryError::InvalidField {
            field: "Name",
            message: error.to_string(),
        })?
        .to_string();
    validate_text("Name", &name, MAX_NAME_BYTES, true)?;
    let commandline = required_string(&key_file, "Exec")?;
    let executable = parse_exec(&commandline)?;

    if let Some(try_exec) = optional_string(&key_file, "TryExec")? {
        validate_text("TryExec", &try_exec, MAX_EXEC_BYTES, true)?;
        if glib::find_program_in_path(Path::new(&try_exec)).is_none() {
            return Ok(DesktopEntryOutcome::Suppressed(
                SuppressionReason::MissingTryExec,
            ));
        }
    }
    let info = gio::DesktopAppInfo::from_keyfile(&key_file)
        .ok_or_else(|| DesktopEntryError::Malformed("GIO rejected the desktop entry".into()))?;
    let visible = if current_desktops.is_empty() {
        info.should_show()
    } else {
        current_desktops
            .iter()
            .any(|desktop| info.shows_in(Some(desktop)))
    };
    if !visible {
        return Ok(DesktopEntryOutcome::Suppressed(
            SuppressionReason::DesktopVisibility,
        ));
    }
    let generic_name = optional_locale_string(&key_file, "GenericName")?;
    if let Some(value) = &generic_name {
        validate_text("GenericName", value, MAX_NAME_BYTES, true)?;
    }
    let icon = optional_string(&key_file, "Icon")?;
    if let Some(value) = &icon {
        validate_text("Icon", value, MAX_EXEC_BYTES, true)?;
    }
    let categories = optional_string(&key_file, "Categories")?
        .map(|value| {
            value
                .split(';')
                .filter(|category| !category.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if categories.len() > MAX_CATEGORIES {
        return Err(DesktopEntryError::InvalidField {
            field: "Categories",
            message: format!("contains more than {MAX_CATEGORIES} categories"),
        });
    }
    for category in &categories {
        validate_text("Categories", category, MAX_NAME_BYTES, true)?;
    }
    let terminal = optional_bool(&key_file, "Terminal")?.unwrap_or(false);
    Ok(DesktopEntryOutcome::Visible(DesktopApplication {
        id,
        name,
        generic_name,
        executable,
        commandline,
        terminal,
        icon,
        categories,
        source: path.to_path_buf(),
    }))
}

fn is_helper_or_service(key_file: &KeyFile) -> Result<bool, DesktopEntryError> {
    if key_file
        .has_key(DESKTOP_ENTRY_GROUP, "X-GNOME-Autostart-Phase")
        .map_err(|error| DesktopEntryError::Malformed(error.to_string()))?
        || key_file
            .has_key(DESKTOP_ENTRY_GROUP, "X-KDE-autostart-phase")
            .map_err(|error| DesktopEntryError::Malformed(error.to_string()))?
    {
        return Ok(true);
    }
    match optional_string(key_file, "X-WildBuzzard-Role")?.as_deref() {
        None | Some("application") => Ok(false),
        Some("helper" | "service") => Ok(true),
        Some(_) => Err(DesktopEntryError::InvalidField {
            field: "X-WildBuzzard-Role",
            message: "must be application, helper, or service".into(),
        }),
    }
}

fn required_string(key_file: &KeyFile, key: &'static str) -> Result<String, DesktopEntryError> {
    key_file
        .string(DESKTOP_ENTRY_GROUP, key)
        .map(|value| value.to_string())
        .map_err(|error| DesktopEntryError::InvalidField {
            field: key,
            message: error.to_string(),
        })
}

fn optional_string(
    key_file: &KeyFile,
    key: &'static str,
) -> Result<Option<String>, DesktopEntryError> {
    if !key_file
        .has_key(DESKTOP_ENTRY_GROUP, key)
        .map_err(|error| DesktopEntryError::Malformed(error.to_string()))?
    {
        return Ok(None);
    }
    required_string(key_file, key).map(Some)
}

fn optional_locale_string(
    key_file: &KeyFile,
    key: &'static str,
) -> Result<Option<String>, DesktopEntryError> {
    if !key_file
        .has_key(DESKTOP_ENTRY_GROUP, key)
        .map_err(|error| DesktopEntryError::Malformed(error.to_string()))?
    {
        return Ok(None);
    }
    key_file
        .locale_string(DESKTOP_ENTRY_GROUP, key, None)
        .map(|value| Some(value.to_string()))
        .map_err(|error| DesktopEntryError::InvalidField {
            field: key,
            message: error.to_string(),
        })
}

fn optional_bool(key_file: &KeyFile, key: &'static str) -> Result<Option<bool>, DesktopEntryError> {
    if !key_file
        .has_key(DESKTOP_ENTRY_GROUP, key)
        .map_err(|error| DesktopEntryError::Malformed(error.to_string()))?
    {
        return Ok(None);
    }
    key_file
        .boolean(DESKTOP_ENTRY_GROUP, key)
        .map(Some)
        .map_err(|error| DesktopEntryError::InvalidField {
            field: key,
            message: error.to_string(),
        })
}

fn validate_text(
    field: &'static str,
    value: &str,
    maximum: usize,
    nonempty: bool,
) -> Result<(), DesktopEntryError> {
    if (nonempty && value.is_empty())
        || value.len() > maximum
        || value.chars().any(char::is_control)
    {
        return Err(DesktopEntryError::InvalidField {
            field,
            message: format!(
                "must {}contain at most {maximum} bytes and no control characters",
                if nonempty { "be nonempty, " } else { "" }
            ),
        });
    }
    Ok(())
}

#[derive(Debug)]
struct ExecToken {
    literal: String,
    field_codes: Vec<char>,
}

/// Validate the Desktop Entry specification's Exec grammar after GKeyFile has
/// applied the generic string-value escapes. This is deliberately not a POSIX
/// shell parse: only whole-argument double quotes and the specification's
/// narrowly defined quoted escapes are accepted.
fn parse_exec(commandline: &str) -> Result<PathBuf, DesktopEntryError> {
    if commandline.is_empty() || commandline.len() > MAX_EXEC_BYTES || !commandline.is_ascii() {
        return Err(exec_error(format!(
            "must contain 1 to {MAX_EXEC_BYTES} ASCII bytes"
        )));
    }
    let bytes = commandline.as_bytes();
    let mut offset = 0usize;
    let mut tokens = Vec::new();
    while offset < bytes.len() {
        while bytes.get(offset) == Some(&b' ') {
            offset += 1;
        }
        if offset == bytes.len() {
            break;
        }
        let quoted = bytes[offset] == b'"';
        if quoted {
            offset += 1;
        }
        let mut token = ExecToken {
            literal: String::new(),
            field_codes: Vec::new(),
        };
        let mut closed_quote = !quoted;
        while offset < bytes.len() {
            let byte = bytes[offset];
            if quoted {
                if byte == b'"' {
                    closed_quote = true;
                    offset += 1;
                    if offset < bytes.len() && bytes[offset] != b' ' {
                        return Err(exec_error(
                            "a quoted argument must be quoted in whole".into(),
                        ));
                    }
                    break;
                }
                if byte == b'\\' {
                    let escaped = *bytes
                        .get(offset + 1)
                        .ok_or_else(|| exec_error("quoted escape is incomplete".into()))?;
                    if !matches!(escaped, b'"' | b'`' | b'$' | b'\\') {
                        return Err(exec_error(format!(
                            "quoted backslash may not escape byte 0x{escaped:02x}"
                        )));
                    }
                    token.literal.push(char::from(escaped));
                    offset += 2;
                    continue;
                }
                if matches!(byte, b'`' | b'$') {
                    return Err(exec_error(format!(
                        "reserved character {:?} must be escaped inside quotes",
                        char::from(byte)
                    )));
                }
                if byte == b'%' {
                    let code = parse_field_code(bytes, &mut offset)?;
                    if code == '%' {
                        token.literal.push('%');
                        continue;
                    }
                    return Err(exec_error(format!(
                        "field code %{code} must not appear inside a quoted argument"
                    )));
                }
                if byte.is_ascii_control() && !matches!(byte, b'\t' | b'\n') {
                    return Err(exec_error("quoted argument contains a control byte".into()));
                }
                token.literal.push(char::from(byte));
                offset += 1;
            } else {
                if byte == b' ' {
                    break;
                }
                if byte == b'%' {
                    let code = parse_field_code(bytes, &mut offset)?;
                    if code == '%' {
                        token.literal.push('%');
                    } else {
                        token.field_codes.push(code);
                    }
                    continue;
                }
                if is_unquoted_reserved(byte) || byte.is_ascii_control() {
                    return Err(exec_error(format!(
                        "reserved character {:?} must be inside a whole double-quoted argument",
                        char::from(byte)
                    )));
                }
                token.literal.push(char::from(byte));
                offset += 1;
            }
        }
        if quoted && !closed_quote {
            return Err(exec_error("quoted argument is not terminated".into()));
        }
        tokens.push(token);
    }
    let executable = tokens
        .first()
        .ok_or_else(|| exec_error("does not contain an executable".into()))?;
    if executable.literal.is_empty() {
        return Err(exec_error("executable name must not be empty".into()));
    }
    if !executable.field_codes.is_empty() {
        return Err(exec_error(
            "field codes are forbidden in the executable name".into(),
        ));
    }
    if executable.literal.contains('=') {
        return Err(exec_error("executable name must not contain '='".into()));
    }

    let mut file_argument_codes = 0usize;
    for token in &tokens {
        let standalone_code = (token.literal.is_empty() && token.field_codes.len() == 1)
            .then(|| token.field_codes[0]);
        for code in &token.field_codes {
            if matches!(code, 'f' | 'F' | 'u' | 'U') {
                file_argument_codes += 1;
            }
            if matches!(code, 'F' | 'U' | 'i') && standalone_code != Some(*code) {
                return Err(exec_error(format!(
                    "field code %{code} must be an argument on its own"
                )));
            }
        }
    }
    if file_argument_codes > 1 {
        return Err(exec_error(
            "at most one of %f, %F, %u, or %U may occur".into(),
        ));
    }
    Ok(PathBuf::from(&executable.literal))
}

fn parse_field_code(bytes: &[u8], offset: &mut usize) -> Result<char, DesktopEntryError> {
    let code = *bytes
        .get(*offset + 1)
        .ok_or_else(|| exec_error("ends with an incomplete field code".into()))?;
    *offset += 2;
    if matches!(
        code,
        b'f' | b'F'
            | b'u'
            | b'U'
            | b'd'
            | b'D'
            | b'n'
            | b'N'
            | b'i'
            | b'c'
            | b'k'
            | b'v'
            | b'm'
            | b'%'
    ) {
        Ok(char::from(code))
    } else {
        Err(exec_error(format!(
            "contains unknown field code %{}",
            char::from(code)
        )))
    }
}

fn is_unquoted_reserved(byte: u8) -> bool {
    matches!(
        byte,
        b'"' | b'\''
            | b'\\'
            | b'>'
            | b'<'
            | b'~'
            | b'|'
            | b'&'
            | b';'
            | b'$'
            | b'*'
            | b'?'
            | b'#'
            | b'('
            | b')'
            | b'`'
            | b'\t'
            | b'\n'
    )
}

fn exec_error(message: String) -> DesktopEntryError {
    DesktopEntryError::InvalidField {
        field: "Exec",
        message,
    }
}

fn diagnostic(
    path: &Path,
    desktop_id: Option<String>,
    error: impl fmt::Display,
) -> DesktopEntryDiagnostic {
    DesktopEntryDiagnostic {
        path: path.to_path_buf(),
        desktop_id,
        message: error.to_string(),
    }
}

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedAppImageDesktopEntry {
    pub id: RegistrationId,
    pub display_name: String,
    pub helper: PathBuf,
}

impl GeneratedAppImageDesktopEntry {
    pub fn render(&self) -> Result<String, DesktopEntryError> {
        validate_text("Name", &self.display_name, MAX_NAME_BYTES, true)?;
        if self.display_name.trim() != self.display_name {
            return Err(DesktopEntryError::InvalidField {
                field: "Name",
                message: "must not have surrounding whitespace".into(),
            });
        }
        let helper = self
            .helper
            .to_str()
            .ok_or_else(|| DesktopEntryError::InvalidField {
                field: "Exec",
                message: "helper path is not valid UTF-8".into(),
            })?;
        if !self.helper.is_absolute()
            || self.helper.as_os_str().as_bytes()[1..]
                .split(|byte| *byte == b'/')
                .any(|component| component.is_empty() || component == b"." || component == b"..")
            || self
                .helper
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
            || helper
                .bytes()
                .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'"' | b'\'' | b'\\'))
        {
            return Err(DesktopEntryError::InvalidField {
                field: "Exec",
                message: "helper must be an absolute normalized token-safe path".into(),
            });
        }
        let name = self.display_name.replace('\\', "\\\\");
        Ok(format!(
            "[Desktop Entry]\nVersion=1.0\nType=Application\nName={name}\nExec={helper} launch {}\nTryExec={helper}\nIcon={}\nTerminal=false\nCategories=Utility;\nX-WildBuzzard-AppImage-ID={}\n",
            self.id,
            self.id.icon_name(),
            self.id,
        ))
    }

    pub fn write(&self, path: &Path) -> Result<(), DesktopEntryError> {
        if path.file_name().and_then(OsStr::to_str) != Some(self.id.desktop_file_id().as_str()) {
            return Err(DesktopEntryError::InvalidId(format!(
                "managed AppImage entry must be named {}",
                self.id.desktop_file_id()
            )));
        }
        atomic_write(path, self.render()?.as_bytes(), 0o644)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn write_entry(path: &Path, name: &str, extra: &str) {
        fs::write(
            path,
            format!(
                "[Desktop Entry]\nVersion=1.0\nType=Application\nName={name}\nExec=/usr/bin/true %U\n{extra}"
            ),
        )
        .unwrap();
    }

    #[test]
    fn nested_paths_form_stable_freedesktop_ids() {
        assert_eq!(
            DesktopEntryId::from_relative_path(Path::new("vendor/tools.desktop"))
                .unwrap()
                .as_str(),
            "vendor-tools.desktop"
        );
        assert!(DesktopEntryId::from_relative_path(Path::new("../escape.desktop")).is_err());
        assert!(DesktopEntryId::from_relative_path(Path::new("vendor/./tools.desktop")).is_err());
    }

    #[test]
    fn discovery_respects_precedence_and_hidden_tombstones() {
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        let system = temp.path().join("system");
        fs::create_dir(&user).unwrap();
        fs::create_dir(&system).unwrap();
        write_entry(&system.join("browser.desktop"), "Browser", "");
        fs::write(
            user.join("browser.desktop"),
            "[Desktop Entry]\nType=Application\nHidden=true\n",
        )
        .unwrap();
        write_entry(&system.join("editor.desktop"), "Editor", "");
        let catalog = discover_application_directories(&[user, system], &[]);
        assert_eq!(catalog.applications.len(), 1, "{:?}", catalog.diagnostics);
        assert_eq!(catalog.applications[0].name, "Editor");
    }

    #[test]
    fn discovery_uses_gio_visibility_and_classifies_helpers() {
        let temp = tempfile::tempdir().unwrap();
        write_entry(
            &temp.path().join("visible.desktop"),
            "Visible",
            "OnlyShowIn=WildBuzzard;\n",
        );
        write_entry(
            &temp.path().join("other.desktop"),
            "Other",
            "OnlyShowIn=GNOME;\n",
        );
        write_entry(
            &temp.path().join("helper.desktop"),
            "Helper",
            "X-WildBuzzard-Role=helper\n",
        );
        let catalog =
            discover_application_directories(&[temp.path().to_path_buf()], &["WildBuzzard".into()]);
        assert_eq!(catalog.applications.len(), 1, "{:?}", catalog.diagnostics);
        assert_eq!(catalog.applications[0].name, "Visible");
    }

    #[test]
    fn symlinks_and_oversized_entries_do_not_escape_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("applications");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        write_entry(&outside.join("escape.desktop"), "Escape", "");
        symlink(&outside, root.join("linked")).unwrap();
        symlink(outside.join("escape.desktop"), root.join("escape.desktop")).unwrap();
        fs::write(
            root.join("huge.desktop"),
            vec![b'a'; MAX_DESKTOP_ENTRY_BYTES + 1],
        )
        .unwrap();
        let catalog = discover_application_directories(&[root], &[]);
        assert!(catalog.applications.is_empty());
        assert!(catalog.diagnostics.len() >= 2);
    }

    #[test]
    fn saturated_high_precedence_root_cannot_starve_lower_roots() {
        let temp = tempfile::tempdir().unwrap();
        let high = temp.path().join("high");
        let low = temp.path().join("low");
        fs::create_dir(&high).unwrap();
        fs::create_dir(&low).unwrap();
        for (file_name, display_name) in [
            ("a.desktop", "High A"),
            ("b.desktop", "High B"),
            ("c.desktop", "High C"),
            ("d.desktop", "High D"),
        ] {
            write_entry(&high.join(file_name), display_name, "");
        }
        write_entry(&low.join("real.desktop"), "Real Application", "");
        let catalog = discover_application_directories_with_limits(
            &[high, low],
            &[],
            ScanLimits {
                max_roots: 2,
                max_entries_per_root: 16,
                max_desktop_files: 4,
            },
        );
        let ids = catalog
            .applications
            .iter()
            .map(|application| application.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["a.desktop", "b.desktop", "real.desktop"]);
        assert!(
            catalog
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("deterministic scan budget"))
        );
    }

    #[test]
    fn unused_earlier_root_quotas_are_donated_to_the_last_root() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        let empty = temp.path().join("empty");
        let populated = temp.path().join("populated");
        fs::create_dir(&empty).unwrap();
        fs::create_dir(&populated).unwrap();
        for suffix in ['a', 'b', 'c', 'd', 'e', 'f', 'g'] {
            write_entry(
                &populated.join(format!("{suffix}.desktop")),
                &format!("Application {suffix}"),
                "",
            );
        }

        let catalog = discover_application_directories_with_limits(
            &[missing, empty, populated],
            &[],
            ScanLimits {
                max_roots: 3,
                max_entries_per_root: 16,
                max_desktop_files: 6,
            },
        );
        let ids = catalog
            .applications
            .iter()
            .map(|application| application.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "a.desktop",
                "b.desktop",
                "c.desktop",
                "d.desktop",
                "e.desktop",
                "f.desktop",
            ]
        );
    }

    #[test]
    fn malformed_exec_and_control_names_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        write_entry(&temp.path().join("bad-code.desktop"), "Bad", "");
        fs::write(
            temp.path().join("bad-code.desktop"),
            "[Desktop Entry]\nType=Application\nName=Bad\nExec=example %Z\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("bad-name.desktop"),
            "[Desktop Entry]\nType=Application\nName=Bad\\nName\nExec=example\n",
        )
        .unwrap();
        let catalog = discover_application_directories(&[temp.path().to_path_buf()], &[]);
        assert!(catalog.applications.is_empty());
        assert_eq!(catalog.diagnostics.len(), 2);
    }

    #[test]
    fn exec_grammar_rejects_field_codes_as_executables_and_hostile_quoting() {
        for hostile in [
            "%f /usr/bin/true",
            "/usr/%c/bin/true",
            r#""%f" /usr/bin/true"#,
            "/usr/bin/true %F-suffix",
            "/usr/bin/true %f %U",
            "/usr/bin/true 'single-quoted'",
            "/usr/bin/true unquoted;command",
            r#"/usr/bin/true "unterminated"#,
            r#"/usr/bin/true "unescaped $value""#,
            "/usr/bin/true %Z",
        ] {
            assert!(
                parse_exec(hostile).is_err(),
                "accepted hostile Exec={hostile:?}"
            );
        }
    }

    #[test]
    fn exec_grammar_accepts_whole_double_quotes_and_spec_field_codes() {
        assert_eq!(
            parse_exec(r#""/usr/bin/true" "odd; argument" %U"#).unwrap(),
            PathBuf::from("/usr/bin/true")
        );
        assert_eq!(
            parse_exec(r#"/usr/bin/true "cost \$5 and \`literal\`" %%"#).unwrap(),
            PathBuf::from("/usr/bin/true")
        );
        assert_eq!(
            parse_exec(r#""/tmp/program name" --flag=%c"#).unwrap(),
            PathBuf::from("/tmp/program name")
        );
    }

    #[test]
    fn generated_appimage_entry_never_embeds_the_untrusted_target() {
        let temp = tempfile::tempdir().unwrap();
        let id = RegistrationId::generate();
        let generated = GeneratedAppImageDesktopEntry {
            id,
            display_name: "Odd % Name 日本語".into(),
            helper: PathBuf::from("/usr/libexec/wildbuzzard-shortcut-helper"),
        };
        let rendered = generated.render().unwrap();
        assert!(rendered.contains(&format!(
            "Exec=/usr/libexec/wildbuzzard-shortcut-helper launch {id}"
        )));
        assert!(!rendered.contains("/shared"));
        assert!(!rendered.contains("sh -c"));
        let path = temp.path().join(id.desktop_file_id());
        generated.write(&path).unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
}
