//! Mac spike B: desktop audio and microphone through ScreenCaptureKit, mixed and encoded to AAC,
//! using the same code the app uses. Writes mix.aac, which plays in QuickTime Player.
//!
//! Usage: mac_spike_b [seconds] [mic volume, default 1.0]

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("mac_spike_b only runs on macOS");
}

#[cfg(target_os = "macos")]
fn main() {
    imp::run();
}

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::channel;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use clip_a_pc::engine::{Msg, Shared};
    use clip_a_pc::mac::capture::{Capture, CaptureCfg, Chunk, DESKTOP, MIC};
    use clip_a_pc::mac::util::memory_mb;
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSApplication;

    pub fn run() {
        let args: Vec<String> = std::env::args().collect();
        let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(15);
        let gain: f32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1.0);
        if let Err(e) = real_main(secs, gain) {
            eprintln!("\nFAILED: {e}");
            std::process::exit(1);
        }
    }

    /// ADTS header for AAC-LC, 48 kHz, stereo.
    fn adts(len: usize) -> [u8; 7] {
        let l = len + 7;
        [0xFF, 0xF1, (1 << 6) | (3 << 2), (2 << 6) | ((l >> 11) & 3) as u8, ((l >> 3) & 0xFF) as u8, (((l & 7) << 5) | 0x1F) as u8, 0xFC]
    }

    fn real_main(secs: u64, gain: f32) -> Result<(), String> {
        let mtm = MainThreadMarker::new().ok_or("must run on the main thread")?;
        let _app = NSApplication::sharedApplication(mtm);
        println!("memory at start: {:.0} MB", memory_mb());

        let mut cap = Capture::start(&CaptureCfg { display_id: None, max_w: 1280, max_h: 720, fps: 30, mic: true })?;
        println!("capture started ({}x{}). Play some audio and talk for {secs} s.", cap.width, cap.height);
        println!("If the mic never shows a level, allow Terminal under Privacy & Security, Microphone.\n");

        // tee the audio: count and measure it here, then pass it on to the real audio code
        let rx = cap.audio_rx.take().ok_or("no audio channel")?;
        let (fwd_tx, fwd_rx) = channel::<Chunk>();
        let frames = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
        let peaks = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
        {
            let (frames, peaks) = (frames.clone(), peaks.clone());
            std::thread::spawn(move || {
                for c in rx {
                    frames[c.src].fetch_add(c.samples.len() as u64 / 2, Ordering::Relaxed);
                    let p = c.samples.iter().fold(0f32, |m, v| m.max(v.abs()));
                    peaks[c.src].fetch_max((p * 10_000.0) as u64, Ordering::Relaxed);
                    if fwd_tx.send(c).is_err() {
                        break;
                    }
                }
            });
        }
        let (tx, packets) = channel::<Msg>();
        let shared = Arc::new(Shared::default());
        let (stop, done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let worker = {
            let (shared, stop, done) = (shared.clone(), stop.clone(), done.clone());
            std::thread::spawn(move || clip_a_pc::mac::audio::run(fwd_rx, tx, shared, stop, done, gain))
        };

        let start = Instant::now();
        let mut last = [0u64; 2];
        let mut peak_all = [0f64; 2];
        while start.elapsed() < Duration::from_secs(secs) {
            std::thread::sleep(Duration::from_secs(1));
            let f = [frames[DESKTOP].load(Ordering::Relaxed), frames[MIC].load(Ordering::Relaxed)];
            let p = [peaks[DESKTOP].swap(0, Ordering::Relaxed) as f64 / 10_000.0, peaks[MIC].swap(0, Ordering::Relaxed) as f64 / 10_000.0];
            for i in 0..2 {
                peak_all[i] = peak_all[i].max(p[i]);
            }
            println!(
                "t={:>2}s  desktop: {:>6} samples/s peak {:.2} | mic: {:>6} samples/s peak {:.2} | memory {:.0} MB",
                start.elapsed().as_secs(),
                f[0] - last[0],
                p[0],
                f[1] - last[1],
                p[1],
                memory_mb()
            );
            last = f;
        }
        done.store(true, Ordering::Relaxed);
        stop.store(true, Ordering::Relaxed);
        cap.stop();
        let _ = worker.join();

        let mut aac = Vec::new();
        let (mut n, mut bytes) = (0usize, 0usize);
        while let Ok(m) = packets.try_recv() {
            if let Msg::Audio { data, .. } = m {
                aac.extend_from_slice(&adts(data.len()));
                aac.extend_from_slice(&data);
                n += 1;
                bytes += data.len();
            }
        }
        std::fs::write("mix.aac", aac).map_err(|e| e.to_string())?;
        println!("\n--- result ---");
        println!("AAC packets {n} ({:.1} s of audio), {:.0} kbps", n as f64 * 1024.0 / 48000.0, bytes as f64 * 8.0 / secs as f64 / 1000.0);
        println!("loudest desktop {:.2}, loudest mic {:.2}", peak_all[0], peak_all[1]);
        println!("wrote mix.aac");
        Ok(())
    }
}
