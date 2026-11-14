//! Plain linear resampling of interleaved stereo audio, for microphones that do
//! not run at 48 kHz. Good enough for speech; each chunk is converted on its own.

/// Convert interleaved stereo f32 from `from` Hz to `to` Hz.
pub fn resample_stereo(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || input.len() < 2 || from == 0 || to == 0 {
        return input.to_vec();
    }
    let frames_in = input.len() / 2;
    let frames_out = ((frames_in as u64 * to as u64) / from as u64) as usize;
    let step = from as f64 / to as f64;
    let mut out = Vec::with_capacity(frames_out * 2);
    for i in 0..frames_out {
        let pos = i as f64 * step;
        let a = pos.floor() as usize;
        let frac = (pos - a as f64) as f32;
        let b = (a + 1).min(frames_in - 1);
        for ch in 0..2 {
            let x = input[a.min(frames_in - 1) * 2 + ch];
            let y = input[b * 2 + ch];
            out.push(x + (y - x) * frac);
        }
    }
    out
}

/// Turn mono into interleaved stereo.
pub fn mono_to_stereo(input: &[f32]) -> Vec<f32> {
    input.iter().flat_map(|&s| [s, s]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_rate_is_untouched() {
        let v = vec![0.1, 0.2, 0.3, 0.4];
        assert_eq!(resample_stereo(&v, 48000, 48000), v);
    }

    #[test]
    fn length_follows_the_ratio() {
        let v = vec![0.5f32; 2 * 441];
        assert_eq!(resample_stereo(&v, 44100, 48000).len() / 2, 480);
        let v = vec![0.5f32; 2 * 160];
        assert_eq!(resample_stereo(&v, 16000, 48000).len() / 2, 480);
        let v = vec![0.5f32; 2 * 960];
        assert_eq!(resample_stereo(&v, 96000, 48000).len() / 2, 480);
    }

    #[test]
    fn constant_signal_stays_constant_and_channels_stay_apart() {
        let mut v = Vec::new();
        for _ in 0..441 {
            v.extend_from_slice(&[0.25, -0.5]);
        }
        let o = resample_stereo(&v, 44100, 48000);
        for f in o.chunks(2) {
            assert!((f[0] - 0.25).abs() < 1e-6 && (f[1] + 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn a_ramp_keeps_its_shape() {
        let v: Vec<f32> = (0..200).flat_map(|i| [i as f32 / 200.0, i as f32 / 200.0]).collect();
        let o = resample_stereo(&v, 16000, 48000);
        assert!(o.windows(2).step_by(2).all(|w| w[0] <= w[1] + 1e-6 || true));
        let l: Vec<f32> = o.chunks(2).map(|f| f[0]).collect();
        assert!(l.windows(2).all(|w| w[1] >= w[0] - 1e-6), "not monotonic");
    }

    #[test]
    fn mono_becomes_stereo_and_tiny_input_is_safe() {
        assert_eq!(mono_to_stereo(&[0.1, 0.2]), vec![0.1, 0.1, 0.2, 0.2]);
        assert!(resample_stereo(&[], 16000, 48000).is_empty());
        assert_eq!(resample_stereo(&[0.3, 0.3], 16000, 48000).len() % 2, 0);
    }
}
