//! Desktop notifications in the notch on Linux (the Windows ones are notices.rs).
//!
//! Apps send notifications to the desktop's notification daemon over D-Bus (`org.freedesktop.Notifications.Notify`). Crest
//! becomes a monitor of the session bus (the mechanism behind `dbus-monitor`): it sees those calls and their answers without
//! taking the notification name from the daemon, so the desktop's own banners keep working. Each call becomes a notice with the
//! shape the page already knows, with `source: "windows"` because that is the source the page offers "open the app" and "delete
//! from the centre" for.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use zbus::blocking::fdo::{DBusProxy, MonitoringProxy};
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Type as MessageType;
use zbus::names::{BusName, UniqueName};
use zbus::zvariant::OwnedValue;
use zbus::MatchRule;

use super::media::{number, text};
use crate::notify_wire::clean;

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
/// The icon is sent as a data URL no bigger than this, at most this many pixels square (the page shows it small).
const ICON_MAX_BYTES: usize = 256 * 1024;
const ICON_PX: u32 = 48;
/// A call whose answer does not come within this long is dropped.
const PENDING_FOR: Duration = Duration::from_secs(10);

// ---- what the page receives -----------------------------------------------------------------------------------------

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    access: &'static str,
    mode: &'static str,
    pipe: bool,
    pipe_failed: bool,
}

#[derive(Clone, Serialize, Default, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NoticeIn {
    source: &'static str,
    app_id: String,
    app_name: String,
    title: String,
    body: String,
    icon: Option<String>,
    at: u64,
    replace: Option<String>,
    progress: Option<f64>,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    command: Option<String>,
    windows_id: Option<u32>,
}

#[derive(Default)]
pub struct NoticesState {
    /// Bumped by every start and stop, so a monitor thread knows when it has been replaced.
    generation: AtomicU64,
    status: Mutex<Option<&'static str>>,
}

fn status(mode: &'static str) -> Status {
    Status { access: "allowed", mode, pipe: false, pipe_failed: false }
}

// ---- turning a Notify call into a notice ----------------------------------------------------------------------------

/// The parts of a Notify call that matter, as read from its arguments.
#[derive(Debug, Default, PartialEq)]
struct Raw {
    app_name: String,
    app_icon: String,
    summary: String,
    body: String,
    desktop_entry: Option<String>,
    image_path: Option<String>,
    value: Option<i32>,
    /// A volume or brightness popup: the desktop's own indicator, not a message.
    osd: bool,
}

/// Basic notification markup (`<b>`, `<i>`, `<a href>`, `<br>`) and the XML entities, as plain text.
fn strip_markup(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for c in body.chars() {
        match (in_tag, c) {
            (false, '<') => {
                in_tag = true;
                tag.clear();
            }
            (true, '>') => {
                in_tag = false;
                if tag.trim_start_matches('/').to_ascii_lowercase().starts_with("br") {
                    out.push('\n');
                }
            }
            (true, c) => tag.push(c),
            (false, c) => out.push(c),
        }
    }
    out.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&#39;", "'").replace("&amp;", "&")
}

/// An id the page may hand back to open the app: letters, digits and a few separators, as `appOpens` (history.ts) allows.
fn safe_id(raw: &str) -> String {
    let id: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || "._-+()@#=".contains(c) { c } else { '-' }).take(200).collect();
    id.trim_matches('-').to_string()
}

fn notice_from(raw: Raw, id: u32, at: u64, icon: Option<String>) -> Option<NoticeIn> {
    if raw.osd {
        return None;
    }
    let app_name = clean(Some(raw.app_name.clone()), 128, false).unwrap_or_default();
    let title = clean(Some(raw.summary), 256, false);
    let body = clean(Some(strip_markup(&raw.body)), 4000, true);
    // Nothing to show.
    if title.is_none() && body.is_none() {
        return None;
    }
    // The desktop entry names the app best and is what launches it; without one the app's name stands in.
    let app_id = raw.desktop_entry.as_deref().map(safe_id).filter(|i| !i.is_empty()).unwrap_or_else(|| safe_id(&app_name.to_lowercase()));
    Some(NoticeIn {
        source: "windows",
        app_name: if app_name.is_empty() { app_id.clone() } else { app_name },
        app_id,
        title: title.unwrap_or_default(),
        body: body.unwrap_or_default(),
        icon,
        at,
        // The same id on every notice: an app that updates one (progress, a chat) replaces it in the history.
        replace: Some(id.to_string()),
        progress: raw.value.filter(|v| (0..=100).contains(v)).map(f64::from),
        windows_id: Some(id),
        ..NoticeIn::default()
    })
}

// ---- icons ----------------------------------------------------------------------------------------------------------

fn data_url(mime: &str, bytes: &[u8]) -> Option<String> {
    (bytes.len() <= ICON_MAX_BYTES).then(|| format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

fn mime_of(path: &str) -> &'static str {
    match path.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("svg") => "image/svg+xml",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

/// A file named by a path or a `file://` link.
fn icon_file(spec: &str) -> Option<String> {
    let path = spec.strip_prefix("file://").unwrap_or(spec);
    if !path.starts_with('/') {
        return None;
    }
    data_url(mime_of(path), &std::fs::read(path).ok()?)
}

/// Where icon files live: a theme folder holds size folders (48x48, scalable ...), which hold a folder per kind of icon.
const ICON_SIZES: [&str; 6] = ["48x48", "64x64", "scalable", "128x128", "256x256", "32x32"];

fn icon_in(root: &std::path::Path, name: &str) -> Option<PathBuf> {
    // Pixmaps are loose files.
    for ext in ["png", "svg"] {
        let loose = root.join(format!("{name}.{ext}"));
        if loose.is_file() {
            return Some(loose);
        }
    }
    for theme in std::fs::read_dir(root).ok()?.flatten() {
        for size in ICON_SIZES {
            for kind in std::fs::read_dir(theme.path().join(size)).into_iter().flatten().flatten() {
                for ext in ["png", "svg"] {
                    let path = kind.path().join(format!("{name}.{ext}"));
                    if path.is_file() {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

/// A theme icon by name, looked for in the icon folders (the full theme lookup, with inheritance, is not done).
fn icon_theme(name: &str) -> Option<String> {
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    [home.join(".local/share/icons"), PathBuf::from("/usr/share/icons"), PathBuf::from("/usr/share/pixmaps")]
        .iter()
        .find_map(|root| icon_in(root, name))
        .and_then(|path| icon_file(&path.to_string_lossy()))
}

/// The `image-data` hint: width, height, row stride, alpha, bits per sample, channels and the raw pixels, as a PNG.
fn icon_pixels(value: &OwnedValue) -> Option<String> {
    let (w, h, stride, alpha, bits, channels, data): (i32, i32, i32, bool, i32, i32, Vec<u8>) = value.try_clone().ok()?.try_into().ok()?;
    if bits != 8 || !(3..=4).contains(&channels) || w <= 0 || h <= 0 || w > 4096 || h > 4096 || stride < w * channels {
        return None;
    }
    let (w, h, stride, channels) = (w as usize, h as usize, stride as usize, channels as usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let line = data.get(row * stride..row * stride + w * channels)?;
        for px in line.chunks_exact(channels) {
            rgba.extend_from_slice(&[px[0], px[1], px[2], if alpha && channels == 4 { px[3] } else { 255 }]);
        }
    }
    let image = image::RgbaImage::from_raw(w as u32, h as u32, rgba)?;
    let image = if w as u32 > ICON_PX || h as u32 > ICON_PX { image::imageops::thumbnail(&image, ICON_PX, ICON_PX) } else { image };
    let mut png = Cursor::new(Vec::new());
    image.write_to(&mut png, image::ImageFormat::Png).ok()?;
    data_url("image/png", &png.into_inner())
}

// ---- the monitor ----------------------------------------------------------------------------------------------------

/// A call seen, waiting for the daemon's answer, which carries the id it gave the notification.
struct Pending {
    raw: Raw,
    icon: Option<String>,
    at: Instant,
}

type Args = (String, u32, String, String, String, Vec<String>, HashMap<String, OwnedValue>, i32);

fn read_call(message: &zbus::Message) -> Option<(Raw, Option<String>)> {
    let (app_name, _replaces, app_icon, summary, body, _actions, hints, _timeout): Args = message.body().deserialize().ok()?;
    let hint = |k: &str| hints.get(k);
    let raw = Raw {
        app_name,
        summary,
        body,
        desktop_entry: hint("desktop-entry").and_then(text),
        image_path: hint("image-path").or_else(|| hint("image_path")).and_then(text),
        value: hint("value").and_then(number).map(|n| n as i32),
        osd: hint("x-canonical-private-synchronous").is_some() || hint("synchronous").is_some(),
        app_icon,
    };
    if raw.osd {
        return Some((raw, None));
    }
    // Pixels sent with the call first, then a file, then the app's icon by path or by theme name.
    let icon = hint("image-data")
        .or_else(|| hint("image_data"))
        .or_else(|| hint("icon_data"))
        .and_then(icon_pixels)
        .or_else(|| raw.image_path.as_deref().and_then(|p| icon_file(p).or_else(|| icon_theme(p))))
        .or_else(|| icon_file(&raw.app_icon).or_else(|| icon_theme(&raw.app_icon)));
    Some((raw, icon))
}

/// What the monitor is told about, and from whom: the notification daemon's traffic and nothing else. A rule for every method
/// return on the bus would hand Crest every reply every app gets, the keyring's included, so each rule names the daemon.
fn rules(owner: &str) -> Result<Vec<MatchRule<'static>>, String> {
    let owner = UniqueName::try_from(owner.to_string()).map_err(|e| e.to_string())?;
    let built = || -> Result<Vec<MatchRule<'static>>, zbus::Error> {
        Ok(vec![
            // Calls to Notify, sent to the daemon.
            MatchRule::builder().msg_type(MessageType::MethodCall).interface(NOTIFICATIONS)?.member("Notify")?.destination(owner.clone())?.build(),
            // Its answers, which carry the id of the notification just sent.
            MatchRule::builder().msg_type(MessageType::MethodReturn).sender(owner.clone())?.build(),
            MatchRule::builder().msg_type(MessageType::Signal).sender(owner.clone())?.interface(NOTIFICATIONS)?.member("NotificationClosed")?.build(),
            // The daemon going away or being replaced: the rules above name its bus name, so the watch starts over.
            MatchRule::builder().msg_type(MessageType::Signal).sender("org.freedesktop.DBus")?.interface("org.freedesktop.DBus")?.member("NameOwnerChanged")?.arg(0, NOTIFICATIONS)?.build(),
        ])
    };
    built().map_err(|e| e.to_string())
}

/// The bus name the notification daemon has right now.
fn daemon_owner() -> Result<String, String> {
    let conn = Connection::session().map_err(|e| e.to_string())?;
    let proxy = DBusProxy::new(&conn).map_err(|e| e.to_string())?;
    let name = BusName::try_from(NOTIFICATIONS).map_err(|e| e.to_string())?;
    let owner = proxy.get_name_owner(name).map_err(|_| "no notification daemon".to_string())?;
    Ok(owner.to_string())
}

enum Watched {
    Stop,
    /// The daemon changed: look it up again.
    DaemonChanged,
}

/// One watch of the daemon as it is now.
fn watch(on_in: &mut impl FnMut(NoticeIn), on_gone: &mut impl FnMut(u32), keep_going: &impl Fn() -> bool) -> Result<Watched, String> {
    let rules = rules(&daemon_owner()?)?;
    // A monitor can send nothing, and BecomeMonitor works once per connection: this one is only for watching.
    let conn = Connection::session().map_err(|e| e.to_string())?;
    MonitoringProxy::new(&conn).map_err(|e| e.to_string())?.become_monitor(&rules, 0).map_err(|e| e.to_string())?;

    let mut pending: HashMap<(String, u32), Pending> = HashMap::new();
    for message in MessageIterator::from(conn) {
        if !keep_going() {
            return Ok(Watched::Stop);
        }
        let Ok(message) = message else { continue };
        let header = message.header();
        pending.retain(|_, p| p.at.elapsed() < PENDING_FOR);
        match header.message_type() {
            MessageType::MethodCall if header.member().is_some_and(|m| m.as_str() == "Notify") => {
                let (Some(sender), serial) = (header.sender().map(|s| s.to_string()), message.primary_header().serial_num().get()) else { continue };
                if let Some((raw, icon)) = read_call(&message) {
                    pending.insert((sender, serial), Pending { raw, icon, at: Instant::now() });
                }
            }
            MessageType::MethodReturn => {
                let (Some(to), Some(serial)) = (header.destination().map(|d| d.to_string()), header.reply_serial().map(|s| s.get())) else { continue };
                let Some(call) = pending.remove(&(to, serial)) else { continue };
                let Ok(id) = message.body().deserialize::<u32>() else { continue };
                if let Some(notice) = notice_from(call.raw, id, crate::now_ms(), call.icon) {
                    on_in(notice);
                }
            }
            MessageType::Signal => match header.member().map(|m| m.as_str()) {
                Some("NameOwnerChanged") => return Ok(Watched::DaemonChanged),
                Some("NotificationClosed") => {
                    if let Ok((id, _reason)) = message.body().deserialize::<(u32, u32)>() {
                        on_gone(id);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(Watched::Stop)
}

/// Watches until `keep_going` says to stop (checked on each message), telling `on_in` about each notice and `on_gone` about each
/// notification the desktop closed. A daemon that is replaced while this runs is picked up again.
fn monitor(mut on_in: impl FnMut(NoticeIn), mut on_gone: impl FnMut(u32), keep_going: impl Fn() -> bool) -> Result<(), String> {
    loop {
        match watch(&mut on_in, &mut on_gone, &keep_going)? {
            Watched::Stop => return Ok(()),
            Watched::DaemonChanged => thread::sleep(Duration::from_millis(500)),
        }
    }
}

// ---- commands -------------------------------------------------------------------------------------------------------

/// Starts or stops the monitor: `windows` is the page's switch for this source (the name is Windows', the meaning is "the desktop's
/// notifications"). `pipe` is crest-notify's, which has no Linux version.
#[tauri::command]
pub fn notices_configure(windows: bool, pipe: bool, app: AppHandle, state: State<'_, NoticesState>) {
    let _ = pipe;
    let mut mode = state.status.lock().unwrap();
    let running = *mode == Some("event-log");
    if windows == running {
        return;
    }
    // A new generation ends the old monitor on the next message it sees.
    let generation = state.generation.fetch_add(1, Ordering::Relaxed) + 1;
    if !windows {
        *mode = Some("off");
        let _ = app.emit("notices://status", status("off"));
        return;
    }
    *mode = Some("event-log");
    let _ = app.emit("notices://status", status("event-log"));
    let handle = app.clone();
    thread::spawn(move || {
        let state = handle.state::<NoticesState>();
        let (on_in, on_gone) = (handle.clone(), handle.clone());
        let current = || handle.state::<NoticesState>().generation.load(Ordering::Relaxed) == generation;
        if let Err(error) = monitor(move |notice| drop(on_in.emit("notices://in", notice)), move |id| drop(on_gone.emit("notices://gone", vec![id])), current) {
            eprintln!("notices: the session bus would not let Crest watch notifications: {error}");
            // Only if this is still the monitor that was asked for.
            if state.generation.load(Ordering::Relaxed) == generation {
                *state.status.lock().unwrap() = Some("failed");
                let _ = handle.emit("notices://status", status("failed"));
            }
        }
    });
}

#[tauri::command]
pub fn notices_status(state: State<'_, NoticesState>) -> Status {
    status(state.status.lock().unwrap().unwrap_or("off"))
}

/// There is no permission to ask for on Linux.
#[tauri::command]
pub fn notices_request_access() -> &'static str {
    "allowed"
}

/// Deletes notifications from the desktop's own centre, as the user deleted them in Crest's.
#[tauri::command]
pub async fn notices_remove_windows(ids: Vec<u32>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let conn = Connection::session().map_err(|e| e.to_string())?;
        let daemon = Proxy::new(&conn, NOTIFICATIONS, "/org/freedesktop/Notifications", NOTIFICATIONS).map_err(|e| e.to_string())?;
        for id in ids {
            // One that is already gone is not an error.
            let _ = daemon.call_method("CloseNotification", &(id,));
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Whether an app id may be handed to the launcher: the same characters the page allows (`appOpens`, history.ts).
fn launchable(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && !id.contains("..") && !id.starts_with('-') && id.chars().all(|c| c.is_ascii_alphanumeric() || "._-!{}\\ +()@#=".contains(c))
}

/// Opens the app a notice came from, by its desktop entry.
#[tauri::command]
pub async fn notices_open_app(app_id: String) -> Result<(), String> {
    if !launchable(&app_id) {
        return Err("invalid".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let id = app_id.trim_end_matches(".desktop");
        let launched = std::process::Command::new("gtk-launch").arg(id).status().is_ok_and(|s| s.success());
        if launched {
            return Ok(());
        }
        std::process::Command::new("gio").args(["launch", &format!("/usr/share/applications/{id}.desktop")]).status().ok().filter(|s| s.success()).map(drop).ok_or_else(|| "not-found".to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Only the asks of crest-notify, which has no Linux version.
#[tauri::command]
pub fn notices_answer(id: u64, choice: Option<String>) -> bool {
    let _ = (id, choice);
    false
}

/// A one-time code is four to eight digits.
fn valid_code(code: &str) -> bool {
    (4..=8).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_digit())
}

/// Copies a one-time code. It stays out of the clipboard history, as a password manager's copy does.
#[tauri::command]
pub fn notices_copy_code(code: String) -> Result<(), String> {
    if !valid_code(&code) {
        return Err("invalid".into());
    }
    super::clipboard::copy_private(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(summary: &str, body: &str) -> Raw {
        Raw { app_name: "Files".into(), summary: summary.into(), body: body.into(), ..Raw::default() }
    }

    /// Sends a real notification through the session bus while a monitor runs, and expects to see it, whole. It shows a banner on the
    /// desktop for a moment. `cargo test -- --ignored --nocapture linux::notices`
    #[test]
    #[ignore = "sends a real desktop notification"]
    fn a_real_notification_is_seen_by_the_monitor() {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let watcher = thread::spawn(move || monitor(move |n| drop(tx.send(n)), |_| {}, move || !flag.load(Ordering::Relaxed)));
        // Give the monitor time to become one before the notification is sent.
        thread::sleep(Duration::from_millis(800));
        let conn = Connection::session().unwrap();
        let daemon = Proxy::new(&conn, NOTIFICATIONS, "/org/freedesktop/Notifications", NOTIFICATIONS).unwrap();
        let hints: HashMap<&str, zbus::zvariant::Value> = HashMap::from([("desktop-entry", "org.gnome.Nautilus".into()), ("value", 40i32.into())]);
        let id: u32 = daemon.call_method("Notify", &("Crest test", 0u32, "dialog-information", "Crest live test", "<b>Hello</b> &amp; welcome", Vec::<&str>::new(), hints, 3000i32)).unwrap().body().deserialize().unwrap();
        let got = rx.recv_timeout(Duration::from_secs(5)).expect("the monitor saw nothing");
        println!("notice seen: {got:?}");
        assert_eq!((got.title.as_str(), got.body.as_str(), got.app_name.as_str(), got.app_id.as_str()), ("Crest live test", "Hello & welcome", "Crest test", "org.gnome.Nautilus"));
        assert_eq!((got.windows_id, got.progress, got.source), (Some(id), Some(40.0), "windows"));
        let _ = daemon.call_method("CloseNotification", &(id,));
        // One more message ends the watcher.
        stop.store(true, Ordering::Relaxed);
        let _ = daemon.call_method("GetCapabilities", &());
        let _ = watcher.join();
    }

    #[test]
    fn every_rule_names_the_daemon_and_no_reply_is_taken_from_anyone_else() {
        let rules = rules(":1.42").unwrap();
        assert_eq!(rules.len(), 4);
        let texts: Vec<String> = rules.iter().map(|r| r.to_string()).collect();
        for t in &texts {
            assert!(t.contains("sender=") || t.contains("destination="), "a rule that is open to everyone: {t}");
        }
        let returns: Vec<&String> = texts.iter().filter(|t| t.contains("method_return")).collect();
        assert_eq!(returns.len(), 1);
        assert!(returns[0].contains("sender=':1.42'"), "{}", returns[0]);
        assert!(rules_for_bad_name().is_err());
    }

    fn rules_for_bad_name() -> Result<Vec<MatchRule<'static>>, String> {
        rules("not a bus name")
    }

    /// Other services' traffic (the bus itself, the keyring) is sent while the monitor listens, and none of their replies may arrive; the
    /// daemon's own, for a real notification, must. `cargo test -- --ignored --nocapture linux::notices`
    #[test]
    #[ignore = "uses the live session bus and shows a banner"]
    fn the_monitor_gets_the_daemons_traffic_and_no_other_apps_replies() {
        let owner = daemon_owner().unwrap();
        let conn = Connection::session().unwrap();
        MonitoringProxy::new(&conn).unwrap().become_monitor(&rules(&owner).unwrap(), 0).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for m in MessageIterator::from(conn).flatten() {
                let h = m.header();
                let _ = tx.send((h.message_type(), h.sender().map(|s| s.to_string()), h.member().map(|m| m.to_string())));
            }
        });
        thread::sleep(Duration::from_millis(800));
        let probe = Connection::session().unwrap();
        let bus = Proxy::new(&probe, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus").unwrap();
        let _ = bus.call_method("ListNames", &());
        // The keyring answers a call too, if it runs.
        let _ = Proxy::new(&probe, "org.freedesktop.secrets", "/org/freedesktop/secrets", "org.freedesktop.DBus.Peer").and_then(|p| p.call_method("Ping", &()));
        let daemon = Proxy::new(&probe, NOTIFICATIONS, "/org/freedesktop/Notifications", NOTIFICATIONS).unwrap();
        let hints: HashMap<&str, zbus::zvariant::Value> = HashMap::new();
        let id: u32 = daemon.call_method("Notify", &("Crest rule test", 0u32, "", "Rule test", "not a secret", Vec::<&str>::new(), hints, 2000i32)).unwrap().body().deserialize().unwrap();
        thread::sleep(Duration::from_millis(1200));
        let seen: Vec<_> = rx.try_iter().collect();
        println!("seen {} messages: {:?}", seen.len(), seen);
        assert!(seen.iter().any(|(t, _, m)| *t == MessageType::MethodCall && m.as_deref() == Some("Notify")), "the Notify call was not seen");
        let replies: Vec<_> = seen.iter().filter(|(t, _, _)| *t == MessageType::MethodReturn).collect();
        assert!(!replies.is_empty(), "the daemon's answer was not seen");
        for (_, sender, _) in &replies {
            assert_eq!(sender.as_deref(), Some(owner.as_str()), "a reply from another service reached the monitor");
        }
        let _ = daemon.call_method("CloseNotification", &(id,));
    }

    #[test]
    fn a_loose_icon_file_is_found_and_a_path_is_not_a_name() {
        let dir = std::env::temp_dir().join(format!("crest-icons-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("theme/48x48/apps")).unwrap();
        std::fs::write(dir.join("theme/48x48/apps/myapp.png"), b"png").unwrap();
        std::fs::write(dir.join("loose.svg"), b"<svg/>").unwrap();
        assert_eq!(icon_in(&dir, "myapp"), Some(dir.join("theme/48x48/apps/myapp.png")));
        assert_eq!(icon_in(&dir, "loose"), Some(dir.join("loose.svg")));
        assert_eq!(icon_in(&dir, "absent"), None);
        assert!(icon_theme("../../etc/passwd").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn markup_and_entities_become_plain_text() {
        assert_eq!(strip_markup("<b>Bold</b> and <i>italic</i> &amp; <a href=\"x\">link</a>"), "Bold and italic & link");
        assert_eq!(strip_markup("line<br/>two<br>three"), "line\ntwo\nthree");
        assert_eq!(strip_markup("5 &lt; 6 &gt; 4"), "5 < 6 > 4");
    }

    #[test]
    fn a_notice_carries_the_desktop_entry_and_the_ids() {
        let mut r = raw("Copy finished", "<b>3</b> files");
        r.desktop_entry = Some("org.gnome.Nautilus".into());
        r.value = Some(40);
        let n = notice_from(r, 17, 1000, None).unwrap();
        assert_eq!((n.source, n.app_id.as_str(), n.app_name.as_str(), n.title.as_str(), n.body.as_str()), ("windows", "org.gnome.Nautilus", "Files", "Copy finished", "3 files"));
        assert_eq!((n.replace.as_deref(), n.windows_id, n.progress, n.at), (Some("17"), Some(17), Some(40.0), 1000));
    }

    #[test]
    fn without_a_desktop_entry_the_apps_name_is_the_id() {
        let n = notice_from(raw("Hi", ""), 1, 0, None).unwrap();
        assert_eq!(n.app_id, "files");
        let mut r = raw("Hi", "");
        r.app_name = "My App / Beta".into();
        assert_eq!(notice_from(r, 1, 0, None).unwrap().app_id, "my-app---beta");
    }

    #[test]
    fn nothing_to_show_the_osd_and_a_bad_progress_are_handled() {
        assert!(notice_from(raw("", "  "), 1, 0, None).is_none());
        assert!(notice_from(raw("", "<b></b>"), 1, 0, None).is_none(), "markup alone is nothing");
        let mut osd = raw("Volume", "50");
        osd.osd = true;
        assert!(notice_from(osd, 1, 0, None).is_none(), "the desktop's own volume popup");
        let mut r = raw("x", "y");
        r.value = Some(150);
        assert_eq!(notice_from(r, 1, 0, None).unwrap().progress, None);
    }

    #[test]
    fn titles_and_bodies_are_cleaned_and_cut_like_the_windows_ones() {
        let n = notice_from(raw(&format!("a\u{202E}b{}", "x".repeat(400)), &"y".repeat(5000)), 1, 0, None).unwrap();
        assert!(n.title.starts_with("ab") && n.title.chars().count() == 256);
        assert_eq!(n.body.chars().count(), 4000);
    }

    #[test]
    fn only_launchable_ids_and_real_codes_are_accepted() {
        assert!(launchable("org.gnome.Nautilus") && launchable("firefox_firefox"));
        assert!(!launchable("") && !launchable("a/../b") && !launchable("-rf") && !launchable("a;b") && !launchable("a$b") && !launchable(&"x".repeat(600)));
        assert!(valid_code("1234") && valid_code("12345678"));
        assert!(!valid_code("123") && !valid_code("123456789") && !valid_code("12a456") && !valid_code(""));
    }

    #[test]
    fn pixels_become_a_small_png_and_bad_ones_are_refused() {
        // A 2x1 image, 4 channels, with a stride of exactly two pixels.
        let ok = OwnedValue::try_from(zbus::zvariant::Value::from((2i32, 1i32, 8i32, true, 8i32, 4i32, vec![255u8, 0, 0, 255, 0, 255, 0, 255]))).unwrap();
        let url = icon_pixels(&ok).unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
        let short = OwnedValue::try_from(zbus::zvariant::Value::from((2i32, 1i32, 8i32, true, 8i32, 4i32, vec![1u8, 2, 3]))).unwrap();
        assert!(icon_pixels(&short).is_none(), "fewer bytes than the size says");
        let deep = OwnedValue::try_from(zbus::zvariant::Value::from((1i32, 1i32, 4i32, true, 16i32, 4i32, vec![0u8; 4]))).unwrap();
        assert!(icon_pixels(&deep).is_none(), "only 8 bits per sample");
    }
}
