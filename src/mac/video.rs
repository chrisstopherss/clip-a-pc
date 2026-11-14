//! Screen frames to H.264 with VideoToolbox, on a steady clock.
//!
//! ScreenCaptureKit only delivers a frame when the screen changes, so a timer takes
//! the newest frame at every tick and hands it to the hardware encoder.

use std::ffi::c_void;
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFType};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime, kVTProfileLevel_H264_Main_AutoLevel,
    VTCompressionSession, VTEncodeInfoFlags, VTSessionSetProperty,
};

use super::capture::Capture;
use super::util::{log, now_100ns};
use crate::config::Quality;
use crate::engine::{Msg, Shared};
use crate::mp4::VideoParams;

const CODEC_H264: u32 = 0x6176_6331; // 'avc1'

/// Where the encoder's output goes. Lives behind a raw pointer for VideoToolbox's callback.
struct Sink {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
    width: u32,
    height: u32,
    params: Mutex<Option<(Vec<u8>, Vec<u8>)>>,
    errors: AtomicU64,
}

fn time_100ns(pts: i64) -> CMTime {
    CMTime { value: pts, timescale: 10_000_000, flags: CMTimeFlags::Valid, epoch: 0 }
}

unsafe fn marked_not_sync(arr: &objc2_core_foundation::CFArray) -> bool {
    let first = arr.value_at_index(0);
    if first.is_null() {
        return false;
    }
    let dict = &*(first as *const CFDictionary);
    let v = dict.value(kCMSampleAttachmentKey_NotSync as *const _ as *const c_void);
    !v.is_null() && (&*(v as *const CFBoolean)).as_bool()
}

/// VideoToolbox calls this on its own thread for every encoded frame.
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
    // already length-prefixed (AVCC), which is what the MP4 writer wants
    let data = std::slice::from_raw_parts(ptr as *const u8, total).to_vec();
    let key = match sb.sample_attachments_array(false) {
        Some(arr) if arr.count() > 0 => !marked_not_sync(&arr),
        _ => true,
    };
    if key {
        if let Some(fd) = sb.format_description() {
            let (mut sps, mut pps) = (Vec::new(), Vec::new());
            for (i, out) in [(0usize, &mut sps), (1usize, &mut pps)] {
                let (mut p, mut n, mut cnt, mut hdr) = (std::ptr::null::<u8>(), 0usize, 0usize, 0i32);
                if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&fd, i, &mut p, &mut n, &mut cnt, &mut hdr) == 0 && !p.is_null() {
                    out.extend_from_slice(std::slice::from_raw_parts(p, n));
                }
            }
            if !sps.is_empty() && !pps.is_empty() {
                let mut cur = sink.params.lock().unwrap();
                if cur.as_ref().map(|(a, b)| a != &sps || b != &pps).unwrap_or(true) {
                    *sink.shared.video.lock().unwrap() =
                        Some(VideoParams { width: sink.width, height: sink.height, sps: sps.clone(), pps: pps.clone() });
                    *cur = Some((sps, pps));
                }
            }
        }
    }
    if sink.params.lock().unwrap().is_none() {
        return; // nothing can be played without the parameter sets
    }
    let t = sb.presentation_time_stamp();
    let pts = (t.value as i128 * 10_000_000 / t.timescale.max(1) as i128) as i64;
    let _ = sink.tx.send(Msg::Video { pts, key, data });
}

fn set_prop(session: &VTCompressionSession, key: &objc2_core_foundation::CFString, value: &CFType) {
    let st = unsafe { VTSessionSetProperty(session.as_ref(), key, Some(value)) };
    if st != 0 {
        log(&format!("encoder setting {key:?} not accepted (status {st})"));
    }
}

/// Encode until `stop` is set or the capture is lost. Returns an error if capture was lost.
pub fn run(cap: &Capture, q: Quality, tx: Sender<Msg>, shared: Arc<Shared>, stop: &AtomicBool, frames: &AtomicU64) -> Result<(), String> {
    let fps = q.fps();
    let frame_100ns = 10_000_000 / fps as i64;
    let sink = Box::new(Sink { tx, shared: shared.clone(), width: cap.width as u32, height: cap.height as u32, params: Mutex::new(None), errors: AtomicU64::new(0) });

    let mut session_ptr: *mut VTCompressionSession = null_mut();
    let st = unsafe {
        VTCompressionSession::create(
            None,
            cap.width as i32,
            cap.height as i32,
            CODEC_H264,
            None,
            None,
            None,
            Some(on_encoded),
            &*sink as *const Sink as *mut c_void,
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
        set_prop(session, kVTCompressionPropertyKey_AverageBitRate, CFNumber::new_i32(q.mean_bps() as i32).as_ref());
        set_prop(session, kVTCompressionPropertyKey_ExpectedFrameRate, CFNumber::new_i32(fps as i32).as_ref());
        set_prop(session, kVTCompressionPropertyKey_MaxKeyFrameInterval, CFNumber::new_i32(fps as i32).as_ref());
        set_prop(session, kVTCompressionPropertyKey_AllowFrameReordering, CFBoolean::new(false).as_ref());
        session.prepare_to_encode_frames();
    }
    *shared.video_info.lock().unwrap() = format!("VideoToolbox H.264 ({}x{} at {} fps)", cap.width, cap.height, fps);

    let result = (|| -> Result<(), String> {
        let wait = Instant::now();
        while cap.latest.0.lock().unwrap().is_none() {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if wait.elapsed() > Duration::from_secs(5) {
                return Err("no frames arrived from the screen within 5 s".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut start = Instant::now();
        let mut start_pts = now_100ns();
        let mut n = 0u64;
        while !stop.load(Ordering::Relaxed) {
            if cap.lost.load(Ordering::Relaxed) {
                return Err("the screen capture was stopped (display change, sleep or permission)".into());
            }
            let due = start + Duration::from_nanos(n * 1_000_000_000 / fps as u64);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            } else if now - due > Duration::from_millis(250) {
                // fell far behind (sleep, heavy load): restart the timeline instead of bursting
                start = now;
                start_pts = now_100ns();
                n = 0;
            }
            let img = cap.latest.0.lock().unwrap().clone().ok_or("lost the frame")?;
            let pts = start_pts + n as i64 * frame_100ns;
            let st = unsafe { session.encode_frame(&img, time_100ns(pts), time_100ns(frame_100ns), None, null_mut(), null_mut()) };
            if st != 0 {
                return Err(format!("the encoder rejected a frame (status {st})"));
            }
            frames.fetch_add(1, Ordering::Relaxed);
            n += 1;
        }
        Ok(())
    })();

    unsafe {
        let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags::empty(), epoch: 0 };
        session.complete_frames(invalid);
        session.invalidate();
    }
    drop(sink); // only after the session can no longer call back
    result
}
