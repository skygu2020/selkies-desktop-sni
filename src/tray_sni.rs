//! Native StatusNotifierItem (SNI) host alongside the existing XEmbed host.
//!
//! The D-Bus connection is driven by the panel's poll loop; no worker thread is
//! created. Icon pixmaps are converted from native-endian ARGB32/BGRA into the
//! premultiplied RGBA format used by tiny-skia.

use std::os::fd::RawFd;
use std::rc::Rc;

use rustsni::{ItemId, TrayEvent, TrayHost};
use tiny_skia::{IntSize, Pixmap};

use crate::render::resize_pixmap;

/// An SNI item prepared for rendering in the panel.
#[derive(Clone)]
pub struct SniIcon {
    pub id: ItemId,
    pub title: String,
    pub icon_name: String,
    pub pixmap: Option<Rc<Pixmap>>,
    pub item_is_menu: bool,
}

/// D-Bus StatusNotifierWatcher/Host and its current ordered item list.
pub struct TraySni {
    host: TrayHost,
    icons: Vec<SniIcon>,
}

impl TraySni {
    /// Starts an SNI watcher. If another watcher already owns the well-known
    /// name, SNI is unavailable in this session and the caller can keep using
    /// the XEmbed host without failing desktop startup.
    pub fn new() -> Option<Self> {
        let mut host = match TrayHost::new() {
            Ok(host) => host,
            Err(error) => {
                eprintln!("selkies-desktop: cannot start StatusNotifier host: {error}");
                return None;
            }
        };
        let _ = host.poll();
        Some(Self { host, icons: Vec::new() })
    }

    /// D-Bus descriptor, suitable for the existing poll loop.
    pub fn fd(&self) -> RawFd {
        self.host.fd()
    }

    /// Current items in deterministic order.
    pub fn icons(&self) -> &[SniIcon] {
        &self.icons
    }

    /// Process pending D-Bus messages and refresh the icon snapshot.
    /// Returns true when visible item data changed.
    pub fn poll(&mut self, size: u32) -> bool {
        let events = match self.host.poll() {
            Ok(events) => events,
            Err(error) => {
                eprintln!("selkies-desktop: StatusNotifier poll failed: {error}");
                return false;
            }
        };
        let relevant = events.iter().any(|event| matches!(
            event,
            TrayEvent::ItemAdded(_) | TrayEvent::ItemChanged(_) | TrayEvent::ItemRemoved(_) | TrayEvent::HostShutdown
        ));
        if !relevant && !self.icons.is_empty() {
            return false;
        }

        let mut items: Vec<_> = self.host.items().values().collect();
        items.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        let mut next = Vec::with_capacity(items.len());
        for item in items {
            // Do not hide Passive items: some applications publish a useful
            // icon before switching to Active, and users asked to see all.
            let pixmap = item.best_icon_pixmap().and_then(icon_pixmap)
                .and_then(|pix| resize_pixmap(&pix, size).map(Rc::new));
            next.push(SniIcon {
                id: item.id.clone(),
                title: if item.title.is_empty() { item.item_id.clone() } else { item.title.clone() },
                icon_name: item.icon_name.clone(),
                pixmap,
                item_is_menu: item.item_is_menu,
            });
        }
        let changed = !same_icons(&self.icons, &next);
        if changed {
            self.icons = next;
        }
        changed
    }

    /// Dispatches a pointer action to the selected item.
    pub fn click(&mut self, index: usize, button: u8, x: i32, y: i32) {
        let Some(icon) = self.icons.get(index) else { return; };
        let id = icon.id.clone();
        let menu_only = icon.item_is_menu;
        let result = match button {
            1 if menu_only => self.host.context_menu(&id, x, y),
            1 => self.host.activate(&id, x, y),
            2 => self.host.secondary_activate(&id, x, y),
            3 => self.host.context_menu(&id, x, y),
            _ => return,
        };
        if let Err(error) = result {
            eprintln!("selkies-desktop: tray action for {id} failed: {error}");
        }
    }

    /// Forwards a wheel event to an item (SNI delta is expressed in steps).
    pub fn scroll(&mut self, index: usize, delta: i32, horizontal: bool) {
        let Some(icon) = self.icons.get(index) else { return; };
        if let Err(error) = self.host.scroll(&icon.id, delta, if horizontal { "horizontal" } else { "vertical" }) {
            eprintln!("selkies-desktop: tray scroll for {} failed: {error}", icon.id);
        }
    }
}

fn same_icons(a: &[SniIcon], b: &[SniIcon]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| {
        a.id == b.id && a.title == b.title && a.icon_name == b.icon_name && a.item_is_menu == b.item_is_menu
            && match (&a.pixmap, &b.pixmap) {
                (Some(a), Some(b)) => a.width() == b.width() && a.height() == b.height() && a.data() == b.data(),
                (None, None) => true,
                _ => false,
            }
    })
}

/// Converts rustsni's native-endian ARGB32 pixmap into tiny-skia's RGBA pixmap.
/// rustsni has already converted the wire ARGB32 into native-endian BGRA on Linux.
fn icon_pixmap(icon: &rustsni::IconPixmap) -> Option<Pixmap> {
    let width = icon.width;
    let height = icon.height;
    let len = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    if width == 0 || height == 0 || icon.data.len() < len {
        return None;
    }
    let mut rgba = vec![0u8; len];
    for (src, dst) in icon.data[..len].chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
        // rustsni::IconPixmap documents native-endian ARGB32; on little-endian
        // Linux its byte order is BGRA. tiny-skia stores premultiplied RGBA.
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = src[3];
    }
    Pixmap::from_vec(rgba, IntSize::from_wh(width, height)?)
}
