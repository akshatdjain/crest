//! Seconds since the user's last input on Linux, for the break reminders (src/features/breaks). Asked for when the page wants it.
//!
//! GNOME's Mutter keeps an exact idle time. Elsewhere (KDE, Xfce, a bare window manager) systemd-logind's IdleHint says only
//! whether the session has been idle for a while and since when, which is coarser but needs no particular desktop.

use zbus::blocking::{Connection, Proxy};

fn mutter() -> Result<u64, String> {
    let conn = Connection::session().map_err(|e| e.to_string())?;
    let monitor = Proxy::new(&conn, "org.gnome.Mutter.IdleMonitor", "/org/gnome/Mutter/IdleMonitor/Core", "org.gnome.Mutter.IdleMonitor").map_err(|e| e.to_string())?;
    let ms: u64 = monitor.call_method("GetIdletime", &()).and_then(|r| r.body().deserialize()).map_err(|e| e.to_string())?;
    Ok(ms / 1000)
}

/// Seconds idle from logind's hints: 0 while the session is not idle, else the time since it became idle (microseconds on the
/// realtime clock).
fn idle_from_hint(idle: bool, since_us: u64, now_us: u64) -> u64 {
    if idle && since_us > 0 { now_us.saturating_sub(since_us) / 1_000_000 } else { 0 }
}

fn logind() -> Result<u64, String> {
    let conn = Connection::system().map_err(|e| e.to_string())?;
    let session = Proxy::new(&conn, "org.freedesktop.login1", "/org/freedesktop/login1/session/auto", "org.freedesktop.login1.Session").map_err(|e| e.to_string())?;
    let idle: bool = session.get_property("IdleHint").map_err(|e| e.to_string())?;
    let since: u64 = session.get_property("IdleSinceHint").map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_micros() as u64;
    Ok(idle_from_hint(idle, since, now))
}

pub fn read() -> Result<u64, String> {
    mutter().or_else(|_| logind())
}

#[cfg(test)]
mod tests {
    use super::idle_from_hint;

    #[test]
    fn logind_hints_become_seconds() {
        assert_eq!(idle_from_hint(false, 5_000_000, 9_000_000), 0, "an active session is not idle");
        assert_eq!(idle_from_hint(true, 5_000_000, 9_000_000), 4);
        assert_eq!(idle_from_hint(true, 0, 9_000_000), 0, "no start time known");
        assert_eq!(idle_from_hint(true, 9_000_000, 5_000_000), 0, "a clock that went back is not negative");
    }
}
