//! What PipeWire is doing, kept up to date from one long-running `pw-dump --monitor` instead of asking again every second.
//!
//! `pw-dump --monitor` prints the whole graph once and then, each time an object changes, that object again (a removed one with a null
//! `info`). The nodes are kept in a small model, so the privacy dots read it without starting a program, and a change to a sink or the
//! default-sink setting wakes the status thread to read the volume once. The monitor runs only while something wants it: the page
//! polling the privacy dots, or the volume alerts, and is ended a while after the last want.

use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::sync::{Condvar, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use super::events::{self, Event};

/// The monitor is ended this long after it was last wanted.
const LINGER: Duration = Duration::from_secs(20);
/// A monitor that cannot start or keeps ending is tried again after this long, and then later.
const RETRY: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(120);

#[derive(Serialize, Debug, PartialEq, Default, Clone, Copy)]
pub struct Privacy {
    pub mic: bool,
    pub camera: bool,
    pub screen: bool,
}

/// The parts of a node that decide what it is and whether it is in use.
#[derive(Debug, Default, Clone)]
struct Node {
    class: String,
    state: String,
    pid: Option<u64>,
    capture_sink: bool,
    monitor: bool,
    role: String,
}

fn truthy(v: &Value) -> bool {
    v.as_bool().unwrap_or(false) || v.as_str() == Some("true")
}

/// What one array of the stream changed that the rest of Crest cares about.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    pub privacy: bool,
    /// A sink or the default-sink setting changed: the volume may have.
    pub sink: bool,
}

#[derive(Default)]
pub struct Model {
    nodes: HashMap<u64, Node>,
}

impl Model {
    /// Applies the objects of one array: the first holds everything, later ones the objects that changed.
    pub fn apply(&mut self, objects: &[Value], own_pid: u64) -> Changes {
        let before = self.privacy(own_pid);
        let mut changes = Changes::default();
        for o in objects {
            let Some(id) = o["id"].as_u64() else { continue };
            // A removed object has a null info and no type.
            if o["info"].is_null() {
                if let Some(gone) = self.nodes.remove(&id) {
                    changes.sink |= gone.class == "Audio/Sink";
                }
                continue;
            }
            let kind = o["type"].as_str().unwrap_or("");
            if kind == "PipeWire:Interface:Metadata" {
                changes.sink = true;
                continue;
            }
            if kind != "PipeWire:Interface:Node" {
                continue;
            }
            let info = &o["info"];
            let node = self.nodes.entry(id).or_default();
            if let Some(state) = info["state"].as_str() {
                node.state = state.to_string();
            }
            let props = &info["props"];
            if let Some(class) = props["media.class"].as_str() {
                node.class = class.to_string();
            }
            if let Some(pid) = props["application.process.id"].as_u64() {
                node.pid = Some(pid);
            }
            if !props["stream.capture.sink"].is_null() {
                node.capture_sink = truthy(&props["stream.capture.sink"]);
            }
            if !props["stream.monitor"].is_null() {
                node.monitor = truthy(&props["stream.monitor"]);
            }
            if let Some(role) = props["media.role"].as_str() {
                node.role = role.to_string();
            }
            changes.sink |= node.class == "Audio/Sink";
        }
        changes.privacy = self.privacy(own_pid) != before;
        changes
    }

    /// What is capturing: a running capture stream or source is a microphone; a running video source, or a video stream with the
    /// camera role, is the camera; any other running video stream is a screen share. Crest's own capture is skipped by process id
    /// and by its `stream.capture.sink` property (the equalizer reads the output's monitor), as are monitor streams.
    pub fn privacy(&self, own_pid: u64) -> Privacy {
        let mut p = Privacy::default();
        for n in self.nodes.values().filter(|n| n.state == "running" && n.pid != Some(own_pid)) {
            match n.class.as_str() {
                "Stream/Input/Audio" if !n.capture_sink && !n.monitor => p.mic = true,
                "Audio/Source" => p.mic = true,
                "Video/Source" => p.camera = true,
                "Stream/Input/Video" if n.role == "Camera" => p.camera = true,
                "Stream/Input/Video" => p.screen = true,
                _ => {}
            }
        }
        p
    }
}

/// The privacy of one whole `pw-dump` array, as a one-off reading.
#[cfg(test)]
fn parse_privacy(json: &[u8], own_pid: u64) -> Privacy {
    let Ok(Value::Array(objects)) = serde_json::from_slice::<Value>(json) else { return Privacy::default() };
    let mut model = Model::default();
    model.apply(&objects, own_pid);
    model.privacy(own_pid)
}

// ---- the monitor ----------------------------------------------------------------------------------------------------

struct Shared {
    privacy: Privacy,
    /// The last time something asked for the monitor.
    wanted: Option<Instant>,
    running: bool,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared { privacy: Privacy { mic: false, camera: false, screen: false }, wanted: None, running: false });
static WAKE: Condvar = Condvar::new();
static STARTED: Once = Once::new();

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Says the monitor is needed (again), and starts it if it is not running.
pub fn want() {
    shared().wanted = Some(Instant::now());
    STARTED.call_once(|| {
        thread::spawn(run);
    });
    WAKE.notify_all();
}

/// What the monitor knows now. It is read from memory: nothing is started for it.
pub fn privacy() -> Privacy {
    want();
    shared().privacy
}

/// Whether the monitor process is running now.
#[cfg(test)]
fn running() -> bool {
    shared().running
}

fn still_wanted() -> bool {
    shared().wanted.is_some_and(|at| at.elapsed() < LINGER)
}

/// The thread of the monitor: waits to be wanted, runs `pw-dump --monitor` while it is, ends it when it is not.
fn run() {
    let mut retry = RETRY;
    loop {
        // Wait until wanted.
        let mut guard = shared();
        while !guard.wanted.is_some_and(|at| at.elapsed() < LINGER) {
            guard = WAKE.wait_timeout(guard, LINGER).unwrap_or_else(|e| e.into_inner()).0;
        }
        drop(guard);
        let started = Instant::now();
        let ok = follow();
        shared().running = false;
        // One that ran for a while was fine; one that fails at once is tried again later and later.
        if ok && started.elapsed() > RETRY {
            retry = RETRY;
        } else {
            thread::sleep(retry);
            retry = (retry * 2).min(RETRY_MAX);
        }
    }
}

/// Runs one `pw-dump --monitor` until it ends or is no longer wanted. False if it could not be started.
fn follow() -> bool {
    let Ok(mut child) = Command::new("pw-dump").arg("--monitor").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return false };
    let Some(stdout) = child.stdout.take() else { return false };
    shared().running = true;
    let pid = child.id();
    // A watchdog ends the process when nothing wants it any more: the reader below only wakes when the graph changes.
    let watchdog = thread::spawn(move || {
        while still_wanted() {
            thread::sleep(Duration::from_secs(5));
            if !shared().running {
                return;
            }
        }
        let _ = Command::new("kill").arg(pid.to_string()).status();
    });
    let own = u64::from(std::process::id());
    let mut model = Model::default();
    for array in serde_json::Deserializer::from_reader(BufReader::new(stdout)).into_iter::<Value>().flatten() {
        let Some(objects) = array.as_array() else { continue };
        let changes = model.apply(objects, own);
        shared().privacy = model.privacy(own);
        if changes.sink {
            events::send(Event::Sink);
        }
    }
    let _ = child.wait();
    shared().running = false;
    shared().privacy = Privacy::default();
    let _ = watchdog.join();
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u64, class: &str, state: &str, extra: Value) -> Value {
        let mut props = serde_json::json!({ "media.class": class });
        for (k, v) in extra.as_object().unwrap() {
            props[k] = v.clone();
        }
        serde_json::json!({ "id": id, "type": "PipeWire:Interface:Node", "info": { "state": state, "props": props } })
    }

    fn dump(nodes: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&nodes).unwrap()
    }

    #[test]
    fn nothing_running_shows_no_dots() {
        let d = dump(vec![node(1, "Video/Source", "suspended", serde_json::json!({})), node(2, "Audio/Source", "idle", serde_json::json!({}))]);
        assert_eq!(parse_privacy(&d, 1), Privacy::default());
        assert_eq!(parse_privacy(b"not json", 1), Privacy::default());
    }

    #[test]
    fn a_running_microphone_or_camera_lights_its_dot() {
        let d = dump(vec![node(1, "Stream/Input/Audio", "running", serde_json::json!({ "application.process.id": 77 })), node(2, "Video/Source", "running", serde_json::json!({}))]);
        assert_eq!(parse_privacy(&d, 1), Privacy { mic: true, camera: true, screen: false });
    }

    #[test]
    fn crests_own_capture_and_monitors_are_not_a_microphone() {
        let own = dump(vec![node(1, "Stream/Input/Audio", "running", serde_json::json!({ "application.process.id": 5 }))]);
        assert!(!parse_privacy(&own, 5).mic);
        let eq = dump(vec![node(1, "Stream/Input/Audio", "running", serde_json::json!({ "stream.capture.sink": "true" }))]);
        assert!(!parse_privacy(&eq, 1).mic);
        let monitor = dump(vec![node(1, "Stream/Input/Audio", "running", serde_json::json!({ "stream.monitor": true }))]);
        assert!(!parse_privacy(&monitor, 1).mic);
    }

    #[test]
    fn a_video_stream_is_a_camera_or_a_screen_share_by_its_role() {
        let cam = dump(vec![node(1, "Stream/Input/Video", "running", serde_json::json!({ "media.role": "Camera" }))]);
        assert_eq!(parse_privacy(&cam, 1), Privacy { mic: false, camera: true, screen: false });
        let share = dump(vec![node(1, "Stream/Input/Video", "running", serde_json::json!({ "media.role": "Screen" }))]);
        assert_eq!(parse_privacy(&share, 1), Privacy { mic: false, camera: false, screen: true });
    }

    #[test]
    fn later_arrays_update_and_remove_what_the_first_one_held() {
        let mut model = Model::default();
        // The first array holds everything: an idle microphone stream, a sink.
        let first = [node(10, "Stream/Input/Audio", "idle", serde_json::json!({ "application.process.id": 77 })), node(20, "Audio/Sink", "suspended", serde_json::json!({}))];
        let c = model.apply(&first, 1);
        assert_eq!((c.privacy, c.sink), (false, true), "a sink is there: the volume may be read");
        assert!(!model.privacy(1).mic);
        // The stream starts: an update that carries only its new state, with no props at all.
        let c = model.apply(&[serde_json::json!({ "id": 10, "type": "PipeWire:Interface:Node", "info": { "state": "running" } })], 1);
        assert_eq!((c.privacy, c.sink), (true, false));
        assert!(model.privacy(1).mic, "the class and the process were remembered from the first array");
        // It is removed.
        let c = model.apply(&[serde_json::json!({ "id": 10, "info": null })], 1);
        assert!(c.privacy && !model.privacy(1).mic);
        // The default sink changes: a metadata object.
        let c = model.apply(&[serde_json::json!({ "id": 3, "type": "PipeWire:Interface:Metadata", "info": { "props": {} } })], 1);
        assert!(c.sink && !c.privacy);
        // A client or a device is not a node and changes nothing.
        let c = model.apply(&[serde_json::json!({ "id": 88, "type": "PipeWire:Interface:Client", "info": { "props": {} } })], 1);
        assert_eq!(c, Changes::default());
    }

    /// Starts the real monitor, waits for the first array, and checks it loaded the graph and that nothing is capturing now. The
    /// process is ended when the test ends. `cargo test -- --ignored --nocapture linux::pipewire`
    #[test]
    #[ignore = "runs pw-dump --monitor on the live desktop"]
    fn the_live_monitor_loads_the_graph_and_shows_nothing_capturing() {
        assert_eq!(privacy(), Privacy::default(), "before the first array there is nothing to report");
        thread::sleep(Duration::from_millis(2500));
        assert!(running(), "pw-dump --monitor is not running");
        let p = privacy();
        println!("privacy after the graph loaded: {p:?}");
        assert_eq!(p, Privacy::default(), "something is really capturing, or a false positive");
    }

    /// Records from the microphone for a moment and checks that the dot lights through the monitor's updates and goes off again when the
    /// recording ends. GNOME shows its own microphone indicator meanwhile. `cargo test -- --ignored --nocapture linux::pipewire`
    #[test]
    #[ignore = "records from the microphone for a few seconds"]
    fn a_real_recording_lights_the_microphone_dot_and_ending_it_puts_it_out() {
        let wait_for = |mic: bool| (0..60).any(|_| {
            thread::sleep(Duration::from_millis(100));
            privacy().mic == mic
        });
        privacy();
        thread::sleep(Duration::from_millis(2500));
        assert!(!privacy().mic, "a microphone is already in use");
        let file = std::env::temp_dir().join(format!("crest-mic-{}.wav", std::process::id()));
        let mut recorder = Command::new("pw-record").arg("--rate").arg("8000").arg(&file).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().expect("pw-record");
        let lit = wait_for(true);
        let _ = Command::new("kill").arg("-INT").arg(recorder.id().to_string()).status();
        let _ = recorder.wait();
        let out = wait_for(false);
        let _ = std::fs::remove_file(&file);
        println!("lit while recording: {lit}; out after it ended: {out}");
        assert!(lit, "the dot did not light while a recording ran");
        assert!(out, "the dot did not go out when the recording ended");
    }

    #[test]
    fn an_update_that_changes_nothing_reports_nothing() {
        let mut model = Model::default();
        model.apply(&[node(10, "Stream/Input/Audio", "running", serde_json::json!({}))], 1);
        let c = model.apply(&[node(10, "Stream/Input/Audio", "running", serde_json::json!({}))], 1);
        assert!(!c.privacy);
    }
}
