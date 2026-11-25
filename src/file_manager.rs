//! D-Bus implementation of org.freedesktop.FileManager1.
//!
//! Browsers call ShowItems when the user asks to reveal a downloaded file.
//! Keep this service on its own connection/thread so the desktop event loop
//! remains non-blocking.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use rustbus::connection::Timeout;
use rustbus::connection::ll_conn::DuplexConn;
use rustbus::standard_messages;

const SERVICE: &str = "org.freedesktop.FileManager1";
const PATH: &str = "/org/freedesktop/FileManager1";
const INTERFACE: &str = "org.freedesktop.FileManager1";

/// The FileManager1 service, with a wakeup descriptor for the desktop poll loop.
pub struct Service {
    receiver: Receiver<Vec<PathBuf>>,
    wake: UnixStream,
}

impl Service {
    pub fn fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// Clears the wakeup socket and drains all queued requests.
    pub fn drain(&self) -> Vec<Vec<PathBuf>> {
        // Clone the descriptor so reading does not require a mutable borrow
        // of the service held by the desktop event loop.
        if let Ok(mut wake) = self.wake.try_clone() {
            let mut bytes = [0u8; 128];
            loop {
                match wake.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        self.receiver.try_iter().collect()
    }
}

/// Starts the FileManager1 service and returns paths requested by clients.
pub fn start() -> Option<Service> {
    let (wake, writer) = UnixStream::pair().ok()?;
    wake.set_nonblocking(true).ok()?;
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("file-manager-dbus".into())
        .spawn(move || {
            if let Err(error) = run(tx, writer) {
                let expected_conflict =
                    error.to_string() == format!("{SERVICE} is already owned by another application");
                if !expected_conflict || std::env::var_os("SELKIES_DESKTOP_DEBUG").is_some() {
                    if expected_conflict {
                        eprintln!(
                            "selkies-desktop: {SERVICE} is already owned; leaving the existing file manager service untouched"
                        );
                    } else {
                        eprintln!("selkies-desktop: FileManager1 service stopped: {error}");
                    }
                }
            }
        })
        .ok()?;
    Some(Service { receiver: rx, wake })
}

fn run(
    tx: Sender<Vec<PathBuf>>,
    mut wake: UnixStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = DuplexConn::connect_to_bus(rustbus::get_session_bus_path()?, true)?;
    conn.send_hello(Timeout::Infinite)?;

    let request = standard_messages::request_name(SERVICE, 0);
    let serial = conn.send.send_message_write_all(&request)?;
    loop {
        let reply = conn.recv.get_next_message(Timeout::Infinite)?;
        if reply.dynheader.response_serial == Some(serial) {
            let result: u32 = reply.body.parser().get()?;
            if result != 1 && result != 4 {
                return Err(format!("{SERVICE} is already owned by another application").into());
            }
            break;
        }
    }

    loop {
        let msg = conn.recv.get_next_message(Timeout::Infinite)?;
        // Replies and unrelated bus traffic do not carry this object path
        // and method member. rustbus 0.19.3's MessageType does not expose a
        // MethodCall variant, so dispatch using the dynamic method headers.
        if msg.dynheader.object.as_deref() != Some(PATH)
            || msg.dynheader.member.is_none()
        {
            continue;
        }
        let interface = msg.dynheader.interface.as_deref().unwrap_or("");
        let member = msg.dynheader.member.as_deref().unwrap_or("");
        if interface == "org.freedesktop.DBus.Introspectable" && member == "Introspect" {
            let xml = r#"<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
  <interface name="org.freedesktop.FileManager1">
    <method name="ShowFolders"><arg type="as" direction="in"/><arg type="s" direction="in"/></method>
    <method name="ShowItems"><arg type="as" direction="in"/><arg type="s" direction="in"/></method>
  </interface>
</node>"#;
            let mut reply = msg.dynheader.make_response();
            reply.body.push_param(xml).unwrap();
            conn.send.send_message_write_all(&reply)?;
            continue;
        }
        if interface != INTERFACE {
            let error = msg.dynheader.make_error_response(
                "org.freedesktop.DBus.Error.UnknownInterface".to_owned(),
                Some(format!("Unknown interface: {interface}")),
            );
            conn.send.send_message_write_all(&error)?;
            continue;
        }
        let mut parser = msg.body.parser();
        let uris: Vec<String> = match parser.get() {
            Ok(uris) => uris,
            Err(_) => {
                let error = msg.dynheader.make_error_response(
                    "org.freedesktop.DBus.Error.InvalidArgs".to_owned(),
                    Some("Expected an array of file URIs".to_owned()),
                );
                conn.send.send_message_write_all(&error)?;
                continue;
            }
        };
        let _startup_id: String = parser.get().unwrap_or_default();

        if member == "ShowItems" || member == "ShowFolders" {
            let paths: Vec<PathBuf> = uris.iter().filter_map(|uri| file_uri_to_path(uri)).collect();
            if !paths.is_empty() && tx.send(paths).is_ok() {
                let _ = wake.write_all(&[1]);
            }
        } else {
            let error = msg.dynheader.make_error_response(
                "org.freedesktop.DBus.Error.UnknownMethod".to_owned(),
                Some(format!("Unknown method: {member}")),
            );
            conn.send.send_message_write_all(&error)?;
            continue;
        }
        let reply = msg.dynheader.make_response();
        conn.send.send_message_write_all(&reply)?;
    }
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let encoded = if rest.starts_with('/') {
        rest.to_owned()
    } else if let Some(path) = rest.strip_prefix("localhost/") {
        format!("/{path}")
    } else {
        return None;
    };
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = *bytes.get(i + 1)?;
            let low = *bytes.get(i + 2)?;
            decoded.push((hex(high)? << 4) | hex(low)?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    if decoded.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(decoded)))
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_file_uris() {
        assert_eq!(file_uri_to_path("file:///tmp/a%20b.txt"), Some(PathBuf::from("/tmp/a b.txt")));
        assert_eq!(file_uri_to_path("file://localhost/tmp/a%23b"), Some(PathBuf::from("/tmp/a#b")));
        assert_eq!(file_uri_to_path("https://example.com/a"), None);
    }

    #[test]
    fn rejects_nul_bytes() {
        assert_eq!(file_uri_to_path("file:///tmp/a%00b"), None);
    }
}
