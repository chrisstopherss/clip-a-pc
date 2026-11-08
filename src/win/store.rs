//! Settings file and the start-with-Windows entry.

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::System::Registry::*;

use super::log::data_dir;
use crate::config::Config;

fn config_path() -> std::path::PathBuf {
    data_dir().join("config.txt")
}

/// None means there is no settings file yet, which is how first run is detected.
pub fn load_config() -> Option<Config> {
    std::fs::read_to_string(config_path()).ok().map(|t| Config::parse(&t))
}

pub fn save_config(c: &Config) -> std::io::Result<()> {
    std::fs::write(config_path(), c.to_text())
}

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("clip-a-pc");

pub fn set_registry_string(subkey: &str, name: &str, value: &str) -> bool {
    unsafe {
        let mut key = HKEY::default();
        let sub = HSTRING::from(subkey);
        if RegCreateKeyExW(HKEY_CURRENT_USER, &sub, None, None, REG_OPTION_NON_VOLATILE, KEY_WRITE, None, &mut key, None).is_err() {
            return false;
        }
        let data: Vec<u8> = value.encode_utf16().chain(std::iter::once(0)).flat_map(|u| u.to_le_bytes()).collect();
        let ok = RegSetValueExW(key, &HSTRING::from(name), None, REG_SZ, Some(&data)).is_ok();
        let _ = RegCloseKey(key);
        ok
    }
}

pub fn autostart_enabled() -> bool {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_READ, &mut key).is_err() {
            return false;
        }
        let found = RegQueryValueExW(key, RUN_VALUE, None, None, None, None).is_ok();
        let _ = RegCloseKey(key);
        found
    }
}

pub fn set_autostart(on: bool) {
    unsafe {
        if on {
            if let Ok(exe) = std::env::current_exe() {
                let _ = set_registry_string(
                    "Software\\Microsoft\\Windows\\CurrentVersion\\Run",
                    "clip-a-pc",
                    &format!("\"{}\"", exe.display()),
                );
            }
        } else {
            let mut key = HKEY::default();
            if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_WRITE, &mut key).is_ok() {
                let _ = RegDeleteValueW(key, RUN_VALUE);
                let _ = RegCloseKey(key);
            }
        }
    }
}
