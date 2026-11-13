//! Calls to the desktop portals (xdg-desktop-portal), which every current desktop provides in its own style: GNOME, KDE, wlroots'
//! and the rest each draw their own colour picker and screenshot UI behind the same interface.
//!
//! A portal method that needs the user answers later: the call returns at once with a Request object, and the answer arrives as a
//! `Response` signal on it. The path of that object can be worked out in advance from the caller's bus name and a token, so the
//! signal is subscribed to before the call and cannot be missed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

const DESKTOP: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";

pub struct Response {
    /// 0 = done, 1 = the user cancelled, 2 = anything else.
    pub code: u32,
    pub results: HashMap<String, OwnedValue>,
}

static TOKENS: AtomicU64 = AtomicU64::new(0);

/// A bus name as the portal writes it into a request path: no colon, dots as underscores.
fn sender_part(unique_name: &str) -> String {
    unique_name.trim_start_matches(':').replace('.', "_")
}

/// Calls `interface.method(parent_window = "", options)` and waits up to `timeout` for the answer.
pub fn request(interface: &'static str, method: &'static str, mut options: HashMap<&'static str, Value<'static>>, timeout: Duration) -> Result<Response, String> {
    let (tx, rx) = mpsc::channel();
    // On a thread of its own, so that waiting for a user who walked away does not hold the caller.
    thread::spawn(move || {
        let _ = tx.send(ask(interface, method, &mut options));
    });
    rx.recv_timeout(timeout).map_err(|_| "timeout".to_string())?
}

fn ask(interface: &str, method: &str, options: &mut HashMap<&'static str, Value<'static>>) -> Result<Response, String> {
    let conn = Connection::session().map_err(|e| e.to_string())?;
    let unique = conn.unique_name().ok_or("no bus name")?.to_string();
    let token = format!("crest{}_{}", std::process::id(), TOKENS.fetch_add(1, Ordering::Relaxed));
    let request_path = format!("/org/freedesktop/portal/desktop/request/{}/{token}", sender_part(&unique));
    let request = Proxy::new(&conn, DESKTOP, request_path, "org.freedesktop.portal.Request").map_err(|e| e.to_string())?;
    // Subscribed before the call: the answer may come at once.
    let mut responses = request.receive_signal("Response").map_err(|e| e.to_string())?;
    options.insert("handle_token", Value::from(token));
    let portal = Proxy::new(&conn, DESKTOP, PATH, interface.to_string()).map_err(|e| e.to_string())?;
    portal.call_method(method, &("", &*options)).map_err(|e| e.to_string())?;
    let message = responses.next().ok_or("no answer")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = message.body().deserialize().map_err(|e| e.to_string())?;
    Ok(Response { code, results })
}

/// A `file://` link as a path: the link is percent-encoded.
pub fn path_from_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            out.push(u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bus_name_becomes_the_part_of_a_request_path() {
        assert_eq!(sender_part(":1.407"), "1_407");
        assert_eq!(sender_part(":1.12.3"), "1_12_3");
    }

    #[test]
    fn file_links_are_decoded_into_paths() {
        assert_eq!(path_from_uri("file:///home/me/Pictures/Screenshot%20from%202026.png").as_deref(), Some("/home/me/Pictures/Screenshot from 2026.png"));
        assert_eq!(path_from_uri("file:///plain.png").as_deref(), Some("/plain.png"));
        assert_eq!(path_from_uri("https://example.com/x.png"), None);
        assert_eq!(path_from_uri("file:///bad%zz.png"), None);
        // A trailing percent is not an escape.
        assert_eq!(path_from_uri("file:///a%2").as_deref(), Some("/a%2"));
    }
}
