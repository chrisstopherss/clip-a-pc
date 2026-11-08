//! Spike B: capture desktop audio (WASAPI loopback of the default output) and the
//! default microphone, mix them on one clock, encode AAC with the built-in
//! Media Foundation encoder, and write mix.wav and mix.aac so you can listen.
//!
//! Usage: spike_b.exe [seconds]

use std::time::{Duration, Instant};

use windows::core::PWSTR;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::mixer::{Mixer, FRAME, RATE};
use std::collections::VecDeque;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

const LOOPBACK: usize = 0;
const MIC: usize = 1;

pub fn run() {
    if let Err(e) = real_main() {
        eprintln!("\nFAILED: {e}");
        std::process::exit(1);
    }
}

fn qpc_now_100ns() -> i64 {
    let (mut c, mut f) = (0i64, 0i64);
    unsafe {
        let _ = QueryPerformanceCounter(&mut c);
        let _ = QueryPerformanceFrequency(&mut f);
    }
    (c as i128 * 10_000_000 / f as i128) as i64
}

// ---------------------------------------------------------------- capture

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
        let dev = en
            .GetDefaultAudioEndpoint(if render { eRender } else { eCapture }, eConsole)
            .ok()?;
        Some(pwstr_to_string(dev.GetId().ok()?))
    }
}

fn friendly_name(dev: &IMMDevice) -> String {
    unsafe {
        let store = match dev.OpenPropertyStore(STGM_READ) {
            Ok(s) => s,
            Err(_) => return "?".into(),
        };
        match store.GetValue(&windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName) {
            Ok(v) => v.to_string(),
            Err(_) => "?".into(),
        }
    }
}

fn open_capture(en: &IMMDeviceEnumerator, render: bool) -> Res<Capture> {
    let dev = unsafe { en.GetDefaultAudioEndpoint(if render { eRender } else { eCapture }, eConsole)? };
    open_device(&dev, render)
}

fn open_device(dev: &IMMDevice, render: bool) -> Res<Capture> {
    unsafe {
        let id = pwstr_to_string(dev.GetId()?);
        let name = friendly_name(dev);
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        // Ask the audio engine for 48 kHz stereo float no matter what the device uses.
        let fmt = WAVEFORMATEX {
            wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
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

#[derive(Default)]
struct SecondStats {
    packets: u64,
    frames: u64,
    silent_flags: u64,
    discontinuities: u64,
    peak: f32,
}

/// Drain every waiting packet into the mixer.
fn drain(c: &Capture, mixer: &mut Mixer, src: usize, st: &mut SecondStats) -> Res<()> {
    unsafe {
        loop {
            let avail = c.cap.GetNextPacketSize()?;
            if avail == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut qpc = 0u64;
            c.cap.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
            let n = frames as usize * 2;
            let samples: Vec<f32> = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                st.silent_flags += 1;
                vec![0.0; n]
            } else {
                std::slice::from_raw_parts(data as *const f32, n).to_vec()
            };
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                st.discontinuities += 1;
            }
            c.cap.ReleaseBuffer(frames)?;
            st.packets += 1;
            st.frames += frames as u64;
            st.peak = samples.iter().fold(st.peak, |p, v| p.max(v.abs()));
            mixer.push(src, qpc as i64, &samples);
        }
    }
}


// ------------------------------------------------- system-wide audio (process loopback)

/// Everything every app plays except this program, tapped before it reaches any
/// device, so it does not matter which output the apps use or how it is routed.
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

fn drain_system(s: &mut SysLoop, mixer: &mut Mixer, st: &mut SecondStats) -> Res<()> {
    while s.cap.get_next_packet_size()?.unwrap_or(0) > 0 {
        s.bytes.clear();
        let info = s.cap.read_from_device_to_deque(&mut s.bytes)?;
        let raw: Vec<u8> = s.bytes.iter().copied().collect();
        let samples: Vec<f32> = raw.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        if info.flags.silent {
            st.silent_flags += 1;
        }
        if info.flags.data_discontinuity {
            st.discontinuities += 1;
        }
        st.packets += 1;
        st.frames += samples.len() as u64 / 2;
        st.peak = samples.iter().fold(st.peak, |p, v| p.max(v.abs()));
        mixer.push(LOOPBACK, info.timestamp as i64, &samples);
    }
    Ok(())
}

// ---------------------------------------------------------------- AAC

struct Aac {
    t: IMFTransform,
    out_size: u32,
    pub asc: Vec<u8>,
    pub frames: Vec<Vec<u8>>,
}

impl Aac {
    fn new() -> Res<Self> {
        unsafe {
            let want = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: MFAudioFormat_AAC };
            let mut acts: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut n = 0u32;
            MFTEnumEx(
                MFT_CATEGORY_AUDIO_ENCODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                None,
                Some(&want),
                &mut acts,
                &mut n,
            )?;
            if n == 0 {
                return Err("no AAC encoder found (N edition of Windows without the Media Feature Pack?)".into());
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
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000)?; // 128 kbps
            out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw AAC, no ADTS
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

            // AudioSpecificConfig is the tail of the output type's user data
            let cur = t.GetOutputCurrentType(0)?;
            let mut blob = [0u8; 64];
            let mut len = 0u32;
            cur.GetBlob(&MF_MT_USER_DATA, &mut blob, Some(&mut len))?;
            let asc = if len > 12 { blob[12..len as usize].to_vec() } else { Vec::new() };

            let info = t.GetOutputStreamInfo(0)?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Self { t, out_size: info.cbSize.max(8192), asc, frames: Vec::new() })
        }
    }

    fn collect(&mut self) -> Res<()> {
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
                            self.frames.push(std::slice::from_raw_parts(p, len as usize).to_vec());
                            media.Unlock()?;
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }

    fn encode(&mut self, pcm: &[i16], pts: i64) -> Res<()> {
        unsafe {
            let bytes = pcm.len() * 2;
            let buf = MFCreateMemoryBuffer(bytes as u32)?;
            let (mut p, mut max, mut cur) = (std::ptr::null_mut::<u8>(), 0u32, 0u32);
            buf.Lock(&mut p, Some(&mut max), Some(&mut cur))?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, p, bytes);
            buf.Unlock()?;
            buf.SetCurrentLength(bytes as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(FRAME as i64 * 10_000_000 / RATE)?;
            self.t.ProcessInput(0, &sample, 0)?;
        }
        self.collect()
    }

    fn drain(&mut self) -> Res<()> {
        unsafe {
            self.t.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.t.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        self.collect()
    }
}

// ---------------------------------------------------------------- files

fn write_wav(path: &str, pcm: &[i16]) -> std::io::Result<()> {
    let data_len = (pcm.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + pcm.len() * 2);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&(RATE as u32).to_le_bytes());
    v.extend_from_slice(&(RATE as u32 * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        v.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, v)
}

fn write_adts(path: &str, frames: &[Vec<u8>]) -> std::io::Result<()> {
    let mut v = Vec::new();
    for f in frames {
        let len = f.len() + 7;
        // MPEG-4, AAC-LC, 48 kHz (index 3), 2 channels
        v.extend_from_slice(&[
            0xFF,
            0xF1,
            (1 << 6) | (3 << 2),
            (2 << 6) | ((len >> 11) & 3) as u8,
            ((len >> 3) & 0xFF) as u8,
            (((len & 7) << 5) | 0x1F) as u8,
            0xFC,
        ]);
        v.extend_from_slice(f);
    }
    std::fs::write(path, v)
}

// ---------------------------------------------------------------- main

/// Read waiting packets and return the loudest sample, discarding the audio.
fn peek_peak(c: &Capture) -> f32 {
    let mut peak = 0f32;
    unsafe {
        while c.cap.GetNextPacketSize().unwrap_or(0) > 0 {
            let mut data: *mut u8 = std::ptr::null_mut();
            let (mut frames, mut flags) = (0u32, 0u32);
            if c.cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                break;
            }
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 && !data.is_null() {
                let s = std::slice::from_raw_parts(data as *const f32, frames as usize * 2);
                peak = s.iter().fold(peak, |p, v| p.max(v.abs()));
            }
            let _ = c.cap.ReleaseBuffer(frames);
        }
    }
    peak
}

fn scan(secs: u64) -> Res<()> {
    let en: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let def_out = default_id(&en, true);
    let def_in = default_id(&en, false);
    let mut list: Vec<(Capture, bool, bool, f32, u32)> = Vec::new(); // capture, render, is_default, peak, loud seconds
    for render in [true, false] {
        let flow = if render { eRender } else { eCapture };
        let coll = unsafe { en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)? };
        for i in 0..unsafe { coll.GetCount()? } {
            let dev = unsafe { coll.Item(i)? };
            match open_device(&dev, render) {
                Ok(c) => {
                    let is_def = Some(&c.id) == if render { def_out.as_ref() } else { def_in.as_ref() };
                    list.push((c, render, is_def, 0.0, 0));
                }
                Err(e) => println!("could not open {}: {e}", friendly_name(&dev)),
            }
        }
    }
    println!("listening to {} devices for {secs} s. Play audio the way you normally do (game, browser, music) and talk.\n", list.len());
    let start = Instant::now();
    let mut last = Instant::now();
    let mut sec_peak = vec![0f32; list.len()];
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(20));
        for (i, item) in list.iter_mut().enumerate() {
            let p = peek_peak(&item.0);
            item.3 = item.3.max(p);
            sec_peak[i] = sec_peak[i].max(p);
        }
        if last.elapsed() >= Duration::from_secs(1) {
            last = Instant::now();
            for (i, item) in list.iter_mut().enumerate() {
                if sec_peak[i] > 0.005 {
                    item.4 += 1;
                }
                sec_peak[i] = 0.0;
            }
        }
    }
    println!("{:<18} {:<8} {:>5} {:>8}  device", "kind", "default", "peak", "loud s");
    for (c, render, is_def, peak, loud) in &list {
        println!(
            "{:<18} {:<8} {:>5.2} {:>8}  {}",
            if *render { "output (loopback)" } else { "input (mic)" },
            if *is_def { "DEFAULT" } else { "" },
            peak,
            loud,
            c.name
        );
    }
    Ok(())
}

fn real_main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|a| a == "--devices").unwrap_or(false) {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            timeBeginPeriod(1);
        }
        return scan(args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10));
    }
    let system_mode = args.iter().any(|a| a == "--system");
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).or_else(|| args.get(2).and_then(|s| s.parse().ok())).unwrap_or(15);
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        timeBeginPeriod(1);
    }
    let en: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    let mut sys: Option<SysLoop> = None;
    let mut loopback = if system_mode {
        match open_system_loopback() {
            Ok(s) => {
                println!("desktop audio: ALL apps except this program (process loopback, device independent)");
                sys = Some(s);
            }
            Err(e) => println!("system-wide desktop audio FAILED to open: {e}"),
        }
        None
    } else {
        match open_capture(&en, true) {
            Ok(c) => {
                println!("desktop audio (loopback): {}", c.name);
                Some(c)
            }
            Err(e) => {
                println!("desktop audio FAILED to open: {e}");
                None
            }
        }
    };
    let mut mic = match open_capture(&en, false) {
        Ok(c) => {
            println!("microphone: {}", c.name);
            Some(c)
        }
        Err(e) => {
            println!("microphone FAILED to open: {e}");
            None
        }
    };

    let mut aac = Aac::new()?;
    println!("AAC encoder ready, AudioSpecificConfig = {:02x?}", aac.asc);
    println!("\nrecording {secs} s. Play some audio, talk, stop the audio for a few seconds,");
    println!("and if you can, switch your default output or mic device in Windows sound settings.\n");

    let mut mixer = Mixer::new(2);
    if let Some(g) = args.iter().position(|a| a == "--mic-gain").and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<f32>().ok()) {
        mixer.set_gain(MIC, g);
        println!("mic gain set to {g}x");
    }
    let mut pcm_all: Vec<i16> = Vec::new();
    let (mut ls, mut ms) = (SecondStats::default(), SecondStats::default());
    let mut mix_peak = 0f32;
    let mut mix_frames = 0u64;
    let mut device_changes = 0u32;
    let start = Instant::now();
    let mut last_report = Instant::now();
    let mut last_poll = Instant::now();

    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(10));

        // follow the default devices if they change
        if last_poll.elapsed() >= Duration::from_millis(500) {
            last_poll = Instant::now();
            for (render, slot, label) in [(true, &mut loopback, "desktop audio"), (false, &mut mic, "microphone")] {
                if render && system_mode {
                    continue;
                }
                let now_id = default_id(&en, render);
                let have = slot.as_ref().map(|c| c.id.clone());
                if now_id != have {
                    if let Some(old) = slot.take() {
                        unsafe { let _ = old.client.Stop(); }
                    }
                    match open_capture(&en, render) {
                        Ok(c) => {
                            println!("  >> default {label} changed, now: {}", c.name);
                            device_changes += 1;
                            *slot = Some(c);
                        }
                        Err(e) => println!("  >> default {label} changed but could not open it: {e}"),
                    }
                }
            }
        }

        if let Some(c) = &loopback {
            drain(c, &mut mixer, LOOPBACK, &mut ls)?;
        }
        if let Some(sl) = sys.as_mut() {
            drain_system(sl, &mut mixer, &mut ls)?;
        }
        if let Some(c) = &mic {
            drain(c, &mut mixer, MIC, &mut ms)?;
        }
        for (pts, frame) in mixer.pull(qpc_now_100ns()) {
            let pcm: Vec<i16> = frame.iter().map(|v| (v * 32767.0) as i16).collect();
            mix_peak = frame.iter().fold(mix_peak, |p, v| p.max(v.abs()));
            mix_frames += 1;
            aac.encode(&pcm, pts)?;
            pcm_all.extend_from_slice(&pcm);
        }

        if last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            println!(
                "t={:>2}s  desktop: pkts {:>3} peak {:.2} silent {:>2} | mic: pkts {:>3} peak {:.2} | mix peak {:.2}",
                start.elapsed().as_secs(), ls.packets, ls.peak, ls.silent_flags, ms.packets, ms.peak, mix_peak
            );
            ls = SecondStats::default();
            ms = SecondStats::default();
            mix_peak = 0.0;
        }
    }

    aac.drain()?;
    write_wav("mix.wav", &pcm_all)?;
    write_adts("mix.aac", &aac.frames)?;

    let wall = start.elapsed().as_secs_f64();
    let aac_bytes: usize = aac.frames.iter().map(|f| f.len()).sum();
    let (l, m) = (mixer.stats(LOOPBACK), mixer.stats(MIC));
    println!("\n--- result ---");
    println!("mixed frames {mix_frames} ({:.1} s of audio in {wall:.1} s of wall time)", mix_frames as f64 * FRAME as f64 / RATE as f64);
    println!("AAC frames {}, {:.0} kbps, ASC {:02x?}", aac.frames.len(), aac_bytes as f64 * 8.0 / wall / 1000.0, aac.asc);
    println!(
        "desktop: chunks {}, gap-filled {:.2} s, dropped {:.3} s",
        l.chunks, l.gap_samples as f64 / RATE as f64, l.dropped_samples as f64 / RATE as f64
    );
    println!(
        "mic:     chunks {}, gap-filled {:.2} s, dropped {:.3} s",
        m.chunks, m.gap_samples as f64 / RATE as f64, m.dropped_samples as f64 / RATE as f64
    );
    println!("device changes followed: {device_changes}");
    println!("wrote mix.wav and mix.aac");
    Ok(())
}
