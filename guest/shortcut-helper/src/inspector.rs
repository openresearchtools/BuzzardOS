// SPDX-License-Identifier: AGPL-3.0-or-later

use glib::{KeyFile, KeyFileFlags};
use image::{DynamicImage, GenericImageView, ImageEncoder, RgbaImage, imageops};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use thiserror::Error;
use wildbuzzard_desktop_core::FileObservation;

const ELF_HEADER_SIZE: usize = 64;
const TYPE2_MARKER: &[u8] = b"AI\x02";
const SQUASHFS_MAGIC: &[u8] = b"hsqs";
const SQUASHFS_SUPERBLOCK_SIZE: usize = 96;
const MAX_RUNTIME_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const SCAN_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_LISTING_BYTES: usize = 1024 * 1024;
const MAX_DESKTOP_METADATA_BYTES: usize = 256 * 1024;
const MAX_ICON_BYTES: usize = 4 * 1024 * 1024;
const MAX_METADATA_PATH_BYTES: usize = 4096;
const MAX_METADATA_ENTRIES: usize = 16_384;
const MAX_DISPLAY_NAME_BYTES: usize = 512;
const MAX_ICON_NAME_BYTES: usize = 255;
const MAX_ICON_DIMENSION: u32 = 4096;
const MAX_ICON_PIXELS: u64 = 16 * 1024 * 1024;
const METADATA_TIMEOUT: Duration = Duration::from_secs(5);
const UNSQUASHFS: &str = "/usr/bin/unsquashfs";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedAppImage {
    pub display_name: String,
    pub identity_key: String,
    pub observation: FileObservation,
    pub squashfs_offset: u64,
    pub icon: Option<InspectedIcon>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedIcon {
    pub source_name: String,
    pub content_sha256: String,
    pub png_256: Vec<u8>,
}

#[derive(Debug)]
pub struct ValidatedAppImage {
    path: PathBuf,
    file: File,
    observation: FileObservation,
    squashfs_offset: u64,
}

impl ValidatedAppImage {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn observation(&self) -> &FileObservation {
        &self.observation
    }

    pub fn squashfs_offset(&self) -> u64 {
        self.squashfs_offset
    }

    pub fn authorize_owner_execute(&self) -> Result<(), InspectionError> {
        let metadata = self.file.metadata().map_err(|source| InspectionError::Io {
            path: self.path.display().to_string(),
            source,
        })?;
        let current = metadata.permissions().mode();
        if current & libc::S_IXUSR == 0 {
            self.file
                .set_permissions(std::fs::Permissions::from_mode(current | libc::S_IXUSR))
                .map_err(|source| InspectionError::Authorization {
                    path: self.path.display().to_string(),
                    source,
                })?;
            self.file.sync_all().map_err(|source| InspectionError::Io {
                path: self.path.display().to_string(),
                source,
            })?;
        }
        Ok(())
    }

    pub fn inspect_metadata(&self) -> Result<InspectedAppImage, InspectionError> {
        let fallback_name = fallback_display_name(&self.path)?;
        let listing = unsquashfs(
            self,
            &[
                "-offset",
                &self.squashfs_offset.to_string(),
                "-lc",
                "-max-depth",
                "8",
                "-no-xattrs",
                "-processors",
                "1",
                "-mem",
                "16M",
                "-no-progress",
            ],
            &[],
            MAX_LISTING_BYTES,
        )?;
        let entries = parse_listing(&listing)?;
        let desktop_path = entries
            .iter()
            .filter(|entry| entry.extension() == Some(OsStr::new("desktop")))
            .min_by_key(|entry| (entry.components().count(), entry.as_os_str().len()))
            .cloned();

        let (display_name, icon_name) = if let Some(desktop_path) = desktop_path {
            let metadata = cat_entry(self, &desktop_path, MAX_DESKTOP_METADATA_BYTES)?;
            parse_desktop_metadata(&metadata).unwrap_or((fallback_name.clone(), None))
        } else {
            (fallback_name.clone(), None)
        };
        let icon = icon_name
            .as_deref()
            .and_then(|name| select_png_entry(&entries, name))
            .and_then(|entry| match cat_entry(self, &entry, MAX_ICON_BYTES) {
                Ok(bytes) => normalize_png(&entry, &bytes).ok(),
                Err(_) => None,
            });

        Ok(InspectedAppImage {
            identity_key: identity_key(&display_name),
            display_name,
            observation: self.observation.clone(),
            squashfs_offset: self.squashfs_offset,
            icon,
        })
    }

    /// Execute the exact descriptor that passed validation. The pathname is
    /// deliberately not resolved again: a concurrent rename or replacement
    /// therefore cannot substitute another executable between inspection and
    /// launch. Keeping this one read-only descriptor inherited also lets the
    /// native Type-2 runtime reopen its own SquashFS through `/proc/self/fd`.
    pub fn spawn_exact(&self) -> Result<Child, InspectionError> {
        let descriptor_path = descriptor_path(self.file.as_raw_fd());
        let mut command = Command::new(&descriptor_path);
        command
            .arg0(&descriptor_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        inherit_descriptor(&mut command, self.file.as_raw_fd());
        command.spawn().map_err(|source| InspectionError::Launch {
            path: self.path.display().to_string(),
            source,
        })
    }
}

#[derive(Debug, Error)]
pub enum InspectionError {
    #[error("AppImage path must be an absolute normalized guest path")]
    InvalidPath,
    #[error("AppImage target is not a regular file or is a symbolic link: {0}")]
    UnsafeTarget(String),
    #[error("file is not an x86-64 Type-2 AppImage: {0}")]
    InvalidAppImage(String),
    #[error("AppImage metadata parser is unavailable at {UNSQUASHFS}")]
    ParserUnavailable,
    #[error("AppImage metadata parser failed: {0}")]
    ParserFailed(String),
    #[error("AppImage metadata parser exceeded its {limit}-byte output limit")]
    ParserOutputLimit { limit: usize },
    #[error("AppImage metadata parser exceeded its time limit")]
    ParserTimeout,
    #[error("AppImage metadata contains an unsafe or excessive archive listing")]
    UnsafeMetadataListing,
    #[error("AppImage metadata field is malformed: {0}")]
    MalformedMetadata(String),
    #[error("cannot add owner execute permission to {path}: {source}")]
    Authorization {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot launch validated AppImage {path}: {source}")]
    Launch {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("I/O error for {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

pub fn validate_appimage(path: &Path) -> Result<ValidatedAppImage, InspectionError> {
    validate_absolute_path(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
        .map_err(|source| {
            if matches!(source.raw_os_error(), Some(code) if code == libc::ELOOP || code == libc::ENXIO)
            {
                InspectionError::UnsafeTarget(path.display().to_string())
            } else {
                InspectionError::Io {
                    path: path.display().to_string(),
                    source,
                }
            }
        })?;
    let metadata = file.metadata().map_err(|source| InspectionError::Io {
        path: path.display().to_string(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(InspectionError::UnsafeTarget(path.display().to_string()));
    }
    let size = metadata.len();
    if size < (ELF_HEADER_SIZE + SQUASHFS_SUPERBLOCK_SIZE) as u64 {
        return Err(InspectionError::InvalidAppImage("file is too short".into()));
    }
    let mut elf = [0u8; ELF_HEADER_SIZE];
    file.read_exact_at(&mut elf, 0)
        .map_err(|source| InspectionError::Io {
            path: path.display().to_string(),
            source,
        })?;
    validate_elf_header(&elf)?;
    let squashfs_offset = find_squashfs(&file, size, path)?;
    Ok(ValidatedAppImage {
        path: path.to_path_buf(),
        file,
        observation: FileObservation {
            device: metadata.dev(),
            inode: metadata.ino(),
            size,
        },
        squashfs_offset,
    })
}

fn validate_absolute_path(path: &Path) -> Result<(), InspectionError> {
    if !path.is_absolute()
        || path == Path::new("/")
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
        || path.as_os_str().len() > 4096
    {
        return Err(InspectionError::InvalidPath);
    }
    Ok(())
}

fn validate_elf_header(header: &[u8; ELF_HEADER_SIZE]) -> Result<(), InspectionError> {
    if &header[..4] != b"\x7fELF"
        || header[4] != 2
        || header[5] != 1
        || header[6] != 1
        || &header[8..11] != TYPE2_MARKER
        || u16::from_le_bytes([header[18], header[19]]) != 62
        || u32::from_le_bytes([header[20], header[21], header[22], header[23]]) != 1
        || u16::from_le_bytes([header[52], header[53]]) < ELF_HEADER_SIZE as u16
    {
        return Err(InspectionError::InvalidAppImage(
            "ELF class, architecture, Type-2 marker, or header is invalid".into(),
        ));
    }
    let elf_type = u16::from_le_bytes([header[16], header[17]]);
    if !matches!(elf_type, 2 | 3) {
        return Err(InspectionError::InvalidAppImage(
            "ELF type is neither executable nor position-independent executable".into(),
        ));
    }
    Ok(())
}

fn find_squashfs(file: &File, file_size: u64, path: &Path) -> Result<u64, InspectionError> {
    let scan_end = file_size.min(MAX_RUNTIME_SCAN_BYTES);
    let mut offset = ELF_HEADER_SIZE as u64;
    let mut overlap = Vec::new();
    while offset < scan_end {
        let length = usize::try_from((scan_end - offset).min(SCAN_CHUNK_BYTES as u64))
            .unwrap_or(SCAN_CHUNK_BYTES);
        let mut bytes = vec![0u8; length];
        file.read_exact_at(&mut bytes, offset)
            .map_err(|source| InspectionError::Io {
                path: path.display().to_string(),
                source,
            })?;
        let overlap_length = overlap.len();
        let mut combined = overlap;
        combined.extend_from_slice(&bytes);
        for index in memmem(&combined, SQUASHFS_MAGIC) {
            let candidate = offset
                .saturating_sub(overlap_length as u64)
                .saturating_add(u64::try_from(index).unwrap_or(u64::MAX));
            if validate_squashfs_superblock(file, candidate, file_size, path)? {
                return Ok(candidate);
            }
        }
        overlap = combined[combined.len().saturating_sub(3)..].to_vec();
        offset += length as u64;
    }
    Err(InspectionError::InvalidAppImage(
        "no valid bounded SquashFS payload was found".into(),
    ))
}

fn memmem(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    haystack
        .windows(needle.len())
        .enumerate()
        .filter_map(|(index, window)| (window == needle).then_some(index))
        .collect()
}

fn validate_squashfs_superblock(
    file: &File,
    offset: u64,
    file_size: u64,
    path: &Path,
) -> Result<bool, InspectionError> {
    if offset
        .checked_add(SQUASHFS_SUPERBLOCK_SIZE as u64)
        .is_none_or(|end| end > file_size)
    {
        return Ok(false);
    }
    let mut block = [0u8; SQUASHFS_SUPERBLOCK_SIZE];
    file.read_exact_at(&mut block, offset)
        .map_err(|source| InspectionError::Io {
            path: path.display().to_string(),
            source,
        })?;
    let inodes = u32::from_le_bytes(block[4..8].try_into().expect("fixed slice"));
    let block_size = u32::from_le_bytes(block[12..16].try_into().expect("fixed slice"));
    let compression = u16::from_le_bytes(block[20..22].try_into().expect("fixed slice"));
    let block_log = u16::from_le_bytes(block[22..24].try_into().expect("fixed slice"));
    let major = u16::from_le_bytes(block[28..30].try_into().expect("fixed slice"));
    let minor = u16::from_le_bytes(block[30..32].try_into().expect("fixed slice"));
    let bytes_used = u64::from_le_bytes(block[40..48].try_into().expect("fixed slice"));
    Ok(&block[..4] == SQUASHFS_MAGIC
        && inodes > 0
        && block_size.is_power_of_two()
        && (4096..=1024 * 1024).contains(&block_size)
        && block_log == block_size.trailing_zeros() as u16
        && (1..=6).contains(&compression)
        && major == 4
        && minor == 0
        && bytes_used >= SQUASHFS_SUPERBLOCK_SIZE as u64
        && offset
            .checked_add(bytes_used)
            .is_some_and(|end| end <= file_size))
}

fn unsquashfs(
    image: &ValidatedAppImage,
    options: &[&str],
    entries: &[&OsStr],
    limit: usize,
) -> Result<Vec<u8>, InspectionError> {
    let parser = std::fs::symlink_metadata(UNSQUASHFS)
        .ok()
        .filter(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .ok_or(InspectionError::ParserUnavailable)?;
    if parser.permissions().mode() & 0o111 == 0 {
        return Err(InspectionError::ParserUnavailable);
    }
    let descriptor_path = descriptor_path(image.file.as_raw_fd());
    let mut command = Command::new(UNSQUASHFS);
    command
        .args(options)
        .arg(&descriptor_path)
        .args(entries)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    inherit_descriptor(&mut command, image.file.as_raw_fd());
    let child = command.spawn().map_err(|source| InspectionError::Io {
        path: UNSQUASHFS.into(),
        source,
    })?;
    bounded_child_output(child, limit, METADATA_TIMEOUT)
}

fn descriptor_path(descriptor: i32) -> String {
    format!("/proc/self/fd/{descriptor}")
}

fn inherit_descriptor(command: &mut Command, descriptor: i32) {
    // SAFETY: this closure runs in the post-fork child before exec and calls
    // only async-signal-safe fcntl. It changes only the validated read-only
    // descriptor's close-on-exec flag in that child.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            if flags < 0 || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn cat_entry(
    image: &ValidatedAppImage,
    entry: &Path,
    limit: usize,
) -> Result<Vec<u8>, InspectionError> {
    validate_metadata_path(entry)?;
    let offset = image.squashfs_offset.to_string();
    unsquashfs(
        image,
        &[
            "-offset",
            &offset,
            "-cat",
            "-no-xattrs",
            "-processors",
            "1",
            "-mem",
            "16M",
            "-no-progress",
            "-no-wildcards",
        ],
        &[entry.as_os_str()],
        limit,
    )
}

fn bounded_child_output(
    mut child: Child,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<u8>, InspectionError> {
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    set_nonblocking(stdout.as_raw_fd())?;
    set_nonblocking(stderr.as_raw_fd())?;
    let started = Instant::now();
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut stdout_done = false;
    let mut stderr_done = false;
    loop {
        let output_overflow = drain_nonblocking(&mut stdout, &mut output, limit, &mut stdout_done)?;
        let _ = drain_nonblocking(&mut stderr, &mut errors, 64 * 1024, &mut stderr_done)?;
        if output_overflow {
            let _ = child.kill();
            let _ = child.wait();
            return Err(InspectionError::ParserOutputLimit { limit });
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(InspectionError::ParserTimeout);
        }
        if let Some(status) = child.try_wait().map_err(|source| InspectionError::Io {
            path: UNSQUASHFS.into(),
            source,
        })? {
            let output_overflow =
                drain_nonblocking(&mut stdout, &mut output, limit, &mut stdout_done)?;
            let _ = drain_nonblocking(&mut stderr, &mut errors, 64 * 1024, &mut stderr_done)?;
            if output_overflow {
                return Err(InspectionError::ParserOutputLimit { limit });
            }
            if !status.success() {
                return Err(InspectionError::ParserFailed(
                    String::from_utf8_lossy(&errors).trim().to_owned(),
                ));
            }
            return Ok(output);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn set_nonblocking(descriptor: i32) -> Result<(), InspectionError> {
    // SAFETY: fcntl is called on an owned live pipe descriptor.
    let current = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if current < 0
        || unsafe { libc::fcntl(descriptor, libc::F_SETFL, current | libc::O_NONBLOCK) } < 0
    {
        return Err(InspectionError::Io {
            path: "metadata parser pipe".into(),
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn drain_nonblocking(
    stream: &mut impl Read,
    output: &mut Vec<u8>,
    limit: usize,
    done: &mut bool,
) -> Result<bool, InspectionError> {
    if *done {
        return Ok(false);
    }
    let mut buffer = [0u8; 8192];
    let mut overflow = false;
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => {
                *done = true;
                return Ok(overflow);
            }
            Ok(length) => {
                let remaining = limit.saturating_sub(output.len());
                output.extend_from_slice(&buffer[..length.min(remaining)]);
                overflow |= length > remaining;
                if overflow {
                    // Keep the pipe drained on future calls without retaining
                    // attacker-controlled output beyond the fixed cap.
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(overflow),
            Err(error) => {
                return Err(InspectionError::Io {
                    path: "metadata parser pipe".into(),
                    source: error,
                });
            }
        }
    }
}

fn parse_listing(bytes: &[u8]) -> Result<Vec<PathBuf>, InspectionError> {
    let text = std::str::from_utf8(bytes).map_err(|_| InspectionError::UnsafeMetadataListing)?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let relative = trimmed
            .strip_prefix("squashfs-root/")
            .or_else(|| trimmed.strip_prefix("squashfs-root"))
            .unwrap_or(trimmed);
        if relative.is_empty() {
            continue;
        }
        let path = PathBuf::from(relative);
        validate_metadata_path(&path)?;
        if entries.len() >= MAX_METADATA_ENTRIES {
            return Err(InspectionError::UnsafeMetadataListing);
        }
        entries.push(path);
    }
    entries.sort();
    entries.dedup();
    Ok(entries)
}

fn validate_metadata_path(path: &Path) -> Result<(), InspectionError> {
    if path.is_absolute()
        || path.as_os_str().len() > MAX_METADATA_PATH_BYTES
        || path.components().any(|component| {
            !matches!(component, Component::Normal(_))
                || component.as_os_str().as_encoded_bytes().is_empty()
        })
    {
        return Err(InspectionError::UnsafeMetadataListing);
    }
    Ok(())
}

fn parse_desktop_metadata(bytes: &[u8]) -> Result<(String, Option<String>), InspectionError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| InspectionError::MalformedMetadata("desktop file is not UTF-8".into()))?;
    let key_file = KeyFile::new();
    key_file
        .load_from_data(text, KeyFileFlags::NONE)
        .map_err(|error| InspectionError::MalformedMetadata(error.to_string()))?;
    let entry_type = key_file
        .string("Desktop Entry", "Type")
        .map_err(|error| InspectionError::MalformedMetadata(error.to_string()))?;
    if entry_type.as_str() != "Application" {
        return Err(InspectionError::MalformedMetadata(
            "desktop metadata is not an Application".into(),
        ));
    }
    let name = key_file
        .locale_string("Desktop Entry", "Name", None)
        .map_err(|error| InspectionError::MalformedMetadata(error.to_string()))?;
    let name = sanitize_name(&name)?;
    let icon = key_file
        .string("Desktop Entry", "Icon")
        .ok()
        .and_then(|value| sanitize_icon_name(&value).ok());
    Ok((name, icon))
}

fn sanitize_name(name: &str) -> Result<String, InspectionError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_DISPLAY_NAME_BYTES
        || trimmed.chars().any(char::is_control)
    {
        return Err(InspectionError::MalformedMetadata(
            "Name is empty, oversized, or contains control characters".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

fn sanitize_icon_name(icon: &str) -> Result<String, InspectionError> {
    let icon = icon.trim();
    if icon.is_empty()
        || icon.len() > MAX_ICON_NAME_BYTES
        || icon == "."
        || icon == ".."
        || !icon
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(InspectionError::MalformedMetadata(
            "Icon is not a bounded basename".into(),
        ));
    }
    Ok(icon.to_owned())
}

fn fallback_display_name(path: &Path) -> Result<String, InspectionError> {
    let file_name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| InspectionError::MalformedMetadata("filename is not UTF-8".into()))?;
    let without_suffix = file_name
        .strip_suffix(".AppImage")
        .or_else(|| file_name.strip_suffix(".appimage"))
        .unwrap_or(file_name);
    sanitize_name(without_suffix)
}

fn identity_key(name: &str) -> String {
    name.chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
        .collect()
}

fn select_png_entry(entries: &[PathBuf], icon_name: &str) -> Option<PathBuf> {
    let icon_stem = Path::new(icon_name)
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or(icon_name);
    entries
        .iter()
        .filter(|entry| entry.extension() == Some(OsStr::new("png")))
        .filter(|entry| {
            entry
                .file_stem()
                .and_then(OsStr::to_str)
                .is_some_and(|stem| stem == icon_stem)
        })
        .min_by_key(|entry| {
            let path = entry.to_string_lossy();
            let preferred = if path.contains("256x256") {
                0
            } else if path.contains("128x128") {
                1
            } else if path.contains("64x64") {
                2
            } else {
                3
            };
            (
                preferred,
                entry.components().count(),
                entry.as_os_str().len(),
            )
        })
        .cloned()
}

fn normalize_png(path: &Path, bytes: &[u8]) -> Result<InspectedIcon, InspectionError> {
    if bytes.len() < 24
        || bytes.len() > MAX_ICON_BYTES
        || &bytes[..8] != b"\x89PNG\r\n\x1a\n"
        || &bytes[12..16] != b"IHDR"
    {
        return Err(InspectionError::MalformedMetadata(
            "icon is not a bounded PNG".into(),
        ));
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("fixed slice"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("fixed slice"));
    if width == 0
        || height == 0
        || width > MAX_ICON_DIMENSION
        || height > MAX_ICON_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_ICON_PIXELS
    {
        return Err(InspectionError::MalformedMetadata(
            "icon dimensions exceed the safe limit".into(),
        ));
    }
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|error| InspectionError::MalformedMetadata(error.to_string()))?;
    let normalized = fit_icon(image, 256);
    let mut output = Vec::new();
    image::codecs::png::PngEncoder::new(&mut output)
        .write_image(
            normalized.as_raw(),
            normalized.width(),
            normalized.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| InspectionError::MalformedMetadata(error.to_string()))?;
    let digest = Sha256::digest(&output);
    Ok(InspectedIcon {
        source_name: path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("icon.png")
            .to_owned(),
        content_sha256: format!("{digest:x}"),
        png_256: output,
    })
}

fn fit_icon(image: DynamicImage, size: u32) -> RgbaImage {
    let (width, height) = image.dimensions();
    let scale = f64::from(size) / f64::from(width.max(height));
    let target_width = (f64::from(width) * scale).round().max(1.0) as u32;
    let target_height = (f64::from(height) * scale).round().max(1.0) as u32;
    let resized = image
        .resize_exact(
            target_width,
            target_height,
            image::imageops::FilterType::Lanczos3,
        )
        .into_rgba8();
    let mut canvas = RgbaImage::new(size, size);
    imageops::overlay(
        &mut canvas,
        &resized,
        i64::from((size - target_width) / 2),
        i64::from((size - target_height) / 2),
    );
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn elf_prefix() -> Vec<u8> {
        let mut bytes = vec![0u8; 4096];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        bytes[8..11].copy_from_slice(TYPE2_MARKER);
        bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
        bytes[52..54].copy_from_slice(&(ELF_HEADER_SIZE as u16).to_le_bytes());
        bytes
    }

    fn append_synthetic_squashfs(mut bytes: Vec<u8>) -> Vec<u8> {
        let offset = bytes.len();
        bytes.resize(offset + SQUASHFS_SUPERBLOCK_SIZE, 0);
        let block = &mut bytes[offset..];
        block[..4].copy_from_slice(SQUASHFS_MAGIC);
        block[4..8].copy_from_slice(&1u32.to_le_bytes());
        block[12..16].copy_from_slice(&131_072u32.to_le_bytes());
        block[20..22].copy_from_slice(&1u16.to_le_bytes());
        block[22..24].copy_from_slice(&17u16.to_le_bytes());
        block[28..30].copy_from_slice(&4u16.to_le_bytes());
        block[30..32].copy_from_slice(&0u16.to_le_bytes());
        block[40..48].copy_from_slice(&(SQUASHFS_SUPERBLOCK_SIZE as u64).to_le_bytes());
        bytes
    }

    #[test]
    fn validates_x86_64_type2_and_rejects_fake_markers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Valid.AppImage");
        fs::write(&path, append_synthetic_squashfs(elf_prefix())).unwrap();
        let validated = validate_appimage(&path).unwrap();
        assert_eq!(validated.squashfs_offset(), 4096);

        let fake = temp.path().join("Fake.AppImage");
        let mut bytes = elf_prefix();
        bytes[18..20].copy_from_slice(&183u16.to_le_bytes());
        fs::write(&fake, append_synthetic_squashfs(bytes)).unwrap();
        assert!(matches!(
            validate_appimage(&fake),
            Err(InspectionError::InvalidAppImage(_))
        ));
    }

    #[test]
    fn final_symlink_is_rejected_without_touching_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("Target.AppImage");
        let link = temp.path().join("Link.AppImage");
        fs::write(&target, append_synthetic_squashfs(elf_prefix())).unwrap();
        symlink(&target, &link).unwrap();
        assert!(matches!(
            validate_appimage(&link),
            Err(InspectionError::UnsafeTarget(_))
        ));
        assert_eq!(
            fs::metadata(target).unwrap().permissions().mode() & 0o111,
            0
        );
    }

    #[test]
    fn listing_rejects_absolute_parent_and_excessive_paths() {
        for listing in [
            b"/absolute\n".as_slice(),
            b"squashfs-root/../escape\n".as_slice(),
        ] {
            assert!(matches!(
                parse_listing(listing),
                Err(InspectionError::UnsafeMetadataListing)
            ));
        }
    }

    #[test]
    fn desktop_metadata_is_sanitized_without_using_exec() {
        let metadata = b"[Desktop Entry]\nType=Application\nName=Useful Tool\nIcon=tool-icon\nExec=sh -c 'ignored'\n";
        let (name, icon) = parse_desktop_metadata(metadata).unwrap();
        assert_eq!(name, "Useful Tool");
        assert_eq!(icon.as_deref(), Some("tool-icon"));
        assert!(
            parse_desktop_metadata(b"[Desktop Entry]\nType=Application\nName=Bad\x01Control\n")
                .is_err()
        );
    }

    #[test]
    fn png_normalization_is_bounded_and_deterministic() {
        let mut source = RgbaImage::new(32, 16);
        source.fill(255);
        source.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(
                source.as_raw(),
                source.width(),
                source.height(),
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        let first = normalize_png(Path::new("tool.png"), &bytes).unwrap();
        let second = normalize_png(Path::new("tool.png"), &bytes).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.content_sha256.len(), 64);
        let decoded = image::load_from_memory(&first.png_256).unwrap();
        assert_eq!(decoded.dimensions(), (256, 256));
    }

    #[test]
    fn real_squashfs_metadata_is_read_without_executing_the_image() {
        if !Command::new("/usr/bin/mksquashfs")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("skipping real SquashFS fixture: mksquashfs is unavailable");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("buzz.desktop"),
            b"[Desktop Entry]\nType=Application\nName=Fixture Buzz\nIcon=fixture\nExec=never-run\n",
        )
        .unwrap();
        let mut icon = RgbaImage::new(32, 32);
        icon.fill(240);
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                icon.as_raw(),
                icon.width(),
                icon.height(),
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        fs::write(root.join("fixture.png"), png).unwrap();
        let squashfs = temp.path().join("fixture.squashfs");
        let status = Command::new("/usr/bin/mksquashfs")
            .arg(&root)
            .arg(&squashfs)
            .args([
                "-noappend",
                "-quiet",
                "-no-progress",
                "-no-xattrs",
                "-processors",
                "1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let appimage = temp.path().join("Fixture.AppImage");
        let mut image_bytes = elf_prefix();
        image_bytes.extend_from_slice(&fs::read(squashfs).unwrap());
        fs::write(&appimage, image_bytes).unwrap();

        let validated = validate_appimage(&appimage).unwrap();
        let inspected = validated.inspect_metadata().unwrap();
        assert_eq!(inspected.display_name, "Fixture Buzz");
        assert_eq!(inspected.identity_key, "fixturebuzz");
        assert!(inspected.icon.is_some());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn launch_executes_the_validated_inode_after_path_replacement() {
        if !Command::new("/usr/bin/mksquashfs")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("skipping descriptor-launch fixture: mksquashfs is unavailable");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("fixture"), b"fixture").unwrap();
        let squashfs = temp.path().join("fixture.squashfs");
        let status = Command::new("/usr/bin/mksquashfs")
            .arg(&root)
            .arg(&squashfs)
            .args([
                "-noappend",
                "-quiet",
                "-no-progress",
                "-no-xattrs",
                "-processors",
                "1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let payload = fs::read(&squashfs).unwrap();
        let target = temp.path().join("Swappable.AppImage");
        let replacement = temp.path().join("Replacement.AppImage");
        make_executable_appimage(Path::new("/usr/bin/true"), &target, &payload);
        make_executable_appimage(Path::new("/usr/bin/false"), &replacement, &payload);

        let validated = validate_appimage(&target).unwrap();
        let inspected_inode = validated.observation().inode;
        let retained = temp.path().join("retained-original");
        fs::rename(&target, &retained).unwrap();
        fs::rename(&replacement, &target).unwrap();
        assert_ne!(fs::metadata(&target).unwrap().ino(), inspected_inode);

        validated.authorize_owner_execute().unwrap();
        let status = validated.spawn_exact().unwrap().wait().unwrap();
        assert!(
            status.success(),
            "path replacement executed /usr/bin/false instead of the validated /usr/bin/true inode"
        );
        assert_eq!(fs::metadata(&retained).unwrap().ino(), inspected_inode);
    }

    #[cfg(target_arch = "x86_64")]
    fn make_executable_appimage(program: &Path, destination: &Path, squashfs: &[u8]) {
        let mut bytes = fs::read(program).unwrap();
        assert!(bytes.len() >= ELF_HEADER_SIZE);
        bytes[8..11].copy_from_slice(TYPE2_MARKER);
        bytes.extend_from_slice(squashfs);
        fs::write(destination, bytes).unwrap();
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700)).unwrap();
    }
}
