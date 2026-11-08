//! Mixes several timestamped audio sources into one steady stream of fixed-size frames.
//!
//! Each source delivers chunks of 48 kHz stereo f32 with a capture timestamp.
//! Chunks are placed on a shared sample timeline by timestamp. Sources that go
//! quiet (loopback sends nothing while silent) read as zeros. Small timestamp
//! jitter is smoothed out, large jumps are treated as gaps or overlaps.

use std::collections::VecDeque;

pub const RATE: i64 = 48_000;
/// Samples per output frame (also the AAC frame size).
pub const FRAME: usize = 1024;
/// Chunk timestamps within this many samples of where the last chunk ended are
/// treated as contiguous (20 ms).
const JITTER_TOL: i64 = 960;
/// How long to wait for late chunks before a frame is emitted (100 ms).
const LATENCY_100NS: i64 = 1_000_000;

#[derive(Default, Clone, Copy, Debug)]
pub struct SourceStats {
    pub gap_samples: u64,
    pub dropped_samples: u64,
    pub chunks: u64,
}

struct Source {
    /// interleaved stereo
    buf: VecDeque<f32>,
    /// sample index of the first buffered sample
    base: i64,
    /// where the previous chunk ended
    next: Option<i64>,
    gain: f32,
    stats: SourceStats,
}

pub struct Mixer {
    t0: Option<i64>,
    sources: Vec<Source>,
    next_out: i64,
}

impl Mixer {
    pub fn new(sources: usize) -> Self {
        Self {
            t0: None,
            sources: (0..sources)
                .map(|_| Source { buf: VecDeque::new(), base: 0, next: None, gain: 1.0, stats: SourceStats::default() })
                .collect(),
            next_out: 0,
        }
    }

    pub fn set_gain(&mut self, src: usize, gain: f32) {
        self.sources[src].gain = gain;
    }

    pub fn stats(&self, src: usize) -> SourceStats {
        self.sources[src].stats
    }

    fn to_idx(&self, t: i64) -> i64 {
        let t0 = self.t0.unwrap_or(t);
        (t - t0) * RATE / 10_000_000
    }

    fn to_time(&self, idx: i64) -> i64 {
        self.t0.unwrap_or(0) + idx * 10_000_000 / RATE
    }

    /// Add a chunk of interleaved stereo samples captured at `t` (100 ns units).
    pub fn push(&mut self, src: usize, t: i64, data: &[f32]) {
        if self.t0.is_none() {
            self.t0 = Some(t);
        }
        let idx = self.to_idx(t);
        let s = &mut self.sources[src];
        s.stats.chunks += 1;
        let n = (data.len() / 2) as i64;
        let expected = s.next.unwrap_or(idx);
        let mut place = if (idx - expected).abs() <= JITTER_TOL { expected } else { idx };
        let mut data = &data[..(n as usize) * 2];

        if s.buf.is_empty() && s.next.is_none() {
            s.base = place.max(0);
        }
        // late data for time that has already been emitted
        if place < s.base {
            let skip = (s.base - place).min(n);
            s.stats.dropped_samples += skip as u64;
            if skip >= n {
                s.next = Some(place + n);
                return;
            }
            data = &data[(skip as usize) * 2..];
            place = s.base;
        }
        let tail = s.base + (s.buf.len() / 2) as i64;
        if place > tail {
            let pad = (place - tail) as usize;
            s.buf.extend(std::iter::repeat(0.0).take(pad * 2));
            s.stats.gap_samples += pad as u64;
        } else if place < tail {
            let ov = ((tail - place) as usize).min(data.len() / 2);
            s.stats.dropped_samples += ov as u64;
            data = &data[ov * 2..];
            place = tail;
        }
        s.buf.extend(data.iter().copied());
        s.next = Some(place + (data.len() / 2) as i64);
    }

    /// Emit every frame that is old enough that late chunks no longer matter.
    /// Returns (timestamp in 100 ns, FRAME interleaved stereo samples).
    pub fn pull(&mut self, now: i64) -> Vec<(i64, Vec<f32>)> {
        let mut out = Vec::new();
        let Some(_) = self.t0 else { return out };
        let ready = self.to_idx(now - LATENCY_100NS);
        while self.next_out + FRAME as i64 <= ready {
            let i0 = self.next_out;
            let mut frame = vec![0f32; FRAME * 2];
            for s in &mut self.sources {
                for j in 0..FRAME as i64 * 2 {
                    let idx = i0 + j / 2;
                    if idx >= s.base {
                        let k = ((idx - s.base) * 2 + j % 2) as usize;
                        if let Some(v) = s.buf.get(k) {
                            frame[j as usize] += v * s.gain;
                        }
                    }
                }
                let end = i0 + FRAME as i64;
                if end > s.base {
                    let drop = (((end - s.base) * 2) as usize).min(s.buf.len());
                    s.buf.drain(..drop);
                    s.base = end;
                }
            }
            for v in &mut frame {
                *v = v.clamp(-1.0, 1.0);
            }
            out.push((self.to_time(i0), frame));
            self.next_out += FRAME as i64;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 10_000;

    fn chunk(v: f32, samples: usize) -> Vec<f32> {
        vec![v; samples * 2]
    }

    fn frames_at(m: &mut Mixer, now_ms: i64) -> Vec<(i64, Vec<f32>)> {
        m.pull(now_ms * MS)
    }

    #[test]
    fn two_sources_are_summed() {
        let mut m = Mixer::new(2);
        m.push(0, 0, &chunk(0.25, 4096));
        m.push(1, 0, &chunk(0.5, 4096));
        let f = frames_at(&mut m, 400);
        assert!(!f.is_empty());
        assert!(f[0].1.iter().all(|&v| (v - 0.75).abs() < 1e-6));
    }

    #[test]
    fn output_is_clamped() {
        let mut m = Mixer::new(2);
        m.push(0, 0, &chunk(0.9, 4096));
        m.push(1, 0, &chunk(0.9, 4096));
        let f = frames_at(&mut m, 400);
        assert!(f[0].1.iter().all(|&v| v == 1.0));
    }

    #[test]
    fn frames_wait_for_latency() {
        let mut m = Mixer::new(1);
        m.push(0, 0, &chunk(0.1, 4800));
        assert!(frames_at(&mut m, 50).is_empty());
        assert!(!frames_at(&mut m, 200).is_empty());
    }

    #[test]
    fn frame_timestamps_are_evenly_spaced() {
        let mut m = Mixer::new(1);
        m.push(0, 5_000_000, &chunk(0.1, 48_000));
        let f = frames_at(&mut m, 5_000_000 / MS + 900);
        assert!(f.len() > 10);
        for w in f.windows(2) {
            let d = w[1].0 - w[0].0;
            assert!((d - 213_333).abs() <= 1, "spacing {d}");
        }
        assert_eq!(f[0].0, 5_000_000);
    }

    #[test]
    fn silent_gap_is_zero_filled() {
        let mut m = Mixer::new(1);
        m.push(0, 0, &chunk(0.5, 1024)); // 21 ms of sound
        m.push(0, 500 * MS, &chunk(0.5, 1024)); // next one 500 ms later
        let f = frames_at(&mut m, 1500);
        // first frame is sound, the ones between are silence, one near 500 ms is sound again
        assert!(f[0].1.iter().all(|&v| v == 0.5));
        assert!(f[5].1.iter().all(|&v| v == 0.0));
        let loud = f.iter().position(|(t, fr)| *t >= 490 * MS && fr[0] > 0.0).unwrap();
        assert!(loud > 5);
        assert!(m.stats(0).gap_samples > 20_000);
    }

    #[test]
    fn timestamp_jitter_does_not_create_gaps() {
        let mut m = Mixer::new(1);
        let mut t = 0i64;
        for i in 0..100 {
            let jitter = if i % 2 == 0 { 4 * MS } else { -4 * MS };
            // 480 samples = 10 ms per chunk
            m.push(0, (t + jitter).max(0), &chunk(0.3, 480));
            t += 10 * MS;
        }
        assert_eq!(m.stats(0).gap_samples, 0);
        assert_eq!(m.stats(0).dropped_samples, 0);
    }

    #[test]
    fn big_forward_jump_is_a_gap() {
        let mut m = Mixer::new(1);
        m.push(0, 0, &chunk(0.3, 480));
        m.push(0, 200 * MS, &chunk(0.3, 480));
        assert!(m.stats(0).gap_samples > 8000);
    }

    #[test]
    fn late_data_is_dropped_not_replayed() {
        let mut m = Mixer::new(1);
        m.push(0, 0, &chunk(0.3, 48_000));
        let f = frames_at(&mut m, 2000);
        assert!(!f.is_empty());
        m.push(0, 100 * MS, &chunk(0.9, 480)); // old timestamp, already emitted
        assert!(m.stats(0).dropped_samples > 0);
    }

    #[test]
    fn missing_source_just_reads_as_silence() {
        let mut m = Mixer::new(2);
        m.push(0, 0, &chunk(0.4, 4096));
        let f = frames_at(&mut m, 400);
        assert!(f[0].1.iter().all(|&v| (v - 0.4).abs() < 1e-6));
    }

    #[test]
    fn gain_is_applied() {
        let mut m = Mixer::new(2);
        m.set_gain(1, 0.5);
        m.push(0, 0, &chunk(0.2, 4096));
        m.push(1, 0, &chunk(0.4, 4096));
        let f = frames_at(&mut m, 400);
        assert!(f[0].1.iter().all(|&v| (v - 0.4).abs() < 1e-6));
    }

    #[test]
    fn long_run_keeps_sample_count_steady() {
        // 60 s of 10 ms chunks must give almost exactly 60 s of frames
        let mut m = Mixer::new(1);
        let mut frames = 0usize;
        for i in 0..6000i64 {
            m.push(0, i * 10 * MS, &chunk(0.1, 480));
            frames += frames_at(&mut m, i * 10 + 10).len();
        }
        let expect = 60 * 48_000 / FRAME - 6;
        assert!((frames as i64 - expect as i64).abs() <= 3, "{frames} vs {expect}");
        assert_eq!(m.stats(0).gap_samples, 0);
    }
}
