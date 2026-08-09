// SPDX-License-Identifier: AGPL-3.0-or-later

//! Read-only discovery of host media nodes advertised by PipeWire.
//!
//! Machine metadata stores only a stable PipeWire node name.  Transient
//! object IDs, serials, and physical backend endpoints are resolved again on
//! the host whenever a bridge starts.  None of this data is accepted from the
//! guest or mounted into it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ResourceLocator;

const MAX_PIPEWIRE_DUMP_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HostMediaKind {
    AudioSink,
    Microphone,
    Camera,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostMediaBackend {
    /// The host PipeWire node itself is the only advertised capture route.
    PipeWire,
    /// Physical ALSA endpoint advertised by the PipeWire node. This is
    /// discovery evidence only for microphones: the broker must still capture
    /// their stable PipeWire node through the desktop-accounted Pulse service.
    Alsa { device: String },
    /// Physical Video4Linux endpoint advertised by the PipeWire node.
    V4l2 { device: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMediaDevice {
    /// Stable identity persisted in host-owned machine metadata.
    pub node_name: String,
    pub description: String,
    /// PipeWire serial valid only for the current host session.
    pub serial: String,
    pub kind: HostMediaKind,
    pub backend: HostMediaBackend,
    pub is_default: bool,
}

pub fn discover_host_media(resources: &ResourceLocator) -> Result<Vec<HostMediaDevice>> {
    let pw_dump = resources.helper_or_path("pw-dump")?;
    let output = Command::new(&pw_dump).output().with_context(|| {
        format!(
            "running bundled media discovery helper {}",
            pw_dump.display()
        )
    })?;
    if !output.status.success() {
        bail!(
            "host PipeWire discovery exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.len() > MAX_PIPEWIRE_DUMP_BYTES {
        bail!("host PipeWire discovery exceeded 32 MiB");
    }
    let graph: Value =
        serde_json::from_slice(&output.stdout).context("parsing host PipeWire graph")?;
    devices_from_pipewire_graph(&graph)
}

fn devices_from_pipewire_graph(graph: &Value) -> Result<Vec<HostMediaDevice>> {
    let objects = graph
        .as_array()
        .context("host PipeWire graph is not a JSON array")?;
    let defaults = DefaultNodes::from_graph(objects);
    let mut devices = Vec::new();
    for object in objects {
        if object.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(props) = object.pointer("/info/props").and_then(Value::as_object) else {
            continue;
        };
        let Some(kind) = props
            .get("media.class")
            .and_then(Value::as_str)
            .and_then(kind_from_media_class)
        else {
            continue;
        };
        let Some(node_name) = clean_string(props.get("node.name"), 512) else {
            continue;
        };
        let Some(serial) = scalar_string(props.get("object.serial")) else {
            continue;
        };
        let description = (kind == HostMediaKind::Camera)
            .then(|| clean_string(props.get("api.v4l2.cap.card"), 512))
            .flatten()
            .or_else(|| clean_string(props.get("node.description"), 512))
            .or_else(|| clean_string(props.get("node.nick"), 512))
            .unwrap_or_else(|| node_name.clone());
        let backend = match kind {
            HostMediaKind::AudioSink => HostMediaBackend::PipeWire,
            HostMediaKind::Microphone => clean_string(props.get("api.alsa.path"), 256)
                .filter(|device| valid_alsa_device(device))
                .map(|device| HostMediaBackend::Alsa { device })
                .unwrap_or(HostMediaBackend::PipeWire),
            HostMediaKind::Camera => clean_string(props.get("api.v4l2.path"), 4096)
                .map(PathBuf::from)
                .filter(|device| valid_video_device(device))
                .map(|device| HostMediaBackend::V4l2 { device })
                .unwrap_or(HostMediaBackend::PipeWire),
        };
        let is_default = defaults.for_kind(kind) == Some(node_name.as_str());
        devices.push(HostMediaDevice {
            node_name,
            description,
            serial,
            kind,
            backend,
            is_default,
        });
    }
    devices.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| right.is_default.cmp(&left.is_default))
            .then_with(|| left.description.cmp(&right.description))
            .then_with(|| left.node_name.cmp(&right.node_name))
    });
    Ok(devices)
}

fn kind_from_media_class(class: &str) -> Option<HostMediaKind> {
    match class {
        "Audio/Sink" => Some(HostMediaKind::AudioSink),
        "Audio/Source" => Some(HostMediaKind::Microphone),
        "Video/Source" => Some(HostMediaKind::Camera),
        _ => None,
    }
}

fn clean_string(value: Option<&Value>, maximum: usize) -> Option<String> {
    let value = value?.as_str()?;
    if value.is_empty()
        || value.len() > maximum
        || value.chars().any(|character| character.is_control())
    {
        return None;
    }
    Some(value.to_owned())
}

fn scalar_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value)
            if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            Some(value.clone())
        }
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn valid_alsa_device(device: &str) -> bool {
    device.starts_with("hw:")
        && device.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b':' | b',' | b'.' | b'_' | b'-')
        })
}

fn valid_video_device(device: &Path) -> bool {
    if device.parent() != Some(Path::new("/dev"))
        || !device
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.strip_prefix("video").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                })
            })
    {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(device) else {
        return false;
    };
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    !metadata.file_type().is_symlink()
        && metadata.file_type().is_char_device()
        && libc::major(metadata.rdev()) == 81
}

#[derive(Default)]
struct DefaultNodes {
    audio_sink: Option<String>,
    microphone: Option<String>,
    camera: Option<String>,
}

impl DefaultNodes {
    fn from_graph(objects: &[Value]) -> Self {
        let mut defaults = Self::default();
        for object in objects {
            if object.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Metadata")
                || object
                    .pointer("/props/metadata.name")
                    .and_then(Value::as_str)
                    != Some("default")
            {
                continue;
            }
            let Some(entries) = object.get("metadata").and_then(Value::as_array) else {
                continue;
            };
            for entry in entries {
                let Some(key) = entry.get("key").and_then(Value::as_str) else {
                    continue;
                };
                let name = entry.get("value").and_then(|value| match value {
                    Value::Object(value) => {
                        value.get("name").and_then(Value::as_str).map(str::to_owned)
                    }
                    Value::String(value) => {
                        serde_json::from_str::<Value>(value).ok().and_then(|value| {
                            value.get("name").and_then(Value::as_str).map(str::to_owned)
                        })
                    }
                    _ => None,
                });
                match key {
                    "default.audio.sink" => defaults.audio_sink = name,
                    "default.audio.source" => defaults.microphone = name,
                    "default.video.source" => defaults.camera = name,
                    _ => {}
                }
            }
        }
        defaults
    }

    fn for_kind(&self, kind: HostMediaKind) -> Option<&str> {
        match kind {
            HostMediaKind::AudioSink => self.audio_sink.as_deref(),
            HostMediaKind::Microphone => self.microphone.as_deref(),
            HostMediaKind::Camera => self.camera.as_deref(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_defaults_and_physical_backends() {
        let graph = serde_json::json!([
            {
                "type": "PipeWire:Interface:Metadata",
                "props": {"metadata.name": "default"},
                "metadata": [
                    {"key": "default.audio.source", "value": {"name": "mic.one"}},
                    {"key": "default.video.source", "value": {"name": "camera.one"}}
                ]
            },
            {
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Audio/Source",
                    "node.name": "mic.one",
                    "node.description": "Built-in microphone",
                    "object.serial": 42,
                    "api.alsa.path": "hw:Card_1,6"
                }}
            },
            {
                "type": "PipeWire:Interface:Node",
                "info": {"props": {
                    "media.class": "Video/Source",
                    "node.name": "camera.one",
                    "object.serial": "43"
                }}
            }
        ]);
        let devices = devices_from_pipewire_graph(&graph).unwrap();
        assert_eq!(devices.len(), 2);
        assert!(devices[0].is_default);
        assert!(matches!(devices[0].backend, HostMediaBackend::Alsa { .. }));
        assert!(devices[1].is_default);
        assert_eq!(devices[1].backend, HostMediaBackend::PipeWire);
    }

    #[test]
    fn rejects_untrusted_backend_values() {
        assert!(!valid_alsa_device("hw:card;touch /tmp/x"));
        assert!(!valid_alsa_device("plughw:1,0"));
        assert!(!valid_video_device(Path::new("/dev/../etc/passwd")));
        assert!(!valid_video_device(Path::new("/tmp/video0")));
    }
}
