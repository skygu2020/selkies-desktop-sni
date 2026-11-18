//! StatusNotifierWatcher implementation.
//!
//! This module implements the `org.kde.StatusNotifierWatcher` D-Bus interface.
//! The watcher is the central registry on the session bus:
//!
//! - Tray items call `RegisterStatusNotifierItem` to announce themselves.
//! - The host calls `RegisterStatusNotifierHost` to announce itself.
//! - The watcher emits `StatusNotifierItemRegistered` /
//!   `StatusNotifierItemUnregistered` signals so the host knows when to
//!   update its tray representation.
//!
//! The watcher emits `StatusNotifierHostRegistered` at startup so that
//! already-running items may re-register via `RegisterStatusNotifierItem`.
//!
//! Additionally it subscribes to:
//! - `NameOwnerChanged` to detect items vanishing from the bus.
//! - `com.canonical.dbusmenu` signals for menu layout updates.

use std::collections::HashMap;

use rustbus::connection::Timeout;
use rustbus::connection::ll_conn::DuplexConn;
use rustbus::message_builder::MarshalledMessage;
use rustbus::standard_messages;

use crate::item::{ItemId, TrayItem};
use crate::{Result, TrayEvent};

pub(crate) const WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
pub(crate) const WATCHER_PATH: &str = "/StatusNotifierWatcher";

const WATCHER_INTROSPECT_XML: &str = include_str!("../protocols/org.kde.StatusNotifierWatcher.xml");

/// Request the watcher bus names and set up signal subscriptions.
pub fn register(conn: &mut DuplexConn) -> Result<()> {
    // Request both the freedesktop and KDE well-known names
    for name in [
        "org.freedesktop.StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    ] {
        let msg = standard_messages::request_name(name, 0);
        let serial = conn.send.send_message_write_all(&msg)?;
        // Wait for the reply matching our serial; skip unrelated signals.
        loop {
            let resp = conn.recv.get_next_message(Timeout::Infinite)?;
            if resp.dynheader.response_serial == Some(serial) {
                match resp.typ {
                    rustbus::message_builder::MessageType::Reply => {
                        // Check the reply body: 1 = PRIMARY_OWNER, 4 = ALREADY_OWNER.
                        // 2 = IN_QUEUE and 3 = EXISTS mean another watcher is active.
                        let result: u32 = resp.body.parser().get().unwrap_or(0);
                        if result != 1 && result != 4 {
                            return Err(crate::Error::WatcherAlreadyRunning);
                        }
                        break;
                    }
                    _ => return Err(crate::Error::WatcherAlreadyRunning),
                }
            }
            if !matches!(resp.typ, rustbus::message_builder::MessageType::Signal) {
                break; // unexpected — abort spin
            }
        }
    }

    // Subscribe to NameOwnerChanged so we know when items vanish
    let add_match = standard_messages::add_match(
        "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged'",
    );
    conn.send.send_message_write_all(&add_match)?;

    // Subscribe to dbusmenu signals (LayoutUpdated, ItemsPropertiesUpdated)
    let add_match =
        standard_messages::add_match("type='signal',interface='com.canonical.dbusmenu'");
    conn.send.send_message_write_all(&add_match)?;

    // Subscribe to StatusNotifierItem property-change notifications (e.g. Fcitx5 NewIcon).
    let add_match = standard_messages::add_match(
        "type='signal',interface='org.kde.StatusNotifierItem'",
    );
    conn.send.send_message_write_all(&add_match)?;

    Ok(())
}

/// Handle an incoming method call.
pub fn handle_call(
    conn: &mut DuplexConn,
    msg: &MarshalledMessage,
    items: &mut HashMap<ItemId, TrayItem>,
    events: &mut Vec<TrayEvent>,
    pending: &mut std::collections::VecDeque<rustbus::message_builder::MarshalledMessage>,
) -> Result<()> {
    let iface = msg.dynheader.interface.as_deref().unwrap_or("");
    let member = msg.dynheader.member.as_deref().unwrap_or("");
    let object = msg.dynheader.object.as_deref().unwrap_or("");

    // Handle Introspectable on any object path (D-Bus spec requirement).
    // The dbus-daemon and bus clients often introspect "/" to discover
    // available object trees.
    if iface == "org.freedesktop.DBus.Introspectable" && member == "Introspect" {
        let xml = if object == WATCHER_PATH {
            WATCHER_INTROSPECT_XML
        } else {
            r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <node name="StatusNotifierWatcher"/>
</node>"#
        };
        let mut reply = msg.dynheader.make_response();
        reply.body.push_param(xml).unwrap();
        conn.send.send_message_write_all(&reply)?;
        return Ok(());
    }

    if object != WATCHER_PATH {
        let err = msg.dynheader.make_error_response(
            "org.freedesktop.DBus.Error.UnknownObject".to_owned(),
            Some(format!(
                "This service does not implement the object path: {object}"
            )),
        );
        conn.send.send_message_write_all(&err)?;
        return Ok(());
    }

    match iface {
        WATCHER_INTERFACE => handle_watcher_call(conn, msg, member, items, events, pending),
        "org.freedesktop.DBus.Properties" => handle_properties_call(conn, msg, member, items),
        _ => {
            let err = standard_messages::unknown_method(&msg.dynheader);
            conn.send.send_message_write_all(&err)?;
            Ok(())
        }
    }
}

fn handle_watcher_call(
    conn: &mut DuplexConn,
    msg: &MarshalledMessage,
    member: &str,
    items: &mut HashMap<ItemId, TrayItem>,
    events: &mut Vec<TrayEvent>,
    pending: &mut std::collections::VecDeque<rustbus::message_builder::MarshalledMessage>,
) -> Result<()> {
    match member {
        "RegisterStatusNotifierItem" => {
            let service: String = msg.body.parser().get()?;
            let sender = msg.dynheader.sender.as_deref().unwrap_or("");
            let (bus_name, object_path) = parse_service(sender, &service);

            // Build service_id: sender+path if only a path was given, else the raw service
            let service_id = if service.starts_with('/') {
                format!("{sender}{service}")
            } else {
                service.clone()
            };

            // Read item properties from the bus
            match TrayItem::from_bus_get_all(conn, &service_id, &bus_name, &object_path, pending) {
                Ok(item) => {
                    // Deduplicate: if this bus_name is already known (e.g. from step 3
                    // unique-name probe), skip the insert but still emit ItemChanged.
                    let is_new = !items.contains_key(&ItemId(service_id.clone()))
                        && !items.values().any(|existing| existing.bus_name == bus_name);
                    if is_new {
                        let id = item.id.clone();
                        items.insert(id.clone(), item);
                        events.push(TrayEvent::ItemAdded(id));
                    }

                    // Emit StatusNotifierItemRegistered signal
                    let mut sig = rustbus::MessageBuilder::new()
                        .signal(
                            WATCHER_INTERFACE,
                            "StatusNotifierItemRegistered",
                            WATCHER_PATH,
                        )
                        .build();
                    sig.body.push_param(&service_id as &str).unwrap();
                    conn.send.send_message_write_all(&sig)?;
                }
                Err(_e) => {}
            }

            // Send empty reply
            let reply = msg.dynheader.make_response();
            conn.send.send_message_write_all(&reply)?;
        }
        "RegisterStatusNotifierHost" => {
            let reply = msg.dynheader.make_response();
            conn.send.send_message_write_all(&reply)?;
        }
        "GetHostServiceName" => {
            let mut reply = msg.dynheader.make_response();
            let host_name = format!("org.freedesktop.StatusNotifierHost-{}", std::process::id());
            reply.body.push_param(host_name.as_str()).unwrap();
            conn.send.send_message_write_all(&reply)?;
        }
        _ => {
            let err = standard_messages::unknown_method(&msg.dynheader);
            conn.send.send_message_write_all(&err)?;
        }
    }
    Ok(())
}

fn handle_properties_call(
    conn: &mut DuplexConn,
    msg: &MarshalledMessage,
    member: &str,
    items: &mut HashMap<ItemId, TrayItem>,
) -> Result<()> {
    match member {
        "Get" => {
            let mut parser = msg.body.parser();
            let iface: &str = parser.get()?;
            let prop: &str = parser.get()?;

            // Only handle properties for our watcher interface
            if iface != WATCHER_INTERFACE {
                let err = msg.dynheader.make_error_response(
                    "org.freedesktop.DBus.Error.UnknownProperty".to_owned(),
                    Some(format!("Unknown interface: {iface}")),
                );
                conn.send.send_message_write_all(&err)?;
                return Ok(());
            }

            let mut reply = msg.dynheader.make_response();
            match prop {
                "RegisteredStatusNotifierItems" => {
                    let names: Vec<&str> = items.keys().map(|id| id.0.as_str()).collect();
                    reply.body.push_variant(names).unwrap();
                }
                "IsStatusNotifierHostRegistered" => {
                    reply.body.push_variant(true).unwrap();
                }
                "ProtocolVersion" => {
                    reply.body.push_variant(0i32).unwrap();
                }
                _ => {
                    let err = msg.dynheader.make_error_response(
                        "org.freedesktop.DBus.Error.UnknownProperty".to_owned(),
                        Some(format!("Unknown property: {prop}")),
                    );
                    conn.send.send_message_write_all(&err)?;
                    return Ok(());
                }
            }
            conn.send.send_message_write_all(&reply)?;
        }
        "GetAll" => {
            let mut reply = msg.dynheader.make_response();
            let names: Vec<&str> = items.keys().map(|id| id.0.as_str()).collect();
            // Build a dict of all watcher properties: a{sv}
            use rustbus::params::{Base as PBase, Container, Dict, DictMap, Param};
            use rustbus::signature::{Base as SBase, Container as SContainer, Type};
            let mut dict = DictMap::new();
            dict.insert(
                PBase::StringRef("RegisteredStatusNotifierItems"),
                Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
                    sig: Type::Container(SContainer::Array(Box::new(Type::Base(SBase::String)))),
                    value: Param::Container(Container::Array(rustbus::params::Array {
                        element_sig: Type::Base(SBase::String),
                        values: names
                            .into_iter()
                            .map(|n| Param::Base(PBase::StringRef(n)))
                            .collect(),
                    })),
                }))),
            );
            dict.insert(
                PBase::StringRef("IsStatusNotifierHostRegistered"),
                Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
                    sig: Type::Base(SBase::Boolean),
                    value: Param::Base(PBase::Boolean(true)),
                }))),
            );
            dict.insert(
                PBase::StringRef("ProtocolVersion"),
                Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
                    sig: Type::Base(SBase::Int32),
                    value: Param::Base(PBase::Int32(0)),
                }))),
            );
            reply
                .body
                .push_old_param(&Param::Container(Container::Dict(Dict {
                    key_sig: SBase::String,
                    value_sig: Type::Container(SContainer::Variant),
                    map: dict,
                })))
                .unwrap();
            conn.send.send_message_write_all(&reply)?;
        }
        _ => {
            let err = standard_messages::unknown_method(&msg.dynheader);
            conn.send.send_message_write_all(&err)?;
        }
    }
    Ok(())
}

/// Handle incoming signals (NameOwnerChanged, item property-change signals).
pub fn handle_signal(
    conn: &mut DuplexConn,
    msg: &MarshalledMessage,
    items: &mut HashMap<ItemId, TrayItem>,
    events: &mut Vec<TrayEvent>,
    pending: &mut std::collections::VecDeque<rustbus::message_builder::MarshalledMessage>,
) -> Result<()> {
    let iface = msg.dynheader.interface.as_deref().unwrap_or("");
    let member = msg.dynheader.member.as_deref().unwrap_or("");
    if iface == "org.freedesktop.DBus" && member == "NameOwnerChanged" {
        let mut parser = msg.body.parser();
        let name: String = parser.get()?;
        let _old_owner: String = parser.get()?;
        let new_owner: String = parser.get()?;

        // ── Name disappeared from the bus ───────────────────────
        if new_owner.is_empty() {
            let ids_to_remove: Vec<ItemId> = items
                .values()
                .filter(|item| item.bus_name == name)
                .map(|item| item.id.clone())
                .collect();
            for id in ids_to_remove {
                let service_id = id.0.clone();
                items.remove(&id);
                events.push(TrayEvent::ItemRemoved(id));

                // Emit StatusNotifierItemUnregistered signal
                let mut sig = rustbus::MessageBuilder::new()
                    .signal(
                        WATCHER_INTERFACE,
                        "StatusNotifierItemUnregistered",
                        WATCHER_PATH,
                    )
                    .build();
                sig.body.push_param(service_id.as_str()).unwrap();
                conn.send.send_message_write_all(&sig)?;
            }
        }
    } else if iface == "org.kde.StatusNotifierItem" {
        let sender = msg.dynheader.sender.as_deref().unwrap_or("");
        if !sender.is_empty() {
            // Find the existing item to preserve its service_id and object_path
            let existing = items.values().find(|item| item.bus_name == sender);
            if let Some(old) = existing {
                let service_id = old.id.0.clone();
                let object_path = old.object_path.clone();
                match TrayItem::from_bus_get_all(conn, &service_id, sender, &object_path, pending) {
                    Ok(item) => {
                        let id = item.id.clone();
                        items.insert(id.clone(), item);
                        events.push(TrayEvent::ItemChanged(id));
                    }
                    Err(_e) => {}
                }
            }
        }
    } else if iface == "com.canonical.dbusmenu"
        && (member == "LayoutUpdated" || member == "ItemsPropertiesUpdated")
    {
        // Match signal sender to an item's bus_name to emit MenuChanged
        let sender = msg.dynheader.sender.as_deref().unwrap_or("");
        if !sender.is_empty() {
            let ids: Vec<ItemId> = items
                .values()
                .filter(|item| item.bus_name == sender)
                .map(|item| item.id.clone())
                .collect();
            for id in ids {
                events.push(TrayEvent::MenuChanged(id));
            }
        }
    } else if iface == "com.canonical.dbusmenu" && member == "ItemActivationRequested" {
        let sender = msg.dynheader.sender.as_deref().unwrap_or("");
        if !sender.is_empty() {
            let ids: Vec<ItemId> = items
                .values()
                .filter(|item| item.bus_name == sender)
                .map(|item| item.id.clone())
                .collect();
            for id in ids {
                events.push(TrayEvent::MenuActivationRequested(id));
            }
        }
    }

    Ok(())
}

/// Split a raw SNI service registration string into `(bus_name, object_path)`.
///
/// The SNI spec allows three forms:
/// - `"com.example.App"` → bus_name, default path `/StatusNotifierItem`
/// - `"/StatusNotifierItem"` → sender's bus name, given path
/// - `"com.example.App/SomePath"` → bus_name, given path
fn parse_service(sender: &str, service: &str) -> (String, String) {
    if service.starts_with('/') {
        (sender.to_owned(), service.to_owned())
    } else if let Some(slash) = service.find('/') {
        (service[..slash].to_owned(), service[slash..].to_owned())
    } else {
        (
            normalize_item_name(service),
            "/StatusNotifierItem".to_owned(),
        )
    }
}

/// Normalize the service name passed to RegisterStatusNotifierItem.
///
/// Accepts:
/// - Well-known names with suffix: `org.freedesktop.StatusNotifierItem-PID-ID`
/// - KDE variant: `org.kde.StatusNotifierItem-PID-ID`
/// - Unique bus names: `:1.42`
/// - Bare PID-ID: `4077-1`
fn normalize_item_name(name: &str) -> String {
    if name.starts_with("org.freedesktop.StatusNotifierItem")
        || name.starts_with("org.kde.StatusNotifierItem")
        || name.starts_with(':')
    {
        return name.to_owned();
    }
    format!("org.freedesktop.StatusNotifierItem-{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_full_name() {
        let input = "org.freedesktop.StatusNotifierItem-4077-1";
        assert_eq!(normalize_item_name(input), input);
    }

    #[test]
    fn normalize_unique_name() {
        assert_eq!(normalize_item_name(":1.42"), ":1.42");
    }

    #[test]
    fn normalize_pid_id() {
        assert_eq!(
            normalize_item_name("4077-1"),
            "org.freedesktop.StatusNotifierItem-4077-1"
        );
    }

    #[test]
    fn parse_service_uses_sender_for_path_only() {
        let (bus, path) = parse_service(":1.42", "/StatusNotifierItem");
        assert_eq!(bus, ":1.42");
        assert_eq!(path, "/StatusNotifierItem");
    }

    #[test]
    fn parse_service_bus_name_and_path() {
        let (bus, path) = parse_service(":1.42", "com.example.App/SomePath");
        assert_eq!(bus, "com.example.App");
        assert_eq!(path, "/SomePath");
    }

    #[test]
    fn parse_service_just_bus_name() {
        let (bus, path) = parse_service(":1.42", "org.freedesktop.StatusNotifierItem-4077-1");
        assert_eq!(bus, "org.freedesktop.StatusNotifierItem-4077-1");
        assert_eq!(path, "/StatusNotifierItem");
    }

    #[test]
    fn service_id_preserved_on_path_only_registration() {
        // When a path-only registration comes from sender :1.42,
        // the service_id should be sender + path
        let sender = ":1.42";
        let service = "/StatusNotifierItem";
        let service_id = if service.starts_with('/') {
            format!("{sender}{service}")
        } else {
            service.to_owned()
        };
        assert_eq!(service_id, ":1.42/StatusNotifierItem");
    }

    #[test]
    fn normalize_freedesktop_exact() {
        assert_eq!(
            normalize_item_name("org.freedesktop.StatusNotifierItem"),
            "org.freedesktop.StatusNotifierItem"
        );
    }

    #[test]
    fn normalize_kde_exact() {
        assert_eq!(
            normalize_item_name("org.kde.StatusNotifierItem"),
            "org.kde.StatusNotifierItem"
        );
    }
}
