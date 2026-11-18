//! `com.canonical.dbusmenu` support.
//!
//! This module implements the client side of the [DBusMenu] protocol, used by
//! StatusNotifierItem to expose hierarchical menus over D-Bus.
//!
//! The key entry points are:
//! - [`get_layout`] — fetch the full menu tree (recursive `MenuNode`s).
//! - [`get_group_properties`] / [`get_property`] — read item properties.
//! - [`event`] / [`fire_click`] — send click/hover events to menu items.
//!
//! Menu items are identified by numeric IDs and carry a dictionary of
//! properties (label, icon, enabled, visible, toggle state, etc.).
//!
//! [DBusMenu]: https://wiki.ubuntu.com/DesktopExperienceTeam/ApplicationIndicators

use rustbus::connection::Timeout;
use rustbus::connection::ll_conn::DuplexConn;
use rustbus::params::{Base, Container, Param};

use crate::Result;

/// An owned property value from the DBusMenu protocol.
///
/// Matches the variant types used in DBusMenu property dictionaries.
#[derive(Debug, Clone)]
pub enum PropValue {
    /// A string property (e.g. `"label"`, `"icon-name"`, `"toggle-type"`).
    Str(String),
    /// A boolean property (e.g. `"enabled"`, `"visible"`).
    Bool(bool),
    /// An integer property (e.g. `"toggle-state"`, item id).
    Int(i32),
    /// A byte-array property (e.g. `"icon-data"` as raw PNG).
    Bytes(Vec<u8>),
}

/// Properties for a single menu item as returned by
/// [`TrayHost::get_menu_group_properties`](crate::TrayHost::get_menu_group_properties).
#[derive(Debug, Clone)]
pub struct MenuItemProps {
    /// Numeric ID of the menu item.
    pub id: i32,
    /// Key-value property pairs (property name → value).
    pub props: Vec<(String, PropValue)>,
}

/// A node in the DBusMenu layout tree as returned by
/// [`TrayHost::get_menu`](crate::TrayHost::get_menu).
///
/// Nodes are arranged recursively — each node may have `children` that are
/// themselves `MenuNode`s, forming the full menu hierarchy.
#[derive(Debug, Clone)]
pub struct MenuNode {
    /// Numeric ID of this menu item.
    pub id: i32,
    /// Display label (underscores indicate access keys per freedesktop convention).
    pub label: String,
    /// Whether the item can be activated.
    pub enabled: bool,
    /// Whether the item is visible in the menu.
    pub visible: bool,
    /// Freedesktop-compliant icon name.
    pub icon_name: String,
    /// Raw icon data (typically PNG bytes).
    pub icon_data: Vec<u8>,
    /// Toggle behaviour: `""` (none), `"checkmark"`, or `"radio"`.
    pub toggle_type: String,
    /// Toggle state: `0` (off), `1` (on), or `-1` (indeterminate / no toggle).
    pub toggle_state: i32,
    /// Whether this node has children or has `children-display` set.
    pub is_submenu: bool,
    /// Child menu items (populated when `is_submenu` is true).
    pub children: Vec<MenuNode>,
}

/// Fetch properties for one or more menu items.
///
/// Wraps `com.canonical.dbusmenu.GetGroupProperties`. Returns a vector of
/// [`MenuItemProps`], one per requested ID. Non-existent IDs are silently
/// skipped.
///
/// If the returned `Vec` is empty, the server either didn't recognise the
/// IDs or returned no data.
pub fn get_group_properties(
    conn: &mut DuplexConn,
    bus_name: &str,
    menu_path: &str,
    ids: &[i32],
    property_names: &[&str],
) -> Result<Vec<MenuItemProps>> {
    let mut call = rustbus::MessageBuilder::new()
        .call("GetGroupProperties")
        .on(menu_path)
        .with_interface("com.canonical.dbusmenu")
        .at(bus_name)
        .build();
    call.body.push_param(ids).unwrap();
    call.body.push_param(property_names).unwrap();

    let serial = conn.send.send_message_write_all(&call)?;
    let resp = loop {
        let resp = conn.recv.get_next_message(Timeout::Infinite)?;
        if resp.typ == rustbus::message_builder::MessageType::Reply {
            break resp;
        }
        if resp.typ == rustbus::message_builder::MessageType::Error {
            return Ok(Vec::new());
        }
    };
    if resp.dynheader.response_serial != Some(serial) {
        return Ok(Vec::new());
    }

    let mut parser = resp.body.parser();
    let param = match parser.get_param() {
        Ok(p) => p,
        Err(_) => return Ok(Vec::new()),
    };

    let array = match &param {
        Param::Container(Container::Array(arr)) => &arr.values,
        _ => return Ok(Vec::new()),
    };

    let mut result = Vec::new();
    for elem in array {
        let fields = match elem {
            Param::Container(Container::Struct(s)) => s,
            _ => continue,
        };
        if fields.len() < 2 {
            continue;
        }
        let id = match &fields[0] {
            Param::Base(Base::Int32(v)) => *v,
            _ => continue,
        };
        let props = match &fields[1] {
            Param::Container(Container::Dict(d)) => convert_props(&d.map),
            _ => continue,
        };
        result.push(MenuItemProps { id, props });
    }
    Ok(result)
}

/// Fetch a single menu item property.
///
/// Wraps `com.canonical.dbusmenu.GetProperty`. This is mainly useful for
/// debugging — for bulk access prefer [`get_group_properties`].
///
/// Returns `None` if the property doesn't exist or the server returned an
/// error reply.
pub fn get_property(
    conn: &mut DuplexConn,
    bus_name: &str,
    menu_path: &str,
    id: i32,
    name: &str,
) -> Result<Option<PropValue>> {
    let mut call = rustbus::MessageBuilder::new()
        .call("GetProperty")
        .on(menu_path)
        .with_interface("com.canonical.dbusmenu")
        .at(bus_name)
        .build();
    call.body.push_param(id).unwrap();
    call.body.push_param(name).unwrap();

    let serial = conn.send.send_message_write_all(&call)?;
    let resp = loop {
        let resp = conn.recv.get_next_message(Timeout::Infinite)?;
        if resp.typ == rustbus::message_builder::MessageType::Reply {
            break resp;
        }
        if resp.typ == rustbus::message_builder::MessageType::Error {
            return Ok(None);
        }
    };
    if resp.dynheader.response_serial != Some(serial) {
        return Ok(None);
    }

    let mut parser = resp.body.parser();
    let param = match parser.get_param() {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };

    Ok(extract_prop_value(&param))
}

/// Fetch the menu layout tree starting from `parent_id`.
///
/// Wraps `com.canonical.dbusmenu.GetLayout` with `recursion_depth = -1`
/// (unlimited) and requests all properties.
///
/// Returns the root-level menu items as a `Vec<MenuNode>`. Each node may
/// contain children, forming a recursive tree.
///
/// # Arguments
///
/// * `parent_id` — fetch children of this item. Pass `0` to get the root
///   layout. Pass an item's ID to get its submenu.
pub fn get_layout(
    conn: &mut DuplexConn,
    bus_name: &str,
    menu_path: &str,
    parent_id: i32,
) -> Result<Vec<MenuNode>> {
    let mut call = rustbus::MessageBuilder::new()
        .call("GetLayout")
        .on(menu_path)
        .with_interface("com.canonical.dbusmenu")
        .at(bus_name)
        .build();
    call.body.push_param(parent_id).unwrap();
    call.body.push_param(-1i32).unwrap(); // recursion_depth (-1 = unlimited)
    call.body.push_param(Vec::<&str>::new()).unwrap(); // all properties

    let serial = conn.send.send_message_write_all(&call)?;
    // Skip signals until reply
    let resp = loop {
        let resp = conn.recv.get_next_message(Timeout::Infinite)?;
        if resp.typ == rustbus::message_builder::MessageType::Reply {
            break resp;
        }
        if resp.typ == rustbus::message_builder::MessageType::Error {
            return Ok(Vec::new());
        }
    };
    if resp.dynheader.response_serial != Some(serial) {
        return Ok(Vec::new());
    }

    // Response is (u32 revision, (i32 id, a{sv} props, av children))
    let mut parser = resp.body.parser();
    let _revision: u32 = parser.get()?;
    let root = match parser.get_param() {
        Ok(p) => p,
        Err(_) => return Ok(Vec::new()),
    };

    // root is a struct (i32, a{sv}, av)
    let fields = match &root {
        Param::Container(Container::Struct(s)) => s,
        _ => return Ok(Vec::new()),
    };
    if fields.len() < 3 {
        return Ok(Vec::new());
    }

    // children is the third field: av (array of variants)
    let children = match &fields[2] {
        Param::Container(Container::Array(arr)) => &arr.values,
        _ => return Ok(Vec::new()),
    };

    Ok(children.iter().filter_map(parse_menu_node).collect())
}

/// Fire a click event on a menu item using a standalone D-Bus connection.
///
/// This creates a fresh D-Bus connection, so it works without a [`TrayHost`].
/// Useful from background threads or when you only have the item's bus name
/// and menu path.
///
/// [`TrayHost`]: crate::TrayHost
pub fn fire_click(bus_name: &str, menu_path: &str, menu_id: i32) -> Result<()> {
    let mut conn = rustbus::connection::ll_conn::DuplexConn::connect_to_bus(
        rustbus::get_session_bus_path()?,
        false,
    )?;
    conn.send_hello(rustbus::connection::Timeout::Infinite)?;
    event(&mut conn, bus_name, menu_path, menu_id, "clicked")
}

/// Send an event notification to a menu item.
///
/// Wraps `com.canonical.dbusmenu.Event`. Common event types:
/// - `"clicked"` — the item was clicked
/// - `"hovered"` — the item was hovered
///
/// Vendor-specific events can be prefixed with `"x-<vendor>-"`.
///
/// Many servers don't reply to `Event` calls. This function waits at most
/// 100 ms for a reply, treating timeout as success.
pub fn event(
    conn: &mut DuplexConn,
    bus_name: &str,
    menu_path: &str,
    id: i32,
    event_id: &str,
) -> Result<()> {
    let mut call = rustbus::MessageBuilder::new()
        .call("Event")
        .on(menu_path)
        .with_interface("com.canonical.dbusmenu")
        .at(bus_name)
        .build();
    call.body.push_param(id).unwrap();
    call.body.push_param(event_id).unwrap();
    // data: variant containing i32(0)
    call.body.push_variant(0i32).unwrap();
    call.body.push_param(0u32).unwrap(); // timestamp

    let serial = conn.send.send_message_write_all(&call)?;
    // Wait briefly for a reply; ignore timeout (many servers don't reply to Event)
    loop {
        match conn
            .recv
            .get_next_message(Timeout::Duration(std::time::Duration::from_millis(100)))
        {
            Ok(resp) => {
                if resp.typ == rustbus::message_builder::MessageType::Reply
                    && resp.dynheader.response_serial == Some(serial)
                {
                    return Ok(());
                }
                if resp.typ == rustbus::message_builder::MessageType::Error
                    && resp.dynheader.response_serial == Some(serial)
                {
                    let err_name: String = resp.body.parser().get().unwrap_or_default();
                    return Err(crate::Error::MethodCall(err_name));
                }
            }
            Err(rustbus::connection::Error::TimedOut) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}

/// Parse a variant containing (i32, a{sv}, av) into a MenuNode.
fn parse_menu_node(param: &Param) -> Option<MenuNode> {
    let variant_inner = match param {
        Param::Container(Container::Variant(v)) => &v.value,
        _ => return None,
    };

    // variant_inner is Param - match it directly
    let fields = match variant_inner {
        Param::Container(Container::Struct(s)) => s.as_slice(),
        _ => return None,
    };
    if fields.len() < 3 {
        return None;
    }

    let id = match &fields[0] {
        Param::Base(Base::Int32(v)) => *v,
        _ => return None,
    };

    let props = match &fields[1] {
        Param::Container(Container::Dict(d)) => &d.map,
        _ => return None,
    };

    let label = get_str_prop(props, "label");
    let enabled = get_bool_prop(props, "enabled").unwrap_or(true);
    let visible = get_bool_prop(props, "visible").unwrap_or(true);
    let icon_name = get_str_prop(props, "icon-name");
    let icon_data = get_bytes_prop(props, "icon-data");
    let toggle_type = get_str_prop(props, "toggle-type");
    let toggle_state = get_int_prop(props, "toggle-state").unwrap_or(-1);
    let is_submenu = props.iter().any(|(k, _)| match k {
        Base::StringRef(s) => *s == "children-display",
        Base::String(s) => s == "children-display",
        _ => false,
    });

    let children_param = match &fields[2] {
        Param::Container(Container::Array(arr)) => &arr.values,
        _ => return None,
    };
    let children: Vec<MenuNode> = children_param.iter().filter_map(parse_menu_node).collect();

    Some(MenuNode {
        id,
        label,
        enabled,
        visible,
        icon_name,
        icon_data,
        toggle_type,
        toggle_state,
        is_submenu: is_submenu || !children.is_empty(),
        children,
    })
}

/// Look up a key in a DictMap and return the unwrapped variant inner value.
fn get_variant<'a>(props: &'a rustbus::params::DictMap, key: &str) -> Option<&'a Param<'a, 'a>> {
    for (k, v) in props {
        let k_str = match k {
            Base::StringRef(s) => *s,
            Base::String(s) => s.as_str(),
            _ => continue,
        };
        if k_str != key {
            continue;
        }
        if let Param::Container(Container::Variant(var)) = v {
            return Some(&var.value);
        }
    }
    None
}

fn get_str_prop(props: &rustbus::params::DictMap, key: &str) -> String {
    match get_variant(props, key) {
        Some(Param::Base(Base::StringRef(s))) => s.to_string(),
        Some(Param::Base(Base::String(s))) => s.clone(),
        _ => String::new(),
    }
}

fn get_bool_prop(props: &rustbus::params::DictMap, key: &str) -> Option<bool> {
    match get_variant(props, key) {
        Some(Param::Base(Base::Boolean(b))) => Some(*b),
        _ => None,
    }
}

fn get_int_prop(props: &rustbus::params::DictMap, key: &str) -> Option<i32> {
    match get_variant(props, key) {
        Some(Param::Base(Base::Int32(n))) => Some(*n),
        _ => None,
    }
}

fn get_bytes_prop(props: &rustbus::params::DictMap, key: &str) -> Vec<u8> {
    match get_variant(props, key) {
        Some(Param::Container(Container::Array(arr))) => arr
            .values
            .iter()
            .filter_map(|b| {
                if let Param::Base(Base::Byte(v)) = b {
                    Some(*v)
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Convert a borrowed DictMap into owned (String, PropValue) pairs.
fn convert_props(dict: &rustbus::params::DictMap) -> Vec<(String, PropValue)> {
    let mut result = Vec::new();
    for (k, v) in dict {
        let key = match k {
            Base::StringRef(s) => s.to_string(),
            Base::String(s) => s.clone(),
            _ => continue,
        };
        if let Some(val) = extract_prop_value(v) {
            result.push((key, val));
        }
    }
    result
}

/// Extract an owned PropValue from a Param (expects a variant wrapper).
fn extract_prop_value(param: &Param) -> Option<PropValue> {
    let inner = match param {
        Param::Container(Container::Variant(v)) => &v.value,
        other => other,
    };
    match inner {
        Param::Base(Base::StringRef(s)) => Some(PropValue::Str(s.to_string())),
        Param::Base(Base::String(s)) => Some(PropValue::Str(s.clone())),
        Param::Base(Base::Boolean(b)) => Some(PropValue::Bool(*b)),
        Param::Base(Base::Int32(n)) => Some(PropValue::Int(*n)),
        Param::Base(Base::Byte(b)) => Some(PropValue::Bytes(vec![*b])),
        Param::Container(Container::Array(arr)) => {
            let bytes: Vec<u8> = arr
                .values
                .iter()
                .filter_map(|e| {
                    if let Param::Base(Base::Byte(b)) = e {
                        Some(*b)
                    } else {
                        None
                    }
                })
                .collect();
            if bytes.len() == arr.values.len() {
                Some(PropValue::Bytes(bytes))
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_variant_str(s: &str) -> Param<'static, 'static> {
        Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
            sig: rustbus::signature::Type::Base(rustbus::signature::Base::String),
            value: Param::Base(Base::String(s.to_owned())),
        })))
    }

    fn make_variant_bool(b: bool) -> Param<'static, 'static> {
        Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
            sig: rustbus::signature::Type::Base(rustbus::signature::Base::Boolean),
            value: Param::Base(Base::Boolean(b)),
        })))
    }

    fn make_variant_i32(n: i32) -> Param<'static, 'static> {
        Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
            sig: rustbus::signature::Type::Base(rustbus::signature::Base::Int32),
            value: Param::Base(Base::Int32(n)),
        })))
    }

    #[test]
    fn str_prop_found() {
        let mut props = rustbus::params::DictMap::new();
        props.insert(Base::String("label".to_owned()), make_variant_str("Hello"));
        assert_eq!(get_str_prop(&props, "label"), "Hello");
    }

    #[test]
    fn str_prop_missing() {
        let props = rustbus::params::DictMap::new();
        assert_eq!(get_str_prop(&props, "label"), "");
    }

    #[test]
    fn bool_prop_found() {
        let mut props = rustbus::params::DictMap::new();
        props.insert(Base::String("enabled".to_owned()), make_variant_bool(false));
        assert_eq!(get_bool_prop(&props, "enabled"), Some(false));
    }

    #[test]
    fn bool_prop_missing() {
        let props = rustbus::params::DictMap::new();
        assert_eq!(get_bool_prop(&props, "enabled"), None);
    }

    #[test]
    fn int_prop_found() {
        let mut props = rustbus::params::DictMap::new();
        props.insert(Base::String("toggle-state".to_owned()), make_variant_i32(1));
        assert_eq!(get_int_prop(&props, "toggle-state"), Some(1));
    }

    #[test]
    fn int_prop_missing() {
        let props = rustbus::params::DictMap::new();
        assert_eq!(get_int_prop(&props, "toggle-state"), None);
    }

    #[test]
    fn bytes_prop_empty() {
        let props = rustbus::params::DictMap::new();
        assert!(get_bytes_prop(&props, "icon-data").is_empty());
    }

    #[test]
    fn parse_menu_node_full() {
        let mut props = rustbus::params::DictMap::new();
        props.insert(Base::String("label".to_owned()), make_variant_str("Test"));
        props.insert(Base::String("enabled".to_owned()), make_variant_bool(true));
        props.insert(Base::String("visible".to_owned()), make_variant_bool(true));
        props.insert(
            Base::String("icon-name".to_owned()),
            make_variant_str("icon"),
        );
        props.insert(
            Base::String("toggle-type".to_owned()),
            make_variant_str("checkmark"),
        );
        props.insert(Base::String("toggle-state".to_owned()), make_variant_i32(1));
        let children_arr = Param::Container(Container::Array(rustbus::params::Array {
            element_sig: rustbus::signature::Type::Base(rustbus::signature::Base::String),
            values: vec![],
        }));
        let struct_inner = Param::Container(Container::Struct(vec![
            Param::Base(Base::Int32(42)),
            Param::Container(Container::Dict(rustbus::params::Dict {
                key_sig: rustbus::signature::Base::String,
                value_sig: rustbus::signature::Type::Container(
                    rustbus::signature::Container::Variant,
                ),
                map: props,
            })),
            children_arr,
        ]));
        // parse_menu_node expects a Variant wrapping the struct (as in av arrays)
        let node = Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
            sig: rustbus::signature::Type::Base(rustbus::signature::Base::String),
            value: struct_inner,
        })));

        let parsed = parse_menu_node(&node).unwrap();
        assert_eq!(parsed.id, 42);
        assert_eq!(parsed.label, "Test");
        assert!(parsed.enabled);
        assert!(parsed.visible);
        assert_eq!(parsed.icon_name, "icon");
        assert_eq!(parsed.toggle_type, "checkmark");
        assert_eq!(parsed.toggle_state, 1);
        assert!(!parsed.is_submenu);
    }

    #[test]
    fn parse_menu_node_wrong_type() {
        let node = Param::Base(Base::Int32(0));
        assert!(parse_menu_node(&node).is_none());
    }

    #[test]
    fn extract_prop_value_str() {
        let p = make_variant_str("hello");
        match extract_prop_value(&p) {
            Some(PropValue::Str(s)) => assert_eq!(s, "hello"),
            _ => panic!("expected Str"),
        }
    }

    #[test]
    fn extract_prop_value_bool() {
        let p = make_variant_bool(true);
        match extract_prop_value(&p) {
            Some(PropValue::Bool(b)) => assert!(b),
            _ => panic!("expected Bool"),
        }
    }

    #[test]
    fn extract_prop_value_int() {
        let p = make_variant_i32(42);
        match extract_prop_value(&p) {
            Some(PropValue::Int(n)) => assert_eq!(n, 42),
            _ => panic!("expected Int"),
        }
    }

    #[test]
    fn extract_prop_value_bytes_variant() {
        let bytes_param = Param::Container(Container::Array(rustbus::params::Array {
            element_sig: rustbus::signature::Type::Base(rustbus::signature::Base::Byte),
            values: vec![Param::Base(Base::Byte(0xAA)), Param::Base(Base::Byte(0xBB))],
        }));
        let p = Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
            sig: rustbus::signature::Type::Base(rustbus::signature::Base::String),
            value: bytes_param,
        })));
        match extract_prop_value(&p) {
            Some(PropValue::Bytes(b)) => assert_eq!(b, vec![0xAA, 0xBB]),
            _ => panic!("expected Bytes"),
        }
    }

    #[test]
    fn extract_prop_value_none_for_struct() {
        let p = Param::Container(Container::Struct(vec![]));
        assert!(extract_prop_value(&p).is_none());
    }

    #[test]
    fn convert_props_basic() {
        let mut dict = rustbus::params::DictMap::new();
        dict.insert(Base::String("label".to_owned()), make_variant_str("Test"));
        dict.insert(Base::String("enabled".to_owned()), make_variant_bool(false));
        let result = convert_props(&dict);
        assert_eq!(result.len(), 2);
        let label = result.iter().find(|(k, _)| k == "label").unwrap();
        match &label.1 {
            PropValue::Str(s) => assert_eq!(s, "Test"),
            _ => panic!("expected Str"),
        }
        let enabled = result.iter().find(|(k, _)| k == "enabled").unwrap();
        match &enabled.1 {
            PropValue::Bool(b) => assert!(!b),
            _ => panic!("expected Bool"),
        }
    }
}
