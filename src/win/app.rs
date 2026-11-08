//! Console front end for the recorder: start it, press Alt+C to save a clip.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_NOREPEAT};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

use super::engine::{Engine, EngineCfg};
use super::util::{cpu_seconds, memory_mb};
use super::video::list_monitors;
use crate::config::{nearest_duration, DEFAULT_DURATION_SECS};

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn save_and_report(engine: &Engine) {
    let (p0, w0) = memory_mb();
    let t = Instant::now();
    match engine.save() {
        Ok(s) => {
            let (p1, w1) = memory_mb();
            println!(
                "\nCLIP SAVED  {}\n  {:.1} s, {:.1} MB, written in {} ms\n  memory: private {p0:.0} -> {p1:.0} MB, working set {w0:.0} -> {w1:.0} MB\n",
                s.path.display(),
                s.seconds,
                s.bytes as f64 / 1048576.0,
                t.elapsed().as_millis()
            );
        }
        Err(e) => println!("\nsave failed: {e}\n"),
    }
}

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--list") {
        for (i, m) in list_monitors().iter().enumerate() {
            println!("monitor {i}: {}x{} {}{}", m.width, m.height, m.device_name, if m.primary { " (primary)" } else { "" });
        }
        return;
    }
    let secs = nearest_duration(arg_value(&args, "--secs").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_DURATION_SECS));
    let monitor = arg_value(&args, "--monitor").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mic_gain = arg_value(&args, "--mic-gain").and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let autosave: Option<u64> = arg_value(&args, "--autosave").and_then(|v| v.parse().ok());

    let (p, w) = memory_mb();
    println!("clip-a-pc starting. clip length {secs} s, monitor {monitor}, mic gain {mic_gain}x");
    println!("memory at start: private {p:.0} MB, working set {w:.0} MB");
    let engine = match Engine::start(EngineCfg { secs, monitor, mic_gain }) {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("could not start: {e}");
            return;
        }
    };

    std::thread::sleep(Duration::from_millis(1500));
    println!("video: {}", engine.shared.video_info.lock().unwrap());
    println!("audio: {}", engine.shared.audio_info.lock().unwrap());
    println!("ring buffer: {:.0} MB for {} s (+{} s so a clip can start on a keyframe)", engine.ring_capacity as f64 / 1048576.0, secs, 2);

    // status line every 10 s
    {
        let engine = engine.clone();
        std::thread::spawn(move || {
            let (mut last_cpu, mut last_t) = (cpu_seconds(), Instant::now());
            loop {
                std::thread::sleep(Duration::from_secs(10));
                let (span, used) = engine.buffer_status();
                let (p, w) = memory_mb();
                let cpu = cpu_seconds();
                let pct = (cpu - last_cpu) / last_t.elapsed().as_secs_f64() * 100.0;
                last_cpu = cpu;
                last_t = Instant::now();
                println!(
                    "[status] buffered {span:.0} s ({:.0} MB) | memory private {p:.0} MB, working set {w:.0} MB | cpu {pct:.1}% of one core | frames {}",
                    used as f64 / 1048576.0,
                    engine.frames.load(Ordering::Relaxed)
                );
            }
        });
    }

    if let Some(after) = autosave {
        let engine = engine.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(after));
            save_and_report(&engine);
            std::process::exit(0);
        });
        println!("will save one clip after {after} s and exit");
    }

    let hotkey_ok = unsafe { RegisterHotKey(None, 1, MOD_ALT | MOD_NOREPEAT, 'C' as u32).is_ok() };
    if hotkey_ok {
        println!("\nReady. Press Alt+C to save the last {secs} s. Ctrl+C here to quit.\n");
    } else {
        println!("\nCould not register Alt+C (another program already uses it). Use --autosave N instead.\n");
    }

    let saving = Arc::new(AtomicBool::new(false));
    let mut msg = MSG::default();
    loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 <= 0 {
            break;
        }
        if msg.message == WM_HOTKEY && !saving.swap(true, Ordering::SeqCst) {
            let (engine, saving) = (engine.clone(), saving.clone());
            std::thread::spawn(move || {
                save_and_report(&engine);
                saving.store(false, Ordering::SeqCst);
            });
        }
    }
}
