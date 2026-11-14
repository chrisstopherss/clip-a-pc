//! Mac helpers: folders, the log file, clocks, memory.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

static LOG_LOCK: Mutex<()> = Mutex::new(());
const MAX_LOG_BYTES: u64 = 256 * 1024;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// ~/Library/Application Support/clip-a-pc, for settings and the log.
pub fn data_dir() -> PathBuf {
    let d = home().join("Library").join("Application Support").join("clip-a-pc");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn log_path() -> PathBuf {
    data_dir().join("log.txt")
}

pub fn videos_dir() -> PathBuf {
    home().join("Movies")
}

/// Local time as 2026-11-08_21-05-33, for file names.
pub fn timestamp_name() -> String {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!(
            "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

pub fn trim_log() {
    if let Ok(m) = std::fs::metadata(log_path()) {
        if m.len() > MAX_LOG_BYTES {
            let _ = std::fs::write(log_path(), b"");
        }
    }
}

pub fn log(msg: &str) {
    let _g = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{}  {}", timestamp_name(), msg);
    }
}

/// The system's monotonic clock in 100 ns units. ScreenCaptureKit stamps its
/// frames and audio with this same clock, which keeps sound and picture in step.
pub fn now_100ns() -> i64 {
    use std::sync::OnceLock;
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    let (numer, denom) = *TB.get_or_init(|| unsafe {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        libc::mach_timebase_info(&mut info);
        (info.numer.max(1) as u64, info.denom.max(1) as u64)
    });
    let t = unsafe { libc::mach_absolute_time() } as u128;
    (t * numer as u128 / denom as u128 / 100) as i64
}

/// Resident memory of this process in MB.
pub fn memory_mb() -> f64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output();
    out.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok()).map(|kb| kb / 1024.0).unwrap_or(0.0)
}

fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A notification-centre banner. Uses the system's own tool so it needs no extra permission setup.
pub fn notify(title: &str, body: &str) {
    let script = format!("display notification \"{}\" with title \"{}\"", applescript_escape(body), applescript_escape(title));
    let _ = std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).spawn();
}

/// Open a file, folder or URL with the default app.
pub fn open(target: &str) {
    let _ = std::process::Command::new("/usr/bin/open").arg(target).spawn();
}

/// The Screen Recording page of System Settings.
pub fn open_screen_recording_settings() {
    open("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture");
}

/// The .app folder this program is running from, if it is running from one.
pub fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let b = exe.ancestors().nth(3)?.to_path_buf();
    (b.extension().map(|e| e == "app").unwrap_or(false)).then_some(b)
}
