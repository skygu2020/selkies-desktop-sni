//! A poll-friendly [StatusNotifierItem] host library built on `rustbus`.
//!
//! This crate implements the **host side** of the
//! [StatusNotifierItem] protocol (also known as the D-Bus
//! system tray protocol). It acts as a `StatusNotifierWatcher` +
//! `StatusNotifierHost` on the session bus, discovering tray items and
//! providing their properties, icons, and menus — all without blocking.
//!
//! # Architecture
//!
//! The main entry point is [`TrayHost`]. It:
//! 1. Connects to the D-Bus session bus.
//! 2. Registers itself as `org.kde.StatusNotifierWatcher` (so tray items can
//!    register with it).
//! 3. Registers itself as `org.freedesktop.StatusNotifierHost-{pid}` (so items
//!    know a graphical shell is present).
//! 4. Scans for already-running items via `ListNames` and probes each unique
//!    bus name for SNI support (one per [`poll()`](TrayHost::poll) call, non-blocking).
//!
//! After construction, the caller hands the [`fd()`](TrayHost::fd) to a
//! poll/epoll loop. When the fd becomes readable, call
//! [`poll()`](TrayHost::poll) to process pending D-Bus messages and receive
//! [`TrayEvent`]s.
//!
//! # Event flow
//!
//! ```text
//! ┌─────────────┐   poll()    ┌─────────────┐
//! │  TrayHost   │ ──────────► │  TrayEvent  │
//! │  (watcher)  │ ◄────────── │  Vec        │
//! └─────────────┘   events    └─────────────┘
//!        │
//!        │  methods (activate, context_menu, get_menu, scroll, …)
//!        ▼
//! ┌─────────────┐
//! │  TrayItem   │
//! │  (on D-Bus) │
//! └─────────────┘
//! ```
//!
//! # Non-blocking / poll-friendly design
//!
//! This library **never spawns threads**. All I/O is driven by the caller's
//! event loop:
//! - [`fd()`](TrayHost::fd) returns the D-Bus socket fd for poll/epoll.
//! - [`poll()`](TrayHost::poll) reads available messages without blocking
//!   and returns accumulated [`TrayEvent`]s.
//! - Item discovery probes are sent asynchronously (one per `poll()` call)
//!   with a 500 ms timeout and up to 3 retries.
//!
//! # Interacting with tray items
//!
//! Once an item is discovered (you receive [`TrayEvent::ItemAdded`]), you can:
//! - Read its properties from [`TrayHost::items()`] (category, title, status,
//!   icons, tooltip, menu path, …).
//! - Call [`TrayHost::activate()`], [`TrayHost::context_menu()`],
//!   [`TrayHost::secondary_activate()`], or [`TrayHost::scroll()`] to
//!   interact with it.
//! - Get its menu layout via [`TrayHost::get_menu()`] and fire click events
//!   via [`TrayHost::menu_click()`].
//!
//! # Platform
//!
//! Linux only. Requires a running D-Bus session bus.
//!
//! # Minimum supported Rust version
//!
//! Rust 1.85 (edition 2024).
//!
//! [StatusNotifierItem]: https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/
#![allow(clippy::mutable_key_type)]
// `ItemId` wraps `String` but doesn't implement `Borrow<str>`, so
// `HashMap<ItemId, TrayItem>` triggers this lint. This is intentional:
// lookups use the owned `ItemId` type, never `&str`.

mod host;
mod icon;
mod item;
mod menu;
mod watcher;

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, RawFd};

use rustbus::connection::Timeout;
use rustbus::connection::ll_conn::DuplexConn;

pub use icon::{IconPixmap, from_tuples};
pub use item::{ItemId, ToolTip, TrayItem};
pub use menu::{MenuItemProps, MenuNode, PropValue, fire_click as fire_menu_click};

/// Errors returned by this library.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A D-Bus connection-level error (I/O, protocol, etc.).
    #[error("D-Bus error: {0}")]
    Bus(#[from] rustbus::connection::Error),
    /// A D-Bus message marshal (serialisation) error.
    #[error("marshal error: {0}")]
    Marshal(#[from] rustbus::wire::errors::MarshalError),
    /// A D-Bus message unmarshal (deserialisation) error.
    #[error("unmarshal error: {0}")]
    Unmarshal(#[from] rustbus::wire::errors::UnmarshalError),
    /// Another `StatusNotifierWatcher` is already registered on the session bus.
    #[error("another StatusNotifierWatcher is already running")]
    WatcherAlreadyRunning,
    /// The requested tray item was not found in the local cache.
    #[error("item not found: {0}")]
    ItemNotFound(ItemId),
    /// A D-Bus method call returned an error reply.
    #[error("D-Bus method call failed: {0}")]
    MethodCall(String),
}

/// Convenience alias for crate-level [`Result`](std::result::Result).
pub type Result<T> = std::result::Result<T, Error>;

/// Events produced by [`TrayHost::poll()`].
///
/// These describe state changes in the tray — items appearing, disappearing,
/// updating, or requesting interaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEvent {
    /// A new tray item was discovered and added to the host's item cache.
    ItemAdded(ItemId),
    /// An existing item's properties changed (re-read via [`TrayHost::items()`]).
    ItemChanged(ItemId),
    /// An item disappeared from the session bus and was removed.
    ItemRemoved(ItemId),
    /// An item's menu layout changed (re-fetch via [`TrayHost::get_menu()`]).
    MenuChanged(ItemId),
    /// An item requested its menu be shown to the user (menu activation request).
    MenuActivationRequested(ItemId),
    /// The host has been shut down via [`TrayHost::shutdown()`].
    HostShutdown,
}

/// A StatusNotifierWatcher + StatusNotifierHost on the D-Bus session bus.
///
/// `TrayHost` is the central type of this library. It owns the D-Bus
/// connection, tracks discovered tray items, and drives all I/O through
/// a single file descriptor.
///
/// # Example
///
/// ```no_run
/// use rustsni::{ItemId, TrayHost, TrayEvent};
///
/// # fn main() -> Result<(), rustsni::Error> {
/// let mut host = TrayHost::new()?;
///
/// // Flush initial events (items that were already running before us).
/// for event in host.poll()? {
///     if let TrayEvent::ItemAdded(id) = &event {
///         let item = &host.items()[id];
///         println!("tray item: {} ({})", id, item.title);
///     }
/// }
///
/// // The typical pattern: register host.fd() with your event loop.
/// // When the fd is readable, poll and handle events:
/// for event in host.poll()? {
///     match event {
///         TrayEvent::ItemAdded(id) | TrayEvent::ItemChanged(id) => {
///             if let Some(item) = host.items().get(&id) {
///                 println!("  category: {}", item.category);
///                 println!("  has menu: {}", item.has_menu());
///             }
///         }
///         TrayEvent::ItemRemoved(id) => {
///             println!("item gone: {id}");
///         }
///         TrayEvent::MenuChanged(id) => {
///             let menu = host.get_menu(&id, 0)?;
///             println!("menu for {id}: {} items", menu.len());
///         }
///         TrayEvent::HostShutdown => break,
///         _ => {}
///     }
/// }
///
/// // Interact with items:
/// let id = ItemId("app-name".to_owned());
/// let _ = host.activate(&id, 0, 0);
/// let _ = host.context_menu(&id, 0, 0);
/// # Ok(())
/// # }
/// ```
pub struct TrayHost {
    conn: DuplexConn,
    items: HashMap<ItemId, TrayItem>,
    pending_events: Vec<TrayEvent>,
    /// Messages buffered during synchronous property reads.
    pending_messages: VecDeque<rustbus::message_builder::MarshalledMessage>,
}

impl TrayHost {
    /// Connect to the session bus, register as StatusNotifierWatcher and
    /// StatusNotifierHost.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WatcherAlreadyRunning`] if another `StatusNotifierWatcher`
    /// is already registered on the session bus.
    ///
    /// Returns [`Error::Bus`] if the D-Bus connection or hello handshake fails.
    pub fn new() -> Result<Self> {
        let mut conn = DuplexConn::connect_to_bus(rustbus::get_session_bus_path()?, true)?;
        conn.send_hello(Timeout::Infinite)?;

        let mut this = Self {
            conn,
            items: HashMap::new(),
            pending_events: Vec::new(),
            pending_messages: VecDeque::new(),
        };

        watcher::register(&mut this.conn)?;
        host::register(&mut this.conn, &mut this.pending_messages)?;

        // Emit StatusNotifierHostRegistered AFTER host registration and
        // before returning, so items see a fully-initialized watcher+host
        // and the caller can immediately poll() for their responses.
        {
            let sig = rustbus::MessageBuilder::new()
                .signal(
                    crate::watcher::WATCHER_INTERFACE,
                    "StatusNotifierHostRegistered",
                    crate::watcher::WATCHER_PATH,
                )
                .build();
            this.conn.send.send_message_write_all(&sig)?;
        }

        Ok(this)
    }

    /// The D-Bus file descriptor. Register this with your poll/epoll loop.
    ///
    /// The fd becomes readable when there are pending D-Bus messages to
    /// process. Call [`poll()`](Self::poll) when it fires.
    pub fn fd(&self) -> RawFd {
        self.conn.as_raw_fd()
    }

    /// Non-blocking: read all pending D-Bus messages and return tray events.
    /// Call this when `fd()` becomes readable.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Bus`] on D-Bus I/O errors.
    pub fn poll(&mut self) -> Result<Vec<TrayEvent>> {
        let mut events = self.pending_events.drain(..).collect();

        // Process any messages buffered during property reads
        while let Some(msg) = self.pending_messages.pop_front() {
            self.dispatch(msg, &mut events)?;
        }

        // Read available messages from the socket.
        //
        // A single `read_once` may read several D-Bus messages into the
        // internal buffer when they arrive in quick succession.  Drain
        // every complete message before trying to read again.
        loop {
            match self.conn.recv.read_once(Timeout::Nonblock) {
                Ok(()) => {}
                Err(rustbus::connection::Error::TimedOut) => break,
                Err(e) => return Err(e.into()),
            }

            while self.conn.recv.buffer_contains_whole_message()? {
                let msg = self.conn.recv.get_next_message(Timeout::Nonblock)?;
                self.dispatch(msg, &mut events)?;
            }
        }

        Ok(events)
    }

    /// Returns a reference to the current tray item cache.
    ///
    /// The cache is populated by [`poll()`](Self::poll) as items are discovered
    /// and updated. Items are keyed by [`ItemId`].
    pub fn items(&self) -> &HashMap<ItemId, TrayItem> {
        &self.items
    }

    /// Fetch a tray item by bus name and insert it into the cache.
    ///
    /// This is a synchronous, blocking call — it sends `Properties.GetAll` on
    /// the D-Bus and waits for the reply. Non-reply messages arriving during
    /// the wait are buffered and processed by the next [`poll()`](Self::poll).
    ///
    /// Use this when you already know an item's bus address (e.g. from
    /// external lookup) and don't want to wait for async discovery to
    /// reach it.
    ///
    /// # Arguments
    ///
    /// * `bus_name` — D-Bus bus name (unique like `:1.60` or well-known).
    /// * `object_path` — object path, typically `/StatusNotifierItem`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MethodCall`] if the target does not implement the
    /// `org.kde.StatusNotifierItem` interface. Returns [`Error::Bus`] on
    /// D-Bus I/O errors.
    pub fn add_item(&mut self, bus_name: &str, object_path: &str) -> Result<ItemId> {
        let service_id = bus_name.to_owned();
        let item = item::TrayItem::from_bus_get_all(
            &mut self.conn,
            &service_id,
            bus_name,
            object_path,
            &mut self.pending_messages,
        )?;
        let id = item.id.clone();
        self.items.insert(id.clone(), item);
        Ok(id)
    }

    /// Call the item's `Activate` method.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ItemNotFound`] if `id` is not in the item cache.
    /// Returns [`Error::Bus`] on D-Bus I/O errors.
    pub fn activate(&mut self, id: &ItemId, x: i32, y: i32) -> Result<()> {
        let item = self
            .items
            .get(id)
            .ok_or_else(|| crate::Error::ItemNotFound(id.clone()))?;
        item::call_method(
            &mut self.conn,
            &item.bus_name,
            &item.object_path,
            "Activate",
            x,
            y,
        )
    }

    /// Call the item's `ContextMenu` method.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ItemNotFound`] if `id` is not in the item cache.
    /// Returns [`Error::Bus`] on D-Bus I/O errors.
    pub fn context_menu(&mut self, id: &ItemId, x: i32, y: i32) -> Result<()> {
        let item = self
            .items
            .get(id)
            .ok_or_else(|| crate::Error::ItemNotFound(id.clone()))?;
        item::call_method(
            &mut self.conn,
            &item.bus_name,
            &item.object_path,
            "ContextMenu",
            x,
            y,
        )
    }

    /// Call the item's `SecondaryActivate` method.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ItemNotFound`] if `id` is not in the item cache.
    /// Returns [`Error::Bus`] on D-Bus I/O errors.
    pub fn secondary_activate(&mut self, id: &ItemId, x: i32, y: i32) -> Result<()> {
        let item = self
            .items
            .get(id)
            .ok_or_else(|| crate::Error::ItemNotFound(id.clone()))?;
        item::call_method(
            &mut self.conn,
            &item.bus_name,
            &item.object_path,
            "SecondaryActivate",
            x,
            y,
        )
    }

    /// Get the menu layout for a tray item.
    ///
    /// Returns an empty `Vec` if the item has no menu, or if the server
    /// returns no layout data.
    ///
    /// # Arguments
    ///
    /// * `parent_id` — fetch children of this menu item. Pass `0` for root.
    pub fn get_menu(&mut self, id: &ItemId, parent_id: i32) -> Result<Vec<menu::MenuNode>> {
        let item = match self.items.get(id) {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };
        if item.menu_path.is_empty() || item.menu_path == "/" {
            return Ok(Vec::new());
        }
        menu::get_layout(&mut self.conn, &item.bus_name, &item.menu_path, parent_id)
    }

    /// Fire a click event on a menu item.
    ///
    /// Silently returns `Ok(())` if the item has no menu.
    pub fn menu_click(&mut self, id: &ItemId, menu_id: i32) -> Result<()> {
        let item = match self.items.get(id) {
            Some(i) => i,
            None => return Ok(()),
        };
        if item.menu_path.is_empty() || item.menu_path == "/" {
            return Ok(());
        }
        menu::event(
            &mut self.conn,
            &item.bus_name,
            &item.menu_path,
            menu_id,
            "clicked",
        )
    }

    /// Call the item's `Scroll` method.
    ///
    /// `orientation` should be `"horizontal"` or `"vertical"`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ItemNotFound`] if `id` is not in the item cache.
    /// Returns [`Error::Bus`] on D-Bus I/O errors.
    pub fn scroll(&mut self, id: &ItemId, delta: i32, orientation: &str) -> Result<()> {
        let item = self
            .items
            .get(id)
            .ok_or_else(|| crate::Error::ItemNotFound(id.clone()))?;
        item::call_method_i32_str(
            &mut self.conn,
            &item.bus_name,
            &item.object_path,
            "Scroll",
            delta,
            orientation,
        )
    }

    /// Call the item's `ProvideXdgActivationToken` method.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ItemNotFound`] if `id` is not in the item cache.
    /// Returns [`Error::Bus`] on D-Bus I/O errors.
    pub fn provide_xdg_activation_token(&mut self, id: &ItemId, token: &str) -> Result<()> {
        let item = self
            .items
            .get(id)
            .ok_or_else(|| crate::Error::ItemNotFound(id.clone()))?;
        item::call_method_str(
            &mut self.conn,
            &item.bus_name,
            &item.object_path,
            "ProvideXdgActivationToken",
            token,
        )
    }

    /// Get properties for multiple menu items at once via `GetGroupProperties`.
    ///
    /// Returns an empty `Vec` if the item has no menu.
    pub fn get_menu_group_properties(
        &mut self,
        id: &ItemId,
        ids: &[i32],
        property_names: &[&str],
    ) -> Result<Vec<menu::MenuItemProps>> {
        let item = match self.items.get(id) {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };
        if item.menu_path.is_empty() || item.menu_path == "/" {
            return Ok(Vec::new());
        }
        menu::get_group_properties(
            &mut self.conn,
            &item.bus_name,
            &item.menu_path,
            ids,
            property_names,
        )
    }

    /// Get a single property from a menu item via `GetProperty`.
    ///
    /// Returns `None` if the item has no menu or the property doesn't exist.
    pub fn get_menu_property(
        &mut self,
        id: &ItemId,
        menu_id: i32,
        name: &str,
    ) -> Result<Option<menu::PropValue>> {
        let item = match self.items.get(id) {
            Some(i) => i,
            None => return Ok(None),
        };
        if item.menu_path.is_empty() || item.menu_path == "/" {
            return Ok(None);
        }
        menu::get_property(
            &mut self.conn,
            &item.bus_name,
            &item.menu_path,
            menu_id,
            name,
        )
    }

    /// Emit `StatusNotifierHostUnregistered` and push `HostShutdown`.
    pub fn shutdown(&mut self) -> Result<()> {
        let sig = rustbus::MessageBuilder::new()
            .signal(
                "org.kde.StatusNotifierWatcher",
                "StatusNotifierHostUnregistered",
                "/StatusNotifierWatcher",
            )
            .build();
        self.conn.send.send_message_write_all(&sig)?;
        self.pending_events.push(TrayEvent::HostShutdown);
        Ok(())
    }

    fn dispatch(
        &mut self,
        msg: rustbus::message_builder::MarshalledMessage,
        events: &mut Vec<TrayEvent>,
    ) -> Result<()> {
        use rustbus::message_builder::MessageType;

        match msg.typ {
            MessageType::Signal => {
                watcher::handle_signal(
                    &mut self.conn,
                    &msg,
                    &mut self.items,
                    events,
                    &mut self.pending_messages,
                )?;
            }
            MessageType::Call => {
                // Handle all incoming method calls (watcher, properties, etc.)
                watcher::handle_call(
                    &mut self.conn,
                    &msg,
                    &mut self.items,
                    events,
                    &mut self.pending_messages,
                )?;
            }
            _ => {}
        }
        Ok(())
    }
}
