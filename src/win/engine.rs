//! The always-on recorder: video and audio threads feed one ring buffer, and
//! `save` turns the newest part of it into an MP4. A manual recording can also run
//! alongside, streaming the same packets straight to a file until it is stopped.
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
use std::time::{Duration, Instant};

use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::util::{timestamp_name, videos_dir};
use crate::config::Quality;
use crate::livemp4::LiveMp4;
use crate::mp4::{write_mp4, AudioParams, VideoParams};
use crate::ring::{Entry, Ring, Track};

type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// A clip starts on a keyframe (one per second), so the buffer keeps this much extra.
const EXTRA_SECS: u32 = 2;
/// A manual recording stops by itself after this long, so its index cannot grow without limit.
const MAX_RECORDING: Duration = Duration::from_secs(2 * 3600);

pub enum Msg {
    Video { pts: i64, key: bool, data: Vec<u8> },
    Audio { pts: i64, data: Vec<u8> },
    /// The picture source was rebuilt: drop everything buffered so far.
    Reset,
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
    pub quality: Quality,
}

pub struct Saved {
    pub path: PathBuf,
    pub seconds: f64,
    pub bytes: u64,
}

struct Recorder {
    live: LiveMp4<BufWriter<File>>,
    part: PathBuf,
    fin: PathBuf,
    started: Instant,
    video: VideoParams,
    audio: Option<AudioParams>,
}

/// The ring and the recording in progress share one lock, so a packet is never
/// in one and missing from the other.
struct Core {
    ring: Ring,
    rec: Option<Recorder>,
}

/// A recording that has been closed up.
pub struct Finished {
    pub path: PathBuf,
    pub seconds: f64,
    pub bytes: u64,
    /// Why it ended, when it was not the user stopping it. Empty otherwise.
    pub note: String,
}

fn finalize(rec: Recorder, note: &str) -> Result<Finished, String> {
    let Recorder { live, part, fin, video, audio, .. } = rec;
    let seconds = live.seconds();
    live.finish(&video, audio.as_ref()).map_err(|e| format!("could not finish the recording: {e}"))?;
    std::fs::rename(&part, &fin).map_err(|e| e.to_string())?;
    let bytes = std::fs::metadata(&fin).map(|m| m.len()).unwrap_or(0);
    Ok(Finished { path: fin, seconds, bytes, note: note.to_string() })
}

pub struct Engine {
    core: Arc<Mutex<Core>>,
    /// Set when a recording ended on its own (screen change, disk error, time limit).
    finished: Arc<Mutex<Option<Result<Finished, String>>>>,
    pub shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    pub frames: Arc<AtomicU64>,
    pub secs: u32,
    pub quality: Quality,
    pub ring_capacity: usize,
}

impl Engine {
    pub fn start(cfg: EngineCfg) -> Res<Engine> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            timeBeginPeriod(1);
        }
        let capacity = (cfg.secs + EXTRA_SECS) as usize * cfg.quality.ring_bytes_per_sec();
        let core = Arc::new(Mutex::new(Core { ring: Ring::new(capacity, (cfg.secs + EXTRA_SECS) as f64), rec: None }));
        let finished: Arc<Mutex<Option<Result<Finished, String>>>> = Arc::new(Mutex::new(None));
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let frames = Arc::new(AtomicU64::new(0));
        let (tx, rx) = channel::<Msg>();
        let mut threads = Vec::new();

        {
            let (core, stop, shared, finished) = (core.clone(), stop.clone(), shared.clone(), finished.clone());
            threads.push(std::thread::spawn(move || loop {
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(m) => {
                        let mut c = core.lock().unwrap();
                        let mut end: Option<String> = None;
                        match m {
                            Msg::Video { pts, key, data } => {
                                c.ring.push(Track::Video, pts, key, &data);
                                if let Some(r) = c.rec.as_mut() {
                                    if let Err(e) = r.live.add(Track::Video, pts, key, &data) {
                                        end = Some(format!("disk error: {e}"));
                                    }
                                }
                            }
                            Msg::Audio { pts, data } => {
                                c.ring.push(Track::Audio, pts, false, &data);
                                if let Some(r) = c.rec.as_mut() {
                                    if let Err(e) = r.live.add(Track::Audio, pts, false, &data) {
                                        end = Some(format!("disk error: {e}"));
                                    }
                                }
                            }
                            Msg::Reset => {
                                c.ring.clear();
                                *shared.video.lock().unwrap() = None;
                                if c.rec.is_some() {
                                    end = Some("the screen changed".into());
                                }
                            }
                        }
                        if end.is_none() && c.rec.as_ref().map(|r| r.started.elapsed() > MAX_RECORDING).unwrap_or(false) {
                            end = Some("it reached the 2 hour limit".into());
                        }
                        if let Some(why) = end {
                            if let Some(rec) = c.rec.take() {
                                *finished.lock().unwrap() = Some(finalize(rec, &why));
                            }
                        }
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
            let (monitor, quality) = (cfg.monitor, cfg.quality);
            threads.push(std::thread::spawn(move || {
                unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
                if let Err(e) = super::video::run(monitor, quality, tx, shared, stop, frames) {
                    super::log::log(&format!("video capture stopped for good: {e}"));
                }
            }));
        }
        {
            let (tx, shared, stop) = (tx, shared.clone(), stop.clone());
            let gain = cfg.mic_gain;
            threads.push(std::thread::spawn(move || {
                if let Err(e) = super::audio::run(tx, shared, stop, gain) {
                    super::log::log(&format!("audio capture stopped for good: {e}"));
                }
            }));
        }
        Ok(Engine { core, finished, shared, stop, threads: Mutex::new(threads), frames, secs: cfg.secs, quality: cfg.quality, ring_capacity: capacity })
    }

    /// (buffered seconds of video, bytes used)
    pub fn buffer_status(&self) -> (f64, usize) {
        let c = self.core.lock().unwrap();
        (c.ring.span_secs(), c.ring.bytes_used())
    }

    /// Save the clip length chosen in settings.
    pub fn save(&self) -> Res<Saved> {
        self.save_secs(self.secs)
    }

    /// Save the last `secs` seconds (or what is buffered) to the Videos folder.
    /// The buffer only holds the clip length from settings, so asking for more gets that much.
    pub fn save_secs(&self, secs: u32) -> Res<Saved> {
        let video = self.shared.video.lock().unwrap().clone().ok_or("nothing recorded yet")?;
        let audio = self.shared.audio.lock().unwrap().clone();
        let dir = videos_dir().join("clip-a-pc");
        std::fs::create_dir_all(&dir)?;
        let name = format!("clip-{}.mp4", timestamp_name());
        let part = dir.join(format!("{name}.part"));
        let fin = dir.join(&name);

        let seconds;
        {
            let core = self.core.lock().unwrap();
            let ring = &core.ring;
            let clip = ring.select(secs as f64).ok_or("no keyframe buffered yet, try again in a second")?;
            let last = clip.video.last().map(|e| e.pts).unwrap_or(clip.start_pts);
            seconds = (last - clip.start_pts) as f64 / 1e7 + 1.0 / self.quality.fps() as f64;
            let mut w = BufWriter::with_capacity(1 << 20, File::create(&part)?);
            write_mp4(&mut w, ring, &clip, &video, audio.as_ref())?;
            w.flush()?;
        }
        std::fs::rename(&part, &fin)?;
        let bytes = std::fs::metadata(&fin)?.len();
        Ok(Saved { path: fin, seconds, bytes })
    }

    /// Start streaming to a new file. It begins at the most recent keyframe, so
    /// at most one second before the moment of the call is included.
    pub fn start_recording(&self) -> Res<PathBuf> {
        let video = self.shared.video.lock().unwrap().clone().ok_or("the screen is not being captured yet")?;
        let audio = self.shared.audio.lock().unwrap().clone();
        let dir = videos_dir().join("clip-a-pc");
        std::fs::create_dir_all(&dir)?;
        let name = format!("recording-{}.mp4", timestamp_name());
        let part = dir.join(format!("{name}.part"));
        let fin = dir.join(&name);

        let mut core = self.core.lock().unwrap();
        if core.rec.is_some() {
            return Err("already recording".into());
        }
        let clip = core.ring.select(0.0).ok_or("no keyframe buffered yet, try again in a second")?;
        let mut live = LiveMp4::new(BufWriter::with_capacity(1 << 20, File::create(&part)?))?;
        // the packets since that keyframe are already in the ring: copy them in first
        let mut pre: Vec<Entry> = clip.video.iter().chain(clip.audio.iter()).copied().collect();
        pre.sort_by_key(|e| (e.pts, e.track == Track::Audio));
        let mut buf = Vec::new();
        for e in &pre {
            buf.clear();
            core.ring.write_entry(e, &mut buf)?;
            live.add(e.track, e.pts, e.key, &buf)?;
        }
        *self.finished.lock().unwrap() = None;
        core.rec = Some(Recorder { live, part, fin: fin.clone(), started: Instant::now(), video, audio });
        Ok(fin)
    }

    /// Finish the recording in progress and return the finished file.
    pub fn stop_recording(&self) -> Res<Finished> {
        let rec = self.core.lock().unwrap().rec.take();
        match rec {
            Some(r) => Ok(finalize(r, "")?),
            None => Err("not recording".into()),
        }
    }

    pub fn recording_elapsed(&self) -> Option<Duration> {
        self.core.lock().unwrap().rec.as_ref().map(|r| r.started.elapsed())
    }

    /// A recording that ended by itself since the last call, if any.
    pub fn take_finished(&self) -> Option<Result<Finished, String>> {
        self.finished.lock().unwrap().take()
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let threads: Vec<_> = self.threads.lock().unwrap().drain(..).collect();
        for t in threads {
            let _ = t.join();
        }
        // closing the app must not lose a recording in progress
        let rec = self.core.lock().unwrap().rec.take();
        if let Some(r) = rec {
            *self.finished.lock().unwrap() = Some(finalize(r, "clip-a-pc was closed or restarted"));
        }
    }
}
