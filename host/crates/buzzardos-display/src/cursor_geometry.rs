// SPDX-License-Identifier: AGPL-3.0-or-later

/// The nested wlroots backend submits cursor buffers and hotspots in physical
/// output pixels. GTK's texture cursor uses host logical units (scale 1).
/// Apply the same physical-to-logical mapping as the viewport, not guest UI
/// scale: the latter is already represented in the submitted cursor artwork.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CursorGeometry {
    pub width: i32,
    pub height: i32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
}

impl CursorGeometry {
    pub fn new(width: u32, height: u32, x: i32, y: i32, host_scale_120: u32) -> Self {
        let scale = u64::from(host_scale_120.max(120));
        let logical = |pixels: u32| ((u64::from(pixels) * 120 + scale / 2) / scale) as i32;
        let width = logical(width).max(1);
        let height = logical(height).max(1);
        Self {
            width,
            height,
            hotspot_x: logical(x.max(0) as u32).clamp(0, width - 1),
            hotspot_y: logical(y.max(0) as u32).clamp(0, height - 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_cursor_size_is_not_scaled_twice() {
        for scale in [120, 150, 160, 180, 210, 240] {
            let size = 24 * scale / 120;
            assert_eq!(CursorGeometry::new(size, size, 0, 0, scale).width, 24);
        }
    }

    #[test]
    fn cursor_hotspot_uses_the_same_mapping_as_the_image() {
        assert_eq!(
            CursorGeometry::new(48, 64, 16, 32, 240),
            CursorGeometry {
                width: 24,
                height: 32,
                hotspot_x: 8,
                hotspot_y: 16,
            }
        );
        assert_eq!(
            CursorGeometry::new(32, 32, 8, 16, 160),
            CursorGeometry {
                width: 24,
                height: 24,
                hotspot_x: 6,
                hotspot_y: 12,
            }
        );
    }

    #[test]
    fn cursor_geometry_retains_large_application_tools_and_clamps_hotspots() {
        assert_eq!(CursorGeometry::new(128, 128, 0, 0, 120).width, 128);
        assert_eq!(
            CursorGeometry::new(2, 2, 100, -5, 240),
            CursorGeometry {
                width: 1,
                height: 1,
                hotspot_x: 0,
                hotspot_y: 0,
            }
        );
    }
}
