# rustsni

[![crates.io](https://img.shields.io/crates/v/rustsni.svg)](https://crates.io/crates/rustsni)
[![docs.rs](https://img.shields.io/docsrs/rustsni)](https://docs.rs/rustsni)
[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE-MIT)

A poll-friendly [StatusNotifierItem] host library for Linux system trays, built on `rustbus`.

`rustsni` implements the **host side** of the SNI protocol — it acts as a `StatusNotifierWatcher` + `StatusNotifierHost` on the D-Bus session bus, discovers tray items, and provides their properties, icons, and menus — all without spawning threads.

## Features

- **Poll-friendly** — single `fd()` for poll/epoll, no threads, no callbacks
- **Item discovery** — catches items that register before and after the host starts
- **Async probing** — one D-Bus unique name probed per `poll()` call (non-blocking, 500 ms timeout, 3 retries)
- **Full property access** — category, title, status, icons (name + pixmap), tooltip, menu path, window id, and more
- **Menu support** — `com.canonical.dbusmenu` layout fetching, property queries, and click events
- **Item interaction** — `Activate`, `ContextMenu`, `SecondaryActivate`, `Scroll`, `ProvideXdgActivationToken`

## Platform

- Linux only
- Requires a running D-Bus session bus

## Minimum supported Rust version

Rust 1.85 (edition 2024).

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
rustsni = "0.1"
```

Basic example — discover tray items and print them using an
fd-based event loop:

```rust
use rustsni::{ItemId, TrayEvent, TrayHost};

let mut host = TrayHost::new()?;

// Flush initial discovery events (items that were already running).
for event in host.poll()? {
    if let TrayEvent::ItemAdded(id) = event {
        let item = &host.items()[&id];
        println!("tray: {} — {}", item.title, item.status);
    }
}

// Register host.fd() with your poll/epoll loop. When it fires:
for event in host.poll()? {
    match event {
        TrayEvent::ItemAdded(id) | TrayEvent::ItemChanged(id) => {
            if let Some(item) = host.items().get(&id) {
                println!("  category: {}", item.category);
                println!("  has menu: {}", item.has_menu());
            }
        }
        TrayEvent::ItemRemoved(id) => {
            println!("item gone: {id}");
        }
        TrayEvent::HostShutdown => break,
        _ => {}
    }
}

// Interact with items by their ItemId:
let app_id = ItemId("some-item-id".to_owned());
let _ = host.activate(&app_id, 0, 0);     // left-click
let _ = host.context_menu(&app_id, 0, 0);  // right-click
```

See the [docs](https://docs.rs/rustsni) for the full API reference.

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│  Your application (system tray / bar)                    │
│                                                          │
│  poll() → TrayEvent[] ─── handle UI updates              │
│  activate(), get_menu(), menu_click() ─── interact       │
└──────────────────┬───────────────────────────────────────┘
                   │   poll-friendly, no threads
┌──────────────────▼───────────────────────────────────────┐
│  rustsni::TrayHost                                       │
│                                                          │
│  ┌──────────┐  ┌──────────────┐  ┌──────────────────┐   │
│  │ Watcher  │  │ Host         │  │ Item cache       │   │
│  │ (reg. &  │  │ (announce    │  │ (HashMap        │   │
│  │  signal) │  │  presence)   │  │  ItemId→TrayItem)│   │
│  └────┬─────┘  └──────┬───────┘  └────────┬─────────┘   │
│       └───────────────┼───────────────────┘              │
└───────────────────────┼──────────────────────────────────┘
                        │  D-Bus session bus
        ┌───────────────┼─────────────────────┐
        ▼               ▼                     ▼
   ┌────────┐    ┌──────────┐         ┌──────────┐
   │ App A  │    │ App B    │   ...   │ App N    │
   │ SNI    │    │ SNI      │         │ SNI      │
   └────────┘    └──────────┘         └──────────┘
   (tray items on the session bus)
```

## License

MIT — see [LICENSE-MIT](LICENSE-MIT).

[StatusNotifierItem]: https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/
