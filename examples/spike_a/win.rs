//! Spike A: capture a monitor with Windows Graphics Capture, scale and convert
//! to NV12 on the GPU, hardware-encode H.264 through Media Foundation, write MP4.
//!
//! Usage: spike_a.exe [seconds] [output.mp4]

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{factory, Interface, HSTRING};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{FILETIME, HMODULE, POINT, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIDevice, DXGI_ADAPTER_DESC};
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::timeBeginPeriod;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

const FPS: u32 = 30;
const FRAME_100NS: i64 = 10_000_000 / FPS as i64;
const MAX_W: u32 = 1280;
const MAX_H: u32 = 720;
const BITRATE: u32 = 6_000_000;
const POOL: usize = 8;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

pub fn run() {
    if let Err(e) = real_main() {
        eprintln!("\nFAILED: {e}");
        std::process::exit(1);
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

fn filetime_100ns(ft: FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
}

fn process_stats() -> (f64, f64, f64) {
    unsafe {
        let h = GetCurrentProcess();
        let mut pmc = PROCESS_MEMORY_COUNTERS_EX::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        let _ = GetProcessMemoryInfo(h, &mut pmc as *mut _ as *mut _, size);
        let (mut c, mut e, mut k, mut u) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let _ = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        let cpu_s = (filetime_100ns(k) + filetime_100ns(u)) as f64 / 1e7;
        (
            pmc.PrivateUsage as f64 / 1048576.0,
            pmc.WorkingSetSize as f64 / 1048576.0,
            cpu_s,
        )
    }
}

fn mark(label: &str) {
    let (p, ws, _) = process_stats();
    println!("  [mem] {label:<38} private {p:>6.1} MB  ws {ws:>6.1} MB");
}

fn list_hw_h264_encoders() -> Res<Vec<(IMFActivate, String)>> {
    let mut found: Vec<(IMFActivate, String)> = Vec::new();
    unsafe {
        let out_type = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_H264,
        };
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&out_type),
            &mut activates,
            &mut count,
        )?;
        println!("hardware H.264 encoder MFTs found: {count}");
        for i in 0..count as usize {
            if let Some(act) = (*activates.add(i)).as_ref() {
                let mut name = windows::core::PWSTR::null();
                let mut len = 0u32;
                if act
                    .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len)
                    .is_ok()
                {
                    let n = name.to_string().unwrap_or_default();
                    println!("  - {n}");
                    CoTaskMemFree(Some(name.0 as *const _));
                    found.push((act.clone(), n));
                }
            }
        }
        for i in 0..count as usize {
            std::ptr::drop_in_place(activates.add(i));
        }
        CoTaskMemFree(Some(activates as *const _));
    }
    Ok(found)
}

fn real_main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let path = args.get(2).cloned().unwrap_or_else(|| "spike_a.mp4".into());
    let mft_mode = args.iter().any(|a| a == "--mft");

    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        timeBeginPeriod(1);
    }
    let (base_private, base_ws, _) = process_stats();
    println!("memory before anything: private {base_private:.1} MB, working set {base_ws:.1} MB");
    let encoders = list_hw_h264_encoders()?;
    mark("after MFTEnumEx");

    // D3D11 device
    let mut device: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
    }
    let device = device.ok_or("no D3D11 device")?;
    mark("after D3D11 device");
    let ctx = ctx.ok_or("no D3D11 context")?;
    unsafe {
        let mt: ID3D11Multithread = device.cast()?;
        let _ = mt.SetMultithreadProtected(true);
        let dxgi: IDXGIDevice = device.cast()?;
        let adapter = dxgi.GetAdapter()?;
        let desc: DXGI_ADAPTER_DESC = adapter.GetDesc()?;
        let name = String::from_utf16_lossy(&desc.Description);
        println!("adapter: {}", name.trim_end_matches('\0'));
    }

    // Capture item: primary monitor
    let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(hmon)? };
    let size = item.Size()?;
    let (src_w, src_h) = (size.Width as u32, size.Height as u32);
    println!("capturing primary monitor: {src_w}x{src_h}");

    // Output size: fit inside 1280x720, never upscale, even dimensions
    let scale = (MAX_W as f64 / src_w as f64)
        .min(MAX_H as f64 / src_h as f64)
        .min(1.0);
    let out_w = ((src_w as f64 * scale) as u32) & !1;
    let out_h = ((src_h as f64 * scale) as u32) & !1;
    println!("encoding at {out_w}x{out_h} @ {FPS} fps, {} kbps", BITRATE / 1000);

    let dxgi: IDXGIDevice = device.cast()?;
    let winrt_dev: IDirect3DDevice =
        unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()? };

    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_dev,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        4,
        size,
    )?;
    let session = pool.CreateCaptureSession(&item)?;
    let _ = session.SetIsCursorCaptureEnabled(true);
    match session.SetIsBorderRequired(false) {
        Ok(()) => println!("border: SetIsBorderRequired(false) accepted"),
        Err(e) => println!("border: SetIsBorderRequired(false) FAILED: {e}"),
    }

    let latest: Arc<Mutex<Option<Direct3D11CaptureFrame>>> = Arc::new(Mutex::new(None));
    let arrived = Arc::new(AtomicU64::new(0));
    {
        let latest = latest.clone();
        let arrived = arrived.clone();
        pool.FrameArrived(&TypedEventHandler::new(
            move |p: windows::core::Ref<Direct3D11CaptureFramePool>, _| {
                if let Some(p) = p.as_ref() {
                    if let Ok(f) = p.TryGetNextFrame() {
                        arrived.fetch_add(1, Ordering::Relaxed);
                        let mut g = latest.lock().unwrap();
                        if let Some(old) = g.replace(f) {
                            let _ = old.Close();
                        }
                    }
                }
                Ok(())
            },
        ))?;
    }
    session.StartCapture()?;
    mark("after capture session started");

    // GPU scaler / color converter
    let vdev: ID3D11VideoDevice = device.cast()?;
    let vctx: ID3D11VideoContext = ctx.cast()?;
    let cd = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: DXGI_RATIONAL { Numerator: FPS, Denominator: 1 },
        InputWidth: src_w,
        InputHeight: src_h,
        OutputFrameRate: DXGI_RATIONAL { Numerator: FPS, Denominator: 1 },
        OutputWidth: out_w,
        OutputHeight: out_h,
        Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    };
    let venum = unsafe { vdev.CreateVideoProcessorEnumerator(&cd)? };
    let vproc = unsafe { vdev.CreateVideoProcessor(&venum, 0)? };
    unsafe {
        let src_rect = RECT { left: 0, top: 0, right: src_w as i32, bottom: src_h as i32 };
        let dst_rect = RECT { left: 0, top: 0, right: out_w as i32, bottom: out_h as i32 };
        vctx.VideoProcessorSetStreamSourceRect(&vproc, 0, true, Some(&src_rect));
        vctx.VideoProcessorSetStreamDestRect(&vproc, 0, true, Some(&dst_rect));
        vctx.VideoProcessorSetOutputTargetRect(&vproc, true, Some(&dst_rect));
        // input: full-range RGB. output: BT.709, limited range
        let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
        let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
        vctx.VideoProcessorSetStreamColorSpace(&vproc, 0, &in_cs);
        vctx.VideoProcessorSetOutputColorSpace(&vproc, &out_cs);
    }

    mark("after video processor");
    // NV12 texture pool
    let tex_desc = D3D11_TEXTURE2D_DESC {
        Width: out_w,
        Height: out_h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut nv12: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)> = Vec::new();
    for _ in 0..POOL {
        let mut t: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&tex_desc, None, Some(&mut t))? };
        let t = t.ok_or("texture create failed")?;
        let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut ov: Option<ID3D11VideoProcessorOutputView> = None;
        unsafe { vdev.CreateVideoProcessorOutputView(&t, &venum, &ovd, Some(&mut ov))? };
        nv12.push((t, ov.ok_or("output view failed")?));
    }

    mark("after NV12 texture pool");
    // Media Foundation sink writer, hardware encoder, D3D11 input
    let mut token = 0u32;
    let mut mgr: Option<IMFDXGIDeviceManager> = None;
    unsafe {
        MFCreateDXGIDeviceManager(&mut token, &mut mgr)?;
    }
    let mgr = mgr.ok_or("no dxgi manager")?;
    unsafe { mgr.ResetDevice(&device, token)? };

    let mut attrs: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attrs, 4)? };
    let attrs = attrs.ok_or("no attrs")?;
    unsafe {
        attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &mgr)?;
        attrs.SetUINT32(&MF_LOW_LATENCY, 1)?;
    }
    let writer: Option<IMFSinkWriter> = if mft_mode {
        None
    } else {
        Some(unsafe { MFCreateSinkWriterFromURL(&HSTRING::from(path.as_str()), None, &attrs)? })
    };

    if let Some(writer) = &writer {
      unsafe {
        let out_t = MFCreateMediaType()?;
        out_t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        out_t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        out_t.SetUINT32(&MF_MT_AVG_BITRATE, BITRATE)?;
        out_t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        out_t.SetUINT64(&MF_MT_FRAME_SIZE, pack(out_w, out_h))?;
        out_t.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        out_t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        out_t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
        out_t.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, FPS)?;
        let stream = writer.AddStream(&out_t)?;

        let in_t = MFCreateMediaType()?;
        in_t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        in_t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        in_t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        in_t.SetUINT64(&MF_MT_FRAME_SIZE, pack(out_w, out_h))?;
        in_t.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        in_t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        in_t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        in_t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        in_t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        writer.SetInputMediaType(stream, &in_t, None)?;
        writer.BeginWriting()?;
      }
    }

    let mut enc: Option<crate::mft::MftEncoder> = None;
    if mft_mode {
        let (act, name) = encoders
            .iter()
            .find(|(_, n)| n.contains("NVIDIA"))
            .or_else(|| encoders.first())
            .ok_or("no hardware H.264 encoder")?;
        println!("driving encoder directly: {name}");
        enc = Some(crate::mft::MftEncoder::new(act, name.clone(), &mgr, out_w, out_h, FPS, 5_000_000, 8_000_000)?);
    }
    mark("after encoder created");

    // wait for the first captured frame
    let wait = Instant::now();
    while latest.lock().unwrap().is_none() {
        if wait.elapsed() > Duration::from_secs(5) {
            return Err("no frame from Windows Graphics Capture within 5 s".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let (priv0, ws0, cpu0) = process_stats();
    println!("memory after setup: private {priv0:.1} MB, working set {ws0:.1} MB");
    println!("recording {secs} s to {path} ...");

    let start = Instant::now();
    let total_frames = secs * FPS as u64;
    let mut last_report = Instant::now();
    let mut last_arrived = 0u64;
    let mut peak_private = priv0;
    let mut slowest_ms = 0.0f64;

    for n in 0..total_frames {
        let due = start + Duration::from_nanos(n * 1_000_000_000 / FPS as u64);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        let t0 = Instant::now();

        let (tex, ov) = &nv12[n as usize % POOL];
        {
            let guard = latest.lock().unwrap();
            let frame = guard.as_ref().ok_or("lost frame")?;
            let surface = frame.Surface()?;
            let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
            let src_tex: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut iv: Option<ID3D11VideoProcessorInputView> = None;
            unsafe { vdev.CreateVideoProcessorInputView(&src_tex, &venum, &ivd, Some(&mut iv))? };
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(iv),
                ..Default::default()
            }];
            let r = unsafe { vctx.VideoProcessorBlt(&vproc, ov, 0, &streams) };
            // the struct holds the view in a ManuallyDrop, so release it by hand
            unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
            r?;
        }

        unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            let len = buf.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buf.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(n as i64 * FRAME_100NS)?;
            sample.SetSampleDuration(FRAME_100NS)?;
            if let Some(w) = &writer {
                w.WriteSample(0, &sample)?;
            } else if let Some(e) = enc.as_mut() {
                e.feed(&sample)?;
            }
        }
        slowest_ms = slowest_ms.max(t0.elapsed().as_secs_f64() * 1000.0);

        if last_report.elapsed() >= Duration::from_secs(1) {
            let a = arrived.load(Ordering::Relaxed);
            let (p, ws, _) = process_stats();
            peak_private = peak_private.max(p);
            println!(
                "t={:>3}s  submitted {:>4}  capture callbacks/s {:>3}  private {:.1} MB  ws {:.1} MB",
                start.elapsed().as_secs(),
                n + 1,
                a - last_arrived,
                p,
                ws
            );
            last_arrived = a;
            last_report = Instant::now();
        }
    }

    let wall = start.elapsed().as_secs_f64();
    if let Some(w) = &writer {
        unsafe { w.Finalize()? };
    }
    if let Some(e) = enc.as_mut() {
        e.finish()?;
    }
    let (pf, wsf, cpu1) = process_stats();
    let _ = session.Close();
    let _ = pool.Close();

    if let Some(e) = &enc {
        report_packets(e, wall, &path);
    }
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("\n--- result ---");
    println!("file: {path}  ({:.2} MB, avg {:.2} Mbps)", bytes as f64 / 1048576.0, bytes as f64 * 8.0 / wall / 1e6);
    println!("wall {wall:.1} s, process CPU {:.1}% of one core", (cpu1 - cpu0) / wall * 100.0);
    println!("private memory: start {base_private:.1} -> after setup {priv0:.1} -> peak {peak_private:.1} -> end {pf:.1} MB (working set end {wsf:.1} MB)");
    println!("slowest single frame submit: {slowest_ms:.1} ms");
    Ok(())
}

fn report_packets(e: &crate::mft::MftEncoder, wall: f64, path: &str) {
    let n = e.packets.len();
    let total: usize = e.packets.iter().map(|p| p.data.len()).sum();
    let keys: Vec<&crate::mft::Packet> = e.packets.iter().filter(|p| p.key).collect();
    let max = e.packets.iter().map(|p| p.data.len()).max().unwrap_or(0);
    println!("\n--- packets from {} ---", e.name);
    println!("packets {n}, dropped input frames {}, keyframes {}", e.dropped, keys.len());
    println!("total {:.2} MB, avg {:.2} Mbps, largest packet {:.0} KB", total as f64 / 1048576.0, total as f64 * 8.0 / wall / 1e6, max as f64 / 1024.0);
    let gaps: Vec<String> = keys
        .windows(2)
        .map(|w| format!("{:.2}", (w[1].pts_100ns - w[0].pts_100ns) as f64 / 1e7))
        .collect();
    println!("seconds between keyframes: {}", if gaps.is_empty() { "n/a".into() } else { gaps.join(" ") });
    if let Some(k) = keys.first() {
        let d = &k.data;
        let has = |nal: u8| d.windows(5).any(|w| w[0] == 0 && w[1] == 0 && w[2] == 1 && (w[3] & 0x1f) == nal || (w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 1 && (w[4] & 0x1f) == nal));
        println!("first keyframe has SPS: {}, PPS: {}", has(7), has(8));
    }
    let bps = total as f64 / wall;
    println!("a 120 s ring at this rate would hold about {:.0} MB", bps * 120.0 / 1048576.0);
    let h264 = format!("{path}.h264");
    let mut out = Vec::with_capacity(total);
    for p in &e.packets {
        out.extend_from_slice(&p.data);
    }
    match std::fs::write(&h264, out) {
        Ok(()) => println!("raw stream written to {h264} (plays in VLC)"),
        Err(err) => println!("could not write {h264}: {err}"),
    }
}
