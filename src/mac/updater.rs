//! Self-update from the newest GitHub release: download the Mac zip, check its SHA-256, test-run it,
//! swap the app folder, and relaunch. Files fetched with curl carry no quarantine flag, so macOS
//! does not ask again after an update.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::util::{bundle_path, data_dir};
use crate::sha256::Sha256;
use crate::update::{asset_url, expected_hash, is_newer, tag_name};

const REPO: &str = "chrisstopherss/clip-a-pc";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const ZIP: &str = "clip-a-pc-mac.zip";

pub enum Outcome {
    UpToDate,
    /// A verified newer app is waiting.
    Ready(String),
    Failed(String),
}

fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("/usr/bin/curl")
        .args(["-sS", "-L", "--fail", "--max-time", "300", "-H", "User-Agent: clip-a-pc"])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run curl: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(format!("curl failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn staging_dir() -> PathBuf {
    data_dir().join("update")
}

pub fn staged_app() -> PathBuf {
    staging_dir().join("clip-a-pc.app")
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

fn runs_ok(app: &Path) -> bool {
    let Ok(mut child) = Command::new(app.join("Contents/MacOS/clip-a-pc")).arg("--version").stdout(Stdio::null()).spawn() else {
        return false;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return s.success(),
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
    if bundle_path().is_none() {
        return Ok(Outcome::UpToDate); // running outside an app folder: nothing to replace
    }
    let json = curl(&["-H", "Accept: application/vnd.github+json", &format!("https://api.github.com/repos/{REPO}/releases/latest")])?;
    let json = String::from_utf8_lossy(&json).to_string();
    let tag = tag_name(&json).ok_or("the release information had no version")?;
    if !is_newer(&tag, VERSION) {
        return Ok(Outcome::UpToDate);
    }
    let zip_url = asset_url(&json, ZIP).ok_or("the release has no Mac download yet")?;
    let sum_url = asset_url(&json, &format!("{ZIP}.sha256")).ok_or("the release has no Mac checksum file")?;
    let want = expected_hash(&String::from_utf8_lossy(&curl(&[&sum_url])?)).ok_or("the checksum file is not valid")?;

    let dir = staging_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let zip = dir.join(ZIP);
    curl(&["-o", &zip.to_string_lossy(), &zip_url])?;
    if sha256_of(&zip)? != want {
        let _ = std::fs::remove_dir_all(&dir);
        return Err("the download did not match its checksum, so it was thrown away".into());
    }
    let st = Command::new("/usr/bin/ditto").args(["-x", "-k"]).arg(&zip).arg(&dir).status().map_err(|e| e.to_string())?;
    if !st.success() || !staged_app().exists() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err("the download could not be unpacked".into());
    }
    let _ = std::fs::remove_file(&zip);
    if !runs_ok(&staged_app()) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err("the downloaded app would not start, so it was thrown away".into());
    }
    Ok(Outcome::Ready(tag))
}

/// Swap the verified app in and relaunch it a moment after this one exits.
pub fn apply_and_restart() -> Result<(), String> {
    let bundle = bundle_path().ok_or("not running from an app folder")?;
    let new = staged_app();
    if !new.exists() {
        return Err("no downloaded update is waiting".into());
    }
    let old = bundle.with_extension("app.old");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(&bundle, &old).map_err(|e| format!("could not move the running app aside: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &bundle) {
        let _ = std::fs::rename(&old, &bundle);
        return Err(format!("could not put the new version in place: {e}"));
    }
    let script = format!("sleep 1; /usr/bin/open -n '{}'; rm -rf '{}'", bundle.display(), old.display());
    Command::new("/bin/sh").args(["-c", &script]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|e| format!("could not relaunch: {e}"))?;
    Ok(())
}
