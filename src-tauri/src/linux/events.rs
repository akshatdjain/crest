//! What wakes Crest on Linux when something changes, so nothing is asked again on a timer.
//!
//! The status alerts (linux/status.rs) and the privacy dots used to start programs and read files every few hundred milliseconds. These
//! sources only speak when there is news: the services' own D-Bus signals (NetworkManager, BlueZ, UPower, logind, Mutter), inotify on
//! the camera devices, and the PipeWire monitor of linux/pipewire.rs. Each one sends an `Event` to the one thread that handles them.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Mutex, Once, OnceLock};
use std::thread;
use std::time::Duration;

use inotify::{Inotify, WatchMask};
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type as MessageType;
use zbus::zvariant::OwnedValue;
use zbus::MatchRule;

/// What changed, as far as the status thread is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Event {
    Power,
    Net,
    Bluetooth,
    /// A sink or the default sink changed: the volume may have.
    Sink,
    /// The page changed what it wants watched.
    Configure,
}

static TX: OnceLock<Mutex<Sender<Event>>> = OnceLock::new();

/// Makes the channel the status thread waits on. Called once; later calls get a receiver that never hears anything.
pub fn init() -> Receiver<Event> {
    let (tx, rx) = channel();
    let _ = TX.set(Mutex::new(tx));
    rx
}

pub fn send(event: Event) {
    if let Some(tx) = TX.get() {
        let _ = tx.lock().map(|tx| tx.send(event));
    }
}

/// How long a listener waits before it tries again after its bus connection ended (a bus that restarted).
const RELISTEN: Duration = Duration::from_secs(30);

/// Calls `on_message` for each message that matches `rule` on the system or the session bus, for as long as the connection lasts.
fn listen(system: bool, rule: &MatchRule<'static>, on_message: &mut impl FnMut(&zbus::Message)) -> Result<(), String> {
    let conn = if system { Connection::system() } else { Connection::session() }.map_err(|e| e.to_string())?;
    for message in MessageIterator::for_match_rule(rule.clone(), &conn, None).map_err(|e| e.to_string())?.flatten() {
        on_message(&message);
    }
    Ok(())
}

/// Runs `listen` on a thread of its own, and again after a pause if it ends.
fn spawn_listener(system: bool, rule: Result<MatchRule<'static>, zbus::Error>, mut on_message: impl FnMut(&zbus::Message) + Send + 'static) {
    let Ok(rule) = rule else { return };
    thread::spawn(move || loop {
        let _ = listen(system, &rule, &mut on_message);
        thread::sleep(RELISTEN);
    });
}

fn properties_changed(sender: &'static str, path_namespace: Option<&'static str>) -> Result<MatchRule<'static>, zbus::Error> {
    let b = MatchRule::builder().msg_type(MessageType::Signal).sender(sender)?.interface("org.freedesktop.DBus.Properties")?.member("PropertiesChanged")?;
    Ok(match path_namespace {
        Some(ns) => b.path_namespace(ns)?.build(),
        None => b.build(),
    })
}

/// The interface and the changed property names of a PropertiesChanged signal.
fn changed(message: &zbus::Message) -> Option<(String, Vec<String>)> {
    let (interface, props, _invalidated): (String, HashMap<String, OwnedValue>, Vec<String>) = message.body().deserialize().ok()?;
    Some((interface, props.into_keys().collect()))
}

static STARTED: Once = Once::new();

/// Starts the listeners, once. Safe to call from anywhere that needs them.
pub fn start() {
    STARTED.call_once(|| {
        // NetworkManager: the connectivity or the overall state changed.
        spawn_listener(true, properties_changed("org.freedesktop.NetworkManager", Some("/org/freedesktop/NetworkManager")), |m| {
            if changed(m).is_some_and(|(_, keys)| keys.iter().any(|k| k == "Connectivity" || k == "State")) {
                send(Event::Net);
            }
        });
        // BlueZ: a device connected or went, or its battery changed. The device list changing is its own signal.
        spawn_listener(true, properties_changed("org.bluez", Some("/org/bluez")), |m| {
            if changed(m).is_some_and(|(interface, keys)| (interface == "org.bluez.Device1" && keys.iter().any(|k| k == "Connected")) || (interface == "org.bluez.Battery1" && keys.iter().any(|k| k == "Percentage"))) {
                send(Event::Bluetooth);
            }
        });
        spawn_listener(
            true,
            MatchRule::builder().msg_type(MessageType::Signal).sender("org.bluez").and_then(|b| b.interface("org.freedesktop.DBus.ObjectManager")).map(|b| b.build()),
            |_| send(Event::Bluetooth),
        );
        // UPower: a charger plugged or unplugged, or the battery changed level.
        spawn_listener(true, properties_changed("org.freedesktop.UPower", None), |m| {
            if changed(m).is_some_and(|(_, keys)| keys.iter().any(|k| matches!(k.as_str(), "State" | "OnBattery" | "Percentage" | "IsPresent"))) {
                send(Event::Power);
            }
        });
        // The system suspends or resumes (logind), and the display goes off or comes back (Mutter): the pollers pause meanwhile.
        spawn_listener(
            true,
            MatchRule::builder().msg_type(MessageType::Signal).sender("org.freedesktop.login1").and_then(|b| b.interface("org.freedesktop.login1.Manager")).and_then(|b| b.member("PrepareForSleep")).map(|b| b.build()),
            |m| {
                if let Ok(suspending) = m.body().deserialize::<bool>() {
                    crate::standby::set_suspended(suspending);
                    send(Event::Configure);
                }
            },
        );
        spawn_listener(false, properties_changed("org.gnome.Mutter.DisplayConfig", None), |m| {
            if changed(m).is_some_and(|(_, keys)| keys.iter().any(|k| k == "PowerSaveMode")) {
                // 0 is on; anything else is a screen that is off or blanking.
                if let Ok((_, props, _)) = m.body().deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>() {
                    let mode = props.get("PowerSaveMode").and_then(|v| i32::try_from(v).ok()).unwrap_or(0);
                    crate::standby::set_display(mode == 0);
                    send(Event::Configure);
                }
            }
        });
    });
}

// ---- cameras --------------------------------------------------------------------------------------------------------

static CAMERA_OPEN: AtomicBool = AtomicBool::new(false);
static CAMERA_WATCH: Once = Once::new();

/// Whether an app that does not go through PipeWire has a camera device open. Read from memory.
pub fn camera_open() -> bool {
    CAMERA_WATCH.call_once(|| {
        thread::spawn(watch_cameras);
    });
    CAMERA_OPEN.load(Ordering::Relaxed)
}

fn video_devices() -> Vec<PathBuf> {
    fs::read_dir("/dev").into_iter().flatten().flatten().filter(|e| e.file_name().to_string_lossy().starts_with("video")).map(|e| e.path()).collect()
}

/// An app that opened a camera device itself, without PipeWire. Only processes of the user can be looked at, and the PipeWire daemons
/// are skipped: they hold the device while probing it.
fn video_device_held(own_pid: u32) -> bool {
    const SERVERS: [&str; 4] = ["pipewire", "wireplumber", "pipewire-pulse", "xdg-desktop-portal"];
    let Ok(procs) = fs::read_dir("/proc") else { return false };
    procs.flatten().any(|e| {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { return false };
        if pid == own_pid || fs::read_to_string(e.path().join("comm")).is_ok_and(|c| SERVERS.contains(&c.trim())) {
            return false;
        }
        fs::read_dir(e.path().join("fd")).is_ok_and(|fds| fds.flatten().any(|fd| fs::read_link(fd.path()).is_ok_and(|t| t.to_string_lossy().starts_with("/dev/video"))))
    })
}

/// Looks at the processes only when a camera device was opened or closed (inotify), never on a timer. A device that appears or goes
/// (a webcam plugged in) is seen through /dev itself, for creations and removals only.
fn watch_cameras() {
    let own = std::process::id();
    loop {
        CAMERA_OPEN.store(video_device_held(own), Ordering::Relaxed);
        let Ok(mut watcher) = Inotify::init() else { return };
        for device in video_devices() {
            let _ = watcher.watches().add(&device, WatchMask::OPEN | WatchMask::CLOSE_WRITE | WatchMask::CLOSE_NOWRITE);
        }
        let _ = watcher.watches().add("/dev", WatchMask::CREATE | WatchMask::DELETE);
        let mut buffer = [0u8; 4096];
        let failed = loop {
            let Ok(events) = watcher.read_events_blocking(&mut buffer) else { break true };
            let plugged = events.into_iter().any(|e| e.name.is_some_and(|n| n.to_string_lossy().starts_with("video")));
            // Opens and closes come in bursts, and the process can take a moment to show the descriptor.
            thread::sleep(Duration::from_millis(250));
            if plugged {
                // Watch again, with the devices there now.
                break false;
            }
            CAMERA_OPEN.store(video_device_held(own), Ordering::Relaxed);
        };
        if failed {
            thread::sleep(Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_sent_before_the_channel_exists_are_dropped_and_after_it_are_heard() {
        // TX is process-wide: this is the only test that sets it.
        send(Event::Net);
        let rx = init();
        assert!(rx.try_recv().is_err(), "nothing from before the channel was made");
        send(Event::Power);
        send(Event::Sink);
        assert_eq!((rx.try_recv(), rx.try_recv()), (Ok(Event::Power), Ok(Event::Sink)));
    }

    #[test]
    fn a_properties_changed_signal_is_read_into_its_interface_and_property_names() {
        let body = ("org.bluez.Device1".to_string(), HashMap::from([("Connected".to_string(), zbus::zvariant::Value::from(true))]), Vec::<String>::new());
        let message = zbus::Message::signal("/org/bluez/hci0/dev_AA", "org.freedesktop.DBus.Properties", "PropertiesChanged").unwrap().build(&body).unwrap();
        assert_eq!(changed(&message), Some(("org.bluez.Device1".to_string(), vec!["Connected".to_string()])));
        let other = zbus::Message::signal("/x", "org.example.Thing", "Done").unwrap().build(&()).unwrap();
        assert_eq!(changed(&other), None, "a signal with another body is not a property change");
    }

    /// A signal sent on the session bus reaches the listener, which is how every source above is fed.
    #[test]
    fn a_signal_on_the_bus_reaches_a_listener_and_other_signals_do_not() {
        let emitter = Connection::session().unwrap();
        let unique = zbus::names::UniqueName::try_from(emitter.unique_name().unwrap().to_string()).unwrap();
        let rule = MatchRule::builder().msg_type(MessageType::Signal).sender(unique).and_then(|b| b.interface("org.crest.Test")).and_then(|b| b.member("Ping")).map(|b| b.build()).unwrap();
        let (tx, rx) = channel();
        thread::spawn(move || {
            let _ = listen(false, &rule, &mut |_| drop(tx.send(())));
        });
        thread::sleep(Duration::from_millis(500));
        emitter.emit_signal(None::<&str>, "/org/crest/test", "org.crest.Test", "Other", &()).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(600)).is_err(), "a signal the rule does not name");
        emitter.emit_signal(None::<&str>, "/org/crest/test", "org.crest.Test", "Ping", &()).unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(3)).is_ok(), "the signal was not delivered");
    }

    #[test]
    fn a_camera_device_opened_and_closed_is_seen_by_inotify() {
        // A real inotify watch on a real file stands in for a device node: the same OPEN and CLOSE events.
        let dir = std::env::temp_dir().join(format!("crest-ino-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("video99");
        fs::write(&file, b"").unwrap();
        let mut watcher = Inotify::init().unwrap();
        watcher.watches().add(&file, WatchMask::OPEN | WatchMask::CLOSE_NOWRITE | WatchMask::CLOSE_WRITE).unwrap();
        drop(fs::File::open(&file).unwrap());
        let mut buffer = [0u8; 1024];
        let events: Vec<_> = watcher.read_events_blocking(&mut buffer).unwrap().map(|e| e.mask).collect();
        assert!(events.iter().any(|m| m.contains(inotify::EventMask::OPEN)), "{events:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
