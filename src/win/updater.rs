//! Self-update from the newest GitHub release.
//!
//! The download uses the `curl.exe` that ships with Windows, so this program
//! carries no networking or TLS code of its own. A new exe is downloaded next
//! to the running one, checked against the release's SHA-256 file, test-run,
//! and then swapped in: a running exe can be renamed but not overwritten, so the
//! old one is renamed out of the way first.

use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::sha256::Sha256;
use crate::update::{asset_url, expected_hash, is_newer, tag_name};

const REPO: &str = "chrisstopherss/clip-a-pc";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub enum Outcome {
    UpToDate,
    /// A verified newer exe is waiting next to this one.
    Ready(String),
    Failed(String),
}

fn curl_exe() -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join("curl.exe")
}

fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new(curl_exe())
        .args(["-sS", "-L", "--fail", "--max-time", "180", "-H", "User-Agent: clip-a-pc"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("could not run curl.exe: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!("curl failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn paths() -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let name = exe.file_name().ok_or("no exe name")?.to_string_lossy().to_string();
    let dir = exe.parent().ok_or("no exe folder")?;
    Ok((exe.clone(), dir.join(format!("{name}.new")), dir.join(format!("{name}.old"))))
}

fn sha256_of(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finish_hex())
}

/// The downloaded exe must start and exit cleanly when asked for its version.
fn runs_ok(path: &Path) -> bool {
    let Ok(mut child) = Command::new(path).arg("--version").creation_flags(CREATE_NO_WINDOW).spawn() else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if start.elapsed() < Duration::from_secs(10) => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                return false;
            }
        }
    }
}

pub fn check_and_download() -> Outcome {
    match try_check() {
        Ok(o) => o,
        Err(e) => Outcome::Failed(e),
    }
}

fn try_check() -> Result<Outcome, String> {
    let json = curl(&["-H", "Accept: application/vnd.github+json", &format!("https://api.github.com/repos/{REPO}/releases/latest")])?;
    let json = String::from_utf8_lossy(&json).to_string();
    let tag = tag_name(&json).ok_or("the release information had no version")?;
    if !is_newer(&tag, VERSION) {
        return Ok(Outcome::UpToDate);
    }
    let exe_url = asset_url(&json, "clip-a-pc.exe").ok_or("the release has no clip-a-pc.exe")?;
    let sum_url = asset_url(&json, "clip-a-pc.exe.sha256").ok_or("the release has no checksum file")?;
    let (_, new, _) = paths()?;

    let want = expected_hash(&String::from_utf8_lossy(&curl(&[&sum_url])?)).ok_or("the checksum file is not valid")?;
    let _ = std::fs::remove_file(&new);
    curl(&["-o", &new.to_string_lossy(), &exe_url])?;
    let got = sha256_of(&new)?;
    if got != want {
        let _ = std::fs::remove_file(&new);
        return Err("the download did not match its checksum, so it was thrown away".into());
    }
    if !runs_ok(&new) {
        let _ = std::fs::remove_file(&new);
        return Err("the downloaded program would not start, so it was thrown away".into());
    }
    Ok(Outcome::Ready(tag))
}

/// Swap the verified download in and start it. The caller then quits.
pub fn apply_and_restart() -> Result<(), String> {
    let (exe, new, old) = paths()?;
    if !new.exists() {
        return Err("no downloaded update is waiting".into());
    }
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("could not move the running program aside: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("could not put the new version in place: {e}"));
    }
    if let Err(e) = Command::new(&exe).arg("--after-update").spawn() {
        // put the old version back so the app is never left without an exe
        let _ = std::fs::remove_file(&exe);
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("could not start the new version: {e}"));
    }
    Ok(())
}

/// Remove the previous version left behind by an update.
pub fn cleanup_old() {
    if let Ok((_, new, old)) = paths() {
        let _ = std::fs::remove_file(old);
        let _ = new; // a half-finished download is replaced on the next check
    }
}
