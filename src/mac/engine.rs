//! Mac startup for the shared recorder core in `crate::engine`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub use crate::engine::{Engine, Finished, Msg, Saved, Shared};
use crate::config::Quality;

use super::capture::{Capture, CaptureCfg};
use super::util::log;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

pub struct EngineCfg {
    pub secs: u32,
    pub display_id: Option<u32>,
    pub mic_gain: f32,
    pub quality: Quality,
}

/// One capture session. Returns Ok when asked to stop, Err when capture was lost.
fn session(
    cfg: &CaptureCfg,
    q: Quality,
    mic_gain: f32,
    tx: &std::sync::mpsc::Sender<Msg>,
    shared: &Arc<Shared>,
    stop: &Arc<AtomicBool>,
    frames: &Arc<std::sync::atomic::AtomicU64>,
) -> Result<(), String> {
    let mut cap = Capture::start(cfg)?;
    let done = Arc::new(AtomicBool::new(false));
    let audio = cap.audio_rx.take().map(|rx| {
        let (tx, shared, stop, done) = (tx.clone(), shared.clone(), stop.clone(), done.clone());
        std::thread::spawn(move || super::audio::run(rx, tx, shared, stop, done, mic_gain))
    });
    let r = super::video::run(&cap, q, tx.clone(), shared.clone(), stop, frames);
    done.store(true, Ordering::Relaxed);
    cap.stop();
    if let Some(a) = audio {
        let _ = a.join();
    }
    r
}

impl Engine {
    pub fn start(cfg: EngineCfg) -> Res<Engine> {
        let (display_id, q, gain) = (cfg.display_id, cfg.quality, cfg.mic_gain);
        Ok(Engine::new(cfg.secs, cfg.quality, move |tx, shared, stop, frames| {
            let (max_w, max_h) = q.max_size();
            let ccfg = CaptureCfg { display_id, max_w: max_w as usize, max_h: max_h as usize, fps: q.fps() as i32, mic: true };
            vec![std::thread::spawn(move || {
                let mut last_err = String::new();
                while !stop.load(Ordering::Relaxed) {
                    match session(&ccfg, q, gain, &tx, &shared, &stop, &frames) {
                        Ok(()) => return,
                        Err(e) => {
                            if e != last_err {
                                log(&format!("capture error, restarting: {e}"));
                                last_err = e;
                            }
                            let _ = tx.send(Msg::Reset);
                            for _ in 0..30 {
                                if stop.load(Ordering::Relaxed) {
                                    return;
                                }
                                std::thread::sleep(Duration::from_millis(100));
                            }
                        }
                    }
                }
            })]
        }))
    }
}
