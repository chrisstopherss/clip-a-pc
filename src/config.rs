/// Clip lengths offered in the settings, in seconds.
pub const DURATIONS_SECS: [u32; 6] = [15, 30, 45, 60, 90, 120];

pub const DEFAULT_DURATION_SECS: u32 = 30;

/// Snap any value (for example from a hand-edited config file) to the
/// closest allowed clip length. Ties go to the shorter one.
pub fn nearest_duration(secs: u32) -> u32 {
    let mut best = DURATIONS_SECS[0];
    for &d in &DURATIONS_SECS {
        if secs.abs_diff(d) < secs.abs_diff(best) {
            best = d;
        }
    }
    best
}

/// Microphone volume choices shown in settings.
pub const MIC_GAINS: [f32; 6] = [0.5, 1.0, 1.5, 2.0, 3.0, 4.0];

/// Everything the user can change. Stored as plain `key = value` lines.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub duration_secs: u32,
    /// Windows device name of the monitor to record, such as `\\.\DISPLAY1`.
    /// Empty means the primary monitor.
    pub monitor: String,
    pub mic_gain: f32,
    /// Check GitHub for a newer release and install it.
    pub auto_update: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { duration_secs: DEFAULT_DURATION_SECS, monitor: String::new(), mic_gain: 1.0, auto_update: true }
    }
}

impl Config {
    /// Read settings from text. Unknown lines and bad values are ignored, so a
    /// hand-edited file can never stop the program from starting.
    pub fn parse(text: &str) -> Self {
        let mut c = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "clip_seconds" => {
                    if let Ok(n) = v.parse::<u32>() {
                        c.duration_secs = nearest_duration(n);
                    }
                }
                "monitor" => c.monitor = v.to_string(),
                "auto_update" => c.auto_update = !matches!(v.to_ascii_lowercase().as_str(), "false" | "0" | "no" | "off"),
                "mic_volume" => {
                    if let Ok(g) = v.parse::<f32>() {
                        if g.is_finite() {
                            c.mic_gain = g.clamp(0.0, 8.0);
                        }
                    }
                }
                _ => {}
            }
        }
        c
    }

    pub fn to_text(&self) -> String {
        format!(
            "# clip-a-pc settings\nclip_seconds = {}\nmonitor = {}\nmic_volume = {}\nauto_update = {}\n",
            self.duration_secs, self.monitor, self.mic_gain, self.auto_update
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_allowed() {
        assert!(DURATIONS_SECS.contains(&DEFAULT_DURATION_SECS));
    }

    #[test]
    fn exact_values_are_kept() {
        for d in DURATIONS_SECS {
            assert_eq!(nearest_duration(d), d);
        }
    }

    #[test]
    fn values_snap_to_closest() {
        assert_eq!(nearest_duration(0), 15);
        assert_eq!(nearest_duration(40), 45);
        assert_eq!(nearest_duration(75), 60);
        assert_eq!(nearest_duration(1000), 120);
    }

    #[test]
    fn config_round_trips() {
        let c = Config { duration_secs: 90, monitor: "\\\\.\\DISPLAY2".into(), mic_gain: 2.0, auto_update: false };
        assert_eq!(Config::parse(&c.to_text()), c);
    }

    #[test]
    fn empty_or_garbage_gives_defaults() {
        assert_eq!(Config::parse(""), Config::default());
        assert_eq!(Config::parse("not a setting\n=\n###\nclip_seconds = abc\nmic_volume = nan"), Config::default());
    }

    #[test]
    fn hand_edited_values_are_fixed_up() {
        let c = Config::parse("clip_seconds = 40\nmic_volume = 99\n");
        assert_eq!(c.duration_secs, 45);
        assert_eq!(c.mic_gain, 8.0);
        assert_eq!(Config::parse("mic_volume = -3").mic_gain, 0.0);
    }

    #[test]
    fn mic_gains_are_sane() {
        assert!(MIC_GAINS.contains(&1.0));
    }

    #[test]
    fn auto_update_defaults_on_and_can_be_turned_off() {
        assert!(Config::default().auto_update);
        assert!(Config::parse("auto_update = true").auto_update);
        assert!(!Config::parse("auto_update = false").auto_update);
        assert!(!Config::parse("auto_update = 0").auto_update);
        assert!(Config::parse("auto_update = banana").auto_update);
    }
}
