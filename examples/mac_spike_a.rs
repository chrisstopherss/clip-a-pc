//! Mac spike A: capture the main display with ScreenCaptureKit, encode H.264 with
//! VideoToolbox (hardware), keep the packets, and write an MP4 with our own muxer.
//!
//! Usage: mac_spike_a [seconds] [output.mp4]

#[path = "../src/mp4.rs"]
#[allow(dead_code)]
mod mp4;
#[path = "../src/ring.rs"]
#[allow(dead_code)]
mod ring;

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("mac_spike_a only runs on macOS");
}

#[cfg(target_os = "macos")]
fn main() {
    imp::run();
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::c_void;
    use std::ptr::{null_mut, NonNull};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker};
    use objc2_app_kit::NSApplication;
    use objc2_core_foundation::{CFBoolean, CFNumber, CFRetained, CFType};
    use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetH264ParameterSetAtIndex};
    use objc2_core_video::CVImageBuffer;
    use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
    use objc2_screen_capture_kit::{
        SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
    };
    use objc2_video_toolbox::{
        kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
        kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
        kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime, kVTProfileLevel_H264_Main_AutoLevel,
        VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
    };

    use super::mp4::{write_mp4, VideoParams};
    use super::ring::{Ring, Track};

    const FPS: i32 = 30;
    const MAX_W: usize = 1280;
    const MAX_H: usize = 720;
    const BITRATE: i32 = 5_000_000;
    /// '420v': 8-bit 4:2:0 bi-planar video range, which the hardware encoder takes directly
    const PIXEL_FORMAT_420V: u32 = 0x3432_3076;
    const CODEC_H264: u32 = 0x6176_6331; // 'avc1'

    struct Latest(Mutex<Option<CFRetained<CVImageBuffer>>>);
    // The image buffer is only ever touched through retain/release and handed to the encoder.
    unsafe impl Send for Latest {}
    unsafe impl Sync for Latest {}

    struct Ivars {
        latest: Arc<Latest>,
        arrived: Arc<AtomicU64>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "ClipSpikeOutput"]
        #[ivars = Ivars]
        struct Output;

        unsafe impl NSObjectProtocol for Output {}

        unsafe impl SCStreamOutput for Output {
            #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
            fn did_output(&self, _stream: &SCStream, buffer: &CMSampleBuffer, kind: SCStreamOutputType) {
                if kind != SCStreamOutputType::Screen {
                    return;
                }
                // frames with no image attached are status updates (screen idle), skip them
                if let Some(img) = unsafe { buffer.image_buffer() } {
                    self.ivars().arrived.fetch_add(1, Ordering::Relaxed);
                    *self.ivars().latest.0.lock().unwrap() = Some(img);
                }
            }
        }
    );

    impl Output {
        fn new(latest: Arc<Latest>, arrived: Arc<AtomicU64>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(Ivars { latest, arrived });
            unsafe { msg_send![super(this), init] }
        }
    }

    struct Packet {
        pts_100ns: i64,
        key: bool,
        data: Vec<u8>,
    }

    #[derive(Default)]
    struct Sink {
        packets: Mutex<Vec<Packet>>,
        params: Mutex<Option<(Vec<u8>, Vec<u8>)>>,
        errors: AtomicU64,
    }

    fn time_100ns(pts: i64) -> CMTime {
        CMTime { value: pts, timescale: 10_000_000, flags: CMTimeFlags::Valid, epoch: 0 }
    }

    /// Called by VideoToolbox on its own thread for every encoded frame.
    unsafe extern "C-unwind" fn on_encoded(refcon: *mut c_void, _src: *mut c_void, status: i32, _flags: VTEncodeInfoFlags, sample: *mut CMSampleBuffer) {
        let sink = &*(refcon as *const Sink);
        if status != 0 || sample.is_null() {
            sink.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let sb = &*sample;
        let Some(block) = sb.data_buffer() else { return };
        let (mut at, mut total, mut ptr) = (0usize, 0usize, null_mut::<std::ffi::c_char>());
        if block.data_pointer(0, &mut at, &mut total, &mut ptr) != 0 || ptr.is_null() {
            sink.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let data = std::slice::from_raw_parts(ptr as *const u8, total).to_vec();
        // a frame is a keyframe unless it is marked "not sync"
        let key = match sb.sample_attachments_array(false) {
            Some(arr) if arr.count() > 0 => !marked_not_sync(&arr),
            _ => true,
        };
        if key && sink.params.lock().unwrap().is_none() {
            if let Some(fd) = sb.format_description() {
                let (mut sps, mut pps) = (Vec::new(), Vec::new());
                for (i, out) in [(0usize, &mut sps), (1usize, &mut pps)] {
                    let (mut p, mut n, mut cnt, mut hdr) = (std::ptr::null::<u8>(), 0usize, 0usize, 0i32);
                    if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&fd, i, &mut p, &mut n, &mut cnt, &mut hdr) == 0 && !p.is_null() {
                        out.extend_from_slice(std::slice::from_raw_parts(p, n));
                    }
                }
                if !sps.is_empty() && !pps.is_empty() {
                    *sink.params.lock().unwrap() = Some((sps, pps));
                }
            }
        }
        let t = sb.presentation_time_stamp();
        let pts_100ns = (t.value as i128 * 10_000_000 / t.timescale.max(1) as i128) as i64;
        sink.packets.lock().unwrap().push(Packet { pts_100ns, key, data });
    }

    unsafe fn marked_not_sync(arr: &objc2_core_foundation::CFArray) -> bool {
        use objc2_core_foundation::CFDictionary;
        use objc2_core_media::kCMSampleAttachmentKey_NotSync;
        let first = arr.value_at_index(0);
        if first.is_null() {
            return false;
        }
        let dict = &*(first as *const CFDictionary);
        let v = dict.value(kCMSampleAttachmentKey_NotSync as *const _ as *const c_void);
        !v.is_null() && (&*(v as *const CFBoolean)).as_bool()
    }

    fn set_prop(session: &VTCompressionSession, key: &objc2_core_foundation::CFString, value: &CFType) {
        let st = unsafe { VTSessionSetProperty(session.as_ref(), key, Some(value)) };
        if st != 0 {
            println!("  encoder property {key:?} not accepted (status {st})");
        }
    }

    fn rss_mb() -> f64 {
        let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output();
        out.ok().and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<f64>().ok()).map(|kb| kb / 1024.0).unwrap_or(0.0)
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().collect();
        let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
        let path = args.get(2).cloned().unwrap_or_else(|| "mac_spike_a.mp4".into());
        if let Err(e) = real_main(secs, &path) {
            eprintln!("\nFAILED: {e}");
            std::process::exit(1);
        }
    }

    fn real_main(secs: u64, path: &str) -> Result<(), String> {
        // ScreenCaptureKit needs a connection to the window server, which this provides
        let mtm = MainThreadMarker::new().ok_or("must run on the main thread")?;
        let _app = NSApplication::sharedApplication(mtm);
        println!("memory at start: {:.0} MB", rss_mb());

        // 1. find the main display (this is what triggers the Screen Recording permission prompt)
        let (tx, rx) = channel::<Result<(Retained<SCShareableContent>, ()), String>>();
        let handler = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
            let r = if !content.is_null() {
                Ok((unsafe { Retained::retain(content) }.unwrap(), ()))
            } else if !err.is_null() {
                Err(unsafe { &*err }.localizedDescription().to_string())
            } else {
                Err("no content and no error".to_string())
            };
            let _ = tx.send(r);
        });
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
        let (content, _) = rx.recv_timeout(Duration::from_secs(30)).map_err(|_| "timed out asking for screen access")??;
        let displays = unsafe { content.displays() };
        let display = displays.firstObject().ok_or("no displays found")?;
        let (dw, dh) = unsafe { (display.width() as usize, display.height() as usize) };
        println!("display: {dw}x{dh} points");

        let scale = (MAX_W as f64 / dw as f64).min(MAX_H as f64 / dh as f64).min(1.0);
        let (out_w, out_h) = (((dw as f64 * scale) as usize) & !1, ((dh as f64 * scale) as usize) & !1);
        println!("encoding at {out_w}x{out_h} @ {FPS} fps, {} kbps", BITRATE / 1000);

        // 2. configure and start the stream
        let latest = Arc::new(Latest(Mutex::new(None)));
        let arrived = Arc::new(AtomicU64::new(0));
        let output = Output::new(latest.clone(), arrived.clone());
        let filter = unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new()) };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(out_w);
            config.setHeight(out_h);
            config.setMinimumFrameInterval(CMTime { value: 1, timescale: FPS, flags: CMTimeFlags::Valid, epoch: 0 });
            config.setPixelFormat(PIXEL_FORMAT_420V);
            config.setShowsCursor(true);
            config.setQueueDepth(5);
        }
        let stream = unsafe { SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, None) };
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), SCStreamOutputType::Screen, None)
                .map_err(|e| e.localizedDescription().to_string())?;
        }
        let (stx, srx) = channel::<Option<String>>();
        let start = RcBlock::new(move |err: *mut NSError| {
            let _ = stx.send(if err.is_null() { None } else { Some(unsafe { &*err }.localizedDescription().to_string()) });
        });
        unsafe { stream.startCaptureWithCompletionHandler(Some(&start)) };
        if let Some(e) = srx.recv_timeout(Duration::from_secs(30)).map_err(|_| "timed out starting capture")? {
            return Err(format!("capture would not start: {e}"));
        }
        println!("capture started");

        // 3. hardware H.264 encoder
        let sink = Arc::new(Sink::default());
        let mut session_ptr: *mut VTCompressionSession = null_mut();
        let st = unsafe {
            VTCompressionSession::create(
                None,
                out_w as i32,
                out_h as i32,
                CODEC_H264,
                None,
                None,
                None,
                Some(on_encoded),
                Arc::as_ptr(&sink) as *mut c_void,
                NonNull::new(&mut session_ptr).unwrap(),
            )
        };
        if st != 0 || session_ptr.is_null() {
            return Err(format!("could not create the H.264 encoder (status {st})"));
        }
        let session: &VTCompressionSession = unsafe { &*session_ptr };
        unsafe {
            set_prop(session, kVTCompressionPropertyKey_RealTime, CFBoolean::new(true).as_ref());
            set_prop(session, kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_Main_AutoLevel.as_ref());
            set_prop(session, kVTCompressionPropertyKey_AverageBitRate, CFNumber::new_i32(BITRATE).as_ref());
            set_prop(session, kVTCompressionPropertyKey_ExpectedFrameRate, CFNumber::new_i32(FPS).as_ref());
            set_prop(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, CFNumber::new_i32(FPS).as_ref());
            set_prop(session, kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false).as_ref());
            session.prepare_to_encode_frames();
        }
        println!("memory after setup: {:.0} MB", rss_mb());

        // 4. wait for a first frame, then feed the encoder on a steady 30 fps clock
        let wait = Instant::now();
        while latest.0.lock().unwrap().is_none() {
            if wait.elapsed() > Duration::from_secs(5) {
                return Err("no frames arrived from the screen within 5 s".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        println!("recording {secs} s ...");
        let begin = Instant::now();
        let total = secs * FPS as u64;
        let mut last_report = Instant::now();
        let mut last_arrived = 0u64;
        let mut peak = 0f64;
        for n in 0..total {
            let due = begin + Duration::from_nanos(n * 1_000_000_000 / FPS as u64);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            let img = latest.0.lock().unwrap().clone().ok_or("lost the frame")?;
            let pts = (n as i64) * 10_000_000 / FPS as i64;
            let st = unsafe { session.encode_frame(&img, time_100ns(pts), time_100ns(10_000_000 / FPS as i64), None, null_mut(), null_mut()) };
            if st != 0 {
                return Err(format!("encoder rejected a frame (status {st})"));
            }
            if last_report.elapsed() >= Duration::from_secs(1) {
                let a = arrived.load(Ordering::Relaxed);
                let m = rss_mb();
                peak = peak.max(m);
                println!("t={:>3}s  submitted {:>4}  screen frames/s {:>3}  encoded {:>4}  memory {:.0} MB", begin.elapsed().as_secs(), n + 1, a - last_arrived, sink.packets.lock().unwrap().len(), m);
                last_arrived = a;
                last_report = Instant::now();
            }
        }
        let wall = begin.elapsed().as_secs_f64();
        unsafe {
            let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags::empty(), epoch: 0 };
            session.complete_frames(invalid);
            let _ = stream.stopCaptureWithCompletionHandler(None);
            session.invalidate();
        }

        // 5. report, then write the MP4 through the ring and our own muxer
        let packets = std::mem::take(&mut *sink.packets.lock().unwrap());
        let keys: Vec<&Packet> = packets.iter().filter(|p| p.key).collect();
        let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
        println!("\n--- result ---");
        println!("packets {}, keyframes {}, encoder errors {}", packets.len(), keys.len(), sink.errors.load(Ordering::Relaxed));
        println!("total {:.2} MB, avg {:.2} Mbps", bytes as f64 / 1048576.0, bytes as f64 * 8.0 / wall / 1e6);
        let gaps: Vec<String> = keys.windows(2).map(|w| format!("{:.2}", (w[1].pts_100ns - w[0].pts_100ns) as f64 / 1e7)).collect();
        println!("seconds between keyframes: {}", gaps.join(" "));
        println!("peak memory {peak:.0} MB, end {:.0} MB", rss_mb());
        let (sps, pps) = sink.params.lock().unwrap().clone().ok_or("the encoder never produced SPS and PPS")?;
        println!("SPS {} bytes, PPS {} bytes", sps.len(), pps.len());

        let mut ring = Ring::new(bytes + 4096, secs as f64 + 5.0);
        for p in &packets {
            ring.push(Track::Video, p.pts_100ns, p.key, &p.data);
        }
        let clip = ring.select(secs as f64).ok_or("no keyframe in the recording")?;
        let v = VideoParams { width: out_w as u32, height: out_h as u32, sps, pps };
        let mut f = std::io::BufWriter::new(std::fs::File::create(path).map_err(|e| e.to_string())?);
        write_mp4(&mut f, &ring, &clip, &v, None).map_err(|e| e.to_string())?;
        use std::io::Write;
        f.flush().map_err(|e| e.to_string())?;
        println!("wrote {path}");
        Ok(())
    }
}
