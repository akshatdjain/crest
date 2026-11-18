//! Privacy dots and the status notices on Linux (the Windows ones are privacy.rs and sysevents.rs).
//!
//! - Privacy: what captures the microphone, the camera or the screen comes from PipeWire, kept up to date by one `pw-dump --monitor`
//!   (linux/pipewire.rs), and from inotify on the camera devices for apps that open them directly (linux/events.rs). The command
//!   only reads what they know, so the page polling it starts nothing.
//! - Status: charger, battery, internet, Bluetooth, Caps/Num Lock and volume or brightness changes become the same `status://...`
//!   events the page already shows as notices. One thread waits for what the services report (D-Bus signals, PipeWire's monitor)
//!   and wakes on a timer only for what has no signal: the lock-key LEDs and the brightness, read as files, and only while their
//!   alerts are on. The first reading of each source is silent: it is the baseline, not news.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use zbus::blocking::fdo::DBusProxy;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use super::controls;
use super::events::{self, Event};
use super::media::{number, text};
use super::pipewire::{self, Privacy};

// ---- privacy --------------------------------------------------------------------------------------------------------

/// What is capturing now. Read from memory: PipeWire's monitor and the camera watch keep it, nothing is started for this.
#[tauri::command]
pub fn privacy_state() -> Privacy {
    let mut p = pipewire::privacy();
    p.camera = p.camera || events::camera_open();
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

/// NetworkManager's connectivity check (4 is full internet), asked over D-Bus.
fn network_manager_connectivity() -> Option<u32> {
    let conn = Connection::system().ok()?;
    Proxy::new(&conn, "org.freedesktop.NetworkManager", "/org/freedesktop/NetworkManager", "org.freedesktop.NetworkManager").ok()?.get_property::<u32>("Connectivity").ok()
}

/// Whether the internet answers: NetworkManager's own check, else any network interface that is up.
fn read_online() -> bool {
    match network_manager_connectivity() {
        Some(connectivity) => connectivity == 4,
        None => fs::read_dir("/sys/class/net").into_iter().flatten().flatten().any(|e| e.file_name() != "lo" && fs::read_to_string(e.path().join("operstate")).is_ok_and(|s| s.trim() == "up")),
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
        let receiver = events::init();
        events::start();
        thread::spawn(move || watch(app, receiver));
    });
    // The volume alerts need PipeWire's monitor, which is only run while something wants it.
    if volume {
        pipewire::want();
    }
    // The thread may be waiting for news with nothing to time: tell it what is wanted now.
    events::send(Event::Configure);
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

/// How often the lock-key LEDs and the backlight are read while their alerts are on: neither has a signal.
const LOCKS_EVERY: Duration = Duration::from_millis(250);
const BRIGHTNESS_EVERY: Duration = Duration::from_secs(1);
/// Without UPower or NetworkManager (a bare system) the charger and the network are read as files this often.
const FALLBACK_EVERY: Duration = Duration::from_secs(10);

fn name_has_owner(name: &'static str) -> bool {
    let Ok(conn) = Connection::system() else { return false };
    DBusProxy::new(&conn).ok().and_then(|p| p.name_has_owner(name.try_into().ok()?).ok()).unwrap_or(false)
}

/// The thread that tells the page. It blocks until a service reports something or a timer that is needed falls due, and does nothing
/// at all while the screen is off or the system is suspended: the events of that time wait until it is back.
fn watch(app: AppHandle, events: Receiver<Event>) {
    let power: Last<Power> = Last::new();
    let online: Last<bool> = Last::new();
    let caps: Last<bool> = Last::new();
    let num: Last<bool> = Last::new();
    let volume: Last<(u8, bool)> = Last::new();
    let brightness: Last<u8> = Last::new();
    let mut devices: Option<HashMap<String, Device>> = None;
    let (has_upower, has_network_manager) = (name_has_owner("org.freedesktop.UPower"), name_has_owner("org.freedesktop.NetworkManager"));
    let mut pending: HashSet<Event> = HashSet::new();
    // The first readings, which are baselines.
    pending.extend([Event::Power, Event::Net]);
    let start = Instant::now();
    let (mut t_locks, mut t_bright, mut t_power, mut t_net) = (start, start, start + FALLBACK_EVERY, start + FALLBACK_EVERY);

    loop {
        let asleep = crate::standby::asleep();
        // The nearest timer that is needed; none while asleep.
        let timers = [(LOCKS.load(Ordering::Relaxed), t_locks), (BRIGHTNESS.load(Ordering::Relaxed), t_bright), (!has_upower, t_power), (!has_network_manager, t_net)];
        let deadline = (!asleep).then(|| timers.iter().filter(|(needed, _)| *needed).map(|(_, at)| *at).min()).flatten();
        let heard = match deadline {
            Some(at) => match events.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(e) => Some(e),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match events.recv() {
                Ok(e) => Some(e),
                Err(_) => return,
            },
        };
        pending.extend(heard);
        // Drain what else is already waiting, so a burst is one pass.
        pending.extend(events.try_iter());
        if crate::standby::asleep() {
            continue;
        }
        let now = Instant::now();

        if LOCKS.load(Ordering::Relaxed) && now >= t_locks {
            t_locks = now + LOCKS_EVERY;
            let (c, n) = read_locks();
            if let Some(on) = c.and_then(|c| caps.changed(c)) {
                let _ = app.emit("status://lock", serde_json::json!({ "key": "caps", "on": on }));
            }
            if let Some(on) = n.and_then(|n| num.changed(n)) {
                let _ = app.emit("status://lock", serde_json::json!({ "key": "num", "on": on }));
            }
        }
        if !has_upower && now >= t_power {
            t_power = now + FALLBACK_EVERY;
            pending.insert(Event::Power);
        }
        if !has_network_manager && now >= t_net {
            t_net = now + FALLBACK_EVERY;
            pending.insert(Event::Net);
        }
        if BRIGHTNESS.load(Ordering::Relaxed) && now >= t_bright {
            t_bright = now + BRIGHTNESS_EVERY;
            if let Some(level) = controls::brightness_get().and_then(|b| brightness.changed(b)) {
                if !controls::own_change_recent() {
                    let _ = app.emit("status://level", serde_json::json!({ "kind": "brightness", "level": level, "muted": false }));
                }
            }
        }

        for event in std::mem::take(&mut pending) {
            match event {
                Event::Power => {
                    if let Some(p) = read_power().and_then(|p| power.changed(p)) {
                        let _ = app.emit("status://power", p);
                    }
                }
                Event::Net => {
                    if let Some(o) = online.changed(read_online()) {
                        let _ = app.emit("status://net", serde_json::json!({ "online": o }));
                    }
                }
                Event::Bluetooth if BLUETOOTH.load(Ordering::Relaxed) => {
                    let Some(now) = read_devices() else { continue };
                    // The first list is the baseline, and a device the manager only just learnt of is news only when it is connected.
                    if let Some(before) = &devices {
                        for (path, d) in &now {
                            if d.name.is_empty() || before.get(path).is_some_and(|b| b.connected == d.connected) || (before.get(path).is_none() && !d.connected) {
                                continue;
                            }
                            let _ = app.emit("status://bt", serde_json::json!({ "name": d.name, "connected": d.connected, "battery": d.battery, "categories": [d.icon] }));
                        }
                    }
                    devices = Some(now);
                }
                Event::Sink if VOLUME.load(Ordering::Relaxed) => {
                    // One read, for a sink that changed: not one every few hundred milliseconds.
                    if let Some((level, muted)) = controls::volume_percent().and_then(|v| volume.changed(v)) {
                        // Crest's own change (its slider) is not announced.
                        if !controls::own_change_recent() {
                            let _ = app.emit("status://level", serde_json::json!({ "kind": "volume", "level": level, "muted": muted }));
                        }
                    }
                }
                _ => {}
            }
        }
        // Alerts turned off forget their baseline, so turning them on again does not announce what changed in between.
        if !BLUETOOTH.load(Ordering::Relaxed) {
            devices = None;
        } else if devices.is_none() {
            devices = read_devices();
        }
        if !VOLUME.load(Ordering::Relaxed) {
            *volume.0.lock().unwrap() = None;
        } else if volume.0.lock().unwrap().is_none() {
            let _ = volume.changed(controls::volume_percent().unwrap_or((0, false)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
