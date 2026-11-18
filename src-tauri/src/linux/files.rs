//! Showing a file in its folder on Linux (the Windows `explorer /select` and the Mac's `open -R` of convert.rs).
//!
//! The freedesktop file-manager interface does it: `ShowItems` opens the folder with the file selected in whichever file manager the
//! desktop uses (Nautilus, Dolphin, Thunar ...). Without a service that answers, the folder is opened by `xdg-open` instead.

use std::path::Path;
use std::process::Command;

use zbus::blocking::{Connection, Proxy};

/// A path as a `file://` link: everything but letters, digits, `-._~` and `/` is percent-encoded, byte by byte.
fn file_uri(path: &str) -> String {
    let mut uri = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

fn show_items(uri: &str) -> Result<(), String> {
    let conn = Connection::session().map_err(|e| e.to_string())?;
    let manager = Proxy::new(&conn, "org.freedesktop.FileManager1", "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1").map_err(|e| e.to_string())?;
    manager.call_method("ShowItems", &(vec![uri], "")).map(drop).map_err(|e| e.to_string())
}

/// Shows `path` selected in the file manager, or its folder when there is no file manager service.
pub fn reveal(path: &str) -> Result<(), String> {
    if show_items(&file_uri(path)).is_ok() {
        return Ok(());
    }
    let folder = Path::new(path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
    Command::new("xdg-open").arg(folder).spawn().map(drop).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_becomes_an_encoded_file_link() {
        assert_eq!(file_uri("/home/me/Pictures/a.png"), "file:///home/me/Pictures/a.png");
        assert_eq!(file_uri("/home/me/My Shots/Screenshot 1 (2).png"), "file:///home/me/My%20Shots/Screenshot%201%20%282%29.png");
        // Bytes of a multi-byte character are each encoded.
        assert_eq!(file_uri("/tmp/é.txt"), "file:///tmp/%C3%A9.txt");
        assert_eq!(file_uri("/a%b#c?d"), "file:///a%25b%23c%3Fd");
    }

    #[test]
    fn the_opener_is_xdg_open_on_linux() {
        assert_eq!(crate::opener().get_program(), "xdg-open");
    }
}
