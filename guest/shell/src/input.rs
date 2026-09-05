// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

use smithay_client_toolkit::seat::{Capability, keyboard::Modifiers};

use super::{DesktopPointerGesture, ShellAction, ShellSurface};

/// State belongs to the exact Wayland seat, never to the most recent caller.
#[derive(Default)]
pub(super) struct SeatInteraction {
    pub keyboard_focus: Option<ShellSurface>,
    pub modifiers: Modifiers,
    pub hovered: Option<ShellAction>,
    pub desktop_pointer_gesture: Option<DesktopPointerGesture>,
    pub last_desktop_click: Option<(PathBuf, u32)>,
}

/// Capability lifetime and interaction lifetime have the same seat owner.
/// Generic handles let tests exercise actual teardown with observable drops.
pub(super) struct SeatInput<P, K> {
    pub pointer: Option<P>,
    pub keyboard: Option<K>,
    pub interaction: SeatInteraction,
}

impl<P, K> Default for SeatInput<P, K> {
    fn default() -> Self {
        Self {
            pointer: None,
            keyboard: None,
            interaction: SeatInteraction::default(),
        }
    }
}

impl<P, K> SeatInput<P, K> {
    pub fn remove_capability(&mut self, capability: Capability) {
        match capability {
            Capability::Pointer => {
                self.pointer.take();
                self.interaction.hovered = None;
                self.interaction.desktop_pointer_gesture = None;
                self.interaction.last_desktop_click = None;
            }
            Capability::Keyboard => {
                self.keyboard.take();
                self.interaction.keyboard_focus = None;
                self.interaction.modifiers = Modifiers::default();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, collections::HashMap, rc::Rc};

    struct Handle(&'static str, Rc<RefCell<Vec<&'static str>>>);
    impl Drop for Handle {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    #[test]
    fn cua_churn_preserves_human_handles_and_interaction() {
        let drops = Rc::new(RefCell::new(Vec::new()));
        let mut seats = HashMap::new();
        let mut human = SeatInput::default();
        human.pointer = Some(Handle("human-pointer", drops.clone()));
        human.keyboard = Some(Handle("human-keyboard", drops.clone()));
        human.interaction.modifiers.shift = true;
        human.interaction.keyboard_focus = Some(ShellSurface::Desktop);
        human.interaction.hovered = Some(ShellAction::OpenFiles);
        human.interaction.last_desktop_click = Some((PathBuf::from("human"), 123));
        seats.insert("seat0", human);
        for _ in 0..100 {
            for seat in ["seat2", "seat1"] {
                let input = seats.entry(seat).or_default();
                input.pointer = Some(Handle("cua-pointer", drops.clone()));
                input.keyboard = Some(Handle("cua-keyboard", drops.clone()));
                input.interaction.modifiers.ctrl = true;
            }
            seats
                .get_mut("seat1")
                .unwrap()
                .remove_capability(Capability::Pointer);
            assert!(seats["seat1"].keyboard.is_some());
            seats
                .get_mut("seat2")
                .unwrap()
                .remove_capability(Capability::Keyboard);
            assert!(seats["seat2"].pointer.is_some());
            seats.remove("seat2");
            seats.remove("seat1");
            let human = &seats["seat0"];
            assert!(human.pointer.is_some() && human.keyboard.is_some());
            assert!(human.interaction.modifiers.shift);
            assert!(!human.interaction.modifiers.ctrl);
            assert_eq!(
                human.interaction.keyboard_focus,
                Some(ShellSurface::Desktop)
            );
            assert_eq!(human.interaction.hovered, Some(ShellAction::OpenFiles));
            assert_eq!(
                human.interaction.last_desktop_click,
                Some((PathBuf::from("human"), 123))
            );
        }
        assert_eq!(drops.borrow().len(), 400);
        assert!(!drops.borrow().iter().any(|name| name.starts_with("human")));
    }

    #[test]
    fn keyboard_teardown_does_not_reset_its_seats_pointer_state() {
        let mut input: SeatInput<(), ()> = SeatInput::default();
        input.pointer = Some(());
        input.keyboard = Some(());
        input.interaction.hovered = Some(ShellAction::OpenFiles);
        input.interaction.last_desktop_click = Some((PathBuf::from("own"), 42));
        input.interaction.modifiers.ctrl = true;
        input.interaction.keyboard_focus = Some(ShellSurface::Menu);
        input.remove_capability(Capability::Keyboard);
        assert!(input.pointer.is_some());
        assert!(input.keyboard.is_none());
        assert!(!input.interaction.modifiers.ctrl);
        assert_eq!(input.interaction.keyboard_focus, None);
        assert_eq!(input.interaction.hovered, Some(ShellAction::OpenFiles));
        assert_eq!(
            input.interaction.last_desktop_click,
            Some((PathBuf::from("own"), 42))
        );
    }
}
