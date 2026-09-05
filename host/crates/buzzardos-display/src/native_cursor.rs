// SPDX-License-Identifier: AGPL-3.0-or-later

use gtk::gdk;
use gtk::glib::{self, translate::*};
use gtk::prelude::*;
use gtk4 as gtk;
use std::sync::OnceLock;

use crate::cursor_geometry::CursorGeometry;

type CursorConstructor = unsafe extern "C" fn(
    gdk::ffi::GdkCursorGetTextureCallback,
    glib::ffi::gpointer,
    glib::ffi::GDestroyNotify,
    *mut gdk::ffi::GdkCursor,
) -> *mut gdk::ffi::GdkCursor;

struct CursorImage {
    texture: gdk::Texture,
    hotspot_x: i32,
    hotspot_y: i32,
}

/// Give GDK the original physical-pixel texture and its logical geometry
/// separately. Never shrink a cursor bitmap and ask GTK to enlarge it again.
pub(crate) fn from_texture(texture: &gdk::Texture, x: i32, y: i32) -> gdk::Cursor {
    // The 4.16 public API is optional at runtime: the host package also runs
    // on Ubuntu 24.04's GTK 4.14. No private GTK structure or symbol is used.
    static CONSTRUCTOR: OnceLock<Option<CursorConstructor>> = OnceLock::new();
    let constructor = CONSTRUCTOR.get_or_init(|| unsafe {
        let symbol = libc::dlsym(libc::RTLD_DEFAULT, c"gdk_cursor_new_from_callback".as_ptr());
        if symbol.is_null() {
            None
        } else {
            // SAFETY: this is the documented public GDK 4.16 C signature.
            Some(std::mem::transmute::<*mut libc::c_void, CursorConstructor>(symbol))
        }
    });
    if let Some(constructor) = constructor {
        let data = Box::into_raw(Box::new(CursorImage {
            texture: texture.clone(),
            hotspot_x: x,
            hotspot_y: y,
        }));
        // SAFETY: GDK owns data until destroy_image, invokes the callback on
        // the GTK thread, and takes one reference to each returned texture.
        return unsafe {
            from_glib_full(constructor(
                Some(cursor_texture),
                data.cast(),
                Some(destroy_image),
                std::ptr::null_mut(),
            ))
        };
    }
    // Older GTK cannot express independent texture density. Preserve its
    // original texture-cursor behavior, without a lossy resampling stage.
    gdk::Cursor::from_texture(texture, x, y, None)
}

unsafe extern "C" fn cursor_texture(
    _cursor: *mut gdk::ffi::GdkCursor,
    _size: i32,
    scale: f64,
    width: *mut i32,
    height: *mut i32,
    hotspot_x: *mut i32,
    hotspot_y: *mut i32,
    data: glib::ffi::gpointer,
) -> *mut gdk::ffi::GdkTexture {
    // SAFETY: the constructor owns a live CursorImage and GDK provides the
    // four writable out parameters for the duration of this callback.
    let image = unsafe { &*data.cast::<CursorImage>() };
    let geometry = CursorGeometry::new(
        image.texture.width() as u32,
        image.texture.height() as u32,
        image.hotspot_x,
        image.hotspot_y,
        (scale * 120.0).round() as u32,
    );
    unsafe {
        *width = geometry.width;
        *height = geometry.height;
        *hotspot_x = geometry.hotspot_x;
        *hotspot_y = geometry.hotspot_y;
    }
    image.texture.to_glib_full()
}

unsafe extern "C" fn destroy_image(data: glib::ffi::gpointer) {
    // SAFETY: this allocation is transferred exactly once to GDK above.
    drop(unsafe { Box::from_raw(data.cast::<CursorImage>()) });
}
