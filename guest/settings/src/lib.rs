// SPDX-License-Identifier: AGPL-3.0-or-later

mod model;
pub mod sound;
mod ui;
mod updater;

use gio::prelude::*;
use glib::variant::ToVariant;
use gtk4::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub use model::{ChangeSection, PageId, SettingsStore};

pub const APPLICATION_ID: &str = "org.openresearchtools.WildBuzzard.Settings1";
pub const OBJECT_PATH: &str = "/org/openresearchtools/WildBuzzard/Settings1";
const INTROSPECTION_XML: &str = r#"
<node>
  <interface name="org.openresearchtools.WildBuzzard.Settings1">
    <signal name="Changed">
      <arg name="generation" type="t"/>
      <arg name="sections" type="as"/>
    </signal>
  </interface>
</node>
"#;

#[derive(Default)]
pub(crate) struct ChangeBus {
    connection: RefCell<Option<gio::DBusConnection>>,
    registration_error: RefCell<Option<String>>,
    registered: Cell<bool>,
}

impl ChangeBus {
    fn register(&self, application: &gtk4::Application) -> Result<(), String> {
        let result = self.register_inner(application);
        self.registration_error
            .replace(result.as_ref().err().cloned());
        result
    }

    fn register_inner(&self, application: &gtk4::Application) -> Result<(), String> {
        if self.registered.get() {
            return Ok(());
        }
        let connection = application
            .dbus_connection()
            .ok_or_else(|| "the private guest session bus is unavailable".to_owned())?;
        let node = gio::DBusNodeInfo::for_xml(INTROSPECTION_XML)
            .map_err(|error| format!("cannot parse Settings D-Bus interface: {error}"))?;
        let interface = node
            .lookup_interface(APPLICATION_ID)
            .ok_or_else(|| "Settings D-Bus interface is missing".to_owned())?;
        connection
            .register_object(OBJECT_PATH, &interface)
            .build()
            .map_err(|error| format!("cannot register Settings D-Bus object: {error}"))?;
        self.connection.replace(Some(connection));
        self.registered.set(true);
        Ok(())
    }

    pub(crate) fn diagnostic(&self) -> Option<String> {
        self.registration_error.borrow().clone()
    }

    pub(crate) fn emit_changed(
        &self,
        generation: u64,
        sections: &[ChangeSection],
    ) -> Result<(), String> {
        let Some(connection) = self.connection.borrow().clone() else {
            return Err("Settings change bus is unavailable".into());
        };
        let section_names = sections
            .iter()
            .map(|section| section.bus_name())
            .collect::<Vec<_>>();
        let parameters = (generation, section_names).to_variant();
        connection
            .emit_signal(
                None,
                OBJECT_PATH,
                APPLICATION_ID,
                "Changed",
                Some(&parameters),
            )
            .map_err(|error| format!("cannot announce Settings change: {error}"))
    }
}

pub fn run() -> glib::ExitCode {
    let application = gtk4::Application::builder()
        .application_id(APPLICATION_ID)
        .build();
    let bus = Rc::new(ChangeBus::default());
    let window = Rc::new(RefCell::new(None::<gtk4::ApplicationWindow>));

    {
        let bus = Rc::clone(&bus);
        application.connect_startup(move |application| {
            if let Err(error) = bus.register(application) {
                eprintln!("wildbuzzard-settings: {error}");
            }
        });
    }
    {
        let bus = Rc::clone(&bus);
        let window = Rc::clone(&window);
        application.connect_activate(move |application| {
            if let Some(existing) = window.borrow().as_ref() {
                existing.present();
                return;
            }
            let store = match SettingsStore::discover() {
                Ok(store) => Rc::new(RefCell::new(store)),
                Err(error) => {
                    let failed = ui::build_fatal_window(application, &error.to_string());
                    failed.present();
                    window.replace(Some(failed));
                    return;
                }
            };
            let created = ui::build_window(application, store, Rc::clone(&bus));
            created.present();
            window.replace(Some(created));
        });
    }

    application.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_identity_and_signal_contract_are_stable() {
        assert_eq!(
            APPLICATION_ID,
            "org.openresearchtools.WildBuzzard.Settings1"
        );
        assert!(OBJECT_PATH.starts_with('/'));
        let node = gio::DBusNodeInfo::for_xml(INTROSPECTION_XML).unwrap();
        assert!(node.lookup_interface(APPLICATION_ID).is_some());
        assert_eq!(
            INTROSPECTION_XML
                .matches("<signal name=\"Changed\">")
                .count(),
            1
        );
        assert!(INTROSPECTION_XML.contains("<arg name=\"generation\" type=\"t\"/>"));
        assert!(INTROSPECTION_XML.contains("<arg name=\"sections\" type=\"as\"/>"));
    }
}
