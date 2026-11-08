//! A small log file, because the app has no console. Kept short on purpose.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use super::util::timestamp_name;

static LOCK: Mutex<()> = Mutex::new(());
const MAX_BYTES: u64 = 256 * 1024;

pub fn data_dir() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    let d = PathBuf::from(base).join("clip-a-pc");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn log_path() -> PathBuf {
    data_dir().join("log.txt")
}

/// Start a fresh log if the old one has grown large.
pub fn trim() {
    if let Ok(m) = std::fs::metadata(log_path()) {
        if m.len() > MAX_BYTES {
            let _ = std::fs::write(log_path(), b"");
        }
    }
}

pub fn log(msg: &str) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = writeln!(f, "{}  {}", timestamp_name(), msg);
    }
}
