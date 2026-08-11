// SPDX-License-Identifier: AGPL-3.0-or-later

//! Fixed one-shot clipboard transport shared by the native host application
//! and the in-guest Sway clipboard agent.
//!
//! This protocol transports already-authorized bytes. It deliberately has no
//! operation for reading the host clipboard, subscribing to clipboard changes,
//! naming paths, or executing commands.

use std::fmt;
use std::io::{self, Read, Write};

pub const VERSION: u8 = 1;
pub const TEXT_MIME: &str = "text/plain;charset=utf-8";
pub const PNG_MIME: &str = "image/png";
pub const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_IMAGE_DIMENSION: u32 = 8192;
pub const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;
pub const IO_TIMEOUT_SECONDS: u64 = 5;

const MAGIC: [u8; 8] = *b"WBCLIP01";
const HEADER_LEN: usize = 36;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Put = 1,
    Get = 2,
    Probe = 3,
    PutResult = 129,
    GetResult = 130,
    ProbeResult = 131,
}

impl TryFrom<u8> for Kind {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Put),
            2 => Ok(Self::Get),
            3 => Ok(Self::Probe),
            129 => Ok(Self::PutResult),
            130 => Ok(Self::GetResult),
            131 => Ok(Self::ProbeResult),
            _ => Err(ProtocolError::InvalidKind(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mime {
    None = 0,
    Text = 1,
    Png = 2,
}

impl Mime {
    pub fn canonical(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Text => Some(TEXT_MIME),
            Self::Png => Some(PNG_MIME),
        }
    }

    pub fn payload_limit(self) -> usize {
        match self {
            Self::None => 0,
            Self::Text => MAX_TEXT_BYTES,
            Self::Png => MAX_IMAGE_BYTES,
        }
    }
}

impl TryFrom<u8> for Mime {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Text),
            2 => Ok(Self::Png),
            _ => Err(ProtocolError::InvalidMime(value)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Ok = 0,
    InvalidRequest = 1,
    UnsupportedMime = 2,
    TooLarge = 3,
    InvalidContent = 4,
    ClipboardUnavailable = 5,
    Timeout = 6,
    Busy = 7,
    Internal = 8,
}

impl Status {
    pub fn code(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::InvalidRequest => "invalid_request",
            Self::UnsupportedMime => "unsupported_mime",
            Self::TooLarge => "too_large",
            Self::InvalidContent => "invalid_content",
            Self::ClipboardUnavailable => "clipboard_unavailable",
            Self::Timeout => "timeout",
            Self::Busy => "busy",
            Self::Internal => "internal",
        }
    }
}

impl TryFrom<u8> for Status {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Ok),
            1 => Ok(Self::InvalidRequest),
            2 => Ok(Self::UnsupportedMime),
            3 => Ok(Self::TooLarge),
            4 => Ok(Self::InvalidContent),
            5 => Ok(Self::ClipboardUnavailable),
            6 => Ok(Self::Timeout),
            7 => Ok(Self::Busy),
            8 => Ok(Self::Internal),
            _ => Err(ProtocolError::InvalidStatus(value)),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub mime: Mime,
    pub status: Status,
    pub nonce: [u8; 16],
    pub payload: Vec<u8>,
}

impl Drop for Frame {
    fn drop(&mut self) {
        self.nonce.fill(0);
        self.payload.fill(0);
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Clipboard content is privacy-sensitive and must never appear in a
        // debug/error log. Only emit its bounded length.
        formatter
            .debug_struct("Frame")
            .field("kind", &self.kind)
            .field("mime", &self.mime)
            .field("status", &self.status)
            .field("nonce", &"[redacted]")
            .field("payload_bytes", &self.payload.len())
            .finish()
    }
}

impl Frame {
    pub fn put(nonce: [u8; 16], mime: Mime, payload: Vec<u8>) -> Result<Self, ProtocolError> {
        let frame = Self {
            kind: Kind::Put,
            mime,
            status: Status::Ok,
            nonce,
            payload,
        };
        frame.validate_shape()?;
        Ok(frame)
    }

    pub fn get(nonce: [u8; 16]) -> Self {
        Self::empty(Kind::Get, Status::Ok, nonce)
    }

    pub fn probe(nonce: [u8; 16]) -> Self {
        Self::empty(Kind::Probe, Status::Ok, nonce)
    }

    pub fn result(
        kind: Kind,
        nonce: [u8; 16],
        status: Status,
        mime: Mime,
        payload: Vec<u8>,
    ) -> Result<Self, ProtocolError> {
        let frame = Self {
            kind,
            mime,
            status,
            nonce,
            payload,
        };
        frame.validate_shape()?;
        Ok(frame)
    }

    pub fn error(kind: Kind, nonce: [u8; 16], status: Status) -> Self {
        debug_assert!(status != Status::Ok);
        Self {
            kind,
            mime: Mime::None,
            status,
            nonce,
            payload: Vec::new(),
        }
    }

    fn empty(kind: Kind, status: Status, nonce: [u8; 16]) -> Self {
        Self {
            kind,
            mime: Mime::None,
            status,
            nonce,
            payload: Vec::new(),
        }
    }

    /// Moves the sensitive payload out while leaving a zero-length value for
    /// this frame's wiping destructor. The returned bytes remain the caller's
    /// responsibility and should be kept in a wiping owner.
    pub fn take_payload(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.payload)
    }

    pub fn validate_shape(&self) -> Result<(), ProtocolError> {
        if self.payload.len() > self.mime.payload_limit() {
            return Err(ProtocolError::PayloadTooLarge {
                actual: self.payload.len(),
                limit: self.mime.payload_limit(),
            });
        }
        let request = matches!(self.kind, Kind::Put | Kind::Get | Kind::Probe);
        if request && self.status != Status::Ok {
            return Err(ProtocolError::InvalidShape("requests must use status Ok"));
        }
        match self.kind {
            Kind::Put => {
                if !matches!(self.mime, Mime::Text | Mime::Png) {
                    return Err(ProtocolError::InvalidShape("Put requires text or PNG"));
                }
            }
            Kind::Get | Kind::Probe => {
                if self.mime != Mime::None || !self.payload.is_empty() {
                    return Err(ProtocolError::InvalidShape(
                        "Get/Probe cannot carry content",
                    ));
                }
            }
            Kind::PutResult | Kind::ProbeResult => {
                if self.mime != Mime::None || !self.payload.is_empty() {
                    return Err(ProtocolError::InvalidShape(
                        "Put/Probe result cannot carry content",
                    ));
                }
            }
            Kind::GetResult if self.status == Status::Ok => {
                if !matches!(self.mime, Mime::Text | Mime::Png) {
                    return Err(ProtocolError::InvalidShape(
                        "successful Get result requires text or PNG",
                    ));
                }
            }
            Kind::GetResult => {
                if self.mime != Mime::None || !self.payload.is_empty() {
                    return Err(ProtocolError::InvalidShape(
                        "failed Get result cannot carry content",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    BadMagic,
    UnsupportedVersion(u8),
    InvalidKind(u8),
    InvalidMime(u8),
    InvalidStatus(u8),
    InvalidShape(&'static str),
    PayloadTooLarge { actual: usize, limit: usize },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "clipboard transport I/O failed: {error}"),
            Self::BadMagic => formatter.write_str("clipboard frame magic is invalid"),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "clipboard protocol version {version} is unsupported"
                )
            }
            Self::InvalidKind(kind) => write!(formatter, "clipboard frame kind {kind} is invalid"),
            Self::InvalidMime(mime) => write!(formatter, "clipboard MIME code {mime} is invalid"),
            Self::InvalidStatus(status) => {
                write!(formatter, "clipboard status code {status} is invalid")
            }
            Self::InvalidShape(reason) => {
                write!(formatter, "clipboard frame shape is invalid: {reason}")
            }
            Self::PayloadTooLarge { actual, limit } => write!(
                formatter,
                "clipboard payload length {actual} exceeds its {limit}-byte limit"
            ),
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ProtocolError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn write_frame(mut writer: impl Write, frame: &Frame) -> Result<(), ProtocolError> {
    frame.validate_shape()?;
    let mut header = WipingArray([0_u8; HEADER_LEN]);
    header.0[..8].copy_from_slice(&MAGIC);
    header.0[8] = VERSION;
    header.0[9] = frame.kind as u8;
    header.0[10] = frame.mime as u8;
    header.0[11] = frame.status as u8;
    header.0[12..28].copy_from_slice(&frame.nonce);
    header.0[28..36].copy_from_slice(&(frame.payload.len() as u64).to_be_bytes());
    writer.write_all(&header.0)?;
    writer.write_all(&frame.payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame(mut reader: impl Read) -> Result<Frame, ProtocolError> {
    let mut header = WipingArray([0_u8; HEADER_LEN]);
    reader.read_exact(&mut header.0)?;
    if header.0[..8] != MAGIC {
        return Err(ProtocolError::BadMagic);
    }
    if header.0[8] != VERSION {
        return Err(ProtocolError::UnsupportedVersion(header.0[8]));
    }
    let kind = Kind::try_from(header.0[9])?;
    let mime = Mime::try_from(header.0[10])?;
    let status = Status::try_from(header.0[11])?;
    let mut nonce = WipingArray([0_u8; 16]);
    nonce.0.copy_from_slice(&header.0[12..28]);
    let payload_len = u64::from_be_bytes(header.0[28..36].try_into().expect("fixed header slice"));
    let limit = mime.payload_limit();
    if payload_len > limit as u64 {
        return Err(ProtocolError::PayloadTooLarge {
            actual: usize::try_from(payload_len).unwrap_or(usize::MAX),
            limit,
        });
    }
    let mut payload = WipingVec(vec![0_u8; payload_len as usize]);
    reader.read_exact(&mut payload.0)?;
    let frame = Frame {
        kind,
        mime,
        status,
        nonce: nonce.take(),
        payload: payload.take(),
    };
    frame.validate_shape()?;
    Ok(frame)
}

struct WipingArray<const N: usize>([u8; N]);

impl<const N: usize> WipingArray<N> {
    fn take(&mut self) -> [u8; N] {
        std::mem::replace(&mut self.0, [0; N])
    }
}

impl<const N: usize> Drop for WipingArray<N> {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct WipingVec(Vec<u8>);

impl WipingVec {
    fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.0)
    }
}

impl Drop for WipingVec {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip_never_requires_a_terminator() {
        let nonce = [0x42; 16];
        let expected =
            Frame::put(nonce, Mime::Text, "Buzzard — 日本語 🦅".as_bytes().to_vec()).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &expected).unwrap();
        assert_eq!(read_frame(wire.as_slice()).unwrap(), expected);
    }

    #[test]
    fn debug_output_never_contains_clipboard_content_or_nonce() {
        let secret = "do-not-log-this";
        let frame = Frame::put([0x61; 16], Mime::Text, secret.as_bytes().to_vec()).unwrap();
        let output = format!("{frame:?}");
        assert!(!output.contains(secret));
        assert!(!output.contains("616161"));
        assert!(output.contains("payload_bytes"));
    }

    #[test]
    fn rejects_oversized_length_before_allocating_payload() {
        let frame = Frame::put([1; 16], Mime::Text, vec![1]).unwrap();
        let mut wire = Vec::new();
        write_frame(&mut wire, &frame).unwrap();
        wire[28..36].copy_from_slice(&((MAX_TEXT_BYTES as u64) + 1).to_be_bytes());
        assert!(matches!(
            read_frame(wire.as_slice()),
            Err(ProtocolError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_content_on_get_and_error_responses() {
        let invalid = Frame {
            kind: Kind::Get,
            mime: Mime::Text,
            status: Status::Ok,
            nonce: [0; 16],
            payload: b"forbidden".to_vec(),
        };
        assert!(matches!(
            invalid.validate_shape(),
            Err(ProtocolError::InvalidShape(_))
        ));
        let invalid = Frame {
            kind: Kind::GetResult,
            mime: Mime::Text,
            status: Status::InvalidContent,
            nonce: [0; 16],
            payload: b"forbidden".to_vec(),
        };
        assert!(matches!(
            invalid.validate_shape(),
            Err(ProtocolError::InvalidShape(_))
        ));
    }
}
