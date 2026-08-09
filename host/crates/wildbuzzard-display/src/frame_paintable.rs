// SPDX-License-Identifier: AGPL-3.0-or-later

use std::cell::RefCell;

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub(crate) struct FramePaintable {
        pub(crate) current: RefCell<Option<gdk::Texture>>,
        // Keep superseded textures alive until GTK has painted the replacement.
        // This prevents the guest from reusing the old dmabuf while it can
        // still be referenced by the previous render node.
        pub(crate) retired: RefCell<Vec<gdk::Texture>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FramePaintable {
        const NAME: &'static str = "WildBuzzardFramePaintable";
        type Type = super::FramePaintable;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for FramePaintable {}

    impl gdk::subclass::prelude::PaintableImpl for FramePaintable {
        fn current_image(&self) -> gdk::Paintable {
            self.current
                .borrow()
                .as_ref()
                .map(|texture| texture.clone().upcast())
                .unwrap_or_else(|| gdk::Paintable::new_empty(1, 1))
        }

        fn flags(&self) -> gdk::PaintableFlags {
            // Both contents and dimensions can change as new guest output
            // buffers arrive, so neither STATIC flag is truthful.
            gdk::PaintableFlags::empty()
        }

        fn intrinsic_width(&self) -> i32 {
            self.current
                .borrow()
                .as_ref()
                .map_or(1, gdk::prelude::TextureExt::width)
        }

        fn intrinsic_height(&self) -> i32 {
            self.current
                .borrow()
                .as_ref()
                .map_or(1, gdk::prelude::TextureExt::height)
        }

        fn intrinsic_aspect_ratio(&self) -> f64 {
            let width = self.intrinsic_width().max(1);
            let height = self.intrinsic_height().max(1);
            f64::from(width) / f64::from(height)
        }

        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            if let Some(texture) = self.current.borrow().as_ref() {
                gdk::prelude::PaintableExt::snapshot(texture, snapshot, width, height);
            }
        }
    }
}

glib::wrapper! {
    pub(crate) struct FramePaintable(ObjectSubclass<imp::FramePaintable>)
        @implements gdk::Paintable;
}

impl FramePaintable {
    pub(crate) fn new() -> Self {
        glib::Object::builder().build()
    }

    pub(crate) fn set_texture(&self, texture: &impl IsA<gdk::Texture>) {
        let texture = texture.clone().upcast::<gdk::Texture>();
        let imp = self.imp();
        let old_size = imp
            .current
            .borrow()
            .as_ref()
            .map(|current| (current.width(), current.height()));
        if let Some(previous) = imp.current.replace(Some(texture.clone())) {
            imp.retired.borrow_mut().push(previous);
        }
        if old_size != Some((texture.width(), texture.height())) {
            self.invalidate_size();
        }
        self.invalidate_contents();
    }

    pub(crate) fn clear(&self) {
        let imp = self.imp();
        if let Some(current) = imp.current.take() {
            imp.retired.borrow_mut().push(current);
        }
        self.invalidate_size();
        self.invalidate_contents();
    }

    pub(crate) fn has_frame(&self) -> bool {
        self.imp().current.borrow().is_some()
    }

    pub(crate) fn release_retired(&self) {
        self.imp().retired.borrow_mut().clear();
    }
}

impl Default for FramePaintable {
    fn default() -> Self {
        Self::new()
    }
}
