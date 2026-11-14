//! The always-on recorder, shared by every platform: video and audio threads feed one ring buffer, and
//! `save` turns the newest part of it into an MP4. A manual recording can also run
//! alongside, streaming the same packets straight to a file until it is stopped.
//!
//! The capture threads never touch the ring directly. They send packets to a
//! writer thread, so saving a clip (which holds the ring) cannot stall capture.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::Quality;
use crate::platform::{timestamp_name, videos_dir};
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
    /// Build the engine. `spawn` starts the platform's capture threads, which send
    /// encoded packets into the channel it is given.
    pub fn new<F>(secs: u32, quality: Quality, spawn: F) -> Engine
    where
        F: FnOnce(Sender<Msg>, Arc<Shared>, Arc<AtomicBool>, Arc<AtomicU64>) -> Vec<JoinHandle<()>>,
    {
        let capacity = (secs + EXTRA_SECS) as usize * quality.ring_bytes_per_sec();
        let core = Arc::new(Mutex::new(Core { ring: Ring::new(capacity, (secs + EXTRA_SECS) as f64), rec: None }));
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
        threads.extend(spawn(tx, shared.clone(), stop.clone(), frames.clone()));
        Engine { core, finished, shared, stop, threads: Mutex::new(threads), frames, secs, quality, ring_capacity: capacity }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::annexb_to_avcc;
    use crate::mp4::tests::{access_units, adts_frames, have, make_media, run, tmp};
    use std::process::Command;

    /// Packets decoded from generated media, ready to feed an engine.
    struct Media {
        video: Vec<(i64, bool, Vec<u8>)>,
        audio: Vec<(i64, Vec<u8>)>,
        params: VideoParams,
        audio_params: AudioParams,
    }

    fn media() -> Option<Media> {
        if !have("ffmpeg") || !have("ffprobe") {
            eprintln!("skipped: ffmpeg not installed");
            return None;
        }
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = tmp(&format!("engine-media-{}", N.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&dir).unwrap();
        let (vf, af) = make_media(&dir);
        let aus = access_units(&std::fs::read(&vf).unwrap());
        let (aframes, asc) = adts_frames(&std::fs::read(&af).unwrap());
        let (mut sps, mut pps) = (None, None);
        let mut video = Vec::new();
        for (i, au) in aus.iter().enumerate() {
            let mut out = Vec::new();
            let c = annexb_to_avcc(au, &mut out);
            if c.sps.is_some() { sps = c.sps; }
            if c.pps.is_some() { pps = c.pps; }
            video.push(((i as f64 / 30.0 * 1e7) as i64, c.key, out));
        }
        let audio = aframes.into_iter().enumerate().map(|(i, f)| ((i as f64 * 1024.0 / 48000.0 * 1e7) as i64, f)).collect();
        std::fs::remove_dir_all(&dir).ok();
        Some(Media {
            video,
            audio,
            params: VideoParams { width: 1280, height: 720, sps: sps.unwrap(), pps: pps.unwrap() },
            audio_params: AudioParams { sample_rate: 48000, channels: 2, asc, bitrate: 128_000 },
        })
    }

    /// Feed `frames` video frames (and the audio up to the same time) as fast as possible.
    fn feed(tx: &Sender<Msg>, m: &Media, from: usize, to: usize) {
        for i in from..to {
            let (pts, key, data) = &m.video[i];
            tx.send(Msg::Video { pts: *pts, key: *key, data: data.clone() }).unwrap();
            for (apts, a) in m.audio.iter().filter(|(p, _)| *p >= *pts && *p < *pts + 333_333) {
                tx.send(Msg::Audio { pts: *apts, data: a.clone() }).unwrap();
            }
        }
    }

    /// An engine whose "capture" is the test itself, through the returned sender.
    fn engine_with_feed(m: &Media, secs: u32) -> (Engine, Sender<Msg>) {
        let (tx_out, rx_out) = std::sync::mpsc::channel();
        let (params, aparams) = (m.params.clone(), m.audio_params.clone());
        let e = Engine::new(secs, Quality::P720x30, move |tx, shared, _stop, _frames| {
            *shared.video.lock().unwrap() = Some(params);
            *shared.audio.lock().unwrap() = Some(aparams);
            tx_out.send(tx).unwrap();
            Vec::new()
        });
        (e, rx_out.recv().unwrap())
    }

    fn wait_for<F: Fn() -> bool>(f: F) {
        for _ in 0..200 {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting");
    }

    fn decodes_cleanly(path: &std::path::Path) -> f64 {
        let dec = run(Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(path).args(["-f", "null", "-"]));
        assert!(dec.stderr.is_empty(), "decode errors: {}", String::from_utf8_lossy(&dec.stderr));
        let d = run(Command::new("ffprobe").args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"]).arg(path));
        String::from_utf8_lossy(&d.stdout).trim().parse().unwrap()
    }

    static HOME_LOCK: Mutex<()> = Mutex::new(());

    fn with_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tmp("engine-home");
        std::fs::create_dir_all(&home).unwrap();
        let old = std::env::var("HOME").ok();
        std::env::set_var("HOME", &home);
        let r = f(&home);
        match old {
            Some(o) => std::env::set_var("HOME", o),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(&home).ok();
        r
    }

    #[test]
    fn saving_a_clip_of_the_requested_length() {
        let Some(m) = media() else { return };
        with_home(|home| {
            let (e, tx) = engine_with_feed(&m, 4);
            feed(&tx, &m, 0, 150); // 5 s of media
            wait_for(|| e.buffer_status().0 >= 3.0);
            let saved = e.save_secs(2).expect("save");
            assert!(saved.path.starts_with(home));
            let dur = decodes_cleanly(&saved.path);
            assert!((2.0..=3.2).contains(&dur), "asked for 2 s, got {dur}");
            // an oversized request just gives everything buffered
            let all = e.save_secs(120).expect("save");
            assert!(decodes_cleanly(&all.path) > dur);
            e.stop();
        });
    }

    #[test]
    fn saving_with_nothing_buffered_is_a_clear_error_not_a_crash() {
        let Some(m) = media() else { return };
        with_home(|_| {
            let (e, _tx) = engine_with_feed(&m, 4);
            assert!(e.save().is_err());
            assert!(e.stop_recording().is_err());
            e.stop();
        });
    }

    #[test]
    fn manual_recording_includes_pre_roll_and_everything_after() {
        let Some(m) = media() else { return };
        with_home(|_| {
            let (e, tx) = engine_with_feed(&m, 4);
            feed(&tx, &m, 0, 40); // 1.3 s buffered before the press
            wait_for(|| e.buffer_status().0 > 1.0);
            let path = e.start_recording().expect("start");
            assert!(e.recording_elapsed().is_some());
            assert!(e.start_recording().is_err(), "second start must be refused");
            feed(&tx, &m, 40, 170);
            wait_for(|| e.buffer_status().0 > 4.0);
            let f = e.stop_recording().expect("stop");
            assert_eq!(f.path, path);
            assert!(e.recording_elapsed().is_none());
            let dur = decodes_cleanly(&f.path);
            // pre-roll from the keyframe at 1.0 s through the end of 170 frames (5.67 s)
            assert!((4.0..=5.0).contains(&dur), "duration {dur}");
            e.stop();
        });
    }

    #[test]
    fn a_screen_change_ends_the_recording_safely_and_keeps_the_file() {
        let Some(m) = media() else { return };
        with_home(|_| {
            let (e, tx) = engine_with_feed(&m, 4);
            feed(&tx, &m, 0, 70);
            wait_for(|| e.buffer_status().0 > 2.0);
            e.start_recording().expect("start");
            feed(&tx, &m, 70, 120);
            tx.send(Msg::Reset).unwrap();
            wait_for(|| e.recording_elapsed().is_none());
            let f = e.take_finished().expect("a finished recording").expect("saved ok");
            assert!(f.note.contains("screen changed"), "{}", f.note);
            assert!(decodes_cleanly(&f.path) > 1.0);
            assert!(e.take_finished().is_none());
            e.stop();
        });
    }

    #[test]
    fn stopping_the_engine_finishes_a_recording_in_progress() {
        let Some(m) = media() else { return };
        with_home(|_| {
            let (e, tx) = engine_with_feed(&m, 4);
            feed(&tx, &m, 0, 70);
            wait_for(|| e.buffer_status().0 > 2.0);
            e.start_recording().expect("start");
            feed(&tx, &m, 70, 110);
            wait_for(|| e.buffer_status().0 > 3.0);
            e.stop();
            let f = e.take_finished().expect("finished on stop").expect("ok");
            assert!(decodes_cleanly(&f.path) > 1.0);
        });
    }
}
