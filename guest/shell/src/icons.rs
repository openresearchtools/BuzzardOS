// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::model::Application;
use image::{DynamicImage, ImageReader, RgbaImage, imageops::FilterType};
use resvg::{tiny_skia, usvg};
use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

const ICON_SIZE: u32 = 64;

#[derive(Debug)]
pub struct AppIcon {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn load_application_icons(applications: &[Application]) -> BTreeMap<String, AppIcon> {
    applications
        .iter()
        .filter_map(|application| application.icon.as_deref())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|name| match load_icon(name) {
            Some(icon) => Some((name.to_owned(), icon)),
            None => {
                eprintln!("buzzardos-shell: no usable icon for {name}");
                None
            }
        })
        .collect()
}

pub fn load_icon(name: &str) -> Option<AppIcon> {
    let Some(path) = resolve_icon(name) else {
        eprintln!("buzzardos-shell: icon theme has no file for {name}");
        return None;
    };
    let icon = match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) if extension.eq_ignore_ascii_case("svg") => load_svg(&path),
        Some(extension) if extension.eq_ignore_ascii_case("png") => load_raster(&path),
        _ => None,
    };
    if icon.is_none() {
        eprintln!("buzzardos-shell: could not decode icon {}", path.display());
    }
    icon
}

fn resolve_icon(name: &str) -> Option<PathBuf> {
    let requested = Path::new(name);
    if requested.is_absolute() && requested.is_file() {
        return Some(requested.to_path_buf());
    }

    let mut roots = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/icons"));
        roots.push(home.join(".icons"));
        roots.push(home.join(".local/share/pixmaps"));
    }
    roots.extend([
        PathBuf::from("/usr/local/share/icons"),
        PathBuf::from("/usr/share/icons"),
    ]);

    let extension = requested
        .extension()
        .and_then(|extension| extension.to_str());
    let file_stem = if extension.is_some_and(|extension| {
        extension.eq_ignore_ascii_case("png") || extension.eq_ignore_ascii_case("svg")
    }) {
        requested
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(name)
    } else {
        name
    };
    let mut names = vec![file_stem.to_owned(), format!("{file_stem}-symbolic")];
    if file_stem == "applications-development" {
        names.push("applications-engineering-symbolic".to_owned());
    }
    for root in roots {
        for theme in ["BuzzardOS", "hicolor", "Adwaita", "default", ""] {
            for size in [
                "48x48", "64x64", "32x32", "128x128", "256x256", "scalable", "symbolic",
            ] {
                for context in [
                    "apps",
                    "applications",
                    "categories",
                    "places",
                    "actions",
                    "devices",
                    "legacy",
                    "mimetypes",
                    "status",
                ] {
                    for candidate_name in &names {
                        for extension in ["png", "svg"] {
                            let mut candidate = root.clone();
                            if !theme.is_empty() {
                                candidate.push(theme);
                            }
                            candidate.extend([size, context]);
                            candidate.push(format!("{candidate_name}.{extension}"));
                            if candidate.is_file() {
                                return Some(candidate);
                            }
                        }
                    }
                }
            }
        }
    }

    for root in [
        PathBuf::from("/usr/local/share/pixmaps"),
        PathBuf::from("/usr/share/pixmaps"),
    ] {
        for candidate_name in &names {
            for extension in ["png", "svg"] {
                let candidate = root.join(format!("{candidate_name}.{extension}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

fn load_raster(path: &Path) -> Option<AppIcon> {
    let image = ImageReader::open(path).ok()?.decode().ok()?;
    Some(from_image(image))
}

fn load_svg(path: &Path) -> Option<AppIcon> {
    load_svg_at_size(path, ICON_SIZE)
}

fn load_svg_at_size(path: &Path, target_size: u32) -> Option<AppIcon> {
    let bytes = fs::read(path).ok()?;
    let tree = usvg::Tree::from_data(&bytes, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(target_size, target_size)?;
    let size = tree.size();
    let scale = (target_size as f32 / size.width()).min(target_size as f32 / size.height());
    let x = (target_size as f32 - size.width() * scale) / 2.0;
    let y = (target_size as f32 - size.height() * scale) / 2.0;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_translate(x, y).post_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let png = pixmap.encode_png().ok()?;
    let image = ImageReader::new(Cursor::new(png))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let image = image.into_rgba8();
    let mut icon = AppIcon {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    };
    if path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.ends_with("-symbolic"))
    {
        for pixel in icon.rgba.chunks_exact_mut(4) {
            pixel[0] = 220;
            pixel[1] = 225;
            pixel[2] = 229;
        }
    }
    Some(icon)
}

fn from_image(image: DynamicImage) -> AppIcon {
    let image: RgbaImage = image
        .resize(ICON_SIZE, ICON_SIZE, FilterType::Lanczos3)
        .into_rgba8();
    AppIcon {
        width: image.width(),
        height: image.height(),
        rgba: image.into_raw(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn svg_icon_render_keeps_the_requested_extent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mark.svg");
        fs::write(
            &path,
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256" viewBox="0 0 256 256"><path fill="#ff7139" d="M0 0h256v256H0z"/></svg>"##,
        )
        .unwrap();
        let rendered = load_svg_at_size(&path, 188).unwrap();
        assert_eq!((rendered.width, rendered.height), (188, 188));
        assert_eq!(rendered.rgba.len(), 188 * 188 * 4);
    }
}
