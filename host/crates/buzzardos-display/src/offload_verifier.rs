// SPDX-License-Identifier: AGPL-3.0-or-later

//! Fail-closed verification of GTK's actual Wayland subsurface attachment.
//!
//! A `GskSubsurfaceNode` only says that `GtkGraphicsOffload` produced an
//! offload candidate. GTK 4.22.4's Wayland backend makes the authoritative
//! accept/reject decision later and reports it through `GDK_DEBUG=offload`.
//! This module tails only a bounded, post-reset section of that status log and
//! accepts an attachment only when its dmabuf dimensions and complete surface
//! destination match the current frame/geometry generation exactly.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::Serialize;

const GTK_OFFLOAD_LOG_CONTRACT: &str = "gtk-4.22.4-gdk-wayland-offload";
const MAX_TAIL_BYTES: u64 = 256 * 1024;
const MAX_PARTIAL_LINE_BYTES: usize = 16 * 1024;
const MAX_RECORDED_LINE_CHARS: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum OffloadResetKind {
    Frame,
    Geometry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct SurfaceRect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

impl SurfaceRect {
    pub(crate) fn new(x: i32, y: i32, width: i32, height: i32) -> Option<Self> {
        (width > 0 && height > 0).then_some(Self {
            x,
            y,
            width,
            height,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct OffloadExpectation {
    pub(crate) texture_width: u32,
    pub(crate) texture_height: u32,
    pub(crate) scale_120: u32,
    pub(crate) surface_rect: SurfaceRect,
}

impl OffloadExpectation {
    pub(crate) fn new(
        texture_width: u32,
        texture_height: u32,
        scale_120: u32,
        surface_rect: SurfaceRect,
    ) -> Option<Self> {
        (texture_width > 0 && texture_height > 0 && scale_120 > 0).then_some(Self {
            texture_width,
            texture_height,
            scale_120,
            surface_rect,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct OffloadVerificationDiagnostics {
    pub(crate) schema: u32,
    pub(crate) gtk_log_contract: &'static str,
    pub(crate) generation: u64,
    pub(crate) reset_kind: OffloadResetKind,
    pub(crate) candidate: bool,
    pub(crate) attach_verified: bool,
    pub(crate) expected: Option<OffloadExpectation>,
    pub(crate) subsurface_id: Option<String>,
    pub(crate) last_event: String,
    pub(crate) last_record: Option<String>,
    pub(crate) last_rejection: Option<String>,
    pub(crate) matching_move_observed: bool,
    pub(crate) bounded_tail_truncations: u64,
    pub(crate) malformed_records: u64,
}

pub(crate) struct OffloadVerifier {
    log_path: PathBuf,
    offset: u64,
    partial_line: Vec<u8>,
    generation: u64,
    reset_kind: OffloadResetKind,
    expected: Option<OffloadExpectation>,
    candidate: bool,
    verified_generation: Option<u64>,
    subsurface_id: Option<String>,
    last_event: String,
    last_record: Option<String>,
    last_rejection: Option<String>,
    matching_move_observed: bool,
    bounded_tail_truncations: u64,
    malformed_records: u64,
}

impl OffloadVerifier {
    pub(crate) fn new(log_path: impl Into<PathBuf>) -> io::Result<Self> {
        let log_path = log_path.into();
        let offset = log_length(&log_path)?;
        Ok(Self {
            log_path,
            offset,
            partial_line: Vec::new(),
            generation: 0,
            reset_kind: OffloadResetKind::Geometry,
            expected: None,
            candidate: false,
            verified_generation: None,
            subsurface_id: None,
            last_event: "not-started".into(),
            last_record: None,
            last_rejection: None,
            matching_move_observed: false,
            bounded_tail_truncations: 0,
            malformed_records: 0,
        })
    }

    /// Begin a new frame or geometry generation at the current end of the log.
    /// Records that predate this boundary can never verify the new generation.
    pub(crate) fn reset(
        &mut self,
        kind: OffloadResetKind,
        expected: Option<OffloadExpectation>,
    ) -> io::Result<u64> {
        self.partial_line.clear();
        self.generation = self.generation.saturating_add(1);
        self.reset_kind = kind;
        self.expected = expected;
        self.candidate = false;
        self.verified_generation = None;
        self.last_event = if expected.is_some() {
            "awaiting-post-change-attach"
        } else {
            "missing-exact-expectation"
        }
        .into();
        self.last_record = None;
        self.last_rejection = None;
        self.matching_move_observed = false;
        self.offset = match log_length(&self.log_path) {
            Ok(offset) => offset,
            Err(error) => {
                self.last_event = "log-boundary-error".into();
                return Err(error);
            }
        };
        Ok(self.generation)
    }

    /// Record the render-tree candidate separately from actual attachment.
    pub(crate) fn set_candidate(&mut self, candidate: bool) {
        self.candidate = candidate;
        if !candidate {
            self.verified_generation = None;
        }
    }

    /// Read at most the newest `MAX_TAIL_BYTES` written since the current
    /// generation boundary and process only complete records.
    pub(crate) fn poll(&mut self) -> io::Result<bool> {
        match self.poll_inner() {
            Ok(verified) => Ok(verified),
            Err(error) => {
                self.verified_generation = None;
                self.last_event = "log-read-error".into();
                Err(error)
            }
        }
    }

    fn poll_inner(&mut self) -> io::Result<bool> {
        let mut file = match File::open(&self.log_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.verified_generation = None;
                self.last_event = "log-missing".into();
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let length = file.metadata()?.len();
        if length == self.offset {
            return Ok(self.attach_verified());
        }

        if length < self.offset {
            self.offset = 0;
            self.partial_line.clear();
            self.verified_generation = None;
            self.last_event = "log-truncated".into();
        }

        let unread = length.saturating_sub(self.offset);
        let (start, discard_first_partial) = if unread > MAX_TAIL_BYTES {
            self.bounded_tail_truncations = self.bounded_tail_truncations.saturating_add(1);
            self.partial_line.clear();
            (length - MAX_TAIL_BYTES, true)
        } else {
            (self.offset, false)
        };
        file.seek(SeekFrom::Start(start))?;

        let to_read = length.saturating_sub(start);
        let mut bytes = Vec::with_capacity(usize::try_from(to_read).unwrap_or(0));
        file.take(to_read).read_to_end(&mut bytes)?;
        self.offset = start.saturating_add(bytes.len() as u64);

        if discard_first_partial {
            if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
                bytes.drain(..=newline);
            } else {
                return Ok(self.attach_verified());
            }
        } else if !self.partial_line.is_empty() {
            let mut combined = std::mem::take(&mut self.partial_line);
            combined.extend_from_slice(&bytes);
            bytes = combined;
        }

        let complete_length = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        let remainder = bytes.split_off(complete_length);
        if remainder.len() <= MAX_PARTIAL_LINE_BYTES {
            self.partial_line = remainder;
        } else {
            self.malformed_records = self.malformed_records.saturating_add(1);
            self.partial_line.clear();
        }

        for raw_line in bytes.split(|byte| *byte == b'\n') {
            let raw_line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            if raw_line.is_empty() {
                continue;
            }
            let line = String::from_utf8_lossy(raw_line);
            if let Some(event) = parse_offload_record(&line) {
                self.observe(event, &line);
            } else if looks_like_offload_record(&line) {
                self.malformed_records = self.malformed_records.saturating_add(1);
                self.last_event = "malformed-offload-record".into();
                self.last_record = Some(bounded_record(&line));
                self.verified_generation = None;
            }
        }

        Ok(self.attach_verified())
    }

    pub(crate) fn attach_verified(&self) -> bool {
        self.candidate && self.verified_generation == Some(self.generation)
    }

    pub(crate) fn expectation(&self) -> Option<OffloadExpectation> {
        self.expected
    }

    pub(crate) fn diagnostics(&self) -> OffloadVerificationDiagnostics {
        OffloadVerificationDiagnostics {
            schema: 1,
            gtk_log_contract: GTK_OFFLOAD_LOG_CONTRACT,
            generation: self.generation,
            reset_kind: self.reset_kind,
            candidate: self.candidate,
            attach_verified: self.attach_verified(),
            expected: self.expected,
            subsurface_id: self.subsurface_id.clone(),
            last_event: self.last_event.clone(),
            last_record: self.last_record.clone(),
            last_rejection: self.last_rejection.clone(),
            matching_move_observed: self.matching_move_observed,
            bounded_tail_truncations: self.bounded_tail_truncations,
            malformed_records: self.malformed_records,
        }
    }

    fn observe(&mut self, event: ParsedOffloadRecord, line: &str) {
        match event {
            ParsedOffloadRecord::Attach {
                subsurface_id,
                texture_width,
                texture_height,
                surface_rect,
            } => {
                if !self.is_our_subsurface(&subsurface_id) {
                    return;
                }
                let Some(expected) = self.expected else {
                    self.last_event = "attach-without-expectation".into();
                    self.last_record = Some(bounded_record(line));
                    self.verified_generation = None;
                    return;
                };
                if texture_width == expected.texture_width
                    && texture_height == expected.texture_height
                    && surface_rect == expected.surface_rect
                {
                    self.subsurface_id = Some(subsurface_id);
                    self.verified_generation = Some(self.generation);
                    self.last_event = "matching-dmabuf-attach".into();
                    self.last_record = Some(bounded_record(line));
                    self.last_rejection = None;
                } else {
                    self.last_event = "attach-geometry-mismatch".into();
                    self.last_record = Some(bounded_record(line));
                    self.verified_generation = None;
                }
            }
            ParsedOffloadRecord::Move {
                subsurface_id,
                texture_width,
                texture_height,
                reported_rect,
            } => {
                if !self.is_our_subsurface(&subsurface_id) {
                    return;
                }
                self.last_record = Some(bounded_record(line));
                self.matching_move_observed = self.expected.is_some_and(|expected| {
                    texture_width == expected.texture_width
                        && texture_height == expected.texture_height
                        && reported_rect == expected.surface_rect
                });
                // GTK 4.22.4 passes one extra texture-height argument before
                // the destination coordinates and omits the destination
                // height from this debug format. Consequently a Moving line
                // cannot prove the complete expected rectangle. Keep it
                // classified for diagnosis, but require the next Attaching
                // record before promoting this generation.
                self.last_event = "moving-record-not-attach-verification".into();
                self.verified_generation = None;
            }
            ParsedOffloadRecord::Reject {
                subsurface_id,
                reason,
            } => {
                if !self.is_our_subsurface(&subsurface_id) {
                    return;
                }
                self.last_event = "gtk-offload-rejected".into();
                self.last_record = Some(bounded_record(line));
                self.last_rejection = Some(bounded_record(&reason));
                self.verified_generation = None;
            }
        }
    }

    fn is_our_subsurface(&self, subsurface_id: &str) -> bool {
        self.subsurface_id
            .as_deref()
            .is_none_or(|known| known == subsurface_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedOffloadRecord {
    Attach {
        subsurface_id: String,
        texture_width: u32,
        texture_height: u32,
        surface_rect: SurfaceRect,
    },
    Move {
        subsurface_id: String,
        texture_width: u32,
        texture_height: u32,
        reported_rect: SurfaceRect,
    },
    Reject {
        subsurface_id: String,
        reason: String,
    },
}

fn parse_offload_record(line: &str) -> Option<ParsedOffloadRecord> {
    let (subsurface_id, body) = split_subsurface_record(line)?;

    if let Some(reason) = body.strip_prefix("🗙 ") {
        return Some(ParsedOffloadRecord::Reject {
            subsurface_id: subsurface_id.into(),
            reason: reason.into(),
        });
    }

    if let Some(attach) = body.strip_prefix("GdkDmabufTexture Attaching ") {
        let dimensions_start = attach.find('(')? + 1;
        let dimensions_end = attach[dimensions_start..].find(',')? + dimensions_start;
        let (texture_width, texture_height) =
            parse_dimensions(&attach[dimensions_start..dimensions_end])?;
        let destination_start = attach.rfind(" at ")? + " at ".len();
        let surface_rect = parse_rect(&attach[destination_start..])?;
        return Some(ParsedOffloadRecord::Attach {
            subsurface_id: subsurface_id.into(),
            texture_width,
            texture_height,
            surface_rect,
        });
    }

    let moving_marker = " Moving texture (";
    let moving_start = body.find(moving_marker)?;
    let dimensions_start = moving_start + moving_marker.len();
    let dimensions_end = body[dimensions_start..].find(')')? + dimensions_start;
    let (texture_width, texture_height) =
        parse_dimensions(&body[dimensions_start..dimensions_end])?;
    let destination_start = body[dimensions_end..].find(" to ")? + dimensions_end + " to ".len();
    let reported_rect = parse_rect(&body[destination_start..])?;
    Some(ParsedOffloadRecord::Move {
        subsurface_id: subsurface_id.into(),
        texture_width,
        texture_height,
        reported_rect,
    })
}

fn split_subsurface_record(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    let id_end = line.find(']')?;
    let subsurface_id = line.strip_prefix('[')?;
    let subsurface_id = subsurface_id.get(..id_end.checked_sub(1)?)?;
    if !subsurface_id.starts_with("0x")
        || !subsurface_id[2..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let body = line.get(id_end + 1..)?.trim_start();
    Some((subsurface_id, body))
}

fn parse_dimensions(value: &str) -> Option<(u32, u32)> {
    let (width, height) = value.split_once('x')?;
    let width = width.parse().ok()?;
    let height = height.parse().ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

fn parse_rect(value: &str) -> Option<SurfaceRect> {
    let mut values = value.split_ascii_whitespace();
    let x = values.next()?.parse().ok()?;
    let y = values.next()?.parse().ok()?;
    let width = values.next()?.parse().ok()?;
    let height = values.next()?.parse().ok()?;
    SurfaceRect::new(x, y, width, height)
}

fn looks_like_offload_record(line: &str) -> bool {
    line.contains("] 🗙 ")
        || line.contains("GdkDmabufTexture Attaching")
        || line.contains(" Moving texture ")
}

fn bounded_record(value: &str) -> String {
    value.chars().take(MAX_RECORDED_LINE_CHARS).collect()
}

fn log_length(path: &Path) -> io::Result<u64> {
    match path.metadata() {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;

    use super::*;

    const EXPECTED: OffloadExpectation = OffloadExpectation {
        texture_width: 1600,
        texture_height: 1000,
        scale_120: 150,
        surface_rect: SurfaceRect {
            x: 16,
            y: 108,
            width: 1280,
            height: 800,
        },
    };

    fn append(path: &Path, line: &str) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{line}").unwrap();
    }

    #[test]
    fn parses_actual_live_non_integral_rejection() {
        // Captured in
        // /tmp/buzzardos-flash-continuity-live/display-gateway-gdk-debug.log.
        let event = parse_offload_record(
            "[0x5f4dbf0ee5e0] 🗙 Non-integral device coordinates 17.5 107.5 1600 1000 (scale 1.25)",
        )
        .unwrap();
        assert_eq!(
            event,
            ParsedOffloadRecord::Reject {
                subsurface_id: "0x5f4dbf0ee5e0".into(),
                reason: "Non-integral device coordinates 17.5 107.5 1600 1000 (scale 1.25)".into(),
            }
        );
    }

    #[test]
    fn parses_actual_live_non_texture_rejection() {
        // Also copied byte-for-byte from the same live acceptance log.
        let event =
            parse_offload_record("[0x5f4dbf0ee5e0] 🗙 Only textures supported (found GskColorNode)")
                .unwrap();
        assert!(matches!(
            event,
            ParsedOffloadRecord::Reject { reason, .. }
                if reason == "Only textures supported (found GskColorNode)"
        ));
    }

    #[test]
    fn parses_exact_pinned_gtk_attach_format() {
        assert_eq!(
            OffloadExpectation::new(1600, 1000, 150, EXPECTED.surface_rect),
            Some(EXPECTED)
        );
        let event = parse_offload_record(
            "[0x5f4dbf0ee5e0] GdkDmabufTexture Attaching △ (1600x1000, srgb) at 16 108 1280 800",
        )
        .unwrap();
        assert_eq!(
            event,
            ParsedOffloadRecord::Attach {
                subsurface_id: "0x5f4dbf0ee5e0".into(),
                texture_width: 1600,
                texture_height: 1000,
                surface_rect: EXPECTED.surface_rect,
            }
        );
    }

    #[test]
    fn parses_moving_record_but_does_not_treat_it_as_attach_proof() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display-gateway.log");
        fs::write(&path, []).unwrap();
        let mut verifier = OffloadVerifier::new(&path).unwrap();
        verifier
            .reset(OffloadResetKind::Geometry, Some(EXPECTED))
            .unwrap();
        verifier.set_candidate(true);
        append(
            &path,
            "[0x5f4dbf0ee5e0] △ Moving texture (1600x1000) to 16 108 1280 800",
        );

        assert!(!verifier.poll().unwrap());
        let diagnostics = verifier.diagnostics();
        assert!(diagnostics.matching_move_observed);
        assert_eq!(
            diagnostics.last_event,
            "moving-record-not-attach-verification"
        );
    }

    #[test]
    fn only_an_exact_post_reset_dmabuf_attach_verifies() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display-gateway.log");
        append(
            &path,
            "[0x5f4dbf0ee5e0] GdkDmabufTexture Attaching △ (1600x1000, srgb) at 16 108 1280 800",
        );
        let mut verifier = OffloadVerifier::new(&path).unwrap();
        verifier
            .reset(OffloadResetKind::Frame, Some(EXPECTED))
            .unwrap();
        verifier.set_candidate(true);

        assert!(!verifier.poll().unwrap(), "pre-reset record was ignored");
        append(
            &path,
            "[0x5f4dbf0ee5e0] GdkDmabufTexture Attaching △ (1600x1000, srgb) at 16 108 1279 800",
        );
        assert!(!verifier.poll().unwrap(), "mismatched width failed closed");
        append(
            &path,
            "[0x5f4dbf0ee5e0] GdkDmabufTexture Attaching △ (1600x1000, srgb) at 16 108 1280 800",
        );
        assert!(verifier.poll().unwrap());
        assert!(verifier.diagnostics().attach_verified);
    }

    #[test]
    fn a_later_rejection_revokes_the_current_generation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display-gateway.log");
        fs::write(&path, []).unwrap();
        let mut verifier = OffloadVerifier::new(&path).unwrap();
        verifier
            .reset(OffloadResetKind::Frame, Some(EXPECTED))
            .unwrap();
        verifier.set_candidate(true);
        append(
            &path,
            "[0x5f4dbf0ee5e0] GdkDmabufTexture Attaching △ (1600x1000, srgb) at 16 108 1280 800",
        );
        assert!(verifier.poll().unwrap());
        append(
            &path,
            "[0x5f4dbf0ee5e0] 🗙 Non-integral device coordinates 20.5 108 1600 1000 (scale 1.25)",
        );
        assert!(!verifier.poll().unwrap());
        assert_eq!(verifier.diagnostics().last_event, "gtk-offload-rejected");
    }

    #[test]
    fn missing_or_unreadable_log_invalidates_proof() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display-gateway.log");
        fs::write(&path, []).unwrap();
        let mut verifier = OffloadVerifier::new(&path).unwrap();
        verifier
            .reset(OffloadResetKind::Frame, Some(EXPECTED))
            .unwrap();
        verifier.set_candidate(true);
        append(
            &path,
            "[0x5f4dbf0ee5e0] GdkDmabufTexture Attaching △ (1600x1000, srgb) at 16 108 1280 800",
        );
        assert!(verifier.poll().unwrap());

        fs::remove_file(&path).unwrap();
        assert!(!verifier.poll().unwrap());
        assert_eq!(verifier.diagnostics().last_event, "log-missing");

        let parent_file = directory.path().join("not-a-directory");
        fs::write(&parent_file, []).unwrap();
        verifier.log_path = parent_file.join("display-gateway.log");
        verifier.set_candidate(true);
        assert!(
            verifier
                .reset(OffloadResetKind::Frame, Some(EXPECTED))
                .is_err()
        );
        assert!(!verifier.attach_verified());
        assert_eq!(verifier.diagnostics().last_event, "log-boundary-error");
    }

    #[test]
    fn bounded_tail_can_verify_a_complete_final_record() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("display-gateway.log");
        fs::write(&path, []).unwrap();
        let mut verifier = OffloadVerifier::new(&path).unwrap();
        verifier
            .reset(OffloadResetKind::Frame, Some(EXPECTED))
            .unwrap();
        verifier.set_candidate(true);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&vec![b'x'; MAX_TAIL_BYTES as usize + 1024])
            .unwrap();
        file.write_all(b"\n").unwrap();
        writeln!(
            file,
            "[0x5f4dbf0ee5e0] GdkDmabufTexture Attaching △ (1600x1000, srgb) at 16 108 1280 800"
        )
        .unwrap();
        drop(file);

        assert!(verifier.poll().unwrap());
        assert_eq!(verifier.diagnostics().bounded_tail_truncations, 1);
    }
}
