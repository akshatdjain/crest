//! Linux: what the Windows modules do with Win32, as far as every desktop allows. For now these are stand-ins so the shared
//! code builds and runs; each tool that needs more is hidden (src/notch/tabs.ts) until its real version lands here.
//! The audio and monitor watchers are the Mac's, which only use Tauri.

pub mod audio;
pub mod camera;
pub mod capture;
pub mod clipboard;
pub mod controls;
pub mod events;
pub mod files;
pub mod idle;
pub mod input;
pub mod media;
pub mod mixer;
pub mod notices;
pub mod pipewire;
pub mod portal;
pub mod quick;
pub mod status;
pub mod system;
pub mod secrets;
#[path = "../macos/mirror.rs"]
pub mod mirror;

use tauri::WebviewWindow;

/// Run through XWayland on a Wayland session. A native Wayland window can't choose its place, stay on top or hide from
/// Alt+Tab (and GNOME has no layer-shell), while an X11 window can; the pointer is not needed for hover (input.rs).
/// `CREST_WAYLAND=1` keeps native Wayland, and so does a GDK_BACKEND already set. Must run before GTK starts, from main.
pub fn prefer_x11() {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland");
    if wayland && std::env::var_os("DISPLAY").is_some() && std::env::var_os("GDK_BACKEND").is_none() && std::env::var_os("CREST_WAYLAND").is_none() {
        std::env::set_var("GDK_BACKEND", "x11");
    }
}

/// Desktops whose window manager already keeps an always-on-top, skip-taskbar window floating and out of the way. Crest's
/// behaviour there is as tested and stays as it is.
const FULL_DESKTOPS: [&str; 14] = ["gnome", "ubuntu", "kde", "plasma", "xfce", "x-cinnamon", "cinnamon", "mate", "lxqt", "lxde", "budgie", "pantheon", "deepin", "cosmic"];

/// The X11 window type the notch asks for, if it should ask for one.
///
/// A tiling window manager (i3, Sway, Hyprland, bspwm ...) tiles an ordinary window, which is wrong for an overlay at the screen
/// edge. Those managers float a window of one of the dialog-like types, so on a desktop that is not one of FULL_DESKTOPS the
/// notch is a `utility` window. `CREST_WINDOW_TYPE` (normal, utility, dock, notification, splash, dialog) overrides this.
pub fn window_type(desktop: &str, choice: Option<&str>) -> Option<gtk::gdk::WindowTypeHint> {
    use gtk::gdk::WindowTypeHint as Hint;
    match choice.map(str::to_ascii_lowercase).as_deref() {
        Some("normal") => return None,
        Some("utility") => return Some(Hint::Utility),
        Some("dock") => return Some(Hint::Dock),
        Some("notification") => return Some(Hint::Notification),
        Some("splash") => return Some(Hint::Splashscreen),
        Some("dialog") => return Some(Hint::Dialog),
        _ => {}
    }
    let full = desktop.split(':').any(|d| FULL_DESKTOPS.contains(&d.trim().to_ascii_lowercase().as_str()));
    (!full).then_some(Hint::Utility)
}

/// Sets the window type before the window is shown (the manager reads it when the window is mapped).
pub fn apply_window_hints(window: &WebviewWindow) {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let choice = std::env::var("CREST_WINDOW_TYPE").ok();
    if let (Some(hint), Ok(gtk_window)) = (window_type(&desktop, choice.as_deref()), window.gtk_window()) {
        gtk::prelude::GtkWindowExt::set_type_hint(&gtk_window, hint);
    }
}

/// The left mouse button is held (hit_test.rs). Linux does not poll the pointer (input.rs), so this is only here for the shared
/// code to build; nothing calls it.
#[allow(dead_code)]
pub fn left_button_down() -> bool {
    false
}

/// Above other windows. Wayland lets a client ask but not insist, so the real fix is a shell extension on GNOME.
pub fn raise(window: &WebviewWindow) {
    let _ = window.set_always_on_top(true);
}

/// The app that has the keyboard (lib.rs keyboard_focus): no compositor-neutral way to read it, so none is remembered.
pub fn frontmost_pid() -> i32 {
    0
}

/// Gives the keyboard back to that app; false means there is no app to give it to.
pub fn activate_pid(_: i32) -> bool {
    false
}

/// Whether Crest may press keys in other apps (a paste from the history). Not on Linux yet.
pub fn may_press_keys() -> bool {
    false
}

pub fn press_paste() {}

/// Counts every change of the clipboard (clipboard.rs, which calls it by the Mac's name).
pub fn pasteboard_change_count() -> u32 {
    clipboard::change_count()
}

/// Password managers' copies are kept out of the history.
pub fn pasteboard_is_private() -> bool {
    clipboard::is_private()
}

/// Commands the shared interface sends or polls (memory.rs, sysevents.rs, privacy.rs on Windows): answered so the log stays clean.
#[tauri::command]
pub fn memory_low() {}

#[tauri::command]
pub fn glass_live_supported() -> serde_json::Value {
    serde_json::json!({ "supported": false, "reason": "unsupported" })
}

#[tauri::command]
pub fn glass_live_exclude(on: bool) -> Result<(), String> {
    if on {
        Err("unsupported".into())
    } else {
        Ok(())
    }
}

#[tauri::command]
pub fn glass_live_start() -> Result<(), String> {
    Err("unsupported".into())
}

#[tauri::command]
pub fn glass_live_stop() {}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::gdk::WindowTypeHint as Hint;

    #[test]
    fn full_desktops_keep_a_normal_window_and_tiling_ones_get_a_utility_window() {
        for full in ["ubuntu:GNOME", "GNOME", "KDE", "X-Cinnamon", "XFCE", "Budgie:GNOME"] {
            assert_eq!(window_type(full, None), None, "{full}");
        }
        for tiling in ["sway", "Hyprland", "i3", "", "bspwm", "river"] {
            assert_eq!(window_type(tiling, None), Some(Hint::Utility), "{tiling:?}");
        }
    }

    #[test]
    fn the_environment_overrides_the_choice() {
        assert_eq!(window_type("sway", Some("normal")), None);
        assert_eq!(window_type("GNOME", Some("Dock")), Some(Hint::Dock));
        assert_eq!(window_type("GNOME", Some("splash")), Some(Hint::Splashscreen));
        // An unknown value falls back to the desktop's own choice.
        assert_eq!(window_type("GNOME", Some("nonsense")), None);
    }
}
