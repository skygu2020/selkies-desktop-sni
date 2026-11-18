//! StatusNotifierHost registration.
//!
//! Per the [SNI spec], a `StatusNotifierHost` is an object in an application
//! that does the actual graphical representation of tray items. It doesn't
//! need any particular methods or signals on the bus — just the presence of
//! the well-known name `org.freedesktop.StatusNotifierHost-{pid}` is enough
//! to signal items that a graphical shell is running.
//!
//! If no host is registered, SNI items may fall back to the X11 system tray
//! protocol (XDG shell / `_NET_SYSTEM_TRAY`).
//!
//! [SNI spec]: https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/

use rustbus::connection::ll_conn::DuplexConn;
use rustbus::message_builder::MarshalledMessage;
use rustbus::standard_messages;
use std::collections::VecDeque;

use crate::Result;

/// Register as `org.freedesktop.StatusNotifierHost-{pid}` on the session bus.
///
/// This acquires a unique well-known name so tray items know a host is
/// present. The name is automatically released when the D-Bus connection
/// is dropped.
///
/// Unexpected D-Bus messages (e.g. `RegisterStatusNotifierItem` calls from
/// items responding to `StatusNotifierHostRegistered`) arriving during the
/// synchronous wait are buffered into `pending` for later processing.
pub fn register(conn: &mut DuplexConn, pending: &mut VecDeque<MarshalledMessage>) -> Result<()> {
    let name = format!("org.freedesktop.StatusNotifierHost-{}", std::process::id());
    let msg = standard_messages::request_name(&name, 0);
    let serial = conn.send.send_message_write_all(&msg)?;
    loop {
        let resp = conn
            .recv
            .get_next_message(rustbus::connection::Timeout::Infinite)?;
        if resp.dynheader.response_serial == Some(serial) {
            break;
        }
        pending.push_back(resp);
    }
    Ok(())
}
