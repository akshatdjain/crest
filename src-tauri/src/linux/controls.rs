//! The Overview tool's volume, output list, brightness, keep-awake and focus tiles on Linux (the Windows ones are controls.rs and
//! brightness.rs). Everything goes through what a desktop already offers, so nothing needs installing beyond PipeWire:
//! `wpctl` and `pw-dump` for the audio, logind for the brightness (writing the backlight needs no root through it), and the
//! screensaver inhibit interface for keep-awake.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use zbus::blocking::{Connection, Proxy};

/// Runs a program and returns what it printed; a program that is missing or fails is an error.
pub(super) fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program).args(args).output().map_err(|_| format!("{program} is not installed"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("{program} {} failed", args.join(" ")))
    }
}

// ---- volume ---------------------------------------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Volume {
    /// 0..1
    level: f32,
    muted: bool,
}

/// `Volume: 0.46` or `Volume: 0.46 [MUTED]`, which is what `wpctl get-volume` prints.
fn parse_volume(out: &str) -> Option<Volume> {
    let level = out.trim().strip_prefix("Volume:")?.split_whitespace().next()?.parse::<f32>().ok()?;
    Some(Volume { level: level.clamp(0.0, 1.0), muted: out.contains("[MUTED]") })
}

fn read_volume() -> Result<Volume, String> {
    parse_volume(&run("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"])?).ok_or_else(|| "unreadable".to_string())
}

#[tauri::command]
pub async fn volume_get() -> Result<Volume, String> {
    tauri::async_runtime::spawn_blocking(read_volume).await.map_err(|e| e.to_string())?
}

/// Sets the level (0..1) and/or the mute. Raising the level of a muted output unmutes it, as on Windows.
#[tauri::command]
pub async fn volume_set(level: Option<f32>, muted: Option<bool>) -> Result<Volume, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if let Some(l) = level {
            run("wpctl", &["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{:.3}", l.clamp(0.0, 1.0))])?;
            if l > 0.0 && muted.is_none() {
                run("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "0"])?;
            }
        }
        if let Some(m) = muted {
            run("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", if m { "1" } else { "0" }])?;
        }
        read_volume()
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---- outputs --------------------------------------------------------------------------------------------------------

#[derive(Serialize, Debug, PartialEq)]
pub struct Output {
    /// The sink's name, which is stable across restarts (the numeric id is not).
    id: String,
    name: String,
    default: bool,
}

/// A sink of `pw-dump` with the numeric object id `wpctl` wants.
#[derive(Debug, PartialEq)]
struct Sink {
    object: u64,
    output: Output,
}

fn parse_sinks(json: &[u8]) -> Vec<Sink> {
    let Ok(Value::Array(objects)) = serde_json::from_slice::<Value>(json) else { return Vec::new() };
    // The default is a key of the "default" metadata object, as `{"name": "<sink name>"}` (or that as a string).
    let default = objects
        .iter()
        .filter(|o| o["type"] == "PipeWire:Interface:Metadata")
        .flat_map(|o| o["metadata"].as_array().into_iter().flatten())
        .find(|m| m["key"] == "default.audio.sink")
        .and_then(|m| match &m["value"] {
            Value::Object(v) => v.get("name").and_then(Value::as_str).map(str::to_string),
            Value::String(s) => serde_json::from_str::<Value>(s).ok().and_then(|v| v["name"].as_str().map(str::to_string)),
            _ => None,
        });
    objects
        .iter()
        .filter(|o| o["type"] == "PipeWire:Interface:Node" && o["info"]["props"]["media.class"] == "Audio/Sink")
        .filter_map(|o| {
            let props = &o["info"]["props"];
            let id = props["node.name"].as_str()?.to_string();
            let name = props["node.description"].as_str().or_else(|| props["node.nick"].as_str()).unwrap_or(&id).to_string();
            Some(Sink { object: o["id"].as_u64()?, output: Output { default: default.as_deref() == Some(id.as_str()), id, name } })
        })
        .collect()
}

fn sinks() -> Result<Vec<Sink>, String> {
    Ok(parse_sinks(run("pw-dump", &[])?.as_bytes()))
}

#[tauri::command]
pub async fn audio_outputs() -> Result<Vec<Output>, String> {
    tauri::async_runtime::spawn_blocking(|| sinks().map(|s| s.into_iter().map(|s| s.output).collect())).await.map_err(|e| e.to_string())?
}

/// Makes the sink `id` the default. Only an id from the list is accepted.
#[tauri::command]
pub async fn audio_set_output(id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let sink = sinks()?.into_iter().find(|s| s.output.id == id).ok_or("unknown output")?;
        run("wpctl", &["set-default", &sink.object.to_string()]).map(drop)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---- brightness -----------------------------------------------------------------------------------------------------

/// A backlight: its name and folder in /sys/class/backlight.
fn backlight() -> Option<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = fs::read_dir("/sys/class/backlight")
        .ok()?
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .filter(|(_, p)| p.join("brightness").exists() && p.join("max_brightness").exists())
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    // The panel's own driver first: an NVIDIA entry beside it is the GPU's, not the screen's.
    let rank = |name: &str| if name.starts_with("intel_") || name.starts_with("amdgpu_") || name.starts_with("acpi_video") { 0 } else if name.starts_with("nvidia") { 2 } else { 1 };
    found.sort_by_key(|(name, _)| rank(name));
    found.into_iter().next()
}

fn number(path: PathBuf) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The slider's level in percent, None without a backlight (a desktop with an external monitor), which hides the slider.
#[tauri::command]
pub fn brightness_get() -> Option<u8> {
    let (_, dir) = backlight()?;
    let (now, max) = (number(dir.join("brightness"))?, number(dir.join("max_brightness"))?);
    (max > 0).then(|| ((now as f64 / max as f64) * 100.0).round().min(100.0) as u8)
}

/// The raw value for a percentage: at least 1 above 0 so the screen does not go black.
fn raw_brightness(percent: u8, max: u64) -> u32 {
    let raw = (f64::from(percent.min(100)) / 100.0 * max as f64).round() as u64;
    (if percent > 0 { raw.max(1) } else { raw }).min(max) as u32
}

/// Sets the backlight through logind, which lets a logged-in user do it without root.
#[tauri::command]
pub fn brightness_set(percent: u8) -> Result<(), String> {
    let (name, dir) = backlight().ok_or("unsupported")?;
    let max = number(dir.join("max_brightness")).ok_or("unsupported")?;
    let conn = Connection::system().map_err(|_| "unsupported".to_string())?;
    let session = Proxy::new(&conn, "org.freedesktop.login1", "/org/freedesktop/login1/session/auto", "org.freedesktop.login1.Session").map_err(|_| "unsupported".to_string())?;
    session.call_method("SetBrightness", &("backlight", name.as_str(), raw_brightness(percent, max))).map(drop).map_err(|e| e.to_string())
}

// ---- keep awake -----------------------------------------------------------------------------------------------------

/// The inhibit that keeps the screen on: the bus connection that asked for it (the inhibit ends with it) and its cookie.
struct Held {
    conn: Connection,
    cookie: u32,
    /// Tells a timed hold's timer whether it is still the one that is held.
    generation: u64,
}

static AWAKE: Mutex<Option<Held>> = Mutex::new(None);
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn screensaver(conn: &Connection) -> Result<Proxy<'_>, String> {
    Proxy::new(conn, "org.freedesktop.ScreenSaver", "/org/freedesktop/ScreenSaver", "org.freedesktop.ScreenSaver").map_err(|e| e.to_string())
}

/// Ends the hold if it is still the one with `generation` (None = whichever is held).
fn release(generation: Option<u64>) {
    let mut held = AWAKE.lock().unwrap();
    if held.as_ref().is_some_and(|h| generation.is_none_or(|g| g == h.generation)) {
        if let Some(h) = held.take() {
            if let Ok(proxy) = screensaver(&h.conn) {
                let _ = proxy.call_method("UnInhibit", &(h.cookie,));
            }
        }
    }
}

/// Keeps the screen on and the session from idling (`on`), for `minutes` or until turned off. The page shows the countdown;
/// the end of a timed hold is this side's.
#[tauri::command]
pub async fn keep_awake_set(on: bool, minutes: Option<u32>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        release(None);
        if !on {
            return Ok(());
        }
        let conn = Connection::session().map_err(|e| e.to_string())?;
        let cookie: u32 = screensaver(&conn)?.call_method("Inhibit", &("Crest", "Keep awake")).and_then(|r| r.body().deserialize()).map_err(|e| e.to_string())?;
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        *AWAKE.lock().unwrap() = Some(Held { conn, cookie, generation });
        if let Some(m) = minutes.filter(|&m| m > 0) {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(u64::from(m) * 60));
                release(Some(generation));
            });
        }
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---- focus (do not disturb) -----------------------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Focus {
    active: bool,
    kind: &'static str,
}

const SCHEMA: &str = "org.gnome.desktop.notifications";
const KEY: &str = "show-banners";

/// Do not disturb is the notification banners being off (GNOME). Elsewhere the schema is missing and it reads as off.
fn read_focus() -> Focus {
    let off = run("gsettings", &["get", SCHEMA, KEY]).is_ok_and(|v| v.trim() == "false");
    Focus { active: off, kind: "dnd" }
}

#[tauri::command]
pub async fn focus_get() -> Focus {
    tauri::async_runtime::spawn_blocking(read_focus).await.unwrap_or(Focus { active: false, kind: "dnd" })
}

#[tauri::command]
pub async fn focus_set(active: bool) -> Result<Focus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        run("gsettings", &["set", SCHEMA, KEY, if active { "false" } else { "true" }])?;
        Ok(read_focus())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wpctl_volume_is_read_with_its_mute() {
        let v = parse_volume("Volume: 0.46\n").unwrap();
        assert_eq!((v.level, v.muted), (0.46, false));
        let m = parse_volume("Volume: 0.30 [MUTED]\n").unwrap();
        assert_eq!((m.level, m.muted), (0.30, true));
        // Over 100 % is shown as full.
        assert_eq!(parse_volume("Volume: 1.50").unwrap().level, 1.0);
        assert!(parse_volume("nonsense").is_none());
    }

    #[test]
    fn sinks_are_listed_with_the_default_marked() {
        let dump = r#"[
          {"id": 63, "type": "PipeWire:Interface:Node", "info": {"props": {"media.class": "Audio/Sink", "node.name": "alsa_output.usb", "node.description": "Headset"}}},
          {"id": 65, "type": "PipeWire:Interface:Node", "info": {"props": {"media.class": "Audio/Sink", "node.name": "alsa_output.hdmi", "node.nick": "HDMI"}}},
          {"id": 70, "type": "PipeWire:Interface:Node", "info": {"props": {"media.class": "Stream/Output/Audio", "node.name": "chrome"}}},
          {"id": 31, "type": "PipeWire:Interface:Metadata", "metadata": [
            {"subject": 0, "key": "default.audio.source", "value": {"name": "mic"}},
            {"subject": 0, "key": "default.audio.sink", "value": {"name": "alsa_output.hdmi"}}]}
        ]"#;
        let sinks = parse_sinks(dump.as_bytes());
        assert_eq!(sinks.len(), 2);
        assert_eq!(sinks[0], Sink { object: 63, output: Output { id: "alsa_output.usb".into(), name: "Headset".into(), default: false } });
        assert_eq!((sinks[1].object, sinks[1].output.name.as_str(), sinks[1].output.default), (65, "HDMI", true));
        // The same key with its value as a JSON string.
        let as_string = r#"[{"id":1,"type":"PipeWire:Interface:Node","info":{"props":{"media.class":"Audio/Sink","node.name":"a"}}},
          {"id":2,"type":"PipeWire:Interface:Metadata","metadata":[{"key":"default.audio.sink","value":"{\"name\":\"a\"}"}]}]"#;
        assert!(parse_sinks(as_string.as_bytes())[0].output.default);
        assert!(parse_sinks(b"nope").is_empty());
    }

    #[test]
    fn a_percentage_becomes_a_raw_value_that_never_blacks_the_screen_out() {
        assert_eq!(raw_brightness(50, 255), 128);
        assert_eq!(raw_brightness(100, 255), 255);
        assert_eq!(raw_brightness(0, 255), 0);
        assert_eq!(raw_brightness(1, 100), 1);
        assert_eq!(raw_brightness(1, 10), 1, "above 0 stays visible");
        assert_eq!(raw_brightness(200, 100), 100);
    }
}
