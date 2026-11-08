//! Fixed-size ring of already-encoded video and audio packets.
//!
//! One preallocated byte buffer holds every packet back to back, so memory use
//! is constant and nothing is allocated while recording. The oldest packets are
//! overwritten when the buffer is full or older than the keep time.

use std::collections::VecDeque;
use std::io::{self, Write};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Track {
    Video,
    Audio,
}

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub track: Track,
    /// Presentation time in 100 ns units.
    pub pts: i64,
    pub key: bool,
    pub len: usize,
    off: u64,
}

pub struct Ring {
    buf: Vec<u8>,
    entries: VecDeque<Entry>,
    tail: u64,
    keep: i64,
}

/// The packets that make up one saved clip, oldest first.
pub struct Clip {
    pub video: Vec<Entry>,
    pub audio: Vec<Entry>,
    /// Time of the first video keyframe, which is time zero of the clip.
    pub start_pts: i64,
}

impl Ring {
    pub fn new(capacity_bytes: usize, keep_secs: f64) -> Self {
        Self {
            buf: vec![0; capacity_bytes],
            entries: VecDeque::new(),
            tail: 0,
            keep: (keep_secs * 1e7) as i64,
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn bytes_used(&self) -> usize {
        match self.entries.front() {
            Some(f) => (self.tail - f.off) as usize,
            None => 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Add a packet. Returns false if it can never fit.
    pub fn push(&mut self, track: Track, pts: i64, key: bool, data: &[u8]) -> bool {
        let cap = self.buf.len();
        if data.len() > cap {
            return false;
        }
        while let Some(f) = self.entries.front() {
            let too_old = pts - f.pts > self.keep;
            let too_big = (self.tail - f.off) as usize + data.len() > cap;
            if too_old || too_big {
                self.entries.pop_front();
            } else {
                break;
            }
        }
        let pos = (self.tail % cap as u64) as usize;
        let first = data.len().min(cap - pos);
        self.buf[pos..pos + first].copy_from_slice(&data[..first]);
        self.buf[..data.len() - first].copy_from_slice(&data[first..]);
        self.entries.push_back(Entry { track, pts, key, len: data.len(), off: self.tail });
        self.tail += data.len() as u64;
        true
    }

    /// Write the bytes of one entry to `w`, straight from the ring.
    pub fn write_entry<W: Write>(&self, e: &Entry, w: &mut W) -> io::Result<()> {
        let cap = self.buf.len();
        let pos = (e.off % cap as u64) as usize;
        let first = e.len.min(cap - pos);
        w.write_all(&self.buf[pos..pos + first])?;
        if first < e.len {
            w.write_all(&self.buf[..e.len - first])?;
        }
        Ok(())
    }

    /// Pick the packets for a clip of at least `secs` seconds ending now. The clip
    /// starts at the last video keyframe at or before that point, so it can run up
    /// to one keyframe interval longer. If the buffer holds less than `secs`, the
    /// clip starts at its oldest keyframe.
    pub fn select(&self, secs: f64) -> Option<Clip> {
        let newest = self.entries.iter().rev().find(|e| e.track == Track::Video)?.pts;
        let target = newest - (secs * 1e7) as i64;
        let mut start_idx = None;
        for (i, e) in self.entries.iter().enumerate() {
            if e.track == Track::Video && e.key {
                if start_idx.is_none() || e.pts <= target {
                    start_idx = Some(i);
                }
                if e.pts > target {
                    break;
                }
            }
        }
        let start_idx = start_idx?;
        let start_pts = self.entries[start_idx].pts;
        let mut clip = Clip { video: Vec::new(), audio: Vec::new(), start_pts };
        for (i, e) in self.entries.iter().enumerate() {
            match e.track {
                Track::Video if i >= start_idx => clip.video.push(*e),
                Track::Audio if e.pts >= start_pts => clip.audio.push(*e),
                _ => {}
            }
        }
        Some(clip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 10_000_000;

    fn fill(r: &mut Ring, seconds: i64, bytes: usize) {
        for i in 0..seconds * 30 {
            let key = i % 30 == 0;
            let data = vec![(i % 251) as u8; bytes];
            assert!(r.push(Track::Video, i * S / 30, key, &data));
        }
    }

    #[test]
    fn data_survives_wraparound() {
        let mut r = Ring::new(1000, 100.0);
        for i in 0..50u8 {
            r.push(Track::Video, i as i64 * 1000, i % 10 == 0, &vec![i; 70]);
        }
        let last = *r.entries.back().unwrap();
        let mut out = Vec::new();
        r.write_entry(&last, &mut out).unwrap();
        assert_eq!(out, vec![49u8; 70]);
        assert!(r.bytes_used() <= 1000);
    }

    #[test]
    fn old_packets_are_dropped_by_time() {
        let mut r = Ring::new(1 << 20, 5.0);
        fill(&mut r, 20, 100);
        let first = r.entries.front().unwrap().pts;
        assert!(first >= 15 * S - S / 30 - 1);
    }

    #[test]
    fn clip_starts_at_keyframe_and_is_long_enough() {
        let mut r = Ring::new(1 << 20, 100.0);
        fill(&mut r, 10, 100);
        let c = r.select(3.2).unwrap();
        let newest = r.entries.back().unwrap().pts;
        assert!(c.video[0].key);
        assert!((newest - c.start_pts) as f64 / 1e7 >= 3.2);
        assert!((newest - c.start_pts) as f64 / 1e7 < 3.2 + 1.0 + 0.04);
    }

    #[test]
    fn short_buffer_uses_what_it_has() {
        let mut r = Ring::new(1 << 20, 100.0);
        fill(&mut r, 2, 100);
        let c = r.select(60.0).unwrap();
        assert_eq!(c.start_pts, 0);
        assert_eq!(c.video.len(), 60);
    }

    #[test]
    fn no_keyframe_means_no_clip() {
        let mut r = Ring::new(1000, 100.0);
        r.push(Track::Video, 0, false, &[1, 2, 3]);
        assert!(r.select(1.0).is_none());
    }
}
