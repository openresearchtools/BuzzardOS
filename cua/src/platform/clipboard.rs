use std::{
    ffi::OsStr,
    io::{BufRead, Read, Write},
    os::unix::ffi::OsStrExt,
    path::Path,
    process::{Command, Stdio},
    sync::{mpsc, Mutex},
    time::Duration,
};

use crate::core::clipboard::ClipboardBackend;
use clipboard_rs::{common::RustImage, Clipboard, ClipboardContext, ContentFormat};

pub struct LinuxClipboard {
    context: Result<Mutex<ClipboardContext>, String>,
}

impl LinuxClipboard {
    pub fn new() -> Self {
        Self {
            context: ClipboardContext::new()
                .map(Mutex::new)
                .map_err(|error| error.to_string()),
        }
    }

    fn context(&self) -> Result<std::sync::MutexGuard<'_, ClipboardContext>, String> {
        self.context
            .as_ref()
            .map_err(Clone::clone)?
            .lock()
            .map_err(|_| "clipboard lock was poisoned".to_owned())
    }

    fn write_wayland(&self, kind: &str, payload: Vec<u8>) -> Result<(), String> {
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            return Err("Wayland display is unavailable".into());
        }
        spawn_wayland_owner(kind, payload)
    }
}

const INTERNAL_OWNER_ARG: &str = "--internal-wayland-clipboard-owner-v1";
const TEXT_LIMIT: usize = 8 * 1024 * 1024;
const IMAGE_LIMIT: usize = 64 * 1024 * 1024;
const URI_LIMIT: usize = 64 * 1024;

fn spawn_wayland_owner(kind: &str, payload: Vec<u8>) -> Result<(), String> {
    let limit = match kind {
        "text" => TEXT_LIMIT,
        "image/png" => IMAGE_LIMIT,
        "text/uri-list" => URI_LIMIT,
        _ => return Err("unsupported internal clipboard payload type".into()),
    };
    if payload.len() > limit {
        return Err(format!("clipboard {kind} payload exceeds {limit} bytes"));
    }

    let mut child = Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
        .arg(INTERNAL_OWNER_ARG)
        .arg(kind)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start Wayland clipboard owner: {error}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "clipboard owner stdin is unavailable".to_owned())?;
    if let Err(error) = stdin.write_all(&payload) {
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("cannot send clipboard payload: {error}"));
    }
    drop(stdin);

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "clipboard owner stdout is unavailable".to_owned())?;
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = std::io::BufReader::new(stdout)
            .read_line(&mut line)
            .map(|_| line);
        let _ = sender.send(result);
    });

    let response = match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(line)) => line,
        Ok(Err(error)) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("cannot read clipboard owner readiness: {error}"));
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Wayland clipboard owner did not become ready within 5 seconds".into());
        }
    };
    if response.trim_end() != "READY" {
        let _ = child.kill();
        let _ = child.wait();
        return Err(response
            .strip_prefix("ERROR:")
            .unwrap_or("Wayland clipboard owner exited before becoming ready")
            .trim_end()
            .to_owned());
    }

    // The owner is deliberately not waited for: Wayland requires the selection
    // source to remain alive. It exits naturally when another source replaces it.
    drop(child);
    Ok(())
}

pub(crate) fn run_internal_wayland_owner(arguments: &[String]) -> Result<(), String> {
    use wl_clipboard_rs::copy::{MimeType, Options, Source};

    let kind = arguments
        .first()
        .map(String::as_str)
        .ok_or_else(|| "missing internal clipboard payload type".to_owned())?;
    if arguments.len() != 1 {
        return Err("invalid internal clipboard owner invocation".into());
    }
    let limit = match kind {
        "text" => TEXT_LIMIT,
        "image/png" => IMAGE_LIMIT,
        "text/uri-list" => URI_LIMIT,
        _ => return Err("unsupported internal clipboard payload type".into()),
    };
    let mut payload = Vec::new();
    std::io::stdin()
        .take((limit + 1) as u64)
        .read_to_end(&mut payload)
        .map_err(|error| format!("cannot read clipboard payload: {error}"))?;
    if payload.len() > limit {
        return Err(format!("clipboard {kind} payload exceeds {limit} bytes"));
    }

    let mime = match kind {
        "text" => MimeType::Text,
        other => MimeType::Specific(other.to_owned()),
    };
    let mut options = Options::new();
    options.foreground(true);
    let prepared = options
        .prepare_copy(Source::Bytes(payload.into_boxed_slice()), mime)
        .map_err(|error| format!("cannot claim Wayland clipboard: {error}"))?;
    println!("READY");
    std::io::stdout().flush().ok();
    prepared
        .serve()
        .map_err(|error| format!("Wayland clipboard owner failed: {error}"))
}

fn absolute_existing_file(path: &str) -> Result<String, String> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err("clipboard file paths must be absolute".into());
    }
    let canonical = path.canonicalize().map_err(|error| error.to_string())?;
    if !canonical.is_file() {
        return Err("clipboard path must identify an existing file".into());
    }
    Ok(canonical.to_string_lossy().into_owned())
}

fn file_uri(path: &str) -> Vec<u8> {
    let mut uri = b"file://".to_vec();
    for byte in OsStr::new(path).as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(*byte);
        } else {
            uri.extend_from_slice(format!("%{byte:02X}").as_bytes());
        }
    }
    uri.extend_from_slice(b"\r\n");
    uri
}

impl ClipboardBackend for LinuxClipboard {
    fn available_formats(&self) -> Result<Vec<String>, String> {
        self.context()?
            .available_formats()
            .map_err(|e| e.to_string())
    }

    fn read_text(&self) -> Result<Option<String>, String> {
        let context = self.context()?;
        if context.has(ContentFormat::Text) {
            context.get_text().map(Some).map_err(|e| e.to_string())
        } else {
            Ok(None)
        }
    }

    fn write_text(&self, text: String) -> Result<(), String> {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            self.write_wayland("text", text.into_bytes())
        } else {
            self.context()?.set_text(text).map_err(|e| e.to_string())
        }
    }

    fn write_image(&self, absolute_path: &str) -> Result<(), String> {
        let path = absolute_existing_file(absolute_path)?;
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            let image = image::open(&path).map_err(|error| error.to_string())?;
            if image.width() > 8192
                || image.height() > 8192
                || u64::from(image.width()) * u64::from(image.height()) > 64_000_000
            {
                return Err("clipboard image exceeds the 8192-axis or 64-megapixel limit".into());
            }
            let mut encoded = std::io::Cursor::new(Vec::new());
            image
                .write_to(&mut encoded, image::ImageFormat::Png)
                .map_err(|error| error.to_string())?;
            self.write_wayland("image/png", encoded.into_inner())
        } else {
            let image = clipboard_rs::RustImageData::from_path(&path).map_err(|e| e.to_string())?;
            self.context()?.set_image(image).map_err(|e| e.to_string())
        }
    }

    fn write_file_url(&self, absolute_path: &str) -> Result<(), String> {
        let path = absolute_existing_file(absolute_path)?;
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            self.write_wayland("text/uri-list", file_uri(&path))
        } else {
            self.context()?
                .set_files(vec![path])
                .map_err(|e| e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_relative_local_paths_before_clipboard_access() {
        let backend = LinuxClipboard::new();
        assert!(backend
            .write_file_url("relative.txt")
            .unwrap_err()
            .contains("absolute"));
        assert!(backend
            .write_image("relative.png")
            .unwrap_err()
            .contains("absolute"));
    }

    #[test]
    fn file_uri_percent_encodes_non_uri_path_bytes() {
        assert_eq!(file_uri("/tmp/a b#c.txt"), b"file:///tmp/a%20b%23c.txt\r\n");
    }

    #[test]
    fn native_clipboard_round_trips_text_when_ci_has_a_display() {
        if std::env::var_os("CI").is_none() {
            return;
        }
        let backend = LinuxClipboard::new();
        if backend.available_formats().is_err() {
            return;
        }
        backend
            .write_text("Buzzard CUA clipboard test".into())
            .unwrap();
        assert_eq!(
            backend.read_text().unwrap().as_deref(),
            Some("Buzzard CUA clipboard test")
        );
    }
}
