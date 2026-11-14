//! One ScreenCaptureKit stream for the screen, the desktop audio and the microphone.
//! Having one stream means all three share a clock.

use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_core_video::CVImageBuffer;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};

use super::util::log;
use crate::mixer::RATE;
use crate::resample::{mono_to_stereo, resample_stereo};

/// '420v': 8-bit 4:2:0 bi-planar video range, which the hardware encoder takes directly.
const PIXEL_FORMAT_420V: u32 = 0x3432_3076;
const FLAG_NON_INTERLEAVED: u32 = 1 << 5;
const FLAG_FLOAT: u32 = 1;

pub const DESKTOP: usize = 0;
pub const MIC: usize = 1;

/// A piece of audio, already interleaved stereo at 48 kHz.
pub struct Chunk {
    pub src: usize,
    pub t_100ns: i64,
    pub samples: Vec<f32>,
}

pub struct Latest(pub Mutex<Option<CFRetained<CVImageBuffer>>>);
// The buffer is only retained, released and handed to the encoder, never read from two threads at once.
unsafe impl Send for Latest {}
unsafe impl Sync for Latest {}

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub id: u32,
    pub width: usize,
    pub height: usize,
    pub primary: bool,
}

struct OutIvars {
    latest: Arc<Latest>,
    arrived: Arc<AtomicU64>,
    audio_tx: Sender<Chunk>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ClipAPCOutput"]
    #[ivars = OutIvars]
    struct Output;

    unsafe impl NSObjectProtocol for Output {}

    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, buffer: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind == SCStreamOutputType::Screen {
                // frames with no image are status updates (screen idle)
                if let Some(img) = unsafe { buffer.image_buffer() } {
                    self.ivars().arrived.fetch_add(1, Ordering::Relaxed);
                    *self.ivars().latest.0.lock().unwrap() = Some(img);
                }
            } else {
                let src = if kind == SCStreamOutputType::Microphone { MIC } else { DESKTOP };
                if let Some(chunk) = unsafe { audio_chunk(buffer, src) } {
                    let _ = self.ivars().audio_tx.send(chunk);
                }
            }
        }
    }
);

impl Output {
    fn new(iv: OutIvars) -> Retained<Self> {
        let this = Self::alloc().set_ivars(iv);
        unsafe { msg_send![super(this), init] }
    }
}

struct DelIvars {
    lost: Arc<AtomicBool>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ClipAPCStreamDelegate"]
    #[ivars = DelIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl SCStreamDelegate for Delegate {
        #[unsafe(method(stream:didStopWithError:))]
        fn did_stop(&self, _stream: &SCStream, error: &NSError) {
            log(&format!("screen capture stopped: {}", error.localizedDescription()));
            self.ivars().lost.store(true, Ordering::Relaxed);
        }
    }
);

impl Delegate {
    fn new(lost: Arc<AtomicBool>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelIvars { lost });
        unsafe { msg_send![super(this), init] }
    }
}

fn cm_100ns(t: CMTime) -> i64 {
    (t.value as i128 * 10_000_000 / t.timescale.max(1) as i128) as i64
}

/// Turn an audio sample buffer into interleaved stereo 48 kHz floats.
unsafe fn audio_chunk(sb: &CMSampleBuffer, src: usize) -> Option<Chunk> {
    let fd = sb.format_description()?;
    let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&fd);
    if asbd.is_null() {
        return None;
    }
    let asbd = &*asbd;
    if asbd.mFormatFlags & FLAG_FLOAT == 0 || asbd.mBitsPerChannel != 32 {
        return None; // ScreenCaptureKit delivers 32-bit float; anything else we do not understand
    }
    let channels = asbd.mChannelsPerFrame.max(1) as usize;

    let mut needed = 0usize;
    sb.audio_buffer_list_with_retained_block_buffer(&mut needed, null_mut(), 0, None, None, 0, null_mut());
    if needed == 0 {
        return None;
    }
    let mut mem = vec![0u64; needed.div_ceil(8)];
    let abl = mem.as_mut_ptr() as *mut AudioBufferList;
    let mut block: *mut CMBlockBuffer = null_mut();
    let st = sb.audio_buffer_list_with_retained_block_buffer(&mut needed, abl, needed, None, None, 0, &mut block);
    // the call hands back a retained block buffer; hold it until the data is copied
    let _keep = NonNull::new(block).map(|b| CFRetained::from_raw(b));
    if st != 0 {
        return None;
    }
    let n = (*abl).mNumberBuffers as usize;
    let bufs: &[AudioBuffer] = std::slice::from_raw_parts((*abl).mBuffers.as_ptr(), n);
    let floats = |b: &AudioBuffer| -> &[f32] {
        if b.mData.is_null() {
            &[]
        } else {
            std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4)
        }
    };

    let stereo: Vec<f32> = if asbd.mFormatFlags & FLAG_NON_INTERLEAVED != 0 {
        // one buffer per channel
        let l = floats(bufs.first()?);
        match bufs.get(1) {
            Some(r) => {
                let r = floats(r);
                let frames = l.len().min(r.len());
                (0..frames).flat_map(|i| [l[i], r[i]]).collect()
            }
            None => mono_to_stereo(l),
        }
    } else {
        let d = floats(bufs.first()?);
        match channels {
            1 => mono_to_stereo(d),
            2 => d.to_vec(),
            c => d.chunks_exact(c).flat_map(|f| [f[0], f[1]]).collect(),
        }
    };
    let rate = asbd.mSampleRate as u32;
    let samples = if rate != RATE as u32 && rate != 0 { resample_stereo(&stereo, rate, RATE as u32) } else { stereo };
    if samples.is_empty() {
        return None;
    }
    Some(Chunk { src, t_100ns: cm_100ns(sb.presentation_time_stamp()), samples })
}

/// All attached displays, main display first.
pub fn list_monitors() -> Result<Vec<MonitorInfo>, String> {
    let content = shareable_content()?;
    let mut out = Vec::new();
    unsafe {
        for d in content.displays().iter() {
            let f = d.frame();
            out.push(MonitorInfo {
                id: d.displayID(),
                width: d.width() as usize,
                height: d.height() as usize,
                primary: f.origin.x == 0.0 && f.origin.y == 0.0,
            });
        }
    }
    out.sort_by_key(|m| !m.primary);
    Ok(out)
}

fn shareable_content() -> Result<Retained<SCShareableContent>, String> {
    let (tx, rx) = channel::<Result<Retained<SCShareableContent>, String>>();
    let handler = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let r = if !content.is_null() {
            Ok(unsafe { Retained::retain(content) }.unwrap())
        } else if !err.is_null() {
            Err(unsafe { &*err }.localizedDescription().to_string())
        } else {
            Err("no content and no error".to_string())
        };
        let _ = tx.send(r);
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    rx.recv_timeout(Duration::from_secs(30)).map_err(|_| "timed out asking for screen access".to_string())?
}

pub struct CaptureCfg {
    /// Which display, as listed by `list_monitors` (None means the main display).
    pub display_id: Option<u32>,
    pub max_w: usize,
    pub max_h: usize,
    pub fps: i32,
    pub mic: bool,
}

pub struct Capture {
    stream: Retained<SCStream>,
    _output: Retained<Output>,
    _delegate: Retained<Delegate>,
    pub latest: Arc<Latest>,
    pub arrived: Arc<AtomicU64>,
    pub lost: Arc<AtomicBool>,
    /// Taken by the audio thread.
    pub audio_rx: Option<Receiver<Chunk>>,
    pub width: usize,
    pub height: usize,
}

impl Capture {
    pub fn start(cfg: &CaptureCfg) -> Result<Capture, String> {
        let content = shareable_content()?;
        let displays = unsafe { content.displays() };
        let display = displays
            .iter()
            .find(|d| cfg.display_id.map(|id| unsafe { d.displayID() } == id).unwrap_or(false))
            .or_else(|| displays.iter().find(|d| unsafe { d.frame().origin.x == 0.0 && d.frame().origin.y == 0.0 }))
            .or_else(|| displays.firstObject())
            .ok_or("no displays found")?;
        let (dw, dh) = unsafe { (display.width() as usize, display.height() as usize) };
        let scale = (cfg.max_w as f64 / dw as f64).min(cfg.max_h as f64 / dh as f64).min(1.0);
        let (width, height) = (((dw as f64 * scale) as usize) & !1, ((dh as f64 * scale) as usize) & !1);

        let latest = Arc::new(Latest(Mutex::new(None)));
        let arrived = Arc::new(AtomicU64::new(0));
        let lost = Arc::new(AtomicBool::new(false));
        let (audio_tx, audio_rx) = channel::<Chunk>();
        let output = Output::new(OutIvars { latest: latest.clone(), arrived: arrived.clone(), audio_tx });
        let delegate = Delegate::new(lost.clone());

        let filter = unsafe { SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display, &NSArray::new()) };
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(width);
            config.setHeight(height);
            config.setMinimumFrameInterval(CMTime { value: 1, timescale: cfg.fps, flags: CMTimeFlags::Valid, epoch: 0 });
            config.setPixelFormat(PIXEL_FORMAT_420V);
            config.setShowsCursor(true);
            config.setQueueDepth(5);
            config.setCapturesAudio(true);
            config.setSampleRate(RATE as isize);
            config.setChannelCount(2);
            // our own chimes must not end up in a clip
            config.setExcludesCurrentProcessAudio(true);
            // microphone capture through ScreenCaptureKit exists from macOS 15; older systems would crash here
            let can_mic: bool = msg_send![&*config, respondsToSelector: objc2::sel!(setCaptureMicrophone:)];
            if cfg.mic && can_mic {
                config.setCaptureMicrophone(true);
            } else if cfg.mic {
                log("this macOS cannot capture the microphone through ScreenCaptureKit (needs macOS 15)");
            }
        }
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &config, Some(ProtocolObject::from_ref(&*delegate)))
        };
        let sink = ProtocolObject::from_ref(&*output);
        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(sink, SCStreamOutputType::Screen, None)
                .map_err(|e| e.localizedDescription().to_string())?;
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(sink, SCStreamOutputType::Audio, None)
                .map_err(|e| e.localizedDescription().to_string())?;
            if cfg.mic {
                if let Err(e) = stream.addStreamOutput_type_sampleHandlerQueue_error(sink, SCStreamOutputType::Microphone, None) {
                    log(&format!("microphone output not available: {}", e.localizedDescription()));
                }
            }
        }
        let (stx, srx) = channel::<Option<String>>();
        let start = RcBlock::new(move |err: *mut NSError| {
            let _ = stx.send(if err.is_null() { None } else { Some(unsafe { &*err }.localizedDescription().to_string()) });
        });
        unsafe { stream.startCaptureWithCompletionHandler(Some(&start)) };
        if let Some(e) = srx.recv_timeout(Duration::from_secs(30)).map_err(|_| "timed out starting capture".to_string())? {
            return Err(format!("capture would not start: {e}"));
        }
        Ok(Capture { stream, _output: output, _delegate: delegate, latest, arrived, lost, audio_rx: Some(audio_rx), width, height })
    }

    pub fn stop(&self) {
        unsafe {
            self.stream.stopCaptureWithCompletionHandler(None);
        }
    }
}
