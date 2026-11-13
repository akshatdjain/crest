//! The Colour picker and the screenshots on Linux (the Windows ones are capture/ and capture/eyedropper.rs).
//!
//! Both go through the desktop portals, so the picking and the selecting are the desktop's own UI (GNOME, KDE ... each draw
//! theirs) and work on Wayland, where an app may not read the screen itself. Screen and sound recording and "find on screen"
//! are not here: they would need a screencast session with an encoder, and an OCR engine.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use zbus::zvariant::Value;

use super::portal::{path_from_uri, request};

const SCREENSHOT: &str = "org.freedesktop.portal.Screenshot";
/// The portal waits for the user, so it is given time.
const WAIT: Duration = Duration::from_secs(180);

// ---- colour ---------------------------------------------------------------------------------------------------------

/// A colour of three numbers from 0 to 1 as "#rrggbb".
fn rgb_hex(r: f64, g: f64, b: f64) -> String {
    let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
}

/// The colour the user picks on screen as "#rrggbb"; None when cancelled.
#[tauri::command]
pub async fn colour_pick() -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let response = request(SCREENSHOT, "PickColor", HashMap::new(), WAIT)?;
        match response.code {
            0 => {
                let (r, g, b): (f64, f64, f64) = response.results.get("color").and_then(|c| c.try_clone().ok()).and_then(|c| c.try_into().ok()).ok_or("failed")?;
                Ok(Some(rgb_hex(r, g, b)))
            }
            1 => Ok(None),
            _ => Err("failed".to_string()),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---- what the page receives -----------------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    id: String,
    kind: &'static str,
    name: String,
    width: i32,
    height: i32,
    primary: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Shot {
    path: String,
    name: String,
    thumb: Option<String>,
    width: i32,
    height: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecState {
    phase: &'static str,
    elapsed_ms: i64,
    path: Option<String>,
    failed: Vec<&'static str>,
    audio: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureFile {
    path: String,
    name: String,
    kind: &'static str,
    size: u64,
    modified: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Details {
    path: String,
    thumb: Option<String>,
    duration_ms: Option<u64>,
}

// ---- screenshots ----------------------------------------------------------------------------------------------------

/// Where screenshots are kept: Pictures/Screenshots, where GNOME and most tools keep theirs.
fn screenshots_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().picture_dir().map_err(|_| "no-folder".to_string())?.join("Screenshots");
    fs::create_dir_all(&dir).map_err(|_| "no-folder".to_string())?;
    Ok(dir)
}

/// A free "Screenshot 2026-11-14 031500.png", with " (2)" and so on when that name is taken.
fn free_name(dir: &Path, stamp: &str) -> String {
    let first = format!("Screenshot {stamp}.png");
    if !dir.join(&first).exists() {
        return first;
    }
    (2..).map(|n| format!("Screenshot {stamp} ({n}).png")).find(|name| !dir.join(name).exists()).unwrap_or(first)
}

/// A monitor as the portal's picture shows it: where it starts in the picture of all of them, and its size, in physical pixels.
#[derive(Clone, Debug, PartialEq)]
struct Area {
    id: String,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// The rectangle of one monitor in the picture of all of them, when that picture is exactly the bounding box of the monitors. If it
/// is not (the desktop scales it, a monitor is missing), None: the whole picture is kept rather than a wrong piece of it.
fn crop_rect(areas: &[Area], id: &str, picture: (u32, u32)) -> Option<(u32, u32, u32, u32)> {
    let (left, top) = (areas.iter().map(|a| a.x).min()?, areas.iter().map(|a| a.y).min()?);
    let (right, bottom) = (areas.iter().map(|a| a.x + a.w as i32).max()?, areas.iter().map(|a| a.y + a.h as i32).max()?);
    if ((right - left) as u32, (bottom - top) as u32) != picture {
        return None;
    }
    let a = areas.iter().find(|a| a.id == id)?;
    Some(((a.x - left) as u32, (a.y - top) as u32, a.w, a.h))
}

fn areas(app: &AppHandle) -> Vec<Area> {
    app.available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| Area { id: format!("monitor:{}", m.name().cloned().unwrap_or_default()), x: m.position().x, y: m.position().y, w: m.size().width, h: m.size().height })
        .collect()
}

/// Monitors first (the primary, then left to right). Windows are the desktop's to choose from, in its own picker.
#[tauri::command]
pub fn capture_sources(app: AppHandle) -> Vec<SourceInfo> {
    let primary = app.primary_monitor().ok().flatten().and_then(|m| m.name().cloned());
    let mut monitors: Vec<_> = app.available_monitors().unwrap_or_default();
    monitors.sort_by_key(|m| (m.name().cloned() != primary, m.position().x, m.position().y));
    monitors
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let name = m.name().cloned().unwrap_or_default();
            SourceInfo { id: format!("monitor:{name}"), kind: "monitor", primary: Some(&name) == primary.as_ref(), name: if name.is_empty() { format!("Display {}", i + 1) } else { name }, width: m.size().width as i32, height: m.size().height as i32 }
        })
        .collect()
}

/// Takes a screenshot through the portal and keeps it in Pictures/Screenshots, named like the Windows ones, and on the clipboard.
/// "region" shows the desktop's own selector; the others take the whole picture, cut to one monitor for "monitor".
/// None when the user cancelled.
#[tauri::command]
pub async fn capture_shot(app: AppHandle, mode: String, monitor: Option<String>) -> Result<Option<Shot>, String> {
    let dir = screenshots_dir(&app)?;
    let areas = areas(&app);
    tauri::async_runtime::spawn_blocking(move || {
        let options = HashMap::from([("interactive", Value::from(mode == "region")), ("modal", Value::from(true))]);
        let response = request(SCREENSHOT, "Screenshot", options, WAIT).map_err(|_| "failed".to_string())?;
        match response.code {
            0 => {}
            1 => return Ok(None),
            _ => return Err("failed".to_string()),
        }
        let uri = response.results.get("uri").and_then(super::media::text).ok_or("failed")?;
        let taken = path_from_uri(&uri).ok_or("failed")?;
        let mut picture = image::open(&taken).map_err(|_| "failed".to_string())?.to_rgba8();
        if mode == "monitor" {
            let id = format!("monitor:{}", monitor.ok_or("no-monitor")?);
            if !areas.iter().any(|a| a.id == id) {
                return Err("no-monitor".into());
            }
            if let Some((x, y, w, h)) = crop_rect(&areas, &id, picture.dimensions()) {
                picture = image::imageops::crop_imm(&picture, x, y, w, h).to_image();
            }
        }
        let name = free_name(&dir, &jiff::Zoned::now().strftime("%Y-%m-%d %H%M%S").to_string());
        let path = dir.join(&name);
        picture.save(&path).map_err(|_| "save-failed".to_string())?;
        // The portal's own file is the same picture: not left behind as a second copy.
        if Path::new(&taken) != path && taken.starts_with(&*dir.to_string_lossy()) {
            let _ = fs::remove_file(&taken);
        }
        let (width, height) = picture.dimensions();
        let thumb = crate::clipboard::thumbnail(&picture);
        // The picture is also put on the clipboard, as on Windows.
        let _ = arboard::Clipboard::new().and_then(|mut c| c.set_image(arboard::ImageData { width: width as usize, height: height as usize, bytes: picture.into_raw().into() }));
        Ok(Some(Shot { path: path.to_string_lossy().into_owned(), name, thumb, width: width as i32, height: height as i32 }))
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---- the library ----------------------------------------------------------------------------------------------------

/// Recording is not offered on Linux, so the state never leaves idle.
#[tauri::command]
pub fn capture_state() -> RecState {
    RecState { phase: "idle", elapsed_ms: 0, path: None, failed: Vec::new(), audio: false }
}

#[tauri::command]
pub fn capture_start() -> Result<(), String> {
    Err("failed".into())
}

#[tauri::command]
pub fn capture_pause(paused: bool) {
    let _ = paused;
}

#[tauri::command]
pub fn capture_stop() {}

fn is_screenshot(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".png")
}

/// The screenshots in the folder, newest first.
fn list(dir: &Path) -> Vec<CaptureFile> {
    let mut files: Vec<CaptureFile> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata().ok().filter(|m| m.is_file())?;
            is_screenshot(&name).then(|| CaptureFile {
                path: e.path().to_string_lossy().into_owned(),
                name,
                kind: "shot",
                size: meta.len(),
                modified: meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as u64),
            })
        })
        .collect();
    files.sort_by(|a, b| b.modified.cmp(&a.modified));
    files
}

#[tauri::command]
pub fn capture_list(app: AppHandle) -> Result<Vec<CaptureFile>, String> {
    Ok(list(&screenshots_dir(&app)?))
}

/// Whether `path` is a screenshot inside `dir`, by its resolved location: a link or a `..` cannot lead out of the folder.
fn inside_library(dir: &Path, path: &str) -> bool {
    let Ok(real) = fs::canonicalize(path) else { return false };
    let Ok(dir) = fs::canonicalize(dir) else { return false };
    real.parent() == Some(dir.as_path()) && is_screenshot(&real.to_string_lossy())
}

#[tauri::command]
pub async fn capture_details(app: AppHandle, paths: Vec<String>) -> Result<Vec<Details>, String> {
    let dir = screenshots_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        paths.into_iter().map(|path| Details { thumb: inside_library(&dir, &path).then(|| image::open(&path).ok().and_then(|i| crate::clipboard::thumbnail(&i.to_rgba8()))).flatten(), path, duration_ms: None }).collect()
    })
    .await
    .map_err(|e| e.to_string())
}

/// Moves a screenshot of the library to the trash, where it can be taken back.
#[tauri::command]
pub async fn capture_delete(app: AppHandle, path: String) -> Result<(), String> {
    let dir = screenshots_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        if !inside_library(&dir, &path) {
            return Err("not-capture".to_string());
        }
        let trashed = std::process::Command::new("gio").args(["trash", &path]).status().is_ok_and(|s| s.success());
        // Without gio (or a trash) the file is removed, as the page asked.
        if trashed || fs::remove_file(&path).is_ok() { Ok(()) } else { Err("delete-failed".to_string()) }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(id: &str, x: i32, y: i32, w: u32, h: u32) -> Area {
        Area { id: format!("monitor:{id}"), x, y, w, h }
    }

    #[test]
    fn a_colour_is_written_as_six_hex_digits() {
        assert_eq!(rgb_hex(1.0, 0.5, 0.0), "#ff8000");
        assert_eq!(rgb_hex(0.0, 0.0, 0.0), "#000000");
        assert_eq!(rgb_hex(2.0, -1.0, 0.2), "#ff0033", "out of range is clamped");
    }

    #[test]
    fn a_monitor_is_cut_out_of_the_picture_of_all_of_them() {
        // A laptop screen at the left and a larger monitor beside it, tops aligned.
        let areas = [area("eDP", 0, 0, 2048, 1280), area("HDMI", 2048, 0, 1920, 1080)];
        assert_eq!(crop_rect(&areas, "monitor:HDMI", (3968, 1280)), Some((2048, 0, 1920, 1080)));
        assert_eq!(crop_rect(&areas, "monitor:eDP", (3968, 1280)), Some((0, 0, 2048, 1280)));
        // A monitor above the origin: the picture starts at the topmost, leftmost corner.
        let above = [area("A", -1920, -1080, 1920, 1080), area("B", 0, 0, 1920, 1080)];
        assert_eq!(crop_rect(&above, "monitor:B", (3840, 2160)), Some((1920, 1080, 1920, 1080)));
        // A picture of another size is not guessed at; neither is an unknown monitor.
        assert_eq!(crop_rect(&areas, "monitor:HDMI", (1920, 1080)), None);
        assert_eq!(crop_rect(&areas, "monitor:nope", (3968, 1280)), None);
        assert_eq!(crop_rect(&[], "monitor:x", (1, 1)), None);
    }

    #[test]
    fn names_are_numbered_when_taken() {
        let dir = std::env::temp_dir().join(format!("crest-shots-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(free_name(&dir, "2026-11-14 031500"), "Screenshot 2026-11-14 031500.png");
        fs::write(dir.join("Screenshot 2026-11-14 031500.png"), b"x").unwrap();
        assert_eq!(free_name(&dir, "2026-11-14 031500"), "Screenshot 2026-11-14 031500 (2).png");
        fs::write(dir.join("Screenshot 2026-11-14 031500 (2).png"), b"x").unwrap();
        assert_eq!(free_name(&dir, "2026-11-14 031500"), "Screenshot 2026-11-14 031500 (3).png");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_screenshots_directly_in_the_folder_are_the_librarys() {
        let root = std::env::temp_dir().join(format!("crest-lib-{}", std::process::id()));
        let dir = root.join("Screenshots");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.png"), b"x").unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();
        fs::write(dir.join("sub/b.png"), b"x").unwrap();
        fs::write(root.join("outside.png"), b"x").unwrap();
        let inside = |p: &str| inside_library(&dir, &dir.join(p).to_string_lossy());
        assert!(inside("a.png"));
        assert!(!inside("notes.txt") && !inside("sub/b.png") && !inside("missing.png"));
        assert!(!inside_library(&dir, &root.join("outside.png").to_string_lossy()));
        assert!(!inside("../outside.png"), "a .. does not lead out");
        std::os::unix::fs::symlink(root.join("outside.png"), dir.join("link.png")).unwrap();
        assert!(!inside("link.png"), "a link to a file elsewhere is not the library's");
        assert_eq!(list(&dir).iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["a.png"], "never the text, the folder or a link");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn recording_never_starts_and_the_state_stays_idle() {
        assert!(capture_start().is_err());
        let s = capture_state();
        assert_eq!((s.phase, s.elapsed_ms, s.audio), ("idle", 0, false));
    }
}
