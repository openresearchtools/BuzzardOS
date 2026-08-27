// SPDX-License-Identifier: AGPL-3.0-or-later

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WaylandCapabilities {
    pub linux_dmabuf: bool,
    /// Highest zwp_linux_dmabuf_v1 version advertised by the host.
    ///
    /// Version 4 adds feedback and a main-device identity.  Version 3 hosts
    /// remain fully capable of importing dma-bufs, but do not identify their
    /// renderer through that feedback protocol.
    #[serde(default)]
    pub linux_dmabuf_version: u32,
    pub explicit_sync: bool,
    #[serde(default)]
    pub explicit_sync_protocols: Vec<String>,
    pub presentation_time: bool,
    pub dmabuf_main_device: Option<u64>,
    #[serde(default)]
    pub server_side_decorations: bool,
    #[serde(default)]
    pub fractional_scale: bool,
    #[serde(default)]
    pub viewporter: bool,
    #[serde(default)]
    pub color_management: bool,
    #[serde(default)]
    pub color_representation: bool,
    /// Complete host registry inventory, including globals unknown to this
    /// release. Keeping their highest advertised versions makes capability
    /// omissions visible instead of silently reducing the desktop contract.
    #[serde(default)]
    pub globals: BTreeMap<String, u32>,
}

impl WaylandCapabilities {
    pub fn probe(socket: &Path) -> Result<Self> {
        let mut stream = UnixStream::connect(socket)
            .with_context(|| format!("connecting to host Wayland socket {}", socket.display()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .context("setting Wayland probe timeout")?;

        send_request(&mut stream, 1, 1, &2_u32.to_ne_bytes())?;
        send_request(&mut stream, 1, 0, &3_u32.to_ne_bytes())?;

        let mut capabilities = Self::default();
        let mut dmabuf_global = None;
        loop {
            let (object, opcode, payload) = read_event(&mut stream)?;
            if object == 2 && opcode == 0 {
                if let Some((name, interface, version)) = registry_global(&payload) {
                    capabilities
                        .globals
                        .entry(interface.to_owned())
                        .and_modify(|current| *current = (*current).max(version))
                        .or_insert(version);
                    match interface {
                        "zwp_linux_dmabuf_v1" => {
                            capabilities.linux_dmabuf = true;
                            capabilities.linux_dmabuf_version =
                                capabilities.linux_dmabuf_version.max(version);
                            dmabuf_global = Some((name, version));
                        }
                        "zwp_linux_explicit_synchronization_v1"
                        | "wp_linux_drm_syncobj_manager_v1" => {
                            capabilities.explicit_sync = true;
                            capabilities
                                .explicit_sync_protocols
                                .push(interface.to_owned());
                        }
                        "wp_presentation" => capabilities.presentation_time = true,
                        "zxdg_decoration_manager_v1" => {
                            capabilities.server_side_decorations = true;
                        }
                        "wp_fractional_scale_manager_v1" => {
                            capabilities.fractional_scale = true;
                        }
                        "wp_viewporter" => capabilities.viewporter = true,
                        "wp_color_manager_v1" => capabilities.color_management = true,
                        "wp_color_representation_manager_v1" => {
                            capabilities.color_representation = true;
                        }
                        _ => {}
                    }
                }
            } else if object == 3 && opcode == 0 {
                break;
            } else if object == 1 && opcode == 0 {
                bail!("host Wayland compositor rejected the capability probe");
            }
        }

        if let Some((name, version)) = dmabuf_global.filter(|(_, version)| *version >= 4) {
            capabilities.dmabuf_main_device =
                probe_dmabuf_main_device(&mut stream, name, version.min(4))?;
        }
        Ok(capabilities)
    }
}

fn probe_dmabuf_main_device(
    stream: &mut UnixStream,
    global_name: u32,
    version: u32,
) -> Result<Option<u64>> {
    let mut bind = Vec::new();
    bind.extend_from_slice(&global_name.to_ne_bytes());
    append_string(&mut bind, "zwp_linux_dmabuf_v1");
    bind.extend_from_slice(&version.to_ne_bytes());
    bind.extend_from_slice(&4_u32.to_ne_bytes());
    send_request(stream, 2, 0, &bind)?;
    send_request(stream, 4, 2, &5_u32.to_ne_bytes())?;

    let mut main_device = None;
    loop {
        let (object, opcode, payload) = read_event(stream)?;
        if object == 5 && opcode == 2 {
            let length = u32::from_ne_bytes(
                payload
                    .get(..4)
                    .context("invalid main_device event")?
                    .try_into()
                    .expect("four bytes"),
            ) as usize;
            let value = payload
                .get(
                    4..4_usize
                        .checked_add(length)
                        .context("invalid device length")?,
                )
                .context("truncated main_device event")?;
            if let Some(bytes) = value.get(..8) {
                main_device = Some(u64::from_ne_bytes(bytes.try_into().expect("eight bytes")));
            }
        } else if object == 5 && opcode == 0 {
            return Ok(main_device);
        } else if object == 1 && opcode == 0 {
            bail!("host Wayland compositor rejected the dmabuf feedback probe");
        }
    }
}

fn send_request(stream: &mut UnixStream, object: u32, opcode: u16, payload: &[u8]) -> Result<()> {
    let size = 8_usize
        .checked_add(payload.len())
        .context("Wayland request is too large")?;
    let size = u16::try_from(size).context("Wayland request is too large")?;
    stream.write_all(&object.to_ne_bytes())?;
    stream.write_all(&(((u32::from(size)) << 16) | u32::from(opcode)).to_ne_bytes())?;
    stream.write_all(payload)?;
    Ok(())
}

fn read_event(stream: &mut UnixStream) -> Result<(u32, u16, Vec<u8>)> {
    let mut header = [0_u8; 8];
    stream
        .read_exact(&mut header)
        .context("reading host Wayland event header")?;
    let object = u32::from_ne_bytes(header[..4].try_into().expect("four bytes"));
    let size_and_opcode = u32::from_ne_bytes(header[4..].try_into().expect("four bytes"));
    let size = (size_and_opcode >> 16) as usize;
    let opcode = (size_and_opcode & 0xffff) as u16;
    if size < 8 || size % 4 != 0 {
        bail!("host Wayland compositor sent an invalid event size");
    }
    let mut payload = vec![0_u8; size - 8];
    stream
        .read_exact(&mut payload)
        .context("reading host Wayland event")?;
    Ok((object, opcode, payload))
}

fn append_string(payload: &mut Vec<u8>, value: &str) {
    let length = value.len() + 1;
    payload.extend_from_slice(&(length as u32).to_ne_bytes());
    payload.extend_from_slice(value.as_bytes());
    payload.push(0);
    let padded = (length + 3) & !3;
    payload.resize(payload.len() + padded - length, 0);
}

fn registry_global(payload: &[u8]) -> Option<(u32, &str, u32)> {
    let name = u32::from_ne_bytes(payload.get(..4)?.try_into().ok()?);
    let length = u32::from_ne_bytes(payload.get(4..8)?.try_into().ok()?) as usize;
    if length == 0 {
        return None;
    }
    let value = payload.get(8..8_usize.checked_add(length)?)?;
    let value = value.strip_suffix(&[0]).unwrap_or(value);
    let interface = std::str::from_utf8(value).ok()?;
    let padded = (length + 3) & !3;
    let version = u32::from_ne_bytes(payload.get(8 + padded..12 + padded)?.try_into().ok()?);
    Some((name, interface, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_global() {
        let interface = b"zwp_linux_dmabuf_v1\0";
        let padded = (interface.len() + 3) & !3;
        let mut payload = Vec::new();
        payload.extend_from_slice(&17_u32.to_ne_bytes());
        payload.extend_from_slice(&(interface.len() as u32).to_ne_bytes());
        payload.extend_from_slice(interface);
        payload.resize(8 + padded, 0);
        payload.extend_from_slice(&5_u32.to_ne_bytes());
        assert_eq!(
            registry_global(&payload),
            Some((17, "zwp_linux_dmabuf_v1", 5))
        );
    }

    #[test]
    fn rejects_empty_registry_interface() {
        let mut payload = vec![0_u8; 12];
        payload[4..8].copy_from_slice(&0_u32.to_ne_bytes());
        assert_eq!(registry_global(&payload), None);
    }

    #[test]
    fn default_inventory_is_empty_and_color_capabilities_are_explicit() {
        let capabilities = WaylandCapabilities::default();
        assert!(capabilities.globals.is_empty());
        assert_eq!(capabilities.linux_dmabuf_version, 0);
        assert!(!capabilities.color_management);
        assert!(!capabilities.color_representation);
    }
}
