//! Desktop audio and microphone, mixed on one clock and encoded to AAC.
//!
//! Desktop audio is taken from every app except this program before it reaches
//! any output device (Windows 11), so routing tools and exclusive-mode devices
//! cannot hide it. If that is unavailable it falls back to loopback on the
//! default output. The microphone follows the default input device.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::core::PWSTR;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};

use super::engine::{Msg, Shared};
use super::util::qpc_now_100ns;
use crate::mixer::{Mixer, FRAME, RATE};
use crate::mp4::AudioParams;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const DESKTOP: usize = 0;
const MIC: usize = 1;
const AAC_BYTES_PER_SEC: u32 = 16_000; // 128 kbps

struct Capture {
    client: IAudioClient,
    cap: IAudioCaptureClient,
    id: String,
    name: String,
}

fn pwstr_to_string(p: PWSTR) -> String {
    let s = unsafe { p.to_string().unwrap_or_default() };
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

fn default_id(en: &IMMDeviceEnumerator, render: bool) -> Option<String> {
    unsafe {
        let dev = en.GetDefaultAudioEndpoint(if render { eRender } else { eCapture }, eConsole).ok()?;
        Some(pwstr_to_string(dev.GetId().ok()?))
    }
}

fn friendly_name(dev: &IMMDevice) -> String {
    unsafe {
        let Ok(store) = dev.OpenPropertyStore(STGM_READ) else { return "?".into() };
        match store.GetValue(&windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName) {
            Ok(v) => v.to_string(),
            Err(_) => "?".into(),
        }
    }
}

fn open_default(en: &IMMDeviceEnumerator, render: bool) -> Res<Capture> {
    unsafe {
        let dev = en.GetDefaultAudioEndpoint(if render { eRender } else { eCapture }, eConsole)?;
        let id = pwstr_to_string(dev.GetId()?);
        let name = friendly_name(&dev);
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let fmt = WAVEFORMATEX {
            wFormatTag: 3, // float
            nChannels: 2,
            nSamplesPerSec: RATE as u32,
            nAvgBytesPerSec: RATE as u32 * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if render {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 10_000_000, 0, &fmt, None)?;
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok(Capture { client, cap, id, name })
    }
}

fn drain_device(c: &Capture, mixer: &mut Mixer, src: usize) -> Res<()> {
    unsafe {
        loop {
            if c.cap.GetNextPacketSize()? == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let (mut frames, mut flags, mut qpc) = (0u32, 0u32, 0u64);
            c.cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
            let n = frames as usize * 2;
            let samples: Vec<f32> = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                vec![0.0; n]
            } else {
                std::slice::from_raw_parts(data as *const f32, n).to_vec()
            };
            c.cap.ReleaseBuffer(frames)?;
            mixer.push(src, qpc as i64, &samples);
        }
    }
}

/// Everything every app plays except this program (Windows 11).
struct SysLoop {
    _client: wasapi::AudioClient,
    cap: wasapi::AudioCaptureClient,
    _event: wasapi::Handle,
    bytes: VecDeque<u8>,
}

fn open_system_loopback() -> Res<SysLoop> {
    use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};
    wasapi::initialize_mta().ok()?;
    let fmt = WaveFormat::new(32, 32, SampleType::Float, RATE as usize, 2, None);
    let mut client = AudioClient::new_application_loopback_client(std::process::id(), false)?;
    client.initialize_client(
        &fmt,
        Direction::Capture,
        StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 200_000 },
        None,
    )?;
    let event = client.set_get_eventhandle()?;
    let cap = client.get_audiocaptureclient()?;
    client.start_stream()?;
    Ok(SysLoop { _client: client, cap, _event: event, bytes: VecDeque::new() })
}

fn drain_system(s: &mut SysLoop, mixer: &mut Mixer) -> Res<()> {
    while s.cap.get_next_packet_size()?.unwrap_or(0) > 0 {
        s.bytes.clear();
        let info = s.cap.read_from_device_to_deque(&mut s.bytes)?;
        let raw: Vec<u8> = s.bytes.iter().copied().collect();
        let samples: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        mixer.push(DESKTOP, info.timestamp as i64, &samples);
    }
    Ok(())
}

// ------------------------------------------------------------------ AAC

struct Aac {
    t: IMFTransform,
    out_size: u32,
    asc: Vec<u8>,
}

impl Aac {
    fn new() -> Res<Self> {
        unsafe {
            let want = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
            let mut acts: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut n = 0u32;
            MFTEnumEx(MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER, None, Some(&want), &mut acts, &mut n)?;
            if n == 0 {
                return Err("no AAC encoder found (Windows N edition without the Media Feature Pack?)".into());
            }
            let t: IMFTransform = (*acts).as_ref().ok_or("null activate")?.ActivateObject()?;
            for i in 0..n as usize {
                std::ptr::drop_in_place(acts.add(i));
            }
            CoTaskMemFree(Some(acts as *const _));

            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE as u32)?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SEC)?;
            out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
            t.SetOutputType(0, &out, 0)?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE as u32)?;
            inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
            inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, RATE as u32 * 4)?;
            t.SetInputType(0, &inp, 0)?;

            let cur = t.GetOutputCurrentType(0)?;
            let mut blob = [0u8; 64];
            let mut len = 0u32;
            cur.GetBlob(&MF_MT_USER_DATA, &mut blob, Some(&mut len))?;
            if len <= 12 {
                return Err("AAC encoder gave no AudioSpecificConfig".into());
            }
            let asc = blob[12..len as usize].to_vec();
            let info = t.GetOutputStreamInfo(0)?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self { t, out_size: info.cbSize.max(8192), asc })
        }
    }

    /// Encode one FRAME of interleaved 16-bit stereo. Returns any finished AAC frames.
    fn encode(&mut self, pcm: &[i16], pts: i64) -> Res<Vec<Vec<u8>>> {
        unsafe {
            let bytes = pcm.len() * 2;
            let buf = MFCreateMemoryBuffer(bytes as u32)?;
            let mut p = std::ptr::null_mut::<u8>();
            buf.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, p, bytes);
            buf.Unlock()?;
            buf.SetCurrentLength(bytes as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(FRAME as i64 * 10_000_000 / RATE)?;
            self.t.ProcessInput(0, &sample, 0)?;
        }
        let mut out = Vec::new();
        unsafe {
            loop {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                let mut bufs = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(Some(sample)),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let r = self.t.ProcessOutput(0, &mut bufs, &mut status);
                let sample = std::mem::ManuallyDrop::take(&mut bufs[0].pSample);
                let _ev = std::mem::ManuallyDrop::take(&mut bufs[0].pEvents);
                match r {
                    Ok(()) => {
                        if let Some(s) = sample {
                            let media = s.ConvertToContiguousBuffer()?;
                            let (mut p, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
                            media.Lock(&mut p, None, Some(&mut len))?;
                            out.push(std::slice::from_raw_parts(p, len as usize).to_vec());
                            media.Unlock()?;
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(out),
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
}

// ------------------------------------------------------------------ main loop

fn run_once(tx: Sender<Msg>, shared: Arc<Shared>, stop: Arc<AtomicBool>, mic_gain: f32) -> Res<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let en: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    let mut sys = open_system_loopback().ok();
    let mut desktop_dev: Option<Capture> = None;
    let desktop_label;
    if sys.is_some() {
        desktop_label = "all apps (device independent)".to_string();
    } else {
        match open_default(&en, true) {
            Ok(c) => {
                desktop_label = format!("default output: {}", c.name);
                desktop_dev = Some(c);
            }
            Err(e) => desktop_label = format!("unavailable ({e})"),
        }
    }
    let mut mic = open_default(&en, false).ok();
    let mut mic_retry = Instant::now();

    let mut aac = Aac::new()?;
    *shared.audio.lock().unwrap() = Some(AudioParams { sample_rate: RATE as u32, channels: 2, asc: aac.asc.clone(), bitrate: AAC_BYTES_PER_SEC * 8 });
    *shared.audio_info.lock().unwrap() = format!(
        "desktop: {desktop_label} | mic: {}",
        mic.as_ref().map(|m| m.name.clone()).unwrap_or_else(|| "none".into())
    );

    let mut mixer = Mixer::new(2);
    mixer.set_gain(MIC, mic_gain);
    let mut first_pts: Option<i64> = None;
    let mut out_frames = 0u64;
    let mut last_poll = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(10));

        if last_poll.elapsed() >= Duration::from_millis(500) {
            last_poll = Instant::now();
            // follow the default microphone
            let now_id = default_id(&en, false);
            let have = mic.as_ref().map(|c| c.id.clone());
            if now_id != have && (now_id.is_none() || have.is_some() || mic_retry.elapsed() > Duration::from_secs(3)) {
                if let Some(old) = mic.take() {
                    unsafe { let _ = old.client.Stop(); }
                }
                mic_retry = Instant::now();
                mic = open_default(&en, false).ok();
            }
            // follow the default output, only when using the device fallback
            if sys.is_none() {
                let now_id = default_id(&en, true);
                let have = desktop_dev.as_ref().map(|c| c.id.clone());
                if now_id != have {
                    if let Some(old) = desktop_dev.take() {
                        unsafe { let _ = old.client.Stop(); }
                    }
                    desktop_dev = open_default(&en, true).ok();
                }
            }
        }

        if let Some(s) = sys.as_mut() {
            if drain_system(s, &mut mixer).is_err() {
                sys = None; // lost it; the fallback takes over on the next poll
            }
        }
        if let Some(c) = &desktop_dev {
            if drain_device(c, &mut mixer, DESKTOP).is_err() {
                desktop_dev = None;
            }
        }
        if let Some(c) = &mic {
            if drain_device(c, &mut mixer, MIC).is_err() {
                mic = None;
            }
        }

        for (pts, frame) in mixer.pull(qpc_now_100ns()) {
            let pcm: Vec<i16> = frame.iter().map(|v| (v * 32767.0) as i16).collect();
            let base = *first_pts.get_or_insert(pts);
            for data in aac.encode(&pcm, pts)? {
                // one AAC frame covers FRAME samples, in order
                let pts_k = base + out_frames as i64 * FRAME as i64 * 10_000_000 / RATE;
                out_frames += 1;
                let _ = tx.send(Msg::Audio { pts: pts_k, data });
            }
        }
    }
    Ok(())
}

/// Runs until `stop` is set, starting over after an error.
pub fn run(tx: Sender<Msg>, shared: Arc<Shared>, stop: Arc<AtomicBool>, mic_gain: f32) -> Res<()> {
    let mut last_err = String::new();
    while !stop.load(Ordering::Relaxed) {
        match run_once(tx.clone(), shared.clone(), stop.clone(), mic_gain) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg != last_err {
                    super::log::log(&format!("audio error, restarting: {msg}"));
                    last_err = msg;
                }
                for _ in 0..20 {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    Ok(())
}
