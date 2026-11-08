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
}
