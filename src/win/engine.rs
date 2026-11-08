//! The always-on recorder: video and audio threads feed one ring buffer, and
//! `save` turns the newest part of it into an MP4.
//!
//! The capture threads never touch the ring directly. They send packets to a
//! writer thread, so saving a clip (which holds the ring) cannot stall capture.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::util::{timestamp_name, videos_dir};
use crate::mp4::{write_mp4, AudioParams, VideoParams};
use crate::ring::{Ring, Track};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// Ring bytes per second of buffer: a bit above the 5 Mbps average video rate.
const RING_BYTES_PER_SEC: usize = 800_000;
/// A clip starts on a keyframe (one per second), so the buffer keeps this much extra.
const EXTRA_SECS: u32 = 2;

pub enum Msg {
    Video { pts: i64, key: bool, data: Vec<u8> },
    Audio { pts: i64, data: Vec<u8> },
}

#[derive(Default)]
pub struct Shared {
    pub video: Mutex<Option<VideoParams>>,
    pub audio: Mutex<Option<AudioParams>>,
    pub video_info: Mutex<String>,
    pub audio_info: Mutex<String>,
    pub dropped_video: AtomicU64,
}

pub struct EngineCfg {
    pub secs: u32,
    pub monitor: usize,
    pub mic_gain: f32,
}

pub struct Saved {
    pub path: PathBuf,
    pub seconds: f64,
    pub bytes: u64,
}

pub struct Engine {
    ring: Arc<Mutex<Ring>>,
    pub shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    pub frames: Arc<AtomicU64>,
    pub secs: u32,
    pub ring_capacity: usize,
}

impl Engine {
    pub fn start(cfg: EngineCfg) -> Res<Engine> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            timeBeginPeriod(1);
        }
        let capacity = (cfg.secs + EXTRA_SECS) as usize * RING_BYTES_PER_SEC;
        let ring = Arc::new(Mutex::new(Ring::new(capacity, (cfg.secs + EXTRA_SECS) as f64)));
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let (tx, rx) = channel::<Msg>();
        let mut threads = Vec::new();

        {
            let (ring, stop) = (ring.clone(), stop.clone());
            threads.push(std::thread::spawn(move || loop {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(m) => {
                        let mut r = ring.lock().unwrap();
                        match m {
                            Msg::Video { pts, key, data } => r.push(Track::Video, pts, key, &data),
                            Msg::Audio { pts, data } => r.push(Track::Audio, pts, false, &data),
                        };
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }));
        }
        {
            let (tx, shared, stop, frames) = (tx.clone(), shared.clone(), stop.clone(), frames.clone());
            let monitor = cfg.monitor;
            threads.push(std::thread::spawn(move || {
                unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
                if let Err(e) = super::video::run(monitor, tx, shared, stop, frames) {
                    eprintln!("video capture stopped: {e}");
                }
            }));
        }
        {
            let (tx, shared, stop) = (tx, shared.clone(), stop.clone());
            let gain = cfg.mic_gain;
            threads.push(std::thread::spawn(move || {
                if let Err(e) = super::audio::run(tx, shared, stop, gain) {
                    eprintln!("audio capture stopped: {e}");
                }
            }));
        }
        Ok(Engine { ring, shared, stop, threads, frames, secs: cfg.secs, ring_capacity: capacity })
    }

    /// (buffered seconds of video, bytes used)
    pub fn buffer_status(&self) -> (f64, usize) {
        let r = self.ring.lock().unwrap();
        (r.span_secs(), r.bytes_used())
    }

    /// Save the last `secs` seconds (or what is buffered) to the Videos folder.
    pub fn save(&self) -> Res<Saved> {
        let video = self.shared.video.lock().unwrap().clone().ok_or("nothing recorded yet")?;
        let audio = self.shared.audio.lock().unwrap().clone();
        let dir = videos_dir().join("clip-a-pc");
        std::fs::create_dir_all(&dir)?;
        let name = format!("clip-{}.mp4", timestamp_name());
        let part = dir.join(format!("{name}.part"));
        let fin = dir.join(&name);

        let seconds;
        {
            let ring = self.ring.lock().unwrap();
            let clip = ring.select(self.secs as f64).ok_or("no keyframe buffered yet, try again in a second")?;
            let last = clip.video.last().map(|e| e.pts).unwrap_or(clip.start_pts);
            seconds = (last - clip.start_pts) as f64 / 1e7 + 1.0 / 30.0;
            let mut w = BufWriter::with_capacity(1 << 20, File::create(&part)?);
            write_mp4(&mut w, &ring, &clip, &video, audio.as_ref())?;
            w.flush()?;
        }
        std::fs::rename(&part, &fin)?;
        let bytes = std::fs::metadata(&fin)?.len();
        Ok(Saved { path: fin, seconds, bytes })
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
    }
}
