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

/// Recording quality. Higher quality means a bigger buffer in memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    P720x30,
    P1080x60,
}

impl Quality {
    pub const ALL: [Quality; 2] = [Quality::P720x30, Quality::P1080x60];

    pub fn label(self) -> &'static str {
        match self {
            Quality::P720x30 => "720p, 30 fps",
            Quality::P1080x60 => "1080p, 60 fps",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Quality::P720x30 => "720p30",
            Quality::P1080x60 => "1080p60",
        }
    }

    pub fn from_key(s: &str) -> Option<Quality> {
        Quality::ALL.into_iter().find(|q| q.key().eq_ignore_ascii_case(s.trim()))
    }

    /// Largest picture size recorded. Smaller screens are never scaled up.
    pub fn max_size(self) -> (u32, u32) {
        match self {
            Quality::P720x30 => (1280, 720),
            Quality::P1080x60 => (1920, 1080),
        }
    }

    pub fn fps(self) -> u32 {
        match self {
            Quality::P720x30 => 30,
            Quality::P1080x60 => 60,
        }
    }

    /// Average and peak video bitrate, bits per second.
    pub fn mean_bps(self) -> u32 {
        match self {
            Quality::P720x30 => 5_000_000,
            Quality::P1080x60 => 10_000_000,
        }
    }

    pub fn max_bps(self) -> u32 {
        match self {
            Quality::P720x30 => 7_000_000,
            Quality::P1080x60 => 14_000_000,
        }
    }

    /// Buffer bytes reserved per second of clip: a little above the average bitrate.
    pub fn ring_bytes_per_sec(self) -> usize {
        match self {
            Quality::P720x30 => 800_000,
            Quality::P1080x60 => 1_450_000,
        }
    }

    /// Memory the program uses with nothing buffered (graphics driver, encoder, textures).
    fn baseline_mb(self) -> f64 {
        match self {
            Quality::P720x30 => 70.0,
            Quality::P1080x60 => 85.0,
        }
    }
}

/// Rough total memory use in MB for a clip length and quality. A guess for the
/// settings window, not a measurement.
pub fn estimate_memory_mb(secs: u32, q: Quality) -> u32 {
    let ring = (secs + 2) as f64 * q.ring_bytes_per_sec() as f64 / 1_048_576.0;
    (q.baseline_mb() + ring).round() as u32
}

/// Everything the user can change. Stored as plain `key = value` lines.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub duration_secs: u32,
    /// Windows device name of the monitor to record, such as `\\.\DISPLAY1`.
    /// Empty means the primary monitor.
    pub monitor: String,
    pub mic_gain: f32,
    pub quality: Quality,
    /// Check GitHub for a newer release and install it.
    pub auto_update: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self { duration_secs: DEFAULT_DURATION_SECS, monitor: String::new(), mic_gain: 1.0, quality: Quality::P720x30, auto_update: true }
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
                "quality" => {
                    if let Some(q) = Quality::from_key(v) {
                        c.quality = q;
                    }
                }
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
            "# clip-a-pc settings\nclip_seconds = {}\nmonitor = {}\nmic_volume = {}\nquality = {}\nauto_update = {}\n",
            self.duration_secs, self.monitor, self.mic_gain, self.quality.key(), self.auto_update
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
        let c = Config { duration_secs: 90, monitor: "\\\\.\\DISPLAY2".into(), mic_gain: 2.0, quality: Quality::P1080x60, auto_update: false };
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

    #[test]
    fn quality_keys_round_trip_and_unknown_is_ignored() {
        for q in Quality::ALL {
            assert_eq!(Quality::from_key(q.key()), Some(q));
        }
        assert_eq!(Quality::from_key(" 1080P60 "), Some(Quality::P1080x60));
        assert_eq!(Quality::from_key("4k120"), None);
        assert_eq!(Config::parse("quality = 4k120").quality, Quality::P720x30);
        assert_eq!(Config::parse("quality = 1080p60").quality, Quality::P1080x60);
        assert_eq!(Config::default().quality, Quality::P720x30);
    }

    #[test]
    fn higher_quality_means_more_buffer_and_more_memory() {
        assert!(Quality::P1080x60.fps() > Quality::P720x30.fps());
        assert!(Quality::P1080x60.ring_bytes_per_sec() > Quality::P720x30.ring_bytes_per_sec());
        // the ring must hold at least the average bitrate
        for q in Quality::ALL {
            assert!(q.ring_bytes_per_sec() as u32 * 8 > q.mean_bps());
            assert!(q.max_bps() > q.mean_bps());
        }
        for d in DURATIONS_SECS {
            assert!(estimate_memory_mb(d, Quality::P1080x60) > estimate_memory_mb(d, Quality::P720x30));
        }
        assert!(estimate_memory_mb(120, Quality::P720x30) > estimate_memory_mb(15, Quality::P720x30));
    }

    #[test]
    fn estimates_match_the_measured_baseline() {
        // 30 s at 720p was measured near 100 MB; the estimate must not be wildly off
        let e = estimate_memory_mb(30, Quality::P720x30);
        assert!((85..=115).contains(&e), "{e}");
    }
}
