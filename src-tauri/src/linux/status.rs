//! Privacy dots and the status notices on Linux (the Windows ones are privacy.rs and sysevents.rs).
//!
//! - Privacy: one `pw-dump` says which apps capture the microphone, the camera or the screen. PipeWire is where all three
//!   go on a current desktop (a screen share through the portal is a PipeWire stream), and apps that open /dev/video*
//!   directly are found in /proc.
//! - Status: charger, battery, internet, Bluetooth, Caps/Num Lock and volume or brightness changes become the same
//!   `status://...` events the page already shows as notices. One thread watches only what the page asked for, and the
//!   first reading of each source is silent: it is the baseline, not news.

use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::controls::{self, run};
use super::media::{number, text};

// ---- privacy --------------------------------------------------------------------------------------------------------

#[derive(Serialize, Debug, PartialEq, Default)]
pub struct Privacy {
    mic: bool,
    camera: bool,
    screen: bool,
}

fn truthy(v: &Value) -> bool {
    v.as_bool().unwrap_or(false) || v.as_str() == Some("true")
}

/// What `pw-dump` shows being captured. `own_pid` is Crest's own process: its equalizer reads the output's monitor, which is
/// not a microphone, but it is excluded by name as well (`stream.capture.sink`).
fn parse_privacy(json: &[u8], own_pid: u64) -> Privacy {
    let Ok(Value::Array(objects)) = serde_json::from_slice::<Value>(json) else { return Privacy::default() };
    let mut p = Privacy::default();
    for o in objects.iter().filter(|o| o["type"] == "PipeWire:Interface:Node") {
        if o["info"]["state"] != "running" {
            continue;
        }
        let props = &o["info"]["props"];
        if props["application.process.id"].as_u64() == Some(own_pid) {
            continue;
        }
        match props["media.class"].as_str().unwrap_or("") {
            "Stream/Input/Audio" if !truthy(&props["stream.capture.sink"]) && !truthy(&props["stream.monitor"]) => p.mic = true,
            "Audio/Source" => p.mic = true,
            "Video/Source" => p.camera = true,
            // A stream that reads video is the camera's own consumer, or a screen share (the portal's screencast).
            "Stream/Input/Video" if props["media.role"] == "Camera" => p.camera = true,
            "Stream/Input/Video" => p.screen = true,
            _ => {}
        }
    }
    p
}

/// An app that opened a camera device itself, without PipeWire. Only processes of the user can be looked at, and the PipeWire
/// daemons are skipped: they hold the device while probing it.
fn video_device_open(own_pid: u32) -> bool {
    const SERVERS: [&str; 4] = ["pipewire", "wireplumber", "pipewire-pulse", "xdg-desktop-portal"];
    let Ok(procs) = fs::read_dir("/proc") else { return false };
    procs.flatten().any(|e| {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { return false };
        if pid == own_pid {
            return false;
        }
        if fs::read_to_string(e.path().join("comm")).is_ok_and(|c| SERVERS.contains(&c.trim())) {
            return false;
        }
        fs::read_dir(e.path().join("fd")).is_ok_and(|fds| fds.flatten().any(|fd| fs::read_link(fd.path()).is_ok_and(|t| t.to_string_lossy().starts_with("/dev/video"))))
    })
}

#[tauri::command(async)]
pub fn privacy_state() -> Privacy {
    let own = std::process::id();
    let mut p = run("pw-dump", &[]).map(|dump| parse_privacy(dump.as_bytes(), u64::from(own))).unwrap_or_default();
    p.camera = p.camera || video_device_open(own);
    p
}

// ---- status events --------------------------------------------------------------------------------------------------

#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
pub struct Power {
    ac: bool,
    percent: Option<u8>,
    /// False = the machine has no battery.
    battery: bool,
    charging: bool,
    saver: bool,
}

/// One entry of /sys/class/power_supply.
struct Supply {
    kind: String,
    status: String,
    capacity: Option<u8>,
    online: Option<bool>,
}

fn summarize_power(supplies: &[Supply]) -> Option<Power> {
    if supplies.is_empty() {
        return None;
    }
    let battery = supplies.iter().find(|s| s.kind == "Battery");
    let status = battery.map_or("", |b| b.status.as_str());
    let mains = supplies.iter().filter(|s| s.kind == "Mains").find_map(|s| s.online);
    Some(Power {
        // On mains when a charger says so; without one, anything but discharging means a cable.
        ac: mains.unwrap_or(battery.is_none() || status != "Discharging"),
        percent: battery.and_then(|b| b.capacity),
        battery: battery.is_some(),
        charging: status == "Charging",
        saver: false,
    })
}

fn read_power() -> Option<Power> {
    let supplies: Vec<Supply> = fs::read_dir("/sys/class/power_supply")
        .ok()?
        .flatten()
        .filter_map(|e| {
            let dir = e.path();
            let read = |f: &str| fs::read_to_string(dir.join(f)).ok().map(|s| s.trim().to_string());
            // A device battery (a mouse) is not the machine's.
            if read("scope").as_deref() == Some("Device") {
                return None;
            }
            Some(Supply { kind: read("type")?, status: read("status").unwrap_or_default(), capacity: read("capacity").and_then(|c| c.parse().ok()), online: read("online").map(|o| o == "1") })
        })
        .collect();
    summarize_power(&supplies)
}

/// Whether the internet answers: NetworkManager's own check, else any network interface that is up.
fn read_online() -> bool {
    match run("nmcli", &["-t", "-f", "CONNECTIVITY", "general"]) {
        Ok(out) => out.trim() == "full",
        Err(_) => fs::read_dir("/sys/class/net").into_iter().flatten().flatten().any(|e| e.file_name() != "lo" && fs::read_to_string(e.path().join("operstate")).is_ok_and(|s| s.trim() == "up")),
    }
}

/// Caps Lock and Num Lock, from the keyboard LEDs.
fn read_locks() -> (Option<bool>, Option<bool>) {
    let led = |suffix: &str| {
        let mut found = None;
        for e in fs::read_dir("/sys/class/leds").into_iter().flatten().flatten() {
            if e.file_name().to_string_lossy().ends_with(suffix) {
                let on = fs::read_to_string(e.path().join("brightness")).is_ok_and(|b| b.trim() != "0");
                found = Some(found.unwrap_or(false) || on);
            }
        }
        found
    };
    (led("::capslock"), led("::numlock"))
}

#[derive(Clone, PartialEq, Debug)]
struct Device {
    name: String,
    connected: bool,
    icon: String,
    battery: Option<u8>,
}

/// The Bluetooth devices BlueZ knows, by object path.
fn read_devices() -> Option<HashMap<String, Device>> {
    let conn = Connection::system().ok()?;
    let manager = Proxy::new(&conn, "org.bluez", "/", "org.freedesktop.DBus.ObjectManager").ok()?;
    let objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> = manager.call_method("GetManagedObjects", &()).ok()?.body().deserialize().ok()?;
    Some(
        objects
            .into_iter()
            .filter_map(|(path, interfaces)| {
                let device = interfaces.get("org.bluez.Device1")?;
                let battery = interfaces.get("org.bluez.Battery1").and_then(|b| b.get("Percentage")).and_then(number).map(|p| p as u8);
                Some((
                    path.to_string(),
                    Device {
                        name: device.get("Alias").or_else(|| device.get("Name")).and_then(text).unwrap_or_default(),
                        connected: device.get("Connected").and_then(|v| bool::try_from(v).ok()).unwrap_or(false),
                        icon: device.get("Icon").and_then(text).unwrap_or_default(),
                        battery,
                    },
                ))
            })
            .collect(),
    )
}

#[derive(Serialize)]
pub struct Seed {
    power: Option<Power>,
    online: Option<bool>,
    devices: Vec<String>,
}

/// The state at start, so the first change is told against something. None = the source is not there.
#[tauri::command(async)]
pub fn status_seed() -> Seed {
    let devices = read_devices().map(|d| d.into_values().filter(|d| d.connected).map(|d| d.name).collect()).unwrap_or_default();
    Seed { power: read_power(), online: Some(read_online()), devices }
}

static LOCKS: AtomicBool = AtomicBool::new(false);
static BLUETOOTH: AtomicBool = AtomicBool::new(false);
static VOLUME: AtomicBool = AtomicBool::new(false);
static BRIGHTNESS: AtomicBool = AtomicBool::new(false);
static STARTED: Once = Once::new();

/// What the page wants watched: `ddc` is its brightness setting, `locks` the Caps and Num Lock notices (off in a full-screen app),
/// `bt` the Bluetooth ones, `volume` the volume ones. The rest of the arguments are Windows' (hiding its own flyout).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn status_configure(app: AppHandle, ddc: bool, locks: bool, bt: bool, volume: bool, hide_osd: bool, osd_armed: bool) {
    let _ = (hide_osd, osd_armed);
    BRIGHTNESS.store(ddc, Ordering::Relaxed);
    LOCKS.store(locks, Ordering::Relaxed);
    BLUETOOTH.store(bt, Ordering::Relaxed);
    VOLUME.store(volume, Ordering::Relaxed);
    STARTED.call_once(|| {
        thread::spawn(move || watch(app));
    });
}

/// A source's last reading, kept to tell a change from a repeat. The first one is only remembered.
struct Last<T>(Mutex<Option<T>>);

impl<T: PartialEq + Clone> Last<T> {
    const fn new() -> Self {
        Last(Mutex::new(None))
    }

    /// The new value when it differs from the last one and there was a last one.
    fn changed(&self, now: T) -> Option<T> {
        let mut last = self.0.lock().unwrap();
        let news = last.as_ref().is_some_and(|l| *l != now).then(|| now.clone());
        *last = Some(now);
        news
    }
}

fn watch(app: AppHandle) {
    let power: Last<Power> = Last::new();
    let online: Last<bool> = Last::new();
    let caps: Last<bool> = Last::new();
    let num: Last<bool> = Last::new();
    let volume: Last<(u8, bool)> = Last::new();
    let brightness: Last<u8> = Last::new();
    let mut devices: Option<HashMap<String, Device>> = None;
    let due = |at: &mut Instant, every: Duration| {
        let now = Instant::now();
        (now >= *at).then(|| *at = now + every).is_some()
    };
    let start = Instant::now();
    let (mut t_power, mut t_net, mut t_bt, mut t_vol, mut t_bright) = (start, start, start, start, start);
    loop {
        thread::sleep(Duration::from_millis(150));
        if crate::standby::asleep() {
            continue;
        }
        if LOCKS.load(Ordering::Relaxed) {
            let (c, n) = read_locks();
            if let Some(on) = c.and_then(|c| caps.changed(c)) {
                let _ = app.emit("status://lock", serde_json::json!({ "key": "caps", "on": on }));
            }
            if let Some(on) = n.and_then(|n| num.changed(n)) {
                let _ = app.emit("status://lock", serde_json::json!({ "key": "num", "on": on }));
            }
        }
        if due(&mut t_power, Duration::from_secs(3)) {
            if let Some(p) = read_power().and_then(|p| power.changed(p)) {
                let _ = app.emit("status://power", p);
            }
        }
        if due(&mut t_net, Duration::from_secs(5)) {
            if let Some(o) = online.changed(read_online()) {
                let _ = app.emit("status://net", serde_json::json!({ "online": o }));
            }
        }
        if BLUETOOTH.load(Ordering::Relaxed) && due(&mut t_bt, Duration::from_secs(4)) {
            if let Some(now) = read_devices() {
                if let Some(before) = &devices {
                    for (path, d) in &now {
                        if d.name.is_empty() || before.get(path).is_some_and(|b| b.connected == d.connected) {
                            continue;
                        }
                        // A device the manager only just learnt of is news only when it is connected.
                        if before.get(path).is_none() && !d.connected {
                            continue;
                        }
                        let _ = app.emit("status://bt", serde_json::json!({ "name": d.name, "connected": d.connected, "battery": d.battery, "categories": [d.icon] }));
                    }
                }
                devices = Some(now);
            }
        }
        if VOLUME.load(Ordering::Relaxed) && due(&mut t_vol, Duration::from_millis(450)) {
            if let Some((level, muted)) = controls::volume_percent() {
                let news = volume.changed((level, muted));
                // Crest's own change (its slider) is not announced.
                if let (Some((level, muted)), false) = (news, controls::own_change_recent()) {
                    let _ = app.emit("status://level", serde_json::json!({ "kind": "volume", "level": level, "muted": muted }));
                }
            }
        }
        if BRIGHTNESS.load(Ordering::Relaxed) && due(&mut t_bright, Duration::from_secs(1)) {
            if let Some(level) = controls::brightness_get().and_then(|b| brightness.changed(b)) {
                if !controls::own_change_recent() {
                    let _ = app.emit("status://level", serde_json::json!({ "kind": "brightness", "level": level, "muted": false }));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(class: &str, state: &str, extra: Value) -> Value {
        let mut props = serde_json::json!({ "media.class": class });
        for (k, v) in extra.as_object().unwrap() {
            props[k] = v.clone();
        }
        serde_json::json!({ "id": 1, "type": "PipeWire:Interface:Node", "info": { "state": state, "props": props } })
    }

    fn dump(nodes: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&nodes).unwrap()
    }

    #[test]
    fn nothing_running_shows_no_dots() {
        let d = dump(vec![node("Video/Source", "suspended", serde_json::json!({})), node("Audio/Source", "idle", serde_json::json!({}))]);
        assert_eq!(parse_privacy(&d, 1), Privacy::default());
        assert_eq!(parse_privacy(b"not json", 1), Privacy::default());
    }

    #[test]
    fn a_running_microphone_or_camera_lights_its_dot() {
        let d = dump(vec![node("Stream/Input/Audio", "running", serde_json::json!({ "application.process.id": 77 })), node("Video/Source", "running", serde_json::json!({}))]);
        assert_eq!(parse_privacy(&d, 1), Privacy { mic: true, camera: true, screen: false });
    }

    #[test]
    fn crests_own_capture_and_monitors_are_not_a_microphone() {
        let own = dump(vec![node("Stream/Input/Audio", "running", serde_json::json!({ "application.process.id": 5 }))]);
        assert!(!parse_privacy(&own, 5).mic);
        // The equalizer reads the output's monitor, as a child process.
        let eq = dump(vec![node("Stream/Input/Audio", "running", serde_json::json!({ "stream.capture.sink": "true" }))]);
        assert!(!parse_privacy(&eq, 1).mic);
        let monitor = dump(vec![node("Stream/Input/Audio", "running", serde_json::json!({ "stream.monitor": true }))]);
        assert!(!parse_privacy(&monitor, 1).mic);
    }

    #[test]
    fn a_video_stream_is_a_camera_or_a_screen_share_by_its_role() {
        let cam = dump(vec![node("Stream/Input/Video", "running", serde_json::json!({ "media.role": "Camera" }))]);
        assert_eq!(parse_privacy(&cam, 1), Privacy { mic: false, camera: true, screen: false });
        let share = dump(vec![node("Stream/Input/Video", "running", serde_json::json!({ "media.role": "Screen" }))]);
        assert_eq!(parse_privacy(&share, 1), Privacy { mic: false, camera: false, screen: true });
    }

    fn supply(kind: &str, status: &str, capacity: Option<u8>, online: Option<bool>) -> Supply {
        Supply { kind: kind.into(), status: status.into(), capacity, online }
    }

    #[test]
    fn power_is_summarized_from_the_supplies() {
        let charging = summarize_power(&[supply("Mains", "", None, Some(true)), supply("Battery", "Charging", Some(64), None)]).unwrap();
        assert_eq!((charging.ac, charging.charging, charging.percent, charging.battery), (true, true, Some(64), true));
        let on_battery = summarize_power(&[supply("Mains", "", None, Some(false)), supply("Battery", "Discharging", Some(40), None)]).unwrap();
        assert_eq!((on_battery.ac, on_battery.charging), (false, false));
        // No charger entry: discharging means no cable, anything else does.
        assert!(!summarize_power(&[supply("Battery", "Discharging", Some(9), None)]).unwrap().ac);
        assert!(summarize_power(&[supply("Battery", "Full", Some(100), None)]).unwrap().ac);
        // A desktop: mains only, no battery.
        let desktop = summarize_power(&[supply("Mains", "", None, Some(true))]).unwrap();
        assert_eq!((desktop.battery, desktop.percent, desktop.ac), (false, None, true));
        assert!(summarize_power(&[]).is_none());
    }

    /// Reads this machine's real PipeWire and /proc: with nothing capturing, every dot must be off.
    /// `cargo test -- --ignored --nocapture linux::status`
    #[test]
    #[ignore = "reads the live desktop"]
    fn live_privacy_scan_shows_nothing_when_nothing_captures() {
        let p = privacy_state();
        println!("privacy now: {p:?}  | seed: power={:?} online={:?}", status_seed().power, status_seed().online);
        assert_eq!(p, Privacy::default(), "something is really capturing, or a false positive");
    }

    #[test]
    fn a_change_is_reported_only_after_a_first_reading() {
        let last: Last<u8> = Last::new();
        assert_eq!(last.changed(5), None, "the first reading is the baseline");
        assert_eq!(last.changed(5), None, "a repeat is not news");
        assert_eq!(last.changed(6), Some(6));
        assert_eq!(last.changed(5), Some(5));
    }
}
