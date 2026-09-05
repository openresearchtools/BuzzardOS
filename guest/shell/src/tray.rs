// SPDX-License-Identifier: AGPL-3.0-or-later
//! Guest-session StatusNotifier watcher/host and bounded D-Bus menu client.
//!
//! All callbacks run on the shell's existing GLib main context. There are no
//! timers, worker services, host connections, files, or retained activity logs.
//! Protocol references (this module is an original implementation):
//! https://specifications.freedesktop.org/status-notifier-item/latest-single/
//! https://api.kde.org/kstatusnotifieritem.html
//! https://cgit.arctica-project.org/libdbusmenu/tree/libdbusmenu-glib/dbus-menu.xml

use anyhow::{Context, Result, ensure};
use gio::glib::{Variant, variant::ToVariant};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::{Rc, Weak};

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM: &str = "org.kde.StatusNotifierItem";
const MENU: &str = "com.canonical.dbusmenu";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const TIMEOUT_MS: i32 = 1500;
const MAX_ITEMS: usize = 128;
const MAX_REPLY_BYTES: usize = 2 * 1024 * 1024;
const MAX_ICON_AXIS: i32 = 256;
const MAX_MENU_ITEMS: usize = 256;
const MAX_MENU_DEPTH: usize = 6;

const WATCHER_XML: &str = r#"<node><interface name="org.kde.StatusNotifierWatcher">
 <method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
 <method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
 <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
 <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
 <property name="ProtocolVersion" type="i" access="read"/>
 <signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
 <signal name="StatusNotifierItemUnregistered"><arg type="s"/></signal>
 <signal name="StatusNotifierHostRegistered"/>
 <signal name="StatusNotifierHostUnregistered"/>
</interface></node>"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayPixmap {
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA, converted from the protocol's network-order ARGB.
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayItem {
    /// Unique D-Bus owner plus object path; never a title or an array index.
    pub id: String,
    pub title: String,
    pub icon_name: Option<String>,
    pub pixmap: Option<TrayPixmap>,
    pub status: String,
    pub item_is_menu: bool,
    pub has_menu: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayMenuEntry {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub separator: bool,
    pub submenu: bool,
    pub toggle_type: Option<String>,
    /// None for ordinary items; -1, 0, 1 for mixed, off, on.
    pub toggle_state: Option<i32>,
    pub children: Vec<TrayMenuEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayMenu {
    pub revision: u32,
    pub entries: Vec<TrayMenuEntry>,
}

struct Entry {
    service: String,
    owner: String,
    path: String,
    serial: u64,
    item: Option<TrayItem>,
    refreshing: bool,
    refresh_again: bool,
    menu_path: Option<String>,
    menu: Option<TrayMenu>,
    menu_requested: bool,
    menu_refreshing: bool,
    menu_refresh_again: bool,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<String, Entry>,
    next_serial: u64,
    pending_registrations: usize,
    dirty: bool,
    error: Option<String>,
}

/// Own this on the same thread that iterates GLib. Drop unregisters the watcher
/// and subscriptions; item applications continue to own their own lifecycle.
pub struct Tray {
    connection: gio::DBusConnection,
    state: Rc<RefCell<State>>,
    registration: Option<gio::RegistrationId>,
    subscriptions: Vec<gio::SignalSubscriptionId>,
    cancellable: gio::Cancellable,
}

impl Tray {
    pub fn new() -> Result<Self> {
        let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
            .context("connecting to the private guest session bus for the tray")?;
        Self::on_connection(connection)
    }

    fn on_connection(connection: gio::DBusConnection) -> Result<Self> {
        let state = Rc::new(RefCell::new(State::default()));
        let cancellable = gio::Cancellable::new();
        let weak = Rc::downgrade(&state);
        let property_state = weak.clone();
        let cancel = cancellable.clone();
        let node = gio::DBusNodeInfo::for_xml(WATCHER_XML)?;
        let interface = node
            .lookup_interface(WATCHER)
            .context("tray interface missing")?;
        let registration = connection
            .register_object(WATCHER_PATH, &interface)
            .method_call(
                move |connection, sender, _, _, method, parameters, invocation| {
                    let Some(state) = weak.upgrade() else {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.Disconnected",
                            "Tray is closed",
                        );
                        return;
                    };
                    let Some((service,)) = parameters.get::<(String,)>() else {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.InvalidArgs",
                            "Expected a service name or item object path",
                        );
                        return;
                    };
                    let Some(sender) = sender.filter(|sender| gio::dbus_is_unique_name(sender))
                    else {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.AccessDenied",
                            "A session-bus owner is required",
                        );
                        return;
                    };
                    if method == "RegisterStatusNotifierHost" {
                        // The shell itself is already the visualization host. Another
                        // host registration must still refer to a syntactically valid name.
                        if gio::dbus_is_name(&service) {
                            invocation.return_value(Some(&().to_variant()));
                        } else {
                            invocation.return_dbus_error(
                                "org.freedesktop.DBus.Error.InvalidArgs",
                                "Invalid host service name",
                            );
                        }
                        return;
                    }
                    let Ok((service, path)) = registration_target(sender, &service) else {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.InvalidArgs",
                            "Invalid item service or object path",
                        );
                        return;
                    };
                    if state.borrow().pending_registrations >= MAX_ITEMS {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.LimitsExceeded",
                            "Tray item limit reached",
                        );
                        return;
                    }
                    state.borrow_mut().pending_registrations += 1;
                    // Pin the owner before accepting a well-known name. Later calls
                    // never follow an alias to a replacement process.
                    let weak = Rc::downgrade(&state);
                    let sender = sender.to_owned();
                    let next_connection = connection.clone();
                    let cancel = cancel.clone();
                    connection.call(
                        Some(BUS),
                        BUS_PATH,
                        BUS,
                        "GetNameOwner",
                        Some(&(service.clone(),).to_variant()),
                        None,
                        gio::DBusCallFlags::NO_AUTO_START,
                        TIMEOUT_MS,
                        Some(&cancel.clone()),
                        move |reply| {
                            let Some(state) = weak.upgrade() else {
                                invocation.return_dbus_error(
                                    "org.freedesktop.DBus.Error.Disconnected",
                                    "Tray is closed",
                                );
                                return;
                            };
                            state.borrow_mut().pending_registrations -= 1;
                            let owner = reply
                                .ok()
                                .and_then(|reply| reply.get::<(String,)>())
                                .map(|reply| reply.0);
                            if owner.as_deref() != Some(&sender) {
                                invocation.return_dbus_error(
                                    "org.freedesktop.DBus.Error.AccessDenied",
                                    "Item service must belong to the registering connection",
                                );
                                return;
                            }
                            let id = format!("{sender}{path}");
                            let inserted = {
                                let mut state = state.borrow_mut();
                                if state.entries.contains_key(&id) {
                                    false
                                } else if state.entries.len() >= MAX_ITEMS {
                                    invocation.return_dbus_error(
                                        "org.freedesktop.DBus.Error.LimitsExceeded",
                                        "Tray item limit reached",
                                    );
                                    return;
                                } else {
                                    state.next_serial += 1;
                                    let serial = state.next_serial;
                                    state.entries.insert(
                                        id.clone(),
                                        Entry {
                                            service,
                                            owner: sender,
                                            path,
                                            serial,
                                            item: None,
                                            refreshing: false,
                                            refresh_again: false,
                                            menu_path: None,
                                            menu: None,
                                            menu_requested: false,
                                            menu_refreshing: false,
                                            menu_refresh_again: false,
                                        },
                                    );
                                    true
                                }
                            };
                            invocation.return_value(Some(&().to_variant()));
                            if inserted {
                                watcher_signal(
                                    &next_connection,
                                    "StatusNotifierItemRegistered",
                                    &id,
                                );
                                refresh_item(&next_connection, &state, &cancel, &id);
                            }
                        },
                    );
                },
            )
            .property(move |_, _, _, _, property| match property {
                "RegisteredStatusNotifierItems" => property_state
                    .upgrade()
                    .map(|state| state.borrow().entries.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default()
                    .to_variant(),
                "IsStatusNotifierHostRegistered" => true.to_variant(),
                _ => 0_i32.to_variant(),
            })
            .build()?;
        let mut tray = Self {
            connection,
            state,
            registration: Some(registration),
            subscriptions: Vec::new(),
            cancellable,
        };
        let result = tray.connection.call_sync(
            Some(BUS),
            BUS_PATH,
            BUS,
            "RequestName",
            Some(&(WATCHER, 4_u32).to_variant()),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            TIMEOUT_MS,
            gio::Cancellable::NONE,
        )?;
        ensure!(
            result.get::<(u32,)>() == Some((1,)),
            "the guest tray watcher name is already owned"
        );
        tray.subscribe();
        let _ = tray.connection.emit_signal(
            None,
            WATCHER_PATH,
            WATCHER,
            "StatusNotifierHostRegistered",
            Some(&().to_variant()),
        );
        Ok(tray)
    }

    pub fn take_dirty(&self) -> bool {
        std::mem::take(&mut self.state.borrow_mut().dirty)
    }

    /// Latest operation failure, for an explicit UI error if desired. No logs.
    pub fn take_error(&self) -> Option<String> {
        self.state.borrow_mut().error.take()
    }

    pub fn items(&self) -> Vec<TrayItem> {
        self.state
            .borrow()
            .entries
            .values()
            .filter_map(|entry| entry.item.as_ref())
            .filter(|item| item.status != "Passive")
            .cloned()
            .collect()
    }

    pub fn activate(&self, id: &str, x: i32, y: i32) -> Result<()> {
        self.item_call(id, "Activate", &(x, y).to_variant())
    }

    pub fn secondary_activate(&self, id: &str, x: i32, y: i32) -> Result<()> {
        self.item_call(id, "SecondaryActivate", &(x, y).to_variant())
    }

    /// Use this only when has_menu is false; exported menus should be displayed
    /// by the shell with request_menu/menu/activate_menu_item instead.
    pub fn context_menu(&self, id: &str, x: i32, y: i32) -> Result<()> {
        self.item_call(id, "ContextMenu", &(x, y).to_variant())
    }

    pub fn scroll(&self, id: &str, delta: i32, horizontal: bool) -> Result<()> {
        self.item_call(
            id,
            "Scroll",
            &(delta, if horizontal { "horizontal" } else { "vertical" }).to_variant(),
        )
    }

    fn item_call(&self, id: &str, method: &'static str, parameters: &Variant) -> Result<()> {
        let state = self.state.borrow();
        let entry = state
            .entries
            .get(id)
            .context("tray item is no longer available")?;
        let weak = Rc::downgrade(&self.state);
        self.connection.call(
            Some(&entry.owner),
            &entry.path,
            ITEM,
            method,
            Some(parameters),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            TIMEOUT_MS,
            Some(&self.cancellable),
            move |reply| {
                if reply.is_err() {
                    operation_error(&weak, "The application could not complete the tray action.");
                }
            },
        );
        Ok(())
    }

    /// Notify the exporter before fetching its real menu. Results arrive through
    /// the GLib loop and take_dirty; neither this nor layout updates block input.
    pub fn request_menu(&self, id: &str) -> Result<()> {
        self.about_to_show(id, 0)
    }

    /// Call when opening a nested menu, since exporters may populate it lazily.
    pub fn request_submenu(&self, id: &str, menu_id: i32) -> Result<()> {
        let allowed = self
            .state
            .borrow()
            .entries
            .get(id)
            .and_then(|entry| entry.menu.as_ref())
            .is_some_and(|menu| {
                menu_entry(&menu.entries, menu_id).is_some_and(|entry| entry.submenu)
            });
        ensure!(allowed, "tray submenu is disabled, hidden, or stale");
        self.about_to_show(id, menu_id)
    }

    fn about_to_show(&self, id: &str, menu_id: i32) -> Result<()> {
        let (owner, path, serial) = {
            let mut state = self.state.borrow_mut();
            let entry = state
                .entries
                .get_mut(id)
                .context("tray item is no longer available")?;
            let path = entry
                .menu_path
                .clone()
                .context("application does not export a D-Bus menu")?;
            entry.menu_requested = true;
            (entry.owner.clone(), path, entry.serial)
        };
        let weak = Rc::downgrade(&self.state);
        let connection = self.connection.clone();
        let cancel = self.cancellable.clone();
        let id = id.to_owned();
        self.connection.call(
            Some(&owner),
            &path.clone(),
            MENU,
            "AboutToShow",
            Some(&(menu_id,).to_variant()),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            TIMEOUT_MS,
            Some(&self.cancellable),
            move |_| {
                if let Some(state) = weak.upgrade() {
                    let current = state.borrow().entries.get(&id).is_some_and(|entry| {
                        entry.serial == serial && entry.menu_path.as_deref() == Some(&path)
                    });
                    if !current {
                        return;
                    }
                    refresh_menu(&connection, &state, &cancel, &id);
                }
            },
        );
        Ok(())
    }

    pub fn menu(&self, id: &str) -> Option<TrayMenu> {
        self.state
            .borrow()
            .entries
            .get(id)
            .and_then(|entry| entry.menu.clone())
    }

    pub fn close_menu(&self, id: &str) {
        if let Some(entry) = self.state.borrow_mut().entries.get_mut(id) {
            entry.menu_requested = false;
        }
    }

    pub fn activate_menu_item(&self, id: &str, menu_id: i32, timestamp: u32) -> Result<()> {
        let state = self.state.borrow();
        let entry = state
            .entries
            .get(id)
            .context("tray item is no longer available")?;
        let menu = entry.menu.as_ref().context("tray menu has not loaded")?;
        ensure!(
            menu_item_actionable(&menu.entries, menu_id),
            "tray menu item is disabled, hidden, a submenu, or stale"
        );
        let path = entry
            .menu_path
            .as_deref()
            .context("tray menu is no longer available")?;
        let weak = Rc::downgrade(&self.state);
        self.connection.call(
            Some(&entry.owner),
            path,
            MENU,
            "Event",
            Some(&(menu_id, "clicked", 0_i32.to_variant(), timestamp).to_variant()),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            TIMEOUT_MS,
            Some(&self.cancellable),
            move |reply| {
                if reply.is_err() {
                    operation_error(
                        &weak,
                        "The application could not activate the tray menu item.",
                    );
                }
            },
        );
        Ok(())
    }

    #[allow(deprecated)]
    fn subscribe(&mut self) {
        let weak = Rc::downgrade(&self.state);
        self.subscriptions.push(self.connection.signal_subscribe(
            Some(BUS),
            Some(BUS),
            Some("NameOwnerChanged"),
            Some(BUS_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |connection, _, _, _, _, parameters| {
                let Some((name, old_owner, _)) = parameters.get::<(String, String, String)>()
                else {
                    return;
                };
                if old_owner.is_empty() {
                    return;
                }
                let Some(state) = weak.upgrade() else {
                    return;
                };
                let removed = {
                    let mut state = state.borrow_mut();
                    let ids = state
                        .entries
                        .iter()
                        .filter(|(_, entry)| {
                            entry.owner == old_owner
                                && (entry.owner == name || entry.service == name)
                        })
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    for id in &ids {
                        state.entries.remove(id);
                    }
                    state.dirty |= !ids.is_empty();
                    ids
                };
                for id in removed {
                    watcher_signal(connection, "StatusNotifierItemUnregistered", &id);
                }
            },
        ));
        for interface in [ITEM, PROPERTIES, MENU] {
            let weak = Rc::downgrade(&self.state);
            let cancel = self.cancellable.clone();
            self.subscriptions.push(self.connection.signal_subscribe(
                None,
                Some(interface),
                None,
                None,
                None,
                gio::DBusSignalFlags::NONE,
                move |connection, sender, path, interface, member, parameters| {
                    let Some(state) = weak.upgrade() else {
                        return;
                    };
                    let refresh_properties = interface == ITEM
                        && matches!(
                            member,
                            "NewTitle"
                                | "NewIcon"
                                | "NewAttentionIcon"
                                | "NewOverlayIcon"
                                | "NewToolTip"
                                | "NewStatus"
                                | "NewMenu"
                        )
                        || interface == PROPERTIES
                            && member == "PropertiesChanged"
                            && parameters.type_().as_str() == "(sa{sv}as)"
                            && parameters.child_value(0).str() == Some(ITEM);
                    let refresh_layout = interface == MENU
                        && matches!(member, "LayoutUpdated" | "ItemsPropertiesUpdated");
                    if !refresh_properties && !refresh_layout {
                        return;
                    }
                    let ids = state
                        .borrow()
                        .entries
                        .iter()
                        .filter(|(_, entry)| {
                            entry.owner == sender
                                && if refresh_layout {
                                    entry.menu_requested && entry.menu_path.as_deref() == Some(path)
                                } else {
                                    entry.path == path
                                }
                        })
                        .map(|(id, _)| id.clone())
                        .collect::<Vec<_>>();
                    for id in ids {
                        if refresh_layout {
                            refresh_menu(connection, &state, &cancel, &id);
                        } else {
                            refresh_item(connection, &state, &cancel, &id);
                        }
                    }
                },
            ));
        }
    }
}

impl Drop for Tray {
    #[allow(deprecated)]
    fn drop(&mut self) {
        use gio::prelude::CancellableExt;
        self.cancellable.cancel();
        for subscription in self.subscriptions.drain(..) {
            self.connection.signal_unsubscribe(subscription);
        }
        if let Some(registration) = self.registration.take() {
            let _ = self.connection.unregister_object(registration);
        }
        self.connection.call(
            Some(BUS),
            BUS_PATH,
            BUS,
            "ReleaseName",
            Some(&(WATCHER,).to_variant()),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            TIMEOUT_MS,
            gio::Cancellable::NONE,
            |_| {},
        );
    }
}

fn registration_target(sender: &str, service: &str) -> Result<(String, String)> {
    ensure!(
        gio::dbus_is_unique_name(sender),
        "registration requires a unique sender"
    );
    ensure!(service.len() <= 1024, "tray registration is too long");
    if service.starts_with('/') {
        ensure!(Variant::is_object_path(service), "invalid tray object path");
        Ok((sender.to_owned(), service.to_owned()))
    } else {
        ensure!(gio::dbus_is_name(service), "invalid tray service name");
        Ok((service.to_owned(), "/StatusNotifierItem".to_owned()))
    }
}

fn watcher_signal(connection: &gio::DBusConnection, signal: &str, id: &str) {
    let _ = connection.emit_signal(
        None,
        WATCHER_PATH,
        WATCHER,
        signal,
        Some(&(id,).to_variant()),
    );
}

fn operation_error(weak: &Weak<RefCell<State>>, message: &str) {
    if let Some(state) = weak.upgrade() {
        let mut state = state.borrow_mut();
        state.error = Some(message.to_owned());
        state.dirty = true;
    }
}

fn refresh_item(
    connection: &gio::DBusConnection,
    state: &Rc<RefCell<State>>,
    cancel: &gio::Cancellable,
    id: &str,
) {
    let (owner, path, serial) = {
        let mut state = state.borrow_mut();
        let Some(entry) = state.entries.get_mut(id) else {
            return;
        };
        if entry.refreshing {
            entry.refresh_again = true;
            return;
        }
        entry.refreshing = true;
        (entry.owner.clone(), entry.path.clone(), entry.serial)
    };
    let weak = Rc::downgrade(state);
    let next_connection = connection.clone();
    let cancel_next = cancel.clone();
    let id = id.to_owned();
    connection.call(
        Some(&owner),
        &path,
        PROPERTIES,
        "GetAll",
        Some(&(ITEM,).to_variant()),
        None,
        gio::DBusCallFlags::NO_AUTO_START,
        TIMEOUT_MS,
        Some(cancel),
        move |reply| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let parsed = reply
                .ok()
                .filter(|reply| reply.size() <= MAX_REPLY_BYTES)
                .and_then(|reply| reply.get::<(HashMap<String, Variant>,)>())
                .and_then(|(properties,)| parse_item(&id, &properties).ok());
            let (again, menu_changed, failed_initial) = {
                let mut state = state.borrow_mut();
                let Some(entry) = state
                    .entries
                    .get_mut(&id)
                    .filter(|entry| entry.serial == serial)
                else {
                    return;
                };
                entry.refreshing = false;
                let again = std::mem::take(&mut entry.refresh_again);
                let failed_initial = parsed.is_none() && entry.item.is_none();
                let mut changed = false;
                let mut menu_changed = false;
                if let Some((item, menu_path)) = parsed {
                    changed = entry.item.as_ref() != Some(&item) || entry.menu_path != menu_path;
                    menu_changed = entry.menu_path != menu_path;
                    if menu_changed {
                        entry.menu = None;
                    }
                    entry.item = Some(item);
                    entry.menu_path = menu_path;
                }
                state.dirty |= changed;
                if failed_initial {
                    state.entries.remove(&id);
                }
                (again, menu_changed, failed_initial)
            };
            if failed_initial {
                watcher_signal(&next_connection, "StatusNotifierItemUnregistered", &id);
            }
            if again {
                refresh_item(&next_connection, &state, &cancel_next, &id);
            }
            if menu_changed {
                refresh_menu(&next_connection, &state, &cancel_next, &id);
            }
        },
    );
}

fn refresh_menu(
    connection: &gio::DBusConnection,
    state: &Rc<RefCell<State>>,
    cancel: &gio::Cancellable,
    id: &str,
) {
    let (owner, path, serial) = {
        let mut state = state.borrow_mut();
        let Some(entry) = state.entries.get_mut(id) else {
            return;
        };
        if !entry.menu_requested {
            return;
        }
        if entry.menu_refreshing {
            entry.menu_refresh_again = true;
            return;
        }
        let Some(path) = entry.menu_path.clone() else {
            return;
        };
        entry.menu_refreshing = true;
        (entry.owner.clone(), path, entry.serial)
    };
    let weak = Rc::downgrade(state);
    let next_connection = connection.clone();
    let cancel_next = cancel.clone();
    let id = id.to_owned();
    let properties = vec![
        "type",
        "label",
        "enabled",
        "visible",
        "toggle-type",
        "toggle-state",
        "children-display",
    ];
    connection.call(
        Some(&owner),
        &path.clone(),
        MENU,
        "GetLayout",
        Some(&(0_i32, MAX_MENU_DEPTH as i32, properties).to_variant()),
        None,
        gio::DBusCallFlags::NO_AUTO_START,
        TIMEOUT_MS,
        Some(cancel),
        move |reply| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let parsed = reply.ok().and_then(|reply| parse_menu(&reply).ok());
            let again = {
                let mut state = state.borrow_mut();
                let Some(entry) = state
                    .entries
                    .get_mut(&id)
                    .filter(|entry| entry.serial == serial)
                else {
                    return;
                };
                entry.menu_refreshing = false;
                let again = std::mem::take(&mut entry.menu_refresh_again);
                if entry.menu_path.as_deref() == Some(&path) {
                    let changed = entry.menu != parsed;
                    entry.menu = parsed;
                    if entry.menu.is_none() {
                        state.error =
                            Some("The application tray menu is unavailable or invalid.".to_owned());
                    }
                    state.dirty |= changed || state.error.is_some();
                }
                again
            };
            if again {
                refresh_menu(&next_connection, &state, &cancel_next, &id);
            }
        },
    );
}

fn string_property(properties: &HashMap<String, Variant>, key: &str) -> Option<String> {
    properties
        .get(key)
        .and_then(Variant::str)
        .filter(|text| text.len() <= 1024 && !text.contains('\0'))
        .map(str::to_owned)
}

fn parse_item(
    id: &str,
    properties: &HashMap<String, Variant>,
) -> Result<(TrayItem, Option<String>)> {
    let status = string_property(properties, "Status").unwrap_or_else(|| "Active".to_owned());
    ensure!(
        matches!(status.as_str(), "Passive" | "Active" | "NeedsAttention"),
        "invalid tray status"
    );
    let prefix = if status == "NeedsAttention" {
        "AttentionIcon"
    } else {
        "Icon"
    };
    let icon_name = string_property(properties, &format!("{prefix}Name"))
        .filter(|name| !name.is_empty())
        .or_else(|| string_property(properties, "IconName").filter(|name| !name.is_empty()));
    let pixmap = properties
        .get(&format!("{prefix}Pixmap"))
        .and_then(parse_pixmaps)
        .or_else(|| properties.get("IconPixmap").and_then(parse_pixmaps));
    let menu_path = string_property(properties, "Menu")
        .filter(|path| path != "/" && Variant::is_object_path(path));
    let title = string_property(properties, "Title")
        .filter(|title| !title.is_empty())
        .or_else(|| string_property(properties, "Id"))
        .unwrap_or_else(|| "Application".to_owned());
    Ok((
        TrayItem {
            id: id.to_owned(),
            title,
            icon_name,
            pixmap,
            status,
            item_is_menu: properties
                .get("ItemIsMenu")
                .and_then(Variant::get::<bool>)
                .unwrap_or(false),
            has_menu: menu_path.is_some(),
        },
        menu_path,
    ))
}

fn parse_pixmaps(value: &Variant) -> Option<TrayPixmap> {
    if value.type_().as_str() != "a(iiay)"
        || value.size() > MAX_REPLY_BYTES
        || value.n_children() > 32
    {
        return None;
    }
    let mut candidates = value.get::<Vec<(i32, i32, Vec<u8>)>>()?;
    candidates.retain(|(width, height, bytes)| {
        *width > 0
            && *height > 0
            && *width <= MAX_ICON_AXIS
            && *height <= MAX_ICON_AXIS
            && bytes.len() == *width as usize * *height as usize * 4
    });
    // Prefer an image at least as large as the usual tray icon, then the nearest
    // dimensions. Never reinterpret malformed or truncated pixel arrays.
    candidates.sort_by_key(|(w, h, _)| (*w < 24 || *h < 24, (*w - 24).abs() + (*h - 24).abs()));
    let (width, height, argb) = candidates.into_iter().next()?;
    let rgba = argb
        .chunks_exact(4)
        .flat_map(|pixel| [pixel[1], pixel[2], pixel[3], pixel[0]])
        .collect();
    Some(TrayPixmap {
        width: width as u32,
        height: height as u32,
        rgba,
    })
}

fn parse_menu(reply: &Variant) -> Result<TrayMenu> {
    ensure!(
        reply.type_().as_str() == "(u(ia{sv}av))" && reply.size() <= MAX_REPLY_BYTES,
        "invalid tray menu response"
    );
    let revision = reply
        .child_value(0)
        .get::<u32>()
        .context("invalid menu revision")?;
    let mut remaining = MAX_MENU_ITEMS;
    let mut ids = HashSet::new();
    let root = parse_menu_entry(&reply.child_value(1), 0, &mut remaining, &mut ids)?;
    ensure!(root.id == 0, "menu root must be zero");
    Ok(TrayMenu {
        revision,
        entries: root.children,
    })
}

fn parse_menu_entry(
    value: &Variant,
    depth: usize,
    remaining: &mut usize,
    ids: &mut HashSet<i32>,
) -> Result<TrayMenuEntry> {
    ensure!(
        depth <= MAX_MENU_DEPTH && *remaining > 0,
        "tray menu exceeds layout limits"
    );
    *remaining -= 1;
    let (id, properties, children) = value
        .get::<(i32, HashMap<String, Variant>, Vec<Variant>)>()
        .context("invalid tray menu node")?;
    ensure!(
        id >= 0 && ids.insert(id),
        "duplicate or invalid tray menu item id"
    );
    let toggle_type = string_property(&properties, "toggle-type")
        .filter(|kind| matches!(kind.as_str(), "checkmark" | "radio"));
    let toggle_state = toggle_type.as_ref().map(|_| {
        properties
            .get("toggle-state")
            .and_then(Variant::get::<i32>)
            .filter(|state| (0..=1).contains(state))
            .unwrap_or(-1)
    });
    let label = menu_label(&string_property(&properties, "label").unwrap_or_default());
    let submenu = string_property(&properties, "children-display").as_deref() == Some("submenu")
        || !children.is_empty();
    let children = children
        .iter()
        .map(|child| parse_menu_entry(child, depth + 1, remaining, ids))
        .collect::<Result<Vec<_>>>()?;
    Ok(TrayMenuEntry {
        id,
        label,
        enabled: properties
            .get("enabled")
            .and_then(Variant::get::<bool>)
            .unwrap_or(true),
        visible: properties
            .get("visible")
            .and_then(Variant::get::<bool>)
            .unwrap_or(true),
        separator: string_property(&properties, "type").as_deref() == Some("separator"),
        submenu,
        toggle_type,
        toggle_state,
        children,
    })
}

fn menu_label(label: &str) -> String {
    let mut result = String::new();
    let mut characters = label
        .chars()
        .filter(|character| !character.is_control())
        .peekable();
    while let Some(character) = characters.next() {
        if character == '_' {
            if characters.peek() == Some(&'_') {
                result.push('_');
                characters.next();
            }
        } else {
            result.push(character);
        }
    }
    result
}

fn menu_item_actionable(entries: &[TrayMenuEntry], id: i32) -> bool {
    menu_entry(entries, id).is_some_and(|entry| !entry.submenu)
}

fn menu_entry(entries: &[TrayMenuEntry], id: i32) -> Option<&TrayMenuEntry> {
    entries
        .iter()
        .filter(|entry| entry.visible && entry.enabled && !entry.separator)
        .find_map(|entry| {
            if entry.id == id {
                Some(entry)
            } else {
                menu_entry(&entry.children, id)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::glib;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    #[test]
    fn registration_accepts_standard_names_and_sender_relative_paths() {
        assert_eq!(
            registration_target(":1.42", "/Item").unwrap(),
            (":1.42".into(), "/Item".into())
        );
        assert_eq!(
            registration_target(":1.42", "org.example.App").unwrap(),
            ("org.example.App".into(), "/StatusNotifierItem".into())
        );
        for invalid in ["", "invalid", "/bad-path", "/bad//path"] {
            assert!(registration_target(":1.42", invalid).is_err());
        }
        assert!(registration_target("org.example.App", "/Item").is_err());
    }

    #[test]
    fn pixmaps_validate_sizes_and_convert_network_argb_without_losing_alpha() {
        let wire = vec![(1_i32, 1_i32, vec![128_u8, 11, 22, 33])].to_variant();
        assert_eq!(
            parse_pixmaps(&wire),
            Some(TrayPixmap {
                width: 1,
                height: 1,
                rgba: vec![11, 22, 33, 128]
            })
        );
        for (w, h, bytes) in [
            (0, 1, vec![]),
            (1, 1, vec![0; 3]),
            (257, 1, vec![0; 1028]),
            (-1, 1, vec![0; 4]),
        ] {
            assert!(parse_pixmaps(&vec![(w, h, bytes)].to_variant()).is_none());
        }
    }

    fn node(id: i32, values: &[(&str, Variant)], children: Vec<Variant>) -> Variant {
        let properties = values
            .iter()
            .map(|(key, value)| (key.to_string(), value.clone()))
            .collect::<HashMap<_, _>>();
        (id, properties, children).to_variant()
    }

    fn layout(children: Vec<Variant>) -> Variant {
        Variant::tuple_from_iter([7_u32.to_variant(), node(0, &[], children)])
    }

    #[test]
    fn real_menu_layout_preserves_submenus_toggles_and_disabled_ancestors() {
        let menu = parse_menu(&layout(vec![
            node(1, &[("label", "_Open__file".to_variant())], vec![]),
            node(
                2,
                &[
                    ("label", "_Paused".to_variant()),
                    ("toggle-type", "checkmark".to_variant()),
                    ("toggle-state", 1_i32.to_variant()),
                ],
                vec![],
            ),
            node(
                3,
                &[("enabled", false.to_variant())],
                vec![node(4, &[("label", "Child".to_variant())], vec![])],
            ),
            node(5, &[("type", "separator".to_variant())], vec![]),
            node(6, &[("visible", false.to_variant())], vec![]),
        ]))
        .unwrap();
        assert_eq!(menu.revision, 7);
        assert_eq!(menu.entries[0].label, "Open_file");
        assert_eq!(menu.entries[1].toggle_state, Some(1));
        assert!(menu_item_actionable(&menu.entries, 1));
        for id in [3, 4, 5, 6, 99] {
            assert!(!menu_item_actionable(&menu.entries, id));
        }
    }

    #[test]
    fn menu_rejects_duplicates_depth_and_node_exhaustion() {
        assert!(parse_menu(&layout(vec![node(1, &[], vec![]), node(1, &[], vec![])])).is_err());
        let mut deep = node(20, &[], vec![]);
        for id in (1..10).rev() {
            deep = node(id, &[], vec![deep]);
        }
        assert!(parse_menu(&layout(vec![deep])).is_err());
        assert!(
            parse_menu(&layout(
                (1..=MAX_MENU_ITEMS as i32)
                    .map(|id| node(id, &[], vec![]))
                    .collect()
            ))
            .is_err()
        );
    }

    #[test]
    fn attention_status_selects_attention_icon_and_menu_path_is_validated() {
        let properties = HashMap::from([
            ("Title".into(), "Example".to_variant()),
            ("Status".into(), "NeedsAttention".to_variant()),
            ("IconName".into(), "ordinary".to_variant()),
            ("AttentionIconName".into(), "attention".to_variant()),
            ("Menu".into(), "/Menu".to_variant()),
            ("ItemIsMenu".into(), true.to_variant()),
        ]);
        let (item, menu) = parse_item(":1.4/Item", &properties).unwrap();
        assert_eq!(item.icon_name.as_deref(), Some("attention"));
        assert_eq!(menu.as_deref(), Some("/Menu"));
        assert!(item.item_is_menu && item.has_menu);
    }

    struct PrivateBus(Child);

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn pump(context: &glib::MainContext, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(4);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "private tray protocol test timed out"
            );
            while context.pending() {
                context.iteration(false);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn test_call(
        context: &glib::MainContext,
        connection: &gio::DBusConnection,
        destination: &str,
        path: &str,
        interface: &str,
        method: &str,
        parameters: &Variant,
    ) -> std::result::Result<Variant, glib::Error> {
        let result = Rc::new(RefCell::new(None));
        let completed = result.clone();
        connection.call(
            Some(destination),
            path,
            interface,
            method,
            Some(parameters),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            TIMEOUT_MS,
            gio::Cancellable::NONE,
            move |reply| *completed.borrow_mut() = Some(reply),
        );
        pump(context, || result.borrow().is_some());
        result.borrow_mut().take().unwrap()
    }

    #[test]
    fn private_bus_registration_signals_real_menu_actions_and_owner_cleanup() {
        // This bus is exclusive to the test: no DBUS_SESSION_BUS_ADDRESS changes,
        // guest service registration, user applications, or host session access.
        let child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon is required for tray protocol tests");
        let mut bus = PrivateBus(child);
        let mut address = String::new();
        BufReader::new(bus.0.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let context = glib::MainContext::new();
        context.with_thread_default(|| {
            let connect = || gio::DBusConnection::for_address_sync(address.trim(),
                gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                None, gio::Cancellable::NONE).unwrap();
            let host = connect();
            let app = connect();
            let tray = Tray::on_connection(host).unwrap();
            let properties = Rc::new(RefCell::new(HashMap::from([
                ("Title".to_owned(), "Initial app".to_variant()),
                ("Status".to_owned(), "Active".to_variant()),
                ("IconName".to_owned(), "test-icon".to_variant()),
                ("IconPixmap".to_owned(), vec![(1_i32, 1_i32, vec![255_u8, 17, 34, 51])].to_variant()),
                ("Menu".to_owned(), glib::variant::ObjectPath::try_from("/Menu").unwrap().to_variant()),
                ("ItemIsMenu".to_owned(), false.to_variant()),
            ])));
            let item_xml = r#"<node><interface name="org.kde.StatusNotifierItem">
              <property name="Title" type="s" access="read"/><property name="Status" type="s" access="read"/>
              <property name="IconName" type="s" access="read"/><property name="IconPixmap" type="a(iiay)" access="read"/>
              <property name="Menu" type="o" access="read"/><property name="ItemIsMenu" type="b" access="read"/>
              <method name="Activate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
              <method name="SecondaryActivate"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
              <method name="ContextMenu"><arg type="i" direction="in"/><arg type="i" direction="in"/></method>
            </interface></node>"#;
            let methods = Rc::new(RefCell::new(Vec::<(String, Variant)>::new()));
            let observed = methods.clone();
            let property_values = properties.clone();
            let interface = gio::DBusNodeInfo::for_xml(item_xml).unwrap().lookup_interface(ITEM).unwrap();
            let _item_registration = app.register_object("/Item", &interface)
                .property(move |_, _, _, _, name| property_values.borrow()[name].clone())
                .method_call(move |_, _, _, _, name, args, invocation| {
                    observed.borrow_mut().push((name.to_owned(), args));
                    invocation.return_value(Some(&().to_variant()));
                }).build().unwrap();
            let menu_xml = r#"<node><interface name="com.canonical.dbusmenu">
              <method name="AboutToShow"><arg type="i" direction="in"/><arg type="b" direction="out"/></method>
              <method name="GetLayout"><arg type="i" direction="in"/><arg type="i" direction="in"/><arg type="as" direction="in"/>
                <arg type="u" direction="out"/><arg type="(ia{sv}av)" direction="out"/></method>
              <method name="Event"><arg type="i" direction="in"/><arg type="s" direction="in"/>
                <arg type="v" direction="in"/><arg type="u" direction="in"/></method>
            </interface></node>"#;
            let wire_layout = Rc::new(RefCell::new(layout(vec![
                node(1, &[("label", "_Open".to_variant())], vec![]),
                node(2, &[("label", "Unavailable".to_variant()), ("enabled", false.to_variant())], vec![]),
            ])));
            let served_layout = wire_layout.clone();
            let observed = methods.clone();
            let interface = gio::DBusNodeInfo::for_xml(menu_xml).unwrap().lookup_interface(MENU).unwrap();
            let _menu_registration = app.register_object("/Menu", &interface)
                .method_call(move |_, _, _, _, name, args, invocation| {
                    observed.borrow_mut().push((name.to_owned(), args));
                    let result = match name {
                        "AboutToShow" => (true,).to_variant(),
                        "GetLayout" => served_layout.borrow().clone(),
                        _ => ().to_variant(),
                    };
                    invocation.return_value(Some(&result));
                }).build().unwrap();
            for _ in 0..2 {
                test_call(&context, &app, WATCHER, WATCHER_PATH, WATCHER, "RegisterStatusNotifierItem", &("/Item",).to_variant()).unwrap();
            }
            // A caller cannot register a different connection's item. Calls are
            // permanently directed to the registering unique owner, not an alias.
            let outsider = connect();
            assert!(test_call(&context, &outsider, WATCHER, WATCHER_PATH, WATCHER,
                "RegisterStatusNotifierItem", &(app.unique_name().unwrap().as_str(),).to_variant()).is_err());
            pump(&context, || tray.items().len() == 1);
            let item = tray.items().remove(0);
            assert_eq!(item.id, format!("{}/Item", app.unique_name().unwrap()));
            assert_eq!(item.title, "Initial app");
            assert_eq!(item.pixmap.unwrap().rgba, vec![17, 34, 51, 255]);
            assert!(tray.take_dirty());
            assert!(!tray.take_dirty());
            properties.borrow_mut().insert("Title".into(), "Updated app".to_variant());
            app.emit_signal(None, "/Item", ITEM, "NewTitle", Some(&().to_variant())).unwrap();
            pump(&context, || tray.items().first().is_some_and(|item| item.title == "Updated app"));
            tray.activate(&item.id, 10, 20).unwrap();
            tray.secondary_activate(&item.id, 10, 20).unwrap();
            tray.context_menu(&item.id, 10, 20).unwrap();
            pump(&context, || methods.borrow().iter().filter(|(name, _)| name.ends_with("Activate") || name == "ContextMenu").count() == 3);
            assert_eq!(methods.borrow()[0].1.get::<(i32, i32)>(), Some((10, 20)));
            tray.request_menu(&item.id).unwrap();
            pump(&context, || tray.menu(&item.id).is_some());
            assert_eq!(tray.menu(&item.id).unwrap().entries[0].label, "Open");
            assert!(tray.activate_menu_item(&item.id, 2, 77).is_err());
            tray.activate_menu_item(&item.id, 1, 77).unwrap();
            pump(&context, || methods.borrow().iter().any(|(name, _)| name == "Event"));
            let event = methods.borrow().iter().find(|(name, _)| name == "Event").unwrap().1.clone();
            let (menu_id, event_id, data, time) = event.get::<(i32, String, Variant, u32)>().unwrap();
            assert_eq!((menu_id, event_id.as_str(), data.get::<i32>(), time), (1, "clicked", Some(0), 77));
            *wire_layout.borrow_mut() = layout(vec![node(3, &[("label", "Changed action".to_variant())], vec![])]);
            app.emit_signal(None, "/Menu", MENU, "LayoutUpdated", Some(&(8_u32, 0_i32).to_variant())).unwrap();
            pump(&context, || tray.menu(&item.id).is_some_and(|menu| menu.entries[0].id == 3));
            assert!(tray.activate_menu_item(&item.id, 1, 78).is_err());
            *wire_layout.borrow_mut() = layout(vec![node(4, &[
                ("label", "Lazy submenu".to_variant()), ("children-display", "submenu".to_variant()),
            ], vec![])]);
            app.emit_signal(None, "/Menu", MENU, "ItemsPropertiesUpdated",
                Some(&(Vec::<(i32, HashMap<String, Variant>)>::new(), Vec::<(i32, Vec<String>)>::new()).to_variant())).unwrap();
            pump(&context, || tray.menu(&item.id).is_some_and(|menu| menu.entries[0].id == 4));
            assert!(tray.activate_menu_item(&item.id, 4, 79).is_err());
            tray.request_submenu(&item.id, 4).unwrap();
            pump(&context, || methods.borrow().iter().any(|(method, args)| method == "AboutToShow" && args.get::<(i32,)>() == Some((4,))));
            tray.close_menu(&item.id);
            assert!(!tray.state.borrow().entries[&item.id].menu_requested);
            // Malformed or unrelated PropertiesChanged signals never index an
            // unchecked variant or change a registered item.
            app.emit_signal(None, "/Item", PROPERTIES, "PropertiesChanged", Some(&().to_variant())).unwrap();
            properties.borrow_mut().insert("Status".into(), "Passive".to_variant());
            app.emit_signal(None, "/Item", ITEM, "NewStatus", Some(&("Passive",).to_variant())).unwrap();
            pump(&context, || tray.items().is_empty());
            assert_eq!(tray.state.borrow().entries.len(), 1);
            app.close_sync(gio::Cancellable::NONE).unwrap();
            pump(&context, || tray.state.borrow().entries.is_empty());
            assert!(tray.activate(&item.id, 0, 0).is_err());
            drop(tray);
            while context.pending() { context.iteration(false); }
        }).unwrap();
    }
}
