//! The few operating system helpers the shared recorder core needs.

#[cfg(windows)]
pub use crate::win::log::log;
#[cfg(windows)]
pub use crate::win::util::{timestamp_name, videos_dir};

#[cfg(target_os = "macos")]
pub use crate::mac::util::{log, timestamp_name, videos_dir};

#[cfg(not(any(windows, target_os = "macos")))]
pub use fallback::{log, timestamp_name, videos_dir};

/// Plain std versions so the shared core builds and can be tested on Linux.
#[cfg(not(any(windows, target_os = "macos")))]
mod fallback {
    use std::path::PathBuf;

    pub fn log(msg: &str) {
        eprintln!("{msg}");
    }

    pub fn videos_dir() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join("Videos")
    }

    /// UTC, as 2026-11-08_21-05-33.
    pub fn timestamp_name() -> String {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
        let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
        // civil date from days since 1970-01-01
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!("{y:04}-{m:02}-{d:02}_{:02}-{:02}-{:02}", rem / 3600, (rem / 60) % 60, rem % 60)
    }
}
