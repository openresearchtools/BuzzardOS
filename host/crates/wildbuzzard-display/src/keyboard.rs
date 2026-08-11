// SPDX-License-Identifier: AGPL-3.0-or-later

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use xkbcommon::xkb;

const TOKEN_BYTES: usize = 32;
const DIGEST_BYTES: usize = 64;
const MAX_MODEL_BYTES: usize = 64;
const MAX_LAYOUT_BYTES: usize = 256;
const MAX_VARIANT_BYTES: usize = 256;
const MAX_OPTIONS_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyboardMapSpec {
    pub(crate) model: String,
    pub(crate) layout: String,
    pub(crate) variant: String,
    pub(crate) options: String,
}

impl KeyboardMapSpec {
    pub(crate) fn standard_us() -> Self {
        Self {
            model: "pc105".into(),
            layout: "us".into(),
            variant: String::new(),
            options: String::new(),
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        validate_component("model", &self.model, 1, MAX_MODEL_BYTES)?;
        validate_component("layout", &self.layout, 1, MAX_LAYOUT_BYTES)?;
        validate_component("variant", &self.variant, 0, MAX_VARIANT_BYTES)?;
        validate_component("options", &self.options, 0, MAX_OPTIONS_BYTES)?;

        let layouts = self.layout.split(',').count();
        if layouts > 4 || self.layout.split(',').any(str::is_empty) {
            bail!("keyboard layout must contain between one and four non-empty groups");
        }
        if !self.variant.is_empty() {
            let variants = self.variant.split(',').count();
            if variants > 4 {
                bail!("keyboard variant must contain at most four comma-aligned groups");
            }
            if variants > layouts {
                bail!("keyboard variant defines more groups than keyboard layout");
            }
        }
        if !self.options.is_empty() && self.options.split(',').any(str::is_empty) {
            bail!("keyboard options contain an empty option");
        }
        Ok(())
    }
}

fn validate_component(name: &str, value: &str, minimum: usize, maximum: usize) -> Result<()> {
    let bytes = value.as_bytes();
    if !(minimum..=maximum).contains(&bytes.len()) {
        bail!("keyboard {name} must contain {minimum}..={maximum} ASCII bytes");
    }
    if !bytes.iter().copied().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b',' | b':' | b'.')
    }) {
        bail!("keyboard {name} contains unsupported characters");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) enum KeyboardMapMethod {
    #[serde(rename = "PrepareKeyboardMap")]
    Prepare,
    #[serde(rename = "StatusKeyboardMap")]
    Status,
    #[serde(rename = "CommitKeyboardMap")]
    Commit,
    #[serde(rename = "AbortKeyboardMap")]
    Abort,
}

impl KeyboardMapMethod {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "PrepareKeyboardMap" => Some(Self::Prepare),
            "StatusKeyboardMap" => Some(Self::Status),
            "CommitKeyboardMap" => Some(Self::Commit),
            "AbortKeyboardMap" => Some(Self::Abort),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyboardMapRequest {
    Prepare {
        token: String,
        spec: KeyboardMapSpec,
        keymap_sha256: String,
    },
    Status {
        token: String,
    },
    Commit {
        token: String,
        keymap_sha256: String,
    },
    Abort {
        token: String,
        keymap_sha256: String,
    },
}

impl KeyboardMapRequest {
    pub(crate) fn method(&self) -> KeyboardMapMethod {
        match self {
            Self::Prepare { .. } => KeyboardMapMethod::Prepare,
            Self::Status { .. } => KeyboardMapMethod::Status,
            Self::Commit { .. } => KeyboardMapMethod::Commit,
            Self::Abort { .. } => KeyboardMapMethod::Abort,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareRequest {
    schema: u32,
    method: String,
    token: String,
    model: String,
    layout: String,
    variant: String,
    options: String,
    keymap_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusRequest {
    schema: u32,
    method: String,
    token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishRequest {
    schema: u32,
    method: String,
    token: String,
    keymap_sha256: String,
}

pub(crate) fn parse_request(value: serde_json::Value) -> Result<KeyboardMapRequest> {
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .and_then(KeyboardMapMethod::parse)
        .context("unsupported keyboard-map method")?;
    let request = match method {
        KeyboardMapMethod::Prepare => {
            let request: PrepareRequest =
                serde_json::from_value(value).context("parsing keyboard-map prepare request")?;
            require_schema(request.schema)?;
            if request.method != "PrepareKeyboardMap" {
                bail!("keyboard-map method changed while parsing");
            }
            validate_token(&request.token)?;
            validate_digest(&request.keymap_sha256)?;
            let spec = KeyboardMapSpec {
                model: request.model,
                layout: request.layout,
                variant: request.variant,
                options: request.options,
            };
            spec.validate()?;
            KeyboardMapRequest::Prepare {
                token: request.token,
                spec,
                keymap_sha256: request.keymap_sha256,
            }
        }
        KeyboardMapMethod::Status => {
            let request: StatusRequest =
                serde_json::from_value(value).context("parsing keyboard-map status request")?;
            require_schema(request.schema)?;
            if request.method != "StatusKeyboardMap" {
                bail!("keyboard-map method changed while parsing");
            }
            validate_token(&request.token)?;
            KeyboardMapRequest::Status {
                token: request.token,
            }
        }
        KeyboardMapMethod::Commit | KeyboardMapMethod::Abort => {
            let request: FinishRequest =
                serde_json::from_value(value).context("parsing keyboard-map finish request")?;
            require_schema(request.schema)?;
            validate_token(&request.token)?;
            validate_digest(&request.keymap_sha256)?;
            if request.method == "CommitKeyboardMap" {
                KeyboardMapRequest::Commit {
                    token: request.token,
                    keymap_sha256: request.keymap_sha256,
                }
            } else if request.method == "AbortKeyboardMap" {
                KeyboardMapRequest::Abort {
                    token: request.token,
                    keymap_sha256: request.keymap_sha256,
                }
            } else {
                bail!("keyboard-map method changed while parsing");
            }
        }
    };
    Ok(request)
}

fn require_schema(schema: u32) -> Result<()> {
    if schema != 1 {
        bail!("unsupported keyboard-map schema {schema}");
    }
    Ok(())
}

fn validate_token(token: &str) -> Result<()> {
    if token.len() != TOKEN_BYTES
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("keyboard-map token must be 32 lowercase hexadecimal characters");
    }
    Ok(())
}

fn validate_digest(digest: &str) -> Result<()> {
    if digest.len() != DIGEST_BYTES
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("keyboard-map digest must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum KeyboardMapState {
    Prepared,
    Committed,
    Aborted,
    Unknown,
}

#[derive(Debug, Serialize)]
pub(crate) struct KeyboardMapResponse {
    schema: u32,
    ok: bool,
    method: KeyboardMapMethod,
    state: KeyboardMapState,
    active_keymap_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_keymap_sha256: Option<String>,
}

#[derive(Debug)]
pub(crate) struct KeyboardMapFailure {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl KeyboardMapFailure {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub(crate) type KeyboardMapReply = std::result::Result<KeyboardMapResponse, KeyboardMapFailure>;

impl KeyboardMapResponse {
    pub(crate) fn success(
        method: KeyboardMapMethod,
        state: KeyboardMapState,
        active_keymap_sha256: String,
        pending: Option<(&str, &str)>,
    ) -> Self {
        Self {
            schema: 1,
            ok: true,
            method,
            state,
            active_keymap_sha256,
            pending_token: pending.map(|(token, _)| token.to_owned()),
            pending_keymap_sha256: pending.map(|(_, digest)| digest.to_owned()),
        }
    }
}

pub(crate) struct CompiledKeymap {
    pub(crate) fd: OwnedFd,
    pub(crate) size: u32,
    pub(crate) state: xkb::State,
    pub(crate) digest: String,
    keymap: xkb::Keymap,
}

impl CompiledKeymap {
    pub(crate) fn compile(config_root: &Path, spec: &KeyboardMapSpec) -> Result<Self> {
        spec.validate()?;
        let mut context = xkb::Context::new(xkb::CONTEXT_NO_DEFAULT_INCLUDES);
        if !context.include_path_append(config_root) {
            bail!(
                "libxkbcommon rejected bundled XKB config root {}",
                config_root.display()
            );
        }
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            &spec.model,
            &spec.layout,
            &spec.variant,
            if spec.options.is_empty() {
                Some(String::new())
            } else {
                Some(spec.options.clone())
            },
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow!("compiling requested XKB keymap from bundled definitions"))?;
        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        if text.is_empty() || text.as_bytes().contains(&0) {
            bail!("libxkbcommon produced an invalid serialized keymap");
        }
        let digest = format!("{:x}", Sha256::digest(text.as_bytes()));
        let mut protocol_text = text;
        protocol_text.push('\0');
        let name = b"wildbuzzard-keymap\0";
        // SAFETY: the name is NUL-terminated and flags are valid.
        let raw = unsafe {
            libc::memfd_create(
                name.as_ptr().cast(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error()).context("creating XKB keymap memfd");
        }
        // SAFETY: memfd_create returned a new owned descriptor.
        let mut file = unsafe { File::from_raw_fd(raw) };
        file.write_all(protocol_text.as_bytes())
            .context("writing XKB keymap")?;
        file.seek(SeekFrom::Start(0))
            .context("rewinding XKB keymap")?;
        let seals =
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
        // SAFETY: `file` owns a live memfd created with MFD_ALLOW_SEALING and
        // the third argument is the documented F_ADD_SEALS bitmask.
        if unsafe {
            libc::fcntl(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::F_ADD_SEALS,
                seals,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error()).context("sealing XKB keymap memfd");
        }
        let size =
            u32::try_from(protocol_text.len()).context("serialized XKB keymap is too large")?;
        let state = xkb::State::new(&keymap);
        Ok(Self {
            fd: file.into(),
            size,
            state,
            digest,
            keymap,
        })
    }

    pub(crate) fn reset_state(&mut self) {
        self.state = xkb::State::new(&self.keymap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xkb_root() -> &'static Path {
        let root = Path::new("/usr/share/X11/xkb");
        assert!(
            root.join("rules/evdev").exists(),
            "test XKB data is missing"
        );
        root
    }

    #[test]
    fn parser_rejects_unknown_fields_commands_paths_and_uppercase_ids() {
        let valid = serde_json::json!({
            "schema": 1,
            "method": "PrepareKeyboardMap",
            "token": "0123456789abcdef0123456789abcdef",
            "model": "pc105",
            "layout": "us,gb",
            "variant": "intl,",
            "options": "grp:alt_shift_toggle",
            "keymap_sha256": "a".repeat(64),
        });
        assert!(matches!(
            parse_request(valid),
            Ok(KeyboardMapRequest::Prepare { .. })
        ));

        for bad in [
            serde_json::json!({
                "schema": 1, "method": "RunCommand", "token": "0".repeat(32)
            }),
            serde_json::json!({
                "schema": 1, "method": "StatusKeyboardMap",
                "token": "0".repeat(32), "command": "sh"
            }),
            serde_json::json!({
                "schema": 1, "method": "PrepareKeyboardMap",
                "token": "0".repeat(32), "model": "../../etc", "layout": "us",
                "variant": "", "options": "", "keymap_sha256": "a".repeat(64)
            }),
            serde_json::json!({
                "schema": 1, "method": "StatusKeyboardMap", "token": "A".repeat(32)
            }),
        ] {
            assert!(parse_request(bad).is_err());
        }
    }

    #[test]
    fn shared_manually_authored_keyboard_contract_matches_host_parser() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../tests/fixtures/xkb-settings-contract.json"
        ))
        .unwrap();
        assert_eq!(fixture["schema"], 1);
        for case in fixture["cases"].as_array().unwrap() {
            let keyboard = case["keyboard"].as_object().unwrap();
            let request = serde_json::json!({
                "schema": 1,
                "method": "PrepareKeyboardMap",
                "token": "0123456789abcdef0123456789abcdef",
                "model": keyboard["model"],
                "layout": keyboard["layout"],
                "variant": keyboard["variant"],
                "options": keyboard["options"],
                "keymap_sha256": "a".repeat(64),
            });
            assert_eq!(
                parse_request(request).is_ok(),
                case["valid"].as_bool().unwrap(),
                "shared XKB contract case {}",
                case["name"].as_str().unwrap()
            );
        }
    }

    #[test]
    fn keyboard_component_byte_bounds_are_exact() {
        for (field, maximum) in [
            ("model", MAX_MODEL_BYTES),
            ("layout", MAX_LAYOUT_BYTES),
            ("variant", MAX_VARIANT_BYTES),
            ("options", MAX_OPTIONS_BYTES),
        ] {
            let mut spec = KeyboardMapSpec::standard_us();
            match field {
                "model" => spec.model = "a".repeat(maximum),
                "layout" => spec.layout = "a".repeat(maximum),
                "variant" => spec.variant = "a".repeat(maximum),
                "options" => spec.options = "a".repeat(maximum),
                _ => unreachable!(),
            }
            spec.validate().unwrap();
            match field {
                "model" => spec.model.push('a'),
                "layout" => spec.layout.push('a'),
                "variant" => spec.variant.push('a'),
                "options" => spec.options.push('a'),
                _ => unreachable!(),
            }
            assert!(
                spec.validate().is_err(),
                "{field} accepted an oversized value"
            );
        }
    }

    #[test]
    fn german_altgr_uses_mod5_and_level_three() {
        let mut keymap = CompiledKeymap::compile(
            xkb_root(),
            &KeyboardMapSpec {
                model: "pc105".into(),
                layout: "de".into(),
                variant: String::new(),
                options: String::new(),
            },
        )
        .unwrap();
        // evdev KEY_RIGHTALT=100 and KEY_Q=16; XKB keycodes add eight.
        keymap
            .state
            .update_key(xkb::Keycode::new(108), xkb::KeyDirection::Down);
        assert!(
            keymap
                .state
                .mod_name_is_active(xkb::MOD_NAME_ISO_LEVEL3_SHIFT, xkb::STATE_MODS_EFFECTIVE)
        );
        assert_eq!(keymap.state.key_get_utf8(xkb::Keycode::new(24)), "@");
    }

    #[test]
    fn alt_shift_switches_the_us_gb_layout_group() {
        let mut keymap = CompiledKeymap::compile(
            xkb_root(),
            &KeyboardMapSpec {
                model: "pc105".into(),
                layout: "us,gb".into(),
                variant: String::new(),
                options: "grp:alt_shift_toggle".into(),
            },
        )
        .unwrap();
        assert_eq!(
            keymap.state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
            0
        );
        // evdev KEY_LEFTALT=56 and KEY_LEFTSHIFT=42; XKB adds eight.
        keymap
            .state
            .update_key(xkb::Keycode::new(64), xkb::KeyDirection::Down);
        keymap
            .state
            .update_key(xkb::Keycode::new(50), xkb::KeyDirection::Down);
        assert_eq!(
            keymap.state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
            1
        );
    }

    #[test]
    fn comma_aligned_empty_variant_slot_compiles() {
        let keymap = CompiledKeymap::compile(
            xkb_root(),
            &KeyboardMapSpec {
                model: "pc105".into(),
                layout: "us,de".into(),
                variant: ",nodeadkeys".into(),
                options: "grp:alt_shift_toggle".into(),
            },
        )
        .unwrap();
        assert_eq!(keymap.digest.len(), DIGEST_BYTES);
    }

    #[test]
    fn serialized_keymap_memfd_is_permanently_read_only() {
        let keymap = CompiledKeymap::compile(xkb_root(), &KeyboardMapSpec::standard_us()).unwrap();
        // SAFETY: F_GET_SEALS does not modify the live memfd.
        let seals = unsafe {
            libc::fcntl(
                std::os::fd::AsRawFd::as_raw_fd(&keymap.fd),
                libc::F_GET_SEALS,
            )
        };
        assert_eq!(
            seals
                & (libc::F_SEAL_SHRINK
                    | libc::F_SEAL_GROW
                    | libc::F_SEAL_WRITE
                    | libc::F_SEAL_SEAL),
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL
        );
    }
}
