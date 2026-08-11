// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use wildbuzzard_desktop_core::{UpdateState, UpdateStatus, atomic_write_json, read_bounded};

pub const UPDATER_STATE_DIRECTORY: &str = "/var/lib/wildbuzzard-updater";
pub const UPDATER_STATE_PATH: &str = "/var/lib/wildbuzzard-updater/state.json";
const NOTIFICATION_SCHEMA_VERSION: u32 = 1;
const MAX_NOTIFICATION_RECORD_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNotice {
    pub generation: String,
    pub package_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationRecord {
    schema_version: u32,
    plan_generation: String,
}

impl NotificationRecord {
    fn validate(&self) -> Result<(), String> {
        if self.schema_version != NOTIFICATION_SCHEMA_VERSION {
            return Err(format!(
                "unsupported update-notification schema {}",
                self.schema_version
            ));
        }
        if self.plan_generation.len() != 64
            || !self
                .plan_generation
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("update-notification generation is not canonical".into());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct UpdateTracker {
    state_path: PathBuf,
    notification_path: PathBuf,
    state: UpdateState,
    notified_generation: Option<String>,
    notification_record_usable: bool,
}

impl UpdateTracker {
    pub fn new(state_path: PathBuf, notification_path: PathBuf) -> Self {
        let state = load_state_or_default(&state_path);
        let (notified_generation, notification_record_usable) =
            match load_notification_record(&notification_path) {
                Ok(record) => (record.map(|record| record.plan_generation), true),
                Err(error) => {
                    eprintln!(
                        "wildbuzzard-shell: update notification record was preserved: {error}"
                    );
                    (None, false)
                }
            };
        Self {
            state_path,
            notification_path,
            state,
            notified_generation,
            notification_record_usable,
        }
    }

    pub fn badge_count(&self) -> Option<usize> {
        match self.state.status {
            UpdateStatus::Available | UpdateStatus::RestartRecommended => {
                Some(self.state.packages.len())
            }
            UpdateStatus::Failed if self.state.repair_available => Some(self.state.packages.len()),
            _ => None,
        }
        .filter(|count| *count > 0)
    }

    pub fn reload(&mut self) -> Result<bool, String> {
        self.reload_with(emit_notification)
    }

    fn reload_with(
        &mut self,
        notify: impl FnOnce(&UpdateNotice) -> Result<(), String>,
    ) -> Result<bool, String> {
        let updated = match fs::symlink_metadata(&self.state_path) {
            Ok(_) => UpdateState::load(&self.state_path)
                .map_err(|error| format!("cannot validate updater state: {error}"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => UpdateState::default(),
            Err(error) => return Err(format!("cannot inspect updater state: {error}")),
        };
        let changed = updated != self.state;
        self.state = updated;
        if self.notification_record_usable
            && self.state.status == UpdateStatus::Available
            && let Some(generation) = self.state.plan_generation.as_ref()
            && self.notified_generation.as_ref() != Some(generation)
        {
            let notice = UpdateNotice {
                generation: generation.clone(),
                package_count: self.state.packages.len(),
            };
            notify(&notice)?;
            let record = NotificationRecord {
                schema_version: NOTIFICATION_SCHEMA_VERSION,
                plan_generation: generation.clone(),
            };
            atomic_write_json(&self.notification_path, &record)
                .map_err(|error| format!("cannot persist update notification record: {error}"))?;
            self.notified_generation = Some(generation.clone());
        }
        Ok(changed)
    }
}

fn load_state_or_default(path: &Path) -> UpdateState {
    match fs::symlink_metadata(path) {
        Ok(_) => match UpdateState::load(path) {
            Ok(state) => state,
            Err(error) => {
                eprintln!("wildbuzzard-shell: updater state was preserved: {error}");
                UpdateState::default()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => UpdateState::default(),
        Err(error) => {
            eprintln!("wildbuzzard-shell: cannot inspect updater state: {error}");
            UpdateState::default()
        }
    }
}

fn load_notification_record(path: &Path) -> Result<Option<NotificationRecord>, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let bytes = read_bounded(path, MAX_NOTIFICATION_RECORD_BYTES)
                .map_err(|error| error.to_string())?;
            let record: NotificationRecord =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            record.validate()?;
            Ok(Some(record))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn emit_notification(notice: &UpdateNotice) -> Result<(), String> {
    let body = if notice.package_count == 1 {
        "1 Debian package update is ready to review in Settings.".to_owned()
    } else {
        format!(
            "{} Debian package updates are ready to review in Settings.",
            notice.package_count
        )
    };
    let child = Command::new("/usr/bin/notify-send")
        .args([
            "--app-name=Wild Buzzard",
            "--icon=software-update-available-symbolic",
            "--urgency=normal",
            "Software Updates Available",
            &body,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot submit Mako update notification: {error}"))?;
    reap_notification_process(child)
}

fn reap_notification_process(child: Child) -> Result<(), String> {
    let child = Arc::new(Mutex::new(Some(child)));
    let waiter = Arc::clone(&child);
    match std::thread::Builder::new()
        .name("wildbuzzard-notification-reaper".into())
        .spawn(move || {
            if let Ok(mut slot) = waiter.lock()
                && let Some(mut child) = slot.take()
            {
                let _ = child.wait();
            }
        }) {
        Ok(_) => Ok(()),
        Err(error) => {
            if let Ok(mut slot) = child.lock()
                && let Some(mut child) = slot.take()
            {
                let _ = child.kill();
                let _ = child.wait();
            }
            Err(format!(
                "cannot supervise the Mako notification process: {error}"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use wildbuzzard_desktop_core::{UpdateAction, UpdatePackage};

    fn available(generation: &str) -> UpdateState {
        let package = UpdatePackage {
            name: "example".into(),
            installed_version: "1".into(),
            candidate_version: "2".into(),
            download_size: 7,
            security_origin: None,
            action: UpdateAction::Upgrade,
        };
        UpdateState {
            status: UpdateStatus::Available,
            checked_at_unix_seconds: Some(1),
            packages: vec![package],
            download_size: 7,
            plan_generation: Some(generation.into()),
            runtime_revision: Some("runtime-1".into()),
            runtime_ready: true,
            ..UpdateState::default()
        }
    }

    #[test]
    fn one_notification_is_emitted_per_exact_plan_generation() {
        let temp = tempfile::tempdir().unwrap();
        let state_path = temp.path().join("state.json");
        let record_path = temp.path().join("notification.json");
        available(&"a".repeat(64)).save(&state_path).unwrap();
        let mut tracker = UpdateTracker::new(state_path.clone(), record_path);
        let calls = Cell::new(0);
        tracker
            .reload_with(|_| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap();
        tracker
            .reload_with(|_| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(calls.get(), 1);
        available(&"b".repeat(64)).save(&state_path).unwrap();
        tracker
            .reload_with(|_| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(calls.get(), 2);
        assert_eq!(tracker.badge_count(), Some(1));
    }

    #[test]
    fn malformed_or_newer_notification_record_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let state_path = temp.path().join("state.json");
        let record_path = temp.path().join("notification.json");
        available(&"a".repeat(64)).save(&state_path).unwrap();
        fs::write(
            &record_path,
            br#"{"schema_version":99,"plan_generation":"x"}"#,
        )
        .unwrap();
        let original = fs::read(&record_path).unwrap();
        let mut tracker = UpdateTracker::new(state_path, record_path.clone());
        tracker.reload_with(|_| panic!("must not notify")).unwrap();
        assert_eq!(fs::read(record_path).unwrap(), original);
    }

    #[test]
    fn badge_clears_when_plan_is_no_longer_available() {
        let temp = tempfile::tempdir().unwrap();
        let state_path = temp.path().join("state.json");
        let record_path = temp.path().join("notification.json");
        available(&"a".repeat(64)).save(&state_path).unwrap();
        let mut tracker = UpdateTracker::new(state_path.clone(), record_path);
        tracker.reload_with(|_| Ok(())).unwrap();
        assert_eq!(tracker.badge_count(), Some(1));
        let state = UpdateState {
            status: UpdateStatus::UpToDate,
            checked_at_unix_seconds: Some(2),
            runtime_revision: Some("runtime-1".into()),
            runtime_ready: true,
            ..UpdateState::default()
        };
        state.save(&state_path).unwrap();
        tracker.reload_with(|_| Ok(())).unwrap();
        assert_eq!(tracker.badge_count(), None);
    }
}
