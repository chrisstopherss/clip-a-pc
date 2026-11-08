use std::path::PathBuf;
use std::sync::OnceLock;

use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
use windows::Win32::UI::Shell::{FOLDERID_Videos, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

/// The system's high resolution clock in 100 ns units. Audio capture timestamps
/// use the same clock, which is what keeps sound and picture in step.
pub fn qpc_now_100ns() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    let f = *FREQ.get_or_init(|| {
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        f.max(1)
    });
    let mut c = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
    }
    (c as i128 * 10_000_000 / f as i128) as i64
}

/// (private MB, working set MB)
pub fn memory_mb() -> (f64, f64) {
    unsafe {
        let mut pmc = PROCESS_MEMORY_COUNTERS_EX::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let _ = GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc as *mut _ as *mut _, size);
        (pmc.PrivateUsage as f64 / 1048576.0, pmc.WorkingSetSize as f64 / 1048576.0)
    }
}

/// Total CPU time used by this process so far, in seconds.
pub fn cpu_seconds() -> f64 {
    let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
    let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
    unsafe {
        let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
    }
    ft(k) + ft(u)
}

/// The user's Videos folder, wherever Windows (or OneDrive) has put it.
pub fn videos_dir() -> PathBuf {
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None) {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
    }
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Videos")
}

/// Local time as 2026-11-08_21-05-33, for file names.
pub fn timestamp_name() -> String {
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}
