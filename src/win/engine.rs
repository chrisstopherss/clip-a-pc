//! Windows startup for the shared recorder core in `crate::engine`.

use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

pub use crate::engine::{Engine, Finished, Msg, Saved, Shared};
use crate::config::Quality;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

pub struct EngineCfg {
    pub secs: u32,
    pub monitor: usize,
    pub mic_gain: f32,
    pub quality: Quality,
}

impl Engine {
    pub fn start(cfg: EngineCfg) -> Res<Engine> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            timeBeginPeriod(1);
        }
        let (monitor, quality, gain) = (cfg.monitor, cfg.quality, cfg.mic_gain);
        Ok(Engine::new(cfg.secs, cfg.quality, move |tx, shared, stop, frames| {
            let mut threads = Vec::new();
            {
                let (tx, shared, stop, frames) = (tx.clone(), shared.clone(), stop.clone(), frames.clone());
                threads.push(std::thread::spawn(move || {
                    unsafe {
                        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                    }
                    if let Err(e) = super::video::run(monitor, quality, tx, shared, stop, frames) {
                        super::log::log(&format!("video capture stopped for good: {e}"));
                    }
                }));
            }
            {
                let (tx, shared, stop) = (tx, shared.clone(), stop.clone());
                threads.push(std::thread::spawn(move || {
                    if let Err(e) = super::audio::run(tx, shared, stop, gain) {
                        super::log::log(&format!("audio capture stopped for good: {e}"));
                    }
                }));
            }
            threads
        }))
    }
}
