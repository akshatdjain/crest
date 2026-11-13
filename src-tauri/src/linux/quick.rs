//! The Overview tool's quick switches on Linux (the Windows ones are quick.rs, the Mac's macos/quick.rs): Wi-Fi, Bluetooth, the dark
//! theme, the microphone's mute state and the power buttons. Each goes through the tool a desktop already has (NetworkManager,
//! rfkill, GNOME's settings, systemd), and a switch whose hardware or tool is missing reads as None, which hides it.

use serde::Serialize;
use serde_json::Value;

use super::controls::run;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Quick {
    wifi: Option<bool>,
    bluetooth: Option<bool>,
    dark: bool,
    mic_muted: Option<bool>,
}

/// `enabled` / `disabled` from `nmcli radio wifi`, None when the machine has no Wi-Fi device.
fn wifi() -> Option<bool> {
    if !run("nmcli", &["-t", "-f", "TYPE", "device"]).ok()?.lines().any(|l| l.trim() == "wifi") {
        return None;
    }
    match run("nmcli", &["radio", "wifi"]).ok()?.trim() {
        "enabled" => Some(true),
        "disabled" => Some(false),
        _ => None,
    }
}

/// What `rfkill -J` says about the Bluetooth radios.
#[derive(Debug, PartialEq)]
struct Radio {
    /// Not blocked in software.
    on: bool,
    /// Blocked by a hardware switch, which Crest cannot lift.
    hard: bool,
}

fn parse_rfkill(json: &str, kind: &str) -> Option<Radio> {
    let devices: Vec<Value> = serde_json::from_str::<Value>(json).ok()?.get("rfkilldevices")?.as_array()?.iter().filter(|d| d["type"] == kind).cloned().collect();
    if devices.is_empty() {
        return None;
    }
    Some(Radio { on: devices.iter().any(|d| d["soft"] == "unblocked"), hard: devices.iter().any(|d| d["hard"] == "blocked") })
}

fn bluetooth() -> Option<Radio> {
    parse_rfkill(&run("rfkill", &["-J", "-o", "ID,TYPE,SOFT,HARD"]).ok()?, "bluetooth")
}

/// `'prefer-dark'` is GNOME's dark style. Other desktops have no such key and read as light.
fn dark() -> bool {
    run("gsettings", &["get", "org.gnome.desktop.interface", "color-scheme"]).is_ok_and(|v| v.contains("prefer-dark"))
}

#[tauri::command]
pub async fn quick_get() -> Quick {
    tauri::async_runtime::spawn_blocking(|| Quick { wifi: wifi(), bluetooth: bluetooth().map(|r| r.on), dark: dark(), mic_muted: crate::mic::muted() })
        .await
        .unwrap_or(Quick { wifi: None, bluetooth: None, dark: false, mic_muted: None })
}

/// Turns the Wi-Fi or Bluetooth radio on or off. Errors the page knows: "denied" (a hardware switch), "none" (no such radio).
#[tauri::command]
pub async fn quick_radio(kind: String, on: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || match kind.as_str() {
        "wifi" => {
            wifi().ok_or("none")?;
            run("nmcli", &["radio", "wifi", if on { "on" } else { "off" }]).map(drop)
        }
        "bluetooth" => {
            let radio = bluetooth().ok_or("none")?;
            if radio.hard {
                return Err("denied".into());
            }
            run("rfkill", &[if on { "unblock" } else { "block" }, "bluetooth"]).map(drop)
        }
        _ => Err("none".into()),
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn quick_dark(on: bool) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || run("gsettings", &["set", "org.gnome.desktop.interface", "color-scheme", if on { "prefer-dark" } else { "default" }]).map(drop))
        .await
        .map_err(|e| e.to_string())?
}

/// The command behind a power button, None for a name the page does not send.
fn power_command(action: &str) -> Option<(&'static str, &'static [&'static str])> {
    Some(match action {
        "lock" => ("loginctl", &["lock-session"]),
        "sleep" => ("systemctl", &["suspend"]),
        "restart" => ("systemctl", &["reboot"]),
        "shutdown" => ("systemctl", &["poweroff"]),
        _ => return None,
    })
}

/// Lock, sleep, restart or shut down. The page asks twice before the last two.
#[tauri::command]
pub async fn quick_power(action: String) -> Result<(), String> {
    let (program, args) = power_command(&action).ok_or("unknown")?;
    tauri::async_runtime::spawn_blocking(move || run(program, args).map(drop).map_err(|_| "failed".to_string())).await.map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfkill_reports_the_bluetooth_radio() {
        let json = r#"{"rfkilldevices": [
          {"id": 0, "type": "wlan", "soft": "unblocked", "hard": "unblocked"},
          {"id": 1, "type": "bluetooth", "soft": "blocked", "hard": "unblocked"}]}"#;
        assert_eq!(parse_rfkill(json, "bluetooth"), Some(Radio { on: false, hard: false }));
        assert_eq!(parse_rfkill(json, "wlan"), Some(Radio { on: true, hard: false }));
        let hard = r#"{"rfkilldevices": [{"id": 1, "type": "bluetooth", "soft": "unblocked", "hard": "blocked"}]}"#;
        assert_eq!(parse_rfkill(hard, "bluetooth"), Some(Radio { on: true, hard: true }));
        assert_eq!(parse_rfkill(json, "uwb"), None);
        assert_eq!(parse_rfkill("garbage", "bluetooth"), None);
    }

    #[test]
    fn only_the_four_power_buttons_run_anything() {
        assert_eq!(power_command("lock"), Some(("loginctl", &["lock-session"][..])));
        assert_eq!(power_command("shutdown"), Some(("systemctl", &["poweroff"][..])));
        assert_eq!(power_command("rm -rf /"), None);
        assert_eq!(power_command(""), None);
    }
}
