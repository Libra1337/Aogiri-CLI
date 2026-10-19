//! Whether someone is at this computer: how long since the last keyboard or
//! mouse input. Phones are told a turn is done only when nobody is, and an
//! approval waits a moment for the desktop when someone is.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

/// Input within this long means someone is at the computer.
pub const AT_COMPUTER: Duration = Duration::from_secs(3 * 60);

static CACHE: Mutex<Option<(Instant, Option<Duration>)>> = Mutex::new(None);

/// Time since the last input, or None where it cannot be told (a Linux
/// computer without X, a failure). Asked at most every 10 seconds.
pub fn idle() -> Option<Duration> {
    let mut cache = CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
    if let Some((at, value)) = *cache {
        if at.elapsed() < Duration::from_secs(10) {
            return value.map(|idle| idle + at.elapsed());
        }
    }
    let value = read_idle();
    *cache = Some((Instant::now(), value));
    value
}

#[cfg(target_os = "macos")]
fn read_idle() -> Option<Duration> {
    let output = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-c", "IOHIDSystem", "-d", "4", "-r", "-k", "HIDIdleTime"])
        .output()
        .ok()?;
    parse_ioreg(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(any(target_os = "macos", test))]
fn parse_ioreg(text: &str) -> Option<Duration> {
    let line = text.lines().find(|line| line.contains("\"HIDIdleTime\""))?;
    let nanos: u64 = line.rsplit('=').next()?.trim().parse().ok()?;
    Some(Duration::from_nanos(nanos))
}

#[cfg(windows)]
fn read_idle() -> Option<Duration> {
    use windows_sys::Win32::{
        System::SystemInformation::GetTickCount,
        UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO},
    };
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a valid LASTINPUTINFO with cbSize set.
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    // SAFETY: no arguments; the tick count wraps like dwTime does.
    let now = unsafe { GetTickCount() };
    Some(Duration::from_millis(now.wrapping_sub(info.dwTime) as u64))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn read_idle() -> Option<Duration> {
    // X11 through xprintidle where installed; Wayland has no common way.
    let output = std::process::Command::new("xprintidle").output().ok()?;
    let ms: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_millis(ms))
}

/// Someone used this computer within `AT_COMPUTER`; false when it cannot
/// be told.
pub fn at_computer() -> bool {
    // LYNSHEN_AT_COMPUTER=0/1 decides instead (tests, a server).
    match std::env::var("LYNSHEN_AT_COMPUTER").as_deref() {
        Ok("1") => return true,
        Ok("0") => return false,
        _ => {}
    }
    idle().is_some_and(|idle| idle < AT_COMPUTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_idle_time_ioreg_prints() {
        let text = "  | |   \"HIDIdleTime\" = 4617787912041\n  | |   \"Other\" = 1\n";
        assert_eq!(
            parse_ioreg(text),
            Some(Duration::from_nanos(4_617_787_912_041))
        );
        assert_eq!(parse_ioreg("nothing"), None);
    }
}
