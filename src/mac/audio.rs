//! Desktop audio and microphone, mixed on one clock and encoded to AAC.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use super::aac::{Aac, ASC};
use super::capture::{Chunk, MIC};
use super::util::{log, now_100ns};
use crate::engine::{Msg, Shared};
use crate::mixer::{Mixer, FRAME, RATE};
use crate::mp4::AudioParams;

const BITRATE: u32 = 128_000;

/// Runs until `stop` or `done` is set. `rx` carries audio from the capture stream.
pub fn run(rx: Receiver<Chunk>, tx: Sender<Msg>, shared: Arc<Shared>, stop: Arc<AtomicBool>, done: Arc<AtomicBool>, mic_gain: f32) {
    let mut aac = match Aac::new(BITRATE) {
        Ok(a) => a,
        Err(e) => {
            log(&format!("audio disabled: {e}"));
            return;
        }
    };
    *shared.audio.lock().unwrap() = Some(AudioParams { sample_rate: RATE as u32, channels: 2, asc: ASC.to_vec(), bitrate: BITRATE });
    *shared.audio_info.lock().unwrap() = "desktop audio and microphone from ScreenCaptureKit".to_string();

    let mut mixer = Mixer::new(2);
    mixer.set_gain(MIC, mic_gain);
    let mut first_pts: Option<i64> = None;
    let mut out_frames = 0u64;
    let mut seen = [false; 2];

    while !stop.load(Ordering::Relaxed) && !done.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(c) => {
                if !seen[c.src] {
                    seen[c.src] = true;
                    log(&format!("audio is arriving from the {}", if c.src == MIC { "microphone" } else { "desktop" }));
                }
                mixer.push(c.src, c.t_100ns, &c.samples);
                while let Ok(c) = rx.try_recv() {
                    mixer.push(c.src, c.t_100ns, &c.samples);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        for (pts, frame) in mixer.pull(now_100ns()) {
            let base = *first_pts.get_or_insert(pts);
            match aac.encode(&frame) {
                Ok(packets) => {
                    for data in packets {
                        let pts_k = base + out_frames as i64 * FRAME as i64 * 10_000_000 / RATE;
                        out_frames += 1;
                        let _ = tx.send(Msg::Audio { pts: pts_k, data });
                    }
                }
                Err(e) => {
                    log(&format!("audio encoding stopped: {e}"));
                    return;
                }
            }
        }
    }
}
