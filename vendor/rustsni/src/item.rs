//! Tray item data model and D-Bus property reading.
//!
//! This module defines [`TrayItem`] — the resolved properties of a
//! `org.kde.StatusNotifierItem` — and helpers for reading them from the
//! D-Bus via `org.freedesktop.DBus.Properties.GetAll`.
//!
//! A tray item publishes its state through D-Bus properties (category,
//! title, status, icons, tooltip, menu path, etc.) and accepts interaction
//! methods (`Activate`, `ContextMenu`, `Scroll`, …). The SNI spec allows
//! items to be identified three ways:
//!
//! - Well-known name: `org.freedesktop.StatusNotifierItem-{pid}-{id}`
//! - Unique name only: `:1.42`
//! - Sender + object path: when a path-only registration is received, the
//!   sender's unique name is used as the bus name.
//!
//! See the [SNI spec] for details.
//!
//! [SNI spec]: https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/

use rustbus::connection::Timeout;
use rustbus::connection::ll_conn::DuplexConn;

use crate::Result;
use crate::icon::IconPixmap;

/// A tray item's unique identifier.
///
/// The inner string is the `service_id` — either the item's well-known bus
/// name (e.g. `org.freedesktop.StatusNotifierItem-4077-1`) or, for
/// path-only registrations, `sender+path` (e.g. `:1.42/StatusNotifierItem`).
///
/// The inner field is `pub`, so you can access `id.0` directly for use as
/// a cache key or display string.
///
/// # Constructing
///
/// ```rust
/// use rustsni::ItemId;
/// let id = ItemId("org.freedesktop.StatusNotifierItem-4077-1".to_owned());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemId(pub String);

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Tooltip data for a tray item.
///
/// Corresponds to the `(sa(iiay)ss)` signature in the SNI spec:
/// - Freedesktop-compliant icon name
/// - Optional icon pixmap (first image from the `a(iiay)` array)
/// - Title text
/// - Descriptive text (may contain a subset of HTML markup)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolTip {
    /// Freedesktop-compliant icon name.
    pub icon_name: String,
    /// Optional icon pixmap (first entry from the pixmap array, if any).
    pub icon_pixmap: Option<IconPixmap>,
    /// Tooltip title.
    pub title: String,
    /// Descriptive text (may include limited HTML markup).
    pub text: String,
}

/// Resolved properties of a StatusNotifierItem.
///
/// Each field corresponds to a D-Bus property from the
/// `org.kde.StatusNotifierItem` interface, read at a point in time via
/// `Properties.GetAll`.
#[derive(Debug, Clone)]
pub struct TrayItem {
    /// Service identifier used internally by the host to track this item.
    /// For well-known-name registrations this is the bus name; for
    /// path-only registrations it is `sender+path`.
    pub id: ItemId,
    /// D-Bus bus name of the item (unique or well-known).
    pub bus_name: String,
    /// Object path of the item on the bus (typically `/StatusNotifierItem`).
    pub object_path: String,
    /// Category of this item: `ApplicationStatus`, `Communications`,
    /// `SystemServices`, or `Hardware`.
    pub category: String,
    /// Application-specific unique identifier (should be stable across
    /// sessions, e.g. the application name).
    pub item_id: String,
    /// Human-readable title describing the application.
    pub title: String,
    /// Status of the item: `Passive`, `Active`, or `NeedsAttention`.
    pub status: String,
    /// Windowing-system dependent window identifier (0 if unused).
    pub window_id: i32,
    /// Additional path to prepend to the icon theme search path.
    pub icon_theme_path: String,
    /// Freedesktop-compliant icon name (preferred over pixmaps).
    pub icon_name: String,
    /// Icon pixmap data (array of resolutions, ARGB32 big-endian).
    pub icon_pixmaps: Vec<IconPixmap>,
    /// Freedesktop-compliant overlay icon name.
    pub attention_icon_name: String,
    /// Attention icon pixmap data (array of resolutions).
    pub attention_icon_pixmaps: Vec<IconPixmap>,
    /// Name or path to an animation for the `NeedsAttention` state.
    pub attention_movie_name: String,
    /// Overlay icon name for extra state information.
    pub overlay_icon_name: String,
    /// Overlay icon pixmap data (array of resolutions).
    pub overlay_icon_pixmaps: Vec<IconPixmap>,
    /// If `true`, the item only supports a context menu (prefer
    /// `ContextMenu()` over `Activate()`).
    pub item_is_menu: bool,
    /// D-Bus object path of the item's `com.canonical.dbusmenu` menu.
    pub menu_path: String,
    /// Tooltip data (icon name, optional pixmap, title, description text).
    pub tooltip: ToolTip,
}

const SNI_IFACE: &str = "org.kde.StatusNotifierItem";

impl TrayItem {
    /// Send a `Properties.GetAll` call. Returns the serial number for matching the reply.
    pub fn send_get_all(conn: &mut DuplexConn, bus_name: &str, object_path: &str) -> Result<u32> {
        let mut call = rustbus::MessageBuilder::new()
            .call("GetAll")
            .on(object_path)
            .with_interface("org.freedesktop.DBus.Properties")
            .at(bus_name)
            .build();
        call.body.push_param(SNI_IFACE).unwrap();
        let serial = conn.send.send_message_write_all(&call)?;
        Ok(serial)
    }

    /// Build a TrayItem from a `GetAll` reply message. The reply must be a Reply
    /// (not Error) — callers should check the response_serial matches their call.
    pub fn from_get_all_reply(
        reply: &rustbus::message_builder::MarshalledMessage,
        service_id: &str,
        bus_name: &str,
        object_path: &str,
    ) -> Result<Self> {
        use rustbus::params::{Base as PBase, Container, Param};
        use std::collections::HashMap;

        // Parse a{sv} dict
        let mut parser = reply.body.parser();
        let mut props: HashMap<String, Param> = HashMap::new();
        if let Ok(Param::Container(Container::Dict(dict))) = parser.get_param().as_ref() {
            for (key, val) in &dict.map {
                let key_str = match key {
                    PBase::String(s) => s.clone(),
                    PBase::StringRef(s) => s.to_string(),
                    _ => continue,
                };
                props.insert(key_str, val.clone());
            }
        }

        // Extract value helpers
        let get_str = |props: &HashMap<String, Param>, key: &str| -> String {
            match props.get(key) {
                Some(Param::Container(Container::Variant(v))) => match &v.value {
                    Param::Base(PBase::String(s)) => s.clone(),
                    Param::Base(PBase::StringRef(s)) => s.to_string(),
                    Param::Base(PBase::ObjectPath(p)) => p.to_string(),
                    Param::Base(PBase::ObjectPathRef(p)) => p.to_string(),
                    _ => String::new(),
                },
                _ => String::new(),
            }
        };
        let get_bool = |props: &HashMap<String, Param>, key: &str| -> bool {
            match props.get(key) {
                Some(Param::Container(Container::Variant(v))) => match &v.value {
                    Param::Base(PBase::Boolean(b)) => *b,
                    _ => false,
                },
                _ => false,
            }
        };
        let get_int = |props: &HashMap<String, Param>, key: &str| -> i32 {
            match props.get(key) {
                Some(Param::Container(Container::Variant(v))) => match &v.value {
                    Param::Base(PBase::Int32(i)) => *i,
                    _ => 0,
                },
                _ => 0,
            }
        };
        let get_pixmaps = |props: &HashMap<String, Param>, key: &str| -> Vec<IconPixmap> {
            match props.get(key) {
                Some(Param::Container(Container::Variant(v))) => {
                    extract_pixmaps_from_param(&Param::Container(Container::Variant(v.clone())))
                }
                _ => Vec::new(),
            }
        };
        let get_tooltip = |props: &HashMap<String, Param>, key: &str| -> ToolTip {
            match props.get(key) {
                Some(Param::Container(Container::Variant(v))) => {
                    extract_tooltip_from_param(&Param::Container(Container::Variant(v.clone())))
                }
                _ => ToolTip::default(),
            }
        };

        let menu_path = match props.get("Menu") {
            Some(Param::Container(Container::Variant(v))) => match &v.value {
                Param::Base(PBase::ObjectPath(p)) => p.to_string(),
                Param::Base(PBase::ObjectPathRef(p)) => p.to_string(),
                Param::Base(PBase::String(s)) => s.clone(),
                Param::Base(PBase::StringRef(s)) => s.to_string(),
                _ => String::new(),
            },
            _ => String::new(),
        };

        Ok(TrayItem {
            id: ItemId(service_id.to_owned()),
            bus_name: bus_name.to_owned(),
            object_path: object_path.to_owned(),
            category: get_str(&props, "Category"),
            item_id: get_str(&props, "Id"),
            title: get_str(&props, "Title"),
            status: get_str(&props, "Status"),
            window_id: get_int(&props, "WindowId"),
            icon_theme_path: get_str(&props, "IconThemePath"),
            icon_name: get_str(&props, "IconName"),
            icon_pixmaps: get_pixmaps(&props, "IconPixmap"),
            attention_icon_name: get_str(&props, "AttentionIconName"),
            attention_icon_pixmaps: get_pixmaps(&props, "AttentionIconPixmap"),
            attention_movie_name: get_str(&props, "AttentionMovieName"),
            overlay_icon_name: get_str(&props, "OverlayIconName"),
            overlay_icon_pixmaps: get_pixmaps(&props, "OverlayIconPixmap"),
            item_is_menu: get_bool(&props, "ItemIsMenu"),
            menu_path,
            tooltip: get_tooltip(&props, "ToolTip"),
        })
    }

    /// Read all properties in a single `Properties.GetAll` call (1 D-Bus round trip).
    ///
    /// Blocks until the reply arrives (Infinite timeout). Non-reply messages
    /// arriving during the wait are buffered into `pending`.
    pub fn from_bus_get_all(
        conn: &mut DuplexConn,
        service_id: &str,
        bus_name: &str,
        object_path: &str,
        pending: &mut std::collections::VecDeque<rustbus::message_builder::MarshalledMessage>,
    ) -> Result<Self> {
        use rustbus::message_builder::MessageType;

        let serial = Self::send_get_all(conn, bus_name, object_path)?;

        let reply = loop {
            let msg = conn.recv.get_next_message(Timeout::Infinite)?;
            if msg.typ == MessageType::Reply && msg.dynheader.response_serial == Some(serial) {
                break msg;
            }
            // Error for our call (UnknownInterface etc.) → not an SNI item
            if msg.typ == MessageType::Error && msg.dynheader.response_serial == Some(serial) {
                let err_name: String = msg.body.parser().get().unwrap_or_default();
                return Err(crate::Error::MethodCall(err_name));
            }
            pending.push_back(msg);
        };

        Self::from_get_all_reply(&reply, service_id, bus_name, object_path)
    }
}

impl TrayItem {
    /// Whether this item has an associated menu.
    pub fn has_menu(&self) -> bool {
        !self.menu_path.is_empty() && self.menu_path != "/"
    }

    /// Select the largest icon pixmap by pixel count.
    pub fn best_icon_pixmap(&self) -> Option<&IconPixmap> {
        self.icon_pixmaps.iter().max_by_key(|p| p.width * p.height)
    }

    /// Select the largest overlay icon pixmap by pixel count.
    pub fn best_overlay_icon_pixmap(&self) -> Option<&IconPixmap> {
        self.overlay_icon_pixmaps
            .iter()
            .max_by_key(|p| p.width * p.height)
    }

    /// Select the largest attention icon pixmap by pixel count.
    pub fn best_attention_icon_pixmap(&self) -> Option<&IconPixmap> {
        self.attention_icon_pixmaps
            .iter()
            .max_by_key(|p| p.width * p.height)
    }

    /// Icon search paths for theme lookup: `[icon_theme_path, "/usr/share/pixmaps"]`.
    pub fn icon_search_paths(&self) -> Vec<&str> {
        let mut paths = Vec::new();
        if !self.icon_theme_path.is_empty() {
            paths.push(self.icon_theme_path.as_str());
        }
        paths.push("/usr/share/pixmaps");
        paths
    }
}

/// Parse a variant containing (sa(iiay)ss) into a ToolTip.
fn extract_tooltip_from_param(param: &rustbus::params::Param) -> ToolTip {
    use rustbus::params::{Container, Param};

    let inner = match param {
        Param::Container(Container::Variant(v)) => &v.value,
        _ => return ToolTip::default(),
    };

    let fields = match inner {
        Param::Container(Container::Struct(s)) => s,
        _ => return ToolTip::default(),
    };
    if fields.len() < 4 {
        return ToolTip::default();
    }

    let icon_name = match &fields[0] {
        Param::Base(rustbus::params::Base::StringRef(s)) => s.to_string(),
        Param::Base(rustbus::params::Base::String(s)) => s.clone(),
        _ => String::new(),
    };

    // icon pixmap: a(iiay) — take the first image if present
    let icon_pixmap = match &fields[1] {
        Param::Container(Container::Array(arr)) => arr.values.first().and_then(|elem| {
            if let Param::Container(Container::Struct(s)) = elem {
                parse_pixmap_struct(s)
            } else {
                None
            }
        }),
        _ => None,
    };

    let title = match &fields[2] {
        Param::Base(rustbus::params::Base::StringRef(s)) => s.to_string(),
        Param::Base(rustbus::params::Base::String(s)) => s.clone(),
        _ => String::new(),
    };

    let text = match &fields[3] {
        Param::Base(rustbus::params::Base::StringRef(s)) => s.to_string(),
        Param::Base(rustbus::params::Base::String(s)) => s.clone(),
        _ => String::new(),
    };

    ToolTip {
        icon_name,
        icon_pixmap,
        title,
        text,
    }
}

/// Extract IconPixmaps from a Param value (variant containing a(iiay)).
fn extract_pixmaps_from_param(param: &rustbus::params::Param) -> Vec<IconPixmap> {
    use rustbus::params::{Container, Param};

    let inner = match param {
        Param::Container(Container::Variant(v)) => &v.value,
        _ => return Vec::new(),
    };

    let array = match inner {
        Param::Container(Container::Array(arr)) => &arr.values,
        _ => return Vec::new(),
    };

    array
        .iter()
        .filter_map(|elem| {
            if let Param::Container(Container::Struct(s)) = elem {
                parse_pixmap_struct(s)
            } else {
                None
            }
        })
        .collect()
}

/// Parse a single `(i32, i32, ay)` struct into an IconPixmap.
fn parse_pixmap_struct(fields: &[rustbus::params::Param]) -> Option<IconPixmap> {
    use rustbus::params::{Container, Param};

    if fields.len() < 3 {
        return None;
    }
    let w = match &fields[0] {
        Param::Base(rustbus::params::Base::Int32(v)) => *v,
        _ => return None,
    };
    let h = match &fields[1] {
        Param::Base(rustbus::params::Base::Int32(v)) => *v,
        _ => return None,
    };
    let raw: Vec<u8> = match &fields[2] {
        Param::Container(Container::Array(a)) => a
            .values
            .iter()
            .filter_map(|b| {
                if let Param::Base(rustbus::params::Base::Byte(v)) = b {
                    Some(*v)
                } else {
                    None
                }
            })
            .collect(),
        _ => return None,
    };
    IconPixmap::from_argb32be(w, h, &raw)
}

/// Call a method with two `i32` parameters on a StatusNotifierItem.
///
/// Used for `Activate`, `ContextMenu`, and `SecondaryActivate` — all of
/// which take screen coordinates `(x, y)`.
pub fn call_method(
    conn: &mut DuplexConn,
    bus_name: &str,
    object_path: &str,
    method: &str,
    x: i32,
    y: i32,
) -> Result<()> {
    let mut call = rustbus::MessageBuilder::new()
        .call(method)
        .on(object_path)
        .with_interface(SNI_IFACE)
        .at(bus_name)
        .build();
    call.body.push_param(x).unwrap();
    call.body.push_param(y).unwrap();
    conn.send.send_message_write_all(&call)?;
    Ok(())
}

/// Call a method taking a single string parameter on a StatusNotifierItem.
///
/// Used for `ProvideXdgActivationToken`.
pub fn call_method_str(
    conn: &mut DuplexConn,
    bus_name: &str,
    object_path: &str,
    method: &str,
    param: &str,
) -> Result<()> {
    let mut call = rustbus::MessageBuilder::new()
        .call(method)
        .on(object_path)
        .with_interface(SNI_IFACE)
        .at(bus_name)
        .build();
    call.body.push_param(param).unwrap();
    conn.send.send_message_write_all(&call)?;
    Ok(())
}

/// Call a method taking `(i32, &str)` on a StatusNotifierItem.
///
/// Used for `Scroll(delta, orientation)`.
pub fn call_method_i32_str(
    conn: &mut DuplexConn,
    bus_name: &str,
    object_path: &str,
    method: &str,
    p1: i32,
    p2: &str,
) -> Result<()> {
    let mut call = rustbus::MessageBuilder::new()
        .call(method)
        .on(object_path)
        .with_interface(SNI_IFACE)
        .at(bus_name)
        .build();
    call.body.push_param(p1).unwrap();
    call.body.push_param(p2).unwrap();
    conn.send.send_message_write_all(&call)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustbus::params::{Base as PBase, Container, Param};

    /// Helper: create a signature::Type for a(iiay) — array of (i32,i32,ay).
    fn sig_icon_array() -> rustbus::signature::Type {
        use rustbus::signature::{Base as SBase, Container as SContainer, StructTypes, Type};
        Type::Container(SContainer::Array(Box::new(Type::Container(
            SContainer::Struct(
                StructTypes::new(vec![
                    Type::Base(SBase::Int32),
                    Type::Base(SBase::Int32),
                    Type::Container(SContainer::Array(Box::new(Type::Base(SBase::Byte)))),
                ])
                .unwrap(),
            ),
        ))))
    }

    /// Build a Param tree for `(sa(iiay)ss)` and parse it as a ToolTip.
    #[test]
    fn tooltip_parse_full() {
        // (sa(iiay)ss) — struct with 4 fields
        let icon_name: Param = Param::Base(PBase::StringRef("my-icon"));
        let pixel_data: Param = Param::Container(Container::Array(rustbus::params::Array {
            element_sig: rustbus::signature::Type::Base(rustbus::signature::Base::Byte),
            values: vec![
                Param::Base(PBase::Byte(0xFF)), // A
                Param::Base(PBase::Byte(0x11)), // R
                Param::Base(PBase::Byte(0x22)), // G
                Param::Base(PBase::Byte(0x33)), // B
            ],
        }));
        let pixmap_struct: Param = Param::Container(Container::Struct(vec![
            Param::Base(PBase::Int32(1)),
            Param::Base(PBase::Int32(1)),
            pixel_data,
        ]));
        let pixmaps: Param = Param::Container(Container::Array(rustbus::params::Array {
            element_sig: sig_icon_array(),
            values: vec![pixmap_struct],
        }));
        let title: Param = Param::Base(PBase::StringRef("Title Text"));
        let text: Param = Param::Base(PBase::StringRef("Description"));

        let tooltip_struct: Param =
            Param::Container(Container::Struct(vec![icon_name, pixmaps, title, text]));

        // Wrap in variant (as Properties.Get returns)
        let variant: Param =
            Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
                sig: rustbus::signature::Type::Base(rustbus::signature::Base::String),
                value: tooltip_struct,
            })));

        let tooltip = extract_tooltip_from_param(&variant);
        assert_eq!(tooltip.icon_name, "my-icon");
        assert_eq!(tooltip.title, "Title Text");
        assert_eq!(tooltip.text, "Description");
        let px = tooltip.icon_pixmap.unwrap();
        assert_eq!(px.width, 1);
        assert_eq!(px.height, 1);
        // big-endian [A,R,G,B] → LE [B,G,R,A]
        assert_eq!(&px.data, &[0x33, 0x22, 0x11, 0xFF]);
    }

    #[test]
    fn tooltip_parse_empty_icon_array() {
        let icon_name: Param = Param::Base(PBase::StringRef(""));
        let pixmaps: Param = Param::Container(Container::Array(rustbus::params::Array {
            element_sig: rustbus::signature::Type::Base(rustbus::signature::Base::Byte),
            values: vec![],
        }));
        let title: Param = Param::Base(PBase::StringRef("T"));
        let text: Param = Param::Base(PBase::StringRef(""));

        let variant: Param =
            Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
                sig: rustbus::signature::Type::Base(rustbus::signature::Base::String),
                value: Param::Container(Container::Struct(vec![icon_name, pixmaps, title, text])),
            })));

        let tooltip = extract_tooltip_from_param(&variant);
        assert_eq!(tooltip.icon_name, "");
        assert!(tooltip.icon_pixmap.is_none());
        assert_eq!(tooltip.title, "T");
    }

    #[test]
    fn tooltip_parse_wrong_type_returns_default() {
        let variant: Param = Param::Base(PBase::Int32(42));
        let tooltip = extract_tooltip_from_param(&variant);
        assert_eq!(tooltip, ToolTip::default());
    }

    #[test]
    fn tooltip_parse_short_struct_returns_default() {
        // Only 2 fields, need 4
        let variant: Param =
            Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
                sig: rustbus::signature::Type::Base(rustbus::signature::Base::String),
                value: Param::Container(Container::Struct(vec![
                    Param::Base(PBase::StringRef("a")),
                    Param::Base(PBase::StringRef("b")),
                ])),
            })));
        let tooltip = extract_tooltip_from_param(&variant);
        assert_eq!(tooltip, ToolTip::default());
    }

    #[test]
    fn tooltip_default_values() {
        let t = ToolTip::default();
        assert_eq!(t.icon_name, "");
        assert!(t.icon_pixmap.is_none());
        assert_eq!(t.title, "");
        assert_eq!(t.text, "");
    }

    #[test]
    fn tooltip_parse_multiple_pixmaps_takes_first() {
        let pixel_1x1: Param = Param::Container(Container::Struct(vec![
            Param::Base(PBase::Int32(1)),
            Param::Base(PBase::Int32(1)),
            Param::Container(Container::Array(rustbus::params::Array {
                element_sig: rustbus::signature::Type::Base(rustbus::signature::Base::Byte),
                values: vec![
                    Param::Base(PBase::Byte(0xAA)),
                    Param::Base(PBase::Byte(0xBB)),
                    Param::Base(PBase::Byte(0xCC)),
                    Param::Base(PBase::Byte(0xDD)),
                ],
            })),
        ]));
        let pixel_2x1: Param = Param::Container(Container::Struct(vec![
            Param::Base(PBase::Int32(2)),
            Param::Base(PBase::Int32(1)),
            Param::Container(Container::Array(rustbus::params::Array {
                element_sig: rustbus::signature::Type::Base(rustbus::signature::Base::Byte),
                values: vec![
                    Param::Base(PBase::Byte(0x11)),
                    Param::Base(PBase::Byte(0x22)),
                    Param::Base(PBase::Byte(0x33)),
                    Param::Base(PBase::Byte(0x44)),
                    Param::Base(PBase::Byte(0x55)),
                    Param::Base(PBase::Byte(0x66)),
                    Param::Base(PBase::Byte(0x77)),
                    Param::Base(PBase::Byte(0x88)),
                ],
            })),
        ]));
        let variant: Param =
            Param::Container(Container::Variant(Box::new(rustbus::params::Variant {
                sig: rustbus::signature::Type::Base(rustbus::signature::Base::String),
                value: Param::Container(Container::Struct(vec![
                    Param::Base(PBase::StringRef("")),
                    Param::Container(Container::Array(rustbus::params::Array {
                        element_sig: sig_icon_array(),
                        values: vec![pixel_1x1, pixel_2x1],
                    })),
                    Param::Base(PBase::StringRef("")),
                    Param::Base(PBase::StringRef("")),
                ])),
            })));

        let tooltip = extract_tooltip_from_param(&variant);
        let px = tooltip.icon_pixmap.unwrap();
        // Should take the first pixmap (1x1), not the second (2x1)
        assert_eq!(px.width, 1);
        assert_eq!(px.height, 1);
    }

    fn make_test_item() -> TrayItem {
        TrayItem {
            id: ItemId("test".to_owned()),
            bus_name: ":1.1".to_owned(),
            object_path: "/StatusNotifierItem".to_owned(),
            category: String::new(),
            item_id: String::new(),
            title: String::new(),
            status: String::new(),
            window_id: 0,
            icon_theme_path: "/usr/share/icons/hicolor".to_owned(),
            icon_name: String::new(),
            icon_pixmaps: vec![
                IconPixmap {
                    width: 16,
                    height: 16,
                    data: vec![0; 16 * 16 * 4],
                },
                IconPixmap {
                    width: 32,
                    height: 32,
                    data: vec![0; 32 * 32 * 4],
                },
                IconPixmap {
                    width: 24,
                    height: 24,
                    data: vec![0; 24 * 24 * 4],
                },
            ],
            attention_icon_name: String::new(),
            attention_icon_pixmaps: vec![IconPixmap {
                width: 16,
                height: 16,
                data: vec![0; 16 * 16 * 4],
            }],
            attention_movie_name: String::new(),
            overlay_icon_name: String::new(),
            overlay_icon_pixmaps: vec![
                IconPixmap {
                    width: 8,
                    height: 8,
                    data: vec![0; 8 * 8 * 4],
                },
                IconPixmap {
                    width: 22,
                    height: 22,
                    data: vec![0; 22 * 22 * 4],
                },
            ],
            item_is_menu: false,
            menu_path: "/MenuBar".to_owned(),
            tooltip: ToolTip::default(),
        }
    }

    #[test]
    fn has_menu_with_path() {
        let item = make_test_item();
        assert!(item.has_menu());
    }

    #[test]
    fn has_menu_empty_path() {
        let mut item = make_test_item();
        item.menu_path = String::new();
        assert!(!item.has_menu());
    }

    #[test]
    fn has_menu_slash_only() {
        let mut item = make_test_item();
        item.menu_path = "/".to_owned();
        assert!(!item.has_menu());
    }

    #[test]
    fn best_icon_pixmap_selects_largest() {
        let item = make_test_item();
        let best = item.best_icon_pixmap().unwrap();
        assert_eq!(best.width, 32);
        assert_eq!(best.height, 32);
    }

    #[test]
    fn best_icon_pixmap_empty() {
        let mut item = make_test_item();
        item.icon_pixmaps.clear();
        assert!(item.best_icon_pixmap().is_none());
    }

    #[test]
    fn best_overlay_icon_pixmap_selects_largest() {
        let item = make_test_item();
        let best = item.best_overlay_icon_pixmap().unwrap();
        assert_eq!(best.width, 22);
    }

    #[test]
    fn best_attention_icon_pixmap_selects_largest() {
        let item = make_test_item();
        let best = item.best_attention_icon_pixmap().unwrap();
        assert_eq!(best.width, 16);
    }

    #[test]
    fn icon_search_paths_with_theme() {
        let item = make_test_item();
        let paths = item.icon_search_paths();
        assert_eq!(
            paths,
            vec!["/usr/share/icons/hicolor", "/usr/share/pixmaps"]
        );
    }

    #[test]
    fn icon_search_paths_without_theme() {
        let mut item = make_test_item();
        item.icon_theme_path = String::new();
        let paths = item.icon_search_paths();
        assert_eq!(paths, vec!["/usr/share/pixmaps"]);
    }
}
