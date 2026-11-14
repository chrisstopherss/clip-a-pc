//! Settings file and the start-at-login entry.

use std::path::PathBuf;

use super::util::{bundle_path, data_dir};
use crate::config::Config;

fn config_path() -> PathBuf {
    data_dir().join("config.txt")
}

/// None means there is no settings file yet, which is how first run is detected.
pub fn load_config() -> Option<Config> {
    std::fs::read_to_string(config_path()).ok().map(|t| Config::parse(&t))
}

pub fn save_config(c: &Config) -> std::io::Result<()> {
    std::fs::write(config_path(), c.to_text())
}

const LABEL: &str = "io.github.chrisstopherss.clip-a-pc";

fn agent_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join("Library").join("LaunchAgents").join(format!("{LABEL}.plist"))
}

pub fn autostart_enabled() -> bool {
    agent_path().exists()
}

/// Start at login through a LaunchAgent. Only possible from an installed .app.
pub fn set_autostart(on: bool) {
    if !on {
        let _ = std::fs::remove_file(agent_path());
        return;
    }
    let Some(bundle) = bundle_path() else { return };
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array><string>/usr/bin/open</string><string>{}</string></array>\n  <key>RunAtLoad</key><true/>\n</dict>\n</plist>\n",
        bundle.display()
    );
    if let Some(dir) = agent_path().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(agent_path(), plist);
}
