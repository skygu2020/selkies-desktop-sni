//! D-Bus implementation of org.freedesktop.FileManager1.
//!
//! Browsers call ShowItems when the user asks to reveal a downloaded file.
//! Keep this service on its own connection/thread so the desktop event loop
//! remains non-blocking.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use rustbus::connection::Timeout;
use rustbus::connection::ll_conn::DuplexConn;
use rustbus::message_builder::MessageType;
use rustbus::standard_messages;

const SERVICE: &str = "org.freedesktop.FileManager1";
const PATH: &str = "/org/freedesktop/FileManager1";
const INTERFACE: &str = "org.freedesktop.FileManager1";

/// Starts the FileManager1 service and returns paths requested by clients.
pub fn start() -> Receiver<Vec<PathBuf>> {
    let (tx, rx) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("file-manager-dbus".into())
        .spawn(move || {
            if let Err(error) = run(tx) {
                eprintln!("selkies-desktop: FileManager1 service stopped: {error}");
            }
        });
    rx
}

fn run(tx: Sender<Vec<PathBuf>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
        if msg.typ != MessageType::MethodCall
            || msg.dynheader.interface.as_deref() != Some(INTERFACE)
            || msg.dynheader.object.as_deref() != Some(PATH)
        {
            continue;
        }
        let member = msg.dynheader.member.as_deref().unwrap_or("");
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
            if !paths.is_empty() {
                let _ = tx.send(paths);
            }
        } else if member != "ShowItemProperties" {
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
